//! MMX and SSE2 integer instructions as wasm SIMD. An MMX register is the low half of a v128
//! (loaded with v128.load64_zero, stored as its low i64): lane-wise operations give the same
//! low half either way; packs, unpacks and shuffles pick their lanes for the 8-byte width.

use super::{Gen, Off, L_CPU, L_MMX, L_V0, L_V1};
use iced_x86::{Instruction, Mnemonic, OpKind, Register};

/// Lane-wise binary operations: the wasm SIMD opcode.
fn binop(m: Mnemonic) -> Option<u32> {
    use Mnemonic::*;
    Some(match m {
        Paddb => 0x6e,
        Paddsb => 0x6f,
        Paddusb => 0x70,
        Psubb => 0x71,
        Psubsb => 0x72,
        Psubusb => 0x73,
        Pminub => 0x77,
        Pmaxub => 0x79,
        Pavgb => 0x7b,
        Paddw => 0x8e,
        Paddsw => 0x8f,
        Paddusw => 0x90,
        Psubw => 0x91,
        Psubsw => 0x92,
        Psubusw => 0x93,
        Pmullw => 0x95,
        Pminsw => 0x96,
        Pmaxsw => 0x98,
        Pavgw => 0x9b,
        Paddd => 0xae,
        Psubd => 0xb1,
        Pmaddwd => 0xba,
        Paddq => 0xce,
        Psubq => 0xd1,
        Pand => 0x4e,
        Por => 0x50,
        Pxor => 0x51,
        Pcmpeqb => 0x23,
        Pcmpgtb => 0x27,
        Pcmpeqw => 0x2d,
        Pcmpgtw => 0x31,
        Pcmpeqd => 0x37,
        Pcmpgtd => 0x3b,
        _ => return None,
    })
}

/// Shifts: (lane bits, shl/shr_s/shr_u opcode, arithmetic).
fn shift(m: Mnemonic) -> Option<(u32, u32, bool)> {
    use Mnemonic::*;
    Some(match m {
        Psllw => (16, 0x8b, false),
        Psraw => (16, 0x8c, true),
        Psrlw => (16, 0x8d, false),
        Pslld => (32, 0xab, false),
        Psrad => (32, 0xac, true),
        Psrld => (32, 0xad, false),
        Psllq => (64, 0xcb, false),
        Psrlq => (64, 0xcd, false),
        _ => return None,
    })
}

/// Unpacks: (element bytes, high half).
fn unpack(m: Mnemonic) -> Option<(usize, bool)> {
    use Mnemonic::*;
    Some(match m {
        Punpcklbw => (1, false),
        Punpcklwd => (2, false),
        Punpckldq => (4, false),
        Punpcklqdq => (8, false),
        Punpckhbw => (1, true),
        Punpckhwd => (2, true),
        Punpckhdq => (4, true),
        Punpckhqdq => (8, true),
        Unpcklps => (4, false),
        Unpckhps => (4, true),
        Unpcklpd => (8, false),
        Unpckhpd => (8, true),
        _ => return None,
    })
}

/// SSE floating point arithmetic: (lane bits, packed, wasm vector opcode, scalar opcode).
/// MIN/MAX are not IEEE min/max: the second operand unless the first is less (greater).
fn farith(m: Mnemonic) -> Option<(u32, bool, u32, u32)> {
    use Mnemonic::*;
    Some(match m {
        Addps => (32, true, 0xe4, 0),
        Subps => (32, true, 0xe5, 0),
        Mulps => (32, true, 0xe6, 0),
        Divps => (32, true, 0xe7, 0),
        Sqrtps => (32, true, 0xe3, 0),
        Addpd => (64, true, 0xf0, 0),
        Subpd => (64, true, 0xf1, 0),
        Mulpd => (64, true, 0xf2, 0),
        Divpd => (64, true, 0xf3, 0),
        Sqrtpd => (64, true, 0xef, 0),
        Addss => (32, false, 0, 0x92),
        Subss => (32, false, 0, 0x93),
        Mulss => (32, false, 0, 0x94),
        Divss => (32, false, 0, 0x95),
        Sqrtss => (32, false, 0, 0x91),
        Addsd => (64, false, 0, 0xa0),
        Subsd => (64, false, 0, 0xa1),
        Mulsd => (64, false, 0, 0xa2),
        Divsd => (64, false, 0, 0xa3),
        Sqrtsd => (64, false, 0, 0x9f),
        _ => return None,
    })
}

