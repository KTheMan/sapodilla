let bindings;

const MAX_SCAN_EDGE = 4000;

async function workerOwnedScanBytes(file) {
  const bitmap = await createImageBitmap(file);
  const longestEdge = Math.max(bitmap.width, bitmap.height);
  if (longestEdge <= MAX_SCAN_EDGE) {
    bitmap.close();
    return new Uint8Array(await file.arrayBuffer());
  }
  const scale = MAX_SCAN_EDGE / longestEdge;
  const width = Math.max(1, Math.round(bitmap.width * scale));
  const height = Math.max(1, Math.round(bitmap.height * scale));
  const canvas = new OffscreenCanvas(width, height);
  const context = canvas.getContext("2d", { alpha: false });
  if (!context) {
    bitmap.close();
    throw new Error("could not create the isolated scan resampler");
  }
  context.drawImage(bitmap, 0, 0, width, height);
  bitmap.close();
  const blob = await canvas.convertToBlob({ type: "image/png" });
  return new Uint8Array(await blob.arrayBuffer());
}

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
      // Throwing from this async message handler only creates an unhandled
      // rejection inside the Worker; it does not reliably dispatch an error
      // event to the owning Window. Report bootstrap failures explicitly so
      // the UI does not mislabel them as a timeout thirty seconds later.
      self.postMessage({
        type: "initialization-error",
        error: `could not initialize calibration worker: ${error}`,
      });
    }
    return;
  }
  if (message.type !== "request" || !bindings) {
    return;
  }
  try {
    if (message.kind === "scan") {
      const encoded = message.file
        ? await workerOwnedScanBytes(message.file)
        : new Uint8Array(message.bytes);
      sendResult(
        message.id,
        bindings.isolated_calibration_scan(
          encoded,
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
