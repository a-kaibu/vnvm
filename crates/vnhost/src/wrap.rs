//! libc entry points routed through vnhost with `-Wl,--wrap=<name>`.
//!
//! Each `__wrap_x` handles emulated fds (and guest memory) itself and passes everything else
//! to `__real_x`, emscripten's implementation. Variadic ones (fcntl, ioctl) live in c/shim.c.

use std::ffi::{c_char, c_int, c_void};

use libc::{iovec, msghdr, off_t, pollfd, size_t, sockaddr, socklen_t, ssize_t};

use crate::fd::{self, is_ours};
use crate::{dl, mem, proc, set_errno};

extern "C" {
    fn __real_socket(domain: c_int, ty: c_int, proto: c_int) -> c_int;
    fn __real_socketpair(domain: c_int, ty: c_int, proto: c_int, sv: *mut c_int) -> c_int;
    fn __real_bind(fd: c_int, addr: *const sockaddr, len: socklen_t) -> c_int;
    fn __real_listen(fd: c_int, backlog: c_int) -> c_int;
    fn __real_accept(fd: c_int, addr: *mut sockaddr, len: *mut socklen_t) -> c_int;
    fn __real_accept4(fd: c_int, addr: *mut sockaddr, len: *mut socklen_t, flags: c_int) -> c_int;
    fn __real_connect(fd: c_int, addr: *const sockaddr, len: socklen_t) -> c_int;
    fn __real_shutdown(fd: c_int, how: c_int) -> c_int;
    fn __real_setsockopt(fd: c_int, level: c_int, name: c_int, val: *const c_void, len: socklen_t) -> c_int;
    fn __real_getsockopt(fd: c_int, level: c_int, name: c_int, val: *mut c_void, len: *mut socklen_t) -> c_int;
    fn __real_getsockname(fd: c_int, addr: *mut sockaddr, len: *mut socklen_t) -> c_int;
    fn __real_getpeername(fd: c_int, addr: *mut sockaddr, len: *mut socklen_t) -> c_int;
    fn __real_sendmsg(fd: c_int, msg: *const msghdr, flags: c_int) -> ssize_t;
    fn __real_recvmsg(fd: c_int, msg: *mut msghdr, flags: c_int) -> ssize_t;
    fn __real_send(fd: c_int, buf: *const c_void, len: size_t, flags: c_int) -> ssize_t;
    fn __real_recv(fd: c_int, buf: *mut c_void, len: size_t, flags: c_int) -> ssize_t;
    fn __real_sendto(fd: c_int, buf: *const c_void, len: size_t, flags: c_int, addr: *const sockaddr, alen: socklen_t) -> ssize_t;
    fn __real_recvfrom(fd: c_int, buf: *mut c_void, len: size_t, flags: c_int, addr: *mut sockaddr, alen: *mut socklen_t) -> ssize_t;
    fn __real_read(fd: c_int, buf: *mut c_void, len: size_t) -> ssize_t;
    fn __real_write(fd: c_int, buf: *const c_void, len: size_t) -> ssize_t;
    fn __real_readv(fd: c_int, iov: *const iovec, cnt: c_int) -> ssize_t;
    fn __real_writev(fd: c_int, iov: *const iovec, cnt: c_int) -> ssize_t;
    fn __real_close(fd: c_int) -> c_int;
    fn __real_dup(fd: c_int) -> c_int;
    fn __real_dup2(fd: c_int, to: c_int) -> c_int;
    fn __real_poll(fds: *mut pollfd, n: libc::nfds_t, timeout: c_int) -> c_int;
    fn __real_fstat(fd: c_int, st: *mut libc::stat) -> c_int;
    fn __real_lseek(fd: c_int, off: off_t, whence: c_int) -> off_t;
    fn __real_getpid() -> libc::pid_t;
    fn __real_exit(code: c_int) -> !;
    fn __real__exit(code: c_int) -> !;
    fn __real_kill(pid: libc::pid_t, sig: c_int) -> c_int;
    fn __real_pthread_kill(t: libc::pthread_t, sig: c_int) -> c_int;
}

unsafe fn real_poll(fds: *mut pollfd, n: libc::nfds_t, timeout: c_int) -> c_int {
    __real_poll(fds, n, timeout)
}

// ---------------------------------------------------------------------------------------------
// sockets and pipes

