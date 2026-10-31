//! MMX, SSE and SSE2.

use super::X;
use crate::flags::STATUS;
use crate::{fpu, Exit, Fault, Memory};
use iced_x86::{Mnemonic, OpKind, Register};

macro_rules! tryf {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(f) => return Exit::Fault(f),
        }
    };
}

/// Apply `f` to each `lane`-bit element of a and b.
#[inline]
fn lanes(a: u128, b: u128, lane: u32, f: impl Fn(u64, u64) -> u64) -> u128 {
    let m: u128 = if lane == 64 { u64::MAX as u128 } else { (1u128 << lane) - 1 };
    let mut r = 0u128;
    let mut s = 0;
    while s < 128 {
        let x = ((a >> s) & m) as u64;
        let y = ((b >> s) & m) as u64;
        r |= ((f(x, y) as u128) & m) << s;
        s += lane;
    }
    r
}

#[inline]
fn sx(v: u64, bits: u32) -> i64 {
    ((v << (64 - bits)) as i64) >> (64 - bits)
}

fn sat_s(v: i64, bits: u32) -> u64 {
    let lo = -(1i64 << (bits - 1));
    let hi = (1i64 << (bits - 1)) - 1;
    v.clamp(lo, hi) as u64
}

fn sat_u(v: i64, bits: u32) -> u64 {
    v.clamp(0, (1i64 << bits) - 1) as u64
}

#[inline]
fn f32x(v: u128, i: u32) -> f32 {
    f32::from_bits((v >> (32 * i)) as u32)
}
#[inline]
fn f64x(v: u128, i: u32) -> f64 {
    f64::from_bits((v >> (64 * i)) as u64)
}
#[inline]
fn set32(v: u128, i: u32, x: u32) -> u128 {
    (v & !((u32::MAX as u128) << (32 * i))) | (x as u128) << (32 * i)
}
#[inline]
fn set64(v: u128, i: u32, x: u64) -> u128 {
    (v & !((u64::MAX as u128) << (64 * i))) | (x as u128) << (64 * i)
}

/// Interleave the low (or high) halves of a and b in `lane`-bit units,
/// over a vector of `width` bits.
fn unpack(a: u128, b: u128, lane: u32, width: u32, high: bool) -> u128 {
    let m: u128 = if lane == 64 { u64::MAX as u128 } else { (1u128 << lane) - 1 };
    let n = width / lane / 2;
    let base = if high { n * lane } else { 0 };
    let mut r = 0u128;
    for i in 0..n {
        let x = (a >> (base + i * lane)) & m;
        let y = (b >> (base + i * lane)) & m;
        r |= x << (2 * i * lane) | y << ((2 * i + 1) * lane);
    }
    r
}

fn cmp_pred(a: f64, b: f64, p: u8) -> bool {
    let un = a.is_nan() || b.is_nan();
    match p & 7 {
        0 => a == b,
        1 => a < b,
        2 => a <= b,
        3 => un,
        4 => !(a == b),
        5 => !(a < b),
        6 => !(a <= b),
        _ => !un,
    }
}

fn sse_min(a: f64, b: f64) -> bool {
    a < b // true: take a, else b (NaN or equal -> second operand)
}

impl<'a, M: Memory> X<'a, M> {
    fn vbits(&self, i: u32) -> u32 {
        match self.ins.op_kind(i) {
            OpKind::Register => {
                let r = self.ins.op_register(i);
                if r.is_xmm() { 128 } else if r.is_mm() { 64 } else { 32 }
            }
            OpKind::Memory => self.ins.memory_size().size() as u32 * 8,
            _ => 8,
        }
    }

    fn read_v(&mut self, i: u32) -> Result<u128, Fault> {
        match self.ins.op_kind(i) {
            OpKind::Register => {
                let r = self.ins.op_register(i);
                Ok(if r.is_xmm() {
                    self.cpu.xmm[r as usize - Register::XMM0 as usize]
                } else if r.is_mm() {
                    self.cpu.mm[r as usize - Register::MM0 as usize] as u128
                } else {
                    self.get_reg(r) as u128
                })
            }
            OpKind::Memory => {
                let a = self.addr();
                Ok(match self.ins.memory_size().size() {
                    1 => self.mem.read8(a)? as u128,
                    2 => self.mem.read16(a)? as u128,
                    4 => self.mem.read32(a)? as u128,
                    8 => self.mem.read64(a)? as u128,
                    _ => self.mem.read64(a)? as u128 | (self.mem.read64(a.wrapping_add(8))? as u128) << 64,
                })
            }
            _ => Ok(self.ins.immediate(i) as u128),
        }
    }

