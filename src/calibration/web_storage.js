const DB_NAME = "sapodilla-calibration-storage";
const DB_VERSION = 1;
const STORE_NAME = "handles";
const DIRECTORY_KEY = "calibration-directory";
const REGISTRY_FILE = "sapodilla-calibration-registry.json";
const BACKUP_FILE = "sapodilla-calibration-registry.backup.json";
let activeDirectoryHandle = null;

function supported() {
  return (
    typeof window !== "undefined" &&
    "showDirectoryPicker" in window &&
    "indexedDB" in window
  );
}

function openDatabase() {
  return new Promise((resolve, reject) => {
    const request = indexedDB.open(DB_NAME, DB_VERSION);
    request.onupgradeneeded = () => {
      const database = request.result;
      if (!database.objectStoreNames.contains(STORE_NAME)) {
        database.createObjectStore(STORE_NAME);
      }
    };
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error || new Error("Could not open calibration storage database"));
  });
}

async function databaseRequest(mode, operation) {
  const database = await openDatabase();
  try {
    return await new Promise((resolve, reject) => {
      const transaction = database.transaction(STORE_NAME, mode);
      const request = operation(transaction.objectStore(STORE_NAME));
      let result;
      request.onsuccess = () => {
        result = request.result;
      };
      request.onerror = () => reject(request.error || new Error("Calibration storage database request failed"));
      transaction.oncomplete = () => resolve(result);
      transaction.onerror = () => reject(transaction.error || new Error("Calibration storage database transaction failed"));
      transaction.onabort = () => reject(transaction.error || new Error("Calibration storage database transaction was aborted"));
    });
  } finally {
    database.close();
  }
}

async function loadHandle() {
  if (activeDirectoryHandle) {
    return activeDirectoryHandle;
  }
  activeDirectoryHandle = await databaseRequest("readonly", (store) => store.get(DIRECTORY_KEY));
  return activeDirectoryHandle;
}

async function saveHandle(handle) {
  await databaseRequest("readwrite", (store) => store.put(handle, DIRECTORY_KEY));
  activeDirectoryHandle = handle;
}

async function removeHandle() {
  await databaseRequest("readwrite", (store) => store.delete(DIRECTORY_KEY));
  activeDirectoryHandle = null;
}

async function readOptionalFile(directory, name) {
  try {
    const handle = await directory.getFileHandle(name);
    return await (await handle.getFile()).text();
  } catch (error) {
    if (error && error.name === "NotFoundError") {
      return null;
    }
    throw error;
  }
}

async function writeFile(directory, name, contents) {
  const handle = await directory.getFileHandle(name, { create: true });
  const writable = await handle.createWritable();
  try {
    await writable.write(contents);
    await writable.close();
  } catch (error) {
    await writable.abort().catch(() => {});
    throw error;
  }
}

async function snapshot(handle, requestPermission) {
  let permission = await handle.queryPermission({ mode: "readwrite" });
  if (permission !== "granted" && requestPermission) {
    permission = await handle.requestPermission({ mode: "readwrite" });
  }
  if (permission !== "granted") {
    return JSON.stringify({
      state: "permission-required",
      directory_name: handle.name,
      registry_json: null,
      backup_json: null,
    });
  }
  return JSON.stringify({
    state: "ready",
    directory_name: handle.name,
    registry_json: await readOptionalFile(handle, REGISTRY_FILE),
    backup_json: await readOptionalFile(handle, BACKUP_FILE),
  });
}

export function calibrationStorageSupported() {
  return supported();
}

export async function calibrationStorageRestore() {
  if (!supported()) {
    return JSON.stringify({ state: "unavailable", directory_name: null, registry_json: null, backup_json: null });
  }
  const handle = await loadHandle();
  if (!handle) {
    return JSON.stringify({ state: "unconfigured", directory_name: null, registry_json: null, backup_json: null });
  }
  return snapshot(handle, false);
}

export async function calibrationStorageChoose() {
  if (!supported()) {
    throw new Error("This browser does not support calibration folders");
  }
  let handle;
  try {
    handle = await window.showDirectoryPicker({
      id: "sapodilla-calibrations",
      mode: "readwrite",
      startIn: "documents",
    });
  } catch (error) {
    if (error && error.name === "AbortError") {
      return calibrationStorageRestore();
    }
    throw error;
  }
  await saveHandle(handle);
  return snapshot(handle, false);
}

export async function calibrationStorageReconnect() {
  // Startup restoration populates this module-level handle. Keeping it here
  // means requestPermission() is entered directly from the reconnect click,
  // before an IndexedDB await can consume the browser's transient activation.
  const handle = activeDirectoryHandle || await loadHandle();
  if (!handle) {
    throw new Error("No calibration folder has been selected");
  }
  const permission = await handle.requestPermission({ mode: "readwrite" });
  if (permission !== "granted") {
    return JSON.stringify({
      state: "permission-required",
      directory_name: handle.name,
      registry_json: null,
      backup_json: null,
    });
  }
  return snapshot(handle, false);
}

export async function calibrationStorageWrite(registryJson, expectedRegistryJson) {
  const handle = await loadHandle();
  if (!handle) {
    throw new Error("No calibration folder has been selected");
  }
  const permission = await handle.queryPermission({ mode: "readwrite" });
  if (permission !== "granted") {
    throw new Error("Calibration folder permission must be reconnected");
  }

  const expected = expectedRegistryJson || null;
  const currentPrimary = await readOptionalFile(handle, REGISTRY_FILE);
  const currentBackup = await readOptionalFile(handle, BACKUP_FILE);
  if (expected === null) {
    if (currentPrimary !== null || currentBackup !== null) {
      throw new Error("Calibration folder changed before the registry could be initialized");
    }
  } else if (currentPrimary === expected) {
    // Preserve the last verified primary before attempting a replacement. A
    // failed candidate must never become the recovery value.
    await writeFile(handle, BACKUP_FILE, expected);
    const backupReadback = await readOptionalFile(handle, BACKUP_FILE);
    if (backupReadback !== expected) {
      throw new Error("Calibration registry backup read-back did not match the previous data");
    }
  } else if (currentBackup !== expected) {
    throw new Error("Calibration folder changed outside Sapodilla; reconnect it before saving");
  }

  try {
    await writeFile(handle, REGISTRY_FILE, registryJson);
    const readback = await readOptionalFile(handle, REGISTRY_FILE);
    if (readback !== registryJson) {
      throw new Error("Calibration registry read-back did not match the saved data");
    }
  } catch (error) {
    // Best-effort rollback complements the durable backup. Even if rollback
    // itself cannot write, startup will reject a damaged primary and recover
    // the previous verified registry from the backup.
    if (expected !== null) {
      await writeFile(handle, REGISTRY_FILE, expected).catch(() => {});
    } else {
      await handle.removeEntry(REGISTRY_FILE).catch(() => {});
    }
    throw error;
  }

  if (expected === null) {
    await writeFile(handle, BACKUP_FILE, registryJson);
    const backupReadback = await readOptionalFile(handle, BACKUP_FILE);
    if (backupReadback !== registryJson) {
      throw new Error("Calibration registry backup read-back did not match the saved data");
    }
  }
  return handle.name;
}

export async function calibrationStorageForget() {
  if (supported()) {
    await removeHandle();
  }
}
