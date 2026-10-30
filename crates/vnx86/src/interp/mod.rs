//! The reference interpreter: decode one instruction with iced-x86 and
//! execute it. Slow but simple; the JIT is checked against it.

use crate::flags::{self, *};
use crate::{Cpu, Exit, Fault, Memory, ESP, EAX, ECX, EDX, EBP, ESI, EDI};
use iced_x86::{ConditionCode, Decoder, DecoderOptions, Instruction, Mnemonic, OpKind, Register};

mod x87;
mod sse;

macro_rules! tryf {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(f) => return Exit::Fault(f),
        }
    };
}

/// Execute one instruction at `cpu.eip`.
pub fn step<M: Memory>(cpu: &mut Cpu, mem: &mut M) -> Exit {
    step_flow(cpu, mem).0
}

/// [`step`], also telling whether the instruction could transfer control (a branch, call,
/// return, interrupt...).
pub fn step_flow<M: Memory>(cpu: &mut Cpu, mem: &mut M) -> (Exit, bool) {
    if cpu.eip >= cpu.trap_lo && cpu.eip < cpu.trap_hi {
        return (Exit::Trap(cpu.eip), true);
    }
    cpu.materialize();
    let mut buf = [0u8; 15];
    let n = mem.fetch(cpu.eip, &mut buf);
    let mut dec = Decoder::with_ip(32, &buf[..n], cpu.eip as u64, DecoderOptions::NONE);
    let ins = dec.decode();
    if ins.is_invalid() {
        if n < 15 {
            return (Exit::Fault(Fault { addr: cpu.eip.wrapping_add(n as u32), write: false }), true);
        }
        return (Exit::InvalidOpcode, true);
    }
    let branch = ins.flow_control() != iced_x86::FlowControl::Next;
    let mut x = X { cpu, mem, ins: &ins };
    (x.exec(), branch)
}

/// reg_info's index for segment registers: flat 32-bit user mode, they read as Wine's
/// selectors and writes are ignored
const SEG: usize = usize::MAX - 1;

fn selector(r: Register) -> u32 {
    match r {
        Register::CS => 0x23,
        Register::FS => 0x53,
        Register::GS => 0,
        _ => 0x2b,
    }
}

/// Register -> (index, bits, high byte)
#[inline]
fn reg_info(r: Register) -> (usize, u32, bool) {
    if r.is_gpr32() {
        (r.number(), 32, false)
    } else if r.is_gpr16() {
        (r.number(), 16, false)
    } else if r.is_gpr8() {
        let n = r.number();
        if n < 4 { (n, 8, false) } else { (n - 4, 8, true) }
    } else if r.is_segment_register() {
        (SEG, 16, false)
    } else {
        (usize::MAX, 0, false)
    }
}

pub(crate) struct X<'a, M: Memory> {
    pub(crate) cpu: &'a mut Cpu,
    pub(crate) mem: &'a mut M,
    pub(crate) ins: &'a Instruction,
}

impl<'a, M: Memory> X<'a, M> {
    #[inline]
    fn get_reg(&self, r: Register) -> u32 {
        let (i, bits, high) = reg_info(r);
        if i == SEG {
            return selector(r);
        }
        let v = self.cpu.regs[i];
        match (bits, high) {
            (32, _) => v,
            (16, _) => v & 0xffff,
            (8, false) => v & 0xff,
            _ => (v >> 8) & 0xff,
        }
    }

    #[inline]
    fn set_reg(&mut self, r: Register, val: u32) {
        let (i, bits, high) = reg_info(r);
        if i == SEG {
            return;
        }
        let v = &mut self.cpu.regs[i];
        match (bits, high) {
            (32, _) => *v = val,
            (16, _) => *v = (*v & 0xffff_0000) | (val & 0xffff),
            (8, false) => *v = (*v & 0xffff_ff00) | (val & 0xff),
            _ => *v = (*v & 0xffff_00ff) | ((val & 0xff) << 8),
        }
    }

    /// Effective address (offset only, no segment base): LEA.
    fn offset(&self) -> u32 {
        let ins = self.ins;
        let mut a = ins.memory_displacement32();
        let base = ins.memory_base();
        let index = ins.memory_index();
        let mut a16 = false;
        if base != Register::None {
            a16 |= base.is_gpr16();
            a = a.wrapping_add(self.get_reg(base));
        }
        if index != Register::None {
            a16 |= index.is_gpr16();
            a = a.wrapping_add(self.get_reg(index).wrapping_mul(ins.memory_index_scale()));
        }
        if a16 { a & 0xffff } else { a }
    }

    #[inline]
    fn seg_base(&self) -> u32 {
        match self.ins.memory_segment() {
            Register::FS => self.cpu.fs_base,
            Register::GS => self.cpu.gs_base,
            _ => 0,
        }
    }

    /// Linear address of the memory operand.
    #[inline]
    fn addr(&self) -> u32 {
        self.offset().wrapping_add(self.seg_base())
    }

