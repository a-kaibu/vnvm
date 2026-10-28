//! In-process pipes and unix stream sockets, and the fds of files in `vfs`.
//!
//! Wine's unix side and wineserver live in one wasm module, so the kernel objects they talk
//! through (the server socket, request/reply/wait pipes, fds passed with SCM_RIGHTS) are
//! emulated here. Emulated fds start at `FD_BASE`; every other fd goes to emscripten's libc.
//! All state sits behind one lock and one condition variable: few threads, simple wakeups.

use std::collections::{HashMap, VecDeque};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use libc::{c_int, c_void, iovec, msghdr, pollfd, size_t, sockaddr, socklen_t, ssize_t};

use crate::set_errno;

pub const FD_BASE: c_int = 10000;

type Id = u32;

/// A descriptor in flight: an emulated open file description (holding a reference) or a real
/// fd dup'ed by the sender (same process, so the number is valid for the receiver too).
#[derive(Clone, Copy)]
enum Passed {
    Ours(Id),
    Real(c_int),
}

extern "C" {
    fn __real_dup(fd: c_int) -> c_int;
    fn __real_close(fd: c_int) -> c_int;
}

#[derive(Default)]
struct Stream {
    buf: VecDeque<u8>,
    rpos: u64,
    wpos: u64,
    /// fds passed with SCM_RIGHTS, tagged with the stream position of their first byte
    fds: VecDeque<(u64, Vec<Passed>)>,
    readers: u32,
    writers: u32,
}

enum Kind {
    PipeRead(Id),
    PipeWrite(Id),
    Sock { rx: Id, tx: Id },
    Unbound { path: Option<String>, passcred: bool },
    Listener { path: String, queue: VecDeque<Id> },
    /// an open file or directory of `vfs`
    Vfs(u32),
}

struct Open {
    refs: u32,
    nonblock: bool,
    kind: Kind,
}

struct FdEnt {
    open: Id,
    cloexec: bool,
    /// the process that holds it (closed when that process dies)
    owner: i32,
}

#[derive(Default)]
struct Kern {
    fds: HashMap<c_int, FdEnt>,
    opens: HashMap<Id, Open>,
    streams: HashMap<Id, Stream>,
    listeners: HashMap<String, Id>,
    next_id: Id,
    /// fd numbers are handed out in increasing order, so a stale number held by a dying
    /// thread doesn't quickly name someone else's new fd
    next_fd: c_int,
}

static KERN: Mutex<Option<Kern>> = Mutex::new(None);
static COND: Condvar = Condvar::new();

