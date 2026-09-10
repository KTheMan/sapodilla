import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

async function importSource(path, tag) {
  const source = await readFile(new URL(path, import.meta.url), "utf8");
  const encoded = Buffer.from(`${source}\n//# sourceURL=${path}?${tag}`).toString("base64");
  return import(`data:text/javascript;base64,${encoded}#${tag}`);
}

async function loadCalibrationWorker(tag, bindingsSource) {
  const messages = [];
  globalThis.self = {
    postMessage: (message, transfer = []) => messages.push({ message, transfer }),
  };
  globalThis.__calibrationWorkerInput = undefined;
  const bindings = bindingsSource || [
    "export default async function initialize() {}",
    "export function isolated_calibration_scan(bytes) {",
    "  globalThis.__calibrationWorkerInput = Array.from(bytes);",
    "  return [true, '{}', new Uint8Array([4, 5])];",
    "}",
    "export function isolated_calibration_print() {",
    "  return [true, '', new Uint8Array(0)];",
    "}",
  ].join("\n");
  const shimUrl = `data:text/javascript,${encodeURIComponent(bindings)}`;
  await importSource("../static/calibration-worker.js", tag);
  await globalThis.self.onmessage({
    data: { type: "initialize", shimUrl, wasmUrl: "test.wasm" },
  });
  return messages;
}

test("calibration worker reports asynchronous initialization failures", async () => {
  const messages = await loadCalibrationWorker(
    "initialization-error-message",
    [
      "export default async function initialize() {",
      "  throw new Error('simulated WASM bootstrap failure');",
      "}",
    ].join("\n"),
  );
  const failures = messages.filter(
    ({ message }) => message.type === "initialization-error",
  );

  assert.equal(failures.length, 1);
  assert.match(failures[0].message.error, /simulated WASM bootstrap failure/);
});

function installIndexedDb() {
  const values = new Map();
  let upgraded = false;
  const database = {
    objectStoreNames: { contains: () => upgraded },
    createObjectStore: () => {
      upgraded = true;
    },
    close() {},
    transaction() {
      const transaction = {};
      const finish = (operation) => {
        const request = {};
        queueMicrotask(() => {
          try {
            request.result = operation();
            request.onsuccess?.();
            queueMicrotask(() => transaction.oncomplete?.());
          } catch (error) {
            request.error = error;
            request.onerror?.();
          }
        });
        return request;
      };
      transaction.objectStore = () => ({
        get: (key) => finish(() => values.get(key)),
        put: (value, key) => finish(() => values.set(key, value)),
        delete: (key) => finish(() => values.delete(key)),
      });
      return transaction;
    },
  };
  globalThis.indexedDB = {
    open() {
      const request = {};
      queueMicrotask(() => {
        request.result = database;
        if (!upgraded) request.onupgradeneeded?.();
        request.onsuccess?.();
      });
      return request;
    },
  };
  return values;
}

function directoryHandle(initialFiles = {}) {
  const files = new Map(Object.entries(initialFiles));
  const directory = {
    name: "Sapodilla Calibrations",
    permission: "granted",
    requestCount: 0,
    failPrimaryCloseCount: 0,
    async queryPermission() {
      return this.permission;
    },
    async requestPermission() {
      this.requestCount += 1;
      this.permission = "granted";
      return this.permission;
    },
    async getFileHandle(name, options = {}) {
      if (!files.has(name) && !options.create) {
        throw Object.assign(new Error("missing"), { name: "NotFoundError" });
      }
      return {
        async getFile() {
          return { text: async () => files.get(name) };
        },
        async createWritable() {
          let candidate = "";
          return {
            async write(contents) {
              candidate = contents;
            },
            async close() {
              if (name === "sapodilla-calibration-registry.json" && directory.failPrimaryCloseCount > 0) {
                directory.failPrimaryCloseCount -= 1;
                files.set(name, "partial candidate");
                throw new Error("simulated interrupted primary write");
              }
              files.set(name, candidate);
            },
            async abort() {},
          };
        },
      };
    },
    async removeEntry(name) {
      files.delete(name);
    },
  };
  return { directory, files };
}