    fn op_bits(&self, i: u32) -> u32 {
        match self.ins.op_kind(i) {
            OpKind::Register => reg_info(self.ins.op_register(i)).1,
            OpKind::Memory => self.ins.memory_size().size() as u32 * 8,
            OpKind::Immediate8 => 8,
            OpKind::Immediate16 | OpKind::Immediate8to16 => 16,
            _ => 32,
        }
    }

    fn read_mem(&mut self, addr: u32, bits: u32) -> Result<u32, Fault> {
        match bits {
            8 => self.mem.read8(addr).map(|v| v as u32),
            16 => self.mem.read16(addr).map(|v| v as u32),
            _ => self.mem.read32(addr),
        }
    }

    fn write_mem(&mut self, addr: u32, bits: u32, v: u32) -> Result<(), Fault> {
        match bits {
            8 => self.mem.write8(addr, v as u8),
            16 => self.mem.write16(addr, v as u16),
            _ => self.mem.write32(addr, v),
        }
    }

    fn read_op(&mut self, i: u32) -> Result<u32, Fault> {
        match self.ins.op_kind(i) {
            OpKind::Register => Ok(self.get_reg(self.ins.op_register(i))),
            OpKind::Memory => {
                let bits = self.op_bits(i);
                let a = self.addr();
                self.read_mem(a, bits)
            }
            _ => Ok(self.ins.immediate(i) as u32),
        }
    }

    /// Write operand `i` (register or memory) with its own width.
    fn write_op(&mut self, i: u32, v: u32) -> Result<(), Fault> {
        match self.ins.op_kind(i) {
            OpKind::Register => {
                self.set_reg(self.ins.op_register(i), v);
                Ok(())
            }
            _ => {
                let bits = self.op_bits(i);
                let a = self.addr();
                self.write_mem(a, bits, v)
            }
        }
    }

    #[inline]
    fn set_status(&mut self, f: u32) {
        self.cpu.eflags = (self.cpu.eflags & !STATUS) | (f & STATUS);
    }

    /// Replace only the flags in `which`.
    #[inline]
    fn set_flags(&mut self, which: u32, f: u32) {
        self.cpu.eflags = (self.cpu.eflags & !which) | (f & which);
    }

    #[inline]
    fn cf(&self) -> u32 {
        self.cpu.eflags & CF
    }

    fn push(&mut self, v: u32, bits: u32) -> Result<(), Fault> {
        let sp = self.cpu.regs[ESP].wrapping_sub(bits / 8);
        self.write_mem(sp, bits, v)?;
        self.cpu.regs[ESP] = sp;
        Ok(())
    }

    fn pop(&mut self, bits: u32) -> Result<u32, Fault> {
        let sp = self.cpu.regs[ESP];
        let v = self.read_mem(sp, bits)?;
        self.cpu.regs[ESP] = sp.wrapping_add(bits / 8);
        Ok(v)
    }

    #[inline]
    fn next(&self) -> u32 {
        self.ins.next_ip32()
    }

    fn done(&mut self) -> Exit {
        self.cpu.eip = self.next();
        Exit::Continue
    }

    fn jump(&mut self, target: u32) -> Exit {
        self.cpu.eip = target;
        Exit::Continue
    }

    fn cc(&self) -> bool {
        let cc = match self.ins.condition_code() {
            ConditionCode::o => 0,
            ConditionCode::no => 1,
            ConditionCode::b => 2,
            ConditionCode::ae => 3,
            ConditionCode::e => 4,
            ConditionCode::ne => 5,
            ConditionCode::be => 6,
            ConditionCode::a => 7,
            ConditionCode::s => 8,
            ConditionCode::ns => 9,
            ConditionCode::p => 10,
            ConditionCode::np => 11,
            ConditionCode::l => 12,
            ConditionCode::ge => 13,
            ConditionCode::le => 14,
            ConditionCode::g => 15,
            ConditionCode::None => return true,
        };
        flags::cond(self.cpu.eflags, cc)
    }

