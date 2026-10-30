//! x86 -> wasm translation of basic blocks.
//!
//! A block runs from an address up to and including the first instruction that can transfer
//! control. It becomes one wasm function `(cpu: i32) -> i32` that keeps the eight general
//! registers in locals, records the status flags lazily ([`crate::lazy`]) and returns:
//!
//! - 0: done, continue at `cpu.eip`
//! - 1: run the interpreter at `cpu.eip` (an access outside the guest range, or anything
//!   the embedder wants checked the slow way)
//! - otherwise what the `step` import returned for an instruction it ran on the block's behalf
//!
//! Instructions the translator does not handle are run by the `step` import (the
//! interpreter, one instruction) in the middle of the block. A branch back to the block's own
//! start becomes a wasm loop that polls the thread's pending-signal word and `cpu.jit_budget`.
//!
//! Imports of a block module: `env.memory` (the shared linear memory), `env.step(cpu) -> i32`,
//! `env.eflags(cpu) -> i32` (materializes and returns EFLAGS) and
//! `env.status(op, a, b, r, c) -> i32` (status flags of a lazy operation).

pub mod wasm;
pub mod dispatch;

use crate::lazy;
use crate::{Cpu, Memory};
use alloc::vec::Vec;
use core::mem::offset_of;
use iced_x86::{ConditionCode, Decoder, DecoderOptions, FlowControl, Instruction, Mnemonic, OpKind, Register, RflagsBits};
use wasm::{Code, Func, FuncType, Import, Module, I32, I64};

/// Guest addresses the JIT accesses directly: [GUEST_LO, GUEST_HI). Anything else goes
/// through the interpreter (which knows the real rules).
pub const GUEST_LO: u32 = 0x10000;
pub const GUEST_HI: u32 = 0x8000_0000;

/// Longest block, in instructions.
const MAX_INS: usize = 200;

// block return codes
pub const RET_CONTINUE: i32 = 0;
pub const RET_INTERP: i32 = 1;

// function indices in a block module
const F_STEP: u32 = 0;
const F_EFLAGS: u32 = 1;
const F_STATUS: u32 = 2;
const F_BLOCK0: u32 = 3;

// locals of a block function
const L_CPU: u32 = 0;
const L_REG: u32 = 1; // 8 registers
const L_NEXT: u32 = 9;
const L_ST: u32 = 10;
const L_OP: u32 = 11;
const L_FA: u32 = 12;
const L_FB: u32 = 13;
const L_FR: u32 = 14;
const L_FC: u32 = 15;
const L_T: u32 = 16; // 6 temporaries
const N_T: u32 = 6;
const L_W: u32 = L_T + N_T; // i64 temporary
const N_LOCALS_I32: u32 = L_W - 1;

/// Field offsets in [`Cpu`].
struct Off;
impl Off {
    fn reg(i: usize) -> u32 {
        (offset_of!(Cpu, regs) + 4 * i) as u32
    }
    fn eip() -> u32 {
        offset_of!(Cpu, eip) as u32
    }
    #[allow(dead_code)]
    fn eflags() -> u32 {
        offset_of!(Cpu, eflags) as u32
    }
    fn lazy() -> u32 {
        offset_of!(Cpu, lazy) as u32
    }
    fn fs() -> u32 {
        offset_of!(Cpu, fs_base) as u32
    }
    fn gs() -> u32 {
        offset_of!(Cpu, gs_base) as u32
    }
    fn budget() -> u32 {
        offset_of!(Cpu, jit_budget) as u32
    }
}

/// A translated block.
pub struct Block {
    pub start: u32,
    /// first byte after the block's last instruction
    pub end: u32,
    pub instructions: usize,
    /// statically known successors (branch targets, fall-through, return address of a call)
    pub next: Vec<u32>,
}

/// Where the status flags are at a point of the block.
#[derive(Clone, Copy, PartialEq)]
enum Flags {
    /// in the Cpu (`eflags`, or its own lazy state)
    Entry,
    /// lazy operation `op` (kind * 4 + size) with operands in L_FA/L_FB/L_FR/L_FC
    Known(u32),
}

struct Gen<'a> {
    c: Code,
    /// open wasm blocks inside the exit block
    depth: u32,
    flags: Flags,
    start: u32,
    pending: u32,
    ins: &'a Instruction,
    /// status flags read after this instruction (iced RflagsBits)
    live_after: u32,
    temps: u32,
}

const STATUS_BITS: u32 =
    RflagsBits::OF | RflagsBits::SF | RflagsBits::ZF | RflagsBits::AF | RflagsBits::CF | RflagsBits::PF;

fn reg_index(r: Register) -> Option<(u32, u32, bool)> {
    if r.is_gpr32() {
        Some((r.number() as u32, 32, false))
    } else if r.is_gpr16() {
        Some((r.number() as u32, 16, false))
    } else if r.is_gpr8() {
        let n = r.number() as u32;
        Some(if n < 4 { (n, 8, false) } else { (n - 4, 8, true) })
    } else {
        None
    }
}

