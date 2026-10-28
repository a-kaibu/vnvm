//! x87 differential test: random sequences on values in a MAP_32BIT data page,
//! run natively (x87 encodings are the same in 64-bit mode) with the precision
//! control set to double (the Windows default, which the f64 model matches),
//! and in the interpreter.
#![cfg(all(target_arch = "x86_64", target_os = "linux"))]

use iced_x86::code_asm::*;
use vnx86::{step, Cpu, Exit, Fault, Memory};

const PAGE: usize = 4096;
const OUT: u32 = 0x200; // output area: 8 doubles + i64 + i32 + status + eflags

struct TwoRegions {
    code: Vec<u8>,
    data_base: u32,
    data: Vec<u8>,
}

impl Memory for TwoRegions {
    fn read8(&mut self, a: u32) -> Result<u8, Fault> {
        if (0x1000..0x1000 + self.code.len() as u32).contains(&a) {
            return Ok(self.code[(a - 0x1000) as usize]);
        }
        let o = a.wrapping_sub(self.data_base) as usize;
        self.data.get(o).copied().ok_or(Fault { addr: a, write: false })
    }
    fn write8(&mut self, a: u32, v: u8) -> Result<(), Fault> {
        let o = a.wrapping_sub(self.data_base) as usize;
        match self.data.get_mut(o) {
            Some(b) => { *b = v; Ok(()) }
            None => Err(Fault { addr: a, write: true }),
        }
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn gen(rng: &mut Rng) -> Result<Vec<u8>, IcedError> {
    let mut a = CodeAssembler::new(32)?;
    a.fninit()?;
    a.xor(eax, eax)?; // same EFLAGS and EAX on both sides
    a.fldcw(word_ptr(ebx + 0x1f0))?;
    let st = [st0, st1, st2, st3, st4, st5, st6, st7];
    let mut depth = 0u32;
    for k in 0..3 {
        a.fld(qword_ptr(ebx + 8 * k))?;
        depth += 1;
    }
    let mut flags_from_fcomi = false;
    for _ in 0..1 + rng.below(4) {
        if depth < 2 && rng.below(2) == 0 {
            a.fld1()?;
            depth += 1;
        }
        if depth < 2 {
            a.fld(qword_ptr(ebx + 0x18))?;
            depth += 1;
        }
        let i = 1 + rng.below(depth as u64 - 1) as usize;
        let m64 = qword_ptr(ebx + 8 * rng.below(8) as i32);
        let m32 = dword_ptr(ebx + 0x80 + 4 * rng.below(4) as i32);
        let i32m = dword_ptr(ebx + 0x40 + 4 * rng.below(4) as i32);
        let i16m = word_ptr(ebx + 0x60 + 2 * rng.below(4) as i32);
        match rng.below(26) {
            0 => a.fadd_2(st0, st[i])?,
            1 => a.fsub_2(st[i], st0)?,
            2 => a.fsubr_2(st0, st[i])?,
            3 => a.fmul_2(st[i], st0)?,
            4 => a.fdiv_2(st0, st[i])?,
            5 => a.fdivr_2(st[i], st0)?,
            6 if depth > 1 => { a.faddp(st[i], st0)?; depth -= 1; }
            7 if depth > 1 => { a.fsubp(st[i], st0)?; depth -= 1; }
            8 if depth > 1 => { a.fdivrp(st[i], st0)?; depth -= 1; }
            9 => a.fadd(m64)?,
            10 => a.fmul(m32)?,
            11 => a.fisub(i32m)?,
            12 => a.fidivr(i16m)?,
            13 => a.fchs()?,
            14 => a.fabs()?,
            15 => { a.fabs()?; a.fsqrt()?; }
            16 => a.fxch(st0, st[i])?,
            17 if depth < 7 => { a.fld(st[i])?; depth += 1; }
            18 if depth < 7 => { a.fld1()?; depth += 1; }
            19 if depth < 7 => { a.fild(i32m)?; depth += 1; }
            20 if depth < 7 => { a.fild(qword_ptr(ebx + 0x70))?; depth += 1; }
            21 => a.frndint()?,
            22 => a.fcom_2(st0, st[i])?,
            23 => { a.fucomi(st0, st[i])?; flags_from_fcomi = true; }
            24 => a.fst(qword_ptr(ebx + OUT as i32 + 0x60))?,
            _ => a.fist(dword_ptr(ebx + OUT as i32 + 0x48))?,
        }
    }
    let _ = flags_from_fcomi;
    a.fistp(qword_ptr(ebx + OUT as i32 + 0x40))?; // exact-int path when ST0 came from FILD m64
    depth -= 1;
    for k in 0..depth {
        a.fstp(qword_ptr(ebx + OUT as i32 + 8 * k as i32))?;
    }
    a.fnstsw(ax)?;
    a.mov(dword_ptr(ebx + OUT as i32 + 0x50), eax)?;
    a.pushfd()?;
    a.pop(dword_ptr(ebx + OUT as i32 + 0x54))?;
    a.assemble(0x1000)
}

fn fill(rng: &mut Rng, d: &mut [u8]) {
    for k in 0..8 {
        let f: f64 = [0.0, -0.0, 1.0, -1.5, 0.5, 3.0, -7.25, 1e10, 1e-10, 123456.789, -2.5, 4.5, 1.0 / 3.0][rng.below(13) as usize];
        let f = if rng.below(3) == 0 { f * (rng.below(1000) as f64 + 1.0) } else { f };
        d[8 * k..8 * k + 8].copy_from_slice(&f.to_le_bytes());
    }
    for k in 0..4 {
        let v = [0i32, 1, -1, 7, -100, 1 << 20, i32::MIN, 3][rng.below(8) as usize];
        d[0x40 + 4 * k..0x44 + 4 * k].copy_from_slice(&v.to_le_bytes());
        let w = [1i16, -3, 2, 1000, -32768, 5][rng.below(6) as usize];
        d[0x60 + 2 * k..0x62 + 2 * k].copy_from_slice(&w.to_le_bytes());
        let f = [1.5f32, -0.25, 100.0, 3.0e7, 0.1][rng.below(5) as usize];
        d[0x80 + 4 * k..0x84 + 4 * k].copy_from_slice(&f.to_le_bytes());
    }
    // an i64 above 2^53: survives FILD/FISTP only if kept exact
    let big = (0x0123_4567_89ab_cdefu64 ^ rng.next()) as i64;
    d[0x70..0x78].copy_from_slice(&big.to_le_bytes());
    d[0x1f0..0x1f2].copy_from_slice(&0x027fu16.to_le_bytes()); // double precision, round nearest
}

#[test]
fn random_x87_sequences_match_the_host_cpu() {
    let page = unsafe {
        libc::mmap(core::ptr::null_mut(), 2 * PAGE, libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                   libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_32BIT, -1, 0)
    } as *mut u8;
    assert!(page as isize != -1);
    let code = page;
    let data = unsafe { page.add(PAGE) };
    let data_base = data as usize as u32;
    let mut rng = Rng(0xdead_beef_1234_5678);
    let mut failures = Vec::new();
    for _ in 0..30_000 {
        let body = gen(&mut rng).unwrap();
        let mut d = vec![0u8; PAGE];
        fill(&mut rng, &mut d);
        // native: push rbx; mov rbx, rdi; body; pop rbx; ret
        let mut c = vec![0x53, 0x48, 0x89, 0xfb];
        c.extend_from_slice(&body);
        c.extend([0x5b, 0xc3]);
        unsafe {
            core::ptr::copy_nonoverlapping(c.as_ptr(), code, c.len());
            core::ptr::copy_nonoverlapping(d.as_ptr(), data, PAGE);
            let f: extern "C" fn(*mut u8) = core::mem::transmute(code);
            f(data);
        }
        let native = unsafe { core::slice::from_raw_parts(data, PAGE) }.to_vec();

        let mut mem = TwoRegions { code: body.clone(), data_base, data: d.clone() };
        let mut cpu = Cpu::default();
        cpu.eip = 0x1000;
        cpu.regs[3] = data_base;
        cpu.regs[4] = data_base + 0xf00;
        let end = 0x1000 + body.len() as u32;
        let mut n = 0;
        while cpu.eip != end {
            let e = step(&mut cpu, &mut mem);
            assert_eq!(e, Exit::Continue, "at {:x}", cpu.eip);
            n += 1;
            assert!(n < 100);
        }
        let o = OUT as usize;
        let want = &native[o..o + 0x68];
        let got = &mem.data[o..o + 0x68];
        let mut ok = true;
        for k in 0..13 {
            let (w, g) = (&want[8 * k..8 * k + 8], &got[8 * k..8 * k + 8]);
            if w != g {
                let (fw, fg) = (f64::from_le_bytes(w.try_into().unwrap()), f64::from_le_bytes(g.try_into().unwrap()));
                let (iw, ig) = (i64::from_le_bytes(w.try_into().unwrap()), i64::from_le_bytes(g.try_into().unwrap()));
                // f64 model: operands are rounded to 53 bits before the
                // operation (double rounding), so allow 1 ulp; same for an
                // integer result above 2^53.
                let ulp_ok = (k < 8 || k == 12) && (iw.wrapping_sub(ig)).unsigned_abs() <= 1;
                let int_ok = k == 8 && iw.unsigned_abs() > (1u64 << 53) && iw.wrapping_sub(ig).unsigned_abs() <= (iw.unsigned_abs() >> 52) + 1;
                if ulp_ok || int_ok {
                    continue;
                }
                if !(k < 8 || k == 12) || !(fw.is_nan() && fg.is_nan()) {
                    // status word (k == 10): compare C0 C2 C3 and TOP only
                    if k == 10 {
                        let sw = |b: &[u8]| u16::from_le_bytes([b[0], b[1]]) & 0x7d00;
                        let fl = |b: &[u8]| u32::from_le_bytes(b[4..8].try_into().unwrap()) & 0x45;
                        if sw(w) == sw(g) && fl(w) == fl(g) {
                            continue;
                        }
                    }
                    ok = false;
                }
            }
        }
        if !ok && failures.len() < 15 {
            let dis = {
                use iced_x86::{Decoder, DecoderOptions, Formatter, NasmFormatter};
                let mut s = String::new();
                let mut dec = Decoder::with_ip(32, &body, 0x1000, DecoderOptions::NONE);
                let mut f = NasmFormatter::new();
                for ins in &mut dec {
                    let mut t = String::new();
                    f.format(&ins, &mut t);
                    s += &t;
                    s += "; ";
                }
                s
            };
            failures.push(format!("{}\n  want {:02x?}\n  got  {:02x?}", dis, want, got));
        }
    }
    assert!(failures.is_empty(), "mismatches:\n{}", failures.join("\n"));
}