    fn exec(&mut self) -> Exit {
        use Mnemonic::*;
        let ins = self.ins;
        match ins.mnemonic() {
            Nop | Pause | Wait | Endbr32 | Prefetchnta | Prefetcht0 | Prefetcht1 | Prefetcht2 | Prefetchw | Lfence
            | Mfence | Sfence => self.done(),

            Mov => {
                let v = tryf!(self.read_op(1));
                tryf!(self.write_op(0, v));
                self.done()
            }
            Movzx => {
                let v = tryf!(self.read_op(1));
                tryf!(self.write_op(0, v));
                self.done()
            }
            Movsx => {
                let bits = self.op_bits(1);
                let v = tryf!(self.read_op(1));
                let v = if bits == 8 { v as u8 as i8 as i32 as u32 } else { v as u16 as i16 as i32 as u32 };
                tryf!(self.write_op(0, v));
                self.done()
            }
            Lea => {
                let v = self.offset();
                tryf!(self.write_op(0, v));
                self.done()
            }
            Xchg => {
                let a = tryf!(self.read_op(0));
                let b = tryf!(self.read_op(1));
                tryf!(self.write_op(0, b));
                tryf!(self.write_op(1, a));
                self.done()
            }
            Bswap => {
                let v = tryf!(self.read_op(0));
                tryf!(self.write_op(0, v.swap_bytes()));
                self.done()
            }

            Add | Adc | Sub | Sbb | Cmp | And | Or | Xor | Test => {
                let bits = self.op_bits(0);
                let a = tryf!(self.read_op(0));
                let b = tryf!(self.read_op(1));
                let (r, f) = match ins.mnemonic() {
                    Add => flags::add(a, b, 0, bits),
                    Adc => flags::add(a, b, self.cf(), bits),
                    Sub | Cmp => flags::sub(a, b, 0, bits),
                    Sbb => flags::sub(a, b, self.cf(), bits),
                    And | Test => (a & b, flags::logic(a & b, bits)),
                    Or => (a | b, flags::logic(a | b, bits)),
                    _ => (a ^ b, flags::logic(a ^ b, bits)),
                };
                if !matches!(ins.mnemonic(), Cmp | Test) {
                    tryf!(self.write_op(0, r));
                }
                self.set_status(f);
                self.done()
            }
            Inc | Dec => {
                let bits = self.op_bits(0);
                let a = tryf!(self.read_op(0));
                let (r, f) = if ins.mnemonic() == Inc { flags::add(a, 1, 0, bits) } else { flags::sub(a, 1, 0, bits) };
                tryf!(self.write_op(0, r));
                self.set_flags(STATUS & !CF, f);
                self.done()
            }
            Neg => {
                let bits = self.op_bits(0);
                let a = tryf!(self.read_op(0));
                let (r, f) = flags::sub(0, a, 0, bits);
                tryf!(self.write_op(0, r));
                self.set_status(f);
                self.done()
            }
            Not => {
                let a = tryf!(self.read_op(0));
                tryf!(self.write_op(0, !a));
                self.done()
            }

            Shl | Sal | Shr | Sar | Rol | Ror | Rcl | Rcr => self.shift(),
            Shld | Shrd => self.shift_double(),

            Mul | Imul if ins.op_count() == 1 => self.mul1(),
            Imul => {
                let bits = self.op_bits(0);
                let (a, b) = if ins.op_count() == 3 {
                    (tryf!(self.read_op(1)), tryf!(self.read_op(2)))
                } else {
                    (tryf!(self.read_op(0)), tryf!(self.read_op(1)))
                };
                let sx = |v: u32| -> i64 {
                    match bits {
                        16 => v as u16 as i16 as i64,
                        _ => v as i32 as i64,
                    }
                };
                let full = sx(a) * sx(b);
                let r = full as u32 & flags::mask(bits);
                let ovf = sx(r) != full;
                tryf!(self.write_op(0, r));
                let f = flags::szp(r, bits) | if ovf { CF | OF } else { 0 };
                self.set_status(f);
                self.done()
            }
            Div | Idiv => self.div(),

            Cbw => {
                let v = self.cpu.regs[EAX] as u8 as i8 as i16 as u16 as u32;
                self.cpu.regs[EAX] = (self.cpu.regs[EAX] & 0xffff_0000) | v;
                self.done()
            }
            Cwde => {
                self.cpu.regs[EAX] = self.cpu.regs[EAX] as u16 as i16 as i32 as u32;
                self.done()
            }
            Cwd => {
                let hi = if self.cpu.regs[EAX] & 0x8000 != 0 { 0xffff } else { 0 };
                self.cpu.regs[EDX] = (self.cpu.regs[EDX] & 0xffff_0000) | hi;
                self.done()
            }
            Cdq => {
                self.cpu.regs[EDX] = ((self.cpu.regs[EAX] as i32) >> 31) as u32;
                self.done()
            }

            Push => {
                let bits = if ins.op_kind(0) == OpKind::Register || ins.op_kind(0) == OpKind::Memory {
                    self.op_bits(0)
                } else {
                    ins.stack_pointer_increment().unsigned_abs() * 8
                };
                let v = tryf!(self.read_op(0));
                tryf!(self.push(v, bits));
                self.done()
            }
            Pop => {
                let bits = self.op_bits(0);
                let sp = self.cpu.regs[ESP];
                let v = tryf!(self.read_mem(sp, bits));
                self.cpu.regs[ESP] = sp.wrapping_add(bits / 8);
                // POP [ESP+x] uses ESP after the increment
                if let Err(f) = self.write_op(0, v) {
                    self.cpu.regs[ESP] = sp;
                    return Exit::Fault(f);
                }
                self.done()
            }
            Pushad => {
                let sp = self.cpu.regs[ESP];
                let r = self.cpu.regs;
                for (k, &v) in [r[0], r[1], r[2], r[3], sp, r[5], r[6], r[7]].iter().enumerate() {
                    tryf!(self.mem.write32(sp.wrapping_sub(4 * (k as u32 + 1)), v));
                }
                self.cpu.regs[ESP] = sp.wrapping_sub(32);
                self.done()
            }
            Popad => {
                let sp = self.cpu.regs[ESP];
                let mut v = [0u32; 8];
                for (k, slot) in v.iter_mut().enumerate() {
                    *slot = tryf!(self.mem.read32(sp.wrapping_add(4 * k as u32)));
                }
                // memory order: EDI ESI EBP (ESP) EBX EDX ECX EAX
                self.cpu.regs[EDI] = v[0];
                self.cpu.regs[ESI] = v[1];
                self.cpu.regs[EBP] = v[2];
                self.cpu.regs[3] = v[4];
                self.cpu.regs[EDX] = v[5];
                self.cpu.regs[ECX] = v[6];
                self.cpu.regs[EAX] = v[7];
                self.cpu.regs[ESP] = sp.wrapping_add(32);
                self.done()
            }
            Pushfd | Pushf => {
                let bits = if ins.mnemonic() == Pushf { 16 } else { 32 };
                let v = self.cpu.eflags & !(0x3 << 16); // VM and RF read as 0
                tryf!(self.push(v, bits));
                self.done()
            }
            Popfd | Popf => {
                let bits = if ins.mnemonic() == Popf { 16 } else { 32 };
                let v = tryf!(self.pop(bits));
                let which = USER_WRITABLE & flags::mask(bits);
                self.cpu.eflags = (self.cpu.eflags & !which) | (v & which) | 2;
                self.done()
            }
            Lahf => {
                let v = (self.cpu.eflags & (SF | ZF | AF | PF | CF)) | 2;
                self.cpu.regs[EAX] = (self.cpu.regs[EAX] & 0xffff_00ff) | (v << 8);
                self.done()
            }
            Sahf => {
                let v = (self.cpu.regs[EAX] >> 8) & 0xff;
                self.set_flags(SF | ZF | AF | PF | CF, v);
                self.done()
            }
            Clc => { self.cpu.eflags &= !CF; self.done() }
            Stc => { self.cpu.eflags |= CF; self.done() }
            Cmc => { self.cpu.eflags ^= CF; self.done() }
            Cld => { self.cpu.eflags &= !DF; self.done() }
            Std => { self.cpu.eflags |= DF; self.done() }

            Enter => {
                let size = ins.immediate16() as u32;
                let level = ins.immediate8_2nd() as u32 & 31;
                let ebp = self.cpu.regs[EBP];
                let mut sp = self.cpu.regs[ESP].wrapping_sub(4);
                tryf!(self.mem.write32(sp, ebp));
                let frame = sp;
                for l in 1..level {
                    let v = tryf!(self.mem.read32(ebp.wrapping_sub(4 * l)));
                    sp = sp.wrapping_sub(4);
                    tryf!(self.mem.write32(sp, v));
                }
                if level > 0 {
                    sp = sp.wrapping_sub(4);
                    tryf!(self.mem.write32(sp, frame));
                }
                self.cpu.regs[EBP] = frame;
                self.cpu.regs[ESP] = sp.wrapping_sub(size);
                self.done()
            }
            Leave => {
                let bp = self.cpu.regs[EBP];
                let v = tryf!(self.mem.read32(bp));
                self.cpu.regs[ESP] = bp.wrapping_add(4);
                self.cpu.regs[EBP] = v;
                self.done()
            }

            Jmp => match ins.op_kind(0) {
                OpKind::NearBranch32 | OpKind::NearBranch16 => self.jump(ins.near_branch32()),
                _ => {
                    let t = tryf!(self.read_op(0));
                    self.jump(t)
                }
            },
            Call => {
                let t = match ins.op_kind(0) {
                    OpKind::NearBranch32 | OpKind::NearBranch16 => ins.near_branch32(),
                    _ => tryf!(self.read_op(0)),
                };
                let ret = self.next();
                tryf!(self.push(ret, 32));
                self.jump(t)
            }
            Ret => {
                let sp = self.cpu.regs[ESP];
                let t = tryf!(self.mem.read32(sp));
                let extra = if ins.op_count() == 1 { ins.immediate16() as u32 } else { 0 };
                self.cpu.regs[ESP] = sp.wrapping_add(4 + extra);
                self.jump(t)
            }
            Ja | Jae | Jb | Jbe | Je | Jg | Jge | Jl | Jle | Jne | Jno | Jnp | Jns | Jo | Jp | Js => {
                if self.cc() { self.jump(ins.near_branch32()) } else { self.done() }
            }
            Jecxz | Jcxz => {
                let c = if ins.mnemonic() == Jcxz { self.cpu.regs[ECX] & 0xffff } else { self.cpu.regs[ECX] };
                if c == 0 { self.jump(ins.near_branch32()) } else { self.done() }
            }
            Loop | Loope | Loopne => {
                let c = self.cpu.regs[ECX].wrapping_sub(1);
                self.cpu.regs[ECX] = c;
                let zf = self.cpu.eflags & ZF != 0;
                let take = c != 0
                    && match ins.mnemonic() {
                        Loope => zf,
                        Loopne => !zf,
                        _ => true,
                    };
                if take { self.jump(ins.near_branch32()) } else { self.done() }
            }
            Seta | Setae | Setb | Setbe | Sete | Setg | Setge | Setl | Setle | Setne | Setno | Setnp | Setns
            | Seto | Setp | Sets => {
                let v = self.cc() as u32;
                tryf!(self.write_op(0, v));
                self.done()
            }
            Cmova | Cmovae | Cmovb | Cmovbe | Cmove | Cmovg | Cmovge | Cmovl | Cmovle | Cmovne | Cmovno
            | Cmovnp | Cmovns | Cmovo | Cmovp | Cmovs => {
                // the source is read (and may fault) even if the condition is false
                let v = tryf!(self.read_op(1));
                if self.cc() {
                    tryf!(self.write_op(0, v));
                }
                self.done()
            }

            Bt | Bts | Btr | Btc => self.bit_test(),
            Bsf | Bsr => {
                let bits = self.op_bits(0);
                let v = tryf!(self.read_op(1));
                if v == 0 {
                    self.cpu.eflags |= ZF;
                } else {
                    let r = if ins.mnemonic() == Bsf { v.trailing_zeros() } else { 31 - v.leading_zeros() };
                    tryf!(self.write_op(0, r));
                    self.cpu.eflags &= !ZF;
                }
                let _ = bits;
                self.done()
            }
            Tzcnt | Lzcnt | Popcnt => {
                let bits = self.op_bits(0);
                let v = tryf!(self.read_op(1)) & flags::mask(bits);
                let r = match ins.mnemonic() {
                    Popcnt => v.count_ones(),
                    Tzcnt => if v == 0 { bits } else { v.trailing_zeros() },
                    _ => v.leading_zeros() - (32 - bits),
                };
                tryf!(self.write_op(0, r));
                let f = match ins.mnemonic() {
                    Popcnt => if v == 0 { ZF } else { 0 },
                    _ => (if r == 0 { ZF } else { 0 }) | (if v == 0 || (ins.mnemonic() == Lzcnt && r == bits) { CF } else { 0 }),
                };
                self.set_status(f);
                self.done()
            }

            Cmpxchg => {
                let bits = self.op_bits(0);
                let d = tryf!(self.read_op(0));
                let acc = self.cpu.regs[EAX] & flags::mask(bits);
                let (_, f) = flags::sub(acc, d, 0, bits);
                if acc == d {
                    let s = tryf!(self.read_op(1));
                    tryf!(self.write_op(0, s));
                } else {
                    // a memory destination is written back unchanged
                    tryf!(self.write_op(0, d));
                    let r = match bits { 8 => Register::AL, 16 => Register::AX, _ => Register::EAX };
                    self.set_reg(r, d);
                }
                self.set_status(f);
                self.done()
            }
            Cmpxchg8b => {
                let a = self.addr();
                let d = tryf!(self.mem.read64(a));
                let cmp = (self.cpu.regs[EDX] as u64) << 32 | self.cpu.regs[EAX] as u64;
                if d == cmp {
                    let n = (self.cpu.regs[ECX] as u64) << 32 | self.cpu.regs[3] as u64;
                    tryf!(self.mem.write64(a, n));
                    self.cpu.eflags |= ZF;
                } else {
                    tryf!(self.mem.write64(a, d));
                    self.cpu.regs[EAX] = d as u32;
                    self.cpu.regs[EDX] = (d >> 32) as u32;
                    self.cpu.eflags &= !ZF;
                }
                self.done()
            }
            Xadd => {
                let bits = self.op_bits(0);
                let d = tryf!(self.read_op(0));
                let s = tryf!(self.read_op(1));
                let (r, f) = flags::add(d, s, 0, bits);
                // DEST is written last (XADD r, r with the same register)
                if ins.op_kind(0) == OpKind::Memory {
                    tryf!(self.write_op(0, r));
                    tryf!(self.write_op(1, d));
                } else {
                    tryf!(self.write_op(1, d));
                    tryf!(self.write_op(0, r));
                }
                self.set_status(f);
                self.done()
            }

            Movsb | Movsw | Movsd | Stosb | Stosw | Stosd | Lodsb | Lodsw | Lodsd | Cmpsb | Cmpsw | Cmpsd
            | Scasb | Scasw | Scasd
                if ins.op_count() == 0
                    || matches!(ins.op_kind(0), OpKind::MemoryESEDI | OpKind::MemorySegESI)
                    || (ins.op_kind(0) == OpKind::Register && !ins.op_register(0).is_xmm()) =>
            {
                self.string_op()
            }
            Xlatb => {
                let a = self.cpu.regs[3].wrapping_add(self.cpu.regs[EAX] & 0xff).wrapping_add(self.seg_base());
                let v = tryf!(self.mem.read8(a));
                self.set_reg(Register::AL, v as u32);
                self.done()
            }

            Cpuid => {
                let (a, b, c, d) = cpuid(self.cpu.regs[EAX], self.cpu.regs[ECX]);
                self.cpu.regs[EAX] = a;
                self.cpu.regs[3] = b;
                self.cpu.regs[ECX] = c;
                self.cpu.regs[EDX] = d;
                self.done()
            }
            Rdtsc => {
                self.cpu.regs[EAX] = self.cpu.tsc as u32;
                self.cpu.regs[EDX] = (self.cpu.tsc >> 32) as u32;
                self.done()
            }
            Int3 => {
                self.cpu.eip = self.next();
                Exit::Interrupt(3)
            }
            Int => {
                self.cpu.eip = self.next();
                Exit::Interrupt(ins.immediate8())
            }
            Hlt => Exit::Halt,
            _ => {
                use iced_x86::CpuidFeature as F;
                let feats = ins.cpuid_features();
                if feats.iter().any(|f| matches!(f, F::FPU | F::FPU287 | F::FPU387 | F::FPU287XL_ONLY | F::FPU387SL_ONLY)) {
                    self.x87()
                } else {
                    self.sse()
                }
            }
        }
    }

