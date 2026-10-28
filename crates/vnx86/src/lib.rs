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
pub mod fpu;

pub use fpu::Fpu;

pub use interp::step;

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

/// Architectural state of one guest thread.
#[derive(Clone, Debug)]
pub struct Cpu {
    pub regs: [u32; 8],
    pub eip: u32,
    /// EFLAGS, always materialized (bit 1 is kept set).
    pub eflags: u32,
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
            regs: [0; 8], eip: 0, eflags: 0x202, fs_base: 0, gs_base: 0, trap_lo: 0, trap_hi: 0, tsc: 0,
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