fn vreg(r: Register) -> bool {
    r.is_mm() || r.is_xmm()
}

/// Whether [`Gen::packed`] translates `ins`.
pub(super) fn supported(ins: &Instruction) -> bool {
    use Mnemonic::*;
    let m = ins.mnemonic();
    if ins.op_count() == 0 || ins.op_kind(0) != OpKind::Register {
        let from = ins.op_count() == 2 && ins.op_kind(1) == OpKind::Register && super::mem_ok(ins);
        return (matches!(m, Movd | Movq) && from && vreg(ins.op_register(1))) || (m == Movss && from && ins.op_register(1).is_xmm());
    }
    let d = ins.op_register(0);
    let width = if d.is_mm() { 8 } else { 16 };
    // a register of the same kind, or memory of the instruction's width
    let src = |i: u32, size: usize| match ins.op_kind(i) {
        OpKind::Register => {
            let r = ins.op_register(i);
            r.is_mm() == d.is_mm() && vreg(r)
        }
        OpKind::Memory => super::mem_ok(ins) && ins.memory_size().size() == size,
        _ => false,
    };
    match m {
        Movd => {
            let s = ins.op_kind(1) == OpKind::Register;
            (vreg(d) && ((s && ins.op_register(1).is_gpr32()) || (!s && super::mem_ok(ins))))
                || (d.is_gpr32() && s && vreg(ins.op_register(1)))
        }
        Movq => vreg(d) && src(1, 8),
        Pmovmskb => d.is_gpr32() && ins.op_kind(1) == OpKind::Register && vreg(ins.op_register(1)),
        _ if farith(m).is_some() => {
            let (bits, packed, ..) = farith(m).unwrap();
            d.is_xmm() && src(1, if packed { 16 } else { bits as usize / 8 })
        }
        Minps | Maxps | Minpd | Maxpd | Shufps | Cmpps => d.is_xmm() && src(1, 16),
        Movss => d.is_xmm() && src(1, 4),
        Movhlps | Movlhps => d.is_xmm() && ins.op_kind(1) == OpKind::Register && ins.op_register(1).is_xmm(),
        _ if !vreg(d) || ins.op_count() < 2 => false,
        Pandn | Pmulhw | Pmulhuw | Packsswb | Packuswb | Packssdw => ins.op_count() == 2 && src(1, width),
        _ if binop(m).is_some() || unpack(m).is_some() => ins.op_count() == 2 && src(1, width),
        _ if shift(m).is_some() => ins.op_count() == 2 && (ins.op_kind(1) == OpKind::Immediate8 || src(1, width)),
        Pshufw => d.is_mm() && ins.op_count() == 3 && src(1, 8),
        Pshufd => d.is_xmm() && ins.op_count() == 3 && src(1, 16),
        Pslldq | Psrldq => d.is_xmm() && ins.op_kind(1) == OpKind::Immediate8,
        _ => false,
    }
}

