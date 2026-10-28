//! Differential test against the host CPU: random register-form integer
//! instructions run natively (64-bit mode, where these encodings mean the same
//! as in 32-bit mode when no REX prefix is used) and in the interpreter.
#![cfg(all(target_arch = "x86_64", target_os = "linux"))]

use iced_x86::{Decoder, DecoderOptions, Mnemonic};
use vnx86::flags::*;
use vnx86::{step, Cpu, Exit, Fault, Memory};

struct Vecmem(Vec<u8>);
impl Memory for Vecmem {
    fn read8(&mut self, a: u32) -> Result<u8, Fault> {
        self.0.get(a as usize).copied().ok_or(Fault { addr: a, write: false })
    }
    fn write8(&mut self, a: u32, v: u8) -> Result<(), Fault> {
        match self.0.get_mut(a as usize) {
            Some(b) => { *b = v; Ok(()) }
            None => Err(Fault { addr: a, write: true }),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct State {
    r: [u64; 8],
    flags: u64,
    xmm: [[u8; 16]; 8],
    mm: [u64; 8],
}

struct Native {
    page: *mut u8,
}

impl Native {
    fn new() -> Self {
        let page = unsafe {
            libc::mmap(core::ptr::null_mut(), 4096, libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                       libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0)
        } as *mut u8;
        assert!(!page.is_null() && page as isize != -1);
        Native { page }
    }

    fn run(&self, insn: &[u8], st: &mut State) {
        let mut c: Vec<u8> = vec![0x53, 0x55, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57, 0x49, 0x89, 0xff];
        c.extend([0x41, 0xff, 0x77, 0x40, 0x9d]); // push [r15+64]; popfq
        c.extend([0x41, 0x8b, 0x07, 0x41, 0x8b, 0x4f, 0x08, 0x41, 0x8b, 0x57, 0x10, 0x41, 0x8b, 0x5f, 0x18]);
        c.extend([0x41, 0x8b, 0x6f, 0x28, 0x41, 0x8b, 0x77, 0x30, 0x41, 0x8b, 0x7f, 0x38]);
        for n in 0..8u8 {
            c.extend([0xf3, 0x41, 0x0f, 0x6f, 0x87 | n << 3]);
            c.extend((72 + 16 * n as u32).to_le_bytes());
            c.extend([0x41, 0x0f, 0x6f, 0x87 | n << 3]);
            c.extend((200 + 8 * n as u32).to_le_bytes());
        }
        c.extend_from_slice(insn);
        c.extend([0x9c, 0x41, 0x8f, 0x47, 0x40]); // pushfq; pop [r15+64]
        for n in 0..8u8 {
            c.extend([0xf3, 0x41, 0x0f, 0x7f, 0x87 | n << 3]);
            c.extend((72 + 16 * n as u32).to_le_bytes());
            c.extend([0x41, 0x0f, 0x7f, 0x87 | n << 3]);
            c.extend((200 + 8 * n as u32).to_le_bytes());
        }
        c.extend([0x0f, 0x77]); // emms
        c.extend([0x49, 0x89, 0x07, 0x49, 0x89, 0x4f, 0x08, 0x49, 0x89, 0x57, 0x10, 0x49, 0x89, 0x5f, 0x18]);
        c.extend([0x49, 0x89, 0x6f, 0x28, 0x49, 0x89, 0x77, 0x30, 0x49, 0x89, 0x7f, 0x38]);
        c.extend([0x41, 0x5f, 0x41, 0x5e, 0x41, 0x5d, 0x41, 0x5c, 0x5d, 0x5b, 0xc3]);
        unsafe {
            core::ptr::copy_nonoverlapping(c.as_ptr(), self.page, c.len());
            let f: extern "C" fn(*mut State) = core::mem::transmute(self.page);
            f(st);
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
    fn value(&mut self) -> u32 {
        const EDGE: [u32; 12] = [0, 1, 2, 0x7f, 0x80, 0xff, 0x7fff, 0x8000, 0xffff, 0x7fff_ffff, 0x8000_0000, 0xffff_ffff];
        match self.below(4) {
            0 => EDGE[self.below(EDGE.len() as u64) as usize],
            1 => self.below(64) as u32,
            _ => self.next() as u32,
        }
    }
}

/// A register (not ESP for 16/32-bit operands).
fn reg(rng: &mut Rng, byte: bool) -> u8 {
    loop {
        let r = rng.below(8) as u8;
        if byte || r != 4 {
            return r;
        }
    }
}

fn modrm_rr(rng: &mut Rng, byte: bool) -> u8 {
    0xc0 | reg(rng, byte) << 3 | reg(rng, byte)
}

fn modrm_ext(rng: &mut Rng, ext: u8, byte: bool) -> u8 {
    0xc0 | ext << 3 | reg(rng, byte)
}

fn gen(rng: &mut Rng) -> Vec<u8> {
    let mut v = Vec::new();
    let op16 = rng.below(6) == 0;
    if op16 {
        v.push(0x66);
    }
    let imm = |rng: &mut Rng, v: &mut Vec<u8>, n: usize| {
        let x = rng.value();
        v.extend_from_slice(&x.to_le_bytes()[..n]);
    };
    let wide = if op16 { 2 } else { 4 };
    match rng.below(22) {
        0 | 1 => {
            // ALU r/m,r and r,r/m
            let base = [0x00u8, 0x08, 0x10, 0x18, 0x20, 0x28, 0x30, 0x38][rng.below(8) as usize];
            let op = base + rng.below(4) as u8;
            v.push(op);
            v.push(modrm_rr(rng, op & 1 == 0));
        }
        2 => {
            let op = [0x84u8, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x8b][rng.below(8) as usize];
            v.push(op);
            v.push(modrm_rr(rng, op & 1 == 0));
        }
        3 => {
            let op = [0x80u8, 0x81, 0x83][rng.below(3) as usize];
            v.push(op);
            let e = rng.below(8) as u8; v.push(modrm_ext(rng, e, op == 0x80));
            imm(rng, &mut v, if op == 0x81 { wide } else { 1 });
        }
        4 | 5 => {
            // shifts and rotates (skip /6)
            let op = [0xc0u8, 0xc1, 0xd0, 0xd1, 0xd2, 0xd3][rng.below(6) as usize];
            let ext = [0u8, 1, 2, 3, 4, 5, 7][rng.below(7) as usize];
            v.push(op);
            v.push(modrm_ext(rng, ext, op & 1 == 0));
            if op < 0xd0 {
                let c = if rng.below(2) == 0 { rng.below(8) } else { rng.below(256) } as u8;
                v.push(c);
            }
        }
        6 => {
            let op = [0xf6u8, 0xf7][rng.below(2) as usize];
            let ext = [0u8, 2, 3, 4, 5, 6, 7][rng.below(7) as usize];
            v.push(op);
            v.push(modrm_ext(rng, ext, op == 0xf6));
            if ext == 0 {
                imm(rng, &mut v, if op == 0xf6 { 1 } else { wide });
            }
        }
        7 => {
            let op = [0xfeu8, 0xff][rng.below(2) as usize];
            v.push(op);
            let e = rng.below(2) as u8; v.push(modrm_ext(rng, e, op == 0xfe));
        }
        8 => {
            match rng.below(3) {
                0 => { v.extend([0x0f, 0xaf]); v.push(modrm_rr(rng, false)); }
                1 => { v.push(0x69); v.push(modrm_rr(rng, false)); imm(rng, &mut v, wide); }
                _ => { v.push(0x6b); v.push(modrm_rr(rng, false)); imm(rng, &mut v, 1); }
            }
        }
        9 => {
            if rng.below(2) == 0 {
                v.extend([0x0f, [0xa3u8, 0xab, 0xb3, 0xbb][rng.below(4) as usize]]);
                v.push(modrm_rr(rng, false));
            } else {
                v.extend([0x0f, 0xba]);
                let e = 4 + rng.below(4) as u8; v.push(modrm_ext(rng, e, false));
                imm(rng, &mut v, 1);
            }
        }
        10 => {
            v.extend([0x0f, [0xbcu8, 0xbd][rng.below(2) as usize]]);
            v.push(modrm_rr(rng, false));
        }
        11 => {
            let op = [0xa4u8, 0xa5, 0xac, 0xad][rng.below(4) as usize];
            v.extend([0x0f, op]);
            v.push(modrm_rr(rng, false));
            if op & 1 == 0 {
                v.push(rng.below(32) as u8);
            }
        }
        12 => {
            let op = [0xb0u8, 0xb1, 0xc0, 0xc1][rng.below(4) as usize];
            v.extend([0x0f, op]);
            v.push(modrm_rr(rng, op & 1 == 0));
        }
        13 => {
            v.extend([0x0f, 0x40 + rng.below(16) as u8]);
            v.push(modrm_rr(rng, false));
        }
        14 => {
            v.extend([0x0f, 0x90 + rng.below(16) as u8]);
            v.push(modrm_ext(rng, 0, true));
        }
        15 => {
            let op = [0xb6u8, 0xb7, 0xbe, 0xbf][rng.below(4) as usize];
            v.extend([0x0f, op]);
            let r = reg(rng, false);
            v.push(0xc0 | r << 3 | reg(rng, op & 1 == 0));
        }
        16 => v.push([0x98u8, 0x99, 0x9e, 0x9f, 0xf5, 0xf8, 0xf9][rng.below(7) as usize]),
        17 => {
            v.retain(|&b| b != 0x66);
            v.extend([0x0f, 0xc8 + reg(rng, false)]);
        }
        18 | 19 => {
            // LEA with a SIB or plain base (no RIP-relative form)
            v.retain(|&b| b != 0x66);
            v.push(0x8d);
            let r = reg(rng, false);
            match rng.below(3) {
                0 => { v.push(0x80 | r << 3 | [0u8, 1, 2, 3, 5, 6, 7][rng.below(7) as usize]); imm(rng, &mut v, 4); }
                1 => {
                    v.push(0x44 | r << 3);
                    let idx = reg(rng, false);
                    v.push((rng.below(4) as u8) << 6 | idx << 3 | [0u8, 1, 2, 3, 5, 6, 7][rng.below(7) as usize]);
                    imm(rng, &mut v, 1);
                }
                _ => {
                    v.push(0x04 | r << 3);
                    let idx = reg(rng, false);
                    v.push((rng.below(4) as u8) << 6 | idx << 3 | 5);
                    imm(rng, &mut v, 4);
                }
            }
        }
        _ => {
            // NEG/NOT/INC/DEC already covered; ADC/SBB chains with random CF
            let op = [0x10u8, 0x11, 0x18, 0x19][rng.below(4) as usize];
            v.push(op);
            v.push(modrm_rr(rng, op & 1 == 0));
        }
    }
    v
}

/// Flags the SDM leaves undefined for this instruction.
fn undefined(m: Mnemonic, bytes: &[u8], cpu: &Cpu, bits: u32) -> (u32, bool) {
    use Mnemonic::*;
    let shift_count = |cpu: &Cpu| -> u32 {
        // count is the last immediate byte, CL, or 1
        let last = *bytes.last().unwrap();
        let op = bytes.iter().find(|&&b| b != 0x66).copied().unwrap();
        (match op {
            0xc0 | 0xc1 => last as u32,
            0xd2 | 0xd3 => cpu.regs[1] & 0xff,
            0x0f => match bytes[bytes.iter().position(|&b| b == 0x0f).unwrap() + 1] {
                0xa4 | 0xac => last as u32,
                _ => cpu.regs[1] & 0xff,
            },
            _ => 1,
        }) & 0x1f
    };
    match m {
        And | Or | Xor | Test => (AF, false),
        Shl | Sal | Shr | Sar => {
            let c = shift_count(cpu);
            let mut u = AF;
            if c != 1 { u |= OF; }
            if c > bits { u |= CF; }
            (u, false)
        }
        Rol | Ror | Rcl | Rcr => (if shift_count(cpu) != 1 { OF } else { 0 }, false),
        Shld | Shrd => {
            let c = shift_count(cpu);
            if c > bits { return (STATUS, true); }
            (AF | if c != 1 { OF } else { 0 }, false)
        }
        Mul | Imul => (SF | ZF | AF | PF, false),
        Div | Idiv => (STATUS, false),
        Bt | Bts | Btr | Btc => (OF | SF | AF | PF, false),
        Bsf | Bsr => (CF | OF | SF | AF | PF, false),
        _ => (0, false),
    }
}

#[test]
fn random_integer_ops_match_the_host_cpu() {
    let native = Native::new();
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut done = 0;
    let mut failures = Vec::new();
    for _ in 0..200_000 {
        let insn = gen(&mut rng);
        let mut cpu = Cpu::default();
        for (i, r) in cpu.regs.iter_mut().enumerate() {
            if i != 4 { *r = rng.value(); }
        }
        cpu.regs[4] = 0x8000;
        cpu.eflags = 0x202 | (rng.next() as u32 & STATUS);
        let before = cpu.clone();
        let mut mem = Vecmem(vec![0; 0x10000]);
        mem.0[0x1000..0x1000 + insn.len()].copy_from_slice(&insn);
        cpu.eip = 0x1000;
        let exit = step(&mut cpu, &mut mem);
        let ins = Decoder::new(32, &insn, DecoderOptions::NONE).decode();
        if exit == Exit::DivideError {
            continue; // the host would raise SIGFPE
        }
        assert_eq!(exit, Exit::Continue, "{:02x?} {:?}", insn, ins.mnemonic());
        let mut st = State::default();
        for i in 0..8 { st.r[i] = before.regs[i] as u64; }
        st.flags = before.eflags as u64;
        native.run(&insn, &mut st);
        let bits = {
            let op16 = insn[0] == 0x66;
            let byte = matches!(insn.iter().find(|&&b| b != 0x66), Some(0x00 | 0x02 | 0x08 | 0x0a | 0x10 | 0x12 | 0x18 | 0x1a | 0x20 | 0x22 | 0x28 | 0x2a | 0x30 | 0x32 | 0x38 | 0x3a | 0x80 | 0x84 | 0x86 | 0x88 | 0x8a | 0xc0 | 0xd0 | 0xd2 | 0xf6 | 0xfe));
            if byte { 8 } else if op16 { 16 } else { 32 }
        };
        let (undef, skip) = undefined(ins.mnemonic(), &insn, &before, bits);
        if skip { continue; }
        let want_flags = st.flags as u32 & STATUS & !undef;
        let got_flags = cpu.eflags & STATUS & !undef;
        let mut ok = want_flags == got_flags;
        for i in 0..8 {
            if i != 4 && st.r[i] as u32 != cpu.regs[i] {
                ok = false;
            }
        }
        if !ok && failures.len() < 20 {
            failures.push(format!(
                "{:02x?} {:?}: in {:08x?} fl {:03x}\n  want {:08x?} fl {:03x}\n  got  {:08x?} fl {:03x}",
                insn, ins.mnemonic(), before.regs, before.eflags & STATUS,
                st.r.iter().map(|&v| v as u32).collect::<Vec<_>>(), want_flags, cpu.regs, got_flags
            ));
        }
        done += 1;
    }
    assert!(failures.is_empty(), "{} checked, mismatches:\n{}", done, failures.join("\n"));
    assert!(done > 150_000);
}


/// (prefix, opcode after 0F, imm8, float lane bits (0 = integer), operand classes)
/// classes: b'x' xmm, b'm' mm, b'g' gpr (reg field / rm field)
struct SseOp(u8, u8, bool, u32, u8, u8);

const SSE_OPS: &[SseOp] = &[
    // integer xmm (66) and mmx (none) forms of the same opcodes
    SseOp(0x66, 0x60, false, 0, b'x', b'x'), SseOp(0x66, 0x61, false, 0, b'x', b'x'), SseOp(0x66, 0x62, false, 0, b'x', b'x'),
    SseOp(0x66, 0x63, false, 0, b'x', b'x'), SseOp(0x66, 0x64, false, 0, b'x', b'x'), SseOp(0x66, 0x65, false, 0, b'x', b'x'),
    SseOp(0x66, 0x66, false, 0, b'x', b'x'), SseOp(0x66, 0x67, false, 0, b'x', b'x'), SseOp(0x66, 0x68, false, 0, b'x', b'x'),
    SseOp(0x66, 0x69, false, 0, b'x', b'x'), SseOp(0x66, 0x6a, false, 0, b'x', b'x'), SseOp(0x66, 0x6b, false, 0, b'x', b'x'),
    SseOp(0x66, 0x6c, false, 0, b'x', b'x'), SseOp(0x66, 0x6d, false, 0, b'x', b'x'), SseOp(0x66, 0x6f, false, 0, b'x', b'x'),
    SseOp(0x66, 0x70, true, 0, b'x', b'x'), SseOp(0xf2, 0x70, true, 0, b'x', b'x'), SseOp(0xf3, 0x70, true, 0, b'x', b'x'),
    SseOp(0x66, 0x74, false, 0, b'x', b'x'), SseOp(0x66, 0x75, false, 0, b'x', b'x'), SseOp(0x66, 0x76, false, 0, b'x', b'x'),
    SseOp(0x66, 0xd1, false, 0, b'x', b'x'), SseOp(0x66, 0xd2, false, 0, b'x', b'x'), SseOp(0x66, 0xd3, false, 0, b'x', b'x'),
    SseOp(0x66, 0xd4, false, 0, b'x', b'x'), SseOp(0x66, 0xd5, false, 0, b'x', b'x'), SseOp(0x66, 0xd7, false, 0, b'g', b'x'),
    SseOp(0x66, 0xd8, false, 0, b'x', b'x'), SseOp(0x66, 0xd9, false, 0, b'x', b'x'), SseOp(0x66, 0xda, false, 0, b'x', b'x'),
    SseOp(0x66, 0xdb, false, 0, b'x', b'x'), SseOp(0x66, 0xdc, false, 0, b'x', b'x'), SseOp(0x66, 0xdd, false, 0, b'x', b'x'),
    SseOp(0x66, 0xde, false, 0, b'x', b'x'), SseOp(0x66, 0xdf, false, 0, b'x', b'x'), SseOp(0x66, 0xe0, false, 0, b'x', b'x'),
    SseOp(0x66, 0xe1, false, 0, b'x', b'x'), SseOp(0x66, 0xe2, false, 0, b'x', b'x'), SseOp(0x66, 0xe3, false, 0, b'x', b'x'),
    SseOp(0x66, 0xe4, false, 0, b'x', b'x'), SseOp(0x66, 0xe5, false, 0, b'x', b'x'), SseOp(0x66, 0xe8, false, 0, b'x', b'x'),
    SseOp(0x66, 0xe9, false, 0, b'x', b'x'), SseOp(0x66, 0xea, false, 0, b'x', b'x'), SseOp(0x66, 0xeb, false, 0, b'x', b'x'),
    SseOp(0x66, 0xec, false, 0, b'x', b'x'), SseOp(0x66, 0xed, false, 0, b'x', b'x'), SseOp(0x66, 0xee, false, 0, b'x', b'x'),
    SseOp(0x66, 0xef, false, 0, b'x', b'x'), SseOp(0x66, 0xf1, false, 0, b'x', b'x'), SseOp(0x66, 0xf2, false, 0, b'x', b'x'),
    SseOp(0x66, 0xf3, false, 0, b'x', b'x'), SseOp(0x66, 0xf4, false, 0, b'x', b'x'), SseOp(0x66, 0xf5, false, 0, b'x', b'x'),
    SseOp(0x66, 0xf6, false, 0, b'x', b'x'), SseOp(0x66, 0xf8, false, 0, b'x', b'x'), SseOp(0x66, 0xf9, false, 0, b'x', b'x'),
    SseOp(0x66, 0xfa, false, 0, b'x', b'x'), SseOp(0x66, 0xfb, false, 0, b'x', b'x'), SseOp(0x66, 0xfc, false, 0, b'x', b'x'),
    SseOp(0x66, 0xfd, false, 0, b'x', b'x'), SseOp(0x66, 0xfe, false, 0, b'x', b'x'),
    SseOp(0x00, 0x60, false, 0, b'm', b'm'), SseOp(0x00, 0x61, false, 0, b'm', b'm'), SseOp(0x00, 0x62, false, 0, b'm', b'm'),
    SseOp(0x00, 0x63, false, 0, b'm', b'm'), SseOp(0x00, 0x67, false, 0, b'm', b'm'), SseOp(0x00, 0x68, false, 0, b'm', b'm'),
    SseOp(0x00, 0x6b, false, 0, b'm', b'm'), SseOp(0x00, 0x70, true, 0, b'm', b'm'), SseOp(0x00, 0x74, false, 0, b'm', b'm'),
    SseOp(0x00, 0xd5, false, 0, b'm', b'm'), SseOp(0x00, 0xd7, false, 0, b'g', b'm'), SseOp(0x00, 0xdc, false, 0, b'm', b'm'),
    SseOp(0x00, 0xe5, false, 0, b'm', b'm'), SseOp(0x00, 0xeb, false, 0, b'm', b'm'), SseOp(0x00, 0xf5, false, 0, b'm', b'm'),
    SseOp(0x00, 0xf6, false, 0, b'm', b'm'), SseOp(0x00, 0xfe, false, 0, b'm', b'm'), SseOp(0x00, 0xd1, false, 0, b'm', b'm'),
    SseOp(0x00, 0x6f, false, 0, b'm', b'm'),
    // moves between gpr and vector registers
    SseOp(0x66, 0x6e, false, 0, b'x', b'g'), SseOp(0x66, 0x7e, false, 0, b'x', b'g'), SseOp(0x00, 0x6e, false, 0, b'm', b'g'),
    SseOp(0x00, 0x7e, false, 0, b'm', b'g'), SseOp(0xf3, 0x7e, false, 0, b'x', b'x'), SseOp(0xf3, 0x6f, false, 0, b'x', b'x'),
    SseOp(0x66, 0xc4, true, 0, b'x', b'g'), SseOp(0x66, 0xc5, true, 0, b'g', b'x'),
    // single precision
    SseOp(0x00, 0x10, false, 32, b'x', b'x'), SseOp(0x00, 0x12, false, 32, b'x', b'x'), SseOp(0x00, 0x14, false, 32, b'x', b'x'),
    SseOp(0x00, 0x15, false, 32, b'x', b'x'), SseOp(0x00, 0x16, false, 32, b'x', b'x'), SseOp(0x00, 0x28, false, 32, b'x', b'x'),
    SseOp(0x00, 0x2e, false, 32, b'x', b'x'), SseOp(0x00, 0x2f, false, 32, b'x', b'x'), SseOp(0x00, 0x50, false, 32, b'g', b'x'),
    SseOp(0x00, 0x51, false, 32, b'x', b'x'), SseOp(0x00, 0x54, false, 32, b'x', b'x'), SseOp(0x00, 0x55, false, 32, b'x', b'x'),
    SseOp(0x00, 0x56, false, 32, b'x', b'x'), SseOp(0x00, 0x57, false, 32, b'x', b'x'), SseOp(0x00, 0x58, false, 32, b'x', b'x'),
    SseOp(0x00, 0x59, false, 32, b'x', b'x'), SseOp(0x00, 0x5a, false, 64, b'x', b'x'), SseOp(0x00, 0x5b, false, 32, b'x', b'x'),
    SseOp(0x00, 0x5c, false, 32, b'x', b'x'), SseOp(0x00, 0x5d, false, 32, b'x', b'x'), SseOp(0x00, 0x5e, false, 32, b'x', b'x'),
    SseOp(0x00, 0x5f, false, 32, b'x', b'x'), SseOp(0x00, 0xc2, true, 32, b'x', b'x'), SseOp(0x00, 0xc6, true, 32, b'x', b'x'),
    SseOp(0xf3, 0x10, false, 32, b'x', b'x'), SseOp(0xf3, 0x51, false, 32, b'x', b'x'), SseOp(0xf3, 0x58, false, 32, b'x', b'x'),
    SseOp(0xf3, 0x59, false, 32, b'x', b'x'), SseOp(0xf3, 0x5a, false, 64, b'x', b'x'), SseOp(0xf3, 0x5b, false, 0, b'x', b'x'),
    SseOp(0xf3, 0x5c, false, 32, b'x', b'x'), SseOp(0xf3, 0x5d, false, 32, b'x', b'x'), SseOp(0xf3, 0x5e, false, 32, b'x', b'x'),
    SseOp(0xf3, 0x5f, false, 32, b'x', b'x'), SseOp(0xf3, 0x2c, false, 0, b'g', b'x'), SseOp(0xf3, 0x2d, false, 0, b'g', b'x'),
    SseOp(0xf3, 0x2a, false, 32, b'x', b'g'), SseOp(0xf3, 0xe6, false, 64, b'x', b'x'), SseOp(0xf3, 0xc2, true, 32, b'x', b'x'),
    // double precision
    SseOp(0xf2, 0x10, false, 64, b'x', b'x'), SseOp(0xf2, 0x51, false, 64, b'x', b'x'), SseOp(0xf2, 0x58, false, 64, b'x', b'x'),
    SseOp(0xf2, 0x59, false, 64, b'x', b'x'), SseOp(0xf2, 0x5a, false, 32, b'x', b'x'), SseOp(0xf2, 0x5c, false, 64, b'x', b'x'),
    SseOp(0xf2, 0x5d, false, 64, b'x', b'x'), SseOp(0xf2, 0x5e, false, 64, b'x', b'x'), SseOp(0xf2, 0x5f, false, 64, b'x', b'x'),
    SseOp(0xf2, 0x2c, false, 0, b'g', b'x'), SseOp(0xf2, 0x2d, false, 0, b'g', b'x'), SseOp(0xf2, 0x2a, false, 64, b'x', b'g'),
    SseOp(0xf2, 0xe6, false, 0, b'x', b'x'), SseOp(0xf2, 0xc2, true, 64, b'x', b'x'),
    SseOp(0x66, 0x14, false, 64, b'x', b'x'), SseOp(0x66, 0x15, false, 64, b'x', b'x'), SseOp(0x66, 0x2e, false, 64, b'x', b'x'),
    SseOp(0x66, 0x2f, false, 64, b'x', b'x'), SseOp(0x66, 0x50, false, 64, b'g', b'x'), SseOp(0x66, 0x51, false, 64, b'x', b'x'),
    SseOp(0x66, 0x54, false, 64, b'x', b'x'), SseOp(0x66, 0x57, false, 64, b'x', b'x'), SseOp(0x66, 0x58, false, 64, b'x', b'x'),
    SseOp(0x66, 0x59, false, 64, b'x', b'x'), SseOp(0x66, 0x5a, false, 32, b'x', b'x'), SseOp(0x66, 0x5b, false, 0, b'x', b'x'),
    SseOp(0x66, 0x5c, false, 64, b'x', b'x'), SseOp(0x66, 0x5d, false, 64, b'x', b'x'), SseOp(0x66, 0x5e, false, 64, b'x', b'x'),
    SseOp(0x66, 0x5f, false, 64, b'x', b'x'), SseOp(0x66, 0xc2, true, 64, b'x', b'x'), SseOp(0x66, 0xc6, true, 64, b'x', b'x'),
    SseOp(0x66, 0xe6, false, 0, b'x', b'x'),
];

fn rand_vec(rng: &mut Rng) -> [u8; 16] {
    let mut v = [0u8; 16];
    match rng.below(4) {
        0 => {
            for k in 0..4 {
                let f: f32 = [0.0, -0.0, 1.0, -1.5, 0.5, 1e30, -3.25, 65536.0, 2.5e9, f32::INFINITY, f32::NAN][rng.below(11) as usize];
                v[4 * k..4 * k + 4].copy_from_slice(&f.to_le_bytes());
            }
        }
        1 => {
            for k in 0..2 {
                let f: f64 = [0.0, -0.0, 1.0, -1.5, 0.5, 1e300, -3.25, 2.5e9, -7.5e9, f64::INFINITY, f64::NAN][rng.below(11) as usize];
                v[8 * k..8 * k + 8].copy_from_slice(&f.to_le_bytes());
            }
        }
        2 => {
            for k in 0..4 {
                let x = rng.value();
                v[4 * k..4 * k + 4].copy_from_slice(&x.to_le_bytes());
            }
        }
        _ => {
            for b in v.iter_mut() {
                *b = rng.next() as u8;
            }
        }
    }
    v
}

fn lanes_equal(a: u128, b: u128, fl: u32) -> bool {
    if a == b {
        return true;
    }
    if fl == 0 {
        return false;
    }
    let n = 128 / fl;
    (0..n).all(|i| {
        let m: u128 = if fl == 64 { u64::MAX as u128 } else { (1u128 << fl) - 1 };
        let (x, y) = ((a >> (i * fl)) & m, (b >> (i * fl)) & m);
        if x == y {
            return true;
        }
        if fl == 32 {
            f32::from_bits(x as u32).is_nan() && f32::from_bits(y as u32).is_nan()
        } else {
            f64::from_bits(x as u64).is_nan() && f64::from_bits(y as u64).is_nan()
        }
    })
}

#[test]
fn random_sse_mmx_ops_match_the_host_cpu() {
    let native = Native::new();
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let mut failures = Vec::new();
    let mut done = 0;
    for _ in 0..100_000 {
        let op = &SSE_OPS[rng.below(SSE_OPS.len() as u64) as usize];
        let mut insn = Vec::new();
        if op.0 != 0 {
            insn.push(op.0);
        }
        insn.extend([0x0f, op.1]);
        let pick = |rng: &mut Rng, c: u8| if c == b'g' { reg(rng, false) } else { rng.below(8) as u8 };
        let r = pick(&mut rng, op.4);
        let m = pick(&mut rng, op.5);
        // shifts by immediate (71/72/73) are covered by pshufd-like imm forms below
        insn.push(0xc0 | r << 3 | m);
        if op.2 {
            let imm = rng.next() as u8;
            insn.push(if matches!(op.1, 0xc2) { imm & 7 } else { imm });
        }
        let mut cpu = Cpu::default();
        for (i, x) in cpu.regs.iter_mut().enumerate() {
            if i != 4 { *x = rng.value(); }
        }
        cpu.regs[4] = 0x8000;
        cpu.eflags = 0x202 | (rng.next() as u32 & STATUS);
        for x in cpu.xmm.iter_mut() {
            *x = u128::from_le_bytes(rand_vec(&mut rng));
        }
        for x in cpu.mm.iter_mut() {
            *x = u128::from_le_bytes(rand_vec(&mut rng)) as u64;
        }
        let before = cpu.clone();
        let mut mem = Vecmem(vec![0; 0x10000]);
        mem.0[0x1000..0x1000 + insn.len()].copy_from_slice(&insn);
        cpu.eip = 0x1000;
        let exit = step(&mut cpu, &mut mem);
        let ins = Decoder::new(32, &insn, DecoderOptions::NONE).decode();
        assert_eq!(exit, Exit::Continue, "{:02x?} {:?}", insn, ins.mnemonic());
        let mut st = State::default();
        for i in 0..8 {
            st.r[i] = before.regs[i] as u64;
            st.xmm[i] = before.xmm[i].to_le_bytes();
            st.mm[i] = before.mm[i];
        }
        st.flags = before.eflags as u64;
        native.run(&insn, &mut st);
        let mut ok = (st.flags as u32 & STATUS) == (cpu.eflags & STATUS);
        for i in 0..8 {
            if i != 4 && st.r[i] as u32 != cpu.regs[i] { ok = false; }
            if !lanes_equal(u128::from_le_bytes(st.xmm[i]), cpu.xmm[i], op.3) { ok = false; }
            if st.mm[i] != cpu.mm[i] { ok = false; }
        }
        if !ok && failures.len() < 25 {
            let diff: Vec<String> = (0..8)
                .filter(|&i| u128::from_le_bytes(st.xmm[i]) != cpu.xmm[i])
                .map(|i| format!("xmm{} in {:032x} want {:032x} got {:032x}", i, before.xmm[i], u128::from_le_bytes(st.xmm[i]), cpu.xmm[i]))
                .chain((0..8).filter(|&i| st.mm[i] != cpu.mm[i]).map(|i| format!("mm{} in {:016x} want {:016x} got {:016x}", i, before.mm[i], st.mm[i], cpu.mm[i])))
                .chain((0..8).filter(|&i| i != 4 && st.r[i] as u32 != cpu.regs[i]).map(|i| format!("r{} want {:08x} got {:08x}", i, st.r[i] as u32, cpu.regs[i])))
                .collect();
            failures.push(format!("{:02x?} {:?} flags want {:03x} got {:03x}\n  {}", insn, ins.mnemonic(),
                st.flags as u32 & STATUS, cpu.eflags & STATUS, diff.join("\n  ")));
        }
        done += 1;
    }
    assert!(failures.is_empty(), "{} checked, mismatches:\n{}", done, failures.join("\n"));
}
