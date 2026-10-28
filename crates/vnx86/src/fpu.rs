//! x87 in f64 ("reduced precision", like box86/FEX's fast mode): VN-era
//! code uses the x87 for ordinary float math, where 53 bits are plenty.
//! One exception is kept exact: FILD m64 followed by FISTP m64 (old MSVC
//! CRTs copy 8 bytes that way), via a per-register exact integer shadow.

use crate::flags::{CF, PF, ZF};

#[derive(Clone, Debug)]
pub struct Fpu {
    /// Physical registers R0..R7; ST(i) = regs[(top + i) & 7].
    pub regs: [f64; 8],
    /// Exact 64-bit integer the register was loaded with by FILD (valid
    /// while the bit is set in `exact`).
    pub ints: [i64; 8],
    pub exact: u8,
    /// Bit set = physical register empty.
    pub empty: u8,
    pub top: u32,
    pub cw: u16,
    /// Status word without TOP (C0..C3, exception bits).
    pub sw: u16,
}

impl Default for Fpu {
    fn default() -> Self {
        Fpu { regs: [0.0; 8], ints: [0; 8], exact: 0, empty: 0xff, top: 0, cw: 0x037f, sw: 0 }
    }
}

pub const C0: u16 = 1 << 8;
pub const C1: u16 = 1 << 9;
pub const C2: u16 = 1 << 10;
pub const C3: u16 = 1 << 14;

impl Fpu {
    #[inline]
    fn phys(&self, i: u32) -> usize {
        ((self.top + i) & 7) as usize
    }
    #[inline]
    pub fn st(&self, i: u32) -> f64 {
        self.regs[self.phys(i)]
    }
    #[inline]
    pub fn set_st(&mut self, i: u32, v: f64) {
        let p = self.phys(i);
        self.regs[p] = v;
        self.exact &= !(1 << p);
        self.empty &= !(1 << p);
    }
    pub fn push(&mut self, v: f64) {
        self.top = (self.top + 7) & 7;
        self.set_st(0, v);
    }
    pub fn push_int(&mut self, v: i64) {
        self.push(v as f64);
        let p = self.phys(0);
        self.ints[p] = v;
        self.exact |= 1 << p;
    }
    /// The exact integer in ST(0), if it was loaded by FILD and not changed.
    pub fn exact_int(&self) -> Option<i64> {
        let p = self.phys(0);
        if self.exact & (1 << p) != 0 { Some(self.ints[p]) } else { None }
    }
    /// Keep the exact integer of ST(0) through sign changes and rounding.
    pub fn map_exact(&mut self, f: impl Fn(i64) -> i64) {
        let p = self.phys(0);
        if self.exact & (1 << p) != 0 {
            let v = f(self.ints[p]);
            self.regs[p] = v as f64;
            self.ints[p] = v;
        }
    }
    pub fn pop(&mut self) {
        let p = self.phys(0);
        self.empty |= 1 << p;
        self.exact &= !(1 << p);
        self.top = (self.top + 1) & 7;
    }
    pub fn swap(&mut self, i: u32) {
        let (a, b) = (self.phys(0), self.phys(i));
        self.regs.swap(a, b);
        self.ints.swap(a, b);
        let bit = |m: u8, x: usize| (m >> x) & 1;
        let (ea, eb) = (bit(self.exact, a), bit(self.exact, b));
        self.exact = (self.exact & !(1 << a | 1 << b)) | eb << a | ea << b;
        let (ma, mb) = (bit(self.empty, a), bit(self.empty, b));
        self.empty = (self.empty & !(1 << a | 1 << b)) | mb << a | ma << b;
    }
    pub fn status(&self) -> u16 {
        (self.sw & !(7 << 11)) | ((self.top as u16) << 11)
    }
    pub fn set_status(&mut self, v: u16) {
        self.sw = v & !(7 << 11);
        self.top = ((v >> 11) & 7) as u32;
    }
    /// Full tag word (11 = empty, 00 = valid; zero/special not tracked).
    pub fn tag_word(&self) -> u16 {
        let mut t = 0u16;
        for p in 0..8 {
            if self.empty & (1 << p) != 0 {
                t |= 3 << (2 * p);
            }
        }
        t
    }
    pub fn set_tag_word(&mut self, t: u16) {
        self.empty = 0;
        for p in 0..8 {
            if (t >> (2 * p)) & 3 == 3 {
                self.empty |= 1 << p;
            }
        }
        self.exact = 0;
    }
    /// Round to an integer with the control word's rounding mode.
    pub fn round(&self, v: f64) -> f64 {
        match (self.cw >> 10) & 3 {
            0 => {
                let r = libm_round_even(v);
                r
            }
            1 => floor(v),
            2 => ceil(v),
            _ => trunc(v),
        }
    }
    /// C3 C2 C0 for FCOM / FTST.
    pub fn compare_cc(&mut self, a: f64, b: f64) {
        let cc = if a.is_nan() || b.is_nan() {
            C3 | C2 | C0
        } else if a > b {
            0
        } else if a < b {
            C0
        } else {
            C3
        };
        self.sw = (self.sw & !(C3 | C2 | C1 | C0)) | cc;
    }
}