fn mask(bits: u32) -> u32 {
    if bits >= 32 { u32::MAX } else { (1 << bits) - 1 }
}

/// An operand the generator can handle.
fn simple_operand(ins: &Instruction, i: u32) -> bool {
    match ins.op_kind(i) {
        OpKind::Register => reg_index(ins.op_register(i)).is_some(),
        OpKind::Memory => {
            let ok_reg = |r: Register| r == Register::None || r.is_gpr32();
            ok_reg(ins.memory_base())
                && ok_reg(ins.memory_index())
                && matches!(ins.memory_segment(), Register::DS | Register::SS | Register::ES | Register::CS | Register::FS | Register::GS)
                && matches!(ins.memory_size().size(), 1 | 2 | 4)
        }
        OpKind::Immediate8 | OpKind::Immediate16 | OpKind::Immediate32 | OpKind::Immediate8to16 | OpKind::Immediate8to32 => true,
        _ => false,
    }
}

fn op_bits(ins: &Instruction, i: u32) -> u32 {
    match ins.op_kind(i) {
        OpKind::Register => reg_index(ins.op_register(i)).map_or(32, |r| r.1),
        OpKind::Memory => ins.memory_size().size() as u32 * 8,
        OpKind::Immediate8 => 8,
        OpKind::Immediate16 | OpKind::Immediate8to16 => 16,
        _ => 32,
    }
}

fn cc_index(cc: ConditionCode) -> u32 {
    match cc {
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
        _ => 15,
    }
}

/// iced RflagsBits a condition reads
fn cc_reads(cc: u32) -> u32 {
    match cc >> 1 {
        0 => RflagsBits::OF,
        1 => RflagsBits::CF,
        2 => RflagsBits::ZF,
        3 => RflagsBits::CF | RflagsBits::ZF,
        4 => RflagsBits::SF,
        5 => RflagsBits::PF,
        6 => RflagsBits::SF | RflagsBits::OF,
        _ => RflagsBits::ZF | RflagsBits::SF | RflagsBits::OF,
    }
}

impl<'a> Gen<'a> {
    fn temp(&mut self) -> u32 {
        let t = L_T + self.temps;
        self.temps += 1;
        assert!(self.temps <= N_T);
        t
    }

    fn eip(&self) -> u32 {
        self.ins.ip32()
    }

    /// Leaves the block: continue at the address on top of the stack with status `st`.
    fn exit_with(&mut self, st: i32) {
        self.c.set(L_NEXT).i32c(st).set(L_ST).br(self.depth);
    }

    /// Leaves the block so the interpreter runs the current instruction.
    fn bail(&mut self) {
        let eip = self.eip();
        self.c.u32c(eip);
        self.exit_with(RET_INTERP);
    }

    // ---- registers

    fn get_reg(&mut self, r: Register) {
        let (i, bits, high) = reg_index(r).unwrap();
        self.c.get(L_REG + i);
        match (bits, high) {
            (32, _) => {}
            (16, _) => {
                self.c.u32c(0xffff).and();
            }
            (8, false) => {
                self.c.u32c(0xff).and();
            }
            _ => {
                self.c.i32c(8).shr_u().u32c(0xff).and();
            }
        }
    }

    /// Sets register `r` from the value in local `v`.
    fn set_reg_from(&mut self, r: Register, v: u32) {
        let (i, bits, high) = reg_index(r).unwrap();
        let l = L_REG + i;
        match (bits, high) {
            (32, _) => {
                self.c.get(v).set(l);
            }
            (16, _) => {
                self.c.get(l).u32c(0xffff_0000).and().get(v).u32c(0xffff).and().or().set(l);
            }
            (8, false) => {
                self.c.get(l).u32c(0xffff_ff00).and().get(v).u32c(0xff).and().or().set(l);
            }
            _ => {
                self.c.get(l).u32c(0xffff_00ff).and().get(v).u32c(0xff).and().i32c(8).shl().or().set(l);
            }
        }
    }

    // ---- memory

    /// Computes the linear address of the memory operand into a new temporary and checks it
    /// (leaving the block if outside the guest range).
    fn addr(&mut self, size: u32) -> u32 {
        let ins = self.ins;
        let t = self.temp();
        self.c.u32c(ins.memory_displacement32());
        if ins.memory_base() != Register::None {
            self.c.get(L_REG + ins.memory_base().number() as u32).add();
        }
        if ins.memory_index() != Register::None {
            self.c.get(L_REG + ins.memory_index().number() as u32);
            match ins.memory_index_scale() {
                1 => {}
                s => {
                    self.c.i32c(s.trailing_zeros() as i32).shl();
                }
            }
            self.c.add();
        }
        match ins.memory_segment() {
            Register::FS => {
                self.c.get(L_CPU).load32(Off::fs()).add();
            }
            Register::GS => {
                self.c.get(L_CPU).load32(Off::gs()).add();
            }
            _ => {}
        }
        self.c.set(t);
        self.check(t, size);
        t
    }

