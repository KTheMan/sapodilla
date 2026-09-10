const INITIALIZATION_TIMEOUT_MS = 30_000;
const REQUEST_TIMEOUT_MS = {
  scan: 5 * 60_000,
  print: 90_000,
};

function applicationModuleUrls() {
  const preload = Array.from(
    document.querySelectorAll('link[rel="modulepreload"]'),
  ).find((link) => /sapodilla-[a-f0-9]+\.js(?:$|[?#])/.test(link.href));
  if (!preload) {
    throw new Error("could not locate the Sapodilla browser module");
  }
  const shimUrl = preload.href;
  const wasmUrl = shimUrl.replace(/\.js(?:$|([?#]))/, "_bg.wasm$1");
  return { shimUrl, wasmUrl };
}

// Every request owns a fresh worker and a fresh WebAssembly memory. A scan can
// neither retain its peak memory nor block printing, another scan, or the UI.
function submitDisposable(kind, payload, transfer, callback) {
  let worker;
  let settled = false;
  let initializationTimeout;
  let requestTimeout;

  const finish = (message) => {
    if (settled) return;
    settled = true;
    clearTimeout(initializationTimeout);
    clearTimeout(requestTimeout);
    if (worker) {
      worker.onmessage = null;
      worker.onerror = null;
      worker.onmessageerror = null;
      worker.terminate();
    }
    callback(message);
  };

  try {
    worker = new Worker(new URL("calibration-worker.js", document.baseURI), {
      type: "module",
      name: `sapodilla-calibration-${kind}`,
    });
    worker.onerror = (event) =>
      finish({ ok: false, error: event.message || `${kind} worker failed` });
    worker.onmessageerror = () =>
      finish({ ok: false, error: `${kind} worker returned unreadable data` });
    worker.onmessage = (event) => {
      if (event.data?.type === "ready") {
        clearTimeout(initializationTimeout);
        requestTimeout = setTimeout(
          () => finish({ ok: false, error: `${kind} calibration worker timed out` }),
          REQUEST_TIMEOUT_MS[kind],
        );
        try {
          worker.postMessage({ type: "request", id: 1, kind, ...payload }, transfer);
        } catch (error) {
          finish({ ok: false, error: `could not submit ${kind} calibration work: ${error}` });
        }
      } else if (event.data?.type === "result") {
        finish(event.data);
      }
    };

    const { shimUrl, wasmUrl } = applicationModuleUrls();
    initializationTimeout = setTimeout(
      () => finish({ ok: false, error: `${kind} calibration worker initialization timed out` }),
      INITIALIZATION_TIMEOUT_MS,
    );
    worker.postMessage({ type: "initialize", shimUrl, wasmUrl });
  } catch (error) {
    finish({ ok: false, error: `could not start ${kind} calibration worker: ${error}` });
  }
}

export function isolatedCalibrationScan(bytes, manifestJson, configJson, callback) {
  const owned = Uint8Array.from(bytes);
  submitDisposable(
    "scan",
    { bytes: owned.buffer, manifestJson, configJson },
    [owned.buffer],
    callback,
  );
}

// Keep the browser File out of the UI WebAssembly heap entirely. The File is
// structured-cloned to a disposable worker, which reads and decodes it there.
export function pickIsolatedCalibrationScan(
  manifestJson,
  configJson,
  selectedCallback,
  callback,
) {
  const input = document.createElement("input");
  input.type = "file";
  input.accept = ".png,.jpg,.jpeg,image/png,image/jpeg";
  input.style.display = "none";
  document.body.appendChild(input);
  let completed = false;
  const finish = (message) => {
    if (completed) return;
    completed = true;
    input.remove();
    callback(message);
  };
  input.addEventListener(
    "change",
    () => {
      const file = input.files?.[0];
      if (!file) {
        selectedCallback("");
        finish({ ok: false, cancelled: true, fileName: "scan", error: "Scan import was cancelled." });
        return;
      }
      selectedCallback(file.name);
      submitDisposable(
        "scan",
        { file, manifestJson, configJson },
        [],
        (message) => finish({ ...message, fileName: file.name }),
      );
    },
    { once: true },
  );
  input.addEventListener(
    "cancel",
    () => {
      selectedCallback("");
      finish({ ok: false, cancelled: true, fileName: "scan", error: "Scan import was cancelled." });
    },
    { once: true },
  );
  input.click();
}

export function isolatedCalibrationPrint(requestJson, callback) {
  submitDisposable("print", { requestJson }, [], callback);
}
