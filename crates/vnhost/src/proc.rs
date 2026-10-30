//! Wine processes as instances inside the one wasm module.
//!
//! Every Wine process gets its own instance of the unix-side module (its own statics) and its
//! own slice of the guest address space; all of them share the linear memory, wineserver and
//! the emulated fds. A thread belongs to the process that created it (see the pthread_create
//! wrapper in c/shim.c). Threads of the host itself (main, wineserver) have pid `HOST_PID`.
//!
//! Per process state kept here: the address partition, the environment (getenv/setenv see
//! per-process overrides on top of the host environment), the current directory, the thread
//! list and exit status.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::ffi::{c_char, c_int, CStr, CString};
use std::sync::Mutex;

use crate::mem::GUEST_END;

pub const HOST_PID: i32 = 42;
const FIRST_PID: i32 = 100;

/// Guest addresses everybody may map: the window where wineserver's shared mappings live
/// (see `mem`), up to KUSER_SHARED_DATA and the page above it.
pub const SHARED_LO: usize = 0x7c00_0000;
pub const USER_SHARED_DATA: usize = 0x7ffe_0000;
const AREA_LO: usize = 0x1_0000;
const APP_SIZE: usize = 0x5000_0000;
const HELPER_SIZE: usize = 0x0800_0000;
const MIN_SIZE: usize = 0x0200_0000;

struct Proc {
    lo: usize,
    hi: usize,
    root: bool,
    dead: bool,
    tids: Vec<i32>,
    /// None = unset
    env: BTreeMap<String, Option<CString>>,
    cwd: String,
}

#[derive(Default)]
struct Procs {
    list: BTreeMap<i32, Proc>,
    next: i32,
}

static PROCS: Mutex<Option<Procs>> = Mutex::new(None);

thread_local! {
    static PID: Cell<i32> = const { Cell::new(HOST_PID) };
}

fn with<R>(f: impl FnOnce(&mut Procs) -> R) -> R {
    let mut g = PROCS.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(|| Procs { next: FIRST_PID, ..Default::default() }))
}

pub fn current() -> i32 {
    PID.with(|p| p.get())
}

/// A free range of `size` bytes: apps take the lowest one (exes want 0x400000), helpers the
/// highest one.
fn find_area(p: &Procs, size: usize, low: bool) -> Option<(usize, usize)> {
    let mut used: Vec<(usize, usize)> = p.list.values().map(|q| (q.lo, q.hi)).collect();
    used.sort();
    let mut holes = Vec::new();
    let mut at = AREA_LO;
    for (lo, hi) in used.iter().copied().chain([(SHARED_LO, SHARED_LO)]) {
        if lo >= at + size {
            holes.push((at, lo));
        }
        at = at.max(hi);
    }
    if low {
        holes.first().map(|&(lo, _)| (lo, lo + size))
    } else {
        holes.last().map(|&(_, hi)| (hi - size, hi))
    }
}

/// Creates a process slot; `app` asks for the big low partition.
pub fn create(app: bool, root: bool) -> Option<i32> {
    let parent_env = current_env();
    let parent_cwd = cwd();
    with(|p| {
        let mut size = if app { APP_SIZE } else { HELPER_SIZE };
        let area = loop {
            if let Some(a) = find_area(p, size, app) {
                break a;
            }
            size /= 2;
            if size < MIN_SIZE {
                return None;
            }
        };
        let pid = p.next;
        p.next += 1;
        trace(format_args!("process {pid}: {:#x}-{:#x}{}", area.0, area.1, if root { " (root)" } else { "" }));
        p.list.insert(pid, Proc { lo: area.0, hi: area.1, root, dead: false, tids: Vec::new(), env: parent_env, cwd: parent_cwd });
        Some(pid)
    })
}

pub(crate) fn trace(args: std::fmt::Arguments) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("VNVM_TRACE_PROC").is_some()) {
        eprintln!("vnhost: {args}");
    }
}

/// The host threads' current directory (Wine processes have their own).
static HOST_CWD: Mutex<Option<String>> = Mutex::new(None);

