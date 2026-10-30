//! The dispatcher module: `run(cpu) -> i32` looks `cpu.eip` up in the block hash table and
//! calls the block through the `env.table` funcref table, until a block returns non-zero, an
//! address has no block (returns [`super::RET_INTERP`]), a signal is pending or
//! `cpu.jit_budget` runs out (returns 0).
//!
//! The hash table (owned by the embedder) is `cpu.jit_mask + 1` entries of (eip, table index);
//! eip 0 marks a free entry. Entries are found by linear probing from [`slot`].

use super::wasm::{Code, Func, FuncType, Import, Module, I32};
use super::Off;
use crate::Cpu;
use core::mem::offset_of;

/// First probe position of `eip` in a table of `mask + 1` entries.
#[inline]
pub fn slot(eip: u32, mask: u32) -> u32 {
    (eip.wrapping_mul(0x9e37_79b1) >> 15) & mask
}

pub fn module() -> alloc::vec::Vec<u8> {
    let mut m = Module::default();
    m.types.push(FuncType { params: alloc::vec![I32], results: alloc::vec![I32] });
    m.imports.push(Import::Memory);
    m.imports.push(Import::Table("table"));
    let hash = offset_of!(Cpu, jit_hash) as u32;
    let maskf = offset_of!(Cpu, jit_mask) as u32;
    let pending = offset_of!(Cpu, jit_pending) as u32;
    let (cpu, eip, h, e, st, mask) = (0, 1, 2, 3, 4, 5);
    let mut c = Code::default();
    c.get(cpu).load32(maskf).set(mask);
    c.loop_();
    {
        // budget and signals
        c.get(cpu).get(cpu).load32(Off::budget()).i32c(1).sub().store32(Off::budget());
        c.get(cpu).load32(Off::budget()).i32c(0).le_s();
        c.get(cpu).load32(pending).load32(0).or();
        c.if_().i32c(0).ret().end();
        c.get(cpu).load32(Off::eip()).set(eip);
        c.get(eip).u32c(0x9e37_79b1).mul().i32c(15).shr_u().get(mask).and().set(h);
        c.block();
        c.loop_();
        {
            c.get(cpu).load32(hash).get(h).i32c(3).shl().add().tee(e).load32(0);
            c.get(eip).eq().br_if(1);
            c.get(e).load32(0).eqz().if_().i32c(super::RET_INTERP).ret().end();
            c.get(h).i32c(1).add().get(mask).and().set(h);
            c.br(0);
        }
        c.end();
        c.end();
        c.get(cpu).get(e).load32(4).call_indirect(0, 0).tee(st);
        c.if_().get(st).ret().end();
        c.br(0);
    }
    c.end();
    c.i32c(0);
    m.funcs.push(Func { ty: 0, locals: alloc::vec![I32; 5], code: c });
    m.finish()
}