#[no_mangle]
pub unsafe extern "C" fn __wrap_socket(domain: c_int, ty: c_int, proto: c_int) -> c_int {
    if domain == libc::AF_UNIX && ty & 0xf == libc::SOCK_STREAM {
        return fd::socket(ty);
    }
    __real_socket(domain, ty, proto)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_socketpair(domain: c_int, ty: c_int, proto: c_int, sv: *mut c_int) -> c_int {
    if domain == libc::AF_UNIX {
        return fd::socketpair(ty, &mut *(sv as *mut [c_int; 2]));
    }
    __real_socketpair(domain, ty, proto, sv)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_pipe(fds: *mut c_int) -> c_int {
    fd::pipe2(&mut *(fds as *mut [c_int; 2]), 0)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_pipe2(fds: *mut c_int, flags: c_int) -> c_int {
    fd::pipe2(&mut *(fds as *mut [c_int; 2]), flags)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_bind(f: c_int, addr: *const sockaddr, len: socklen_t) -> c_int {
    if is_ours(f) {
        return fd::bind(f, addr, len);
    }
    __real_bind(f, addr, len)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_listen(f: c_int, backlog: c_int) -> c_int {
    if is_ours(f) {
        return fd::listen(f);
    }
    __real_listen(f, backlog)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_accept(f: c_int, addr: *mut sockaddr, len: *mut socklen_t) -> c_int {
    if is_ours(f) {
        return accept_addr(fd::accept(f, 0), addr, len);
    }
    __real_accept(f, addr, len)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_accept4(f: c_int, addr: *mut sockaddr, len: *mut socklen_t, flags: c_int) -> c_int {
    if is_ours(f) {
        return accept_addr(fd::accept(f, flags), addr, len);
    }
    __real_accept4(f, addr, len, flags)
}

unsafe fn accept_addr(ret: c_int, addr: *mut sockaddr, len: *mut socklen_t) -> c_int {
    if ret >= 0 && !addr.is_null() && !len.is_null() && *len as usize >= 2 {
        (*addr).sa_family = libc::AF_UNIX as _;
        *len = 2;
    }
    ret
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_connect(f: c_int, addr: *const sockaddr, len: socklen_t) -> c_int {
    if is_ours(f) {
        return fd::connect(f, addr, len);
    }
    __real_connect(f, addr, len)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_shutdown(f: c_int, how: c_int) -> c_int {
    if is_ours(f) {
        return fd::shutdown(f, how);
    }
    __real_shutdown(f, how)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_setsockopt(f: c_int, level: c_int, name: c_int, val: *const c_void, len: socklen_t) -> c_int {
    if is_ours(f) {
        if level == libc::SOL_SOCKET && name == 16 /* SO_PASSCRED */ && !val.is_null() {
            return fd::setsockopt_passcred(f, *(val as *const c_int) != 0);
        }
        return 0;
    }
    __real_setsockopt(f, level, name, val, len)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_getsockopt(f: c_int, level: c_int, name: c_int, val: *mut c_void, len: *mut socklen_t) -> c_int {
    if is_ours(f) {
        if !val.is_null() && !len.is_null() && *len >= 4 {
            let v = match (level, name) {
                (libc::SOL_SOCKET, libc::SO_TYPE) => libc::SOCK_STREAM,
                (libc::SOL_SOCKET, libc::SO_RCVBUF) | (libc::SOL_SOCKET, libc::SO_SNDBUF) => 1 << 20,
                _ => 0,
            };
            *(val as *mut c_int) = v;
            *len = 4;
        }
        return 0;
    }
    __real_getsockopt(f, level, name, val, len)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_getsockname(f: c_int, addr: *mut sockaddr, len: *mut socklen_t) -> c_int {
    if is_ours(f) {
        return if fd::is_socket(f).is_some() { accept_addr(0, addr, len) } else { -1 };
    }
    __real_getsockname(f, addr, len)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_getpeername(f: c_int, addr: *mut sockaddr, len: *mut socklen_t) -> c_int {
    if is_ours(f) {
        return accept_addr(0, addr, len);
    }
    __real_getpeername(f, addr, len)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_sendmsg(f: c_int, msg: *const msghdr, flags: c_int) -> ssize_t {
    if is_ours(f) {
        return fd::sendmsg(f, msg);
    }
    __real_sendmsg(f, msg, flags)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_recvmsg(f: c_int, msg: *mut msghdr, flags: c_int) -> ssize_t {
    if is_ours(f) {
        return fd::recvmsg(f, msg, flags);
    }
    __real_recvmsg(f, msg, flags)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_send(f: c_int, buf: *const c_void, len: size_t, flags: c_int) -> ssize_t {
    if is_ours(f) {
        return fd::sendto(f, buf, len);
    }
    __real_send(f, buf, len, flags)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_recv(f: c_int, buf: *mut c_void, len: size_t, flags: c_int) -> ssize_t {
    if is_ours(f) {
        return fd::recvfrom(f, buf, len, flags);
    }
    __real_recv(f, buf, len, flags)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_sendto(f: c_int, buf: *const c_void, len: size_t, flags: c_int, addr: *const sockaddr, alen: socklen_t) -> ssize_t {
    if is_ours(f) {
        return fd::sendto(f, buf, len);
    }
    __real_sendto(f, buf, len, flags, addr, alen)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_recvfrom(f: c_int, buf: *mut c_void, len: size_t, flags: c_int, addr: *mut sockaddr, alen: *mut socklen_t) -> ssize_t {
    if is_ours(f) {
        return fd::recvfrom(f, buf, len, flags);
    }
    __real_recvfrom(f, buf, len, flags, addr, alen)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_read(f: c_int, buf: *mut c_void, len: size_t) -> ssize_t {
    if is_ours(f) {
        return fd::read(f, buf, len);
    }
    __real_read(f, buf, len)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_write(f: c_int, buf: *const c_void, len: size_t) -> ssize_t {
    if is_ours(f) {
        return fd::write(f, buf, len);
    }
    __real_write(f, buf, len)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_readv(f: c_int, iov: *const iovec, cnt: c_int) -> ssize_t {
    if is_ours(f) {
        return fd::readv(f, iov, cnt);
    }
    __real_readv(f, iov, cnt)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_writev(f: c_int, iov: *const iovec, cnt: c_int) -> ssize_t {
    if is_ours(f) {
        return fd::writev(f, iov, cnt);
    }
    __real_writev(f, iov, cnt)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_close(f: c_int) -> c_int {
    if is_ours(f) {
        return fd::close(f);
    }
    __real_close(f)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_dup(f: c_int) -> c_int {
    if is_ours(f) {
        return fd::dup(f, 0, false);
    }
    __real_dup(f)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_dup2(f: c_int, to: c_int) -> c_int {
    if is_ours(f) {
        return fd::dup2(f, to, false);
    }
    if is_ours(to) {
        set_errno(libc::EINVAL);
        return -1;
    }
    __real_dup2(f, to)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_poll(fds: *mut pollfd, n: libc::nfds_t, timeout: c_int) -> c_int {
    let s = std::slice::from_raw_parts(fds, n as usize);
    if !s.iter().any(|p| is_ours(p.fd)) {
        return __real_poll(fds, n, timeout);
    }
    fd::poll(fds, n, timeout, real_poll)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_fstat(f: c_int, st: *mut libc::stat) -> c_int {
    if is_ours(f) {
        let Some(sock) = fd::is_socket(f) else {
            set_errno(libc::EBADF);
            return -1;
        };
        std::ptr::write_bytes(st, 0, 1);
        (*st).st_mode = if sock { libc::S_IFSOCK } else { libc::S_IFIFO } | 0o600;
        (*st).st_dev = 0x766e;
        (*st).st_ino = f as _;
        (*st).st_nlink = 1;
        (*st).st_blksize = 4096;
        return 0;
    }
    __real_fstat(f, st)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_lseek(f: c_int, off: off_t, whence: c_int) -> off_t {
    if is_ours(f) {
        set_errno(libc::ESPIPE);
        return -1;
    }
    __real_lseek(f, off, whence)
}

#[no_mangle]
pub extern "C" fn vnhost_fcntl(f: c_int, cmd: c_int, arg: c_int) -> c_int {
    fd::fcntl(f, cmd, arg)
}

#[no_mangle]
pub extern "C" fn vnhost_fionread(f: c_int) -> c_int {
    fd::nread(f)
}

#[no_mangle]
pub extern "C" fn vnhost_is_ours(f: c_int) -> c_int {
    is_ours(f) as c_int
}

/// There are no terminals: Wine then treats the std handles as plain files/pipes and doesn't
/// start a unix console (and emscripten's tty ioctls aren't reachable under NODERAWFS).
#[no_mangle]
pub extern "C" fn __wrap_isatty(_f: c_int) -> c_int {
    set_errno(libc::ENOTTY);
    0
}

// ---------------------------------------------------------------------------------------------
// memory

#[no_mangle]
pub unsafe extern "C" fn __wrap_mmap(addr: *mut c_void, len: size_t, prot: c_int, flags: c_int, f: c_int, off: off_t) -> *mut c_void {
    mem::mmap(addr, len, prot, flags, f, off)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_munmap(addr: *mut c_void, len: size_t) -> c_int {
    mem::munmap(addr, len)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_mprotect(addr: *mut c_void, len: size_t, prot: c_int) -> c_int {
    mem::mprotect(addr, len, prot)
}

#[no_mangle]
pub extern "C" fn __wrap_madvise(_addr: *mut c_void, _len: size_t, _advice: c_int) -> c_int {
    0
}

#[no_mangle]
pub extern "C" fn __wrap_msync(_addr: *mut c_void, _len: size_t, _flags: c_int) -> c_int {
    0
}

#[no_mangle]
pub extern "C" fn __wrap_mlock(_addr: *const c_void, _len: size_t) -> c_int {
    0
}

#[no_mangle]
pub extern "C" fn vnhost_mremap() -> *mut c_void {
    mem::mremap()
}

// ---------------------------------------------------------------------------------------------
// dynamic loading

#[no_mangle]
pub unsafe extern "C" fn __wrap_dlopen(path: *const c_char, _flags: c_int) -> *mut c_void {
    dl::dlopen(path)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void {
    dl::dlsym(handle, name)
}

#[no_mangle]
pub extern "C" fn __wrap_dlclose(_handle: *mut c_void) -> c_int {
    0
}

#[no_mangle]
pub extern "C" fn __wrap_dlerror() -> *const c_char {
    dl::dlerror()
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_dladdr(_addr: *const c_void, info: *mut libc::Dl_info) -> c_int {
    dl::dladdr(info)
}

// ---------------------------------------------------------------------------------------------
// signals: everything runs in one process, so a signal is a flag the target thread polls

#[no_mangle]
pub unsafe extern "C" fn __wrap_kill(pid: libc::pid_t, sig: c_int) -> c_int {
    let me = proc::current();
    let target = if pid == 0 || pid == -1 { me } else { pid };
    if target == proc::HOST_PID || target == libc::getpid() {
        return match sig {
            0 => 0,
            libc::SIGKILL | libc::SIGTERM => libc::_exit(128 + sig),
            _ => {
                eprintln!("vnhost: ignoring host signal {sig}");
                0
            }
        };
    }
    if !proc::exists(target) {
        set_errno(libc::ESRCH);
        return -1;
    }
    match sig {
        0 => {}
        libc::SIGKILL | libc::SIGTERM => {
            proc::kill(target);
            if target == me {
                proc::check_alive();
            }
        }
        _ => eprintln!("vnhost: ignoring signal {sig} for process {target}"),
    }
    0
}

#[no_mangle]
pub extern "C" fn __wrap_getpid() -> libc::pid_t {
    match proc::current() {
        proc::HOST_PID => unsafe { __real_getpid() },
        pid => pid,
    }
}

/// exit() and _exit() end only the calling Wine process (the root one ends everything).
#[no_mangle]
pub unsafe extern "C" fn __wrap_exit(code: c_int) -> ! {
    if proc::current() == proc::HOST_PID || proc::vnhost_proc_is_root() != 0 {
        __real_exit(code);
    }
    proc::vnhost_proc_exit();
    unreachable!()
}

#[no_mangle]
pub unsafe extern "C" fn __wrap__exit(code: c_int) -> ! {
    if proc::current() == proc::HOST_PID || proc::vnhost_proc_is_root() != 0 {
        __real__exit(code);
    }
    proc::vnhost_proc_exit();
    unreachable!()
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_pthread_kill(t: libc::pthread_t, sig: c_int) -> c_int {
    match crate::cpu::tid_of(t) {
        Some(tid) => {
            if sig != 0 {
                crate::cpu::vnhost_tkill(tid, sig);
            }
            0
        }
        None => __real_pthread_kill(t, sig),
    }
}

// ---------------------------------------------------------------------------------------------
// missing from emscripten's libc

#[no_mangle]
pub extern "C" fn fgetxattr(_fd: c_int, _name: *const c_char, _value: *mut c_void, _size: size_t) -> ssize_t {
    set_errno(libc::ENOTSUP);
    -1
}

#[no_mangle]
pub extern "C" fn fsetxattr(_fd: c_int, _name: *const c_char, _value: *const c_void, _size: size_t, _flags: c_int) -> c_int {
    set_errno(libc::ENOTSUP);
    -1
}

#[no_mangle]
pub extern "C" fn fremovexattr(_fd: c_int, _name: *const c_char) -> c_int {
    set_errno(libc::ENOTSUP);
    -1
}

#[no_mangle]
pub extern "C" fn getxattr(_path: *const c_char, _name: *const c_char, _value: *mut c_void, _size: size_t) -> ssize_t {
    set_errno(libc::ENOTSUP);
    -1
}

#[no_mangle]
pub extern "C" fn setxattr(_path: *const c_char, _name: *const c_char, _value: *const c_void, _size: size_t, _flags: c_int) -> c_int {
    set_errno(libc::ENOTSUP);
    -1
}

#[no_mangle]
pub extern "C" fn removexattr(_path: *const c_char, _name: *const c_char) -> c_int {
    set_errno(libc::ENOTSUP);
    -1
}

#[no_mangle]
pub extern "C" fn listxattr(_path: *const c_char, _list: *mut c_char, _size: size_t) -> ssize_t {
    set_errno(libc::ENOTSUP);
    -1
}

#[no_mangle]
pub extern "C" fn flistxattr(_fd: c_int, _list: *mut c_char, _size: size_t) -> ssize_t {
    set_errno(libc::ENOTSUP);
    -1
}