/// The calling thread's process's current directory.
pub fn cwd() -> String {
    let pid = current();
    if let Some(c) = with(|p| p.list.get(&pid).map(|q| q.cwd.clone())) {
        return c;
    }
    let mut g = HOST_CWD.lock().unwrap_or_else(|e| e.into_inner());
    g.get_or_insert_with(|| crate::wrap::real_getcwd().unwrap_or_else(|| "/".into())).clone()
}

pub fn set_cwd(path: String) {
    let pid = current();
    let done = with(|p| match p.list.get_mut(&pid) {
        Some(q) => {
            q.cwd = path.clone();
            true
        }
        None => false,
    });
    if !done {
        *HOST_CWD.lock().unwrap_or_else(|e| e.into_inner()) = Some(path);
    }
}

fn current_env() -> BTreeMap<String, Option<CString>> {
    let pid = current();
    with(|p| p.list.get(&pid).map(|q| q.env.clone()).unwrap_or_default())
}

pub fn exists(pid: i32) -> bool {
    with(|p| p.list.get(&pid).is_some_and(|q| !q.dead))
}

pub fn is_dead(pid: i32) -> bool {
    pid != HOST_PID && with(|p| p.list.get(&pid).is_none_or(|q| q.dead))
}

/// The guest range the calling thread's process may map.
pub fn area() -> (usize, usize) {
    let pid = current();
    with(|p| p.list.get(&pid).map_or((AREA_LO, GUEST_END), |q| (q.lo, q.hi)))
}

/// Whether the calling process may map guest range [lo, hi).
pub fn may_map(lo: usize, hi: usize) -> bool {
    if lo >= SHARED_LO {
        return true;
    }
    let pid = current();
    if pid == HOST_PID {
        return true;
    }
    let (a, b) = area();
    lo >= a && hi <= b
}

/// Marks the process dead: its fds close and its threads stop at their next chance.
pub fn kill(pid: i32) {
    let tids = with(|p| match p.list.get_mut(&pid) {
        Some(q) if !q.dead => {
            q.dead = true;
            q.tids.clone()
        }
        _ => Vec::new(),
    });
    trace(format_args!("process {pid}: killed, threads {tids:?}"));
    crate::fd::close_owned(pid);
    for t in tids {
        crate::cpu::vnhost_tkill(t, libc::SIGKILL);
    }
    reap(pid);
}

fn reap(pid: i32) {
    with(|p| {
        if p.list.get(&pid).is_some_and(|q| q.dead && q.tids.is_empty()) {
            p.list.remove(&pid);
            crate::mem::forget_process(pid);
            trace(format_args!("process {pid}: gone"));
        }
    });
}

/// Ends the calling thread if its process was killed.
pub fn check_alive() {
    let pid = current();
    if is_dead(pid) {
        unsafe { libc::pthread_exit(std::ptr::null_mut()) };
    }
}

// ---------------------------------------------------------------------------------------------
// C interface (hangover/main.c, c/shim.c, Wine's process creation)

#[no_mangle]
pub extern "C" fn vnhost_getpid() -> i32 {
    current()
}

/// New process slot; returns its pid or -1.
#[no_mangle]
pub extern "C" fn vnhost_proc_create(app: c_int, root: c_int) -> i32 {
    create(app != 0, root != 0).unwrap_or(-1)
}

/// Sets a variable in a (not yet started) process's environment; `val` NULL unsets it.
#[no_mangle]
pub unsafe extern "C" fn vnhost_proc_setenv(pid: i32, name: *const c_char, val: *const c_char) {
    let name = CStr::from_ptr(name).to_string_lossy().into_owned();
    let val = (!val.is_null()).then(|| CStr::from_ptr(val).to_owned());
    with(|p| {
        if let Some(q) = p.list.get_mut(&pid) {
            q.env.insert(name, val);
        }
    });
}

#[no_mangle]
pub extern "C" fn vnhost_fd_give(fd: c_int, pid: i32) {
    crate::fd::give(fd, pid);
}

