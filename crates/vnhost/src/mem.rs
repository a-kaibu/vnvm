//! mmap and friends for a single wasm linear memory.
//!
//! The guest (Windows) address space is [0, GUEST_END): whatever Wine asks for there is handed
//! out at exactly that address. Everything else (emscripten's heap, stacks, Wine's own unix-side
//! allocations) lives above GUEST_END and goes to emscripten's mmap.
//!
//! The guest space is split between Wine processes (see `proc`); a process can only map its
//! own part, plus the shared page of KUSER_SHARED_DATA.
//!
//! wasm memory can't alias, so MAP_SHARED file mappings share one backing buffer per file:
//! a second shared mapping of the same file returns the same address when it can, and falls
//! back to a copy (with a warning) when a different fixed address is required.
//!
//! wineserver's own shared mappings (the session's shared memory) go to a window of the guest
//! space that every process may see ([`SHARED_LO`], USER_SHARED_DATA), with room to grow in
//! place; Wine processes map views of them at that address (`vnhost_shared_view`).

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use libc::{c_int, c_void, off_t, size_t};

use crate::proc::{HOST_PID, SHARED_LO, USER_SHARED_DATA};
use crate::set_errno;

pub const GUEST_END: usize = 0x8000_0000;
const PAGE: usize = 4096;
const MAP_FIXED_NOREPLACE: c_int = 0x100000;
/// Room a wineserver mapping gets in the shared window (the session mapping grows).
const WINDOW_SLOT: usize = 0x0200_0000;

extern "C" {
    fn __real_mmap(addr: *mut c_void, len: size_t, prot: c_int, flags: c_int, fd: c_int, off: off_t) -> *mut c_void;
    fn __real_munmap(addr: *mut c_void, len: size_t) -> c_int;
}

/// Guest pages that may hold data (were mapped accessible since they were last zeroed).
static DIRTY: [AtomicU64; GUEST_END / PAGE / 64] = [const { AtomicU64::new(0) }; GUEST_END / PAGE / 64];

fn mark(addr: usize, len: usize, dirty: bool) {
    for p in addr / PAGE..(addr + len).div_ceil(PAGE) {
        let bit = 1u64 << (p % 64);
        if dirty {
            DIRTY[p / 64].fetch_or(bit, Ordering::Relaxed);
        } else {
            DIRTY[p / 64].fetch_and(!bit, Ordering::Relaxed);
        }
    }
}

/// Zeroes the dirty pages of a guest range.
unsafe fn zero(addr: usize, len: usize) {
    let mut p = addr / PAGE;
    let end = (addr + len).div_ceil(PAGE);
    while p < end {
        if DIRTY[p / 64].load(Ordering::Relaxed) & (1 << (p % 64)) == 0 {
            p += 1;
            continue;
        }
        let start = p;
        while p < end && DIRTY[p / 64].load(Ordering::Relaxed) & (1 << (p % 64)) != 0 {
            p += 1;
        }
        let a = (start * PAGE).max(addr);
        let b = (p * PAGE).min(addr + len);
        std::ptr::write_bytes(a as *mut u8, 0, b - a);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct FileKey(u64, u64);

struct Backing {
    /// the process whose memory holds it (guest addresses are only valid in their process)
    pid: i32,
    /// address of file offset `off`
    addr: usize,
    off: u64,
    len: usize,
    /// bytes reserved at `addr` (the mapping may grow up to this)
    cap: usize,
    maps: u32,
}

struct Mapping {
    pid: i32,
    len: usize,
    key: FileKey,
    shared_writable: bool,
}

#[derive(Default)]
struct State {
    backings: HashMap<FileKey, Backing>,
    /// shared file mappings by address
    maps: BTreeMap<usize, Mapping>,
    /// next free address of the shared window
    window: usize,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Default::default))
}

fn in_guest(addr: usize, len: usize) -> bool {
    addr != 0 && addr.checked_add(len).is_some_and(|e| e <= GUEST_END)
}

unsafe fn host_alloc(len: usize) -> *mut c_void {
    __real_mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0)
}

unsafe fn fill_from_file(fd: c_int, dst: usize, len: usize, off: off_t) {
    let mut done = 0;
    while done < len {
        let n = libc::pread(fd, (dst + done) as *mut c_void, len - done, off + done as off_t);
        if n <= 0 {
            break;
        }
        done += n as usize;
    }
}

pub unsafe fn mmap(addr: *mut c_void, len: size_t, prot: c_int, flags: c_int, fd: c_int, off: off_t) -> *mut c_void {
    let r = mmap_(addr, len, prot, flags, fd, off);
    if flags & libc::MAP_SHARED != 0 && fd >= 0 {
        crate::proc::trace(format_args!("pid {} mmap shared {:p} len {:#x} prot {} fd {} off {:#x} -> {:p}", crate::proc::current(), addr, len, prot, fd, off, r));
    }
    r
}