struct Guard(MutexGuard<'static, Option<Kern>>);

impl std::ops::Deref for Guard {
    type Target = Kern;
    fn deref(&self) -> &Kern {
        self.0.as_ref().unwrap()
    }
}
impl std::ops::DerefMut for Guard {
    fn deref_mut(&mut self) -> &mut Kern {
        self.0.as_mut().unwrap()
    }
}

fn lock() -> Guard {
    // a thread of a killed process must not touch fds: their numbers may belong to others now
    if crate::proc::current() != crate::proc::HOST_PID && crate::cpu::self_killed() {
        crate::proc::check_alive();
    }
    lock_raw()
}

fn lock_raw() -> Guard {
    let mut g = KERN.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_none() {
        *g = Some(Kern { next_id: 1, next_fd: FD_BASE, ..Default::default() });
    }
    Guard(g)
}

fn wait(g: Guard, deadline: Option<Instant>) -> Guard {
    if crate::proc::is_dead(crate::proc::current()) {
        drop(g);
        crate::proc::check_alive();
        return lock();
    }
    let g = match deadline {
        None => Guard(COND.wait(g.0).unwrap_or_else(|e| e.into_inner())),
        Some(d) => {
            let left = d.saturating_duration_since(Instant::now());
            Guard(COND.wait_timeout(g.0, left).unwrap_or_else(|e| e.into_inner()).0)
        }
    };
    if crate::proc::is_dead(crate::proc::current()) {
        drop(g);
        crate::proc::check_alive();
        return lock();
    }
    g
}

/// Closes every fd held by process `pid`.
pub fn close_owned(pid: i32) {
    let mut k = lock_raw();
    let mine: Vec<c_int> = k.fds.iter().filter(|(_, e)| e.owner == pid).map(|(&fd, _)| fd).collect();
    for fd in mine {
        if let Some(e) = k.fds.remove(&fd) {
            k.unref(e.open);
        }
    }
    changed();
}

/// Hands fd over to process `pid`.
pub fn give(fd: c_int, pid: i32) {
    if let Some(e) = lock().fds.get_mut(&fd) {
        e.owner = pid;
    }
}

fn changed() {
    COND.notify_all();
}

pub fn is_ours(fd: c_int) -> bool {
    fd >= FD_BASE
}

fn err<T: From<i8>>(e: c_int) -> T {
    set_errno(e);
    T::from(-1)
}

impl Kern {
    fn id(&mut self) -> Id {
        self.next_id += 1;
        self.next_id
    }

    fn new_stream(&mut self) -> Id {
        let id = self.id();
        self.streams.insert(id, Stream::default());
        id
    }

    fn new_open(&mut self, kind: Kind) -> Id {
        match kind {
            Kind::PipeRead(s) => self.streams.get_mut(&s).unwrap().readers += 1,
            Kind::PipeWrite(s) => self.streams.get_mut(&s).unwrap().writers += 1,
            Kind::Sock { rx, tx } => {
                self.streams.get_mut(&rx).unwrap().readers += 1;
                self.streams.get_mut(&tx).unwrap().writers += 1;
            }
            _ => {}
        }
        let id = self.id();
        self.opens.insert(id, Open { refs: 0, nonblock: false, kind });
        id
    }

    fn install(&mut self, open: Id, cloexec: bool, min: c_int) -> c_int {
        let mut fd = if min > FD_BASE { min } else { self.next_fd };
        while self.fds.contains_key(&fd) {
            fd = if fd == c_int::MAX { FD_BASE } else { fd + 1 };
        }
        if min <= FD_BASE {
            self.next_fd = if fd == c_int::MAX { FD_BASE } else { fd + 1 };
        }
        self.opens.get_mut(&open).unwrap().refs += 1;
        self.fds.insert(fd, FdEnt { open, cloexec, owner: crate::proc::current() });
        fd
    }

    fn open_of(&self, fd: c_int) -> Option<Id> {
        self.fds.get(&fd).map(|e| e.open)
    }

    fn unref(&mut self, open: Id) {
        let o = self.opens.get_mut(&open).unwrap();
        o.refs -= 1;
        if o.refs > 0 {
            return;
        }
        let o = self.opens.remove(&open).unwrap();
        match o.kind {
            Kind::PipeRead(s) => self.drop_end(s, true),
            Kind::PipeWrite(s) => self.drop_end(s, false),
            Kind::Sock { rx, tx } => {
                self.drop_end(rx, true);
                self.drop_end(tx, false);
            }
            Kind::Listener { path, queue } => {
                self.listeners.remove(&path);
                for q in queue {
                    self.unref(q);
                }
            }
            Kind::Unbound { .. } => {}
            Kind::Vfs(h) => crate::vfs::release(h),
        }
        changed();
    }

    fn release(&mut self, p: Passed) {
        match p {
            Passed::Ours(id) => self.unref(id),
            Passed::Real(fd) => unsafe {
                __real_close(fd);
            },
        }
    }

    fn drop_end(&mut self, s: Id, reader: bool) {
        let st = self.streams.get_mut(&s).unwrap();
        if reader {
            st.readers -= 1;
        } else {
            st.writers -= 1;
        }
        if st.readers == 0 {
            // nobody can receive what is still queued
            st.buf.clear();
            let pending: Vec<Passed> = st.fds.drain(..).flat_map(|(_, v)| v).collect();
            let gone = st.writers == 0;
            for p in pending {
                self.release(p);
            }
            if gone {
                self.streams.remove(&s);
            }
        }
    }

    fn read_stream(&self, open: Id) -> Option<Id> {
        match self.opens.get(&open)?.kind {
            Kind::PipeRead(s) => Some(s),
            Kind::Sock { rx, .. } => Some(rx),
            _ => None,
        }
    }

    fn write_stream(&self, open: Id) -> Option<Id> {
        match self.opens.get(&open)?.kind {
            Kind::PipeWrite(s) => Some(s),
            Kind::Sock { tx, .. } => Some(tx),
            _ => None,
        }
    }

    fn revents(&self, fd: c_int, events: i16) -> i16 {
        let Some(open) = self.open_of(fd) else { return libc::POLLNVAL };
        let mut r = 0;
        if let Some(s) = self.read_stream(open) {
            let st = &self.streams[&s];
            if !st.buf.is_empty() || !st.fds.is_empty() {
                r |= libc::POLLIN;
            }
            if st.writers == 0 {
                r |= libc::POLLHUP | libc::POLLIN;
            }
        }
        if let Some(s) = self.write_stream(open) {
            let st = &self.streams[&s];
            if st.readers == 0 {
                r |= libc::POLLERR | libc::POLLHUP;
            } else {
                r |= libc::POLLOUT;
            }
        }
        match &self.opens[&open].kind {
            Kind::Listener { queue, .. } if !queue.is_empty() => r |= libc::POLLIN,
            Kind::Vfs(_) => r |= libc::POLLIN | libc::POLLOUT,
            _ => {}
        }
        r & (events | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL)
    }
}

fn nonblock(k: &Kern, open: Id, flags: c_int) -> bool {
    k.opens[&open].nonblock || flags & libc::MSG_DONTWAIT != 0
}

// ---------------------------------------------------------------------------------------------
// creation

/// A new fd for a `vfs` handle.
pub fn vfs_install(h: u32, flags: c_int) -> c_int {
    let mut k = lock();
    let o = k.new_open(Kind::Vfs(h));
    k.opens.get_mut(&o).unwrap().nonblock = flags & libc::O_NONBLOCK != 0;
    k.install(o, flags & libc::O_CLOEXEC != 0, 0)
}

/// The `vfs` handle behind fd, if it is one.
pub fn vfs_handle(fd: c_int) -> Option<u32> {
    if !is_ours(fd) {
        return None;
    }
    let k = lock();
    match k.opens.get(&k.open_of(fd)?)?.kind {
        Kind::Vfs(h) => Some(h),
        _ => None,
    }
}

pub fn pipe2(fds: &mut [c_int; 2], flags: c_int) -> c_int {
    let mut k = lock();
    let s = k.new_stream();
    let r = k.new_open(Kind::PipeRead(s));
    let w = k.new_open(Kind::PipeWrite(s));
    let nb = flags & libc::O_NONBLOCK != 0;
    k.opens.get_mut(&r).unwrap().nonblock = nb;
    k.opens.get_mut(&w).unwrap().nonblock = nb;
    let ce = flags & libc::O_CLOEXEC != 0;
    fds[0] = k.install(r, ce, 0);
    fds[1] = k.install(w, ce, 0);
    0
}

pub fn socketpair(ty: c_int, fds: &mut [c_int; 2]) -> c_int {
    let mut k = lock();
    let a = k.new_stream();
    let b = k.new_stream();
    let x = k.new_open(Kind::Sock { rx: a, tx: b });
    let y = k.new_open(Kind::Sock { rx: b, tx: a });
    let nb = ty & libc::SOCK_NONBLOCK != 0;
    k.opens.get_mut(&x).unwrap().nonblock = nb;
    k.opens.get_mut(&y).unwrap().nonblock = nb;
    let ce = ty & libc::SOCK_CLOEXEC != 0;
    fds[0] = k.install(x, ce, 0);
    fds[1] = k.install(y, ce, 0);
    0
}

pub fn socket(ty: c_int) -> c_int {
    let mut k = lock();
    let o = k.new_open(Kind::Unbound { path: None, passcred: false });
    k.opens.get_mut(&o).unwrap().nonblock = ty & libc::SOCK_NONBLOCK != 0;
    k.install(o, ty & libc::SOCK_CLOEXEC != 0, 0)
}

/// Absolute path of a unix socket address, resolved against the current directory.
unsafe fn sun_path(addr: *const sockaddr, len: socklen_t) -> Option<String> {
    if addr.is_null() || (*addr).sa_family as c_int != libc::AF_UNIX {
        return None;
    }
    let un = addr as *const libc::sockaddr_un;
    let max = (len as usize).saturating_sub(2).min(108);
    let bytes = std::slice::from_raw_parts((*un).sun_path.as_ptr() as *const u8, max);
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let p = String::from_utf8_lossy(&bytes[..end]).into_owned();
    if p.starts_with('/') {
        return Some(p);
    }
    let cwd = std::env::current_dir().ok()?;
    Some(format!("{}/{}", cwd.display(), p))
}

pub unsafe fn bind(fd: c_int, addr: *const sockaddr, len: socklen_t) -> c_int {
    let Some(path) = sun_path(addr, len) else { return err(libc::EAFNOSUPPORT) };
    let mut k = lock();
    let Some(o) = k.open_of(fd) else { return err(libc::EBADF) };
    if k.listeners.contains_key(&path) {
        return err(libc::EADDRINUSE);
    }
    match &mut k.opens.get_mut(&o).unwrap().kind {
        Kind::Unbound { path: p, .. } => *p = Some(path.clone()),
        _ => return err(libc::EINVAL),
    }
    // leave a socket file behind so stat() and unlink() of the path behave
    let c = std::ffi::CString::new(path).unwrap();
    let f = libc::open(c.as_ptr(), libc::O_CREAT | libc::O_WRONLY, 0o600);
    if f >= 0 {
        libc::close(f);
    }
    0
}

pub fn listen(fd: c_int) -> c_int {
    let mut k = lock();
    let Some(o) = k.open_of(fd) else { return err(libc::EBADF) };
    let path = match &k.opens[&o].kind {
        Kind::Unbound { path: Some(p), .. } => p.clone(),
        Kind::Listener { .. } => return 0,
        _ => return err(libc::EINVAL),
    };
    k.opens.get_mut(&o).unwrap().kind = Kind::Listener { path: path.clone(), queue: VecDeque::new() };
    k.listeners.insert(path, o);
    0
}

pub unsafe fn connect(fd: c_int, addr: *const sockaddr, len: socklen_t) -> c_int {
    let Some(path) = sun_path(addr, len) else { return err(libc::EAFNOSUPPORT) };
    let mut k = lock();
    let Some(o) = k.open_of(fd) else { return err(libc::EBADF) };
    if !matches!(k.opens[&o].kind, Kind::Unbound { .. }) {
        return err(libc::EISCONN);
    }
    let Some(&l) = k.listeners.get(&path) else { return err(libc::ECONNREFUSED) };
    let a = k.new_stream();
    let b = k.new_stream();
    // our end keeps its open file description; the peer end waits in the accept queue
    k.streams.get_mut(&a).unwrap().readers += 1;
    k.streams.get_mut(&b).unwrap().writers += 1;
    k.opens.get_mut(&o).unwrap().kind = Kind::Sock { rx: a, tx: b };
    let peer = k.new_open(Kind::Sock { rx: b, tx: a });
    k.opens.get_mut(&peer).unwrap().refs = 1;
    if let Kind::Listener { queue, .. } = &mut k.opens.get_mut(&l).unwrap().kind {
        queue.push_back(peer);
    }
    changed();
    0
}

pub fn accept(fd: c_int, flags: c_int) -> c_int {
    let mut k = lock();
    loop {
        let Some(o) = k.open_of(fd) else { return err(libc::EBADF) };
        let nb = k.opens[&o].nonblock;
        let peer = match &mut k.opens.get_mut(&o).unwrap().kind {
            Kind::Listener { queue, .. } => queue.pop_front(),
            _ => return err(libc::EINVAL),
        };
        if let Some(p) = peer {
            k.opens.get_mut(&p).unwrap().nonblock = flags & libc::SOCK_NONBLOCK != 0;
            let nfd = k.install(p, flags & libc::SOCK_CLOEXEC != 0, 0);
            k.unref(p); // the queue's reference
            return nfd;
        }
        if nb {
            return err(libc::EAGAIN);
        }
        k = wait(k, None);
    }
}

// ---------------------------------------------------------------------------------------------
// data transfer

/// Reads into `iov`; passed fds (if any are due) are installed and returned through `fds_out`.
unsafe fn recv(fd: c_int, iov: &[iovec], flags: c_int, mut fds_out: Option<&mut Vec<c_int>>) -> ssize_t {
    let mut k = lock();
    loop {
        let Some(o) = k.open_of(fd) else { return err(libc::EBADF) };
        let Some(s) = k.read_stream(o) else {
            return err(if matches!(k.opens[&o].kind, Kind::Unbound { .. }) { libc::ENOTCONN } else { libc::EBADF });
        };
        let want: usize = iov.iter().map(|v| v.iov_len).sum();
        let st = k.streams.get_mut(&s).unwrap();
        if !st.buf.is_empty() || want == 0 {
            // don't read past the next batch of passed fds unless it is attached to our first byte
            let mut n = want.min(st.buf.len());
            if let Some(&(pos, _)) = st.fds.front() {
                if pos > st.rpos {
                    n = n.min((pos - st.rpos) as usize);
                }
            }
            let mut passed = Vec::new();
            if let Some(&(pos, _)) = st.fds.front() {
                if pos == st.rpos && n > 0 && flags & libc::MSG_PEEK == 0 {
                    passed = st.fds.pop_front().unwrap().1;
                    if let Some(&(next, _)) = st.fds.front() {
                        n = n.min((next - st.rpos) as usize);
                    }
                }
            }
            let mut copied = 0;
            for v in iov {
                let take = (n - copied).min(v.iov_len);
                let dst = std::slice::from_raw_parts_mut(v.iov_base as *mut u8, take);
                for (i, b) in st.buf.iter().skip(copied).take(take).enumerate() {
                    dst[i] = *b;
                }
                copied += take;
                if copied == n {
                    break;
                }
            }
            if flags & libc::MSG_PEEK == 0 {
                st.buf.drain(..n);
                st.rpos += n as u64;
            }
            let cloexec = flags & libc::MSG_CMSG_CLOEXEC != 0;
            for p in passed {
                let Some(out) = fds_out.as_deref_mut() else {
                    k.release(p);
                    continue;
                };
                match p {
                    Passed::Ours(id) => {
                        out.push(k.install(id, cloexec, 0));
                        k.unref(id); // the in-flight reference
                    }
                    Passed::Real(fd) => out.push(fd),
                }
            }
            changed();
            return n as ssize_t;
        }
        if st.writers == 0 {
            return 0;
        }
        if nonblock(&k, o, flags) {
            return err(libc::EAGAIN);
        }
        k = wait(k, None);
    }
}

unsafe fn send(fd: c_int, iov: &[iovec], passed: &[c_int]) -> ssize_t {
    let mut k = lock();
    let Some(o) = k.open_of(fd) else { return err(libc::EBADF) };
    let Some(s) = k.write_stream(o) else {
        return err(if matches!(k.opens[&o].kind, Kind::Unbound { .. }) { libc::ENOTCONN } else { libc::EBADF });
    };
    if k.streams[&s].readers == 0 {
        return err(libc::EPIPE);
    }
    let mut ids = Vec::new();
    for &p in passed {
        if is_ours(p) {
            let Some(id) = k.open_of(p) else {
                for q in ids {
                    k.release(q);
                }
                return err(libc::EBADF);
            };
            k.opens.get_mut(&id).unwrap().refs += 1;
            ids.push(Passed::Ours(id));
        } else {
            let d = __real_dup(p);
            if d < 0 {
                for q in ids {
                    k.release(q);
                }
                return -1;
            }
            ids.push(Passed::Real(d));
        }
    }
    let st = k.streams.get_mut(&s).unwrap();
    if !ids.is_empty() {
        st.fds.push_back((st.wpos, ids));
    }
    let mut n = 0;
    for v in iov {
        st.buf.extend(std::slice::from_raw_parts(v.iov_base as *const u8, v.iov_len));
        n += v.iov_len;
    }
    st.wpos += n as u64;
    changed();
    n as ssize_t
}

pub unsafe fn read(fd: c_int, buf: *mut c_void, len: size_t) -> ssize_t {
    recv(fd, &[iovec { iov_base: buf, iov_len: len }], 0, None)
}

pub unsafe fn readv(fd: c_int, iov: *const iovec, cnt: c_int) -> ssize_t {
    recv(fd, std::slice::from_raw_parts(iov, cnt.max(0) as usize), 0, None)
}

pub unsafe fn write(fd: c_int, buf: *const c_void, len: size_t) -> ssize_t {
    send(fd, &[iovec { iov_base: buf as *mut c_void, iov_len: len }], &[])
}

pub unsafe fn writev(fd: c_int, iov: *const iovec, cnt: c_int) -> ssize_t {
    send(fd, std::slice::from_raw_parts(iov, cnt.max(0) as usize), &[])
}

pub unsafe fn recvfrom(fd: c_int, buf: *mut c_void, len: size_t, flags: c_int) -> ssize_t {
    recv(fd, &[iovec { iov_base: buf, iov_len: len }], flags, None)
}

pub unsafe fn sendto(fd: c_int, buf: *const c_void, len: size_t) -> ssize_t {
    send(fd, &[iovec { iov_base: buf as *mut c_void, iov_len: len }], &[])
}

pub unsafe fn sendmsg(fd: c_int, msg: *const msghdr) -> ssize_t {
    let m = &*msg;
    let mut passed = Vec::new();
    if !m.msg_control.is_null() && m.msg_controllen as usize >= std::mem::size_of::<libc::cmsghdr>() {
        let mut c = libc::CMSG_FIRSTHDR(m);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let n = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / 4;
                let data = libc::CMSG_DATA(c) as *const c_int;
                for i in 0..n {
                    passed.push(data.add(i).read_unaligned());
                }
            }
            c = libc::CMSG_NXTHDR(m, c);
        }
    }
    send(fd, std::slice::from_raw_parts(m.msg_iov, m.msg_iovlen as usize), &passed)
}

