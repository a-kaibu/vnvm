//! Translates some code with the JIT and writes the wasm modules (for `node` to validate):
//!   cargo run -p vnx86 --example jitdump -- out_dir [file offset len]
//! Without a file, translates a built-in sample of common instructions.
use iced_x86::code_asm::*;
use vnx86::{Fault, Memory};

struct Flat {
    base: u32,
    data: Vec<u8>,
}

impl Memory for Flat {
    fn read8(&mut self, addr: u32) -> Result<u8, Fault> {
        self.data.get(addr.wrapping_sub(self.base) as usize).copied().ok_or(Fault { addr, write: false })
    }
    fn write8(&mut self, addr: u32, _v: u8) -> Result<(), Fault> {
        Err(Fault { addr, write: true })
    }
}

fn sample() -> Vec<u8> {
    let mut a = CodeAssembler::new(32).unwrap();
    let mut top = a.create_label();
    a.set_label(&mut top).unwrap();
    a.mov(eax, dword_ptr(esi + ecx * 4 + 8)).unwrap();
    a.movzx(edx, byte_ptr(edi)).unwrap();
    a.xor(eax, edx).unwrap();
    a.shr(eax, 8).unwrap();
    a.add(ebx, eax).unwrap();
    a.adc(edx, 0).unwrap();
    a.imul_3(eax, ebx, 3).unwrap();
    a.lea(edi, ptr(edi + 1)).unwrap();
    a.push(eax).unwrap();
    a.pop(edx).unwrap();
    a.sete(al).unwrap();
    a.cmovl(ecx, edx).unwrap();
    a.cpuid().unwrap();
    a.inc(ecx).unwrap();
    a.cmp(ecx, 100).unwrap();
    a.jl(top).unwrap();
    a.ret().unwrap();
    a.assemble(0x401000).unwrap()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let out = std::path::PathBuf::from(&args[1]);
    std::fs::create_dir_all(&out).unwrap();
    let (base, data) = match args.get(2) {
        Some(f) => {
            let all = std::fs::read(f).unwrap();
            let off: usize = args[3].parse().unwrap();
            let len: usize = args[4].parse().unwrap();
            (0x401000, all[off..off + len].to_vec())
        }
        None => (0x401000, sample()),
    };
    let mut mem = Flat { base, data };
    let mut m = vnx86::jit::wasm::Module::default();
    let mut eip = base;
    let end = base + mem.data.len() as u32;
    let mut n = 0;
    while eip < end {
        match vnx86::jit::translate(&mut m, &mut mem, eip, 0x1000) {
            Some(b) => {
                n += 1;
                eip = b.end;
            }
            None => eip += 1,
        }
    }
    std::fs::write(out.join("blocks.wasm"), m.finish()).unwrap();
    std::fs::write(out.join("dispatch.wasm"), vnx86::jit::dispatch::module()).unwrap();
    eprintln!("{n} blocks");
}
