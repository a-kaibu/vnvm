//! i386 user-mode CPU core for running Windows code under a wasm-built Wine
//! (see docs/hangover.md).
//!
//! The guest address space is flat 32-bit; segment registers other than FS/GS
//! (Windows' TEB) are ignored. The CPU never owns memory: everything goes
//! through [`Memory`], so the same core runs natively in tests (a Vec) and in
//! the browser (the wasm linear memory itself, guest address = wasm address).

#![no_std]
extern crate alloc;

mod interp;
pub mod flags;
pub mod lazy;
pub mod jit;
pub mod fpu;

pub use iced_x86;
pub use fpu::Fpu;

pub use interp::{cpuid, step, step_flow};

impl Cpu {
    /// EFLAGS with the lazy status flags worked out.
    #[inline]
    pub fn flags(&self) -> u32 {
        if self.lazy.op == 0 { self.eflags } else { (self.eflags & !flags::STATUS) | self.lazy.status() }
    }

    /// Makes `eflags` exact again.
    #[inline]
    pub fn materialize(&mut self) {
        if self.lazy.op != 0 {
            self.eflags = self.flags();
            self.lazy.op = 0;
        }
    }
}

/// A memory access the guest may not make (unmapped / protected page).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fault {
    pub addr: u32,
    pub write: bool,
}

/// Guest memory. Little-endian, unaligned accesses allowed.
pub trait Memory {
    fn read8(&mut self, addr: u32) -> Result<u8, Fault>;
    fn write8(&mut self, addr: u32, v: u8) -> Result<(), Fault>;

    #[inline]
    fn read16(&mut self, addr: u32) -> Result<u16, Fault> {
        Ok(self.read8(addr)? as u16 | (self.read8(addr.wrapping_add(1))? as u16) << 8)
    }
    #[inline]
    fn read32(&mut self, addr: u32) -> Result<u32, Fault> {
        Ok(self.read16(addr)? as u32 | (self.read16(addr.wrapping_add(2))? as u32) << 16)
    }
    #[inline]
    fn read64(&mut self, addr: u32) -> Result<u64, Fault> {
        Ok(self.read32(addr)? as u64 | (self.read32(addr.wrapping_add(4))? as u64) << 32)
    }
    #[inline]
    fn write16(&mut self, addr: u32, v: u16) -> Result<(), Fault> {
        self.write8(addr, v as u8)?;
        self.write8(addr.wrapping_add(1), (v >> 8) as u8)
    }
    #[inline]
    fn write32(&mut self, addr: u32, v: u32) -> Result<(), Fault> {
        self.write16(addr, v as u16)?;
        self.write16(addr.wrapping_add(2), (v >> 16) as u16)
    }
    #[inline]
    fn write64(&mut self, addr: u32, v: u64) -> Result<(), Fault> {
        self.write32(addr, v as u32)?;
        self.write32(addr.wrapping_add(4), (v >> 32) as u32)
    }
    /// Host pointer to [addr, addr + len) if all of it is plain accessible memory (bulk
    /// string operations); None to go byte by byte.
    fn host_range(&mut self, _addr: u32, _len: u32) -> Option<*mut u8> {
        None
    }

    /// Copy up to `buf.len()` instruction bytes at `addr`; returns how many
    /// were readable (decoding fails cleanly on a short fetch).
    fn fetch(&mut self, addr: u32, buf: &mut [u8]) -> usize {
        for (i, b) in buf.iter_mut().enumerate() {
            match self.read8(addr.wrapping_add(i as u32)) {
                Ok(v) => *b = v,
                Err(_) => return i,
            }
        }
        buf.len()
    }
}

/// General registers in encoding order.
pub const EAX: usize = 0;
pub const ECX: usize = 1;
pub const EDX: usize = 2;
pub const EBX: usize = 3;
pub const ESP: usize = 4;
pub const EBP: usize = 5;
pub const ESI: usize = 6;
pub const EDI: usize = 7;