    fn shift(&mut self) -> Exit {
        use Mnemonic::*;
        let ins = self.ins;
        let bits = self.op_bits(0);
        let m = flags::mask(bits);
        let a = tryf!(self.read_op(0)) & m;
        let count = if ins.op_count() > 1 { tryf!(self.read_op(1)) & 0x1f } else { 1 };
        let sb = flags::sign(bits);
        let eff = match ins.mnemonic() {
            Rcl | Rcr => count % (bits + 1),
            Rol | Ror => count % bits,
            _ => count,
        };
        if count == 0 {
            return self.done();
        }
        let cf_in = self.cf();
        let (r, f, which) = match ins.mnemonic() {
            Shl | Sal => {
                let wide = (a as u64) << count;
                let r = wide as u32 & m;
                let cf = if count <= bits { ((wide >> bits) & 1) as u32 } else { 0 };
                let of = ((r & sb != 0) as u32) ^ cf;
                (r, flags::szp(r, bits) | cf * CF | of * OF, STATUS)
            }
            Shr => {
                let r = if count >= 32 { 0 } else { a >> count };
                let cf = if count <= bits { (a >> (count - 1)) & 1 } else { 0 };
                let of = (a & sb != 0) as u32;
                (r, flags::szp(r, bits) | cf * CF | of * OF, STATUS)
            }
            Sar => {
                let sx = ((a << (32 - bits)) as i32) >> (32 - bits);
                let c = count.min(31);
                let r = (sx >> c) as u32 & m;
                let cf = ((sx >> (c - 1).min(31)) & 1) as u32;
                let cf = if count > 31 { (sx < 0) as u32 } else { cf };
                (r, flags::szp(r, bits) | cf * CF, STATUS)
            }
            Rol => {
                let r = if eff == 0 { a } else { ((a << eff) | (a >> (bits - eff))) & m };
                let cf = r & 1;
                let of = ((r & sb != 0) as u32) ^ cf;
                (r, cf * CF | of * OF, CF | OF)
            }
            Ror => {
                let r = if eff == 0 { a } else { ((a >> eff) | (a << (bits - eff))) & m };
                let cf = (r & sb != 0) as u32;
                let of = cf ^ ((r & (sb >> 1) != 0) as u32);
                (r, cf * CF | of * OF, CF | OF)
            }
            Rcl => {
                let mut v = a as u64 | (cf_in as u64) << bits;
                let w = bits + 1;
                if eff != 0 {
                    v = ((v << eff) | (v >> (w - eff))) & ((1u64 << w) - 1);
                }
                let r = v as u32 & m;
                let cf = ((v >> bits) & 1) as u32;
                let of = ((r & sb != 0) as u32) ^ cf;
                (r, cf * CF | of * OF, CF | OF)
            }
            _ => {
                // RCR
                let mut v = a as u64 | (cf_in as u64) << bits;
                let w = bits + 1;
                if eff != 0 {
                    v = ((v >> eff) | (v << (w - eff))) & ((1u64 << w) - 1);
                }
                let r = v as u32 & m;
                let cf = ((v >> bits) & 1) as u32;
                let of = ((r & sb != 0) as u32) ^ ((r & (sb >> 1) != 0) as u32);
                (r, cf * CF | of * OF, CF | OF)
            }
        };
        tryf!(self.write_op(0, r));
        self.set_flags(which, f);
        self.done()
    }

