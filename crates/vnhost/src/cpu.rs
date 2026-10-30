//! The x86 run loop driven by Wine's wasm signal layer (dlls/ntdll/unix/signal_wasm.c).
//!
//! Guest and host share the linear memory, so a guest address is used as a pointer as is.
//! The first 64 KiB stay unmapped for the guest so NULL dereferences raise access violations.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use vnx86::{Cpu, Exit, Fault, Memory};

const NULL_PAGES: u32 = 0x10000;

/// Exit codes shared with signal_wasm.c (`enum vnhost_exit`).
const EXIT_TRAP: i32 = 1;
const EXIT_FAULT: i32 = 2;
const EXIT_DIVIDE: i32 = 3;
const EXIT_INVALID: i32 = 4;
const EXIT_INT: i32 = 5;
const EXIT_HALT: i32 = 6;
const EXIT_PENDING: i32 = 7;

/// The linear memory as the guest sees it.
pub(crate) struct Linear;

#[inline(always)]
fn mem_end() -> u64 {
    #[cfg(target_family = "wasm")]
    {
        core::arch::wasm32::memory_size(0) as u64 * 65536
    }
    #[cfg(not(target_family = "wasm"))]
    {
        0
    }
}

#[inline(always)]
fn check(addr: u32, len: u32, write: bool) -> Result<*mut u8, Fault> {
    if addr < NULL_PAGES || addr as u64 + len as u64 > mem_end() {
        return Err(Fault { addr, write });
    }
    Ok(addr as usize as *mut u8)
}

impl Memory for Linear {
    #[inline(always)]
    fn read8(&mut self, addr: u32) -> Result<u8, Fault> {
        Ok(unsafe { *check(addr, 1, false)? })
    }
    #[inline(always)]
    fn write8(&mut self, addr: u32, v: u8) -> Result<(), Fault> {
        unsafe { *check(addr, 1, true)? = v };
        Ok(())
    }
    #[inline(always)]
    fn read16(&mut self, addr: u32) -> Result<u16, Fault> {
        Ok(unsafe { (check(addr, 2, false)? as *const u16).read_unaligned() })
    }
    #[inline(always)]
    fn read32(&mut self, addr: u32) -> Result<u32, Fault> {
        Ok(unsafe { (check(addr, 4, false)? as *const u32).read_unaligned() })
    }
    #[inline(always)]
    fn read64(&mut self, addr: u32) -> Result<u64, Fault> {
        Ok(unsafe { (check(addr, 8, false)? as *const u64).read_unaligned() })
    }
    #[inline(always)]
    fn write16(&mut self, addr: u32, v: u16) -> Result<(), Fault> {
        unsafe { (check(addr, 2, true)? as *mut u16).write_unaligned(v) };
        Ok(())
    }
    #[inline(always)]
    fn write32(&mut self, addr: u32, v: u32) -> Result<(), Fault> {
        unsafe { (check(addr, 4, true)? as *mut u32).write_unaligned(v) };
        Ok(())
    }
    #[inline(always)]
    fn write64(&mut self, addr: u32, v: u64) -> Result<(), Fault> {
        unsafe { (check(addr, 8, true)? as *mut u64).write_unaligned(v) };
        Ok(())
    }
    fn host_range(&mut self, addr: u32, len: u32) -> Option<*mut u8> {
        check(addr, len, true).ok()
    }
    fn fetch(&mut self, addr: u32, buf: &mut [u8]) -> usize {
        let end = mem_end();
        if addr < NULL_PAGES || addr as u64 >= end {
            return 0;
        }
        let n = (buf.len() as u64).min(end - addr as u64) as usize;
        unsafe { std::ptr::copy_nonoverlapping(addr as usize as *const u8, buf.as_mut_ptr(), n) };
        n
    }
}

// ---------------------------------------------------------------------------------------------
// signals: wineserver and other threads mark them here; the run loop notices between blocks

struct ThreadSignals {
    pending: AtomicU32,
    pthread: libc::pthread_t,
}

static THREADS: Mutex<Option<HashMap<i32, Arc<ThreadSignals>>>> = Mutex::new(None);
static NEXT_TID: AtomicU32 = AtomicU32::new(1000);

thread_local! {
    static SELF: (i32, Arc<ThreadSignals>) = {
        let tid = NEXT_TID.fetch_add(1, Ordering::Relaxed) as i32;
        let s = Arc::new(ThreadSignals { pending: AtomicU32::new(0), pthread: unsafe { libc::pthread_self() } });
        THREADS.lock().unwrap().get_or_insert_with(HashMap::new).insert(tid, s.clone());
        (tid, s)
    };
}

