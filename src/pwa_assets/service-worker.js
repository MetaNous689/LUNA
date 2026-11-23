"use strict";

const CACHE = "luna-monitor-shell-v10";
const SHELL = [
  "/",
  "/assets/app.css?v=luna-limits-20261123",
  "/assets/app.js?v=luna-limits-20261123",
  "/manifest.webmanifest?v=icon-20261022-ringed-planet-cropped",
  "/icon.png?v=icon-20261022-ringed-planet-cropped",
  "/icon-192.png?v=icon-20261022-ringed-planet-cropped",
  "/icon-512.png?v=icon-20261022-ringed-planet-cropped",
];

self.addEventListener("install", (event) => {
  event.waitUntil(caches.open(CACHE).then((cache) => cache.addAll(SHELL)).then(() => self.skipWaiting()));
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches.keys()
      .then((keys) => Promise.all(keys.filter((key) => (key.startsWith("luna-monitor-shell-")) && key !== CACHE).map((key) => caches.delete(key))))
      .then(() => self.clients.claim()),
  );
});

self.addEventListener("fetch", (event) => {
  const request = event.request;
  const url = new URL(request.url);
  if (request.method !== "GET" || url.origin !== self.location.origin || url.pathname.startsWith("/api/")) return;

  if (request.mode === "navigate") {
    event.respondWith(
      fetch(request, { cache: "no-store" })
        .then((response) => {
          if (response.ok) {
            const copy = response.clone();
            event.waitUntil(caches.open(CACHE).then((cache) => cache.put("/", copy)));
          }
          return response;
        })
        .catch(() => caches.match("/")),
    );
    return;
  }

  event.respondWith(caches.match(request).then((cached) => cached || fetch(request)));
});
