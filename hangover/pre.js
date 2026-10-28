// Pass node's environment through to the program (WINEPREFIX, WINEDEBUG, ...).
Module.preRun = Module.preRun || [];
Module.preRun.push(() => {
  if (typeof process !== 'undefined' && process.env) Object.assign(ENV, process.env);
});