pub unsafe fn recvmsg(fd: c_int, msg: *mut msghdr, flags: c_int) -> ssize_t {
    let m = &mut *msg;
    let mut fds = Vec::new();
    let n = recv(fd, std::slice::from_raw_parts(m.msg_iov, m.msg_iovlen as usize), flags, Some(&mut fds));
    m.msg_flags = 0;
    let space = m.msg_controllen as usize;
    m.msg_controllen = 0;
    if n >= 0 && !fds.is_empty() {
        let need = libc::CMSG_SPACE((fds.len() * 4) as u32) as usize;
        if m.msg_control.is_null() || space < need {
            m.msg_flags |= libc::MSG_CTRUNC;
            let mut k = lock();
            for f in fds {
                if let Some(e) = k.fds.remove(&f) {
                    k.unref(e.open);
                } else if !is_ours(f) {
                    __real_close(f);
                }
            }
        } else {
            m.msg_controllen = need as _;
            let c = libc::CMSG_FIRSTHDR(m);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN((fds.len() * 4) as u32) as _;
            let data = libc::CMSG_DATA(c) as *mut c_int;
            for (i, f) in fds.iter().enumerate() {
                data.add(i).write_unaligned(*f);
            }
        }
    }
    n
}

// ---------------------------------------------------------------------------------------------
// descriptor management