/// Our tid for a pthread that has called into vnhost before.
pub fn tid_of(t: libc::pthread_t) -> Option<i32> {
    let g = THREADS.lock().unwrap();
    g.as_ref()?.iter().find(|(_, s)| s.pthread == t).map(|(&tid, _)| tid)
}

#[no_mangle]
pub extern "C" fn vnhost_gettid() -> i32 {
    SELF.with(|s| s.0)
}

/// Marks `sig` pending for thread `tid`; 0 on success, -1 (ESRCH) if there is no such thread.
#[no_mangle]
pub extern "C" fn vnhost_tkill(tid: i32, sig: i32) -> i32 {
    let t = THREADS.lock().unwrap().as_ref().and_then(|m| m.get(&tid).cloned());
    match t {
        Some(t) => {
            if sig > 0 && sig < 32 {
                t.pending.fetch_or(1 << sig, Ordering::SeqCst);
            }
            0
        }
        None => {
            crate::set_errno(libc::ESRCH);
            -1
        }
    }
}

/// Whether the calling thread's process was killed (see `proc::kill`).
pub fn self_killed() -> bool {
    SELF.with(|s| s.1.pending.load(Ordering::Relaxed) & (1 << libc::SIGKILL) != 0)
}

/// Drops a thread that has ended.
pub fn forget_thread(tid: i32) {
    if let Some(m) = THREADS.lock().unwrap().as_mut() {
        m.remove(&tid);
    }
}

/// Takes `sig` off the calling thread's pending set; returns whether it was pending.
#[no_mangle]
pub extern "C" fn vnhost_signal_pending(sig: i32) -> i32 {
    SELF.with(|s| (s.1.pending.fetch_and(!(1 << sig), Ordering::SeqCst) & (1 << sig) != 0) as i32)
}

// ---------------------------------------------------------------------------------------------
// the C interface of signal_wasm.c

#[no_mangle]
pub extern "C" fn vnhost_cpu_create(trap_lo: u32, trap_hi: u32) -> *mut Cpu {
    let mut cpu = Box::new(Cpu::default());
    cpu.trap_lo = trap_lo;
    cpu.trap_hi = trap_hi;
    cpu.fpu.cw = 0x27f;
    Box::into_raw(cpu)
}

#[no_mangle]
pub unsafe extern "C" fn vnhost_cpu_destroy(cpu: *mut Cpu) {
    drop(Box::from_raw(cpu));
}

/// regs: eax ecx edx ebx esp ebp esi edi eip eflags
#[no_mangle]
pub unsafe extern "C" fn vnhost_cpu_get(cpu: *const Cpu, regs: *mut u32) {
    let c = &*cpu;
    let r = std::slice::from_raw_parts_mut(regs, 10);
    r[..8].copy_from_slice(&c.regs);
    r[8] = c.eip;
    r[9] = c.flags();
}

#[no_mangle]
pub unsafe extern "C" fn vnhost_cpu_set(cpu: *mut Cpu, regs: *const u32) {
    let c = &mut *cpu;
    let r = std::slice::from_raw_parts(regs, 10);
    c.regs.copy_from_slice(&r[..8]);
    c.eip = r[8];
    c.eflags = r[9] | 0x2;
    c.lazy.op = 0;
}

#[no_mangle]
pub unsafe extern "C" fn vnhost_cpu_set_fs(cpu: *mut Cpu, base: u32) {
    (*cpu).fs_base = base;
}

/// FXSAVE image of the x87/SSE state into a host buffer.
#[no_mangle]
pub unsafe extern "C" fn vnhost_cpu_fxsave(cpu: *const Cpu, dst: *mut u8) {
    let mut buf = Buf(std::slice::from_raw_parts_mut(dst, 512));
    let _ = vnx86::fxsave(&*cpu, &mut buf, 0);
}

#[no_mangle]
pub unsafe extern "C" fn vnhost_cpu_fxrstor(cpu: *mut Cpu, src: *const u8) {
    let mut copy = [0u8; 512];
    copy.copy_from_slice(std::slice::from_raw_parts(src, 512));
    let mut buf = Buf(&mut copy);
    let _ = vnx86::fxrstor(&mut *cpu, &mut buf, 0);
}