unsafe fn mmap_(addr: *mut c_void, len: size_t, prot: c_int, flags: c_int, fd: c_int, off: off_t) -> *mut c_void {
    let len = len.div_ceil(PAGE) * PAGE;
    let want = addr as usize;
    let guest = in_guest(want, len) && (flags & libc::MAP_FIXED != 0 || want != 0);
    let anon = flags & libc::MAP_ANONYMOUS != 0 || fd < 0;
    if guest && !crate::proc::may_map(want, want + len) {
        // another process's part of the address space
        set_errno(if flags & libc::MAP_FIXED != 0 { libc::ENOMEM } else { libc::EEXIST });
        return libc::MAP_FAILED;
    }

    if want >= GUEST_END && flags & (libc::MAP_FIXED | MAP_FIXED_NOREPLACE) != 0 {
        // the host part of the memory can't be handed out at a chosen address
        set_errno(if flags & libc::MAP_FIXED != 0 { libc::ENOMEM } else { libc::EEXIST });
        return libc::MAP_FAILED;
    }
    if anon {
        if !guest {
            return __real_mmap(std::ptr::null_mut(), len, prot, flags & !libc::MAP_FIXED, -1, 0);
        }
        if prot == libc::PROT_NONE {
            // a reservation or decommit: discard contents, but leave shared file mappings alone
            // (wineserver maps some of them, like the user shared data, before Wine reserves
            // the surrounding address space)
            let mut keep = shared_ranges(want, len);
            keep.extend(usd_overlap(want, len));
            keep.sort_unstable();
            let mut a = want;
            for (lo, hi) in keep.iter().copied().chain([(want + len, want + len)]) {
                if lo > a {
                    zero(a, lo - a);
                    mark(a, lo - a, false);
                }
                a = a.max(hi);
            }
            return addr;
        }
        // the user shared data page belongs to wineserver: Wine maps anonymous memory there
        // first and the file over it, which must not wipe what is already in it
        for (lo, hi) in outside_usd(want, len) {
            unmap_shared(lo, hi - lo);
            zero(lo, hi - lo);
        }
        mark(want, len, true);
        return addr;
    }

    let mut st: libc::stat = std::mem::zeroed();
    if libc::fstat(fd, &mut st) != 0 {
        return libc::MAP_FAILED;
    }
    let key = FileKey(st.st_dev as u64, st.st_ino as u64);

    if flags & libc::MAP_SHARED != 0 {
        // reuse the file's backing buffer when the mapping fits inside it
        let pid = crate::proc::current();
        let reuse = with(|s| {
            let b = s.backings.get_mut(&key)?;
            if b.addr < SHARED_LO && b.pid != pid {
                return None;
            }
            let start = (off as u64).checked_sub(b.off)? as usize;
            if start + len > b.len {
                return None;
            }
            let at = b.addr + start;
            if guest && at != want {
                return None;
            }
            b.maps += 1;
            s.maps.insert(at, Mapping { pid, len, key, shared_writable: prot & libc::PROT_WRITE != 0 });
            Some(at)
        });
        if let Some(at) = reuse {
            return at as *mut c_void;
        }
        let exists = with(|s| s.backings.get(&key).is_some_and(|b| b.maps > 0));
        if exists && prot & libc::PROT_WRITE != 0 {
            eprintln!("vnhost: shared mapping of file {}:{} at {:#x} can't alias the existing one; copying", key.0, key.1, want);
        }
    }

    if !guest && flags & libc::MAP_SHARED != 0 && crate::proc::current() == HOST_PID {
        if let Some(at) = window_map(key, fd, len, off, prot) {
            return at as *mut c_void;
        }
    }

    let dst = if guest {
        unmap_shared(want, len);
        zero(want, len);
        mark(want, len, true);
        want
    } else {
        let p = host_alloc(len);
        if p == libc::MAP_FAILED {
            return p;
        }
        p as usize
    };
    fill_from_file(fd, dst, len, off);

    if flags & libc::MAP_SHARED != 0 {
        let pid = crate::proc::current();
        with(|s| {
            if !s.backings.get(&key).is_some_and(|b| b.maps > 0) {
                s.backings.insert(key, Backing { pid, addr: dst, off: off as u64, len, cap: len, maps: 0 });
            }
            if let Some(b) = s.backings.get_mut(&key) {
                if b.addr + (off as u64 - b.off.min(off as u64)) as usize == dst {
                    b.maps += 1;
                }
            }
            s.maps.insert(dst, Mapping { pid, len, key, shared_writable: prot & libc::PROT_WRITE != 0 });
        });
    }
    dst as *mut c_void
}

