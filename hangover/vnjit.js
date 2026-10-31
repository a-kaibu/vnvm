// The JIT's module loader (see crates/vnhost/src/jit.rs). Every thread (worker) has its own
// funcref table, dispatcher instance and block instances; all share the one memory.
addToLibrary({
  $VNJIT: { table: null, used: 0, run: null, env: null },

  vnvm_jit_init__deps: ['$VNJIT'],
  vnvm_jit_init__sig: 'vpp',
  vnvm_jit_init: (ptr, len) => {
    ptr >>>= 0;
    const bytes = HEAPU8.slice(ptr, ptr + len);
    VNJIT.table = new WebAssembly.Table({ element: 'anyfunc', initial: 0 });
    VNJIT.used = 0;
    const inst = new WebAssembly.Instance(new WebAssembly.Module(bytes),
      { env: { memory: wasmMemory, table: VNJIT.table } });
    VNJIT.run = inst.exports.f0;
    VNJIT.env = {
      memory: wasmMemory,
      step: wasmExports.vnhost_jit_step,
      eflags: wasmExports.vnhost_jit_eflags,
      status: wasmExports.vnhost_jit_status,
      table: VNJIT.table,
    };
  },

  vnvm_jit_load__deps: ['$VNJIT'],
  vnvm_jit_load__sig: 'ippi',
  vnvm_jit_load: (ptr, len, n) => {
    ptr >>>= 0;
    let inst;
    try {
      inst = new WebAssembly.Instance(new WebAssembly.Module(HEAPU8.slice(ptr, ptr + len)), { env: VNJIT.env });
    } catch (e) {
      err(`vnjit: ${e}`);
      return -1;
    }
    // grown by half again each time: growing copies the table
    const base = VNJIT.used;
    if (base + n > VNJIT.table.length) VNJIT.table.grow(Math.max(n, VNJIT.table.length >> 1, 256));
    VNJIT.used += n;
    for (let i = 0; i < n; i++) VNJIT.table.set(base + i, inst.exports['f' + i]);
    return base;
  },

  vnvm_jit_run__deps: ['$VNJIT'],
  vnvm_jit_run__sig: 'ip',
  vnvm_jit_run: (cpu) => VNJIT.run(cpu),
});
