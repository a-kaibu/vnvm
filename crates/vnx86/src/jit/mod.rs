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
//! start becomes a wasm loop; backward jumps count down `cpu.jit_budget` (signals are noticed
//! when it runs out).
//!
//! Imports of a block module: `env.memory` (the shared linear memory), `env.step(cpu) -> i32`,
//! `env.eflags(cpu) -> i32` (materializes and returns EFLAGS) and
//! `env.status(op, a, b, r, c) -> i32` (status flags of a lazy operation).

pub mod wasm;
pub mod dispatch;

use crate::lazy;
use crate::{Cpu, Fpu, Memory};
use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;
use core::mem::offset_of;
use iced_x86::{ConditionCode, Decoder, DecoderOptions, FlowControl, Instruction, Mnemonic, OpKind, Register, RflagsBits};
use wasm::{Code, Func, FuncType, Import, Module, F64, I32, I64};

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

// parameters of a region function: what a jump between regions hands over
const L_CPU: u32 = 0;
const L_ARG: u32 = 1; // entry: table index | block << ENTRY_SHIFT
const L_REG: u32 = 2; // 8 registers
const L_OP: u32 = 10; // lazy flags (L_OP -1: in the Cpu)
const L_FA: u32 = 11;
const L_FB: u32 = 12;
const L_FR: u32 = 13;
const L_FC: u32 = 14;
const L_BUD: u32 = 15; // cpu.jit_budget while inside
pub(crate) const N_PARAMS: u32 = 16;
// locals
const L_NEXT: u32 = 16;
const L_ST: u32 = 17;
const L_BLK: u32 = 18;
const L_T: u32 = 19; // 6 temporaries
const N_T: u32 = 6;
const L_W: u32 = L_T + N_T; // i64 temporary
const L_F0: u32 = L_W + 1; // two f64 temporaries
const L_F1: u32 = L_W + 2;
const N_LOCALS_I32: u32 = L_W - N_PARAMS;

/// A hash table entry's value: the function's table index | the block's index << ENTRY_SHIFT.
pub const ENTRY_SHIFT: u32 = 20;
/// Most blocks in one region.
pub const MAX_BLOCKS: usize = 1 << 11;
/// Most instructions in one region: V8's optimizing compiler takes superlinear time on big
/// functions (and runs the code in its baseline tier until it is done).
const MAX_REGION_INS: usize = 1500;

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
    fn fpu(field: usize) -> u32 {
        (offset_of!(Cpu, fpu) + field) as u32
    }
    fn fregs() -> u32 {
        Off::fpu(offset_of!(Fpu, regs))
    }
    fn fints() -> u32 {
        Off::fpu(offset_of!(Fpu, ints))
    }
    fn fexact() -> u32 {
        Off::fpu(offset_of!(Fpu, exact))
    }
    fn fempty() -> u32 {
        Off::fpu(offset_of!(Fpu, empty))
    }
    fn ftop() -> u32 {
        Off::fpu(offset_of!(Fpu, top))
    }
    fn xmm(r: Register) -> u32 {
        (offset_of!(Cpu, xmm) + 16 * (r as usize - Register::XMM0 as usize)) as u32
    }
}

/// A translated region: one function holding blocks found from an entry by following direct
/// branches. Blocks jump to each other inside the function; returns and indirect jumps look
/// the target up in the embedder's hash table and stay inside if it is one of the region's own.
pub struct Region {
    /// block start addresses; block i is entered with `(table index | i << ENTRY_SHIFT)`
    pub starts: Vec<u32>,
    pub instructions: usize,
}

/// Where the status flags are at a point of the block.
#[derive(Clone, Copy, PartialEq)]
enum Flags {
    /// not known statically: the lazy operation in L_OP/L_FA.. if L_OP != -1, else in the Cpu
    /// (`eflags`, or its own lazy state)
    Entry,
    /// lazy operation `op` (kind * 4 + size) with operands in L_FA/L_FB/L_FR/L_FC
    Known(u32),
}

struct Gen<'a> {
    c: Code,
    /// open wasm blocks inside the exit block
    depth: u32,
    flags: Flags,
    targets: &'a BTreeMap<u32, u32>,
    /// the current block's start, and `depth` at its own loop label
    block_start: u32,
    block_depth: u32,
    block_index: u32,
    /// return addresses of the region's own calls (checked first by RET)
    returns: &'a [u32],
    ins: &'a Instruction,
    /// status flags read after this instruction (iced RflagsBits)
    live_after: u32,
    temps: u32,
    /// Registers whose value was checked to be at least SAFE_MARGIN inside the guest range,
    /// with how far they moved since (push/pop): accesses near them need no check.
    safe: [Option<i32>; 8],
}