    fn write_v(&mut self, i: u32, v: u128) -> Result<(), Fault> {
        match self.ins.op_kind(i) {
            OpKind::Register => {
                let r = self.ins.op_register(i);
                if r.is_xmm() {
                    self.cpu.xmm[r as usize - Register::XMM0 as usize] = v;
                } else if r.is_mm() {
                    self.cpu.mm[r as usize - Register::MM0 as usize] = v as u64;
                } else {
                    self.set_reg(r, v as u32);
                }
                Ok(())
            }
            _ => {
                let a = self.addr();
                match self.ins.memory_size().size() {
                    4 => self.mem.write32(a, v as u32),
                    8 => self.mem.write64(a, v as u64),
                    _ => {
                        self.mem.write64(a, v as u64)?;
                        self.mem.write64(a.wrapping_add(8), (v >> 64) as u64)
                    }
                }
            }
        }
    }

    /// Merge the low `bits` of v into operand 0 (MOVSS/MOVSD/MOVLPS register forms).
    fn write_low(&mut self, v: u128, bits: u32) -> Result<(), Fault> {
        if self.ins.op_kind(0) == OpKind::Memory {
            return self.write_v(0, v);
        }
        let old = self.read_v(0)?;
        let m = (1u128 << bits) - 1;
        self.write_v(0, (old & !m) | (v & m))
    }

    fn round_mx(&self, v: f64) -> f64 {
        match (self.cpu.mxcsr >> 13) & 3 {
            0 => fpu::libm_round_even(v),
            1 => fpu::floor(v),
            2 => fpu::ceil(v),
            _ => fpu::trunc(v),
        }
    }

    fn to_i32(&self, v: f64, trunc: bool) -> u32 {
        let r = if trunc { fpu::trunc(v) } else { self.round_mx(v) };
        if r >= -2147483648.0 && r <= 2147483647.0 { r as i32 as u32 } else { 0x8000_0000 }
    }

