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