/// Architectural state of one guest thread. `repr(C)`: the JIT addresses the fields directly.
#[derive(Clone, Debug)]
#[repr(C)]
pub struct Cpu {
    pub regs: [u32; 8],
    pub eip: u32,
    /// EFLAGS (bit 1 is kept set). Its status flags are stale while `lazy.op` is set: use
    /// [`Cpu::flags`] / [`Cpu::materialize`].
    pub eflags: u32,
    /// Status flags of the last JIT-executed operation, computed only when needed.
    pub lazy: lazy::Lazy,
    /// For the JIT's dispatcher (crates/vnhost): block hash table, its mask, the address of
    /// the thread's pending-signal word, and how many more blocks to run before returning.
    pub jit_hash: u32,
    pub jit_mask: u32,
    pub jit_pending: u32,
    pub jit_budget: i32,
    /// JIT: ring of recent jumps between regions (see jit::emit_lookup)
    pub jit_ring: u32,
    pub fs_base: u32,
    pub gs_base: u32,
    /// [start, end): jumping here stops execution with [`Exit::Trap`]
    /// (the syscall / unix-call dispatchers of the wasm-built Wine).
    pub trap_lo: u32,
    pub trap_hi: u32,
    /// Value returned by RDTSC (the embedder advances it).
    pub tsc: u64,
    pub fpu: Fpu,
    /// MMX registers (kept apart from the x87 stack: code does not mix them).
    pub mm: [u64; 8],
    pub xmm: [u128; 8],
    pub mxcsr: u32,
}

impl Default for Cpu {
    fn default() -> Self {
        Cpu {
            regs: [0; 8], eip: 0, eflags: 0x202, lazy: lazy::Lazy::default(), jit_hash: 0, jit_mask: 0,
            jit_pending: 0, jit_budget: 0, jit_ring: 0, fs_base: 0, gs_base: 0, trap_lo: 0, trap_hi: 0, tsc: 0,
            fpu: Fpu::default(), mm: [0; 8], xmm: [0; 8], mxcsr: 0x1f80,
        }
    }
}

/// Why [`step`] stopped. EIP points at the instruction that caused it
/// (for `Trap`, at the trap address itself).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// The instruction completed.
    Continue,
    /// Control reached the trap range.
    Trap(u32),
    /// Page fault.
    Fault(Fault),
    /// #DE (DIV / IDIV by zero or overflow).
    DivideError,
    /// #UD, or an instruction this core does not implement.
    InvalidOpcode,
    /// INT n (INT3 = 3).
    Interrupt(u8),
    /// HLT (privileged in user mode; the embedder decides).
    Halt,
}

/// FXSAVE image (512 bytes) of the x87/SSE state at `a`.
pub fn fxsave<M: Memory + ?Sized>(cpu: &Cpu, mem: &mut M, a: u32) -> Result<(), Fault> {
    let f = &cpu.fpu;
    let tw = !f.empty; // abridged: 1 = valid
    mem.write16(a, f.cw)?;
    mem.write16(a + 2, f.status())?;
    mem.write8(a + 4, tw)?;
    mem.write8(a + 5, 0)?;
    for o in (6..24).step_by(2) {
        mem.write16(a + o, 0)?;
    }
    mem.write32(a + 24, cpu.mxcsr)?;
    mem.write32(a + 28, 0xffff)?;
    for i in 0..8u32 {
        let b = fpu::to_f80(cpu.fpu.st(i));
        for (k, x) in b.iter().enumerate() {
            mem.write8(a + 32 + 16 * i + k as u32, *x)?;
        }
        for k in 10..16 {
            mem.write8(a + 32 + 16 * i + k, 0)?;
        }
    }
    for i in 0..8u32 {
        let x = cpu.xmm[i as usize];
        mem.write64(a + 160 + 16 * i, x as u64)?;
        mem.write64(a + 168 + 16 * i, (x >> 64) as u64)?;
    }
    Ok(())
}

/// Loads the x87/SSE state from an FXSAVE image at `a`.
pub fn fxrstor<M: Memory + ?Sized>(cpu: &mut Cpu, mem: &mut M, a: u32) -> Result<(), Fault> {
    let cw = mem.read16(a)?;
    let sw = mem.read16(a + 2)?;
    let tw = mem.read8(a + 4)?;
    let mxcsr = mem.read32(a + 24)?;
    let mut regs = [0.0; 8];
    for (i, r) in regs.iter_mut().enumerate() {
        let mut b = [0u8; 10];
        for (k, x) in b.iter_mut().enumerate() {
            *x = mem.read8(a + 32 + 16 * i as u32 + k as u32)?;
        }
        *r = fpu::from_f80(&b);
    }
    let mut xmm = [0u128; 8];
    for (i, x) in xmm.iter_mut().enumerate() {
        let lo = mem.read64(a + 160 + 16 * i as u32)?;
        let hi = mem.read64(a + 168 + 16 * i as u32)?;
        *x = lo as u128 | (hi as u128) << 64;
    }
    let f = &mut cpu.fpu;
    f.cw = cw;
    f.set_status(sw);
    f.empty = !tw;
    f.exact = 0;
    for (i, r) in regs.iter().enumerate() {
        let p = ((f.top + i as u32) & 7) as usize;
        f.regs[p] = *r;
    }
    cpu.mxcsr = mxcsr;
    cpu.xmm = xmm;
    Ok(())
}
