//! libc entry points routed through vnhost with `-Wl,--wrap=<name>`.
//!
//! Each `__wrap_x` handles emulated fds (and guest memory) itself and passes everything else
//! to `__real_x`, emscripten's implementation. Variadic ones (fcntl, ioctl) live in c/shim.c.

use std::ffi::{c_char, c_int, c_void};

use libc::{iovec, msghdr, off_t, pollfd, size_t, sockaddr, socklen_t, ssize_t};

use crate::fd::{self, is_ours};
use crate::{dl, mem, proc, set_errno, vfs};

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
    fn __real_select(n: c_int, r: *mut libc::fd_set, w: *mut libc::fd_set, e: *mut libc::fd_set, tv: *mut libc::timeval) -> c_int;
    fn __real_fstat(fd: c_int, st: *mut libc::stat) -> c_int;
    fn __real_lseek(fd: c_int, off: off_t, whence: c_int) -> off_t;
    fn __real_getpid() -> libc::pid_t;
    fn __real_exit(code: c_int) -> !;
    fn __real__exit(code: c_int) -> !;
    fn __real_kill(pid: libc::pid_t, sig: c_int) -> c_int;
    fn __real_pthread_kill(t: libc::pthread_t, sig: c_int) -> c_int;
}

/// readv/writev on a file, one buffer at a time.
unsafe fn each_iov(iov: *const iovec, cnt: c_int, mut f: impl FnMut(*mut c_void, size_t) -> ssize_t) -> ssize_t {
    let mut total = 0;
    for v in std::slice::from_raw_parts(iov, cnt.max(0) as usize) {
        let n = f(v.iov_base, v.iov_len);
        if n < 0 {
            return if total > 0 { total } else { n };
        }
        total += n;
        if (n as size_t) < v.iov_len {
            break;
        }
    }
    total
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
    if let Some(h) = fd::vfs_handle(f) {
        return vfs::pread(h, buf, len, None);
    }
    if is_ours(f) {
        return fd::read(f, buf, len);
    }
    __real_read(f, buf, len)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_write(f: c_int, buf: *const c_void, len: size_t) -> ssize_t {
    if let Some(h) = fd::vfs_handle(f) {
        return vfs::pwrite(h, buf, len, None);
    }
    if is_ours(f) {
        return fd::write(f, buf, len);
    }
    __real_write(f, buf, len)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_readv(f: c_int, iov: *const iovec, cnt: c_int) -> ssize_t {
    if let Some(h) = fd::vfs_handle(f) {
        return each_iov(iov, cnt, |b, l| vfs::pread(h, b, l, None));
    }
    if is_ours(f) {
        return fd::readv(f, iov, cnt);
    }
    __real_readv(f, iov, cnt)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_writev(f: c_int, iov: *const iovec, cnt: c_int) -> ssize_t {
    if let Some(h) = fd::vfs_handle(f) {
        return each_iov(iov, cnt, |b, l| vfs::pwrite(h, b, l, None));
    }
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

/// select() with no descriptors is how Wine sleeps (NtDelayExecution); emscripten's returns
/// at once.
#[no_mangle]
pub unsafe extern "C" fn __wrap_select(
    n: c_int,
    r: *mut libc::fd_set,
    w: *mut libc::fd_set,
    e: *mut libc::fd_set,
    tv: *mut libc::timeval,
) -> c_int {
    if n == 0 && !tv.is_null() {
        let tv = &*tv;
        std::thread::sleep(std::time::Duration::new(tv.tv_sec.max(0) as u64, (tv.tv_usec.max(0) * 1000) as u32));
        return 0;
    }
    __real_select(n, r, w, e, tv)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_poll(fds: *mut pollfd, n: libc::nfds_t, timeout: c_int) -> c_int {
    if n == 0 && timeout > 0 {
        std::thread::sleep(std::time::Duration::from_millis(timeout as u64));
        return 0;
    }
    let s = std::slice::from_raw_parts(fds, n as usize);
    if !s.iter().any(|p| is_ours(p.fd)) {
        return __real_poll(fds, n, timeout);
    }
    fd::poll(fds, n, timeout, real_poll)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_fstat(f: c_int, st: *mut libc::stat) -> c_int {
    if let Some(h) = fd::vfs_handle(f) {
        return vfs::fstat(h, &mut *st);
    }
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
    if let Some(h) = fd::vfs_handle(f) {
        return vfs::lseek(h, off, whence);
    }
    if is_ours(f) {
        set_errno(libc::ESPIPE);
        return -1;
    }
    __real_lseek(f, off, whence)
}

#[no_mangle]
pub extern "C" fn vnhost_fcntl(f: c_int, cmd: c_int, arg: c_int) -> c_int {
    if cmd == libc::F_GETFL {
        if let Some(h) = fd::vfs_handle(f) {
            return vfs::flags(h);
        }
    }
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

// ---------------------------------------------------------------------------------------------
// files (see vfs): calls by path, and by fd for what the fd section above doesn't cover

extern "C" {
    fn __real_openat(dirfd: c_int, path: *const c_char, flags: c_int, mode: libc::mode_t) -> c_int;
    fn __real_stat(path: *const c_char, st: *mut libc::stat) -> c_int;
    fn __real_lstat(path: *const c_char, st: *mut libc::stat) -> c_int;
    fn __real_fstatat(dirfd: c_int, path: *const c_char, st: *mut libc::stat, flags: c_int) -> c_int;
    fn __real_access(path: *const c_char, mode: c_int) -> c_int;
    fn __real_faccessat(dirfd: c_int, path: *const c_char, mode: c_int, flags: c_int) -> c_int;
    fn __real_readlink(path: *const c_char, buf: *mut c_char, size: size_t) -> ssize_t;
    fn __real_mkdir(path: *const c_char, mode: libc::mode_t) -> c_int;
    fn __real_rmdir(path: *const c_char) -> c_int;
    fn __real_unlink(path: *const c_char) -> c_int;
    fn __real_rename(from: *const c_char, to: *const c_char) -> c_int;
    fn __real_symlink(target: *const c_char, path: *const c_char) -> c_int;
    fn __real_link(from: *const c_char, to: *const c_char) -> c_int;
    fn __real_chmod(path: *const c_char, mode: libc::mode_t) -> c_int;
    fn __real_utimensat(dirfd: c_int, path: *const c_char, times: *const libc::timespec, flags: c_int) -> c_int;
    fn __real_getcwd(buf: *mut c_char, size: size_t) -> *mut c_char;
    fn __real_realpath(path: *const c_char, out: *mut c_char) -> *mut c_char;
    fn __real_opendir(path: *const c_char) -> *mut c_void;
    fn __real_fdopendir(fd: c_int) -> *mut c_void;
    fn __real_readdir(d: *mut c_void) -> *mut libc::dirent;
    fn __real_closedir(d: *mut c_void) -> c_int;
    fn __real_rewinddir(d: *mut c_void);
    fn __real_dirfd(d: *mut c_void) -> c_int;
    fn __real_pread(fd: c_int, buf: *mut c_void, len: size_t, off: off_t) -> ssize_t;
    fn __real_pwrite(fd: c_int, buf: *const c_void, len: size_t, off: off_t) -> ssize_t;
    fn __real_ftruncate(fd: c_int, len: off_t) -> c_int;
    fn __real_fsync(fd: c_int) -> c_int;
    fn __real_fchmod(fd: c_int, mode: libc::mode_t) -> c_int;
    fn __real_fchdir(fd: c_int) -> c_int;
    fn __real_futimens(fd: c_int, times: *const libc::timespec) -> c_int;
    fn __real_fstatfs(fd: c_int, st: *mut libc::statfs) -> c_int;
    fn __real_fstatvfs(fd: c_int, st: *mut libc::statvfs) -> c_int;
    fn __real_posix_fallocate(fd: c_int, off: off_t, len: off_t) -> c_int;
    fn __real_fgetxattr(fd: c_int, name: *const c_char, val: *mut c_void, size: size_t) -> ssize_t;
    fn __real_fsetxattr(fd: c_int, name: *const c_char, val: *const c_void, size: size_t, flags: c_int) -> c_int;
    fn __real_fremovexattr(fd: c_int, name: *const c_char) -> c_int;
    fn __real_getxattr(path: *const c_char, name: *const c_char, val: *mut c_void, size: size_t) -> ssize_t;
}

pub(crate) fn real_getcwd() -> Option<String> {
    let mut buf = [0 as c_char; 4096];
    unsafe {
        if __real_getcwd(buf.as_mut_ptr(), buf.len()).is_null() {
            return None;
        }
        Some(std::ffi::CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned())
    }
}

pub(crate) unsafe fn real_stat(path: &std::ffi::CStr, st: *mut libc::stat) -> c_int {
    __real_stat(path.as_ptr(), st)
}

/// mkdir -p in emscripten's file system.
pub(crate) unsafe fn real_mkdir_p(path: &std::ffi::CStr) {
    let p = path.to_string_lossy();
    let mut at = String::new();
    for c in p.split('/').filter(|c| !c.is_empty()) {
        at.push('/');
        at.push_str(c);
        if let Ok(c) = std::ffi::CString::new(at.clone()) {
            __real_mkdir(c.as_ptr(), 0o755);
        }
    }
}

/// Runs `f` with the absolute path when vfs leaves a path to emscripten.
macro_rules! route {
    ($r:expr, |$p:ident| $real:expr) => {
        match $r {
            vfs::Route::Done(v) => v,
            vfs::Route::Real(c) => {
                let $p = c.as_ptr();
                $real
            }
        }
    };
}

/// open/openat (the variadic entry points are in c/shim.c)
#[no_mangle]
pub unsafe extern "C" fn vnhost_openat(dirfd: c_int, path: *const c_char, flags: c_int, mode: libc::mode_t) -> c_int {
    match vfs::open(dirfd, path, flags, mode) {
        vfs::Route::Done(-2) => __real_openat(dirfd, path, flags, mode),
        vfs::Route::Done(v) => v,
        vfs::Route::Real(c) => __real_openat(libc::AT_FDCWD, c.as_ptr(), flags, mode),
    }
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_stat(path: *const c_char, st: *mut libc::stat) -> c_int {
    route!(vfs::stat(libc::AT_FDCWD, path, st, true), |p| __real_stat(p, st))
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_lstat(path: *const c_char, st: *mut libc::stat) -> c_int {
    route!(vfs::stat(libc::AT_FDCWD, path, st, false), |p| __real_lstat(p, st))
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_fstatat(dirfd: c_int, path: *const c_char, st: *mut libc::stat, flags: c_int) -> c_int {
    if flags & libc::AT_EMPTY_PATH != 0 && *path == 0 {
        return __wrap_fstat(dirfd, st);
    }
    match vfs::stat(dirfd, path, st, flags & libc::AT_SYMLINK_NOFOLLOW == 0) {
        vfs::Route::Done(-2) => __real_fstatat(dirfd, path, st, flags),
        vfs::Route::Done(v) => v,
        vfs::Route::Real(c) => __real_fstatat(libc::AT_FDCWD, c.as_ptr(), st, flags),
    }
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_access(path: *const c_char, mode: c_int) -> c_int {
    route!(vfs::access(libc::AT_FDCWD, path), |p| __real_access(p, mode))
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_faccessat(dirfd: c_int, path: *const c_char, mode: c_int, flags: c_int) -> c_int {
    match vfs::access(dirfd, path) {
        vfs::Route::Done(-2) => __real_faccessat(dirfd, path, mode, flags),
        vfs::Route::Done(v) => v,
        vfs::Route::Real(c) => __real_faccessat(libc::AT_FDCWD, c.as_ptr(), mode, flags),
    }
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_readlink(path: *const c_char, buf: *mut c_char, size: size_t) -> ssize_t {
    route!(vfs::readlink(libc::AT_FDCWD, path, buf, size), |p| __real_readlink(p, buf, size))
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_mkdir(path: *const c_char, mode: libc::mode_t) -> c_int {
    route!(vfs::mkdir(path, mode), |p| __real_mkdir(p, mode))
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_rmdir(path: *const c_char) -> c_int {
    route!(vfs::remove(path, true), |p| __real_rmdir(p))
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_unlink(path: *const c_char) -> c_int {
    route!(vfs::remove(path, false), |p| __real_unlink(p))
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_rename(from: *const c_char, to: *const c_char) -> c_int {
    let to_abs = std::ffi::CString::new(vfs::normalize(&std::ffi::CStr::from_ptr(to).to_string_lossy())).unwrap_or_default();
    route!(vfs::rename(from, to), |p| __real_rename(p, to_abs.as_ptr()))
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_symlink(target: *const c_char, path: *const c_char) -> c_int {
    route!(vfs::symlink(target, path), |p| __real_symlink(target, p))
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_link(from: *const c_char, to: *const c_char) -> c_int {
    let to_abs = std::ffi::CString::new(vfs::normalize(&std::ffi::CStr::from_ptr(to).to_string_lossy())).unwrap_or_default();
    route!(vfs::link(from, to), |p| __real_link(p, to_abs.as_ptr()))
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_chmod(path: *const c_char, mode: libc::mode_t) -> c_int {
    route!(vfs::chmod(path, mode), |p| __real_chmod(p, mode))
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_utimensat(dirfd: c_int, path: *const c_char, times: *const libc::timespec, flags: c_int) -> c_int {
    if path.is_null() {
        return __wrap_futimens(dirfd, times);
    }
    match vfs::utimensat(dirfd, path, times, flags) {
        vfs::Route::Done(-2) => __real_utimensat(dirfd, path, times, flags),
        vfs::Route::Done(v) => v,
        vfs::Route::Real(c) => __real_utimensat(libc::AT_FDCWD, c.as_ptr(), times, flags),
    }
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_chdir(path: *const c_char) -> c_int {
    vfs::chdir(path)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_fchdir(f: c_int) -> c_int {
    if let Some(h) = fd::vfs_handle(f) {
        return vfs::fchdir(h);
    }
    // a real directory: let emscripten find its path, then make it ours
    if __real_fchdir(f) != 0 {
        return -1;
    }
    if let Some(c) = real_getcwd() {
        proc::set_cwd(c);
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_getcwd(buf: *mut c_char, size: size_t) -> *mut c_char {
    let c = proc::cwd();
    if buf.is_null() {
        return std::ffi::CString::new(c).unwrap_or_default().into_raw_libc();
    }
    if c.len() + 1 > size {
        set_errno(libc::ERANGE);
        return std::ptr::null_mut();
    }
    std::ptr::copy_nonoverlapping(c.as_ptr() as *const c_char, buf, c.len());
    *buf.add(c.len()) = 0;
    buf
}

trait IntoRawLibc {
    fn into_raw_libc(self) -> *mut c_char;
}

impl IntoRawLibc for std::ffi::CString {
    /// A copy in malloc'ed memory (the caller frees it with free()).
    fn into_raw_libc(self) -> *mut c_char {
        let b = self.as_bytes_with_nul();
        unsafe {
            let p = libc::malloc(b.len()) as *mut c_char;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(b.as_ptr() as *const c_char, p, b.len());
            }
            p
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_realpath(path: *const c_char, out: *mut c_char) -> *mut c_char {
    match vfs::realpath(path) {
        vfs::Route::Real(c) => __real_realpath(c.as_ptr(), out),
        vfs::Route::Done(Err(e)) => {
            set_errno(e);
            std::ptr::null_mut()
        }
        vfs::Route::Done(Ok(p)) => {
            if out.is_null() {
                return std::ffi::CString::new(p).unwrap_or_default().into_raw_libc();
            }
            let n = p.len().min(libc::PATH_MAX as usize - 1);
            std::ptr::copy_nonoverlapping(p.as_ptr() as *const c_char, out, n);
            *out.add(n) = 0;
            out
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_opendir(path: *const c_char) -> *mut c_void {
    let f = vnhost_openat(libc::AT_FDCWD, path, libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC, 0);
    if f < 0 {
        return std::ptr::null_mut();
    }
    if fd::vfs_handle(f).is_some() {
        return vfs::fdopendir(f);
    }
    __real_fdopendir(f)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_fdopendir(f: c_int) -> *mut c_void {
    if fd::vfs_handle(f).is_some() {
        return vfs::fdopendir(f);
    }
    __real_fdopendir(f)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_readdir(d: *mut c_void) -> *mut libc::dirent {
    if vfs::is_dir_stream(d) {
        return vfs::readdir(d);
    }
    __real_readdir(d)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_closedir(d: *mut c_void) -> c_int {
    if vfs::is_dir_stream(d) {
        return vfs::closedir(d);
    }
    __real_closedir(d)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_rewinddir(d: *mut c_void) {
    if vfs::is_dir_stream(d) {
        return vfs::rewinddir(d);
    }
    __real_rewinddir(d)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_dirfd(d: *mut c_void) -> c_int {
    if vfs::is_dir_stream(d) {
        return vfs::dirfd(d);
    }
    __real_dirfd(d)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_pread(f: c_int, buf: *mut c_void, len: size_t, off: off_t) -> ssize_t {
    if let Some(h) = fd::vfs_handle(f) {
        return vfs::pread(h, buf, len, Some(off as u64));
    }
    __real_pread(f, buf, len, off)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_pwrite(f: c_int, buf: *const c_void, len: size_t, off: off_t) -> ssize_t {
    if let Some(h) = fd::vfs_handle(f) {
        return vfs::pwrite(h, buf, len, Some(off as u64));
    }
    __real_pwrite(f, buf, len, off)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_ftruncate(f: c_int, len: off_t) -> c_int {
    if let Some(h) = fd::vfs_handle(f) {
        return vfs::ftruncate(h, len);
    }
    __real_ftruncate(f, len)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_fsync(f: c_int) -> c_int {
    if is_ours(f) {
        return 0;
    }
    __real_fsync(f)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_fchmod(f: c_int, mode: libc::mode_t) -> c_int {
    if let Some(h) = fd::vfs_handle(f) {
        return vfs::fchmod(h, mode);
    }
    __real_fchmod(f, mode)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_futimens(f: c_int, times: *const libc::timespec) -> c_int {
    if let Some(h) = fd::vfs_handle(f) {
        return vfs::futimens(h, times);
    }
    __real_futimens(f, times)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_fstatfs(f: c_int, st: *mut libc::statfs) -> c_int {
    if is_ours(f) {
        return vfs::fstatfs(&mut *st);
    }
    __real_fstatfs(f, st)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_fstatvfs(f: c_int, st: *mut libc::statvfs) -> c_int {
    if is_ours(f) {
        return vfs::fstatvfs(&mut *st);
    }
    __real_fstatvfs(f, st)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_posix_fallocate(f: c_int, off: off_t, len: off_t) -> c_int {
    if let Some(h) = fd::vfs_handle(f) {
        return vfs::fallocate(h, off, len);
    }
    __real_posix_fallocate(f, off, len)
}

// no extended attributes on our files (Wine then keeps DOS attributes elsewhere)

#[no_mangle]
pub unsafe extern "C" fn __wrap_fgetxattr(f: c_int, name: *const c_char, val: *mut c_void, size: size_t) -> ssize_t {
    if is_ours(f) {
        set_errno(libc::ENOTSUP);
        return -1;
    }
    __real_fgetxattr(f, name, val, size)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_fsetxattr(f: c_int, name: *const c_char, val: *const c_void, size: size_t, flags: c_int) -> c_int {
    if is_ours(f) {
        set_errno(libc::ENOTSUP);
        return -1;
    }
    __real_fsetxattr(f, name, val, size, flags)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_fremovexattr(f: c_int, name: *const c_char) -> c_int {
    if is_ours(f) {
        set_errno(libc::ENOTSUP);
        return -1;
    }
    __real_fremovexattr(f, name)
}

#[no_mangle]
pub unsafe extern "C" fn __wrap_getxattr(path: *const c_char, name: *const c_char, val: *mut c_void, size: size_t) -> ssize_t {
    let mut st: libc::stat = std::mem::zeroed();
    route!(
        match vfs::stat(libc::AT_FDCWD, path, &mut st, true) {
            vfs::Route::Done(0) => {
                set_errno(libc::ENOTSUP);
                vfs::Route::Done(-1)
            }
            other => other.map(|v| v as ssize_t),
        },
        |p| __real_getxattr(p, name, val, size)
    )
}

// ---------------------------------------------------------------------------------------------
// clocks

extern "C" {
    fn __real_clock_gettime(clk: libc::clockid_t, ts: *mut libc::timespec) -> c_int;
    fn emscripten_get_now() -> f64;
    fn emscripten_date_now() -> f64;
}

/// The monotonic and real-time clocks straight from performance.now() / Date.now() as doubles:
/// emscripten's clock_gettime goes through WASI's clock_time_get, whose 64-bit arguments make
/// every call box BigInts in JS. The wineserver and every wait read the clock thousands of times
/// a second.
#[no_mangle]
pub unsafe extern "C" fn __wrap_clock_gettime(clk: libc::clockid_t, ts: *mut libc::timespec) -> c_int {
    let ms = match clk {
        libc::CLOCK_REALTIME => emscripten_date_now(),
        libc::CLOCK_MONOTONIC | libc::CLOCK_MONOTONIC_RAW | libc::CLOCK_BOOTTIME => emscripten_get_now(),
        _ => return __real_clock_gettime(clk, ts),
    };
    let ns = (ms * 1e6) as i64;
    (*ts).tv_sec = (ns / 1_000_000_000) as _;
    (*ts).tv_nsec = (ns % 1_000_000_000) as _;
    0
}