    fn shift_double(&mut self) -> Exit {
        let ins = self.ins;
        let bits = self.op_bits(0);
        let m = flags::mask(bits);
        let d = tryf!(self.read_op(0)) & m;
        let s = tryf!(self.read_op(1)) & m;
        let count = tryf!(self.read_op(2)) & 0x1f;
        if count == 0 {
            return self.done();
        }
        let sb = flags::sign(bits);
        let (r, cf) = if ins.mnemonic() == Mnemonic::Shld {
            let wide = ((d as u128) << bits | s as u128) << count;
            let r = (wide >> bits) as u32 & m;
            let cf = ((wide >> (2 * bits)) & 1) as u32;
            (r, cf)
        } else {
            let wide = ((s as u64) << bits | d as u64) >> (count - 1);
            let cf = (wide & 1) as u32;
            ((wide >> 1) as u32 & m, cf)
        };
        let of = ((r ^ d) & sb != 0) as u32;
        tryf!(self.write_op(0, r));
        self.set_status(flags::szp(r, bits) | cf * CF | of * OF);
        self.done()
    }

    fn mul1(&mut self) -> Exit {
        let ins = self.ins;
        let bits = self.op_bits(0);
        let s = tryf!(self.read_op(0));
        let signed = ins.mnemonic() == Mnemonic::Imul;
        let r = &mut self.cpu.regs;
        let ovf;
        let lo;
        match bits {
            8 => {
                let a = r[EAX] & 0xff;
                let p = if signed { (a as u8 as i8 as i32 * s as u8 as i8 as i32) as u32 } else { a * (s & 0xff) };
                r[EAX] = (r[EAX] & 0xffff_0000) | (p & 0xffff);
                lo = p & 0xff;
                ovf = if signed { p as u16 as i16 != p as u8 as i8 as i16 } else { p & 0xff00 != 0 };
            }
            16 => {
                let a = r[EAX] & 0xffff;
                let p = if signed { (a as u16 as i16 as i32 * s as u16 as i16 as i32) as u32 } else { a * (s & 0xffff) };
                r[EAX] = (r[EAX] & 0xffff_0000) | (p & 0xffff);
                r[EDX] = (r[EDX] & 0xffff_0000) | (p >> 16);
                lo = p & 0xffff;
                ovf = if signed { p as i32 != p as u16 as i16 as i32 } else { p >> 16 != 0 };
            }
            _ => {
                let a = r[EAX];
                let p = if signed { (a as i32 as i64 * s as i32 as i64) as u64 } else { a as u64 * s as u64 };
                r[EAX] = p as u32;
                r[EDX] = (p >> 32) as u32;
                lo = p as u32;
                ovf = if signed { p as i64 != p as u32 as i32 as i64 } else { p >> 32 != 0 };
            }
        }
        let f = flags::szp(lo, bits) | if ovf { CF | OF } else { 0 };
        self.set_status(f);
        self.done()
    }