/// The calling thread joins process `pid`.
#[no_mangle]
pub extern "C" fn vnhost_proc_enter(pid: i32) {
    PID.with(|p| p.set(pid));
    if pid == HOST_PID {
        return;
    }
    let tid = crate::cpu::vnhost_gettid();
    with(|p| {
        if let Some(q) = p.list.get_mut(&pid) {
            q.tids.push(tid);
        }
    });
}

/// The calling thread is going away.
#[no_mangle]
pub extern "C" fn vnhost_proc_leave() {
    let pid = current();
    if pid == HOST_PID {
        return;
    }
    let tid = crate::cpu::vnhost_gettid();
    let last = with(|p| match p.list.get_mut(&pid) {
        Some(q) => {
            q.tids.retain(|&t| t != tid);
            q.tids.is_empty()
        }
        None => false,
    });
    crate::cpu::forget_thread(tid);
    if last {
        // a process whose threads all ended without calling exit()
        kill(pid);
    }
}

#[no_mangle]
pub unsafe extern "C" fn vnhost_proc_area(lo: *mut usize, hi: *mut usize) {
    let (a, b) = area();
    *lo = a;
    *hi = b;
}

#[no_mangle]
pub extern "C" fn vnhost_proc_is_root() -> c_int {
    let pid = current();
    with(|p| p.list.get(&pid).is_some_and(|q| q.root)) as c_int
}

/// Kills the calling thread's process; returns only on host threads.
#[no_mangle]
pub extern "C" fn vnhost_proc_exit() {
    let pid = current();
    if pid == HOST_PID {
        return;
    }
    kill(pid);
    unsafe { libc::pthread_exit(std::ptr::null_mut()) };
}

// ---------------------------------------------------------------------------------------------
// environment

extern "C" {
    fn __real_getenv(name: *const c_char) -> *mut c_char;
    fn __real_setenv(name: *const c_char, val: *const c_char, overwrite: c_int) -> c_int;
    fn __real_unsetenv(name: *const c_char) -> c_int;
    fn __real_putenv(s: *mut c_char) -> c_int;
}

/// The process override for `name`: Some(None) if unset there, None if not overridden.
fn env_get(name: &str) -> Option<Option<*mut c_char>> {
    let pid = current();
    with(|p| {
        let v = p.list.get(&pid)?.env.get(name)?;
        Some(v.as_ref().map(|s| s.as_ptr() as *mut c_char))
    })
}

fn env_set(name: String, val: Option<CString>) -> bool {
    let pid = current();
    with(|p| match p.list.get_mut(&pid) {
        Some(q) => {
            q.env.insert(name, val);
            true
        }
        None => false,
    })
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_getenv(name: *const c_char) -> *mut c_char {
    if current() != HOST_PID {
        if let Some(v) = env_get(&CStr::from_ptr(name).to_string_lossy()) {
            return v.unwrap_or(std::ptr::null_mut());
        }
    }
    __real_getenv(name)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_setenv(name: *const c_char, val: *const c_char, overwrite: c_int) -> c_int {
    let n = CStr::from_ptr(name).to_string_lossy().into_owned();
    if current() != HOST_PID {
        if overwrite == 0 && !__wrap_getenv(name).is_null() {
            return 0;
        }
        if env_set(n, Some(CStr::from_ptr(val).to_owned())) {
            return 0;
        }
    }
    __real_setenv(name, val, overwrite)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_unsetenv(name: *const c_char) -> c_int {
    if current() != HOST_PID && env_set(CStr::from_ptr(name).to_string_lossy().into_owned(), None) {
        return 0;
    }
    __real_unsetenv(name)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_putenv(s: *mut c_char) -> c_int {
    if current() != HOST_PID {
        let kv = CStr::from_ptr(s).to_string_lossy().into_owned();
        let ok = match kv.split_once('=') {
            Some((k, v)) => env_set(k.to_owned(), CString::new(v).ok()),
            None => env_set(kv, None),
        };
        if ok {
            return 0;
        }
    }
    __real_putenv(s)
}