test("directory handles restore from IndexedDB and reconnect permission", async () => {
  installIndexedDb();
  const { directory } = directoryHandle();
  globalThis.window = {
    indexedDB: globalThis.indexedDB,
    showDirectoryPicker: async () => directory,
  };

  const first = await importSource("../src/calibration/web_storage.js", "choose");
  assert.equal(JSON.parse(await first.calibrationStorageChoose()).state, "ready");

  directory.permission = "prompt";
  const restored = await importSource("../src/calibration/web_storage.js", "restore");
  assert.equal(JSON.parse(await restored.calibrationStorageRestore()).state, "permission-required");
  assert.equal(JSON.parse(await restored.calibrationStorageReconnect()).state, "ready");
  assert.equal(directory.requestCount, 1);
});

test("an interrupted primary write preserves the last verified backup", async () => {
  installIndexedDb();
  const primary = "verified previous registry";
  const { directory, files } = directoryHandle({
    "sapodilla-calibration-registry.json": primary,
    "sapodilla-calibration-registry.backup.json": primary,
  });
  globalThis.window = {
    indexedDB: globalThis.indexedDB,
    showDirectoryPicker: async () => directory,
  };
  const storage = await importSource("../src/calibration/web_storage.js", "failure");
  await storage.calibrationStorageChoose();
  // Fail both the candidate close and the best-effort rollback close. The
  // previous registry must still be available exclusively from backup.
  directory.failPrimaryCloseCount = 2;

  await assert.rejects(
    storage.calibrationStorageWrite("uncommitted candidate", primary),
    /simulated interrupted primary write/,
  );
  assert.equal(files.get("sapodilla-calibration-registry.json"), "partial candidate");
  assert.equal(files.get("sapodilla-calibration-registry.backup.json"), primary);
  assert.notEqual(files.get("sapodilla-calibration-registry.backup.json"), "uncommitted candidate");
});

test("service-worker activation deletes only obsolete Sapodilla caches", async () => {
  const handlers = new Map();
  const deleted = [];
  const added = [];
  const cache = {
    add: async (url) => added.push(String(url)),
    match: async () => undefined,
    put: async () => {},
  };
  globalThis.self = {
    registration: { scope: "https://example.test/sapodilla/" },
    location: {
      href: "https://example.test/sapodilla/coi-serviceworker.js?build=build-abcdef",
      origin: "https://example.test",
    },
    clients: { claim: async () => {} },
    skipWaiting: async () => {},
    addEventListener: (name, handler) => handlers.set(name, handler),
  };
  globalThis.caches = {
    keys: async () => [
      "sapodilla-app-shell-v0",
      "sapodilla-app-shell-v1",
      "sapodilla-app-shell-v2",
      "sapodilla-app-shell-build-abcdef",
      "another-project-app-shell-v9",
    ],
    open: async () => cache,
    delete: async (key) => {
      deleted.push(key);
      return true;
    },
  };

  await importSource("../static/coi-serviceworker.js", "cache-ownership");
  let installation;
  handlers.get("install")({ waitUntil: (promise) => (installation = promise) });
  await installation;
  assert.ok(added.includes(
    "https://example.test/sapodilla/calibration-worker.js?build=build-abcdef",
  ));
  let activation;
  handlers.get("activate")({ waitUntil: (promise) => (activation = promise) });
  await activation;
  assert.deepEqual(deleted, [
    "sapodilla-app-shell-v0",
    "sapodilla-app-shell-v1",
    "sapodilla-app-shell-v2",
  ]);

  let fetchedRequest;
  globalThis.fetch = async (request) => {
    fetchedRequest = request;
    return new Response("fresh worker", { status: 200 });
  };
  let fetchResponse;
  handlers.get("fetch")({
    request: new Request(
      "https://example.test/sapodilla/calibration-worker.js?build=build-abcdef",
    ),
    respondWith: (promise) => (fetchResponse = promise),
  });
  await fetchResponse;
  assert.equal(fetchedRequest.cache, "no-store");
});

