// Per-process unix modules, loaded only by the threads of their process.
//
// Every Wine process dlopen()s its own instance of the unix module (/tmp/vnvm-<uid>/unix-<t>-<pid>.so,
// see crates/vnhost/c/shim.c), and Emscripten loads every side module on every thread so that
// function pointers (table slots) mean the same everywhere. Instantiating every process's
// instance on every new thread was most of the startup CPU, and only the threads of a process
// call into its instance. Here a thread of another process reserves the module's table slots
// instead, and loads it when it enters that process (vnvm_dl_enter).
//
// That needs the table to grow the same way on every thread, whichever modules it loaded:
// - a module's slots start at its table_addr (set by the thread that loaded it first);
// - the unix module's exports get no GOT entries (nobody imports them; Emscripten would add a
//   slot for each on the first module a thread loads);
// - the unix module has no GOT.func imports (build.sh checks);
// - a dlsym() slot (catch-up) is reserved on threads that skipped the module, and filled if
//   they load it.
addToLibrary({
  $VNDL: { unix: false, done: {}, skipped: {}, slots: {}, caught: {} },

  $vndlUnixPid: (handle) => {
    const name = UTF8ToString(handle + {{{ C_STRUCTS.dso.name }}});
    const m = /\/unix-\d+-(\d+)\.so$/.exec(name);
    return m ? Number(m[1]) : 0;
  },

  // the table up to where `handle`'s slots start
  $vndlAlign: (handle) => {
    const at = {{{ makeGetValue('handle', C_STRUCTS.dso.table_addr, '*') }}};
    if (wasmTable.length < at) wasmTable.grow(at - wasmTable.length);
    else if (wasmTable.length > at) throw new Error(`vndl: table at ${wasmTable.length}, module at ${at}`);
  },

  _dlopen_js__deps: ['$VNDL', '$dlopenInternal', '$vndlUnixPid', '$vndlAlign', 'vnhost_getpid'],
  _dlopen_js: (handle) => {
    const pid = vndlUnixPid(handle);
    if (!pid) return dlopenInternal(handle, { loadAsync: false });
    // a worker runs one thread after another, and each syncs from the start
    if (VNDL.done[handle]) return handle;
    VNDL.done[handle] = 1;
    if ({{{ makeGetValue('handle', C_STRUCTS.dso.mem_allocated, 'i8') }}}) {
      // loaded before (on another thread): its slots come next
      vndlAlign(handle);
      if (pid != _vnhost_getpid()) {
        wasmTable.grow({{{ makeGetValue('handle', C_STRUCTS.dso.table_size, 'i32') }}});
        VNDL.skipped[pid] = handle;
        VNDL.slots[handle] = [];
        return handle;
      }
    }
    VNDL.unix = true;
    try {
      return dlopenInternal(handle, { loadAsync: false });
    } finally {
      VNDL.unix = false;
    }
  },

  // the JS main thread syncs asynchronously; it runs no Wine code (its table may differ), so it
  // loads no unix module at all
  _emscripten_dlopen_js__deps: ['$VNDL', '$vndlUnixPid', '$dlopenInternal', '$callUserCallback', '$dlSetError'],
  _emscripten_dlopen_js: (handle, onsuccess, onerror, user_data) => {
    const done = (ok) => {
      {{{ runtimeKeepalivePop() }}}
      callUserCallback(() => (ok ? {{{ makeDynCall('vpp', 'onsuccess') }}} : {{{ makeDynCall('vpp', 'onerror') }}})(handle, user_data));
    };
    {{{ runtimeKeepalivePush() }}}
    if (vndlUnixPid(handle)) {
      VNDL.slots[handle] = [];
      return void setTimeout(() => done(true), 0);
    }
    const promise = dlopenInternal(handle, { loadAsync: true });
    if (!promise) return done(false);
    promise.then(() => done(true), (e) => {
      dlSetError(`Could not load dynamic lib: ${UTF8ToString(handle + {{{ C_STRUCTS.dso.name }}})}\n${e}`);
      done(false);
    });
  },

  _dlsym_catchup_js__deps: ['$VNDL', '$addFunction', '$getEmptyTableSlot'],
  _dlsym_catchup_js: (handle, symbolIndex) => {
    // (replayed for each thread a worker runs)
    const key = `${handle}:${symbolIndex}`;
    if (key in VNDL.caught) return VNDL.caught[key];
    const pending = VNDL.slots[handle];
    if (pending) {
      pending.push([(VNDL.caught[key] = getEmptyTableSlot()), symbolIndex]);
      return VNDL.caught[key];
    }
    const sym = Object.values(LDSO.loadedLibsByHandle[handle].exports)[symbolIndex];
    return (VNDL.caught[key] = addFunction(sym, sym.sig));
  },

  // Emscripten's, without GOT entries for the unix module's functions
  $updateGOT__deps: ['$VNDL', '$GOT', '$isInternalSym', '$addFunction'],
  $updateGOT: (exports, replace) => {
    for (const symName in exports) {
      if (isInternalSym(symName)) continue;
      const value = exports[symName];
      if (VNDL.unix && typeof value == 'function') continue;
      GOT[symName] ||= new WebAssembly.Global({ value: 'i32', mutable: true });
      if (replace || GOT[symName].value == 0) {
        if (typeof value == 'function') GOT[symName].value = addFunction(value);
        else if (typeof value == 'number') GOT[symName].value = value;
        else err(`unhandled export type for '${symName}': ${typeof value}`);
      }
    }
  },

  vnvm_dl_enter__deps: ['$VNDL', '$dlopenInternal', '$setWasmTableEntry'],
  vnvm_dl_enter__sig: 'vi',
  vnvm_dl_enter: (pid) => {
    const handle = VNDL.skipped[pid];
    if (!handle) return;
    delete VNDL.skipped[pid];
    // its slots are reserved: the module goes into them (Emscripten's grow for it is skipped),
    // nothing else may grow the table
    const grow = wasmTable.grow;
    let size = {{{ makeGetValue('handle', C_STRUCTS.dso.table_size, 'i32') }}};
    wasmTable.grow = (n) => {
      if (n != size) throw new Error('vndl: table grown while entering a process');
      size = -1;
      return wasmTable.length;
    };
    try {
      VNDL.unix = true;
      if (!dlopenInternal(handle, { loadAsync: false })) throw new Error(`vndl: cannot load process ${pid}`);
    } finally {
      VNDL.unix = false;
      wasmTable.grow = grow;
    }
    const exports = Object.values(LDSO.loadedLibsByHandle[handle].exports);
    for (const [slot, i] of VNDL.slots[handle]) setWasmTableEntry(slot, exports[i]);
    delete VNDL.slots[handle];
  },
});