/// ZF PF CF for FCOMI / FUCOMI (same encoding as COMISD).
pub fn compare_eflags(a: f64, b: f64) -> u32 {
    if a.is_nan() || b.is_nan() {
        ZF | PF | CF
    } else if a > b {
        0
    } else if a < b {
        CF
    } else {
        ZF
    }
}

// no_std float helpers (core has no floor/ceil without std)
pub fn trunc(v: f64) -> f64 {
    if !(v.abs() < 4503599627370496.0) {
        return v; // already integral, inf or NaN
    }
    libm::copysign((v as i64) as f64, v)
}
pub fn floor(v: f64) -> f64 {
    let t = trunc(v);
    libm::copysign(if t > v { t - 1.0 } else { t }, v)
}
pub fn ceil(v: f64) -> f64 {
    let t = trunc(v);
    libm::copysign(if t < v { t + 1.0 } else { t }, v)
}
pub fn libm_round_even(v: f64) -> f64 {
    if !(v.abs() < 4503599627370496.0) {
        return v;
    }
    let f = floor(v);
    let d = v - f;
    let r = if d > 0.5 {
        f + 1.0
    } else if d < 0.5 {
        f
    } else if (f as i64) & 1 == 0 {
        f
    } else {
        f + 1.0
    };
    libm::copysign(r, v)
}

/// f64 -> 80-bit extended (10 bytes, little-endian).
pub fn to_f80(v: f64) -> [u8; 10] {
    let bits = v.to_bits();
    let sign = (bits >> 63) as u16;
    let exp = ((bits >> 52) & 0x7ff) as i32;
    let frac = bits & ((1u64 << 52) - 1);
    let (e, m) = if exp == 0 && frac == 0 {
        (0u16, 0u64)
    } else if exp == 0x7ff {
        (0x7fff, (1u64 << 63) | (frac << 11))
    } else if exp == 0 {
        // subnormal f64: normalize
        let lz = frac.leading_zeros() - 11; // shift to put the top bit at 52
        let m = frac << (lz + 11);
        ((exp - 1023 + 16383 - lz as i32 + 1) as u16, m)
    } else {
        ((exp - 1023 + 16383) as u16, (1u64 << 63) | (frac << 11))
    };
    let mut out = [0u8; 10];
    out[..8].copy_from_slice(&m.to_le_bytes());
    out[8..].copy_from_slice(&((sign << 15) | e).to_le_bytes());
    out
}

/// 80-bit extended -> f64 (rounded to nearest).
pub fn from_f80(b: &[u8; 10]) -> f64 {
    let m = u64::from_le_bytes(b[..8].try_into().unwrap());
    let se = u16::from_le_bytes([b[8], b[9]]);
    let sign = if se & 0x8000 != 0 { -1.0 } else { 1.0 };
    let e = (se & 0x7fff) as i32;
    if e == 0 && m == 0 {
        return sign * 0.0;
    }
    if e == 0x7fff {
        return if m << 1 == 0 { sign * f64::INFINITY } else { f64::NAN };
    }
    // value = m * 2^(e - 16383 - 63)
    let mut v = m as f64; // rounds the 64-bit mantissa to 53 bits
    let mut p = e - 16383 - 63;
    while p > 0 {
        let s = p.min(1000);
        v *= pow2(s);
        p -= s;
    }
    while p < 0 {
        let s = (-p).min(1000);
        v /= pow2(s);
        p += s;
    }
    sign * v
}

fn pow2(n: i32) -> f64 {
    f64::from_bits(((n + 1023) as u64) << 52)
}
