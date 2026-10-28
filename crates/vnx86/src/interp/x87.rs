//! x87 instructions (see crate::fpu for the f64 model).

use super::X;
use crate::flags::{self, *};
use crate::fpu::{self, C0, C1, C2, C3};
use crate::{Exit, Memory, EAX};
use iced_x86::{MemorySize, Mnemonic, OpKind, Register};

macro_rules! tryf {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(f) => return Exit::Fault(f),
        }
    };
}

fn st_index(r: Register) -> u32 {
    r as u32 - Register::ST0 as u32
}

impl<'a, M: Memory> X<'a, M> {
    fn fload(&mut self) -> Result<f64, crate::Fault> {
        let a = self.addr();
        Ok(match self.ins.memory_size() {
            MemorySize::Float32 => f32::from_bits(self.mem.read32(a)?) as f64,
            MemorySize::Float64 => f64::from_bits(self.mem.read64(a)?),
            MemorySize::Float80 => {
                let mut b = [0u8; 10];
                for (i, x) in b.iter_mut().enumerate() {
                    *x = self.mem.read8(a.wrapping_add(i as u32))?;
                }
                fpu::from_f80(&b)
            }
            MemorySize::Int16 => self.mem.read16(a)? as i16 as f64,
            MemorySize::Int32 => self.mem.read32(a)? as i32 as f64,
            MemorySize::Int64 => self.mem.read64(a)? as i64 as f64,
            _ => 0.0,
        })
    }

    fn fstore(&mut self, v: f64) -> Result<(), crate::Fault> {
        let a = self.addr();
        match self.ins.memory_size() {
            MemorySize::Float32 => self.mem.write32(a, (v as f32).to_bits()),
            MemorySize::Float64 => self.mem.write64(a, v.to_bits()),
            _ => {
                let b = fpu::to_f80(v);
                for (i, x) in b.iter().enumerate() {
                    self.mem.write8(a.wrapping_add(i as u32), *x)?;
                }
                Ok(())
            }
        }
    }

    /// (destination ST index, source value) for two-operand arithmetic.
    fn farith_operands(&mut self) -> Result<(u32, f64, bool), crate::Fault> {
        let ins = self.ins;
        if ins.op_count() == 1 && ins.op_kind(0) == OpKind::Memory {
            return Ok((0, self.fload()?, false));
        }
        if ins.op_count() == 2 {
            let d = st_index(ins.op_register(0));
            let s = st_index(ins.op_register(1));
            return Ok((d, self.cpu.fpu.st(s), true));
        }
        Ok((1, self.cpu.fpu.st(0), true))
    }