    /// Leaves the block unless [addr, addr + size) is inside the guest range.
    fn check(&mut self, t: u32, size: u32) {
        self.c.get(t).u32c(GUEST_LO).sub().u32c(GUEST_HI - GUEST_LO - size).gt_u().if_();
        self.depth += 1;
        self.bail();
        self.depth -= 1;
        self.c.end();
    }

    fn load(&mut self, bits: u32) {
        match bits {
            8 => self.c.load8u(0),
            16 => self.c.load16u(0),
            _ => self.c.load32(0),
        };
    }

    fn store(&mut self, bits: u32) {
        match bits {
            8 => self.c.store8(0),
            16 => self.c.store16(0),
            _ => self.c.store32(0),
        };
    }

    // ---- operands

    /// Pushes operand `i`. `addr` is the checked address if it is a memory operand.
    fn read_op(&mut self, i: u32, addr: Option<u32>) {
        let ins = self.ins;
        match ins.op_kind(i) {
            OpKind::Register => self.get_reg(ins.op_register(i)),
            OpKind::Memory => {
                self.c.get(addr.unwrap());
                self.load(op_bits(ins, i));
            }
            _ => {
                let v = ins.immediate(i) as u32 & mask(op_bits(ins, 0).max(op_bits(ins, i)));
                self.c.u32c(v);
            }
        }
    }

    /// Writes operand `i` from local `v`.
    fn write_op(&mut self, i: u32, addr: Option<u32>, v: u32) {
        let ins = self.ins;
        match ins.op_kind(i) {
            OpKind::Register => self.set_reg_from(ins.op_register(i), v),
            _ => {
                self.c.get(addr.unwrap()).get(v);
                self.store(op_bits(ins, i));
            }
        }
    }

    /// The checked address of the instruction's memory operand, if it has one.
    fn mem_addr(&mut self) -> Option<u32> {
        let ins = self.ins;
        (0..ins.op_count()).find(|&i| ins.op_kind(i) == OpKind::Memory).map(|i| self.addr(op_bits(ins, i) / 8))
    }

    // ---- flags

    fn flags_live(&self, bits: u32) -> bool {
        self.live_after & bits != 0
    }

    /// Records a lazy flag operation (if anything reads its flags).
    fn set_lazy(&mut self, kind: u32, bits: u32, a: Option<u32>, b: Option<u32>, r: u32, c: Option<u32>) {
        if !self.flags_live(STATUS_BITS) {
            return;
        }
        let op = lazy::op(kind, bits);
        self.c.u32c(op).set(L_OP);
        if let Some(a) = a {
            self.c.get(a).set(L_FA);
        }
        if let Some(b) = b {
            self.c.get(b).set(L_FB);
        }
        self.c.get(r).set(L_FR);
        if let Some(c) = c {
            self.c.get(c).set(L_FC);
        }
        self.flags = Flags::Known(op);
    }

    /// Pushes EFLAGS (exact status flags).
    fn eflags(&mut self) {
        match self.flags {
            Flags::Entry => {
                self.c.get(L_CPU).call(F_EFLAGS);
            }
            Flags::Known(op) => {
                self.c.u32c(op).get(L_FA).get(L_FB).get(L_FR).get(L_FC).call(F_STATUS);
            }
        }
    }

