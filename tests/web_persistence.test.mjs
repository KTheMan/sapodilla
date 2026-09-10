import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

async function importSource(path, tag) {
  const source = await readFile(new URL(path, import.meta.url), "utf8");
  const encoded = Buffer.from(`${source}\n//# sourceURL=${path}?${tag}`).toString("base64");
  return import(`data:text/javascript;base64,${encoded}#${tag}`);
}

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
  globalThis.self = {
    registration: { scope: "https://example.test/sapodilla/" },
    clients: { claim: async () => {} },
    addEventListener: (name, handler) => handlers.set(name, handler),
  };
  globalThis.caches = {
    keys: async () => [
      "sapodilla-app-shell-v0",
      "sapodilla-app-shell-v1",
      "sapodilla-app-shell-v2",
      "another-project-app-shell-v9",
    ],
    delete: async (key) => {
      deleted.push(key);
      return true;
    },
  };

  await importSource("../static/coi-serviceworker.js", "cache-ownership");
  let activation;
  handlers.get("activate")({ waitUntil: (promise) => (activation = promise) });
  await activation;
  assert.deepEqual(deleted, [
    "sapodilla-app-shell-v0",
    "sapodilla-app-shell-v1",
  ]);
});

test("calibration client reuses one isolated worker and transfers scan bytes", async () => {
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

    terminate() {}
  };

  const client = await importSource(
    "../src/calibration/isolated_worker.js",
    "persistent-worker",
  );
  const scan = await new Promise((resolve) =>
    client.isolatedCalibrationScan(new Uint8Array([1, 2, 3]), "{}", "{}", resolve),
  );
  const print = await new Promise((resolve) =>
    client.isolatedCalibrationPrint("{}", resolve),
  );

  assert.equal(scan.ok, true);
  assert.equal(print.ok, true);
  assert.equal(workers.length, 1);
  assert.equal(workers[0].options.type, "module");
  assert.equal(workers[0].messages[0].message.wasmUrl,
    "https://example.test/sapodilla/sapodilla-0123456789abcdef_bg.wasm");
  const scanMessage = workers[0].messages.find(
    ({ message }) => message.kind === "scan",
  );
  assert.equal(scanMessage.transfer.length, 1);
  assert.equal(scanMessage.transfer[0], scanMessage.message.bytes);
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
