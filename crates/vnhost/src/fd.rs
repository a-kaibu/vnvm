//! In-process pipes and unix stream sockets, and the fds of files in `vfs`.
//!
//! Wine's unix side and wineserver live in one wasm module, so the kernel objects they talk
//! through (the server socket, request/reply/wait pipes, fds passed with SCM_RIGHTS) are
//! emulated here. Emulated fds start at `FD_BASE`; every other fd goes to emscripten's libc.
//! All state sits behind one lock. A blocked thread waits on its own condition variable,
//! registered under the objects (streams, listeners) it waits for, so a write wakes only the
//! threads that read that stream (waking every thread on every change cost ~0.4ms per
//! wineserver request with a few dozen threads).

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
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
    /// an epoll instance; its state is in `Kern::epolls` under this open's id
    Epoll,
    /// /dev/ntsync, and one of its objects
    NtsyncDev,
    Sync(Sync),
}

/// An ntsync object (see the ntsync section below).
enum Sync {
    Sem { count: u32, max: u32 },
    Mutex { owner: u32, count: u32, dead: bool },
    /// `pulses`: waiters a PulseEvent still owes a wakeup to (an auto-reset event's waiters
    /// take one each; a manual one's are all released)
    Event { manual: bool, signaled: bool, pulses: u32 },
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

/// The maps are keyed by small integers and looked up a few times per fd on every poll (the
/// wineserver polls all its clients' pipes, ~170 fds, about a thousand times a second while a
/// game idles): one multiply instead of SipHash.
#[derive(Default)]
struct IntHasher(u64);

impl std::hash::Hasher for IntHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(5) ^ b as u64).wrapping_mul(0x51_7cc1_b727_220a_95);
        }
    }
    fn write_u32(&mut self, v: u32) {
        self.0 = (self.0 ^ v as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
    fn write_i32(&mut self, v: i32) {
        self.write_u32(v as u32);
    }
}

type IntMap<K, V> = HashMap<K, V, std::hash::BuildHasherDefault<IntHasher>>;

#[derive(Default)]
struct Kern {
    fds: IntMap<c_int, FdEnt>,
    opens: IntMap<Id, Open>,
    streams: IntMap<Id, Stream>,
    listeners: HashMap<String, Id>,
    next_id: Id,
    /// fd numbers are handed out in increasing order, so a stale number held by a dying
    /// thread doesn't quickly name someone else's new fd
    next_fd: c_int,
    /// object (stream or listener open) -> the threads blocked on it
    waiters: IntMap<Id, Vec<Arc<Condvar>>>,
    /// threads blocked on many objects (the wineserver's poll): woken by any change
    waiters_any: Vec<Arc<Condvar>>,
    epolls: IntMap<Id, Epoll>,
    /// object -> the (epoll, fd) interests it may make ready
    watch: IntMap<Id, Vec<(Id, c_int)>>,
}

/// An fd registered with an epoll instance.
struct Interest {
    events: u32,
    data: u64,
    /// the open file description it was added with (None: a real fd)
    open: Option<Id>,
    keys: Vec<Id>,
}

/// epoll for the wineserver, which otherwise polls all ~170 of its fds about a thousand times a
/// second while a game idles. Level-triggered: only the fds that one of their objects woke since
/// (`cand`), and the ones last found ready, are checked again.
#[derive(Default)]
struct Epoll {
    interest: IntMap<c_int, Interest>,
    cand: RefCell<Vec<c_int>>,
}

/// A wait on more objects than this goes in `Kern::waiters_any`.
const MANY_KEYS: usize = 16;

static KERN: Mutex<Option<Kern>> = Mutex::new(None);

thread_local! {
    static PARKER: Arc<Condvar> = Arc::new(Condvar::new());
}

/// Bumped by every wake: a thread about to block spins on it for a moment first, since the
/// answer to a wineserver request usually comes within microseconds and sleeping and waking a
/// wasm thread costs tens of them.
static WAKES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
const SPIN: Duration = Duration::from_micros(50);

/// How many turns of the spin loop take `SPIN`, measured once. The loop doesn't read the clock:
/// performance.now() costs about a microsecond in a Chromium worker, and reading it while
/// spinning was a fifth of the browser's own time during the story.
fn spin_iters() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static ITERS: AtomicU32 = AtomicU32::new(0);
    let n = ITERS.load(Ordering::Relaxed);
    if n != 0 {
        return n;
    }
    const TRY: u32 = 200_000;
    let t = Instant::now();
    let mut x = 0;
    for _ in 0..TRY {
        x |= WAKES.load(Ordering::Acquire);
        std::hint::spin_loop();
    }
    std::hint::black_box(x);
    let ns = t.elapsed().as_nanos().max(1) as u64;
    let us: u64 = std::env::var("VNVM_SPIN_US").ok().and_then(|v| v.parse().ok()).unwrap_or(SPIN.as_micros() as u64);
    let n = (TRY as u64 * us * 1000 / ns).clamp(1, 1 << 20) as u32;
    ITERS.store(n, Ordering::Relaxed);
    n
}

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
    // the lock is held for microseconds at a time: spin for it rather than sleep (std's own
    // spin is short, and a sleeping wasm thread takes tens of microseconds to wake)
    let mut got = None;
    for _ in 0..spin_iters() {
        match KERN.try_lock() {
            Ok(g) => got = Some(g),
            Err(std::sync::TryLockError::Poisoned(e)) => got = Some(e.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => {
                std::hint::spin_loop();
                continue;
            }
        }
        break;
    }
    let mut g = got.unwrap_or_else(|| KERN.lock().unwrap_or_else(|e| e.into_inner()));
    if g.is_none() {
        *g = Some(Kern { next_id: 1, next_fd: FD_BASE, ..Default::default() });
    }
    Guard(g)
}