    /// Pushes condition `cc` (0/1).
    fn cond(&mut self, cc: u32) {
        let neg = cc & 1 != 0;
        if let Flags::Known(op) = self.flags {
            let bits = 8 << (op & 3);
            let m = mask(bits);
            let kind = op >> 2;
            let sext = |c: &mut Code, l: u32| {
                c.get(l);
                match bits {
                    8 => {
                        c.extend8_s();
                    }
                    16 => {
                        c.extend16_s();
                    }
                    _ => {}
                }
            };
            let zero = |c: &mut Code| {
                c.get(L_FR);
                if bits < 32 {
                    c.u32c(m).and();
                }
            };
            let sign = |c: &mut Code| {
                c.get(L_FR).i32c(bits as i32 - 1).shr_u().i32c(1).and();
            };
            let done = match (kind, cc >> 1) {
                (lazy::SUB, 1) => {
                    self.c.get(L_FA).u32c(m).and().get(L_FB).u32c(m).and().lt_u();
                    true
                }
                (lazy::SUB, 3) => {
                    self.c.get(L_FA).u32c(m).and().get(L_FB).u32c(m).and().le_u();
                    true
                }
                (lazy::SUB, 6) => {
                    sext(&mut self.c, L_FA);
                    sext(&mut self.c, L_FB);
                    self.c.lt_s();
                    true
                }
                (lazy::SUB, 7) => {
                    sext(&mut self.c, L_FA);
                    sext(&mut self.c, L_FB);
                    self.c.le_s();
                    true
                }
                (lazy::ADD, 1) => {
                    zero(&mut self.c);
                    self.c.get(L_FA).u32c(m).and().lt_u();
                    true
                }
                (lazy::LOGIC, 0) | (lazy::LOGIC, 1) => {
                    self.c.i32c(0);
                    true
                }
                (lazy::LOGIC, 3) => {
                    zero(&mut self.c);
                    self.c.eqz();
                    true
                }
                (lazy::LOGIC, 6) => {
                    sign(&mut self.c);
                    true
                }
                (lazy::LOGIC, 7) => {
                    zero(&mut self.c);
                    self.c.eqz();
                    sign(&mut self.c);
                    self.c.or();
                    true
                }
                (lazy::ADD | lazy::SUB | lazy::LOGIC | lazy::INC | lazy::DEC | lazy::ADC | lazy::SBB | lazy::MUL | lazy::SHL | lazy::SHR | lazy::SAR, 2) => {
                    zero(&mut self.c);
                    self.c.eqz();
                    true
                }
                (lazy::ADD | lazy::SUB | lazy::LOGIC | lazy::INC | lazy::DEC | lazy::ADC | lazy::SBB | lazy::MUL | lazy::SHL | lazy::SHR | lazy::SAR, 4) => {
                    sign(&mut self.c);
                    true
                }
                _ => false,
            };
            if done {
                if neg {
                    self.c.i32c(1).xor();
                }
                return;
            }
        }
        // from EFLAGS
        let t = self.temp();
        self.eflags();
        self.c.set(t);
        let bit = |c: &mut Code, n: i32| {
            c.get(t).i32c(n).shr_u().i32c(1).and();
        };
        match cc >> 1 {
            0 => bit(&mut self.c, 11),
            1 => bit(&mut self.c, 0),
            2 => bit(&mut self.c, 6),
            3 => {
                self.c.get(t).u32c(0x41).and().i32c(0).ne();
            }
            4 => bit(&mut self.c, 7),
            5 => bit(&mut self.c, 2),
            6 => {
                self.c.get(t).i32c(7).shr_u().get(t).i32c(11).shr_u().xor().i32c(1).and();
            }
            _ => {
                self.c.get(t).i32c(7).shr_u().get(t).i32c(11).shr_u().xor().get(t).i32c(6).shr_u().or().i32c(1).and();
            }
        }
        if neg {
            self.c.i32c(1).xor();
        }
    }

    // ---- stack

    /// Pushes the 32-bit value in local `v`.
    fn push32(&mut self, v: u32) {
        let t = self.temp();
        self.c.get(L_REG + 4).i32c(4).sub().set(t);
        self.check(t, 4);
        self.c.get(t).get(v).store32(0);
        self.c.get(t).set(L_REG + 4);
    }

    /// Pops 32 bits into a new temporary.
    fn pop32(&mut self) -> u32 {
        let t = self.temp();
        let sp = L_REG + 4;
        self.check(sp, 4);
        self.c.get(sp).load32(0).set(t);
        self.c.get(sp).i32c(4).add().set(sp);
        t
    }

    // ---- the interpreter, for one instruction

    /// Runs the current instruction through the `step` import. With `ends`, leaves the
    /// block afterwards at whatever EIP the instruction produced.
    fn fallback(&mut self, ends: bool) {
        self.spill(self.eip());
        self.c.get(L_CPU).call(F_STEP).set(L_ST);
        self.reload();
        self.c.i32c(-1).set(L_OP);
        self.flags = Flags::Entry;
        self.c.get(L_ST).if_();
        self.depth += 1;
        self.c.get(L_CPU).load32(Off::eip());
        self.c.set(L_NEXT).br(self.depth);
        self.depth -= 1;
        self.c.end();
        if ends {
            self.c.get(L_CPU).load32(Off::eip());
            self.exit_with(RET_CONTINUE);
        }
    }

    /// Writes registers, EIP and the lazy flags back to the Cpu.
    fn spill(&mut self, eip: u32) {
        for i in 0..8 {
            self.c.get(L_CPU).get(L_REG + i).store32(Off::reg(i as usize));
        }
        self.c.get(L_CPU).u32c(eip).store32(Off::eip());
        if let Flags::Known(op) = self.flags {
            let l = Off::lazy();
            self.c.get(L_CPU).u32c(op).store32(l);
            self.c.get(L_CPU).get(L_FA).store32(l + 4);
            self.c.get(L_CPU).get(L_FB).store32(l + 8);
            self.c.get(L_CPU).get(L_FR).store32(l + 12);
            self.c.get(L_CPU).get(L_FC).store32(l + 16);
        }
    }

