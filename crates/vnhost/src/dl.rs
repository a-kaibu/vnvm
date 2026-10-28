//! dlopen over statically linked unix libraries.
//!
//! Each Wine unix library (ntdll.so, win32u.so, ...) is linked into the one wasm module; a
//! generated C table registers the symbols Wine looks up by name (`__wine_unix_call_funcs`
//! and friends) under the library's file name.

use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::Mutex;

#[repr(C)]
pub struct Sym {
    pub name: *const c_char,
    pub addr: *mut c_void,
}

struct Module {
    name: String,
    syms: Vec<(String, usize)>,
}

static MODULES: Mutex<Vec<Module>> = Mutex::new(Vec::new());
static ERROR: Mutex<Option<CString>> = Mutex::new(None);

const HANDLE_BASE: usize = 0x100;

/// Registers `name` (a file name such as "win32u.so") with a NULL-terminated symbol table.
#[no_mangle]
pub unsafe extern "C" fn vnhost_register_unixlib(name: *const c_char, syms: *const Sym) {
    let mut list = Vec::new();
    let mut p = syms;
    while !(*p).name.is_null() {
        list.push((CStr::from_ptr((*p).name).to_string_lossy().into_owned(), (*p).addr as usize));
        p = p.add(1);
    }
    let name = CStr::from_ptr(name).to_string_lossy().into_owned();
    MODULES.lock().unwrap().push(Module { name, syms: list });
}

fn set_error(msg: String) {
    *ERROR.lock().unwrap() = CString::new(msg).ok();
}

pub unsafe fn dlopen(path: *const c_char) -> *mut c_void {
    if path.is_null() {
        return HANDLE_BASE as *mut c_void; // the main program
    }
    let path = CStr::from_ptr(path).to_string_lossy().into_owned();
    let base = path.rsplit('/').next().unwrap_or(&path);
    let mods = MODULES.lock().unwrap();
    match mods.iter().position(|m| m.name == base) {
        Some(i) => (HANDLE_BASE + 1 + i) as *mut c_void,
        None => {
            drop(mods);
            set_error(format!("{path}: not linked into this module"));
            std::ptr::null_mut()
        }
    }
}

pub unsafe fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void {
    let name = CStr::from_ptr(name).to_string_lossy();
    let mods = MODULES.lock().unwrap();
    let h = handle as usize;
    let found = if h > HANDLE_BASE && h <= HANDLE_BASE + mods.len() {
        mods[h - HANDLE_BASE - 1].syms.iter().find(|s| s.0 == name).map(|s| s.1)
    } else {
        // RTLD_DEFAULT, RTLD_NEXT or the main program: search everything
        mods.iter().flat_map(|m| m.syms.iter()).find(|s| s.0 == name).map(|s| s.1)
    };
    drop(mods);
    match found {
        Some(a) => a as *mut c_void,
        None => {
            set_error(format!("{name}: symbol not found"));
            std::ptr::null_mut()
        }
    }
}

pub fn dlerror() -> *const c_char {
    let mut e = ERROR.lock().unwrap();
    // keep the string alive until the next call, as dlerror() promises
    static LAST: Mutex<Option<CString>> = Mutex::new(None);
    let msg = e.take();
    let mut last = LAST.lock().unwrap();
    *last = msg;
    last.as_ref().map_or(std::ptr::null(), |s| s.as_ptr())
}

/// dladdr() answers with ntdll.so's install path for every address: Wine only uses it to find
/// its own directory.
pub unsafe fn dladdr(info: *mut libc::Dl_info) -> i32 {
    static PATH: Mutex<Option<CString>> = Mutex::new(None);
    let mut p = PATH.lock().unwrap();
    if p.is_none() {
        let root = std::env::var("VNVM_WINE_ROOT").unwrap_or_else(|_| "/opt/wine-vnbox".into());
        *p = CString::new(format!("{root}/lib/wine/i386-unix/ntdll.so")).ok();
    }
    (*info).dli_fname = p.as_ref().unwrap().as_ptr();
    (*info).dli_fbase = std::ptr::null_mut();
    (*info).dli_sname = std::ptr::null();
    (*info).dli_saddr = std::ptr::null_mut();
    1
}