/// A wineserver mapping of a file in the shared window: next to the file's earlier mapping when
/// it continues it (a grown session), else in a new slot.
unsafe fn window_map(key: FileKey, fd: c_int, len: usize, off: off_t, prot: c_int) -> Option<usize> {
    let off = off as u64;
    let (at, fill) = with(|s| {
        if let Some(b) = s.backings.get_mut(&key) {
            if b.addr < SHARED_LO || off < b.off || (off - b.off) as usize + len > b.cap {
                return None;
            }
            let start = (off - b.off) as usize;
            let at = b.addr + start;
            // only the part past what is mapped already comes from the file
            let fill = (start + len).saturating_sub(b.len.max(start)).min(len);
            b.len = b.len.max(start + len);
            b.maps += 1;
            s.maps.insert(at, Mapping { pid: HOST_PID, len, key, shared_writable: prot & libc::PROT_WRITE != 0 });
            return Some((at, (at + len - fill, fill)));
        }
        let cap = len.max(WINDOW_SLOT).div_ceil(PAGE) * PAGE;
        let base = if s.window == 0 { SHARED_LO } else { s.window };
        if base + cap > USER_SHARED_DATA {
            return None;
        }
        s.window = base + cap;
        s.backings.insert(key, Backing { pid: HOST_PID, addr: base, off, len, cap, maps: 1 });
        s.maps.insert(base, Mapping { pid: HOST_PID, len, key, shared_writable: prot & libc::PROT_WRITE != 0 });
        Some((base, (base, len)))
    })?;
    if fill.1 > 0 {
        zero(fill.0, fill.1);
        mark(fill.0, fill.1, true);
        fill_from_file(fd, fill.0, fill.1, (off + (fill.0 - at) as u64) as off_t);
    }
    Some(at)
}

/// Where a Wine process sees [off, off+len) of a file that wineserver has mapped in the shared
/// window (NULL if it hasn't).
#[no_mangle]
pub unsafe extern "C" fn vnhost_shared_view(fd: c_int, off: u64, len: size_t) -> *mut c_void {
    let mut st: libc::stat = std::mem::zeroed();
    if libc::fstat(fd, &mut st) != 0 {
        return std::ptr::null_mut();
    }
    let key = FileKey(st.st_dev as u64, st.st_ino as u64);
    with(|s| {
        let b = s.backings.get(&key)?;
        if b.addr < SHARED_LO || off < b.off || (off - b.off) as usize + len > b.len {
            return None;
        }
        Some((b.addr + (off - b.off) as usize) as *mut c_void)
    })
    .unwrap_or(std::ptr::null_mut())
}

/// Forgets the shared mappings of a process that has gone away.
pub fn forget_process(pid: i32) {
    with(|s| {
        let gone: Vec<usize> = s.maps.iter().filter(|(_, m)| m.pid == pid).map(|(&a, _)| a).collect();
        for a in gone {
            let m = s.maps.remove(&a).unwrap();
            if let Some(b) = s.backings.get_mut(&m.key) {
                b.maps = b.maps.saturating_sub(1);
            }
        }
        s.backings.retain(|_, b| b.maps > 0 && b.pid != pid || b.addr >= GUEST_END && b.maps > 0);
    });
}

/// Shared file mappings overlapping [addr, addr+len), sorted, clipped to the range.
/// The part of [addr, addr+len) that is the user shared data page.
fn usd_overlap(addr: usize, len: usize) -> Option<(usize, usize)> {
    let (lo, hi) = (addr.max(USER_SHARED_DATA), (addr + len).min(USER_SHARED_DATA + PAGE));
    (lo < hi).then_some((lo, hi))
}

/// [addr, addr+len) without the user shared data page.
fn outside_usd(addr: usize, len: usize) -> Vec<(usize, usize)> {
    match usd_overlap(addr, len) {
        None => vec![(addr, addr + len)],
        Some((lo, hi)) => [(addr, lo), (hi, addr + len)].into_iter().filter(|r| r.0 < r.1).collect(),
    }
}

fn shared_ranges(addr: usize, len: usize) -> Vec<(usize, usize)> {
    with(|s| {
        s.maps
            .iter()
            .filter(|(&a, m)| a < addr + len && a + m.len > addr)
            .map(|(&a, m)| (a.max(addr), (a + m.len).min(addr + len)))
            .collect()
    })
}

/// Forgets shared mappings that start inside [addr, addr+len).
fn unmap_shared(addr: usize, len: usize) {
    with(|s| {
        let gone: Vec<usize> = s.maps.range(addr..addr + len).map(|(&a, _)| a).collect();
        for a in gone {
            let m = s.maps.remove(&a).unwrap();
            if let Some(b) = s.backings.get_mut(&m.key) {
                b.maps = b.maps.saturating_sub(1);
                if b.maps == 0 {
                    s.backings.remove(&m.key);
                }
            }
            let _ = m.shared_writable;
        }
    });
}

pub unsafe fn munmap(addr: *mut c_void, len: size_t) -> c_int {
    let a = addr as usize;
    let len = len.div_ceil(PAGE) * PAGE;
    let shared = with(|s| s.maps.range(a..a + len).next().is_some());
    if shared {
        unmap_shared(a, len);
        if a >= GUEST_END {
            return 0; // the backing buffer may still be referenced; leak it rather than free twice
        }
    }
    if in_guest(a, len) {
        return 0;
    }
    __real_munmap(addr, len)
}

pub unsafe fn mprotect(addr: *mut c_void, len: size_t, prot: c_int) -> c_int {
    let a = addr as usize;
    if in_guest(a, len) && prot != libc::PROT_NONE {
        mark(a & !(PAGE - 1), len + (a & (PAGE - 1)), true);
    }
    0
}

pub fn mremap() -> *mut c_void {
    set_errno(libc::ENOMEM);
    libc::MAP_FAILED
}
