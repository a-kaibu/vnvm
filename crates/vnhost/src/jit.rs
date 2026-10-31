//! The x86 -> wasm JIT on the host side: per thread, a block hash table read by the
//! dispatcher module (`vnx86::jit::dispatch`), and the helpers block modules import
//! (`vnhost_jit_step`, `vnhost_jit_eflags`, `vnhost_jit_status`). The modules themselves are
//! compiled and instantiated by vnjit.js; each thread has its own funcref table.
//!
//! VNVM_JIT=0 turns the JIT off (the interpreter runs everything). VNVM_JIT_DUMP=dir writes every
//! region module to dir/<first root>-<roots>.wasm. VNVM_JIT_LAYOUT=1 prints the block order, loops
//! and wasm bytes per mnemonic of new regions (of merged ones too with VNVM_JIT_STATS).
//!
//! Regions are kept across runs: with VNVM_JIT_URL set, the regions built since the last save are
//! sent there now and then (see [`maybe_save`]; vnvm-server merges them into what it has), and the
//! page (or pre.js under node) hands the next run that module and index (`jit::saved`). A
//! saved region goes in when its first root first misses, if the guest code is still the same:
//! no interpreter warm-up and no translation, and in Chromium the browser's code cache of the
//! module keeps V8's optimized code too.

use crate::cpu::Linear;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use vnx86::jit::{self, wasm::Module};
use vnx86::{Cpu, Exit};

/// Misses at an address before its block is compiled (VNVM_JIT_THRESHOLD overrides).
const THRESHOLD: u32 = 50;

fn threshold() -> u32 {
    use std::sync::OnceLock;
    static T: OnceLock<u32> = OnceLock::new();
    *T.get_or_init(|| std::env::var("VNVM_JIT_THRESHOLD").ok().and_then(|v| v.parse().ok()).unwrap_or(THRESHOLD))
}
/// Blocks the dispatcher runs before returning to the run loop.
const BUDGET: i32 = 1 << 14;
/// Most blocks compiled into one module.
const MAX_BLOCKS: usize = 1024;

#[cfg(target_os = "emscripten")]
extern "C" {
    fn vnvm_jit_init(ptr: *const u8, len: usize);
    fn vnvm_jit_load(ptr: *const u8, len: usize, n: u32) -> i32;
    fn vnvm_jit_run(cpu: *mut Cpu) -> i32;
    fn vnvm_jit_bundle_idx(ptr: *mut u8, cap: usize) -> i32;
    fn vnvm_jit_bundle_load() -> i32;
    fn vnvm_jit_save(url: *const std::ffi::c_char, ptr: *const u8, len: usize);
}

struct State {
    /// (eip, table index); eip 0 is a free entry
    hash: Vec<[u32; 2]>,
    used: usize,
    misses: HashMap<u32, u32>,
    /// addresses with a block, or that could not be translated
    known: HashSet<u32>,
    /// region (table index) -> the addresses it was built from
    roots: HashMap<u32, Vec<u32>>,
    /// jumps between regions, written by the regions: count, then (from, to) pairs
    ring: Box<[u32; 2 + 2 * jit::RING_SIZE as usize]>,
    /// (region, target) pairs already merged, and targets that did not fit
    merged: HashSet<(u32, u32)>,
    /// ring windows each hot jump topped so far
    hot: HashMap<(u32, u32), u32>,
    merges: u32,
    /// table index of the saved regions' first function on this thread, once instantiated
    saved: Option<i32>,
}

/// Samples of the same jump between regions (out of RING_SIZE) that make them merge.
const MERGE_SAMPLES: usize = 12;
/// Ring windows a jump must top that way before the merge: startup code has jumps that are
/// hot for a moment only, and each merge is a new region V8 compiles again.
const MERGE_WINDOWS: u32 = 8;
/// Merges per thread, at most (each compiles a new region).
const MAX_MERGES: u32 = 512;

/// A region module any thread built, kept for the others: every thread (and every process,
/// for the DLLs they share) would otherwise translate the same code again.
struct Shared {
    bytes: Vec<u8>,
    starts: Vec<(u32, u32)>,
    code: Vec<(u32, u32)>,
    /// the guest code bytes of `code`, concatenated
    text: Vec<u8>,
}

/// Shared regions by their roots.
static SHARED: Mutex<Option<HashMap<Vec<u32>, Vec<Arc<Shared>>>>> = Mutex::new(None);

