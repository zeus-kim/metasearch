// Service Worker for metasearch PWA
const CACHE_NAME = 'metasearch-v3';
const STATIC_ASSETS = [
  '/',
  '/classic',
  '/manifest.json',
  '/icon-192.png',
  '/icon-512.png'
];

// Major language packs to pre-cache
const LANG_ASSETS = [
  '/lang/en.json',
  '/lang/ko.json',
  '/lang/ja.json',
  '/lang/zh.json',
  '/lang/es.json',
  '/lang/fr.json',
  '/lang/de.json'
];

// Install: cache static assets
self.addEventListener('install', event => {
  event.waitUntil(
    caches.open(CACHE_NAME).then(cache => {
      return cache.addAll([...STATIC_ASSETS, ...LANG_ASSETS]);
    }).catch(() => {
      return caches.open(CACHE_NAME).then(cache => cache.addAll(STATIC_ASSETS));
    })
  );
  self.skipWaiting();
});

// Activate: cleanup old caches
self.addEventListener('activate', event => {
  event.waitUntil(
    caches.keys().then(keys => {
      return Promise.all(
        keys.filter(k => k !== CACHE_NAME).map(k => caches.delete(k))
      );
    })
  );
  self.clients.claim();
});

// Fetch: network-first with cache fallback
self.addEventListener('fetch', event => {
  if (event.request.method !== 'GET') return;

  const url = new URL(event.request.url);

  // Skip cross-origin requests (external images, audio streams, etc.)
  if (url.origin !== self.location.origin) {
    return;
  }

  // Skip API requests (always fresh)
  if (url.pathname.startsWith('/api/') ||
      url.pathname.startsWith('/search') ||
      url.pathname.startsWith('/answer') ||
      url.pathname.startsWith('/news') ||
      url.pathname.startsWith('/trends')) {
    return;
  }

  // Cache-first for static assets
  if (url.pathname.startsWith('/lang/') ||
      url.pathname.endsWith('.png') ||
      url.pathname.endsWith('.json')) {
    event.respondWith(
      caches.match(event.request).then(cached => {
        if (cached) return cached;
        return fetch(event.request).then(response => {
          if (response.ok) {
            const clone = response.clone();
            caches.open(CACHE_NAME).then(cache => cache.put(event.request, clone));
          }
          return response;
        });
      })
    );
    return;
  }

  // Network-first for everything else
  event.respondWith(
    fetch(event.request)
      .then(response => {
        if (response.ok) {
          const clone = response.clone();
          caches.open(CACHE_NAME).then(cache => cache.put(event.request, clone));
        }
        return response;
      })
      .catch(() => caches.match(event.request))
  );
});
