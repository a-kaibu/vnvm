//! The x86 -> wasm JIT on the host side: per thread, a block hash table read by the
//! dispatcher module (`vnx86::jit::dispatch`), and the helpers block modules import
//! (`vnhost_jit_step`, `vnhost_jit_eflags`, `vnhost_jit_status`). The modules themselves are
//! compiled and instantiated by vnjit.js; each thread has its own funcref table.
//!
//! VNVM_JIT=0 turns the JIT off (the interpreter runs everything).

use crate::cpu::Linear;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use vnx86::jit::{self, wasm::Module};
use vnx86::{Cpu, Exit};

/// Misses at an address before its block is compiled.
const THRESHOLD: u32 = 2;
/// Blocks the dispatcher runs before returning to the run loop.
const BUDGET: i32 = 1 << 14;
/// Most blocks compiled into one module.
const MAX_BLOCKS: usize = 1024;

#[cfg(target_os = "emscripten")]
extern "C" {
    fn vnvm_jit_init(ptr: *const u8, len: usize);
    fn vnvm_jit_load(ptr: *const u8, len: usize, n: u32) -> i32;
    fn vnvm_jit_run(cpu: *mut Cpu) -> i32;
}

struct State {
    /// (eip, table index); eip 0 is a free entry
    hash: Vec<[u32; 2]>,
    used: usize,
    misses: HashMap<u32, u32>,
    /// addresses with a block, or that could not be translated
    known: HashSet<u32>,
    pending: u32,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    static EXIT: Cell<Option<Exit>> = const { Cell::new(None) };
}

pub fn enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| cfg!(target_os = "emscripten") && std::env::var("VNVM_JIT").map_or(true, |v| v != "0"))
}

pub enum Ran {
    /// budget used up or a signal pending
    Stopped,
    Exit(Exit),
    /// no block at cpu.eip, or a block wants the interpreter
    Missed,
}

fn with_state<R>(pending: u32, f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let st = s.get_or_insert_with(|| {
            let d = jit::dispatch::module();
            #[cfg(target_os = "emscripten")]
            unsafe {
                vnvm_jit_init(d.as_ptr(), d.len())
            };
            let _ = d;
            State { hash: vec![[0; 2]; 1 << 12], used: 0, misses: HashMap::new(), known: HashSet::new(), pending }
        });
        f(st)
    })
}

pub fn run(cpu: &mut Cpu, pending: u32) -> Ran {
    with_state(pending, |st| {
        cpu.jit_hash = st.hash.as_ptr() as usize as u32;
        cpu.jit_mask = st.hash.len() as u32 - 1;
    });
    cpu.jit_pending = pending;
    cpu.jit_budget = BUDGET;
    #[cfg(target_os = "emscripten")]
    let r = unsafe { vnvm_jit_run(cpu) };
    #[cfg(not(target_os = "emscripten"))]
    let r = jit::RET_INTERP;
    match r {
        jit::RET_CONTINUE => Ran::Stopped,
        jit::RET_INTERP => Ran::Missed,
        _ => Ran::Exit(EXIT.with(|e| e.take()).unwrap_or(Exit::Continue)),
    }
}

fn insert(hash: &mut [[u32; 2]], eip: u32, idx: u32) {
    let mask = hash.len() as u32 - 1;
    let mut h = jit::dispatch::slot(eip, mask);
    while hash[h as usize][0] != 0 {
        h = (h + 1) & mask;
    }
    hash[h as usize] = [eip, idx];
}

/// Counts a miss at cpu.eip and compiles its block once it is warm; true if it did.
pub(crate) fn maybe_compile(cpu: &mut Cpu, mem: &mut Linear) -> bool {
    let eip = cpu.eip;
    if !(jit::GUEST_LO..jit::GUEST_HI).contains(&eip) || (eip >= cpu.trap_lo && eip < cpu.trap_hi) {
        return false;
    }
    with_state(cpu.jit_pending, |st| {
        if st.known.contains(&eip) {
            return false;
        }
        let n = st.misses.entry(eip).or_insert(0);
        *n += 1;
        if *n < THRESHOLD {
            return false;
        }
        st.misses.remove(&eip);
        // the block and what it statically reaches, as one function
        let mut m = Module::default();
        let (lo, hi) = (cpu.trap_lo, cpu.trap_hi);
        let known = &mut st.known;
        let Some(region) = jit::translate_region(&mut m, mem, eip, st.pending, MAX_BLOCKS, |a| {
            !(a >= lo && a < hi) && known.insert(a)
        }) else {
            return false;
        };
        let starts = region.starts;
        let bytes = m.finish();
        #[cfg(target_os = "emscripten")]
        let base = unsafe { vnvm_jit_load(bytes.as_ptr(), bytes.len(), 1) };
        #[cfg(not(target_os = "emscripten"))]
        let base = { let _ = bytes; -1 };
        if base < 0 {
            return false;
        }
        while (st.used + starts.len()) * 2 > st.hash.len() {
            let mut bigger = vec![[0u32; 2]; st.hash.len() * 2];
            for e in st.hash.iter().filter(|e| e[0] != 0) {
                insert(&mut bigger, e[0], e[1]);
            }
            st.hash = bigger;
        }
        for (i, &a) in starts.iter().enumerate() {
            insert(&mut st.hash, a, base as u32 | (i as u32) << jit::ENTRY_SHIFT);
        }
        st.used += starts.len();
        true
    })
}

/// One instruction run by the interpreter for a block: 0 if it completed, else 2 with the exit
/// kept for [`run`].
#[no_mangle]
pub unsafe extern "C" fn vnhost_jit_step(cpu: *mut Cpu) -> i32 {
    if stats() {
        count_step(&*cpu);
    }
    match vnx86::step_flow(&mut *cpu, &mut Linear).0 {
        Exit::Continue => 0,
        e => {
            EXIT.with(|x| x.set(Some(e)));
            2
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn vnhost_jit_eflags(cpu: *mut Cpu) -> u32 {
    let cpu = &mut *cpu;
    cpu.materialize();
    cpu.eflags
}

#[no_mangle]
pub extern "C" fn vnhost_jit_status(op: u32, a: u32, b: u32, r: u32, c: u32) -> u32 {
    vnx86::lazy::Lazy { op, a, b, r, c }.status()
}

fn stats() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VNVM_JIT_STATS").is_some())
}

/// VNVM_JIT_STATS: which instructions blocks hand to the interpreter, printed every 2^22.
fn count_step(cpu: &Cpu) {
    thread_local! {
        static COUNTS: RefCell<(u64, HashMap<String, u64>)> = RefCell::new((0, HashMap::new()));
    }
    let mut b = [0u8; 16];
    let n = vnx86::Memory::fetch(&mut Linear, cpu.eip, &mut b);
    let mut d = vnx86::iced_x86::Decoder::with_ip(32, &b[..n], cpu.eip as u64, vnx86::iced_x86::DecoderOptions::NONE);
    let ins = d.decode();
    let key = format!("{:?}", ins.mnemonic());
    COUNTS.with(|c| {
        let mut c = c.borrow_mut();
        *c.1.entry(key).or_insert(0) += 1;
        c.0 += 1;
        if c.0 % (1 << 22) == 0 {
            let mut v: Vec<_> = c.1.iter().collect();
            v.sort_by(|a, b| b.1.cmp(a.1));
            let top: Vec<String> = v.iter().take(20).map(|(k, n)| format!("{k}:{n}")).collect();
            eprintln!("vnjit steps {}: {}", c.0, top.join(" "));
        }
    });
}