fn code_bytes(mem: &mut Linear, code: &[(u32, u32)]) -> Option<Vec<u8>> {
    let mut text = Vec::new();
    for &(a, n) in code {
        let at = text.len();
        text.resize(at + n as usize, 0);
        if vnx86::Memory::fetch(mem, a, &mut text[at..]) != n as usize {
            return None;
        }
    }
    Some(text)
}

/// A region for `roots` some thread built from the same code as ours.
fn shared(mem: &mut Linear, roots: &[u32]) -> Option<Arc<Shared>> {
    let list = SHARED.lock().unwrap().as_ref()?.get(roots)?.clone();
    list.into_iter().find(|r| code_bytes(mem, &r.code).is_some_and(|t| t == r.text))
}

fn share(mem: &mut Linear, roots: &[u32], bytes: Vec<u8>, region: &jit::Region) {
    let Some(text) = code_bytes(mem, &region.code) else { return };
    let r = Arc::new(Shared { bytes, starts: region.starts.clone(), code: region.code.clone(), text });
    SHARED.lock().unwrap().get_or_insert_with(HashMap::new).entry(roots.to_vec()).or_default().push(r);
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

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let st = s.get_or_insert_with(|| {
            let d = jit::dispatch::module();
            #[cfg(target_os = "emscripten")]
            unsafe {
                vnvm_jit_init(d.as_ptr(), d.len())
            };
            let _ = d;
            State {
                hash: vec![[0; 2]; 1 << 12],
                used: 0,
                misses: HashMap::new(),
                known: HashSet::new(),
                roots: HashMap::new(),
                ring: Box::new([0; 2 + 2 * jit::RING_SIZE as usize]),
                merged: HashSet::new(),
                hot: HashMap::new(),
                merges: 0,
                saved: None,
            }
        });
        f(st)
    })
}

