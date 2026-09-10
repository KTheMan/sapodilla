let bindings;

function sendResult(id, result) {
  const ok = Boolean(result[0]);
  const text = String(result[1] ?? "");
  const bytes = Uint8Array.from(result[2] ?? []);
  const message = ok
    ? { type: "result", id, ok, text, bytes: bytes.buffer }
    : { type: "result", id, ok, error: text, bytes: bytes.buffer };
  self.postMessage(message, [bytes.buffer]);
}

self.onmessage = async (event) => {
  const message = event.data;
  if (message.type === "initialize") {
    try {
      bindings = await import(message.shimUrl);
      await bindings.default({ module_or_path: message.wasmUrl });
      self.postMessage({ type: "ready" });
    } catch (error) {
      throw new Error(`could not initialize calibration worker: ${error}`);
    }
    return;
  }
  if (message.type !== "request" || !bindings) {
    return;
  }
  try {
    if (message.kind === "scan") {
      sendResult(
        message.id,
        bindings.isolated_calibration_scan(
          new Uint8Array(message.bytes),
          message.manifestJson,
          message.configJson,
        ),
      );
    } else if (message.kind === "print") {
      sendResult(
        message.id,
        bindings.isolated_calibration_print(message.requestJson),
      );
    } else {
      throw new Error(`unknown calibration worker request: ${message.kind}`);
    }
  } catch (error) {
    self.postMessage({
      type: "result",
      id: message.id,
      ok: false,
      error: String(error),
    });
  }
};