test("service-worker bootstrap reads Trunk's build hash before registering", async () => {
  const index = await readFile(new URL("../index.html", import.meta.url), "utf8");
  const rustLink = index.indexOf('data-trunk rel="rust"');
  const revisionLookup = index.indexOf("const moduleSource");

  assert.ok(rustLink >= 0 && rustLink < revisionLookup);
  assert.match(index, /coi-serviceworker\.js/);
  assert.match(index, /updateViaCache:\s*"none"/);
  assert.match(index, /controllerchange/);
});

test("calibration client gives every request a disposable isolated worker", async () => {
  const workers = [];
  const appModule = "https://example.test/sapodilla/sapodilla-0123456789abcdef.js";
  globalThis.document = {
    baseURI: "https://example.test/sapodilla/",
    querySelectorAll: () => [{ href: appModule }],
  };
  globalThis.Worker = class {
    constructor(url, options) {
      this.url = String(url);
      this.options = options;
      this.messages = [];
      workers.push(this);
    }

    postMessage(message, transfer = []) {
      this.messages.push({ message, transfer });
      if (message.type === "initialize") {
        queueMicrotask(() => this.onmessage({ data: { type: "ready" } }));
      } else if (message.type === "request") {
        queueMicrotask(() =>
          this.onmessage({
            data: {
              type: "result",
              id: message.id,
              ok: true,
              text: message.kind === "scan" ? "{}" : "",
              bytes: new Uint8Array([9, 8]).buffer,
            },
          }),
        );
      }
    }

    terminate() {
      this.terminated = true;
    }
  };

  const client = await importSource(
    "../src/calibration/isolated_worker.js",
    "disposable-worker",
  );
  const scan = await new Promise((resolve) =>
    client.isolatedCalibrationScan(new Uint8Array([1, 2, 3]), "{}", "{}", resolve),
  );
  const print = await new Promise((resolve) =>
    client.isolatedCalibrationPrint("{}", resolve),
  );

  assert.equal(scan.ok, true);
  assert.equal(print.ok, true);
  assert.equal(workers.length, 2);
  assert.equal(workers[0].options.type, "module");
  assert.equal(
    workers[0].url,
    "https://example.test/sapodilla/calibration-worker.js?build=0123456789abcdef",
  );
  assert.equal(workers[0].messages[0].message.wasmUrl,
    "https://example.test/sapodilla/sapodilla-0123456789abcdef_bg.wasm");
  const scanMessage = workers[0].messages.find(
    ({ message }) => message.kind === "scan",
  );
  assert.equal(scanMessage.transfer.length, 1);
  assert.equal(scanMessage.transfer[0], scanMessage.message.bytes);
  assert.equal(workers[0].terminated, true);
  assert.equal(workers[1].terminated, true);
});

