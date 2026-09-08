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
  assert.deepEqual(deleted, ["sapodilla-app-shell-v0"]);
});
