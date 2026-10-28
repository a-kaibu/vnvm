//! Runs real functions from vnbox's i386 ntdll.dll (Wine 11.20) on the interpreter.
//! Skipped when the DLL is not present (it lives in the vnvm image build tree).

use vnx86::{Cpu, Exit, Fault, Memory, ESP};

const NTDLL: &str = "/home/user/img/rootfs/opt/wine-vnbox/lib/wine/i386-windows/ntdll.dll";
const RETURN_TRAP: u32 = 0x0000_f000;
const STACK_TOP: u32 = 0x0010_0000;
const TEB: u32 = 0x0020_0000;
const HEAP: u32 = 0x0030_0000;

/// 4 GiB address space backed by lazily allocated 4 KiB pages.
struct Sparse {
    pages: Vec<Option<Box<[u8; 4096]>>>,
}

impl Sparse {
    fn new() -> Self {
        Sparse { pages: (0..1 << 20).map(|_| None).collect() }
    }
    fn map(&mut self, addr: u32, len: u32) {
        for p in addr >> 12..=(addr + len.max(1) - 1) >> 12 {
            self.pages[p as usize].get_or_insert_with(|| Box::new([0; 4096]));
        }
    }
    fn put(&mut self, addr: u32, data: &[u8]) {
        self.map(addr, data.len() as u32);
        for (i, b) in data.iter().enumerate() {
            self.write8(addr + i as u32, *b).unwrap();
        }
    }
    fn get(&mut self, addr: u32, len: usize) -> Vec<u8> {
        (0..len).map(|i| self.read8(addr + i as u32).unwrap()).collect()
    }
    fn cstr(&mut self, addr: u32) -> String {
        let mut v = Vec::new();
        let mut a = addr;
        while let Ok(b) = self.read8(a) {
            if b == 0 {
                break;
            }
            v.push(b);
            a += 1;
        }
        String::from_utf8_lossy(&v).into_owned()
    }
}

impl Memory for Sparse {
    #[inline]
    fn read8(&mut self, addr: u32) -> Result<u8, Fault> {
        match &self.pages[(addr >> 12) as usize] {
            Some(p) => Ok(p[(addr & 0xfff) as usize]),
            None => Err(Fault { addr, write: false }),
        }
    }
    #[inline]
    fn write8(&mut self, addr: u32, v: u8) -> Result<(), Fault> {
        match &mut self.pages[(addr >> 12) as usize] {
            Some(p) => {
                p[(addr & 0xfff) as usize] = v;
                Ok(())
            }
            None => Err(Fault { addr, write: true }),
        }
    }
}