    fn div(&mut self) -> Exit {
        let ins = self.ins;
        let bits = self.op_bits(0);
        let s = tryf!(self.read_op(0));
        let signed = ins.mnemonic() == Mnemonic::Idiv;
        let r = &mut self.cpu.regs;
        match bits {
            8 => {
                let a = r[EAX] & 0xffff;
                let (q, rem) = if signed {
                    let d = s as u8 as i8 as i32;
                    if d == 0 { return Exit::DivideError; }
                    let n = a as u16 as i16 as i32;
                    let q = n / d;
                    if q != q as i8 as i32 { return Exit::DivideError; }
                    (q as u32, (n % d) as u32)
                } else {
                    let d = s & 0xff;
                    if d == 0 { return Exit::DivideError; }
                    let q = a / d;
                    if q > 0xff { return Exit::DivideError; }
                    (q, a % d)
                };
                r[EAX] = (r[EAX] & 0xffff_0000) | (q & 0xff) | (rem & 0xff) << 8;
            }
            16 => {
                let a = (r[EDX] & 0xffff) << 16 | (r[EAX] & 0xffff);
                let (q, rem) = if signed {
                    let d = s as u16 as i16 as i32;
                    if d == 0 { return Exit::DivideError; }
                    let n = a as i32;
                    let (q, rm) = (n.wrapping_div(d), n.wrapping_rem(d));
                    if q != q as i16 as i32 || (n == i32::MIN && d == -1) { return Exit::DivideError; }
                    (q as u32, rm as u32)
                } else {
                    let d = s & 0xffff;
                    if d == 0 { return Exit::DivideError; }
                    let q = a / d;
                    if q > 0xffff { return Exit::DivideError; }
                    (q, a % d)
                };
                r[EAX] = (r[EAX] & 0xffff_0000) | (q & 0xffff);
                r[EDX] = (r[EDX] & 0xffff_0000) | (rem & 0xffff);
            }
            _ => {
                let a = (r[EDX] as u64) << 32 | r[EAX] as u64;
                let (q, rem) = if signed {
                    let d = s as i32 as i64;
                    if d == 0 { return Exit::DivideError; }
                    let n = a as i64;
                    if n == i64::MIN && d == -1 { return Exit::DivideError; }
                    let q = n / d;
                    if q != q as i32 as i64 { return Exit::DivideError; }
                    (q as u32, (n % d) as u32)
                } else {
                    let d = s as u64;
                    if d == 0 { return Exit::DivideError; }
                    let q = a / d;
                    if q > 0xffff_ffff { return Exit::DivideError; }
                    (q as u32, (a % d) as u32)
                };
                r[EAX] = q;
                r[EDX] = rem;
            }
        }
        // flags are undefined after DIV; leave them alone
        self.done()
    }