/// Blocks until one of `keys` changes (or anything, after a process died), or `deadline`.
fn wait(g: Guard, keys: &[Id], deadline: Option<Instant>) -> Guard {
    wait_ex(g, keys, deadline, keys.len() <= MANY_KEYS)
}

fn wait_ex(g: Guard, keys: &[Id], deadline: Option<Instant>, spin: bool) -> Guard {
    if crate::proc::is_dead(crate::proc::current()) {
        drop(g);
        crate::proc::check_alive();
        return lock();
    }
    let seen = WAKES.load(std::sync::atomic::Ordering::Acquire);
    drop(g);
    // (not for the wineserver's wait: its next request may be long in coming)
    let many = keys.len() > MANY_KEYS;
    if spin {
        for _ in 0..spin_iters() {
            if WAKES.load(std::sync::atomic::Ordering::Acquire) != seen {
                break;
            }
            std::hint::spin_loop();
        }
    }
    let mut g = lock_raw();
    if WAKES.load(std::sync::atomic::Ordering::Acquire) != seen {
        return g;
    }
    let cv = PARKER.with(|p| p.clone());
    if many {
        g.waiters_any.push(cv.clone());
    } else {
        for &k in keys {
            g.waiters.entry(k).or_default().push(cv.clone());
        }
    }
    let mut g = match deadline {
        None => Guard(cv.wait(g.0).unwrap_or_else(|e| e.into_inner())),
        Some(d) => {
            let left = d.saturating_duration_since(Instant::now());
            Guard(cv.wait_timeout(g.0, left).unwrap_or_else(|e| e.into_inner()).0)
        }
    };
    if many {
        g.waiters_any.retain(|c| !Arc::ptr_eq(c, &cv));
    } else {
        for &k in keys {
            if let Some(v) = g.waiters.get_mut(&k) {
                v.retain(|c| !Arc::ptr_eq(c, &cv));
                if v.is_empty() {
                    g.waiters.remove(&k);
                }
            }
        }
    }
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
    // its blocked threads notice they are dead
    for c in k.waiters.values().flatten().chain(&k.waiters_any) {
        c.notify_one();
    }
}

