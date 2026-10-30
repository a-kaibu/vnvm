//! A small WebAssembly binary encoder: just what the JIT emits.

use alloc::vec::Vec;

pub const I32: u8 = 0x7f;
pub const I64: u8 = 0x7e;
pub const F64: u8 = 0x7c;

/// Function bodies are written straight into a byte vector.
#[derive(Default)]
pub struct Code {
    pub b: Vec<u8>,
}

pub fn uleb(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let c = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(c);
            return;
        }
        out.push(c | 0x80);
    }
}

pub fn sleb(out: &mut Vec<u8>, mut v: i64) {
    loop {
        let c = (v & 0x7f) as u8;
        v >>= 7;
        let done = (v == 0 && c & 0x40 == 0) || (v == -1 && c & 0x40 != 0);
        out.push(if done { c } else { c | 0x80 });
        if done {
            return;
        }
    }
}

fn name(out: &mut Vec<u8>, s: &str) {
    uleb(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

fn section(out: &mut Vec<u8>, id: u8, body: &[u8]) {
    out.push(id);
    uleb(out, body.len() as u64);
    out.extend_from_slice(body);
}

#[allow(dead_code)]
impl Code {
    #[inline]
    pub fn op(&mut self, o: u8) -> &mut Self {
        self.b.push(o);
        self
    }
    pub fn i32c(&mut self, v: i32) -> &mut Self {
        self.b.push(0x41);
        sleb(&mut self.b, v as i64);
        self
    }
    pub fn u32c(&mut self, v: u32) -> &mut Self {
        self.i32c(v as i32)
    }
    pub fn i64c(&mut self, v: i64) -> &mut Self {
        self.b.push(0x42);
        sleb(&mut self.b, v);
        self
    }
    pub fn get(&mut self, l: u32) -> &mut Self {
        self.b.push(0x20);
        uleb(&mut self.b, l as u64);
        self
    }
    pub fn set(&mut self, l: u32) -> &mut Self {
        self.b.push(0x21);
        uleb(&mut self.b, l as u64);
        self
    }
    pub fn tee(&mut self, l: u32) -> &mut Self {
        self.b.push(0x22);
        uleb(&mut self.b, l as u64);
        self
    }
    fn mem(&mut self, o: u8, align: u32, off: u32) -> &mut Self {
        self.b.push(o);
        uleb(&mut self.b, align as u64);
        uleb(&mut self.b, off as u64);
        self
    }
    pub fn load32(&mut self, off: u32) -> &mut Self {
        self.mem(0x28, 0, off)
    }
    pub fn load64(&mut self, off: u32) -> &mut Self {
        self.mem(0x29, 0, off)
    }
    pub fn load8u(&mut self, off: u32) -> &mut Self {
        self.mem(0x2d, 0, off)
    }
    pub fn load8s(&mut self, off: u32) -> &mut Self {
        self.mem(0x2c, 0, off)
    }
    pub fn load16u(&mut self, off: u32) -> &mut Self {
        self.mem(0x2f, 0, off)
    }
    pub fn load16s(&mut self, off: u32) -> &mut Self {
        self.mem(0x2e, 0, off)
    }
    pub fn f32load(&mut self, off: u32) -> &mut Self {
        self.mem(0x2a, 0, off)
    }
    pub fn f64load(&mut self, off: u32) -> &mut Self {
        self.mem(0x2b, 0, off)
    }
    pub fn f32store(&mut self, off: u32) -> &mut Self {
        self.mem(0x38, 0, off)
    }
    pub fn f64store(&mut self, off: u32) -> &mut Self {
        self.mem(0x39, 0, off)
    }
    pub fn store32(&mut self, off: u32) -> &mut Self {
        self.mem(0x36, 0, off)
    }
    pub fn store64(&mut self, off: u32) -> &mut Self {
        self.mem(0x37, 0, off)
    }
    pub fn store8(&mut self, off: u32) -> &mut Self {
        self.mem(0x3a, 0, off)
    }
    pub fn store16(&mut self, off: u32) -> &mut Self {
        self.mem(0x3b, 0, off)
    }
    /// block / loop / if with an empty type
    pub fn block(&mut self) -> &mut Self {
        self.b.extend_from_slice(&[0x02, 0x40]);
        self
    }
    pub fn loop_(&mut self) -> &mut Self {
        self.b.extend_from_slice(&[0x03, 0x40]);
        self
    }
    pub fn if_(&mut self) -> &mut Self {
        self.b.extend_from_slice(&[0x04, 0x40]);
        self
    }
    /// `if` producing an i32
    pub fn if_i32(&mut self) -> &mut Self {
        self.b.extend_from_slice(&[0x04, I32]);
        self
    }
    pub fn else_(&mut self) -> &mut Self {
        self.op(0x05)
    }
    pub fn end(&mut self) -> &mut Self {
        self.op(0x0b)
    }
    pub fn br(&mut self, depth: u32) -> &mut Self {
        self.b.push(0x0c);
        uleb(&mut self.b, depth as u64);
        self
    }
    pub fn br_if(&mut self, depth: u32) -> &mut Self {
        self.b.push(0x0d);
        uleb(&mut self.b, depth as u64);
        self
    }
    pub fn ret(&mut self) -> &mut Self {
        self.op(0x0f)
    }
    pub fn call(&mut self, f: u32) -> &mut Self {
        self.b.push(0x10);
        uleb(&mut self.b, f as u64);
        self
    }
    pub fn call_indirect(&mut self, ty: u32, table: u32) -> &mut Self {
        self.b.push(0x11);
        uleb(&mut self.b, ty as u64);
        uleb(&mut self.b, table as u64);
        self
    }
    pub fn drop_(&mut self) -> &mut Self {
        self.op(0x1a)
    }
    pub fn select(&mut self) -> &mut Self {
        self.op(0x1b)
    }
    // i32 arithmetic
    pub fn eqz(&mut self) -> &mut Self {
        self.op(0x45)
    }
    pub fn eq(&mut self) -> &mut Self {
        self.op(0x46)
    }
    pub fn ne(&mut self) -> &mut Self {
        self.op(0x47)
    }
    pub fn lt_s(&mut self) -> &mut Self {
        self.op(0x48)
    }
    pub fn lt_u(&mut self) -> &mut Self {
        self.op(0x49)
    }
    pub fn gt_s(&mut self) -> &mut Self {
        self.op(0x4a)
    }
    pub fn gt_u(&mut self) -> &mut Self {
        self.op(0x4b)
    }
    pub fn le_s(&mut self) -> &mut Self {
        self.op(0x4c)
    }
    pub fn le_u(&mut self) -> &mut Self {
        self.op(0x4d)
    }
    pub fn ge_s(&mut self) -> &mut Self {
        self.op(0x4e)
    }
    pub fn ge_u(&mut self) -> &mut Self {
        self.op(0x4f)
    }
    pub fn add(&mut self) -> &mut Self {
        self.op(0x6a)
    }
    pub fn sub(&mut self) -> &mut Self {
        self.op(0x6b)
    }
    pub fn mul(&mut self) -> &mut Self {
        self.op(0x6c)
    }
    pub fn and(&mut self) -> &mut Self {
        self.op(0x71)
    }
    pub fn or(&mut self) -> &mut Self {
        self.op(0x72)
    }
    pub fn xor(&mut self) -> &mut Self {
        self.op(0x73)
    }
    pub fn shl(&mut self) -> &mut Self {
        self.op(0x74)
    }
    pub fn shr_s(&mut self) -> &mut Self {
        self.op(0x75)
    }
    pub fn shr_u(&mut self) -> &mut Self {
        self.op(0x76)
    }
    pub fn rotl(&mut self) -> &mut Self {
        self.op(0x77)
    }
    pub fn rotr(&mut self) -> &mut Self {
        self.op(0x78)
    }
    pub fn extend8_s(&mut self) -> &mut Self {
        self.op(0xc0)
    }
    pub fn extend16_s(&mut self) -> &mut Self {
        self.op(0xc1)
    }
    // i64
    pub fn i64_extend_u(&mut self) -> &mut Self {
        self.op(0xad)
    }
    pub fn i64_extend_s(&mut self) -> &mut Self {
        self.op(0xac)
    }
    pub fn i64_mul(&mut self) -> &mut Self {
        self.op(0x7e)
    }
    pub fn i64_shr_u(&mut self) -> &mut Self {
        self.op(0x88)
    }
    pub fn wrap(&mut self) -> &mut Self {
        self.op(0xa7)
    }
    /// memory.copy within memory 0
    pub fn memory_copy(&mut self) -> &mut Self {
        self.b.extend_from_slice(&[0xfc, 10, 0, 0]);
        self
    }
    pub fn memory_fill(&mut self) -> &mut Self {
        self.b.extend_from_slice(&[0xfc, 11, 0]);
        self
    }
}

pub struct FuncType {
    pub params: Vec<u8>,
    pub results: Vec<u8>,
}

pub enum Import {
    /// env.memory: shared, up to 4GB
    Memory,
    /// env.<name>: a funcref table
    Table(&'static str),
    Func(&'static str, u32),
}

pub struct Func {
    pub ty: u32,
    /// local types beyond the parameters
    pub locals: Vec<u8>,
    pub code: Code,
}

/// Builds a module: types, imports, functions exported as "f0", "f1", ...
#[derive(Default)]
pub struct Module {
    pub types: Vec<FuncType>,
    pub imports: Vec<Import>,
    pub funcs: Vec<Func>,
}

impl Module {
    pub fn imported_funcs(&self) -> u32 {
        self.imports.iter().filter(|i| matches!(i, Import::Func(..))).count() as u32
    }

    pub fn finish(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"\0asm\x01\0\0\0");
        let mut s = Vec::new();
        uleb(&mut s, self.types.len() as u64);
        for t in &self.types {
            s.push(0x60);
            uleb(&mut s, t.params.len() as u64);
            s.extend_from_slice(&t.params);
            uleb(&mut s, t.results.len() as u64);
            s.extend_from_slice(&t.results);
        }
        section(&mut out, 1, &s);

        s.clear();
        uleb(&mut s, self.imports.len() as u64);
        for i in &self.imports {
            name(&mut s, "env");
            match i {
                Import::Memory => {
                    name(&mut s, "memory");
                    // memory, shared with a maximum: 0..65536 pages
                    s.extend_from_slice(&[0x02, 0x03, 0x00]);
                    uleb(&mut s, 65536);
                }
                Import::Table(n) => {
                    name(&mut s, n);
                    s.extend_from_slice(&[0x01, 0x70, 0x00, 0x00]);
                }
                Import::Func(n, ty) => {
                    name(&mut s, n);
                    s.push(0x00);
                    uleb(&mut s, *ty as u64);
                }
            }
        }
        section(&mut out, 2, &s);

        s.clear();
        uleb(&mut s, self.funcs.len() as u64);
        for f in &self.funcs {
            uleb(&mut s, f.ty as u64);
        }
        section(&mut out, 3, &s);

        s.clear();
        let base = self.imported_funcs();
        uleb(&mut s, self.funcs.len() as u64);
        for i in 0..self.funcs.len() {
            let mut n = Vec::from(&b"f"[..]);
            let mut d = Vec::new();
            let mut v = i;
            loop {
                d.push(b'0' + (v % 10) as u8);
                v /= 10;
                if v == 0 {
                    break;
                }
            }
            d.reverse();
            n.extend_from_slice(&d);
            uleb(&mut s, n.len() as u64);
            s.extend_from_slice(&n);
            s.push(0x00);
            uleb(&mut s, base as u64 + i as u64);
        }
        section(&mut out, 7, &s);

        s.clear();
        uleb(&mut s, self.funcs.len() as u64);
        for f in &self.funcs {
            let mut body = Vec::new();
            // locals, run-length encoded
            let mut groups: Vec<(u32, u8)> = Vec::new();
            for &t in &f.locals {
                match groups.last_mut() {
                    Some((n, ty)) if *ty == t => *n += 1,
                    _ => groups.push((1, t)),
                }
            }
            uleb(&mut body, groups.len() as u64);
            for (n, t) in groups {
                uleb(&mut body, n as u64);
                body.push(t);
            }
            body.extend_from_slice(&f.code.b);
            body.push(0x0b);
            uleb(&mut s, body.len() as u64);
            s.extend_from_slice(&body);
        }
        section(&mut out, 10, &s);
        out
    }
}
