const CACHE_PREFIX = "sapodilla-app-shell-";
const CACHE_NAME = `${CACHE_PREFIX}v2`;
const SHELL_PATHS = [
  "./",
  "./index.html",
  "./manifest.webmanifest",
  "./calibration-worker.js",
  "./icons/sapodilla-192.png",
  "./icons/sapodilla-512.png",
];

function withIsolationHeaders(response) {
  if (!response || response.status === 0) {
    return response;
  }
  const headers = new Headers(response.headers);
  headers.set("Cross-Origin-Embedder-Policy", "require-corp");
  headers.set("Cross-Origin-Opener-Policy", "same-origin");
  return new Response(response.body, {
    status: response.status,
    statusText: response.statusText,
    headers,
  });
}

self.addEventListener("install", (event) => {
  event.waitUntil(
    caches
      .open(CACHE_NAME)
      .then((cache) =>
        Promise.allSettled(
          SHELL_PATHS.map((path) => cache.add(new URL(path, self.registration.scope))),
        ),
      )
      .then(() => self.skipWaiting()),
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((keys) =>
        Promise.all(
          keys
            .filter((key) => key.startsWith(CACHE_PREFIX) && key !== CACHE_NAME)
            .map((key) => caches.delete(key)),
        ),
      )
      .then(() => self.clients.claim()),
  );
});

self.addEventListener("fetch", (event) => {
  const request = event.request;
  if (
    request.method !== "GET" ||
    (request.cache === "only-if-cached" && request.mode !== "same-origin")
  ) {
    return;
  }
  const url = new URL(request.url);
  if (url.origin !== self.location.origin) {
    return;
  }

  const isNavigation = request.mode === "navigate";
  // Only content-addressed build artifacts are immutable. Trunk's generated
  // snippet names and our worker scripts can keep the same URL while their
  // bytes (and SRI digest) change, so serving those cache-first can prevent a
  // corrected build from ever loading.
  const isImmutableAsset =
    /\/[a-z0-9_-]+-[a-f0-9]{16,}(?:_bg)?\.(?:js|wasm)$/.test(url.pathname) ||
    /\/icons\/[^/]+\.png$/.test(url.pathname);
  event.respondWith(
    (async () => {
      const cache = await caches.open(CACHE_NAME);
      if (isImmutableAsset) {
        const cached = await cache.match(request);
        if (cached) {
          return withIsolationHeaders(cached);
        }
      }

      try {
        const response = withIsolationHeaders(await fetch(request));
        if (response && response.ok) {
          await cache.put(request, response.clone());
        }
        return response;
      } catch (error) {
        const cached = await cache.match(request);
        if (cached) {
          return withIsolationHeaders(cached);
        }
        if (isNavigation) {
          const shell = await cache.match(new URL("./", self.registration.scope));
          if (shell) {
            return withIsolationHeaders(shell);
          }
        }
        throw error;
      }
    })(),
  );
});
