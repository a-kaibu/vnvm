// Pass node's environment through to the program (WINEPREFIX, WINEDEBUG, ...), and point it
// at the unix side module that sits next to this file.
Module.preRun = Module.preRun || [];
Module.preRun.push(() => {
  if (typeof process !== 'undefined' && process.env) Object.assign(ENV, process.env);
  if (!ENV.VNVM_UNIX_MODULE && typeof __dirname !== 'undefined')
    ENV.VNVM_UNIX_MODULE = require('path').join(__dirname, 'wineunix.so');
});