impl Gen<'_> {
    fn voff(r: Register) -> u32 {
        if r.is_mm() {
            Off::mm(r)
        } else {
            Off::xmm(r)
        }
    }

    /// The local holding MMX register `r`, loaded from the Cpu the first time in the block.
    fn mm_get(&mut self, r: Register) -> u32 {
        let i = r as u32 - Register::MM0 as u32;
        if self.mmx.0 & (1 << i) == 0 {
            self.c.get(L_CPU).v128load64(Off::mm(r)).set(L_MMX + i);
            self.mmx.0 |= 1 << i;
        }
        L_MMX + i
    }

    /// Sets MMX register `r` from the v128 on the stack (only its low half counts: every use of
    /// an MMX register reads lanes that come from the low halves of its operands).
    fn mm_set(&mut self, r: Register) {
        let i = r as u32 - Register::MM0 as u32;
        self.c.set(L_MMX + i);
        self.mmx.0 |= 1 << i;
        self.mmx.1 |= 1 << i;
    }

    /// Pushes operand `i` (register, or memory at `a`) as a v128; `mm` says the width.
    fn vread(&mut self, i: u32, a: Option<u32>, mm: bool) {
        if self.ins.op_kind(i) == OpKind::Register && self.ins.op_register(i).is_mm() {
            let l = self.mm_get(self.ins.op_register(i));
            self.c.get(l);
            return;
        }
        let (base, off) = match self.ins.op_kind(i) {
            OpKind::Memory => (a.unwrap(), 0),
            _ => (L_CPU, Self::voff(self.ins.op_register(i))),
        };
        self.c.get(base);
        if mm {
            self.c.v128load64(off);
        } else {
            self.c.v128load(off);
        }
    }

    /// Stores the v128 on the stack (the Cpu below it) to register `r`.
    fn vstore(&mut self, r: Register) {
        if r.is_mm() {
            self.mm_set(r);
            self.c.drop_();
        } else {
            self.c.v128store(Off::xmm(r));
        }
    }

    /// Translates an instruction [`supported`] says it can.
    pub(super) fn packed(&mut self) {
        use Mnemonic::*;
        let ins = self.ins;
        let m = ins.mnemonic();
        if matches!(m, Movd | Movq) {
            return self.movdq();
        }
        if m == Pmovmskb {
            let t = self.temp();
            self.vread(1, None, ins.op_register(1).is_mm());
            self.c.simd(0x64); // i8x16.bitmask
            if ins.op_register(1).is_mm() {
                self.c.i32c(0xff).and();
            }
            self.c.set(t);
            return self.set_reg_from(ins.op_register(0), t);
        }
        if m == Movss || farith(m).is_some_and(|f| !f.1) {
            return self.scalar();
        }
        let d = ins.op_register(0);
        let mm = d.is_mm();
        let a = self.mem_addr_sized(if mm { 8 } else { 16 });
        self.c.get(L_CPU);
        if let Some(op) = binop(m) {
            self.vread(0, None, mm);
            self.vread(1, a, mm);
            self.c.simd(op);
        } else if let Some((_, _, op, _)) = farith(m) {
            if !matches!(m, Sqrtps | Sqrtpd) {
                self.vread(0, None, false);
            }
            self.vread(1, a, false);
            self.c.simd(op);
        } else if matches!(m, Minps | Maxps | Minpd | Maxpd) {
            // the first operand where it is less (greater), else the second
            let cmp = match m {
                Minps => 0x43, // f32x4.lt
                Maxps => 0x44, // f32x4.gt
                Minpd => 0x49, // f64x2.lt
                _ => 0x4a,     // f64x2.gt
            };
            self.vread(0, None, false);
            self.c.set(L_V0);
            self.vread(1, a, false);
            self.c.set(L_V1);
            self.c.get(L_V0).get(L_V1).get(L_V0).get(L_V1).simd(cmp).simd(0x52); // v128.bitselect
        } else if m == Cmpps {
            self.vread(0, None, false);
            self.c.set(L_V0);
            self.vread(1, a, false);
            self.c.set(L_V1);
            let (x, y) = (L_V0, L_V1);
            match ins.immediate8() & 7 {
                0 => self.c.get(x).get(y).simd(0x41),
                1 => self.c.get(x).get(y).simd(0x43),
                2 => self.c.get(x).get(y).simd(0x45),
                3 => self.c.get(x).get(x).simd(0x42).get(y).get(y).simd(0x42).simd(0x50),
                4 => self.c.get(x).get(y).simd(0x42),
                5 => self.c.get(x).get(y).simd(0x43).simd(0x4d),
                6 => self.c.get(x).get(y).simd(0x45).simd(0x4d),
                _ => self.c.get(x).get(x).simd(0x41).get(y).get(y).simd(0x41).simd(0x4e),
            };
        } else if matches!(m, Shufps | Movhlps | Movlhps) {
            // dword lanes: 0..3 of the destination, 4..7 of the source
            let pick: [u8; 4] = match m {
                Shufps => {
                    let imm = ins.immediate8();
                    [imm & 3, (imm >> 2) & 3, 4 + ((imm >> 4) & 3), 4 + (imm >> 6)]
                }
                Movhlps => [6, 7, 2, 3],
                _ => [0, 1, 4, 5],
            };
            let mut l = [0u8; 16];
            for (k, &p) in pick.iter().enumerate() {
                for t in 0..4 {
                    l[k * 4 + t] = p * 4 + t as u8;
                }
            }
            self.vread(0, None, false);
            self.vread(1, a, false);
            self.c.shuffle(l);
        } else if m == Pandn {
            // ~dst & src = andnot(src, dst)
            self.vread(1, a, mm);
            self.vread(0, None, mm);
            self.c.simd(0x4f);
        } else if matches!(m, Pmulhw | Pmulhuw) {
            let (lo, hi, shr, narrow) = if m == Pmulhw { (0xbc, 0xbd, 0xac, 0x85) } else { (0xbe, 0xbf, 0xad, 0x86) };
            self.vread(0, None, mm);
            self.c.set(L_V0);
            self.vread(1, a, mm);
            self.c.set(L_V1);
            self.c.get(L_V0).get(L_V1).simd(lo).i32c(16).simd(shr);
            self.c.get(L_V0).get(L_V1).simd(hi).i32c(16).simd(shr);
            self.c.simd(narrow);
        } else if matches!(m, Packsswb | Packuswb | Packssdw) {
            let op = match m {
                Packsswb => 0x65,
                Packuswb => 0x66,
                _ => 0x85,
            };
            self.vread(0, None, mm);
            self.vread(1, a, mm);
            if mm {
                // both halves into one vector, narrowed with itself
                let mut l = [0u8; 16];
                for (k, x) in l.iter_mut().enumerate() {
                    *x = if k < 8 { k as u8 } else { 8 + k as u8 };
                }
                self.c.shuffle(l).set(L_V0).get(L_V0).get(L_V0);
            }
            self.c.simd(op);
        } else if let Some((e, high)) = unpack(m) {
            let n = if mm { 8 } else { 16 };
            let start = if high { n / 2 } else { 0 };
            let mut l = [0u8; 16];
            for k in 0..16 / (2 * e) {
                for t in 0..e {
                    let at = start + k * e + t;
                    let at = if at < 16 { at } else { 0 };
                    l[2 * k * e + t] = at as u8;
                    l[2 * k * e + e + t] = 16 + at as u8;
                }
            }
            self.vread(0, None, mm);
            self.vread(1, a, mm);
            self.c.shuffle(l);
        } else if let Some((bits, op, arith)) = shift(m) {
            self.vread(0, None, mm);
            if ins.op_kind(1) == OpKind::Immediate8 {
                let n = ins.immediate8() as u32;
                if n < bits {
                    self.c.i32c(n as i32).simd(op);
                } else if arith {
                    self.c.i32c(bits as i32 - 1).simd(op);
                } else {
                    self.c.drop_().v128zero();
                }
            } else {
                // the count is the source's low 64 bits; past the lane width, zeros or sign
                let w = super::L_W;
                if ins.op_kind(1) == OpKind::Register && ins.op_register(1).is_mm() {
                    let l = self.mm_get(ins.op_register(1));
                    self.c.get(l).low64().set(w);
                } else {
                    let (base, off) = match ins.op_kind(1) {
                        OpKind::Memory => (a.unwrap(), 0),
                        _ => (L_CPU, Self::voff(ins.op_register(1))),
                    };
                    self.c.get(base).load64(off).set(w);
                }
                let inside = |c: &mut super::Code| {
                    c.get(w).i64c(bits as i64).op(0x54); // i64.lt_u
                };
                if arith {
                    self.c.get(w).wrap().i32c(bits as i32 - 1);
                    inside(&mut self.c);
                    self.c.op(0x1b); // select
                    self.c.simd(op);
                } else {
                    self.c.get(w).wrap().simd(op);
                    self.c.i32c(0);
                    inside(&mut self.c);
                    self.c.sub().simd(0x0f).simd(0x4e); // and splat(-(count < bits))
                }
            }
        } else if matches!(m, Pshufw | Pshufd) {
            let (e, n) = if m == Pshufw { (2, 4) } else { (4, 4) };
            let imm = ins.immediate8();
            let mut l = [0u8; 16];
            for k in 0..n {
                let j = (imm >> (2 * k)) as usize & 3;
                for t in 0..e {
                    l[k * e + t] = (j * e + t) as u8;
                }
            }
            self.vread(1, a, mm);
            self.c.v128zero().shuffle(l);
        } else {
            // Pslldq / Psrldq: bytes in from zeros
            let n = (ins.immediate8() as usize).min(16);
            let mut l = [16u8; 16];
            for (k, x) in l.iter_mut().enumerate() {
                if m == Pslldq && k >= n {
                    *x = (k - n) as u8;
                } else if m == Psrldq && k + n < 16 {
                    *x = (k + n) as u8;
                }
            }
            self.vread(0, None, mm);
            self.c.v128zero().shuffle(l);
        }
        self.vstore(d);
    }

    /// MOVSS and the scalar SSE arithmetic: lane 0 only, the rest of the destination kept
    /// (MOVSS from memory clears it).
    fn scalar(&mut self) {
        let ins = self.ins;
        let m = ins.mnemonic();
        use Mnemonic::Movss;
        let (bits, op) = farith(m).map_or((32, 0), |f| (f.0, f.3));
        let a = self.mem_addr_sized(bits / 8);
        if ins.op_kind(0) == OpKind::Memory {
            // MOVSS m32, xmm
            self.c.get(a.unwrap()).get(L_CPU).load32(Off::xmm(ins.op_register(1))).store32(0);
            return;
        }
        let d = ins.op_register(0);
        self.c.get(L_CPU);
        if m == Movss && ins.op_kind(1) == OpKind::Memory {
            self.c.get(a.unwrap()).v128load32(0);
            return self.vstore(d);
        }
        let (extract, replace) = if bits == 32 { (0x1f, 0x20) } else { (0x21, 0x22) };
        self.vread(0, None, false);
        if m == Movss {
            self.vread(1, None, false);
            self.c.lane(0x1b, 0).lane(0x1c, 0); // i32x4.extract_lane 0, replace_lane 0
            return self.vstore(d);
        }
        if op != 0x91 && op != 0x9f {
            // (not SQRT: unary on the source)
            self.vread(0, None, false);
            self.c.lane(extract, 0);
        }
        match a {
            Some(a) if bits == 32 => {
                self.c.get(a).v128load32(0);
            }
            Some(a) => {
                self.c.get(a).v128load64(0);
            }
            None => self.vread(1, None, false),
        }
        self.c.lane(extract, 0).op(op as u8).lane(replace, 0);
        self.vstore(d);
    }

    /// MOVD / MOVQ between MMX/XMM registers, general registers and memory.
    fn movdq(&mut self) {
        let ins = self.ins;
        let q = ins.mnemonic() == Mnemonic::Movq;
        let size = if q { 8 } else { 4 };
        let a = self.mem_addr_sized(size);
        if q && ins.op_kind(0) == OpKind::Register && ins.op_register(0).is_mm() && ins.op_kind(1) == OpKind::Register && ins.op_register(1).is_mm() {
            let l = self.mm_get(ins.op_register(1));
            self.c.get(l);
            return self.mm_set(ins.op_register(0));
        }
        if q && ins.op_kind(0) == OpKind::Register && ins.op_register(0).is_mm() && ins.op_kind(1) == OpKind::Memory {
            self.c.get(a.unwrap()).v128load64(0);
            return self.mm_set(ins.op_register(0));
        }
        if ins.op_kind(0) == OpKind::Register && vreg(ins.op_register(0)) {
            // into a vector register: zero-extended
            let d = ins.op_register(0);
            if d.is_mm() {
                self.c.v128zero();
            } else {
                self.c.get(L_CPU);
            }
            if q {
                match ins.op_kind(1) {
                    OpKind::Memory => {
                        self.c.get(a.unwrap()).load64(0);
                    }
                    _ => self.vlow64(ins.op_register(1)),
                };
            } else {
                self.read_op(1, a);
                self.c.i64_extend_u();
            }
            if d.is_mm() {
                self.c.simd(0x1e).b.push(0); // i64x2.replace_lane 0
                let i = d as u32 - Register::MM0 as u32;
                self.c.set(L_MMX + i);
                self.mmx.0 |= 1 << i;
                self.mmx.1 |= 1 << i;
            } else {
                self.c.store64(Off::xmm(d));
                self.c.get(L_CPU).i64c(0).store64(Off::xmm(d) + 8);
            }
            return;
        }
        // out of a vector register
        let r = ins.op_register(1);
        if q {
            self.c.get(a.unwrap());
            self.vlow64(r);
            self.c.store64(0);
        } else {
            let t = self.temp();
            self.vlow64(r);
            self.c.wrap().set(t);
            self.write_op(0, a, t);
        }
    }

    /// Pushes the low 64 bits of vector register `r` as an i64.
    fn vlow64(&mut self, r: Register) {
        if r.is_mm() {
            let l = self.mm_get(r);
            self.c.get(l).low64();
        } else {
            self.c.get(L_CPU).load64(Self::voff(r));
        }
    }
}
