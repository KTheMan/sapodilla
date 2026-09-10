let workerPromise;
let workerInstance;
let nextRequestId = 1;
const pending = new Map();
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

function rejectPending(message) {
  for (const request of pending.values()) {
    clearTimeout(request.timeout);
    request.callback({ ok: false, error: message });
  }
  pending.clear();
}

function discardWorker(message) {
  workerInstance?.terminate();
  workerInstance = undefined;
  workerPromise = undefined;
  rejectPending(message);
}

function isolatedWorker() {
  if (workerPromise) {
    return workerPromise;
  }
  workerPromise = new Promise((resolve, reject) => {
    const worker = new Worker(
      new URL("calibration-worker.js", document.baseURI),
      { type: "module", name: "sapodilla-calibration" },
    );
    workerInstance = worker;
    const initializationTimeout = setTimeout(() => {
      const message = "calibration worker initialization timed out";
      discardWorker(message);
      reject(new Error(message));
    }, INITIALIZATION_TIMEOUT_MS);
    const fail = (message) => {
      clearTimeout(initializationTimeout);
      discardWorker(message);
      reject(new Error(message));
    };
    worker.onerror = (event) =>
      fail(event.message || "calibration worker failed");
    worker.onmessageerror = () => fail("calibration worker returned unreadable data");
    worker.onmessage = (event) => {
      const message = event.data;
      if (message.type === "ready") {
        clearTimeout(initializationTimeout);
        resolve(worker);
        return;
      }
      if (message.type !== "result") {
        return;
      }
      const request = pending.get(message.id);
      if (!request) {
        return;
      }
      pending.delete(message.id);
      clearTimeout(request.timeout);
      request.callback(message);
    };
    try {
      const { shimUrl, wasmUrl } = applicationModuleUrls();
      worker.postMessage({ type: "initialize", shimUrl, wasmUrl });
    } catch (error) {
      worker.terminate();
      fail(String(error));
    }
  });
  return workerPromise;
}

function submit(kind, payload, transfer, callback) {
  const id = nextRequestId++;
  isolatedWorker().then(
    (worker) => {
      const timeout = setTimeout(() => {
        discardWorker(`${kind} calibration worker timed out`);
      }, REQUEST_TIMEOUT_MS[kind]);
      pending.set(id, { callback, timeout });
      try {
        worker.postMessage({ type: "request", id, kind, ...payload }, transfer);
      } catch (error) {
        discardWorker(`could not submit ${kind} calibration work: ${error}`);
      }
    },
    (error) => callback({ ok: false, error: String(error) }),
  );
}

export function isolatedCalibrationScan(
  bytes,
  manifestJson,
  configJson,
  callback,
) {
  const owned = Uint8Array.from(bytes);
  submit(
    "scan",
    { bytes: owned.buffer, manifestJson, configJson },
    [owned.buffer],
    callback,
  );
}

export function isolatedCalibrationPrint(requestJson, callback) {
  submit("print", { requestJson }, [], callback);
}