/// See [`Gen::safe`].
const SAFE_MARGIN: i32 = 0x8000;

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

/// A memory operand with 32-bit registers and an ordinary segment.
fn mem_ok(ins: &Instruction) -> bool {
    let ok_reg = |r: Register| r == Register::None || r.is_gpr32();
    ok_reg(ins.memory_base())
        && ok_reg(ins.memory_index())
        && matches!(ins.memory_segment(), Register::DS | Register::SS | Register::ES | Register::CS | Register::FS | Register::GS)
}

/// x87 memory operand sizes the JIT handles (f32/f64).
fn fmem_ok(ins: &Instruction) -> bool {
    mem_ok(ins) && matches!(ins.memory_size(), iced_x86::MemorySize::Float32 | iced_x86::MemorySize::Float64)
}

/// Operand `i` is an XMM register or a 128-bit memory operand the JIT can address.
fn xmm_or_m128(ins: &Instruction, i: u32) -> bool {
    match ins.op_kind(i) {
        OpKind::Register => ins.op_register(i).is_xmm(),
        OpKind::Memory => mem_ok(ins) && ins.memory_size().size() == 16,
        _ => false,
    }
}

fn st_index(r: Register) -> u32 {
    r as u32 - Register::ST0 as u32
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

    // ---- x87 (the f64 model of crate::fpu; TOP is read from the Cpu each instruction)

    fn ftop_load(&mut self) -> u32 {
        let t = self.temp();
        self.c.get(L_CPU).load32(Off::ftop()).set(t);
        t
    }

    /// New local with the physical register of ST(i).
    fn fphys(&mut self, top: u32, i: u32) -> u32 {
        let p = self.temp();
        self.c.get(top);
        if i != 0 {
            self.c.i32c(i as i32).add();
        }
        self.c.i32c(7).and().set(p);
        p
    }

    /// Pushes the address of register `p` (the Fpu field offset goes on the load/store).
    fn freg_at(&mut self, p: u32, shift: i32) {
        self.c.get(p).i32c(shift).shl().get(L_CPU).add();
    }

    fn fbit(&mut self, field: u32, p: u32, set: bool) {
        self.c.get(L_CPU).get(L_CPU).load8u(field).i32c(1).get(p).shl();
        if set {
            self.c.or();
        } else {
            self.c.i32c(-1).xor().and();
        }
        self.c.store8(field);
    }

    /// regs[p] = f64 local `v`, not exact, not empty
    fn fset(&mut self, p: u32, v: u32) {
        self.freg_at(p, 3);
        self.c.get(v).f64store(Off::fregs());
        self.fbit(Off::fexact(), p, false);
        self.fbit(Off::fempty(), p, false);
    }

    /// TOP -= 1 (updating local `top`)
    fn fpush_top(&mut self, top: u32) {
        self.c.get(top).i32c(7).add().i32c(7).and().set(top);
        self.c.get(L_CPU).get(top).store32(Off::ftop());
    }

    fn fpop(&mut self, top: u32) {
        self.fbit(Off::fempty(), top, true);
        self.fbit(Off::fexact(), top, false);
        self.c.get(L_CPU).get(top).i32c(1).add().i32c(7).and().store32(Off::ftop());
    }

    /// Pushes the f32/f64 memory operand at checked address `a` as f64.
    fn fload_mem(&mut self, a: u32) {
        self.c.get(a);
        if self.ins.memory_size() == iced_x86::MemorySize::Float32 {
            self.c.f32load(0).op(0xbb /* f64.promote_f32 */);
        } else {
            self.c.f64load(0);
        }
    }

    /// Swaps bits `a` and `b` of a byte field.
    fn fswap_bits(&mut self, field: u32, a: u32, b: u32) {
        let (x, y) = (self.temp(), self.temp());
        self.c.get(L_CPU).load8u(field).set(x);
        // y = ((x >> a) ^ (x >> b)) & 1; x ^= y << a | y << b
        self.c.get(x).get(a).shr_u().get(x).get(b).shr_u().xor().i32c(1).and().set(y);
        self.c.get(L_CPU).get(x).get(y).get(a).shl().get(y).get(b).shl().or().xor().store8(field);
        self.temps -= 2;
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
        let base = ins.memory_base();
        let seg = ins.memory_segment();
        if base.is_gpr32() && ins.memory_index() == Register::None && seg != Register::FS && seg != Register::GS {
            let disp = ins.memory_displacement32() as i32;
            if self.near_safe(base.number(), disp, size) {
                return t;
            }
        }
        self.check(t, size);
        t
    }

    /// Whether [reg + disp, + size) is inside the guest range by what is known of `reg`,
    /// checking `reg` itself (once) when nothing is known yet.
    fn near_safe(&mut self, reg: usize, disp: i32, size: u32) -> bool {
        if disp.unsigned_abs() >= SAFE_MARGIN as u32 / 2 {
            return false;
        }
        let off = match self.safe[reg] {
            Some(o) => o,
            None => {
                let l = L_REG + reg as u32;
                self.c.get(l).u32c(GUEST_LO + SAFE_MARGIN as u32).sub().u32c(GUEST_HI - GUEST_LO - 2 * SAFE_MARGIN as u32).gt_u().if_();
                self.depth += 1;
                self.bail();
                self.depth -= 1;
                self.c.end();
                self.safe[reg] = Some(0);
                0
            }
        };
        let lo = off + disp;
        lo > -SAFE_MARGIN && lo + (size as i32) < SAFE_MARGIN
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
    /// The checked address of the memory operand, if any, for an access of `size` bytes.
    fn mem_addr_sized(&mut self, size: u32) -> Option<u32> {
        let ins = self.ins;
        (0..ins.op_count()).any(|i| ins.op_kind(i) == OpKind::Memory).then(|| self.addr(size))
    }

    /// Pushes the address part of a v128 store to operand `i` (the value follows).
    fn xmm_dst_prep(&mut self, i: u32, a: Option<u32>) {
        if self.ins.op_kind(i) == OpKind::Memory {
            self.c.get(a.unwrap());
        } else {
            self.c.get(L_CPU);
        }
    }

    fn xmm_dst_store(&mut self, i: u32) {
        if self.ins.op_kind(i) == OpKind::Memory {
            self.c.v128store(0);
        } else {
            self.c.v128store(Off::xmm(self.ins.op_register(i)));
        }
    }

    /// Pushes XMM register or m128 operand `i` as a v128.
    fn xmm_read(&mut self, i: u32, a: Option<u32>) {
        if self.ins.op_kind(i) == OpKind::Memory {
            self.c.get(a.unwrap()).v128load(0);
        } else {
            self.c.get(L_CPU).v128load(Off::xmm(self.ins.op_register(i)));
        }
    }

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
                lazy_to_cpu(&mut self.c);
                self.c.i32c(-1).set(L_OP);
                self.c.end();
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
        if !self.near_safe(4, -4, 4) {
            self.check(t, 4);
        }
        self.c.get(t).get(v).store32(0);
        self.c.get(t).set(L_REG + 4);
        self.safe[4] = self.safe[4].map(|o| o - 4);
    }

    /// Pops 32 bits into a new temporary.
    fn pop32(&mut self) -> u32 {
        let t = self.temp();
        let sp = L_REG + 4;
        if !self.near_safe(4, 0, 4) {
            self.check(sp, 4);
        }
        self.c.get(sp).load32(0).set(t);
        self.c.get(sp).i32c(4).add().set(sp);
        self.safe[4] = self.safe[4].map(|o| o + 4);
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
            self.jump_dyn();
        }
    }

    /// Writes registers, EIP and the lazy flags back to the Cpu.
    fn spill(&mut self, eip: u32) {
        for i in 0..8 {
            self.c.get(L_CPU).get(L_REG + i).store32(Off::reg(i as usize));
        }
        self.c.get(L_CPU).u32c(eip).store32(Off::eip());
        match self.flags {
            Flags::Known(op) => {
                self.c.u32c(op).set(L_OP);
                lazy_store(&mut self.c);
            }
            Flags::Entry => {
                lazy_to_cpu(&mut self.c);
                self.c.end();
            }
        }
    }

    fn reload(&mut self) {
        self.safe = [None; 8];
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
        // registers this instruction writes lose what was known about them (push/pop keep
        // ESP's track themselves)
        let mut written = 0u32;
        for u in iced_x86::InstructionInfoFactory::new().info(ins).used_registers() {
            use iced_x86::OpAccess::*;
            if matches!(u.access(), Write | CondWrite | ReadWrite | ReadCondWrite) && u.register().is_gpr() {
                written |= 1 << u.register().full_register32().number();
            }
        }
        if matches!(ins.mnemonic(), Push | Pop) {
            written &= !(1 << 4);
            if ins.mnemonic() == Pop && ins.op_kind(0) == OpKind::Register && ins.op_register(0).full_register32() == Register::ESP {
                written |= 1 << 4;
            }
        }
        let r = self.emit_ins(last, next, simple);
        for i in 0..8 {
            if written & (1 << i) != 0 {
                self.safe[i] = None;
            }
        }
        r
    }

    fn emit_ins(&mut self, last: bool, next: u32, simple: bool) -> bool {
        use Mnemonic::*;
        let ins = self.ins;
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
            Lea if op_bits(ins, 0) == 32
                && [ins.memory_base(), ins.memory_index()].iter().all(|&r| r == Register::None || r.is_gpr32()) =>
            {
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
                if !self.flags_live(STATUS_BITS) {
                    self.read_op(x, addr);
                    self.read_op(y, addr);
                    self.c.mul().set(r);
                    self.write_op(0, addr, r);
                    return self.after(last, next);
                }
                self.read_op(x, addr);
                self.c.i64_extend_s();
                self.read_op(y, addr);
                self.c.i64_extend_s().i64_mul().set(L_W);
                self.c.get(L_W).wrap().set(r);
                self.write_op(0, addr, r);
                {
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
                self.safe[4] = self.safe[5];
                let v = self.pop32();
                self.c.get(v).set(L_REG + 5);
            }


            // ---- x87
            Fld if ins.op_kind(0) == OpKind::Memory && fmem_ok(ins) => {
                let a = self.addr(ins.memory_size().size() as u32);
                self.fload_mem(a);
                self.c.set(L_F0);
                let top = self.ftop_load();
                self.fpush_top(top);
                self.fset(top, L_F0);
            }
            Fld if ins.op_kind(0) == OpKind::Register => {
                // copies the exact integer too
                let top = self.ftop_load();
                let pi = self.fphys(top, st_index(ins.op_register(0)));
                self.fpush_top(top);
                self.freg_at(top, 3);
                self.freg_at(pi, 3);
                self.c.f64load(Off::fregs()).f64store(Off::fregs());
                self.freg_at(top, 3);
                self.freg_at(pi, 3);
                self.c.load64(Off::fints()).store64(Off::fints());
                self.fbit(Off::fempty(), top, false);
                // exact[top] = exact[pi]
                self.fbit(Off::fexact(), top, false);
                self.c.get(L_CPU).get(L_CPU).load8u(Off::fexact());
                self.c.get(L_CPU).load8u(Off::fexact()).get(pi).shr_u().i32c(1).and().get(top).shl().or();
                self.c.store8(Off::fexact());
            }
            Fldz | Fld1 => {
                self.c.op(0x44); // f64.const
                self.c.b.extend_from_slice(&(if ins.mnemonic() == Fldz { 0.0f64 } else { 1.0 }).to_le_bytes());
                self.c.set(L_F0);
                let top = self.ftop_load();
                self.fpush_top(top);
                self.fset(top, L_F0);
            }
            Fild if ins.memory_size() == iced_x86::MemorySize::Int32 && mem_ok(ins) => {
                let a = self.addr(4);
                let top = self.ftop_load();
                self.fpush_top(top);
                self.freg_at(top, 3);
                self.c.get(a).load32(0).op(0xb7 /* f64.convert_i32_s */).f64store(Off::fregs());
                self.freg_at(top, 3);
                self.c.get(a).load32(0).i64_extend_s().store64(Off::fints());
                self.fbit(Off::fexact(), top, true);
                self.fbit(Off::fempty(), top, false);
            }
            Fst | Fstp if ins.op_kind(0) == OpKind::Memory && fmem_ok(ins) => {
                let a = self.addr(ins.memory_size().size() as u32);
                let top = self.ftop_load();
                self.c.get(a);
                self.freg_at(top, 3);
                self.c.f64load(Off::fregs());
                if ins.memory_size() == iced_x86::MemorySize::Float32 {
                    self.c.op(0xb6 /* f32.demote_f64 */).f32store(0);
                } else {
                    self.c.f64store(0);
                }
                if ins.mnemonic() == Fstp {
                    self.fpop(top);
                }
            }
            Fst | Fstp if ins.op_kind(0) == OpKind::Register => {
                let top = self.ftop_load();
                let pi = self.fphys(top, st_index(ins.op_register(0)));
                self.freg_at(top, 3);
                self.c.f64load(Off::fregs()).set(L_F0);
                self.fset(pi, L_F0);
                if ins.mnemonic() == Fstp {
                    self.fpop(top);
                }
            }
            Fxch if ins.op_count() == 2 => {
                let i = st_index(ins.op_register(1)).max(st_index(ins.op_register(0)));
                let top = self.ftop_load();
                let pb = self.fphys(top, i);
                // regs and ints (moved as f64 bits: loads and stores keep them exactly)
                for off in [Off::fregs(), Off::fints()] {
                    self.freg_at(top, 3);
                    self.c.f64load(off).set(L_F0);
                    self.freg_at(top, 3);
                    self.freg_at(pb, 3);
                    self.c.f64load(off).f64store(off);
                    self.freg_at(pb, 3);
                    self.c.get(L_F0).f64store(off);
                }
                self.fswap_bits(Off::fexact(), top, pb);
                self.fswap_bits(Off::fempty(), top, pb);
            }
            Fadd | Faddp | Fsub | Fsubp | Fsubr | Fsubrp | Fmul | Fmulp | Fdiv | Fdivp | Fdivr | Fdivrp
                if (ins.op_count() == 1 && ins.op_kind(0) == OpKind::Memory && fmem_ok(ins))
                    || (ins.op_count() == 2 && ins.op_kind(0) == OpKind::Register) =>
            {
                let mn = ins.mnemonic();
                // s -> L_F1, pd = physical register of the destination
                let (top, pd) = if ins.op_count() == 1 {
                    let a = self.addr(ins.memory_size().size() as u32);
                    self.fload_mem(a);
                    self.c.set(L_F1);
                    let top = self.ftop_load();
                    (top, top)
                } else {
                    let top = self.ftop_load();
                    let ps = self.fphys(top, st_index(ins.op_register(1)));
                    self.freg_at(ps, 3);
                    self.c.f64load(Off::fregs()).set(L_F1);
                    (top, self.fphys(top, st_index(ins.op_register(0))))
                };
                self.freg_at(pd, 3);
                self.c.f64load(Off::fregs());
                if matches!(mn, Fsubr | Fsubrp | Fdivr | Fdivrp) {
                    self.c.set(L_F0).get(L_F1).get(L_F0);
                } else {
                    self.c.get(L_F1);
                }
                self.c.op(match mn {
                    Fadd | Faddp => 0xa0,
                    Fsub | Fsubp | Fsubr | Fsubrp => 0xa1,
                    Fmul | Fmulp => 0xa2,
                    _ => 0xa3,
                });
                self.c.set(L_F0);
                self.fset(pd, L_F0);
                if matches!(mn, Faddp | Fsubp | Fsubrp | Fmulp | Fdivp | Fdivrp) {
                    self.fpop(top);
                }
            }
            Div | Idiv if op_bits(ins, 0) == 32 && simple => {
                let addr = self.mem_addr();
                let d = self.temp();
                self.read_op(0, addr);
                self.c.set(d);
                let (eax, edx) = (L_REG, L_REG + 2);
                // n = edx:eax
                self.c.get(edx).i64_extend_u().i64c(32).op(0x86).get(eax).i64_extend_u().op(0x84).set(L_W);
                if ins.mnemonic() == Div {
                    // #DE (divide by zero, quotient too big): the interpreter raises it
                    self.c.get(d).eqz().get(edx).get(d).ge_u().or().if_();
                    self.depth += 1;
                    self.bail();
                    self.depth -= 1;
                    self.c.end();
                    self.c.get(L_W).get(d).i64_extend_u().op(0x82 /* i64.rem_u */).wrap();
                    self.c.get(L_W).get(d).i64_extend_u().op(0x80 /* i64.div_u */).wrap().set(eax);
                    self.c.set(edx);
                } else {
                    // d == 0, d == -1 (MIN / -1 traps in wasm) or a quotient outside i32
                    let r = self.temp();
                    self.c.get(d).eqz().get(d).i32c(-1).eq().or().if_();
                    self.depth += 1;
                    self.bail();
                    self.depth -= 1;
                    self.c.end();
                    self.c.get(L_W).get(d).i64_extend_s().op(0x81 /* i64.rem_s */).wrap().set(r);
                    self.c.get(L_W).get(d).i64_extend_s().op(0x7f /* i64.div_s */).set(L_W);
                    self.c.get(L_W).get(L_W).wrap().i64_extend_s().op(0x52 /* i64.ne */).if_();
                    self.depth += 1;
                    self.bail();
                    self.depth -= 1;
                    self.c.end();
                    self.c.get(L_W).wrap().set(eax);
                    self.c.get(r).set(edx);
                }
            }

            // ---- SSE: 128-bit moves and bitwise logic on v128
            Movdqu | Movdqa | Movups | Movaps | Movupd | Movapd | Lddqu if xmm_or_m128(ins, 0) && xmm_or_m128(ins, 1) => {
                let a = self.mem_addr_sized(16);
                self.xmm_dst_prep(0, a);
                self.xmm_read(1, a);
                self.xmm_dst_store(0);
            }
            Pxor | Xorps | Xorpd if ins.op_kind(1) == OpKind::Register && ins.op_register(0) == ins.op_register(1) => {
                self.c.get(L_CPU).simd(12).b.extend_from_slice(&[0; 16]); // v128.const 0
                self.c.v128store(Off::xmm(ins.op_register(0)));
            }
            Pxor | Xorps | Xorpd | Por | Orps | Orpd | Pand | Andps | Andpd | Pandn | Andnps | Andnpd
                if ins.op_kind(0) == OpKind::Register && ins.op_register(0).is_xmm() && xmm_or_m128(ins, 1) =>
            {
                let a = self.mem_addr_sized(16);
                let d = Off::xmm(ins.op_register(0));
                self.c.get(L_CPU);
                if matches!(ins.mnemonic(), Pandn | Andnps | Andnpd) {
                    // ~dst & src = andnot(src, dst)
                    self.xmm_read(1, a);
                    self.c.get(L_CPU).v128load(d).simd(79);
                } else {
                    self.c.get(L_CPU).v128load(d);
                    self.xmm_read(1, a);
                    self.c.simd(match ins.mnemonic() {
                        Pxor | Xorps | Xorpd => 81,
                        Por | Orps | Orpd => 80,
                        _ => 78,
                    });
                }
                self.c.v128store(d);
            }

            // ---- control flow (always ends the block)
            Jmp if ins.op_kind(0) == OpKind::NearBranch32 => {
                self.goto(ins.near_branch32());
                return false;
            }
            Jo | Jno | Jb | Jae | Je | Jne | Jbe | Ja | Js | Jns | Jp | Jnp | Jl | Jge | Jle | Jg if ins.op_kind(0) == OpKind::NearBranch32 => {
                self.cond(cc_index(ins.condition_code()));
                self.c.if_();
                self.depth += 1;
                self.goto(ins.near_branch32());
                self.depth -= 1;
                self.c.end();
                self.goto(next);
                return false;
            }
            Call if ins.op_kind(0) == OpKind::NearBranch32 => {
                let ret = self.temp();
                self.c.u32c(next).set(ret);
                self.push32(ret);
                self.goto(ins.near_branch32());
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
                self.jump_dyn();
                return false;
            }
            Jmp if simple && op_bits(ins, 0) == 32 => {
                let addr = self.mem_addr();
                self.read_op(0, addr);
                self.jump_dyn();
                return false;
            }
            Ret => {
                let t = self.pop32();
                if ins.op_count() == 1 {
                    let n = ins.immediate16() as i32;
                    self.c.get(L_REG + 4).i32c(n).add().set(L_REG + 4);
                }
                // most returns go back to a call in the same region
                for &r in self.returns {
                    self.c.get(t).u32c(r).eq().if_();
                    self.depth += 1;
                    self.goto(r);
                    self.depth -= 1;
                    self.c.end();
                }
                self.c.get(t);
                self.jump_dyn();
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
        self.after(last, next)
    }

    /// End of an instruction that does not branch: the block continues, or at its last
    /// instruction falls through to the next address.
    fn after(&mut self, last: bool, next: u32) -> bool {
        if last {
            self.goto(next);
        }
        true
    }

    /// Jumps to a static target: the block inside the region (polling the budget and signals
    /// on backward jumps), or out to the dispatcher.
    fn goto(&mut self, target: u32) {
        let Some(&k) = self.targets.get(&target) else {
            // maybe another region's
            self.c.u32c(target);
            self.jump_dyn();
            return;
        };
        if target <= self.eip() {
            // (pending signals are noticed when the budget runs out)
            self.c.get(L_BUD).i32c(1).sub().tee(L_BUD).i32c(0).le_s();
            self.c.if_();
            self.depth += 1;
            self.c.u32c(target);
            self.exit_with(RET_CONTINUE);
            self.depth -= 1;
            self.c.end();
        }
        if target == self.block_start {
            self.c.br(self.depth - self.block_depth);
        } else if k > self.block_index {
            // a later block starts right after the end of its wasm block
            self.c.br(self.depth - self.block_depth + k - self.block_index);
        } else {
            self.c.i32c(k as i32).set(L_BLK).br(self.depth - 1);
        }
    }

    /// Jumps to the address on the stack (looked up by the region's shared code).
    fn jump_dyn(&mut self) {
        self.c.set(L_NEXT);
        self.c.br(self.depth - 2);
    }
}

/// Stores the lazy operation in the locals to the Cpu (L_OP holds it).
fn lazy_store(c: &mut Code) {
    let lo = Off::lazy();
    c.get(L_CPU).get(L_OP).store32(lo);
    c.get(L_CPU).get(L_FA).store32(lo + 4);
    c.get(L_CPU).get(L_FB).store32(lo + 8);
    c.get(L_CPU).get(L_FR).store32(lo + 12);
    c.get(L_CPU).get(L_FC).store32(lo + 16);
}

/// Opens `if L_OP != -1` and stores the lazy operation; the caller closes the `if`.
fn lazy_to_cpu(c: &mut Code) {
    c.get(L_OP).i32c(-1).ne().if_();
    lazy_store(c);
}

/// Writes registers, EIP (L_NEXT), the budget and lazy flags back to the Cpu.
fn writeback(c: &mut Code) {
    for i in 0..8 {
        c.get(L_CPU).get(L_REG + i).store32(Off::reg(i as usize));
    }
    c.get(L_CPU).get(L_NEXT).store32(Off::eip());
    c.get(L_CPU).get(L_BUD).store32(Off::budget());
    lazy_to_cpu(c);
    c.end();
}

/// Code after the blocks (inside the top loop): finds L_NEXT in the hash table and continues
/// there if the block is in this function, else leaves with RET_CONTINUE.
fn emit_lookup(c: &mut Code) {
    let hash = offset_of!(Cpu, jit_hash) as u32;
    let maskf = offset_of!(Cpu, jit_mask) as u32;
    let (h, e, v) = (L_T, L_T + 1, L_T + 2);
    // depth here: top 0, exit 1
    // pending signals are noticed when the budget runs out (the dispatcher checks them)
    c.get(L_BUD).i32c(1).sub().tee(L_BUD).i32c(0).le_s();
    c.if_().i32c(RET_CONTINUE).set(L_ST).br(2).end();
    c.get(L_NEXT).u32c(0x9e37_79b1).mul().i32c(15).shr_u().get(L_CPU).load32(maskf).and().set(h);
    c.block(); // miss: miss 0, top 1, exit 2
    c.loop_(); // probe 0, miss 1, top 2, exit 3
    c.get(L_CPU).load32(hash).get(h).i32c(3).shl().add().tee(e).load32(0).set(v);
    c.get(v).get(L_NEXT).eq().if_(); // if 0, probe 1, miss 2, top 3
    c.get(e).load32(4).tee(v).u32c((1 << ENTRY_SHIFT) - 1).and();
    c.get(L_ARG).u32c((1 << ENTRY_SHIFT) - 1).and().ne().if_();
    // another region's block: hand over directly (a tail call with the state as arguments,
    // no dispatcher round trip)
    c.get(L_CPU).get(v);
    for l in L_REG..N_PARAMS {
        c.get(l);
    }
    c.get(v).u32c((1 << ENTRY_SHIFT) - 1).and();
    c.op(0x13).op(2).op(0); // return_call_indirect table 0
    c.end();
    c.get(v).i32c(ENTRY_SHIFT as i32).shr_u().set(L_BLK).br(3);
    c.end();
    c.get(v).eqz().br_if(1);
    c.get(h).i32c(1).add().get(L_CPU).load32(maskf).and().set(h).br(0);
    c.end();
    c.end();
    c.i32c(RET_CONTINUE).set(L_ST).br(1);
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

fn successors(last: &Instruction) -> Vec<u32> {
    let mut next = Vec::new();
    let direct = matches!(last.op0_kind(), OpKind::NearBranch32);
    match last.flow_control() {
        FlowControl::ConditionalBranch | FlowControl::Call if direct => {
            next.push(last.near_branch32());
            next.push(last.next_ip32());
        }
        FlowControl::UnconditionalBranch if direct => next.push(last.near_branch32()),
        FlowControl::Next | FlowControl::IndirectCall | FlowControl::Call | FlowControl::ConditionalBranch => {
            next.push(last.next_ip32())
        }
        _ => {}
    }
    next
}

/// Liveness of the status flags after each instruction (everything is live at the end).
fn liveness(list: &[Instruction]) -> Vec<u32> {
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
    live
}

/// Adds a region starting at `entry` to `module` (one function, exported as "f<n>"),
/// taking at most `max_blocks` blocks whose start `accept` agrees to. None if not even the
/// entry block can be translated.
pub fn translate_region<M: Memory>(
    module: &mut Module,
    mem: &mut M,
    entry: u32,
    max_blocks: usize,
    mut accept: impl FnMut(u32) -> bool,
) -> Option<Region> {
    let mut blocks: Vec<(u32, Vec<Instruction>)> = Vec::new();
    let mut targets = BTreeMap::new();
    let mut queue = VecDeque::from([entry]);
    let mut total = 0;
    while let Some(a) = queue.pop_front() {
        if blocks.len() >= max_blocks.min(MAX_BLOCKS) || total >= MAX_REGION_INS {
            break;
        }
        if targets.contains_key(&a) || !(GUEST_LO..GUEST_HI).contains(&a) || !accept(a) {
            continue;
        }
        let list = decode(mem, a);
        let Some(last) = list.last() else { continue };
        queue.extend(successors(last));
        total += list.len();
        targets.insert(a, blocks.len() as u32);
        blocks.push((a, list));
    }
    if blocks.is_empty() {
        return None;
    }
    if module.types.is_empty() {
        module.types.push(FuncType { params: alloc::vec![I32], results: alloc::vec![I32] });
        module.types.push(FuncType { params: alloc::vec![I32; 5], results: alloc::vec![I32] });
        module.types.push(FuncType { params: alloc::vec![I32; N_PARAMS as usize], results: alloc::vec![I32] });
        module.imports.push(Import::Memory);
        module.imports.push(Import::Func("step", 0));
        module.imports.push(Import::Func("eflags", 0));
        module.imports.push(Import::Func("status", 1));
        module.imports.push(Import::Table("table"));
    }

    let n = blocks.len() as u32;
    let mut returns: Vec<u32> = blocks
        .iter()
        .filter_map(|(_, l)| l.last())
        .filter(|l| l.flow_control() == FlowControl::Call || l.flow_control() == FlowControl::IndirectCall)
        .map(|l| l.next_ip32())
        .filter(|a| targets.contains_key(a))
        .collect();
    returns.sort_unstable();
    returns.dedup();
    if returns.len() > 4 {
        returns.clear();
    }
    let mut g = Gen { c: Code::default(), depth: 0, flags: Flags::Entry, targets: &targets, block_start: 0, block_depth: 0, block_index: 0, returns: &returns, ins: &blocks[0].1[0], live_after: 0, temps: 0, safe: [None; 8] };
    g.c.get(L_ARG).i32c(ENTRY_SHIFT as i32).shr_u().set(L_BLK);
    g.c.block(); // exit
    g.c.loop_(); // top
    g.c.block(); // dyn: returns and indirect jumps
    for _ in 0..n {
        g.c.block();
    }
    g.c.get(L_BLK).op(0x0e);
    wasm::uleb(&mut g.c.b, n as u64);
    for i in 0..n {
        wasm::uleb(&mut g.c.b, i as u64);
    }
    wasm::uleb(&mut g.c.b, n as u64 - 1);
    let mut count = 0;
    for (k, (start, list)) in blocks.iter().enumerate() {
        g.c.end();
        g.c.loop_(); // jumps to the block's own start
        g.depth = n + 2 - k as u32;
        g.block_start = *start;
        g.block_index = k as u32;
        g.safe = [None; 8];
        g.block_depth = g.depth;
        g.flags = Flags::Entry;
        let live = liveness(list);
        let m = list.len();
        for (i, ins) in list.iter().enumerate() {
            g.ins = ins;
            g.live_after = live[i];
            count += 1;
            if !g.emit(i + 1 == m) {
                break;
            }
        }
        g.c.end();
    }
    g.c.end(); // dyn
    emit_lookup(&mut g.c);
    g.c.end(); // top
    g.c.end(); // exit
    writeback(&mut g.c);
    g.c.get(L_ST);

    let mut locals = alloc::vec![I32; N_LOCALS_I32 as usize];
    locals.extend_from_slice(&[I64, F64, F64]);
    module.funcs.push(Func { ty: 2, locals, code: g.c });
    Some(Region { starts: blocks.into_iter().map(|b| b.0).collect(), instructions: count })
}
