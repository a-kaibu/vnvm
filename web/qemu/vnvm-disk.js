// Streaming disk backend for block/vnvm.c. Linked into the QEMU module with
// --pre-js, so it exists in every worker: the manifest is read by the main
// loop thread at open, the chunks by QEMU's thread pool workers (synchronous
// XHR is allowed there, and the main loop keeps running meanwhile). Disks are
// keyed by manifest URL, each worker opens one on first use.
//
// A manifest (crates/vnvm-pack) lists the image's chunks by content hash; ""
// marks an all-zero chunk. Chunk files are immutable, so the browser's HTTP
// cache keeps them across runs, and the server sends them zstd-encoded.
(() => {
  const disks = new Map();
  // Recently used chunks, shared by all disks. Small: the guest's page cache
  // already holds what it reads, this only covers partial-chunk reads.
  const cache = new Map();
  const CACHE_MAX = 64;
  const stats = { fetches: 0, bytes: 0, ms: 0 };

  function get(url, type) {
    const xhr = new XMLHttpRequest();
    xhr.open("GET", url, false);
    xhr.responseType = type;
    xhr.send();
    if (xhr.status !== 200) throw new Error(`${url}: HTTP ${xhr.status}`);
    return xhr.response;
  }

  globalThis.vnvmDiskOpen = (url) => {
    let d = disks.get(url);
    if (d) return d;
    const m = get(url, "json");
    const base = new URL(m.chunks_url || "chunks/", new URL(url, location.href)).href;
    d = { size: m.size, chunk: m.chunk_size, hashes: m.chunks, base };
    disks.set(url, d);
    return d;
  };

  function chunk(d, i) {
    const h = d.hashes[i];
    if (!h) return null;
    let c = cache.get(h);
    if (c) {
      cache.delete(h);
      cache.set(h, c);
      return c;
    }
    const t0 = performance.now();
    c = new Uint8Array(get(`${d.base}${h.slice(0, 2)}/${h}`, "arraybuffer"));
    stats.fetches++;
    stats.bytes += c.length;
    stats.ms += performance.now() - t0;
    cache.set(h, c);
    if (cache.size > CACHE_MAX) cache.delete(cache.keys().next().value);
    return c;
  }

  globalThis.vnvmDiskRead = (url, offset, len, heap, dst) => {
    const d = globalThis.vnvmDiskOpen(url);
    let pos = 0;
    while (pos < len) {
      const at = offset + pos;
      const i = Math.floor(at / d.chunk);
      const off = at - i * d.chunk;
      const n = Math.min(len - pos, d.chunk - off);
      const c = chunk(d, i);
      if (c) heap.set(c.subarray(off, off + n), dst + pos);
      else heap.fill(0, dst + pos, dst + pos + n);
      pos += n;
    }
  };

  globalThis.vnvmDiskStats = () => stats;
})();