pub fn close(fd: c_int) -> c_int {
    let mut k = lock();
    match k.fds.remove(&fd) {
        Some(e) => {
            k.unref(e.open);
            0
        }
        None => err(libc::EBADF),
    }
}

pub fn dup(fd: c_int, min: c_int, cloexec: bool) -> c_int {
    let mut k = lock();
    match k.open_of(fd) {
        Some(o) => k.install(o, cloexec, min),
        None => err(libc::EBADF),
    }
}

pub fn dup2(fd: c_int, to: c_int, cloexec: bool) -> c_int {
    if fd == to {
        return to;
    }
    if !is_ours(to) {
        return err(libc::EINVAL);
    }
    let mut k = lock();
    let Some(o) = k.open_of(fd) else { return err(libc::EBADF) };
    if let Some(e) = k.fds.remove(&to) {
        k.unref(e.open);
    }
    k.install(o, cloexec, to)
}

pub fn fcntl(fd: c_int, cmd: c_int, arg: c_int) -> c_int {
    let mut k = lock();
    let Some(o) = k.open_of(fd) else { return err(libc::EBADF) };
    match cmd {
        libc::F_GETFD => k.fds[&fd].cloexec as c_int,
        libc::F_SETFD => {
            k.fds.get_mut(&fd).unwrap().cloexec = arg & libc::FD_CLOEXEC != 0;
            0
        }
        libc::F_GETFL => {
            let mut fl = libc::O_RDWR;
            if k.opens[&o].nonblock {
                fl |= libc::O_NONBLOCK;
            }
            fl
        }
        libc::F_SETFL => {
            k.opens.get_mut(&o).unwrap().nonblock = arg & libc::O_NONBLOCK != 0;
            0
        }
        libc::F_DUPFD => k.install(o, false, arg),
        libc::F_DUPFD_CLOEXEC => k.install(o, true, arg),
        _ => 0,
    }
}