#[no_mangle]
pub unsafe extern "C" fn vnhost_cpuid(leaf: u32, sub: u32, regs: *mut u32) {
    let (a, b, c, d) = vnx86::cpuid(leaf, sub);
    let r = std::slice::from_raw_parts_mut(regs, 4);
    r.copy_from_slice(&[a, b, c, d]);
}

#[cfg(target_os = "emscripten")]
extern "C" {
    fn _emscripten_process_dlopen_queue();
}

/// What an [`Exit`] means for signal_wasm.c; None to keep running.
fn exit_code(cpu: &Cpu, mem: &mut Linear, exit: Exit, info: &mut [u32]) -> Option<i32> {
    match exit {
        Exit::Continue => None,
        Exit::Trap(a) => {
            info[0] = a;
            Some(EXIT_TRAP)
        }
        Exit::Fault(f) => {
            info[0] = f.addr;
            info[1] = f.write as u32;
            Some(EXIT_FAULT)
        }
        Exit::DivideError => Some(EXIT_DIVIDE),
        Exit::InvalidOpcode => {
            info[0] = cpu.eip;
            if std::env::var_os("VNVM_TRACE_UD").is_some() {
                let mut b = [0u8; 16];
                let n = mem.fetch(cpu.eip, &mut b);
                eprintln!("vnhost: invalid opcode at {:#x}: {:02x?}", cpu.eip, &b[..n]);
            }
            Some(EXIT_INVALID)
        }
        Exit::Interrupt(n) => {
            info[0] = n as u32;
            Some(EXIT_INT)
        }
        Exit::Halt => Some(EXIT_HALT),
    }
}

/// Runs until the guest reaches a trap address, faults, or a signal is pending.
/// `info` gets the trap/fault address (and for faults, whether it was a write).
#[no_mangle]
pub unsafe extern "C" fn vnhost_cpu_run(cpu: *mut Cpu, info: *mut u32) -> i32 {
    let cpu = &mut *cpu;
    let info = std::slice::from_raw_parts_mut(info, 2);
    let signals = SELF.with(|s| s.1.clone());
    let mut mem = Linear;
    let mut rounds = 0u32;
    let jit = crate::jit::enabled();
    crate::jit::count_entry(cpu.eip);
    loop {
        let pending = signals.pending.load(Ordering::Relaxed);
        if pending != 0 {
            if pending & (1 << libc::SIGKILL) != 0 {
                signals.pending.fetch_and(!(1 << libc::SIGKILL), Ordering::SeqCst);
                crate::proc::check_alive();
                continue;
            }
            return EXIT_PENDING;
        }
        rounds = rounds.wrapping_add(1);
        if rounds % 64 == 0 {
            // another thread's dlopen (a new process) waits until every thread has loaded it
            #[cfg(target_os = "emscripten")]
            unsafe { _emscripten_process_dlopen_queue() };
        }
        if jit {
            match crate::jit::run(cpu, signals.pending.as_ptr() as u32) {
                crate::jit::Ran::Stopped => {
                    crate::jit::count_entry(cpu.eip | 0x8000_0000);
                    continue;
                }
                crate::jit::Ran::Exit(e) => match exit_code(cpu, &mut mem, e, info) {
                    Some(code) => return code,
                    None => continue,
                },
                crate::jit::Ran::Missed => {
                    crate::jit::count_miss(cpu.eip);
                    if crate::jit::maybe_compile(cpu, &mut mem) {
                        continue;
                    }
                }
            }
        }
        // the interpreter: up to the next branch with the JIT, a batch of steps without
        for _ in 0..4096 {
            let (e, branch) = vnx86::step_flow(cpu, &mut mem);
            if let Some(code) = exit_code(cpu, &mut mem, e, info) {
                return code;
            }
            if jit && branch {
                break;
            }
        }
    }
}

/// A host buffer seen as guest memory at address 0 (for FXSAVE images).
struct Buf<'a>(&'a mut [u8]);

impl Memory for Buf<'_> {
    fn read8(&mut self, addr: u32) -> Result<u8, Fault> {
        self.0.get(addr as usize).copied().ok_or(Fault { addr, write: false })
    }
    fn write8(&mut self, addr: u32, v: u8) -> Result<(), Fault> {
        match self.0.get_mut(addr as usize) {
            Some(b) => {
                *b = v;
                Ok(())
            }
            None => Err(Fault { addr, write: true }),
        }
    }
}