    pub(super) fn x87(&mut self) -> Exit {
        use Mnemonic::*;
        let ins = self.ins;
        let mn = ins.mnemonic();
        match mn {
            Fld => {
                if ins.op_kind(0) == OpKind::Memory {
                    let v = tryf!(self.fload());
                    self.cpu.fpu.push(v);
                } else {
                    // FLD ST(i) copies the exact integer too
                    let i = st_index(ins.op_register(0));
                    let f = &self.cpu.fpu;
                    let p = ((f.top + i) & 7) as usize;
                    match (f.exact & (1 << p) != 0).then(|| f.ints[p]) {
                        Some(x) => self.cpu.fpu.push_int(x),
                        None => {
                            let v = self.cpu.fpu.st(i);
                            self.cpu.fpu.push(v);
                        }
                    }
                }
            }
            Fild => {
                let a = self.addr();
                let v = match ins.memory_size() {
                    MemorySize::Int16 => tryf!(self.mem.read16(a)) as i16 as i64,
                    MemorySize::Int32 => tryf!(self.mem.read32(a)) as i32 as i64,
                    _ => tryf!(self.mem.read64(a)) as i64,
                };
                self.cpu.fpu.push_int(v);
            }
            Fst | Fstp => {
                let v = self.cpu.fpu.st(0);
                if ins.op_kind(0) == OpKind::Memory {
                    tryf!(self.fstore(v));
                } else {
                    let i = st_index(ins.op_register(0));
                    self.cpu.fpu.set_st(i, v);
                }
                if mn == Fstp {
                    self.cpu.fpu.pop();
                }
            }
            Fist | Fistp | Fisttp => {
                let f = &self.cpu.fpu;
                let v = f.st(0);
                let a = self.addr();
                let exact = f.exact_int();
                let r = if mn == Fisttp { fpu::trunc(v) } else { f.round(v) };
                match ins.memory_size() {
                    MemorySize::Int16 => {
                        let x = if r >= -32768.0 && r <= 32767.0 { r as i16 } else { i16::MIN };
                        tryf!(self.mem.write16(a, x as u16));
                    }
                    MemorySize::Int32 => {
                        let x = if r >= -2147483648.0 && r <= 2147483647.0 { r as i32 } else { i32::MIN };
                        tryf!(self.mem.write32(a, x as u32));
                    }
                    _ => {
                        let x = match exact {
                            Some(e) => e,
                            None if r >= -9.223372036854775808e18 && r < 9.223372036854775808e18 => r as i64,
                            None => i64::MIN,
                        };
                        tryf!(self.mem.write64(a, x as u64));
                    }
                }
                if mn != Fist {
                    self.cpu.fpu.pop();
                }
            }
            Fxch => {
                let i = if ins.op_count() == 0 {
                    1
                } else if ins.op_count() == 2 {
                    st_index(ins.op_register(1)).max(st_index(ins.op_register(0)))
                } else {
                    st_index(ins.op_register(0))
                };
                self.cpu.fpu.swap(i);
            }
            Fld1 => self.cpu.fpu.push(1.0),
            Fldz => self.cpu.fpu.push(0.0),
            Fldpi => self.cpu.fpu.push(core::f64::consts::PI),
            Fldl2e => self.cpu.fpu.push(core::f64::consts::LOG2_E),
            Fldl2t => self.cpu.fpu.push(core::f64::consts::LOG2_10),
            Fldlg2 => self.cpu.fpu.push(core::f64::consts::LOG10_2),
            Fldln2 => self.cpu.fpu.push(core::f64::consts::LN_2),

            Fadd | Faddp | Fsub | Fsubp | Fsubr | Fsubrp | Fmul | Fmulp | Fdiv | Fdivp | Fdivr | Fdivrp => {
                let (d, s, _) = tryf!(self.farith_operands());
                let x = self.cpu.fpu.st(d);
                let r = match mn {
                    Fadd | Faddp => x + s,
                    Fsub | Fsubp => x - s,
                    Fsubr | Fsubrp => s - x,
                    Fmul | Fmulp => x * s,
                    Fdiv | Fdivp => x / s,
                    _ => s / x,
                };
                self.cpu.fpu.set_st(d, r);
                if matches!(mn, Faddp | Fsubp | Fsubrp | Fmulp | Fdivp | Fdivrp) {
                    self.cpu.fpu.pop();
                }
            }
            Fiadd | Fisub | Fisubr | Fimul | Fidiv | Fidivr => {
                let s = tryf!(self.fload());
                let x = self.cpu.fpu.st(0);
                let r = match mn {
                    Fiadd => x + s,
                    Fisub => x - s,
                    Fisubr => s - x,
                    Fimul => x * s,
                    Fidiv => x / s,
                    _ => s / x,
                };
                self.cpu.fpu.set_st(0, r);
            }
            Fchs => {
                let e = self.cpu.fpu.exact_int();
                let v = self.cpu.fpu.st(0);
                self.cpu.fpu.set_st(0, -v);
                if let Some(i) = e {
                    self.cpu.fpu.push_int(i.wrapping_neg());
                    self.cpu.fpu.swap(1);
                    self.cpu.fpu.pop();
                }
            }
            Fabs => {
                let e = self.cpu.fpu.exact_int();
                let v = self.cpu.fpu.st(0);
                self.cpu.fpu.set_st(0, v.abs());
                if let Some(i) = e {
                    self.cpu.fpu.push_int(i.wrapping_abs());
                    self.cpu.fpu.swap(1);
                    self.cpu.fpu.pop();
                }
            }
            Fsqrt => { let v = self.cpu.fpu.st(0); self.cpu.fpu.set_st(0, libm::sqrt(v)); }
            Frndint => {
                if self.cpu.fpu.exact_int().is_none() {
                    let v = self.cpu.fpu.st(0);
                    let r = self.cpu.fpu.round(v);
                    self.cpu.fpu.set_st(0, r);
                }
            }
            Fsin => { let v = self.cpu.fpu.st(0); self.cpu.fpu.set_st(0, libm::sin(v)); self.cpu.fpu.sw &= !C2; }
            Fcos => { let v = self.cpu.fpu.st(0); self.cpu.fpu.set_st(0, libm::cos(v)); self.cpu.fpu.sw &= !C2; }
            Fsincos => {
                let v = self.cpu.fpu.st(0);
                self.cpu.fpu.set_st(0, libm::sin(v));
                self.cpu.fpu.push(libm::cos(v));
                self.cpu.fpu.sw &= !C2;
            }
            Fptan => {
                let v = self.cpu.fpu.st(0);
                self.cpu.fpu.set_st(0, libm::tan(v));
                self.cpu.fpu.push(1.0);
                self.cpu.fpu.sw &= !C2;
            }
            Fpatan => {
                let (x, y) = (self.cpu.fpu.st(0), self.cpu.fpu.st(1));
                self.cpu.fpu.set_st(1, libm::atan2(y, x));
                self.cpu.fpu.pop();
            }
            F2xm1 => { let v = self.cpu.fpu.st(0); self.cpu.fpu.set_st(0, libm::exp2(v) - 1.0); }
            Fyl2x => {
                let (x, y) = (self.cpu.fpu.st(0), self.cpu.fpu.st(1));
                self.cpu.fpu.set_st(1, y * libm::log2(x));
                self.cpu.fpu.pop();
            }
            Fyl2xp1 => {
                let (x, y) = (self.cpu.fpu.st(0), self.cpu.fpu.st(1));
                self.cpu.fpu.set_st(1, y * libm::log2(x + 1.0));
                self.cpu.fpu.pop();
            }
            Fscale => {
                let (x, s) = (self.cpu.fpu.st(0), self.cpu.fpu.st(1));
                self.cpu.fpu.set_st(0, libm::ldexp(x, fpu::trunc(s).clamp(-4096.0, 4096.0) as i32));
            }
            Fprem | Fprem1 => {
                let (x, y) = (self.cpu.fpu.st(0), self.cpu.fpu.st(1));
                let q = if mn == Fprem { fpu::trunc(x / y) } else { fpu::libm_round_even(x / y) };
                let r = x - q * y;
                self.cpu.fpu.set_st(0, r);
                let qi = q.abs() as i64;
                let mut sw = self.cpu.fpu.sw & !(C0 | C1 | C2 | C3);
                if qi & 1 != 0 { sw |= C1; }
                if qi & 2 != 0 { sw |= C3; }
                if qi & 4 != 0 { sw |= C0; }
                self.cpu.fpu.sw = sw;
            }
            Fxtract => {
                let v = self.cpu.fpu.st(0);
                let (m, e) = libm::frexp(v);
                self.cpu.fpu.set_st(0, (e - 1) as f64);
                self.cpu.fpu.push(m * 2.0);
            }
            Fcom | Fcomp | Fcompp | Fucom | Fucomp | Fucompp | Ficom | Ficomp => {
                let s = if ins.op_count() >= 1 && ins.op_kind(ins.op_count() - 1) == OpKind::Memory {
                    tryf!(self.fload())
                } else if ins.op_count() == 2 {
                    self.cpu.fpu.st(st_index(ins.op_register(1)))
                } else if ins.op_count() == 1 {
                    self.cpu.fpu.st(st_index(ins.op_register(0)))
                } else {
                    self.cpu.fpu.st(1)
                };
                let a = self.cpu.fpu.st(0);
                self.cpu.fpu.compare_cc(a, s);
                match mn {
                    Fcomp | Fucomp | Ficomp => self.cpu.fpu.pop(),
                    Fcompp | Fucompp => { self.cpu.fpu.pop(); self.cpu.fpu.pop(); }
                    _ => {}
                }
            }
            Fcomi | Fcomip | Fucomi | Fucomip => {
                let s = self.cpu.fpu.st(st_index(ins.op_register(1)));
                let a = self.cpu.fpu.st(0);
                let f = fpu::compare_eflags(a, s);
                self.cpu.eflags = (self.cpu.eflags & !STATUS) | f;
                if matches!(mn, Fcomip | Fucomip) {
                    self.cpu.fpu.pop();
                }
            }
            Ftst => { let a = self.cpu.fpu.st(0); self.cpu.fpu.compare_cc(a, 0.0); }
            Fxam => {
                let f = &self.cpu.fpu;
                let p = ((f.top) & 7) as usize;
                let v = f.st(0);
                let mut cc = if f.empty & (1 << p) != 0 {
                    C3 | C0
                } else if v.is_nan() {
                    C0
                } else if v.is_infinite() {
                    C2 | C0
                } else if v == 0.0 {
                    C3
                } else if v.is_subnormal() {
                    C3 | C2
                } else {
                    C2
                };
                if v.is_sign_negative() { cc |= C1; }
                self.cpu.fpu.sw = (self.cpu.fpu.sw & !(C0 | C1 | C2 | C3)) | cc;
            }
            Fcmovb | Fcmove | Fcmovbe | Fcmovu | Fcmovnb | Fcmovne | Fcmovnbe | Fcmovnu => {
                let e = self.cpu.eflags;
                let c = match mn {
                    Fcmovb => e & CF != 0,
                    Fcmove => e & ZF != 0,
                    Fcmovbe => e & (CF | ZF) != 0,
                    Fcmovu => e & PF != 0,
                    Fcmovnb => e & CF == 0,
                    Fcmovne => e & ZF == 0,
                    Fcmovnbe => e & (CF | ZF) == 0,
                    _ => e & PF == 0,
                };
                if c {
                    let v = self.cpu.fpu.st(st_index(ins.op_register(1)));
                    self.cpu.fpu.set_st(0, v);
                }
            }
            Fldcw => {
                let a = self.addr();
                self.cpu.fpu.cw = tryf!(self.mem.read16(a));
            }
            Fnstcw | Fstcw => {
                let a = self.addr();
                tryf!(self.mem.write16(a, self.cpu.fpu.cw));
            }
            Fnstsw | Fstsw => {
                let v = self.cpu.fpu.status();
                if ins.op_kind(0) == OpKind::Memory {
                    let a = self.addr();
                    tryf!(self.mem.write16(a, v));
                } else {
                    self.cpu.regs[EAX] = (self.cpu.regs[EAX] & 0xffff_0000) | v as u32;
                }
            }
            Fnclex | Fclex => self.cpu.fpu.sw &= !0x80ff,
            Fninit | Finit => {
                let cw = 0x037f;
                self.cpu.fpu = crate::Fpu::default();
                self.cpu.fpu.cw = cw;
            }
            Wait | Fnop | Fndisi | Fneni | Fnsetpm => {}
            Ffree | Ffreep => {
                let i = st_index(ins.op_register(0));
                let p = ((self.cpu.fpu.top + i) & 7) as u8;
                self.cpu.fpu.empty |= 1 << p;
                if mn == Ffreep {
                    self.cpu.fpu.pop();
                }
            }
            Fincstp => self.cpu.fpu.top = (self.cpu.fpu.top + 1) & 7,
            Fdecstp => self.cpu.fpu.top = (self.cpu.fpu.top + 7) & 7,
            Fnstenv | Fstenv | Fnsave | Fsave => {
                let a = self.addr();
                let f = &self.cpu.fpu;
                let env = [f.cw as u32 | 0xffff_0000, f.status() as u32 | 0xffff_0000, f.tag_word() as u32 | 0xffff_0000, 0, 0, 0, 0];
                for (i, v) in env.iter().enumerate() {
                    tryf!(self.mem.write32(a + 4 * i as u32, *v));
                }
                if matches!(mn, Fnsave | Fsave) {
                    for i in 0..8 {
                        let b = fpu::to_f80(self.cpu.fpu.st(i));
                        for (k, x) in b.iter().enumerate() {
                            tryf!(self.mem.write8(a + 28 + 10 * i + k as u32, *x));
                        }
                    }
                    self.cpu.fpu = crate::Fpu::default();
                }
            }
            Fldenv | Frstor => {
                let a = self.addr();
                let cw = tryf!(self.mem.read32(a)) as u16;
                let sw = tryf!(self.mem.read32(a + 4)) as u16;
                let tw = tryf!(self.mem.read32(a + 8)) as u16;
                let mut regs = [0.0; 8];
                if mn == Frstor {
                    for (i, r) in regs.iter_mut().enumerate() {
                        let mut b = [0u8; 10];
                        for (k, x) in b.iter_mut().enumerate() {
                            *x = tryf!(self.mem.read8(a + 28 + 10 * i as u32 + k as u32));
                        }
                        *r = fpu::from_f80(&b);
                    }
                }
                let f = &mut self.cpu.fpu;
                f.cw = cw;
                f.set_status(sw);
                f.set_tag_word(tw);
                if mn == Frstor {
                    for (i, r) in regs.iter().enumerate() {
                        let p = ((f.top + i as u32) & 7) as usize;
                        f.regs[p] = *r;
                    }
                }
            }
            Fxsave | Fxrstor => {
                let a = self.addr();
                if mn == Fxsave {
                    tryf!(crate::fxsave(self.cpu, self.mem, a));
                } else {
                    tryf!(crate::fxrstor(self.cpu, self.mem, a));
                }
            }
            _ => return Exit::InvalidOpcode,
        }
        let _ = flags::mask;
        self.done()
    }
}