pub fn run(cpu: &mut Cpu, pending: u32) -> Ran {
    with_state(|st| {
        if st.ring[0] >= jit::RING_SIZE {
            merge_hot(st, cpu);
        }
        cpu.jit_hash = st.hash.as_ptr() as usize as u32;
        cpu.jit_mask = st.hash.len() as u32 - 1;
        cpu.jit_ring = st.ring.as_mut_ptr() as usize as u32;
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

/// Sets the entry of `eip`; true if it is new.
fn insert(hash: &mut [[u32; 2]], eip: u32, idx: u32) -> bool {
    let mask = hash.len() as u32 - 1;
    let mut h = jit::dispatch::slot(eip, mask);
    while hash[h as usize][0] != 0 && hash[h as usize][0] != eip {
        h = (h + 1) & mask;
    }
    let new = hash[h as usize][0] == 0;
    hash[h as usize] = [eip, idx];
    new
}

/// Compiles `module` and points the hash table at its region's blocks.
fn load(st: &mut State, bytes: &[u8], roots: Vec<u32>, starts: &[(u32, u32)]) -> bool {
    if let Some(dir) = std::env::var_os("VNVM_JIT_DUMP") {
        let name = format!("{:08x}-{}.wasm", roots[0], roots.len());
        let _ = std::fs::write(std::path::Path::new(&dir).join(name), bytes);
    }
    #[cfg(target_os = "emscripten")]
    let base = unsafe { vnvm_jit_load(bytes.as_ptr(), bytes.len(), 1) };
    #[cfg(not(target_os = "emscripten"))]
    let base = { let _ = bytes; -1 };
    if base < 0 {
        return false;
    }
    enter(st, base as u32, roots, starts);
    true
}

/// Points the hash table at the blocks of the region at table index `base`.
fn enter(st: &mut State, base: u32, roots: Vec<u32>, starts: &[(u32, u32)]) {
    for &(a, _) in starts {
        st.known.insert(a);
    }
    while (st.used + starts.len()) * 2 > st.hash.len() {
        let mut bigger = vec![[0u32; 2]; st.hash.len() * 2];
        for e in st.hash.iter().filter(|e| e[0] != 0) {
            insert(&mut bigger, e[0], e[1]);
        }
        st.hash = bigger;
    }
    for &(a, i) in starts {
        if insert(&mut st.hash, a, base | i << jit::ENTRY_SHIFT) {
            st.used += 1;
        }
    }
    st.roots.insert(base, roots);
}

/// The saved regions (empty without any).
struct Saved {
    entries: Vec<jit::saved::Entry>,
    /// entries by their first root
    by_root: HashMap<u32, Vec<usize>>,
}

fn saved_regions() -> &'static Saved {
    static S: std::sync::OnceLock<Saved> = std::sync::OnceLock::new();
    S.get_or_init(|| {
        #[cfg(target_os = "emscripten")]
        let entries = unsafe {
            let n = vnvm_jit_bundle_idx(std::ptr::null_mut(), 0);
            let mut b = vec![0u8; n.max(0) as usize];
            if n > 0 {
                vnvm_jit_bundle_idx(b.as_mut_ptr(), b.len());
            }
            jit::saved::decode(&b).unwrap_or_default()
        };
        #[cfg(not(target_os = "emscripten"))]
        let entries = Vec::new();
        let mut by_root: HashMap<u32, Vec<usize>> = HashMap::new();
        for (i, e) in entries.iter().enumerate() {
            by_root.entry(e.roots[0]).or_default().push(i);
        }
        Saved { entries, by_root }
    })
}

/// A saved region for `roots` (any built from `roots[0]` when `exact` is off: the one with most
/// roots) whose guest code is still there, and clear of the trap range `trap`.
fn find_saved(mem: &mut Linear, roots: &[u32], exact: bool, trap: (u32, u32)) -> Option<usize> {
    let s = saved_regions();
    s.by_root
        .get(&roots[0])?
        .iter()
        .copied()
        .filter(|&i| !exact || s.entries[i].roots == roots)
        .filter(|&i| s.entries[i].code.iter().all(|&(a, n)| a.saturating_add(n) <= trap.0 || a >= trap.1))
        .filter(|&i| code_bytes(mem, &s.entries[i].code).is_some_and(|t| t == s.entries[i].text))
        .max_by_key(|&i| s.entries[i].roots.len())
}

/// Puts saved region `i` in (instantiating the saved module on this thread first).
fn load_saved(st: &mut State, i: usize) -> bool {
    #[cfg(target_os = "emscripten")]
    let base = *st.saved.get_or_insert_with(|| unsafe { vnvm_jit_bundle_load() });
    #[cfg(not(target_os = "emscripten"))]
    let base = -1;
    if base < 0 {
        return false;
    }
    let e = &saved_regions().entries[i];
    enter(st, base as u32 + i as u32, e.roots.clone(), &e.starts);
    true
}

/// Sends the regions built since the last save to VNVM_JIT_URL (as one module and its index)
/// once the run is past its start, at most every SAVE_EVERY and only when there are enough to
/// be worth it: each save changes the module, and Chromium then forgets its compiled code.
fn maybe_save() {
    const SAVE_AFTER: Duration = Duration::from_secs(20);
    const SAVE_EVERY: Duration = Duration::from_secs(30);
    const SAVE_MIN: usize = 32;
    static URL: std::sync::OnceLock<Option<std::ffi::CString>> = std::sync::OnceLock::new();
    static SAVE: Mutex<(Option<Instant>, Option<Instant>, Option<HashSet<Vec<u32>>>, bool)> = Mutex::new((None, None, None, false));
    let Some(url) = URL.get_or_init(|| std::env::var("VNVM_JIT_URL").ok().and_then(|u| std::ffi::CString::new(u).ok())) else {
        return;
    };
    let Ok(mut guard) = SAVE.try_lock() else { return };
    let g = &mut *guard;
    let now = Instant::now();
    let start = *g.0.get_or_insert(now);
    if now - start < SAVE_AFTER || g.1.is_some_and(|t| now - t < SAVE_EVERY) {
        return;
    }
    let done = g.2.get_or_insert_with(HashSet::new);
    let new: Vec<(Vec<u32>, Arc<Shared>)> = {
        let sh = SHARED.lock().unwrap();
        let Some(sh) = sh.as_ref() else { return };
        sh.iter().filter(|(r, _)| !done.contains(*r)).map(|(r, l)| (r.clone(), l[0].clone())).collect()
    };
    if new.len() < SAVE_MIN {
        return;
    }
    g.1 = Some(now);
    let entries: Vec<jit::saved::Entry> = new
        .iter()
        .map(|(r, s)| jit::saved::Entry { roots: r.clone(), starts: s.starts.clone(), code: s.code.clone(), text: s.text.clone() })
        .collect();
    let mods: Vec<&[u8]> = new.iter().map(|(_, s)| &s.bytes[..]).collect();
    let names: Vec<String> = entries.iter().map(|e| e.name()).collect();
    let Some(wasm) = jit::wasm::merge(&mods, &names) else { return };
    for (r, _) in new {
        done.insert(r);
    }
    let idx = jit::saved::encode(&entries);
    let mut body = (wasm.len() as u32).to_le_bytes().to_vec();
    body.extend_from_slice(&wasm);
    body.extend_from_slice(&idx);
    // the first time, the store chunks the run read so far too (for the page to fetch ahead)
    let chunks = if g.3 { None } else { Some(crate::vfs::fetched()) };
    g.3 = true;
    #[cfg(target_os = "emscripten")]
    unsafe {
        vnvm_jit_save(url.as_ptr(), body.as_ptr(), body.len());
        if let Some(c) = &chunks {
            let u = std::ffi::CString::new(format!("{}.chunks", url.to_str().unwrap_or(""))).unwrap();
            vnvm_jit_save(u.as_ptr(), c.as_ptr(), c.len());
        }
    };
    let _ = (url, body, chunks);
}

/// Whether the block at `a` ran: the interpreter stops at every branch, so each block that
/// ran before it was compiled missed at least once. Regions take only those (a third of the
/// code they reached statically ran, and V8 compiles all of it); the rest leave the region.
fn ran(misses: &HashMap<u32, u32>, known: &HashSet<u32>, a: u32) -> bool {
    misses.contains_key(&a) || known.contains(&a)
}

/// The jumps between regions the ring saw most: a frequent one gets a new region holding both
/// sides (the source region's roots plus the target), so it becomes a jump inside a function.
fn merge_hot(st: &mut State, cpu: &Cpu) {
    const N: usize = jit::RING_SIZE as usize;
    let mut jumps = [0u64; N];
    for (i, j) in jumps.iter_mut().enumerate() {
        *j = (st.ring[1 + 2 * i] as u64) << 32 | st.ring[2 + 2 * i] as u64;
    }
    st.ring[0] = 0;
    // the most frequent pair (runs of equal ones once sorted)
    jumps.sort_unstable();
    let (mut best, mut c, mut run) = (jumps[0], 0, 0);
    for i in 0..N {
        run = if i > 0 && jumps[i] == jumps[i - 1] { run + 1 } else { 1 };
        if run > c {
            (best, c) = (jumps[i], run);
        }
    }
    let (from, to) = ((best >> 32) as u32, best as u32);
    if c < MERGE_SAMPLES || st.merges >= MAX_MERGES {
        return;
    }
    let w = st.hot.entry((from, to)).or_insert(0);
    *w += 1;
    if *w < MERGE_WINDOWS || !st.merged.insert((from, to)) || !st.merged.insert((u32::MAX, to)) {
        return;
    }
    let Some(roots) = st.roots.get(&from) else { return };
    let mut roots = roots.clone();
    roots.push(to);
    if let Some(r) = shared(&mut Linear, &roots) {
        st.merges += 1;
        load(st, &r.bytes, roots, &r.starts);
        return;
    }
    if let Some(i) = find_saved(&mut Linear, &roots, true, (cpu.trap_lo, cpu.trap_hi)) {
        st.merges += 1;
        load_saved(st, i);
        return;
    }
    let (lo, hi) = (cpu.trap_lo, cpu.trap_hi);
    let mut m = Module { name: Some(format!("x86_{:08x}_{}", roots[0], roots.len())), ..Module::default() };
    if std::env::var_os("VNVM_JIT_LAYOUT").is_some() {
        jit::LAYOUT.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let (misses, known) = (&st.misses, &st.known);
    let Some(region) = jit::translate_region(&mut m, &mut Linear, &roots, MAX_BLOCKS, |a| {
        !(a >= lo && a < hi) && (roots.contains(&a) || ran(misses, known, a))
    }) else {
        return;
    };
    if !region.starts.iter().any(|s| s.0 == to) {
        return; // too big already
    }
    st.merges += 1;
    if stats() {
        eprintln!("vnjit merge {from:#x} -> {to:#x}: {} blocks, roots {:x?}\n{}", region.starts.len(), roots, region.layout);
    }
    let bytes = m.finish();
    if load(st, &bytes, roots.clone(), &region.starts) {
        share(&mut Linear, &roots, bytes, &region);
        maybe_save();
    }
}

/// Counts a miss at cpu.eip and compiles its block once it is warm; true if it did.
pub(crate) fn maybe_compile(cpu: &mut Cpu, mem: &mut Linear) -> bool {
    let eip = cpu.eip;
    if !(jit::GUEST_LO..jit::GUEST_HI).contains(&eip) || (eip >= cpu.trap_lo && eip < cpu.trap_hi) {
        return false;
    }
    with_state(|st| {
        if st.known.contains(&eip) {
            return false;
        }
        // another thread's region from the same code goes in at once
        if st.misses.get(&eip).is_none() {
            if let Some(r) = shared(mem, &[eip]) {
                return load(st, &r.bytes, vec![eip], &r.starts);
            }
            if let Some(i) = find_saved(mem, &[eip], false, (cpu.trap_lo, cpu.trap_hi)) {
                if load_saved(st, i) {
                    return true;
                }
            }
        }
        let n = st.misses.entry(eip).or_insert(0);
        *n += 1;
        if *n < threshold() {
            return false;
        }
        st.misses.remove(&eip);
        // the block and what it statically reaches, as one function
        let mut m = Module { name: Some(format!("x86_{eip:08x}")), ..Module::default() };
        if std::env::var_os("VNVM_JIT_LAYOUT").is_some() {
            jit::LAYOUT.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let (lo, hi) = (cpu.trap_lo, cpu.trap_hi);
        let (misses, known) = (&st.misses, &st.known);
        let Some(region) = jit::translate_region(&mut m, mem, &[eip], MAX_BLOCKS, |a| {
            !(a >= lo && a < hi) && !known.contains(&a) && (a == eip || misses.contains_key(&a))
        }) else {
            st.known.insert(eip);
            return false;
        };
        if std::env::var_os("VNVM_JIT_LAYOUT").is_some() {
            eprintln!("vnjit fresh {eip:#x}\n{}", region.layout);
        }
        let bytes = m.finish();
        let ok = load(st, &bytes, vec![eip], &region.starts);
        if ok {
            share(mem, &[eip], bytes, &region);
            maybe_save();
        } else {
            st.known.insert(eip);
        }
        ok
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
    let pre = if ins.has_lock_prefix() {
        "lock "
    } else if ins.has_rep_prefix() || ins.has_repe_prefix() || ins.has_repne_prefix() {
        "rep "
    } else {
        ""
    };
    let key = format!("{pre}{:?}", ins.mnemonic());
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

/// VNVM_JIT_STATS: where the JIT hands back to the run loop, printed every 2^20.
pub(crate) fn count_miss(eip: u32) {
    if !stats() {
        return;
    }
    thread_local! {
        static MISSES: RefCell<(u64, HashMap<u32, u64>)> = RefCell::new((0, HashMap::new()));
    }
    MISSES.with(|c| {
        let mut c = c.borrow_mut();
        *c.1.entry(eip).or_insert(0) += 1;
        c.0 += 1;
        if c.0 % (1 << 20) == 0 {
            let mut v: Vec<_> = c.1.iter().collect();
            v.sort_by(|a, b| b.1.cmp(a.1));
            let top: Vec<String> = v.iter().take(12).map(|(k, n)| format!("{k:#x}:{n}")).collect();
            eprintln!("vnjit misses {}: {}", c.0, top.join(" "));
        }
    });
}

/// VNVM_JIT_STATS: entries into the run loop per thread (a guest spinning on system calls
/// shows up here), printed every 2^16 with the addresses seen most.
pub(crate) fn count_entry(eip: u32) {
    if !stats() {
        return;
    }
    thread_local! {
        static ENTRIES: RefCell<(u64, HashMap<u32, u64>, Option<std::time::Instant>)> = RefCell::new((0, HashMap::new(), None));
    }
    ENTRIES.with(|c| {
        let mut c = c.borrow_mut();
        *c.1.entry(eip).or_insert(0) += 1;
        c.0 += 1;
        let t0 = *c.2.get_or_insert_with(std::time::Instant::now);
        if c.0 % (1 << 16) == 0 {
            let mut v: Vec<_> = c.1.iter().collect();
            v.sort_by(|a, b| b.1.cmp(a.1));
            let top: Vec<String> = v.iter().take(6).map(|(k, n)| format!("{k:#x}:{n}")).collect();
            eprintln!("vnjit pid {} thread {:?}: {} entries in {:?}: {}", crate::proc::current(), std::thread::current().id(), c.0, t0.elapsed(), top.join(" "));
        }
    });
}