    fn bit_test(&mut self) -> Exit {
        use Mnemonic::*;
        let ins = self.ins;
        let bits = self.op_bits(0);
        let off = tryf!(self.read_op(1));
        let reg_offset = ins.op_kind(1) == OpKind::Register;
        let (val, addr) = if ins.op_kind(0) == OpKind::Memory && reg_offset {
            // bit string: the offset is signed and may address outside the operand
            let o = if bits == 16 { off as u16 as i16 as i32 } else { off as i32 };
            let a = self.addr().wrapping_add(((o >> (if bits == 16 { 4 } else { 5 })) * (bits as i32 / 8)) as u32);
            (tryf!(self.read_mem(a, bits)), Some(a))
        } else {
            (tryf!(self.read_op(0)), None)
        };
        let bit = off & (bits - 1);
        let cf = (val >> bit) & 1;
        let nv = match ins.mnemonic() {
            Bts => val | 1 << bit,
            Btr => val & !(1 << bit),
            Btc => val ^ 1 << bit,
            _ => val,
        };
        if ins.mnemonic() != Bt {
            match addr {
                Some(a) => tryf!(self.write_mem(a, bits, nv)),
                None => tryf!(self.write_op(0, nv)),
            }
        }
        self.set_flags(CF, cf);
        self.done()
    }

