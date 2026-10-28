// vnvm_fetch(name, &len): one file of the store (trees/<name>.tsv or chunks/ab/<hash>, the
// latter brotli-compressed), for crates/vnhost/src/vfs.rs. Returns a malloc'ed buffer, or 0.
//
// node: read from $VNVM_STORE. Browser (a worker, so synchronous XHR is allowed): fetch from
// store/ beside the directory of wine.js; the server sends chunks with "Content-Encoding: br",
// so the browser decompresses them.
addToLibrary({
  vnvm_fetch__deps: ['malloc'],
  // pointers are above 2GB (GLOBAL_BASE): 'p' makes them unsigned
  vnvm_fetch__sig: 'ppp',
  vnvm_fetch: (name, lenp) => {
    name = UTF8ToString(name >>> 0);
    // (not ENV: each thread has its own copy, filled only on the main one)
#if EXPORT_ES6
    const store = new URL('../store', import.meta.url).href;
#else
    const store = process.env.VNVM_STORE || 'store';
#endif
    let data;
    try {
      if (ENVIRONMENT_IS_NODE) {
        data = require('fs').readFileSync(require('path').join(store, name));
        if (name.startsWith('chunks/')) data = require('zlib').brotliDecompressSync(data);
      } else {
        const x = new XMLHttpRequest();
        x.open('GET', store + '/' + name, false);
        // (a binary responseType is refused for synchronous requests on the page's own thread)
        x.overrideMimeType('text/plain; charset=x-user-defined');
        x.send();
        if (x.status !== 200) throw new Error(`HTTP ${x.status}`);
        const t = x.responseText;
        data = new Uint8Array(t.length);
        for (let i = 0; i < t.length; i++) data[i] = t.charCodeAt(i);
      }
    } catch (e) {
      if (!ENVIRONMENT_IS_NODE) err(`vnvm_fetch ${name}: ${e}`);
      return 0;
    }
    const p = _malloc(Math.max(data.length, 1));
    if (!p) return 0;
    HEAPU8.set(data, p >>> 0);
    HEAPU32[lenp >>> 2] = data.length;
    return p;
  },
});