/// Wakes every blocked thread (to look at its signals).
pub fn kick() {
    let k = lock_raw();
    WAKES.fetch_add(1, std::sync::atomic::Ordering::Release);
    for c in k.waiters.values().flatten().chain(&k.waiters_any) {
        c.notify_one();
    }
}

/// Hands fd over to process `pid`.
pub fn give(fd: c_int, pid: i32) {
    if let Some(e) = lock().fds.get_mut(&fd) {
        e.owner = pid;
    }
}


pub fn is_ours(fd: c_int) -> bool {
    fd >= FD_BASE
}

fn err<T: From<i8>>(e: c_int) -> T {
    set_errno(e);
    T::from(-1)
}

impl Kern {
    /// Wakes the threads blocked on `key`.
    fn wake(&self, key: Id) {
        WAKES.fetch_add(1, std::sync::atomic::Ordering::Release);
        for c in self.waiters.get(&key).into_iter().flatten().chain(&self.waiters_any) {
            c.notify_one();
        }
        for &(ep, fd) in self.watch.get(&key).into_iter().flatten() {
            if let Some(e) = self.epolls.get(&ep) {
                e.cand.borrow_mut().push(fd);
            }
            for c in self.waiters.get(&ep).into_iter().flatten() {
                c.notify_one();
            }
        }
    }

    fn unwatch(&mut self, ep: Id, fd: c_int, keys: &[Id]) {
        for k in keys {
            if let Some(v) = self.watch.get_mut(k) {
                v.retain(|&w| w != (ep, fd));
                if v.is_empty() {
                    self.watch.remove(k);
                }
            }
        }
    }

    /// What a thread blocked on `fd` waits for.
    fn keys(&self, fd: c_int, out: &mut Vec<Id>) {
        if let Some(o) = self.open_of(fd) {
            out.push(o);
            out.extend(self.read_stream(o));
            out.extend(self.write_stream(o));
        }
    }

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
            Kind::Epoll => {
                let e = self.epolls.remove(&open).unwrap();
                for (fd, i) in e.interest {
                    self.unwatch(open, fd, &i.keys);
                }
            }
            Kind::NtsyncDev | Kind::Sync(_) => {}
        }
        self.wake(open);
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
        self.wake(s);
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
    k.wake(l);
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
        k = wait(k, &[o], None);
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
            k.wake(s);
            return n as ssize_t;
        }
        if st.writers == 0 {
            return 0;
        }
        if nonblock(&k, o, flags) {
            return err(libc::EAGAIN);
        }
        k = wait(k, &[o, s], None);
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
    k.wake(s);
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
        k.wake(o);
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
    Some(!matches!(k.opens[&o].kind, Kind::PipeRead(_) | Kind::PipeWrite(_) | Kind::Vfs(_) | Kind::Epoll | Kind::NtsyncDev | Kind::Sync(_)))
}

// ---------------------------------------------------------------------------------------------
// poll

/// poll() over any mix of emulated and real fds; real fds are polled without blocking.
pub unsafe fn poll(fds: *mut pollfd, n: libc::nfds_t, timeout: c_int, real_poll: unsafe fn(*mut pollfd, libc::nfds_t, c_int) -> c_int) -> c_int {
    let fds = std::slice::from_raw_parts_mut(fds, n as usize);
    let deadline = (timeout >= 0).then(|| Instant::now() + Duration::from_millis(timeout as u64));
    let real: Vec<usize> = (0..fds.len()).filter(|&i| fds[i].fd >= 0 && !is_ours(fds[i].fd)).collect();
    let mut k = lock();
    let mut keys = Vec::new();
    for p in fds.iter().filter(|p| is_ours(p.fd)) {
        k.keys(p.fd, &mut keys);
    }
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
        k = wait(k, &keys, step);
    }
}

// ---------------------------------------------------------------------------------------------
// epoll

#[repr(C)]
pub struct EpollEvent {
    events: u32,
    data: u64,
}