test("scan picker passes the browser File directly to its disposable worker", async () => {
  const workers = [];
  const listeners = new Map();
  const file = { name: "full-resolution-scan.png" };
  const input = {
    files: [file],
    style: {},
    addEventListener: (name, handler) => listeners.set(name, handler),
    removeEventListener: (name, handler) => {
      if (listeners.get(name) === handler) listeners.delete(name);
    },
    // Chromium may report a cancel after a completed change. The Rust
    // selection callback is one-shot, so the client must suppress the second
    // notification.
    click: () => queueMicrotask(() => {
      const cancel = listeners.get("cancel");
      listeners.get("change")();
      cancel();
    }),
    remove() {},
  };
  globalThis.document = {
    baseURI: "https://example.test/sapodilla/",
    querySelectorAll: () => [{
      href: "https://example.test/sapodilla/sapodilla-0123456789abcdef.js",
    }],
    createElement: () => input,
    body: { appendChild() {} },
  };
  globalThis.Worker = class {
    constructor() {
      this.messages = [];
      workers.push(this);
    }
    postMessage(message, transfer = []) {
      this.messages.push({ message, transfer });
      if (message.type === "initialize") {
        queueMicrotask(() => this.onmessage({ data: { type: "ready" } }));
      } else {
        queueMicrotask(() => this.onmessage({ data: {
          type: "result",
          id: 1,
          ok: true,
          text: "{}",
          bytes: new ArrayBuffer(0),
        } }));
      }
    }
    terminate() {
      this.terminated = true;
    }
  };

  const client = await importSource(
    "../src/calibration/isolated_worker.js",
    "direct-file-worker",
  );
  let selectedName;
  let selectedCalls = 0;
  const result = await new Promise((resolve) =>
    client.pickIsolatedCalibrationScan(
      "{}",
      "{}",
      (name) => {
        selectedCalls += 1;
        selectedName = name;
      },
      resolve,
    ),
  );
  const request = workers[0].messages.find(({ message }) => message.type === "request");

  assert.equal(result.ok, true);
  assert.equal(selectedCalls, 1);
  assert.equal(selectedName, file.name);
  assert.equal(result.fileName, file.name);
  assert.equal(request.message.file, file);
  assert.equal("bytes" in request.message, false);
  assert.equal(request.transfer.length, 0);
  assert.equal(workers[0].terminated, true);
});

test("scan worker downsamples an oversized file without reading it on the UI side", async () => {
  let canvasSize;
  let bitmapClosed = false;
  globalThis.createImageBitmap = async () => ({
    width: 8000,
    height: 4000,
    close: () => { bitmapClosed = true; },
  });
  globalThis.OffscreenCanvas = class {
    constructor(width, height) {
      canvasSize = [width, height];
    }
    getContext() {
      return { drawImage() {} };
    }
    async convertToBlob() {
      return { arrayBuffer: async () => new Uint8Array([7, 8, 9]).buffer };
    }
  };
  const messages = await loadCalibrationWorker("oversized-scan-resample");
  await globalThis.self.onmessage({ data: {
    type: "request",
    id: 1,
    kind: "scan",
    file: { arrayBuffer: () => { throw new Error("original file must not be reread"); } },
    manifestJson: "{}",
    configJson: "{}",
  } });

  assert.deepEqual(canvasSize, [4000, 2000]);
  assert.equal(bitmapClosed, true);
  assert.deepEqual(globalThis.__calibrationWorkerInput, [7, 8, 9]);
  assert.equal(messages.filter(({ message }) => message.type === "result").length, 1);
});

test("scan worker retains original bytes at or below the analysis bound", async () => {
  let canvases = 0;
  globalThis.createImageBitmap = async () => ({
    width: 4000,
    height: 2500,
    close() {},
  });
  globalThis.OffscreenCanvas = class {
    constructor() { canvases += 1; }
  };
  await loadCalibrationWorker("bounded-scan-original");
  await globalThis.self.onmessage({ data: {
    type: "request",
    id: 2,
    kind: "scan",
    file: { arrayBuffer: async () => new Uint8Array([1, 3, 5]).buffer },
    manifestJson: "{}",
    configJson: "{}",
  } });

  assert.equal(canvases, 0);
  assert.deepEqual(globalThis.__calibrationWorkerInput, [1, 3, 5]);
});

test("scan worker reports bitmap failures exactly once", async () => {
  globalThis.createImageBitmap = async () => { throw new Error("bad scan bitmap"); };
  const messages = await loadCalibrationWorker("scan-bitmap-failure");
  await globalThis.self.onmessage({ data: {
    type: "request",
    id: 3,
    kind: "scan",
    file: {},
    manifestJson: "{}",
    configJson: "{}",
  } });
  const results = messages.filter(({ message }) => message.type === "result");

  assert.equal(results.length, 1);
  assert.equal(results[0].message.ok, false);
  assert.match(results[0].message.error, /bad scan bitmap/);
});