fn u16_at(b: &[u8], o: usize) -> u32 {
    u16::from_le_bytes([b[o], b[o + 1]]) as u32
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

struct Image {
    base: u32,
    exports: std::collections::HashMap<String, u32>,
}

/// Maps a PE image at its preferred base (ntdll has no imports to resolve).
fn load(mem: &mut Sparse, file: &[u8]) -> Image {
    let pe = u32_at(file, 0x3c) as usize;
    assert_eq!(&file[pe..pe + 4], b"PE\0\0");
    let nsec = u16_at(file, pe + 6) as usize;
    let opt = pe + 24;
    let base = u32_at(file, opt + 28);
    let image_size = u32_at(file, opt + 56);
    let headers = u32_at(file, opt + 60) as usize;
    mem.map(base, image_size);
    mem.put(base, &file[..headers]);
    let sec = opt + u16_at(file, pe + 20) as usize;
    for i in 0..nsec {
        let s = sec + i * 40;
        let va = u32_at(file, s + 12);
        let raw_size = u32_at(file, s + 16) as usize;
        let raw = u32_at(file, s + 20) as usize;
        if raw_size > 0 {
            mem.put(base + va, &file[raw..raw + raw_size]);
        }
    }
    let exp_rva = u32_at(file, opt + 96);
    let mut exports = std::collections::HashMap::new();
    let d = mem.get(base + exp_rva, 40);
    let (nfun, nnames) = (u32_at(&d, 20), u32_at(&d, 24));
    let (funcs, names, ords) = (u32_at(&d, 28), u32_at(&d, 32), u32_at(&d, 36));
    for i in 0..nnames {
        let name_rva = mem.read32(base + names + i * 4).unwrap();
        let ord = mem.read16(base + ords + i * 2).unwrap() as u32;
        assert!(ord < nfun);
        let rva = mem.read32(base + funcs + ord * 4).unwrap();
        let name = mem.cstr(base + name_rva);
        exports.insert(name, base + rva);
    }
    Image { base, exports }
}

struct Machine {
    mem: Sparse,
    cpu: Cpu,
    ntdll: Image,
    heap: u32,
    steps: u64,
}

impl Machine {
    fn new() -> Option<Self> {
        let file = std::fs::read(NTDLL).ok()?;
        let mut mem = Sparse::new();
        let ntdll = load(&mut mem, &file);
        mem.map(STACK_TOP - 0x10000, 0x10000);
        mem.map(TEB, 0x2000);
        mem.write32(TEB + 0x18, TEB).unwrap(); // NtTib.Self
        mem.map(HEAP, 0x10000);
        mem.map(RETURN_TRAP, 1);
        let mut cpu = Cpu::default();
        cpu.fs_base = TEB;
        cpu.trap_lo = RETURN_TRAP;
        cpu.trap_hi = RETURN_TRAP + 1;
        Some(Machine { mem, cpu, ntdll, heap: HEAP, steps: 0 })
    }

    fn alloc(&mut self, data: &[u8]) -> u32 {
        let a = self.heap;
        self.mem.put(a, data);
        self.heap = (a + data.len() as u32 + 15) & !15;
        a
    }

    /// Calls `func` with 32-bit stack arguments; works for both cdecl and stdcall.
    fn call(&mut self, func: &str, args: &[u32]) -> u32 {
        let entry = *self.ntdll.exports.get(func).unwrap_or_else(|| panic!("no export {func}"));
        self.call_addr(entry, args)
    }

    fn call_addr(&mut self, entry: u32, args: &[u32]) -> u32 {
        let mut sp = STACK_TOP - 0x100;
        for a in args.iter().rev() {
            sp -= 4;
            self.mem.write32(sp, *a).unwrap();
        }
        sp -= 4;
        self.mem.write32(sp, RETURN_TRAP).unwrap();
        self.cpu.regs[ESP] = sp;
        self.cpu.eip = entry;
        loop {
            self.steps += 1;
            match vnx86::step(&mut self.cpu, &mut self.mem) {
                Exit::Continue => {}
                Exit::Trap(RETURN_TRAP) => return self.cpu.regs[0],
                e => panic!("{func:x}: {e:?} at eip {:#x} (ntdll+{:#x})", self.cpu.eip, self.cpu.eip.wrapping_sub(self.ntdll.base), func = entry),
            }
        }
    }
}

#[test]
fn ntdll_functions_run_on_the_interpreter() {
    let Some(mut m) = Machine::new() else {
        eprintln!("skipping: {NTDLL} not found");
        return;
    };
    let t0 = std::time::Instant::now();

    let text = m.alloc(b"The quick brown fox jumps over the lazy dog");
    assert_eq!(m.call("RtlComputeCrc32", &[0, text, 43]), 0x414fa339);
    assert_eq!(m.call("strlen", &[text]), 43);

    let buf = m.alloc(&[0; 256]);
    let fmt = m.alloc(b"%d %x %-5s| %08X %I64d\0");
    let s = m.alloc(b"abc\0");
    // (ntdll's printf has no floating-point conversions)
    let n = m.call("sprintf", &[buf, fmt, -42i32 as u32, 255, s, 0xbeef, 0x89abcdef, 0x1234]);
    let out = m.mem.cstr(buf);
    assert_eq!(out, "-42 ff abc  | 0000BEEF 20016857337327");
    assert_eq!(n as usize, out.len());

    // sscanf into an int, a hex int and a string
    let input = m.alloc(b"123 0x7f word\0");
    let sfmt = m.alloc(b"%d %x %15s\0");
    let iv = m.alloc(&[0; 8]);
    let sv = m.alloc(&[0; 16]);
    assert_eq!(m.call("sscanf", &[input, sfmt, iv, iv + 4, sv]), 3);
    assert_eq!(m.mem.read32(iv).unwrap(), 123);
    assert_eq!(m.mem.read32(iv + 4).unwrap(), 0x7f);
    assert_eq!(m.mem.cstr(sv), "word");

    // 64-bit division helpers (stdcall, args are two u64)
    let (a, b) = (-1_000_000_000_007i64, 13i64);
    let r = m.call("_alldiv", &[a as u32, (a >> 32) as u32, b as u32, (b >> 32) as u32]);
    let q = r as u64 | (m.cpu.regs[2] as u64) << 32;
    assert_eq!(q as i64, a / b);

    // qsort with a guest-side comparator: mov eax,[esp+4]; mov eax,[eax]; mov ecx,[esp+8]; sub eax,[ecx]; ret
    let cmp = m.alloc(&[0x8b, 0x44, 0x24, 0x04, 0x8b, 0x00, 0x8b, 0x4c, 0x24, 0x08, 0x2b, 0x01, 0xc3]);
    let vals: [i32; 8] = [5, -3, 99, 0, 42, 7, -100, 8];
    let arr = m.alloc(&vals.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>());
    m.call("qsort", &[arr, 8, 4, cmp]);
    let mut sorted = vals;
    sorted.sort();
    let got: Vec<i32> = (0..8).map(|i| m.mem.read32(arr + i * 4).unwrap() as i32).collect();
    assert_eq!(got, sorted);

    // libm on x87/SSE: results come back in st(0)
    let x = 2.0f64.to_bits();
    m.call("sqrt", &[x as u32, (x >> 32) as u32]);
    assert_eq!(m.cpu.fpu.st(0), 2.0f64.sqrt());
    let (bx, ex) = (1.5f64.to_bits(), 10.0f64.to_bits());
    m.call("pow", &[bx as u32, (bx >> 32) as u32, ex as u32, (ex >> 32) as u32]);
    assert!((m.cpu.fpu.st(0) - 1.5f64.powi(10)).abs() < 1e-9);

    let dt = t0.elapsed();
    eprintln!("{} instructions in {:?} ({:.1} M/s)", m.steps, dt, m.steps as f64 / dt.as_secs_f64() / 1e6);
}