const EPOLL_CTL_ADD: c_int = 1;
const EPOLL_CTL_DEL: c_int = 2;
const EPOLL_CTL_MOD: c_int = 3;
const EPOLL_ALWAYS: u32 = (libc::POLLHUP | libc::POLLERR) as u32;

#[no_mangle]
pub extern "C" fn epoll_create1(flags: c_int) -> c_int {
    let mut k = lock();
    let o = k.new_open(Kind::Epoll);
    k.epolls.insert(o, Epoll::default());
    k.install(o, flags & libc::O_CLOEXEC != 0, 0)
}

#[no_mangle]
pub extern "C" fn epoll_create(size: c_int) -> c_int {
    if size <= 0 {
        return err(libc::EINVAL);
    }
    epoll_create1(0)
}

#[no_mangle]
pub unsafe extern "C" fn epoll_ctl(epfd: c_int, op: c_int, fd: c_int, ev: *const EpollEvent) -> c_int {
    let mut k = lock();
    let Some(ep) = k.open_of(epfd).filter(|o| k.epolls.contains_key(o)) else { return err(libc::EBADF) };
    let open = if is_ours(fd) {
        match k.open_of(fd) {
            Some(o) => Some(o),
            None => return err(libc::EBADF),
        }
    } else {
        None
    };
    let have = k.epolls[&ep].interest.contains_key(&fd);
    match op {
        EPOLL_CTL_ADD | EPOLL_CTL_MOD => {
            if have != (op == EPOLL_CTL_MOD) {
                return err(if have { libc::EEXIST } else { libc::ENOENT });
            }
            let (events, data) = ((*ev).events, (*ev).data);
            if have {
                let i = k.epolls.get_mut(&ep).unwrap().interest.get_mut(&fd).unwrap();
                i.events = events;
                i.data = data;
            } else {
                let mut keys = Vec::new();
                k.keys(fd, &mut keys);
                for &key in &keys {
                    k.watch.entry(key).or_default().push((ep, fd));
                }
                k.epolls.get_mut(&ep).unwrap().interest.insert(fd, Interest { events, data, open, keys });
            }
            k.epolls[&ep].cand.borrow_mut().push(fd);
            0
        }
        EPOLL_CTL_DEL => {
            let Some(i) = k.epolls.get_mut(&ep).unwrap().interest.remove(&fd) else { return err(libc::ENOENT) };
            k.unwatch(ep, fd, &i.keys);
            0
        }
        _ => err(libc::EINVAL),
    }
}