    fn reload(&mut self) {
        for i in 0..8 {
            self.c.get(L_CPU).load32(Off::reg(i as usize)).set(L_REG + i);
        }
    }

    // ---- instructions

    /// Emits one instruction; returns false if it ended the block.
    fn emit(&mut self, last: bool) -> bool {
        use Mnemonic::*;
        let ins = self.ins;
        self.temps = 0;
        let simple = (0..ins.op_count()).all(|i| simple_operand(ins, i));
        let next = ins.next_ip32();
        match ins.mnemonic() {
            Nop | Pause | Endbr32 | Prefetchnta | Prefetcht0 | Prefetcht1 | Prefetcht2 | Prefetchw | Lfence | Mfence | Sfence => {}

            Mov | Movzx if simple => {
                let a = self.mem_addr();
                let v = self.temp();
                self.read_op(1, a);
                self.c.set(v);
                self.write_op(0, a, v);
            }
            Movsx if simple => {
                let a = self.mem_addr();
                let v = self.temp();
                self.read_op(1, a);
                if op_bits(ins, 1) == 8 {
                    self.c.extend8_s();
                } else {
                    self.c.extend16_s();
                }
                self.c.set(v);
                self.write_op(0, a, v);
            }
            Lea if op_bits(ins, 0) == 32 && simple && ins.memory_segment() != Register::FS && ins.memory_segment() != Register::GS => {
                let v = self.temp();
                self.c.u32c(ins.memory_displacement32());
                if ins.memory_base() != Register::None {
                    self.c.get(L_REG + ins.memory_base().number() as u32).add();
                }
                if ins.memory_index() != Register::None {
                    self.c.get(L_REG + ins.memory_index().number() as u32);
                    if ins.memory_index_scale() > 1 {
                        self.c.i32c(ins.memory_index_scale().trailing_zeros() as i32).shl();
                    }
                    self.c.add();
                }
                self.c.set(v);
                self.write_op(0, None, v);
            }
            Xchg if simple => {
                let a = self.mem_addr();
                let (x, y) = (self.temp(), self.temp());
                self.read_op(0, a);
                self.c.set(x);
                self.read_op(1, a);
                self.c.set(y);
                self.write_op(0, a, y);
                self.write_op(1, a, x);
            }
            Bswap if op_bits(ins, 0) == 32 => {
                let r = L_REG + ins.op_register(0).number() as u32;
                let v = self.temp();
                self.c.get(r).set(v);
                // ((v >> 24) | (v << 24)) | ((v >> 8) & 0xff00) | ((v << 8) & 0xff0000)
                self.c.get(v).i32c(24).shr_u().get(v).i32c(24).shl().or();
                self.c.get(v).i32c(8).shr_u().u32c(0xff00).and().or();
                self.c.get(v).i32c(8).shl().u32c(0xff_0000).and().or().set(r);
            }

            Add | Sub | Cmp | And | Or | Xor | Test | Adc | Sbb if simple => {
                let bits = op_bits(ins, 0);
                let m = mask(bits);
                let addr = self.mem_addr();
                let (a, b, r) = (self.temp(), self.temp(), self.temp());
                let carry = if matches!(ins.mnemonic(), Adc | Sbb) {
                    let c = self.temp();
                    self.cond(2);
                    self.c.set(c);
                    Some(c)
                } else {
                    None
                };
                self.read_op(0, addr);
                self.c.set(a);
                self.read_op(1, addr);
                self.c.u32c(m).and().set(b);
                self.c.get(a).get(b);
                let kind = match ins.mnemonic() {
                    Add => {
                        self.c.add();
                        lazy::ADD
                    }
                    Adc => {
                        self.c.add().get(carry.unwrap()).add();
                        lazy::ADC
                    }
                    Sub | Cmp => {
                        self.c.sub();
                        lazy::SUB
                    }
                    Sbb => {
                        self.c.sub().get(carry.unwrap()).sub();
                        lazy::SBB
                    }
                    And | Test => {
                        self.c.and();
                        lazy::LOGIC
                    }
                    Or => {
                        self.c.or();
                        lazy::LOGIC
                    }
                    _ => {
                        self.c.xor();
                        lazy::LOGIC
                    }
                };
                if bits < 32 {
                    self.c.u32c(m).and();
                }
                self.c.set(r);
                if !matches!(ins.mnemonic(), Cmp | Test) {
                    self.write_op(0, addr, r);
                }
                self.set_lazy(kind, bits, Some(a), Some(b), r, carry);
            }
            Inc | Dec if simple => {
                let bits = op_bits(ins, 0);
                let addr = self.mem_addr();
                let (a, r) = (self.temp(), self.temp());
                let carry = if self.flags_live(RflagsBits::CF) {
                    let c = self.temp();
                    self.cond(2);
                    self.c.set(c);
                    Some(c)
                } else {
                    None
                };
                self.read_op(0, addr);
                self.c.tee(a).i32c(1);
                if ins.mnemonic() == Inc {
                    self.c.add();
                } else {
                    self.c.sub();
                }
                if bits < 32 {
                    self.c.u32c(mask(bits)).and();
                }
                self.c.set(r);
                self.write_op(0, addr, r);
                let kind = if ins.mnemonic() == Inc { lazy::INC } else { lazy::DEC };
                self.set_lazy(kind, bits, Some(a), None, r, carry);
            }
            Neg | Not if simple => {
                let bits = op_bits(ins, 0);
                let addr = self.mem_addr();
                let (a, r, z) = (self.temp(), self.temp(), self.temp());
                self.read_op(0, addr);
                self.c.set(a);
                if ins.mnemonic() == Neg {
                    self.c.i32c(0).get(a).sub();
                } else {
                    self.c.get(a).i32c(-1).xor();
                }
                if bits < 32 {
                    self.c.u32c(mask(bits)).and();
                }
                self.c.set(r);
                self.write_op(0, addr, r);
                if ins.mnemonic() == Neg {
                    self.c.i32c(0).set(z);
                    self.set_lazy(lazy::SUB, bits, Some(z), Some(a), r, None);
                }
            }
            Shl | Sal | Shr | Sar | Rol | Ror if simple && ins.op_count() == 2 => {
                let bits = op_bits(ins, 0);
                let imm = ins.op_kind(1) != OpKind::Register;
                let count = if imm { ins.immediate(1) as u32 & 31 } else { 0 };
                let rot = matches!(ins.mnemonic(), Rol | Ror);
                // flags that depend on a count only known at run time (or rotates, which keep
                // the other flags) go through the interpreter when anyone reads them
                if (imm && count == 0) && !rot {
                    // no-op, flags unchanged
                } else if (!imm || rot) && self.flags_live(STATUS_BITS) {
                    self.fallback(false);
                } else if rot && bits < 32 {
                    self.fallback(false);
                } else {
                    let addr = self.mem_addr();
                    let (a, n, r) = (self.temp(), self.temp(), self.temp());
                    self.read_op(0, addr);
                    self.c.set(a);
                    if imm {
                        self.c.u32c(count).set(n);
                    } else {
                        self.c.get(L_REG + 1).i32c(31).and().set(n);
                    }
                    let m = mask(bits);
                    match ins.mnemonic() {
                        Shl | Sal => {
                            self.c.get(a).get(n).shl();
                            if bits < 32 {
                                self.c.u32c(m).and();
                            }
                        }
                        Shr => {
                            self.c.get(a).get(n).shr_u();
                        }
                        Sar => {
                            self.c.get(a);
                            match bits {
                                8 => {
                                    self.c.extend8_s();
                                }
                                16 => {
                                    self.c.extend16_s();
                                }
                                _ => {}
                            }
                            self.c.get(n).shr_s();
                            if bits < 32 {
                                self.c.u32c(m).and();
                            }
                        }
                        Rol => {
                            self.c.get(a).get(n).rotl();
                        }
                        _ => {
                            self.c.get(a).get(n).rotr();
                        }
                    }
                    self.c.set(r);
                    if !imm {
                        // a zero count leaves the operand alone (and a 8/16-bit SHL/SHR by more
                        // than the width is handled by the masks above)
                    }
                    self.write_op(0, addr, r);
                    if !rot && imm {
                        let kind = match ins.mnemonic() {
                            Shl | Sal => lazy::SHL,
                            Shr => lazy::SHR,
                            _ => lazy::SAR,
                        };
                        self.set_lazy(kind, bits, Some(a), Some(n), r, None);
                    }
                }
            }
            Imul if simple && ins.op_count() >= 2 && op_bits(ins, 0) == 32 => {
                let addr = self.mem_addr();
                let (r, o) = (self.temp(), self.temp());
                let (x, y) = if ins.op_count() == 3 { (1, 2) } else { (0, 1) };
                self.read_op(x, addr);
                self.c.i64_extend_s();
                self.read_op(y, addr);
                if ins.op_kind(y) != OpKind::Register && ins.op_kind(y) != OpKind::Memory {
                    // immediates are sign-extended to the operand size already
                }
                self.c.i64_extend_s().i64_mul().set(L_W);
                self.c.get(L_W).wrap().set(r);
                self.write_op(0, addr, r);
                if self.flags_live(STATUS_BITS) {
                    // overflow: the 64-bit product differs from the sign extension of its low half
                    self.c.get(L_W).get(r).i64_extend_s().op(0x52 /* i64.ne */).set(o);
                    self.set_lazy(lazy::MUL, 32, None, None, r, Some(o));
                }
            }
            Cdq => {
                self.c.get(L_REG).i32c(31).shr_s().set(L_REG + 2);
            }
            Cwde => {
                self.c.get(L_REG).extend16_s().set(L_REG);
            }
            Setb | Setae | Sete | Setne | Setbe | Seta | Sets | Setns | Setp | Setnp | Setl | Setge | Setle | Setg | Seto | Setno if simple => {
                let addr = self.mem_addr();
                let v = self.temp();
                self.cond(cc_index(ins.condition_code()));
                self.c.set(v);
                self.write_op(0, addr, v);
            }
            Cmovb | Cmovae | Cmove | Cmovne | Cmovbe | Cmova | Cmovs | Cmovns | Cmovp | Cmovnp | Cmovl | Cmovge | Cmovle | Cmovg | Cmovo | Cmovno
                if simple && op_bits(ins, 0) == 32 =>
            {
                let addr = self.mem_addr();
                let v = self.temp();
                let dst = L_REG + ins.op_register(0).number() as u32;
                self.read_op(1, addr);
                self.c.get(dst);
                self.cond(cc_index(ins.condition_code()));
                self.c.select().set(v);
                self.c.get(v).set(dst);
            }
            Push if simple && (op_bits(ins, 0) == 32 || ins.op_kind(0) == OpKind::Immediate8to32 || ins.op_kind(0) == OpKind::Immediate32) => {
                let addr = self.mem_addr();
                let v = self.temp();
                self.read_op(0, addr);
                self.c.set(v);
                self.push32(v);
            }
            Pop if simple && op_bits(ins, 0) == 32 && ins.op_kind(0) == OpKind::Register => {
                let v = self.pop32();
                self.set_reg_from(ins.op_register(0), v);
            }
            Leave => {
                let sp = L_REG + 4;
                self.c.get(L_REG + 5).set(sp);
                let v = self.pop32();
                self.c.get(v).set(L_REG + 5);
            }

            // ---- control flow (always ends the block)
            Jmp if ins.op_kind(0) == OpKind::NearBranch32 => {
                let target = ins.near_branch32();
                self.c.u32c(target);
                if target == self.start {
                    self.back_edge();
                } else {
                    self.exit_with(RET_CONTINUE);
                }
                return false;
            }
            Jo | Jno | Jb | Jae | Je | Jne | Jbe | Ja | Js | Jns | Jp | Jnp | Jl | Jge | Jle | Jg if ins.op_kind(0) == OpKind::NearBranch32 => {
                let target = ins.near_branch32();
                self.cond(cc_index(ins.condition_code()));
                self.c.if_();
                self.depth += 1;
                self.c.u32c(target);
                if target == self.start {
                    self.back_edge();
                } else {
                    self.exit_with(RET_CONTINUE);
                }
                self.depth -= 1;
                self.c.end();
                self.c.u32c(next);
                self.exit_with(RET_CONTINUE);
                return false;
            }
            Call if ins.op_kind(0) == OpKind::NearBranch32 => {
                let ret = self.temp();
                self.c.u32c(next).set(ret);
                self.push32(ret);
                self.c.u32c(ins.near_branch32());
                self.exit_with(RET_CONTINUE);
                return false;
            }
            Call if simple && op_bits(ins, 0) == 32 => {
                let addr = self.mem_addr();
                let (t, ret) = (self.temp(), self.temp());
                self.read_op(0, addr);
                self.c.set(t);
                self.c.u32c(next).set(ret);
                self.push32(ret);
                self.c.get(t);
                self.exit_with(RET_CONTINUE);
                return false;
            }
            Jmp if simple && op_bits(ins, 0) == 32 => {
                let addr = self.mem_addr();
                self.read_op(0, addr);
                self.exit_with(RET_CONTINUE);
                return false;
            }
            Ret => {
                let t = self.pop32();
                if ins.op_count() == 1 {
                    let n = ins.immediate16() as i32;
                    self.c.get(L_REG + 4).i32c(n).add().set(L_REG + 4);
                }
                self.c.get(t);
                self.exit_with(RET_CONTINUE);
                return false;
            }

            _ => {
                let ends = ins.flow_control() != FlowControl::Next;
                self.fallback(ends);
                if ends {
                    return false;
                }
            }
        }
        if last {
            self.c.u32c(next);
            self.exit_with(RET_CONTINUE);
        }
        true
    }

