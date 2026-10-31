// The program's environment: node's (WINEPREFIX, WINEDEBUG, ...), or in the page the object it
// passes as `vnvmEnv`. VNVM_UNIX_MODULE (the unix side module) defaults to wineunix.so next to
// wine.js under node.
Module.preRun = Module.preRun || [];
Module.preRun.push(() => {
  if (ENVIRONMENT_IS_NODE) Object.assign(ENV, process.env);
  if (Module.vnvmEnv) Object.assign(ENV, Module.vnvmEnv);
  if (!ENV.VNVM_UNIX_MODULE && ENVIRONMENT_IS_NODE)
    ENV.VNVM_UNIX_MODULE = require('path').join(__dirname, 'wineunix.so');
});

// Regions an earlier run saved (crates/vnhost/src/jit.rs): `vnvmJit` = { module, idx } from the
// page, or under node VNVM_JIT_FILE.wasm/.idx. Every thread gets them with the other shared
// modules.
Module.preInit = [].concat(Module.preInit || [], () => {
  if (ENVIRONMENT_IS_PTHREAD) return;
  const f = ENVIRONMENT_IS_NODE && process.env.VNVM_JIT_FILE;
  if (f && !Module.vnvmJit) {
    try {
      const fs = require('fs');
      Module.vnvmJit = { module: new WebAssembly.Module(fs.readFileSync(f + '.wasm')), idx: new Uint8Array(fs.readFileSync(f + '.idx')) };
    } catch (e) {
      if (e.code != 'ENOENT') err(`vnjit: ${f}: ${e}`);
    }
  }
  if (Module.vnvmJit) sharedModules['@vnjit'] = Module.vnvmJit;
});