/// Bytes ready to read (FIONREAD).
pub fn nread(fd: c_int) -> c_int {
    let k = lock();
    let Some(o) = k.open_of(fd) else { return err(libc::EBADF) };
    match k.read_stream(o) {
        Some(s) => k.streams[&s].buf.len() as c_int,
        None => 0,
    }
}

pub fn shutdown(fd: c_int, how: c_int) -> c_int {
    let mut k = lock();
    let Some(o) = k.open_of(fd) else { return err(libc::EBADF) };
    // model a shutdown by detaching the affected directions from this description
    if let Kind::Sock { rx, tx } = k.opens[&o].kind {
        if how == libc::SHUT_RD || how == libc::SHUT_RDWR {
            k.drop_end(rx, true);
        }
        if how == libc::SHUT_WR || how == libc::SHUT_RDWR {
            k.drop_end(tx, false);
        }
        let new = match how {
            libc::SHUT_RD => Kind::PipeWrite(tx),
            libc::SHUT_WR => Kind::PipeRead(rx),
            _ => Kind::Unbound { path: None, passcred: false },
        };
        k.opens.get_mut(&o).unwrap().kind = new;
        changed();
    }
    0
}

pub fn setsockopt_passcred(fd: c_int, on: bool) -> c_int {
    let mut k = lock();
    let Some(o) = k.open_of(fd) else { return err(libc::EBADF) };
    if let Kind::Unbound { passcred, .. } = &mut k.opens.get_mut(&o).unwrap().kind {
        *passcred = on;
    }
    0
}

