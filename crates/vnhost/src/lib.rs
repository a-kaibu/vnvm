//! Host runtime for running Wine's unix side as wasm32 (Hangover-style vnvm).
//!
//! * `fd`  – in-process pipes and unix sockets shared by the Wine client and wineserver
//! * `mem` – mmap for one linear memory with the guest address space below 2 GiB
//! * `dl`  – dlopen/dlsym over unix libraries that are statically linked in
//! * `cpu` – the x86 run loop the signal layer drives
//! * `proc` – Wine processes: address partitions, environments, exit
//! * `wrap` – the libc entry points (`-Wl,--wrap=...`) that route to the above

pub mod cpu;
pub mod display;
pub mod dl;
pub mod fd;
pub mod mem;
pub mod proc;
mod wrap;

pub(crate) fn set_errno(e: libc::c_int) {
    unsafe { *libc::__errno_location() = e };
}