#[no_mangle]
pub unsafe extern "C" fn epoll_wait(epfd: c_int, out: *mut EpollEvent, max: c_int, timeout: c_int) -> c_int {
    if max <= 0 {
        return err(libc::EINVAL);
    }
    let out = std::slice::from_raw_parts_mut(out, max as usize);
    let deadline = (timeout >= 0).then(|| Instant::now() + Duration::from_millis(timeout as u64));
    let mut k = lock();
    loop {
        let Some(ep) = k.open_of(epfd).filter(|o| k.epolls.contains_key(o)) else { return err(libc::EBADF) };
        let mut cand = std::mem::take(&mut *k.epolls[&ep].cand.borrow_mut());
        cand.sort_unstable();
        cand.dedup();
        let mut n = 0;
        let mut again = Vec::new();
        let mut real = Vec::new();
        for fd in cand {
            let Some(i) = k.epolls[&ep].interest.get(&fd) else { continue };
            let (events, data) = (i.events, i.data);
            let Some(open) = i.open else {
                real.push(fd);
                continue;
            };
            if k.open_of(fd) != Some(open) {
                // closed (or the number reused): gone from the set, as on Linux
                let i = k.epolls.get_mut(&ep).unwrap().interest.remove(&fd).unwrap();
                k.unwatch(ep, fd, &i.keys);
                continue;
            }
            // a connect or shutdown changes what the fd waits on
            let mut keys = Vec::new();
            k.keys(fd, &mut keys);
            if keys != k.epolls[&ep].interest[&fd].keys {
                let old = std::mem::replace(&mut k.epolls.get_mut(&ep).unwrap().interest.get_mut(&fd).unwrap().keys, keys.clone());
                k.unwatch(ep, fd, &old);
                for &key in &keys {
                    k.watch.entry(key).or_default().push((ep, fd));
                }
            }
            let r = k.revents(fd, (events as i16) | libc::POLLHUP | libc::POLLERR) as u16 as u32 & (events | EPOLL_ALWAYS);
            if r == 0 {
                continue;
            }
            again.push(fd);
            if n < out.len() {
                out[n] = EpollEvent { events: r, data };
                n += 1;
            }
        }
        // real fds can't wake us: they stay candidates, polled without blocking
        let has_real = !real.is_empty();
        if has_real {
            let mut tmp: Vec<pollfd> = real
                .iter()
                .map(|&fd| pollfd { fd, events: k.epolls[&ep].interest[&fd].events as i16, revents: 0 })
                .collect();
            libc::poll(tmp.as_mut_ptr(), tmp.len() as _, 0);
            for p in tmp {
                let i = &k.epolls[&ep].interest[&p.fd];
                if p.revents != 0 && n < out.len() {
                    out[n] = EpollEvent { events: p.revents as u16 as u32, data: i.data };
                    n += 1;
                }
            }
        }
        {
            let e = &k.epolls[&ep];
            let mut c = e.cand.borrow_mut();
            c.extend(again);
            c.extend(real);
        }
        if n > 0 || timeout == 0 || deadline.is_some_and(|d| Instant::now() >= d) {
            return n as c_int;
        }
        let step = if has_real {
            let t = Instant::now() + Duration::from_millis(10);
            Some(deadline.map_or(t, |d| d.min(t)))
        } else {
            deadline
        };
        k = wait_ex(k, &[ep], step, false);
    }
}

// ---------------------------------------------------------------------------------------------
// ntsync: Wine's in-process synchronization (server/inproc_sync.c, ntdll unix/sync.c) talks to
// the Linux ntsync driver. With it, events, mutexes and semaphores are set and waited on by the
// threads themselves instead of with a wineserver request each (a thousand a second while a
// game idles). The objects are fds here like there, so they pass between processes as usual.

const NTSYNC_CREATE_SEM: c_int = 0x80;
const NTSYNC_SEM_RELEASE: c_int = 0x81;
const NTSYNC_WAIT_ANY: c_int = 0x82;
const NTSYNC_WAIT_ALL: c_int = 0x83;
const NTSYNC_CREATE_MUTEX: c_int = 0x84;
const NTSYNC_MUTEX_UNLOCK: c_int = 0x85;
const NTSYNC_MUTEX_KILL: c_int = 0x86;
const NTSYNC_CREATE_EVENT: c_int = 0x87;
const NTSYNC_EVENT_SET: c_int = 0x88;
const NTSYNC_EVENT_RESET: c_int = 0x89;
const NTSYNC_EVENT_PULSE: c_int = 0x8a;
const NTSYNC_SEM_READ: c_int = 0x8b;
const NTSYNC_MUTEX_READ: c_int = 0x8c;
const NTSYNC_EVENT_READ: c_int = 0x8d;
const NTSYNC_WAIT_REALTIME: u32 = 1;

#[repr(C)]
struct WaitArgs {
    timeout: u64,
    objs: u64,
    count: u32,
    index: u32,
    flags: u32,
    owner: u32,
    alert: u32,
    pad: u32,
}

pub fn ntsync_open(flags: c_int) -> c_int {
    let mut k = lock();
    let o = k.new_open(Kind::NtsyncDev);
    k.install(o, flags & libc::O_CLOEXEC != 0, 0)
}

impl Kern {
    fn sync(&mut self, open: Id) -> Option<&mut Sync> {
        match &mut self.opens.get_mut(&open)?.kind {
            Kind::Sync(s) => Some(s),
            _ => None,
        }
    }

