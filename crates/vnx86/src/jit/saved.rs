//! Regions kept across runs: a module of region functions (see [`super::wasm::merge`]) and an
//! index saying, for function i, the roots it was built from, its block starts and the guest
//! code it was translated from (used only if that code is still there).
//!
//! Index: "VNJ1", count, then per function: roots, starts (address, block), code (address,
//! length) as u32-counted lists of u32s, then the code bytes (u32 length first).

use alloc::vec::Vec;

#[derive(Clone, PartialEq, Eq)]
pub struct Entry {
    pub roots: Vec<u32>,
    pub starts: Vec<(u32, u32)>,
    pub code: Vec<(u32, u32)>,
    /// the guest code bytes of `code`, concatenated
    pub text: Vec<u8>,
}

impl Entry {
    /// "x86_<first root>_<roots>", the name profilers show
    pub fn name(&self) -> alloc::string::String {
        alloc::format!("x86_{:08x}_{}", self.roots[0], self.roots.len())
    }
}

pub fn encode(entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::from(&b"VNJ1"[..]);
    let u = |out: &mut Vec<u8>, v: u32| out.extend_from_slice(&v.to_le_bytes());
    u(&mut out, entries.len() as u32);
    for e in entries {
        u(&mut out, e.roots.len() as u32);
        for &r in &e.roots {
            u(&mut out, r);
        }
        for list in [&e.starts, &e.code] {
            u(&mut out, list.len() as u32);
            for &(a, b) in list {
                u(&mut out, a);
                u(&mut out, b);
            }
        }
        u(&mut out, e.text.len() as u32);
        out.extend_from_slice(&e.text);
    }
    out
}

struct Reader<'a>(&'a [u8], usize);

impl Reader<'_> {
    fn u(&mut self) -> Option<u32> {
        let v = u32::from_le_bytes(self.0.get(self.1..self.1 + 4)?.try_into().ok()?);
        self.1 += 4;
        Some(v)
    }
    fn pairs(&mut self) -> Option<Vec<(u32, u32)>> {
        (0..self.u()?).map(|_| Some((self.u()?, self.u()?))).collect()
    }
}

pub fn decode(b: &[u8]) -> Option<Vec<Entry>> {
    if b.get(..4)? != b"VNJ1" {
        return None;
    }
    let mut r = Reader(b, 4);
    (0..r.u()?)
        .map(|_| {
            let roots = (0..r.u()?).map(|_| r.u()).collect::<Option<Vec<_>>>()?;
            let (starts, code) = (r.pairs()?, r.pairs()?);
            let n = r.u()? as usize;
            let text = b.get(r.1..r.1 + n)?.to_vec();
            r.1 += n;
            Some(Entry { roots, starts, code, text })
        })
        .collect()
}