    /// A branch to the block's own start (target on the stack): loop, unless a signal is
    /// pending or the budget ran out.
    fn back_edge(&mut self) {
        let t = self.temp();
        self.c.set(t);
        // budget
        self.c.get(L_CPU).get(L_CPU).load32(Off::budget()).i32c(1).sub().store32(Off::budget());
        self.c.get(L_CPU).load32(Off::budget()).i32c(0).le_s();
        if self.pending != 0 {
            self.c.u32c(self.pending).load32(0).or();
        }
        self.c.if_();
        self.depth += 1;
        self.c.get(t);
        self.exit_with(RET_CONTINUE);
        self.depth -= 1;
        self.c.end();
        // the loop is the block just inside the exit block: depth - 1 from here is the loop
        self.c.br(self.depth - 1);
    }
}

/// Decodes the block at `eip` and flag liveness for it.
fn decode<M: Memory>(mem: &mut M, eip: u32) -> Vec<Instruction> {
    let mut out = Vec::new();
    let mut ip = eip;
    while out.len() < MAX_INS {
        let mut buf = [0u8; 15];
        let n = mem.fetch(ip, &mut buf);
        let mut dec = Decoder::with_ip(32, &buf[..n], ip as u64, DecoderOptions::NONE);
        let ins = dec.decode();
        if ins.is_invalid() {
            break;
        }
        ip = ins.next_ip32();
        let fc = ins.flow_control();
        out.push(ins);
        if fc != FlowControl::Next {
            break;
        }
    }
    out
}