pub fn is_socket(fd: c_int) -> Option<bool> {
    let k = lock();
    let o = k.open_of(fd)?;
    Some(!matches!(k.opens[&o].kind, Kind::PipeRead(_) | Kind::PipeWrite(_) | Kind::Vfs(_)))
}

// ---------------------------------------------------------------------------------------------
// poll

/// poll() over any mix of emulated and real fds; real fds are polled without blocking.
pub unsafe fn poll(fds: *mut pollfd, n: libc::nfds_t, timeout: c_int, real_poll: unsafe fn(*mut pollfd, libc::nfds_t, c_int) -> c_int) -> c_int {
    let fds = std::slice::from_raw_parts_mut(fds, n as usize);
    let deadline = (timeout >= 0).then(|| Instant::now() + Duration::from_millis(timeout as u64));
    let real: Vec<usize> = (0..fds.len()).filter(|&i| fds[i].fd >= 0 && !is_ours(fds[i].fd)).collect();
    let mut k = lock();
    loop {
        let mut ready = 0;
        for p in fds.iter_mut() {
            p.revents = 0;
            if is_ours(p.fd) {
                p.revents = k.revents(p.fd, p.events);
            }
        }
        if !real.is_empty() {
            let mut tmp: Vec<pollfd> = real.iter().map(|&i| fds[i]).collect();
            drop(k);
            real_poll(tmp.as_mut_ptr(), tmp.len() as _, 0);
            k = lock();
            for (j, &i) in real.iter().enumerate() {
                fds[i].revents = tmp[j].revents;
            }
        }
        for p in fds.iter() {
            if p.revents != 0 {
                ready += 1;
            }
        }
        if ready > 0 || timeout == 0 || deadline.is_some_and(|d| Instant::now() >= d) {
            return ready;
        }
        // real fds can't wake us; re-check them now and then
        let step = if real.is_empty() { deadline } else { Some(Instant::now() + Duration::from_millis(10)).min(deadline.or(Some(Instant::now() + Duration::from_millis(10)))) };
        k = wait(k, step);
    }
}