    fn string_op(&mut self) -> Exit {
        use Mnemonic::*;
        let ins = self.ins;
        let mn = ins.mnemonic();
        let bits = match mn {
            Movsb | Stosb | Lodsb | Cmpsb | Scasb => 8,
            Movsw | Stosw | Lodsw | Cmpsw | Scasw => 16,
            _ => 32,
        };
        let step: u32 = if self.cpu.eflags & DF != 0 { (bits / 8u32).wrapping_neg() } else { bits / 8 };
        let rep = ins.has_rep_prefix() || ins.has_repe_prefix();
        let repne = ins.has_repne_prefix();
        let src_seg = self.seg_base(); // DS:ESI may take a segment override (FS)
        let compare = matches!(mn, Cmpsb | Cmpsw | Cmpsd | Scasb | Scasw | Scasd);
        loop {
            if (rep || repne) && self.cpu.regs[ECX] == 0 {
                break;
            }
            let si = self.cpu.regs[ESI];
            let di = self.cpu.regs[EDI];
            match mn {
                Movsb | Movsw | Movsd => {
                    let v = tryf!(self.read_mem(si.wrapping_add(src_seg), bits));
                    tryf!(self.write_mem(di, bits, v));
                    self.cpu.regs[ESI] = si.wrapping_add(step);
                    self.cpu.regs[EDI] = di.wrapping_add(step);
                }
                Stosb | Stosw | Stosd => {
                    let v = self.cpu.regs[EAX];
                    tryf!(self.write_mem(di, bits, v));
                    self.cpu.regs[EDI] = di.wrapping_add(step);
                }
                Lodsb | Lodsw | Lodsd => {
                    let v = tryf!(self.read_mem(si.wrapping_add(src_seg), bits));
                    let r = match bits { 8 => Register::AL, 16 => Register::AX, _ => Register::EAX };
                    self.set_reg(r, v);
                    self.cpu.regs[ESI] = si.wrapping_add(step);
                }
                Cmpsb | Cmpsw | Cmpsd => {
                    let a = tryf!(self.read_mem(si.wrapping_add(src_seg), bits));
                    let b = tryf!(self.read_mem(di, bits));
                    let (_, f) = flags::sub(a, b, 0, bits);
                    self.set_status(f);
                    self.cpu.regs[ESI] = si.wrapping_add(step);
                    self.cpu.regs[EDI] = di.wrapping_add(step);
                }
                _ => {
                    let b = tryf!(self.read_mem(di, bits));
                    let (_, f) = flags::sub(self.cpu.regs[EAX], b, 0, bits);
                    self.set_status(f);
                    self.cpu.regs[EDI] = di.wrapping_add(step);
                }
            }
            if !(rep || repne) {
                break;
            }
            self.cpu.regs[ECX] = self.cpu.regs[ECX].wrapping_sub(1);
            if compare {
                let zf = self.cpu.eflags & ZF != 0;
                if (rep && !zf) || (repne && zf) {
                    break;
                }
            }
        }
        self.done()
    }
}

/// A Pentium 4 class CPU with SSE2 (what VN-era code checks for).
/// CPUID as seen by guest code: a Pentium 4 class CPU with SSE2.
pub fn cpuid(leaf: u32, _sub: u32) -> (u32, u32, u32, u32) {
    match leaf {
        0 => (1, u32::from_le_bytes(*b"Genu"), u32::from_le_bytes(*b"ntel"), u32::from_le_bytes(*b"ineI")),
        // family 6 model 15; FPU TSC CX8 CMOV MMX FXSR SSE SSE2
        1 => (0x0000_06f1, 0, 0, (1 << 0) | (1 << 4) | (1 << 8) | (1 << 15) | (1 << 23) | (1 << 24) | (1 << 25) | (1 << 26)),
        _ => (0, 0, 0, 0),
    }
}