/// Adds the block at `eip` to `module` as a function; None if there is nothing to translate
/// (an undecodable first instruction).
pub fn translate<M: Memory>(module: &mut Module, mem: &mut M, eip: u32, pending: u32) -> Option<Block> {
    let list = decode(mem, eip);
    if list.is_empty() {
        return None;
    }
    if module.types.is_empty() {
        module.types.push(FuncType { params: alloc::vec![I32], results: alloc::vec![I32] });
        module.types.push(FuncType { params: alloc::vec![I32; 5], results: alloc::vec![I32] });
        module.imports.push(Import::Memory);
        module.imports.push(Import::Func("step", 0));
        module.imports.push(Import::Func("eflags", 0));
        module.imports.push(Import::Func("status", 1));
    }
    // liveness of the status flags after each instruction: everything is live at the end
    let mut live = alloc::vec![0u32; list.len()];
    let mut l = STATUS_BITS;
    for (i, ins) in list.iter().enumerate().rev() {
        live[i] = l;
        let written = ins.rflags_modified() & STATUS_BITS;
        let mut read = ins.rflags_read() & STATUS_BITS;
        if ins.mnemonic() == Mnemonic::Inc || ins.mnemonic() == Mnemonic::Dec {
            read |= RflagsBits::CF; // the lazy INC/DEC carries CF along
        }
        if ins.condition_code() != ConditionCode::None {
            read |= cc_reads(cc_index(ins.condition_code()));
        }
        l = (l & !written) | read;
    }

    let mut g = Gen { c: Code::default(), depth: 0, flags: Flags::Entry, start: eip, pending, ins: &list[0], live_after: 0, temps: 0 };
    // load the registers, no lazy op yet
    for i in 0..8 {
        g.c.get(L_CPU).load32(Off::reg(i as usize)).set(L_REG + i);
    }
    g.c.i32c(-1).set(L_OP);
    g.c.block(); // exit
    g.c.loop_(); // top: back edges
    g.depth = 1;
    let n = list.len();
    for (i, ins) in list.iter().enumerate() {
        g.ins = ins;
        g.live_after = live[i];
        if !g.emit(i + 1 == n) {
            break;
        }
    }
    g.c.end(); // loop
    g.c.end(); // exit
    // write back
    for i in 0..8 {
        g.c.get(L_CPU).get(L_REG + i).store32(Off::reg(i as usize));
    }
    g.c.get(L_CPU).get(L_NEXT).store32(Off::eip());
    g.c.get(L_OP).i32c(-1).ne().if_();
    let lo = Off::lazy();
    g.c.get(L_CPU).get(L_OP).store32(lo);
    g.c.get(L_CPU).get(L_FA).store32(lo + 4);
    g.c.get(L_CPU).get(L_FB).store32(lo + 8);
    g.c.get(L_CPU).get(L_FR).store32(lo + 12);
    g.c.get(L_CPU).get(L_FC).store32(lo + 16);
    g.c.end();
    g.c.get(L_ST);

    let mut locals = alloc::vec![I32; N_LOCALS_I32 as usize];
    locals.push(I64);
    module.funcs.push(Func { ty: 0, locals, code: g.c });
    let _ = F_BLOCK0;
    let last = list.last().unwrap();
    let mut next = Vec::new();
    let direct = matches!(last.op0_kind(), OpKind::NearBranch32 | OpKind::NearBranch16);
    match last.flow_control() {
        FlowControl::ConditionalBranch | FlowControl::Call if direct => {
            next.push(last.near_branch_target() as u32);
            next.push(last.next_ip32());
        }
        FlowControl::UnconditionalBranch if direct => next.push(last.near_branch_target() as u32),
        FlowControl::Next | FlowControl::IndirectCall | FlowControl::Call | FlowControl::ConditionalBranch => {
            next.push(last.next_ip32())
        }
        _ => {}
    }
    Some(Block { start: eip, end: last.next_ip32(), instructions: n, next })
}