    /// Whether `owner` could take `open` now.
    fn can_take(&self, open: Id, owner: u32) -> bool {
        match &self.opens[&open].kind {
            Kind::Sync(Sync::Sem { count, .. }) => *count > 0,
            Kind::Sync(Sync::Mutex { owner: o, count, .. }) => *o == 0 || (*o == owner && *count < u32::MAX),
            Kind::Sync(Sync::Event { signaled, pulses, .. }) => *signaled || *pulses > 0,
            _ => false,
        }
    }

    /// Takes `open` (which `can_take`); false when it was an abandoned mutex.
    fn take(&mut self, open: Id, owner: u32) -> bool {
        match self.sync(open).unwrap() {
            Sync::Sem { count, .. } => *count -= 1,
            Sync::Mutex { owner: o, count, dead } => {
                *o = owner;
                *count += 1;
                return !std::mem::take(dead);
            }
            Sync::Event { manual, signaled, pulses } => {
                if *pulses > 0 && !*signaled {
                    *pulses -= 1;
                } else if !*manual {
                    *signaled = false;
                }
            }
        }
        true
    }
}

/// ioctl `nr` (the low byte of an ntsync request) on fd.
#[no_mangle]
pub unsafe extern "C" fn vnhost_ntsync_ioctl(fd: c_int, nr: c_int, arg: *mut c_void) -> c_int {
    let mut k = lock();
    let Some(o) = k.open_of(fd) else { return err(libc::EBADF) };
    let a = arg as *mut u32;
    if matches!(k.opens[&o].kind, Kind::NtsyncDev) {
        let new = match nr {
            NTSYNC_CREATE_SEM => {
                let (count, max) = (*a, *a.add(1));
                if count > max {
                    return err(libc::EINVAL);
                }
                Sync::Sem { count, max }
            }
            NTSYNC_CREATE_MUTEX => {
                let (owner, count) = (*a, *a.add(1));
                if (owner == 0) != (count == 0) {
                    return err(libc::EINVAL);
                }
                Sync::Mutex { owner, count, dead: false }
            }
            NTSYNC_CREATE_EVENT => Sync::Event { manual: *a != 0, signaled: *a.add(1) != 0, pulses: 0 },
            NTSYNC_WAIT_ANY | NTSYNC_WAIT_ALL => {
                drop(k);
                return ntsync_wait(&mut *(arg as *mut WaitArgs), nr == NTSYNC_WAIT_ALL);
            }
            _ => return err(libc::ENOTTY),
        };
        let s = k.new_open(Kind::Sync(new));
        return k.install(s, true, 0);
    }
    let waiting = k.waiters.get(&o).map_or(0, |v| v.len() as u32);
    let r = match (k.sync(o), nr) {
        (None, _) => return err(libc::EINVAL),
        (Some(Sync::Sem { count, max }), NTSYNC_SEM_RELEASE) => {
            if (*count as u64 + *a as u64) > *max as u64 {
                return err(libc::EOVERFLOW);
            }
            let prev = *count;
            *count += *a;
            *a = prev;
            0
        }
        (Some(Sync::Sem { count, max }), NTSYNC_SEM_READ) => {
            (*a, *a.add(1)) = (*count, *max);
            return 0;
        }
        (Some(Sync::Mutex { owner, count, .. }), NTSYNC_MUTEX_UNLOCK) => {
            if *a == 0 {
                return err(libc::EINVAL);
            }
            if *owner != *a {
                return err(libc::EPERM);
            }
            *a.add(1) = *count;
            *count -= 1;
            if *count == 0 {
                *owner = 0;
            }
            0
        }
        (Some(Sync::Mutex { owner, count, dead }), NTSYNC_MUTEX_KILL) => {
            if *owner != *a {
                return err(libc::EPERM);
            }
            (*owner, *count, *dead) = (0, 0, true);
            0
        }
        (Some(Sync::Mutex { owner, count, dead }), NTSYNC_MUTEX_READ) => {
            (*a, *a.add(1)) = (*owner, *count);
            return if *dead { err(libc::EOWNERDEAD) } else { 0 };
        }
        (Some(Sync::Event { signaled, pulses, manual }), NTSYNC_EVENT_SET | NTSYNC_EVENT_RESET | NTSYNC_EVENT_PULSE) => {
            *a = *signaled as u32;
            match nr {
                NTSYNC_EVENT_SET => (*signaled, *pulses) = (true, 0),
                NTSYNC_EVENT_RESET => (*signaled, *pulses) = (false, 0),
                _ => {
                    // the threads blocked on it now get it once; nobody later
                    *signaled = false;
                    *pulses = if *manual { waiting } else { waiting.min(1) };
                }
            }
            if nr == NTSYNC_EVENT_RESET {
                return 0;
            }
            0
        }
        (Some(Sync::Event { manual, signaled, .. }), NTSYNC_EVENT_READ) => {
            (*a, *a.add(1)) = (*manual as u32, *signaled as u32);
            return 0;
        }
        _ => return err(libc::EINVAL),
    };
    k.wake(o);
    r
}