    pub(super) fn sse(&mut self) -> Exit {
        use Mnemonic::*;
        let ins = self.ins;
        let mn = ins.mnemonic();
        let w = if ins.op_count() > 0 { self.vbits(0) } else { 128 };
        let w = if w == 32 { 128 } else { w }; // gpr destination: lane math on xmm source
        let wmask: u128 = if w == 64 { u64::MAX as u128 } else { u128::MAX };
        // two-operand integer/logic ops: dst = f(dst, src)
        let binop = |x: &mut Self, f: &dyn Fn(u128, u128) -> u128| -> Exit {
            let a = tryf!(x.read_v(0));
            let b = tryf!(x.read_v(1));
            tryf!(x.write_v(0, f(a, b) & wmask));
            x.done()
        };
        match mn {
            Emms | Femms => {
                self.cpu.fpu.empty = 0xff;
                self.done()
            }
            Fxsave | Fxrstor => self.x87(),
            Ldmxcsr => {
                let a = self.addr();
                self.cpu.mxcsr = tryf!(self.mem.read32(a));
                self.done()
            }
            Stmxcsr => {
                let a = self.addr();
                tryf!(self.mem.write32(a, self.cpu.mxcsr));
                self.done()
            }
            Clflush | Clflushopt => self.done(),

            // ---- moves
            Movd => {
                let v = tryf!(self.read_v(1)) as u32 as u128;
                tryf!(self.write_v(0, v));
                self.done()
            }
            Movq | Movq2dq | Movdq2q => {
                let v = tryf!(self.read_v(1)) as u64 as u128;
                tryf!(self.write_v(0, v));
                self.done()
            }
            Movdqa | Movdqu | Movaps | Movups | Movapd | Movupd | Movntdq | Movntps | Movntpd | Movntq | Lddqu
            | Movnti => {
                let v = tryf!(self.read_v(1));
                tryf!(self.write_v(0, v));
                self.done()
            }
            Movss | Movsd => {
                let bits = if mn == Movss { 32 } else { 64 };
                let v = tryf!(self.read_v(1));
                if ins.op_kind(1) == OpKind::Memory {
                    // load: upper part zeroed
                    tryf!(self.write_v(0, v & ((1u128 << bits) - 1)));
                } else {
                    tryf!(self.write_low(v, bits));
                }
                self.done()
            }
            Movlps | Movlpd => {
                let v = tryf!(self.read_v(1));
                tryf!(self.write_low(v, 64));
                self.done()
            }
            Movhps | Movhpd => {
                if ins.op_kind(0) == OpKind::Memory {
                    let v = tryf!(self.read_v(1)) >> 64;
                    tryf!(self.write_v(0, v));
                } else {
                    let a = tryf!(self.read_v(0));
                    let b = tryf!(self.read_v(1)) as u64;
                    tryf!(self.write_v(0, set64(a, 1, b)));
                }
                self.done()
            }
            Movlhps => binop(self, &|a, b| set64(a, 1, b as u64)),
            Movhlps => binop(self, &|a, b| set64(a, 0, (b >> 64) as u64)),

            // ---- logic
            Pand | Andps | Andpd => binop(self, &|a, b| a & b),
            Pandn | Andnps | Andnpd => binop(self, &|a, b| !a & b),
            Por | Orps | Orpd => binop(self, &|a, b| a | b),
            Pxor | Xorps | Xorpd => binop(self, &|a, b| a ^ b),

            // ---- integer arithmetic
            Paddb => binop(self, &|a, b| lanes(a, b, 8, |x, y| x.wrapping_add(y))),
            Paddw => binop(self, &|a, b| lanes(a, b, 16, |x, y| x.wrapping_add(y))),
            Paddd => binop(self, &|a, b| lanes(a, b, 32, |x, y| x.wrapping_add(y))),
            Paddq => binop(self, &|a, b| lanes(a, b, 64, |x, y| x.wrapping_add(y))),
            Psubb => binop(self, &|a, b| lanes(a, b, 8, |x, y| x.wrapping_sub(y))),
            Psubw => binop(self, &|a, b| lanes(a, b, 16, |x, y| x.wrapping_sub(y))),
            Psubd => binop(self, &|a, b| lanes(a, b, 32, |x, y| x.wrapping_sub(y))),
            Psubq => binop(self, &|a, b| lanes(a, b, 64, |x, y| x.wrapping_sub(y))),
            Paddsb => binop(self, &|a, b| lanes(a, b, 8, |x, y| sat_s(sx(x, 8) + sx(y, 8), 8))),
            Paddsw => binop(self, &|a, b| lanes(a, b, 16, |x, y| sat_s(sx(x, 16) + sx(y, 16), 16))),
            Psubsb => binop(self, &|a, b| lanes(a, b, 8, |x, y| sat_s(sx(x, 8) - sx(y, 8), 8))),
            Psubsw => binop(self, &|a, b| lanes(a, b, 16, |x, y| sat_s(sx(x, 16) - sx(y, 16), 16))),
            Paddusb => binop(self, &|a, b| lanes(a, b, 8, |x, y| sat_u(x as i64 + y as i64, 8))),
            Paddusw => binop(self, &|a, b| lanes(a, b, 16, |x, y| sat_u(x as i64 + y as i64, 16))),
            Psubusb => binop(self, &|a, b| lanes(a, b, 8, |x, y| sat_u(x as i64 - y as i64, 8))),
            Psubusw => binop(self, &|a, b| lanes(a, b, 16, |x, y| sat_u(x as i64 - y as i64, 16))),
            Pmullw => binop(self, &|a, b| lanes(a, b, 16, |x, y| x.wrapping_mul(y))),
            Pmulhw => binop(self, &|a, b| lanes(a, b, 16, |x, y| ((sx(x, 16) * sx(y, 16)) >> 16) as u64)),
            Pmulhuw => binop(self, &|a, b| lanes(a, b, 16, |x, y| (x * y) >> 16)),
            Pmuludq => binop(self, &|a, b| lanes(a, b, 64, |x, y| (x as u32 as u64) * (y as u32 as u64))),
            Pmaddwd => binop(self, &|a, b| {
                lanes(a, b, 32, |x, y| {
                    let p0 = sx(x & 0xffff, 16) * sx(y & 0xffff, 16);
                    let p1 = sx(x >> 16, 16) * sx(y >> 16, 16);
                    (p0 + p1) as u64
                })
            }),
            Pavgb => binop(self, &|a, b| lanes(a, b, 8, |x, y| (x + y + 1) >> 1)),
            Pavgw => binop(self, &|a, b| lanes(a, b, 16, |x, y| (x + y + 1) >> 1)),
            Pminub => binop(self, &|a, b| lanes(a, b, 8, |x, y| x.min(y))),
            Pmaxub => binop(self, &|a, b| lanes(a, b, 8, |x, y| x.max(y))),
            Pminsw => binop(self, &|a, b| lanes(a, b, 16, |x, y| if sx(x, 16) < sx(y, 16) { x } else { y })),
            Pmaxsw => binop(self, &|a, b| lanes(a, b, 16, |x, y| if sx(x, 16) > sx(y, 16) { x } else { y })),
            Psadbw => binop(self, &|a, b| {
                lanes(a, b, 64, |x, y| {
                    let mut s = 0u64;
                    for k in 0..8 {
                        let p = (x >> (8 * k)) & 0xff;
                        let q = (y >> (8 * k)) & 0xff;
                        s += p.abs_diff(q);
                    }
                    s
                })
            }),
            Pcmpeqb => binop(self, &|a, b| lanes(a, b, 8, |x, y| if x == y { 0xff } else { 0 })),
            Pcmpeqw => binop(self, &|a, b| lanes(a, b, 16, |x, y| if x == y { 0xffff } else { 0 })),
            Pcmpeqd => binop(self, &|a, b| lanes(a, b, 32, |x, y| if x == y { 0xffff_ffff } else { 0 })),
            Pcmpgtb => binop(self, &|a, b| lanes(a, b, 8, |x, y| if sx(x, 8) > sx(y, 8) { 0xff } else { 0 })),
            Pcmpgtw => binop(self, &|a, b| lanes(a, b, 16, |x, y| if sx(x, 16) > sx(y, 16) { 0xffff } else { 0 })),
            Pcmpgtd => binop(self, &|a, b| lanes(a, b, 32, |x, y| if sx(x, 32) > sx(y, 32) { 0xffff_ffff } else { 0 })),

            // ---- shifts
            Psllw | Pslld | Psllq | Psrlw | Psrld | Psrlq | Psraw | Psrad => {
                let lane = match mn {
                    Psllw | Psrlw | Psraw => 16,
                    Pslld | Psrld | Psrad => 32,
                    _ => 64,
                };
                let a = tryf!(self.read_v(0));
                let c = match ins.op_kind(1) {
                    OpKind::Immediate8 => ins.immediate8() as u64,
                    _ => tryf!(self.read_v(1)) as u64,
                };
                let r = lanes(a, 0, lane, |x, _| match mn {
                    Psllw | Pslld | Psllq => if c >= lane as u64 { 0 } else { x << c },
                    Psrlw | Psrld | Psrlq => if c >= lane as u64 { 0 } else { x >> c },
                    _ => (sx(x, lane) >> c.min(lane as u64 - 1)) as u64,
                });
                tryf!(self.write_v(0, r & wmask));
                self.done()
            }
            Pslldq | Psrldq => {
                let a = tryf!(self.read_v(0));
                let n = (ins.immediate8() as u32).min(16) * 8;
                let r = if n >= 128 { 0 } else if mn == Pslldq { a << n } else { a >> n };
                tryf!(self.write_v(0, r));
                self.done()
            }

            // ---- pack / unpack / shuffle
            Punpcklbw => binop(self, &|a, b| unpack(a, b, 8, w, false)),
            Punpcklwd => binop(self, &|a, b| unpack(a, b, 16, w, false)),
            Punpckldq | Unpcklps => binop(self, &|a, b| unpack(a, b, 32, w, false)),
            Punpcklqdq | Unpcklpd => binop(self, &|a, b| unpack(a, b, 64, w, false)),
            Punpckhbw => binop(self, &|a, b| unpack(a, b, 8, w, true)),
            Punpckhwd => binop(self, &|a, b| unpack(a, b, 16, w, true)),
            Punpckhdq | Unpckhps => binop(self, &|a, b| unpack(a, b, 32, w, true)),
            Punpckhqdq | Unpckhpd => binop(self, &|a, b| unpack(a, b, 64, w, true)),
            Packsswb | Packssdw | Packuswb => {
                let a = tryf!(self.read_v(0));
                let b = tryf!(self.read_v(1));
                let (src, dst) = if mn == Packssdw { (32, 16) } else { (16, 8) };
                let n = w / src; // elements per operand
                let mut r = 0u128;
                for (k, v) in [a, b].iter().enumerate() {
                    for i in 0..n {
                        let e = sx(((v >> (i * src)) as u64) & ((1u64 << src) - 1), src);
                        let s = if mn == Packuswb { sat_u(e, dst) } else { sat_s(e, dst) };
                        r |= ((s as u128) & ((1u128 << dst) - 1)) << ((k as u32 * n + i) * dst);
                    }
                }
                tryf!(self.write_v(0, r & wmask));
                self.done()
            }
            Pshufd | Pshufw | Pshuflw | Pshufhw => {
                let v = tryf!(self.read_v(1));
                let imm = ins.immediate8() as u32;
                let r = match mn {
                    Pshufd => {
                        let mut r = 0u128;
                        for i in 0..4 {
                            r = set32(r, i, (v >> (32 * ((imm >> (2 * i)) & 3))) as u32);
                        }
                        r
                    }
                    _ => {
                        let base = if mn == Pshufhw { 64 } else { 0 };
                        let mut r = v;
                        for i in 0..4u32 {
                            let src = (v >> (base + 16 * ((imm >> (2 * i)) & 3))) as u16 as u128;
                            r = (r & !(0xffffu128 << (base + 16 * i))) | src << (base + 16 * i);
                        }
                        r
                    }
                };
                tryf!(self.write_v(0, r & wmask));
                self.done()
            }
            Shufps => {
                let a = tryf!(self.read_v(0));
                let b = tryf!(self.read_v(1));
                let imm = ins.immediate8() as u32;
                let pick = |v: u128, s: u32| (v >> (32 * (s & 3))) as u32;
                let mut r = 0u128;
                r = set32(r, 0, pick(a, imm));
                r = set32(r, 1, pick(a, imm >> 2));
                r = set32(r, 2, pick(b, imm >> 4));
                r = set32(r, 3, pick(b, imm >> 6));
                tryf!(self.write_v(0, r));
                self.done()
            }
            Shufpd => {
                let a = tryf!(self.read_v(0));
                let b = tryf!(self.read_v(1));
                let imm = ins.immediate8() as u32;
                let r = set64(set64(0, 0, (a >> (64 * (imm & 1))) as u64), 1, (b >> (64 * ((imm >> 1) & 1))) as u64);
                tryf!(self.write_v(0, r));
                self.done()
            }
            Pextrw => {
                let v = tryf!(self.read_v(1));
                let n = (ins.immediate8() as u32) & (if self.vbits(1) == 64 { 3 } else { 7 });
                let r = (v >> (16 * n)) as u16 as u128;
                tryf!(self.write_v(0, r));
                self.done()
            }
            Pinsrw => {
                let a = tryf!(self.read_v(0));
                let v = tryf!(self.read_v(1)) as u16 as u128;
                let n = (ins.immediate8() as u32) & (if w == 64 { 3 } else { 7 });
                let r = (a & !(0xffffu128 << (16 * n))) | v << (16 * n);
                tryf!(self.write_v(0, r));
                self.done()
            }
            Pmovmskb => {
                let v = tryf!(self.read_v(1));
                let n = self.vbits(1) / 8;
                let mut m = 0u32;
                for i in 0..n {
                    m |= (((v >> (8 * i + 7)) & 1) as u32) << i;
                }
                tryf!(self.write_v(0, m as u128));
                self.done()
            }
            Movmskps | Movmskpd => {
                let v = tryf!(self.read_v(1));
                let lane = if mn == Movmskps { 32 } else { 64 };
                let mut m = 0u32;
                for i in 0..128 / lane {
                    m |= (((v >> (lane * i + lane - 1)) & 1) as u32) << i;
                }
                tryf!(self.write_v(0, m as u128));
                self.done()
            }

            // ---- floating point
            Addss | Subss | Mulss | Divss | Minss | Maxss | Sqrtss | Addps | Subps | Mulps | Divps | Minps | Maxps
            | Sqrtps | Rcpps | Rcpss | Rsqrtps | Rsqrtss => {
                let scalar = matches!(mn, Addss | Subss | Mulss | Divss | Minss | Maxss | Sqrtss | Rcpss | Rsqrtss);
                let a = tryf!(self.read_v(0));
                let b = tryf!(self.read_v(1));
                let mut r = a;
                for i in 0..(if scalar { 1 } else { 4 }) {
                    let (x, y) = (f32x(a, i), f32x(b, i));
                    let v = match mn {
                        Addss | Addps => x + y,
                        Subss | Subps => x - y,
                        Mulss | Mulps => x * y,
                        Divss | Divps => x / y,
                        Minss | Minps => if sse_min(x as f64, y as f64) { x } else { y },
                        Maxss | Maxps => if (x as f64) > (y as f64) { x } else { y },
                        Sqrtss | Sqrtps => if y.is_nan() { f32::from_bits(y.to_bits() | 0x40_0000) } else if y < 0.0 { f32::from_bits(0xffc0_0000) } else { libm::sqrtf(y) },
                        Rcpss | Rcpps => 1.0 / y,
                        _ => 1.0 / libm::sqrtf(y),
                    };
                    r = set32(r, i, v.to_bits());
                }
                tryf!(self.write_v(0, r));
                self.done()
            }
            Addsd | Subsd | Mulsd | Divsd | Minsd | Maxsd | Sqrtsd | Addpd | Subpd | Mulpd | Divpd | Minpd | Maxpd
            | Sqrtpd => {
                let scalar = matches!(mn, Addsd | Subsd | Mulsd | Divsd | Minsd | Maxsd | Sqrtsd);
                let a = tryf!(self.read_v(0));
                let b = tryf!(self.read_v(1));
                let mut r = a;
                for i in 0..(if scalar { 1 } else { 2 }) {
                    let (x, y) = (f64x(a, i), f64x(b, i));
                    let v = match mn {
                        Addsd | Addpd => x + y,
                        Subsd | Subpd => x - y,
                        Mulsd | Mulpd => x * y,
                        Divsd | Divpd => x / y,
                        Minsd | Minpd => if sse_min(x, y) { x } else { y },
                        Maxsd | Maxpd => if x > y { x } else { y },
                        _ => if y.is_nan() { f64::from_bits(y.to_bits() | 0x8_0000_0000_0000) } else if y < 0.0 { f64::from_bits(0xfff8_0000_0000_0000) } else { libm::sqrt(y) },
                    };
                    r = set64(r, i, v.to_bits());
                }
                tryf!(self.write_v(0, r));
                self.done()
            }
            Cmpss | Cmpps | Cmpsd | Cmppd => {
                let a = tryf!(self.read_v(0));
                let b = tryf!(self.read_v(1));
                let p = ins.immediate8();
                let mut r = a;
                match mn {
                    Cmpss | Cmpps => {
                        for i in 0..(if mn == Cmpss { 1 } else { 4 }) {
                            let t = cmp_pred(f32x(a, i) as f64, f32x(b, i) as f64, p);
                            r = set32(r, i, if t { u32::MAX } else { 0 });
                        }
                    }
                    _ => {
                        for i in 0..(if mn == Cmpsd { 1 } else { 2 }) {
                            let t = cmp_pred(f64x(a, i), f64x(b, i), p);
                            r = set64(r, i, if t { u64::MAX } else { 0 });
                        }
                    }
                }
                tryf!(self.write_v(0, r));
                self.done()
            }
            Comiss | Ucomiss | Comisd | Ucomisd => {
                let a = tryf!(self.read_v(0));
                let b = tryf!(self.read_v(1));
                let (x, y) = if matches!(mn, Comiss | Ucomiss) {
                    (f32x(a, 0) as f64, f32x(b, 0) as f64)
                } else {
                    (f64x(a, 0), f64x(b, 0))
                };
                self.cpu.eflags = (self.cpu.eflags & !STATUS) | fpu::compare_eflags(x, y);
                self.done()
            }

            // ---- conversions
            Cvtsi2ss | Cvtsi2sd => {
                let v = tryf!(self.read_v(1)) as u32 as i32;
                let a = tryf!(self.read_v(0));
                let r = if mn == Cvtsi2ss { set32(a, 0, (v as f32).to_bits()) } else { set64(a, 0, (v as f64).to_bits()) };
                tryf!(self.write_v(0, r));
                self.done()
            }
            Cvttss2si | Cvtss2si | Cvttsd2si | Cvtsd2si => {
                let v = tryf!(self.read_v(1));
                let x = if matches!(mn, Cvttss2si | Cvtss2si) { f32x(v, 0) as f64 } else { f64x(v, 0) };
                let r = self.to_i32(x, matches!(mn, Cvttss2si | Cvttsd2si));
                tryf!(self.write_v(0, r as u128));
                self.done()
            }
            Cvtss2sd => {
                let a = tryf!(self.read_v(0));
                let v = tryf!(self.read_v(1));
                tryf!(self.write_v(0, set64(a, 0, (f32x(v, 0) as f64).to_bits())));
                self.done()
            }
            Cvtsd2ss => {
                let a = tryf!(self.read_v(0));
                let v = tryf!(self.read_v(1));
                tryf!(self.write_v(0, set32(a, 0, (f64x(v, 0) as f32).to_bits())));
                self.done()
            }
            Cvtdq2ps => {
                let v = tryf!(self.read_v(1));
                let r = lanes(v, 0, 32, |x, _| (x as u32 as i32 as f32).to_bits() as u64);
                tryf!(self.write_v(0, r));
                self.done()
            }
            Cvtps2dq | Cvttps2dq => {
                let v = tryf!(self.read_v(1));
                let mut r = 0u128;
                for i in 0..4 {
                    r = set32(r, i, self.to_i32(f32x(v, i) as f64, mn == Cvttps2dq));
                }
                tryf!(self.write_v(0, r));
                self.done()
            }
            Cvtdq2pd | Cvtpi2pd => {
                let v = tryf!(self.read_v(1));
                let r = set64(set64(0, 0, ((v as u32 as i32) as f64).to_bits()), 1, (((v >> 32) as u32 as i32) as f64).to_bits());
                tryf!(self.write_v(0, r));
                self.done()
            }
            Cvtpd2dq | Cvttpd2dq | Cvtpd2pi | Cvttpd2pi => {
                let v = tryf!(self.read_v(1));
                let t = matches!(mn, Cvttpd2dq | Cvttpd2pi);
                let r = set32(set32(0, 0, self.to_i32(f64x(v, 0), t)), 1, self.to_i32(f64x(v, 1), t));
                tryf!(self.write_v(0, r));
                self.done()
            }
            Cvtps2pd => {
                let v = tryf!(self.read_v(1));
                let r = set64(set64(0, 0, (f32x(v, 0) as f64).to_bits()), 1, (f32x(v, 1) as f64).to_bits());
                tryf!(self.write_v(0, r));
                self.done()
            }
            Cvtpd2ps => {
                let v = tryf!(self.read_v(1));
                let r = set32(set32(0, 0, (f64x(v, 0) as f32).to_bits()), 1, (f64x(v, 1) as f32).to_bits());
                tryf!(self.write_v(0, r));
                self.done()
            }
            Cvtpi2ps => {
                let a = tryf!(self.read_v(0));
                let v = tryf!(self.read_v(1));
                let r = set32(set32(a, 0, ((v as u32 as i32) as f32).to_bits()), 1, (((v >> 32) as u32 as i32) as f32).to_bits());
                tryf!(self.write_v(0, r));
                self.done()
            }
            Cvtps2pi | Cvttps2pi => {
                let v = tryf!(self.read_v(1));
                let t = mn == Cvttps2pi;
                let r = set32(set32(0, 0, self.to_i32(f32x(v, 0) as f64, t)), 1, self.to_i32(f32x(v, 1) as f64, t));
                tryf!(self.write_v(0, r));
                self.done()
            }
            _ => Exit::InvalidOpcode,
        }
    }
}