test("calibration worker initialization failure completes its request exactly once", async () => {
  globalThis.document = {
    baseURI: "https://example.test/sapodilla/",
    querySelectorAll: () => [{
      href: "https://example.test/sapodilla/sapodilla-0123456789abcdef.js",
    }],
  };
  globalThis.Worker = class {
    postMessage(message) {
      if (message.type === "initialize") {
        queueMicrotask(() => this.onerror({ message: "simulated startup failure" }));
      }
    }
    terminate() {}
  };
  const client = await importSource(
    "../src/calibration/isolated_worker.js",
    "startup-failure",
  );
  let calls = 0;
  const result = await new Promise((resolve) =>
    client.isolatedCalibrationPrint("{}", (message) => {
      calls += 1;
      resolve(message);
    }),
  );
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(calls, 1);
  assert.equal(result.ok, false);
  assert.match(result.error, /simulated startup failure/);
});

test("an explicit worker initialization error completes immediately and reloads a stale build", async () => {
  let reloads = 0;
  let terminated = false;
  globalThis.document = {
    baseURI: "https://example.test/sapodilla/",
    querySelectorAll: () => [{
      href: "https://example.test/sapodilla/sapodilla-0123456789abcdef.js",
    }],
  };
  globalThis.window = {
    location: { reload: () => { reloads += 1; } },
  };
  globalThis.fetch = async () => new Response(
    '<script type="module">import init from "./sapodilla-fedcba9876543210.js";</script>',
    { status: 200 },
  );
  globalThis.Worker = class {
    postMessage(message) {
      if (message.type === "initialize") {
        queueMicrotask(() => this.onmessage({ data: {
          type: "initialization-error",
          error: "could not initialize calibration worker: missing old build",
        } }));
      }
    }
    terminate() {
      terminated = true;
    }
  };
  const client = await importSource(
    "../src/calibration/isolated_worker.js",
    "explicit-initialization-error",
  );
  const started = performance.now();
  const result = await new Promise((resolve) =>
    client.isolatedCalibrationPrint("{}", resolve),
  );
  await new Promise((resolve) => setTimeout(resolve, 0));

  assert.equal(result.ok, false);
  assert.match(result.error, /missing old build/);
  assert.ok(performance.now() - started < 1_000);
  assert.equal(terminated, true);
  assert.equal(reloads, 1);
});

test("a timed-out calibration request is completed once and the worker is recreated", async () => {
  const workers = [];
  const realSetTimeout = globalThis.setTimeout;
  globalThis.setTimeout = (callback, delay, ...args) =>
    realSetTimeout(callback, delay >= 30_000 ? 5 : delay, ...args);
  globalThis.document = {
    baseURI: "https://example.test/sapodilla/",
    querySelectorAll: () => [{
      href: "https://example.test/sapodilla/sapodilla-0123456789abcdef.js",
    }],
  };
  globalThis.Worker = class {
    constructor() {
      this.index = workers.length;
      this.terminated = false;
      workers.push(this);
    }
    postMessage(message) {
      if (message.type === "initialize") {
        queueMicrotask(() => this.onmessage({ data: { type: "ready" } }));
      } else if (message.type === "request" && this.index > 0) {
        queueMicrotask(() => this.onmessage({
          data: {
            type: "result",
            id: message.id,
            ok: true,
            text: "",
            bytes: new ArrayBuffer(0),
          },
        }));
      }
    }
    terminate() {
      this.terminated = true;
    }
  };

  try {
    const client = await importSource(
      "../src/calibration/isolated_worker.js",
      "request-timeout",
    );
    let firstCalls = 0;
    const first = await new Promise((resolve) =>
      client.isolatedCalibrationPrint("{}", (message) => {
        firstCalls += 1;
        resolve(message);
      }),
    );
    const second = await new Promise((resolve) =>
      client.isolatedCalibrationPrint("{}", resolve),
    );

    assert.equal(firstCalls, 1);
    assert.equal(first.ok, false);
    assert.match(first.error, /timed out/);
    assert.equal(workers[0].terminated, true);
    assert.equal(workers.length, 2);
    assert.equal(second.ok, true);
  } finally {
    globalThis.setTimeout = realSetTimeout;
  }
});