/// Monotonic or realtime clock in ns, as Wine computed the deadline with clock_gettime().
fn clock_ns(realtime: bool) -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(if realtime { libc::CLOCK_REALTIME } else { libc::CLOCK_MONOTONIC }, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

unsafe fn ntsync_wait(args: &mut WaitArgs, all: bool) -> c_int {
    let n = args.count as usize;
    if n > 64 {
        return err(libc::EINVAL);
    }
    let fds = std::slice::from_raw_parts(args.objs as usize as *const u32, n);
    let deadline = (args.timeout != u64::MAX).then(|| {
        let now = clock_ns(args.flags & NTSYNC_WAIT_REALTIME != 0);
        Instant::now() + Duration::from_nanos(args.timeout.saturating_sub(now))
    });
    let owner = args.owner;
    let mut k = lock();
    let mut objs = Vec::with_capacity(n + 1);
    for &fd in fds {
        match k.open_of(fd as c_int) {
            Some(o) if k.sync(o).is_some() => objs.push(o),
            _ => return err(libc::EINVAL),
        }
    }
    let alert = if args.alert != 0 {
        match k.open_of(args.alert as c_int) {
            Some(o) if matches!(k.sync(o), Some(Sync::Event { .. })) => Some(o),
            _ => return err(libc::EINVAL),
        }
    } else {
        None
    };
    if all && (1..objs.len()).any(|i| objs[..i].contains(&objs[i])) {
        return err(libc::EINVAL);
    }
    let mut keys = objs.clone();
    keys.extend(alert);
    loop {
        let mut got = None;
        if all {
            if !objs.is_empty() && objs.iter().all(|&o| k.can_take(o, owner)) {
                let mut ok = true;
                for &o in &objs {
                    ok &= k.take(o, owner);
                }
                got = Some((0, ok));
            }
        } else if let Some(i) = objs.iter().position(|&o| k.can_take(o, owner)) {
            got = Some((i, k.take(objs[i], owner)));
        }
        if got.is_none() {
            if let Some(a) = alert.filter(|&a| k.can_take(a, owner)) {
                k.take(a, owner);
                got = Some((n, true));
            }
        }
        if let Some((i, ok)) = got {
            args.index = i as u32;
            // what it took may have been what others wait for too (a manual event stays set)
            return if ok { 0 } else { err(libc::EOWNERDEAD) };
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return err(libc::ETIMEDOUT);
        }
        // a signal handler runs and interrupts the wait, as with the kernel's (checked under the
        // lock: what is set after it was dropped would go unnoticed)
        if crate::cpu::syscall_signals_pending() {
            drop(k);
            crate::cpu::handle_syscall_signals();
            return err(libc::EINTR);
        }
        k = wait(k, &keys, deadline);
    }
}
