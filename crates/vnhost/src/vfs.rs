//! The file system Wine sees in the browser: directory trees packed by `vnvm-pack tree`
//! (`trees/<name>.tsv` plus chunks in the store) mounted where `VNVM_MOUNTS` says
//! ("/opt/wine-vnbox=wineroot:/prefix=prefix32"), read lazily chunk by chunk. Whatever is
//! written stays in memory. Paths outside the mounts go to emscripten's file system.
//!
//! The current directory is per Wine process (`proc`), for every path: relative paths are made
//! absolute here before emscripten sees them.
//!
//! Open files and directories are emulated fds (`fd::Kind::Vfs`) referring to an `Handle`
//! here, so dup, SCM_RIGHTS and per-process cleanup work as for pipes and sockets.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::{Arc, Mutex};

use libc::{off_t, size_t, ssize_t};

use crate::set_errno;

type Ino = u32;
type Hash = [u8; 16];

/// st_dev of our files
const DEV: u64 = 0x766e;

enum Content {
    /// From the store; `None` chunks are all zero.
    Lower { size: u64, chunk: u32, chunks: Arc<Vec<Option<Hash>>> },
    Upper(Vec<u8>),
}

enum Data {
    Dir(BTreeMap<String, Ino>),
    File(Content),
    Link(String),
}

struct Node {
    data: Data,
    /// permission bits only
    mode: u32,
    mtime: i64,
    nlink: u32,
}

struct Handle {
    ino: Ino,
    pos: u64,
    flags: c_int,
    /// absolute path it was opened with (for openat and fchdir)
    path: String,
}

#[derive(Default)]
struct Fs {
    nodes: Vec<Option<Node>>,
    mounts: Vec<(String, Ino)>,
    handles: HashMap<u32, Handle>,
    next_handle: u32,
    /// free slots in `nodes`
    free: Vec<Ino>,
}

static FS: Mutex<Option<Fs>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut Fs) -> R) -> R {
    let mut g = FS.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Fs::load))
}

fn err<T: From<i8>>(e: c_int) -> T {
    set_errno(e);
    T::from(-1)
}

extern "C" {
    /// hangover/vnfs.js: reads `name` from the store into a malloc'ed buffer (NULL if missing)
    fn vnvm_fetch(name: *const c_char, len: *mut size_t) -> *mut u8;
}

fn fetch(name: &str) -> Option<Vec<u8>> {
    let c = CString::new(name).ok()?;
    let mut len = 0;
    unsafe {
        let p = vnvm_fetch(c.as_ptr(), &mut len);
        if trace() {
            eprintln!("vfs: fetch {name}: {}", if p.is_null() { -1 } else { len as isize });
        }
        if p.is_null() {
            return None;
        }
        let v = std::slice::from_raw_parts(p, len).to_vec();
        libc::free(p as *mut c_void);
        Some(v)
    }
}

fn trace() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VNVM_TRACE_VFS").is_some())
}

fn unesc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some(c) => out.push(c),
            None => {}
        }
    }
    out
}

fn hex16(s: &str) -> Option<Hash> {
    if s.len() != 32 {
        return None;
    }
    let mut h = [0; 16];
    for i in 0..16 {
        h[i] = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(h)
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

impl Fs {
    fn load() -> Fs {
        let mut fs = Fs { next_handle: 1, ..Default::default() };
        let Ok(spec) = std::env::var("VNVM_MOUNTS") else { return fs };
        for m in spec.split(':').filter(|m| !m.is_empty()) {
            let Some((at, name)) = m.split_once('=') else { continue };
            let root = fs.add(Data::Dir(BTreeMap::new()), 0o755, now());
            match fetch(&format!("trees/{name}.tsv")) {
                Some(tsv) => fs.load_tree(root, &String::from_utf8_lossy(&tsv)),
                None => eprintln!("vnhost: tree {name} not found in the store"),
            }
            let at = normalize(at);
            // the mount point must exist for whoever lists its parent
            let c = CString::new(at.clone()).unwrap();
            unsafe { crate::wrap::real_mkdir_p(&c) };
            fs.mounts.push((at, root));
        }
        fs
    }

    fn load_tree(&mut self, root: Ino, tsv: &str) {
        let mut lines = tsv.lines();
        let chunk: u32 = lines.next().and_then(|h| h.split('\t').nth(2)?.parse().ok()).unwrap_or(1 << 18);
        for line in lines {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() < 4 {
                continue;
            }
            let mode = u32::from_str_radix(f[1], 8).unwrap_or(0o644);
            let mtime = f[2].parse().unwrap_or(0);
            let path = unesc(f[3]);
            let (dir, name) = match path.rsplit_once('/') {
                Some((d, n)) => (d, n),
                None => ("", path.as_str()),
            };
            let Some(parent) = self.walk(root, dir) else { continue };
            let data = match f[0] {
                "d" => Data::Dir(BTreeMap::new()),
                "l" if f.len() > 4 => Data::Link(unesc(f[4])),
                "f" if f.len() > 5 => {
                    let size = f[4].parse().unwrap_or(0);
                    let chunks = if f[5].is_empty() { Vec::new() } else { f[5].split(',').map(hex16).collect() };
                    Data::File(Content::Lower { size, chunk, chunks: Arc::new(chunks) })
                }
                _ => continue,
            };
            let ino = self.add(data, mode, mtime);
            if let Some(Data::Dir(c)) = self.data_mut(parent) {
                c.insert(name.to_string(), ino);
            }
        }
    }

    /// Follows plain directory names (no links) from `from`.
    fn walk(&self, from: Ino, rel: &str) -> Option<Ino> {
        let mut at = from;
        for c in rel.split('/').filter(|c| !c.is_empty()) {
            match &self.node(at)?.data {
                Data::Dir(ch) => at = *ch.get(c)?,
                _ => return None,
            }
        }
        Some(at)
    }

    fn add(&mut self, data: Data, mode: u32, mtime: i64) -> Ino {
        let n = Node { nlink: if matches!(data, Data::Dir(_)) { 2 } else { 1 }, data, mode, mtime };
        if let Some(i) = self.free.pop() {
            self.nodes[i as usize] = Some(n);
            return i;
        }
        self.nodes.push(Some(n));
        (self.nodes.len() - 1) as Ino
    }

    fn node(&self, ino: Ino) -> Option<&Node> {
        self.nodes.get(ino as usize)?.as_ref()
    }
    fn node_mut(&mut self, ino: Ino) -> Option<&mut Node> {
        self.nodes.get_mut(ino as usize)?.as_mut()
    }
    fn data_mut(&mut self, ino: Ino) -> Option<&mut Data> {
        Some(&mut self.node_mut(ino)?.data)
    }

    fn mount_at(&self, path: &str) -> Option<Ino> {
        self.mounts.iter().find(|(p, _)| p == path).map(|m| m.1)
    }

    /// Resolves an absolute path.
    fn resolve(&self, path: &str, follow_last: bool) -> Result<Found, c_int> {
        let mut todo: VecDeque<String> = path.split('/').filter(|c| !c.is_empty() && *c != ".").map(String::from).collect();
        let mut cur: Vec<String> = Vec::new();
        let mut node = self.mount_at("/");
        let mut links = 0;
        while let Some(c) = todo.pop_front() {
            let last = todo.is_empty();
            if c == ".." {
                cur.pop();
                node = self.lookup_plain(&cur);
                continue;
            }
            let Some(dir) = node else {
                cur.push(c);
                node = self.mount_at(&join(&cur));
                continue;
            };
            let Some(Node { data: Data::Dir(ch), .. }) = self.node(dir) else { return Err(libc::ENOTDIR) };
            let Some(&child) = ch.get(&c) else {
                if last {
                    let path = join_with(&cur, &c);
                    return Ok(Found::Missing { parent: dir, name: c, path });
                }
                return Err(libc::ENOENT);
            };
            if let Some(Node { data: Data::Link(target), .. }) = self.node(child) {
                if !last || follow_last {
                    links += 1;
                    if links > 40 {
                        return Err(libc::ELOOP);
                    }
                    if target.starts_with('/') {
                        cur.clear();
                        node = self.mount_at("/");
                    }
                    for (i, t) in target.split('/').filter(|t| !t.is_empty() && *t != ".").enumerate() {
                        todo.insert(i, t.to_string());
                    }
                    continue;
                }
            }
            cur.push(c);
            node = Some(child);
        }
        let path = join(&cur);
        Ok(match node {
            Some(ino) => Found::Node { ino, path },
            None => Found::Outside(path),
        })
    }

    /// The node at a resolved path (no links in it), if it is ours.
    fn lookup_plain(&self, cur: &[String]) -> Option<Ino> {
        let mut node = self.mount_at("/");
        for i in 0..cur.len() {
            node = match node {
                None => self.mount_at(&join(&cur[..=i])),
                Some(d) => match &self.node(d)?.data {
                    Data::Dir(ch) => Some(*ch.get(&cur[i])?),
                    _ => return None,
                },
            };
        }
        node
    }

    fn stat(&self, ino: Ino, st: &mut libc::stat) {
        let n = self.node(ino).unwrap();
        let (kind, size) = match &n.data {
            Data::Dir(_) => (libc::S_IFDIR, 4096),
            Data::File(Content::Lower { size, .. }) => (libc::S_IFREG, *size),
            Data::File(Content::Upper(v)) => (libc::S_IFREG, v.len() as u64),
            Data::Link(t) => (libc::S_IFLNK, t.len() as u64),
        };
        *st = unsafe { std::mem::zeroed() };
        st.st_dev = DEV as _;
        st.st_ino = ino as u64 + 1;
        st.st_mode = kind | n.mode;
        st.st_nlink = n.nlink as _;
        st.st_size = size as _;
        st.st_blksize = 4096;
        st.st_blocks = size.div_ceil(512) as _;
        st.st_atime = n.mtime as _;
        st.st_mtime = n.mtime as _;
        st.st_ctime = n.mtime as _;
    }

    fn file_size(&self, ino: Ino) -> u64 {
        match self.node(ino).map(|n| &n.data) {
            Some(Data::File(Content::Lower { size, .. })) => *size,
            Some(Data::File(Content::Upper(v))) => v.len() as u64,
            _ => 0,
        }
    }

    fn new_handle(&mut self, ino: Ino, flags: c_int, path: String) -> u32 {
        let id = self.next_handle;
        self.next_handle += 1;
        self.handles.insert(id, Handle { ino, pos: 0, flags, path });
        id
    }

    fn unlink_name(&mut self, parent: Ino, name: &str) {
        let Some(Data::Dir(ch)) = self.data_mut(parent) else { return };
        let Some(ino) = ch.remove(name) else { return };
        if let Some(n) = self.node_mut(ino) {
            n.nlink = n.nlink.saturating_sub(1);
        }
        self.gc(ino);
    }

    /// Frees a node once no name or open handle refers to it.
    fn gc(&mut self, ino: Ino) {
        let linked = self.node(ino).is_some_and(|n| n.nlink > 0 && !(matches!(n.data, Data::Dir(_)) && n.nlink < 2));
        if !linked && !self.handles.values().any(|h| h.ino == ino) {
            if let Some(slot @ Some(_)) = self.nodes.get_mut(ino as usize) {
                *slot = None;
                self.free.push(ino);
            }
        }
    }
}

enum Found {
    Node { ino: Ino, path: String },
    Missing { parent: Ino, name: String, path: String },
    Outside(String),
}

fn join(cur: &[String]) -> String {
    if cur.is_empty() {
        return "/".into();
    }
    let mut s = String::new();
    for c in cur {
        s.push('/');
        s.push_str(c);
    }
    s
}

fn join_with(cur: &[String], last: &str) -> String {
    let mut s = join(cur);
    if !s.ends_with('/') {
        s.push('/');
    }
    s.push_str(last);
    s
}

/// Makes `path` absolute (against the process's current directory) and drops "." and "//".
pub fn normalize(path: &str) -> String {
    if trace() {
        eprintln!("vfs: {path}");
    }
    let full = if path.starts_with('/') { path.to_string() } else { format!("{}/{}", crate::proc::cwd(), path) };
    let mut out = String::with_capacity(full.len());
    for c in full.split('/').filter(|c| !c.is_empty() && *c != ".") {
        out.push('/');
        out.push_str(c);
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

// ---------------------------------------------------------------------------------------------
// chunk cache

struct Cache {
    map: HashMap<Hash, Arc<Vec<u8>>>,
    order: VecDeque<Hash>,
    bytes: usize,
}

static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

fn cache_limit() -> usize {
    static LIMIT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *LIMIT.get_or_init(|| std::env::var("VNVM_CHUNK_CACHE_MB").ok().and_then(|v| v.parse().ok()).unwrap_or(256usize) << 20)
}

fn chunk_data(h: &Hash, len: usize) -> Result<Arc<Vec<u8>>, c_int> {
    {
        let mut g = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = g.as_mut().and_then(|c| c.map.get(h)) {
            return Ok(c.clone());
        }
    }
    let hex: String = h.iter().map(|b| format!("{b:02x}")).collect();
    let mut data = fetch(&format!("chunks/{}/{hex}", &hex[..2])).ok_or(libc::EIO)?;
    data.resize(len, 0);
    let data = Arc::new(data);
    let mut g = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let c = g.get_or_insert_with(|| Cache { map: HashMap::new(), order: VecDeque::new(), bytes: 0 });
    if c.map.insert(*h, data.clone()).is_none() {
        c.order.push_back(*h);
        c.bytes += data.len();
        while c.bytes > cache_limit() {
            let Some(old) = c.order.pop_front() else { break };
            if let Some(d) = c.map.remove(&old) {
                c.bytes -= d.len();
            }
        }
    }
    Ok(data)
}

/// Reads file content at `off` (the lock is not held while chunks are fetched).
fn read_at(ino: Ino, buf: &mut [u8], off: u64) -> Result<usize, c_int> {
    let lower = with(|fs| match fs.node(ino).map(|n| &n.data) {
        Some(Data::File(Content::Upper(v))) => {
            let start = (off as usize).min(v.len());
            let n = buf.len().min(v.len() - start);
            buf[..n].copy_from_slice(&v[start..start + n]);
            Ok(Err(n))
        }
        Some(Data::File(Content::Lower { size, chunk, chunks })) => Ok(Ok((*size, *chunk as u64, chunks.clone()))),
        Some(Data::Dir(_)) => Err(libc::EISDIR),
        _ => Err(libc::EBADF),
    })?;
    let (size, chunk, chunks) = match lower {
        Err(n) => return Ok(n),
        Ok(l) => l,
    };
    if off >= size {
        return Ok(0);
    }
    let end = (off + buf.len() as u64).min(size);
    let mut at = off;
    while at < end {
        let i = (at / chunk) as usize;
        let cstart = i as u64 * chunk;
        let clen = chunk.min(size - cstart) as usize;
        let from = (at - cstart) as usize;
        let n = ((end - at) as usize).min(clen - from);
        let dst = &mut buf[(at - off) as usize..(at - off) as usize + n];
        match chunks.get(i).copied().flatten() {
            None => dst.fill(0),
            Some(h) => dst.copy_from_slice(&chunk_data(&h, clen)?[from..from + n]),
        }
        at += n as u64;
    }
    Ok((end - off) as usize)
}

/// Turns a file from the store into one in memory, before it is written.
fn copy_up(ino: Ino) -> Result<(), c_int> {
    let size = with(|fs| match fs.node(ino).map(|n| &n.data) {
        Some(Data::File(Content::Lower { size, .. })) => Some(*size),
        _ => None,
    });
    let Some(size) = size else { return Ok(()) };
    let mut v = vec![0; size as usize];
    read_at(ino, &mut v, 0)?;
    with(|fs| {
        if let Some(Data::File(c @ Content::Lower { .. })) = fs.data_mut(ino) {
            *c = Content::Upper(v);
        }
    });
    Ok(())
}

fn write_at(ino: Ino, data: &[u8], off: u64) -> Result<usize, c_int> {
    copy_up(ino)?;
    with(|fs| {
        let Some(Data::File(Content::Upper(v))) = fs.data_mut(ino) else { return Err(libc::EBADF) };
        let end = off as usize + data.len();
        if v.len() < end {
            v.resize(end, 0);
        }
        v[off as usize..end].copy_from_slice(data);
        if let Some(n) = fs.node_mut(ino) {
            n.mtime = now();
        }
        Ok(data.len())
    })
}

fn set_size(ino: Ino, size: u64) -> Result<(), c_int> {
    copy_up(ino)?;
    with(|fs| match fs.data_mut(ino) {
        Some(Data::File(Content::Upper(v))) => {
            v.resize(size as usize, 0);
            Ok(())
        }
        Some(Data::Dir(_)) => Err(libc::EISDIR),
        _ => Err(libc::EINVAL),
    })
}

// ---------------------------------------------------------------------------------------------
// the path-based calls; `None` means "not ours, ask emscripten" (with the absolute path)

pub enum Route<T> {
    Done(T),
    Real(CString),
}

impl<T> Route<T> {
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Route<U> {
        match self {
            Route::Done(v) => Route::Done(f(v)),
            Route::Real(c) => Route::Real(c),
        }
    }
}

/// Whether any tree is mounted (if not, only relative paths need our help).
pub fn active() -> bool {
    with(|fs| !fs.mounts.is_empty())
}

unsafe fn path_arg(dirfd: c_int, path: *const c_char) -> Result<String, c_int> {
    let p = CStr::from_ptr(path).to_string_lossy();
    if p.starts_with('/') || dirfd == libc::AT_FDCWD {
        return Ok(normalize(&p));
    }
    let base = handle_path(dirfd).ok_or(libc::EBADF)?;
    Ok(normalize(&format!("{base}/{p}")))
}

/// The path a directory fd was opened with (ours, or one emscripten knows by its fd).
fn handle_path(fd: c_int) -> Option<String> {
    let h = crate::fd::vfs_handle(fd)?;
    with(|fs| fs.handles.get(&h).map(|h| h.path.clone()))
}

fn real(path: String) -> CString {
    CString::new(path).unwrap_or_default()
}

pub unsafe fn open(dirfd: c_int, path: *const c_char, flags: c_int, mode: u32) -> Route<c_int> {
    if dirfd != libc::AT_FDCWD && !CStr::from_ptr(path).to_bytes().starts_with(b"/") && crate::fd::vfs_handle(dirfd).is_none() {
        return Route::Done(-2); // relative to a real directory fd: the caller passes it on as is
    }
    let p = match path_arg(dirfd, path) {
        Ok(p) => p,
        Err(e) => return Route::Done(err(e)),
    };
    let follow = flags & libc::O_NOFOLLOW == 0;
    let found = with(|fs| fs.resolve(&p, follow));
    let (ino, path) = match found {
        Err(e) => return Route::Done(err(e)),
        Ok(Found::Outside(p)) => return Route::Real(real(p)),
        Ok(Found::Node { ino, path }) => {
            if flags & libc::O_CREAT != 0 && flags & libc::O_EXCL != 0 {
                return Route::Done(err(libc::EEXIST));
            }
            (ino, path)
        }
        Ok(Found::Missing { parent, name, path }) => {
            if flags & libc::O_CREAT == 0 {
                return Route::Done(err(libc::ENOENT));
            }
            let ino = with(|fs| {
                let ino = fs.add(Data::File(Content::Upper(Vec::new())), mode & 0o7777, now());
                if let Some(Data::Dir(ch)) = fs.data_mut(parent) {
                    ch.insert(name, ino);
                }
                ino
            });
            (ino, path)
        }
    };
    let kind = with(|fs| fs.node(ino).map(|n| match n.data {
        Data::Dir(_) => 0,
        Data::File(_) => 1,
        Data::Link(_) => 2,
    }));
    match kind {
        Some(0) if flags & libc::O_ACCMODE != libc::O_RDONLY => return Route::Done(err(libc::EISDIR)),
        Some(1) | Some(2) if flags & libc::O_DIRECTORY != 0 => return Route::Done(err(libc::ENOTDIR)),
        Some(2) => return Route::Done(err(libc::ELOOP)),
        None => return Route::Done(err(libc::ENOENT)),
        _ => {}
    }
    if kind == Some(1) && flags & libc::O_TRUNC != 0 && flags & libc::O_ACCMODE != libc::O_RDONLY {
        if let Err(e) = set_size(ino, 0) {
            return Route::Done(err(e));
        }
    }
    let h = with(|fs| fs.new_handle(ino, flags, path));
    Route::Done(crate::fd::vfs_install(h, flags))
}

/// Called when the last fd of a handle is closed.
pub fn release(h: u32) {
    with(|fs| {
        if let Some(handle) = fs.handles.remove(&h) {
            fs.gc(handle.ino);
        }
    })
}

pub unsafe fn stat(dirfd: c_int, path: *const c_char, st: *mut libc::stat, follow: bool) -> Route<c_int> {
    if dirfd != libc::AT_FDCWD && !CStr::from_ptr(path).to_bytes().starts_with(b"/") && crate::fd::vfs_handle(dirfd).is_none() {
        return Route::Done(-2);
    }
    let p = match path_arg(dirfd, path) {
        Ok(p) => p,
        Err(e) => return Route::Done(err(e)),
    };
    with(|fs| match fs.resolve(&p, follow) {
        Err(e) => Route::Done(err(e)),
        Ok(Found::Outside(p)) => Route::Real(real(p)),
        Ok(Found::Missing { .. }) => Route::Done(err(libc::ENOENT)),
        Ok(Found::Node { ino, .. }) => {
            fs.stat(ino, &mut *st);
            Route::Done(0)
        }
    })
}

pub unsafe fn access(dirfd: c_int, path: *const c_char) -> Route<c_int> {
    let mut st: libc::stat = std::mem::zeroed();
    stat(dirfd, path, &mut st, true)
}

pub unsafe fn readlink(dirfd: c_int, path: *const c_char, buf: *mut c_char, size: size_t) -> Route<ssize_t> {
    let p = match path_arg(dirfd, path) {
        Ok(p) => p,
        Err(e) => return Route::Done(err(e)),
    };
    with(|fs| match fs.resolve(&p, false) {
        Err(e) => Route::Done(err(e)),
        Ok(Found::Outside(p)) => Route::Real(real(p)),
        Ok(Found::Missing { .. }) => Route::Done(err(libc::ENOENT)),
        Ok(Found::Node { ino, .. }) => match &fs.node(ino).unwrap().data {
            Data::Link(t) => {
                let n = t.len().min(size);
                std::ptr::copy_nonoverlapping(t.as_ptr(), buf as *mut u8, n);
                Route::Done(n as ssize_t)
            }
            _ => Route::Done(err(libc::EINVAL)),
        },
    })
}

/// Creates a directory or symlink entry.
unsafe fn create(path: *const c_char, data: Data, mode: u32) -> Route<c_int> {
    let p = normalize(&CStr::from_ptr(path).to_string_lossy());
    with(|fs| match fs.resolve(&p, false) {
        Err(e) => Route::Done(err(e)),
        Ok(Found::Outside(p)) => Route::Real(real(p)),
        Ok(Found::Node { .. }) => Route::Done(err(libc::EEXIST)),
        Ok(Found::Missing { parent, name, .. }) => {
            let is_dir = matches!(data, Data::Dir(_));
            let ino = fs.add(data, mode & 0o7777, now());
            if let Some(Data::Dir(ch)) = fs.data_mut(parent) {
                ch.insert(name, ino);
            }
            if is_dir {
                if let Some(n) = fs.node_mut(parent) {
                    n.nlink += 1;
                }
            }
            Route::Done(0)
        }
    })
}

pub unsafe fn mkdir(path: *const c_char, mode: u32) -> Route<c_int> {
    create(path, Data::Dir(BTreeMap::new()), mode)
}

pub unsafe fn symlink(target: *const c_char, path: *const c_char) -> Route<c_int> {
    create(path, Data::Link(CStr::from_ptr(target).to_string_lossy().into_owned()), 0o777)
}

/// unlink (dir: false) or rmdir (dir: true)
pub unsafe fn remove(path: *const c_char, dir: bool) -> Route<c_int> {
    let p = normalize(&CStr::from_ptr(path).to_string_lossy());
    let Some((parent_path, name)) = p.rsplit_once('/') else { return Route::Done(err(libc::EINVAL)) };
    let parent_path = if parent_path.is_empty() { "/" } else { parent_path };
    with(|fs| {
        let parent = match fs.resolve(parent_path, true) {
            Err(e) => return Route::Done(err(e)),
            Ok(Found::Outside(_)) => return Route::Real(real(p.clone())),
            Ok(Found::Missing { .. }) => return Route::Done(err(libc::ENOENT)),
            Ok(Found::Node { ino, .. }) => ino,
        };
        let Some(Data::Dir(ch)) = fs.node(parent).map(|n| &n.data) else { return Route::Done(err(libc::ENOTDIR)) };
        let Some(&ino) = ch.get(name) else { return Route::Done(err(libc::ENOENT)) };
        match (&fs.node(ino).unwrap().data, dir) {
            (Data::Dir(c), true) => {
                if !c.is_empty() {
                    return Route::Done(err(libc::ENOTEMPTY));
                }
                if let Some(n) = fs.node_mut(ino) {
                    n.nlink = 0;
                }
                if let Some(n) = fs.node_mut(parent) {
                    n.nlink -= 1;
                }
            }
            (Data::Dir(_), false) => return Route::Done(err(libc::EISDIR)),
            (_, true) => return Route::Done(err(libc::ENOTDIR)),
            _ => {}
        }
        fs.unlink_name(parent, name);
        Route::Done(0)
    })
}

pub unsafe fn rename(from: *const c_char, to: *const c_char) -> Route<c_int> {
    let a = normalize(&CStr::from_ptr(from).to_string_lossy());
    let b = normalize(&CStr::from_ptr(to).to_string_lossy());
    with(|fs| {
        let src = fs.resolve(&a, false);
        let dst = fs.resolve(&b, false);
        let (src_ino, sp, sn) = match src {
            Err(e) => return Route::Done(err(e)),
            Ok(Found::Outside(_)) => {
                return match dst {
                    Ok(Found::Outside(_)) => Route::Real(real(a)),
                    _ => Route::Done(err(libc::EXDEV)),
                }
            }
            Ok(Found::Missing { .. }) => return Route::Done(err(libc::ENOENT)),
            Ok(Found::Node { ino, path }) => {
                let (d, n) = path.rsplit_once('/').unwrap();
                let parent = match fs.resolve(if d.is_empty() { "/" } else { d }, true) {
                    Ok(Found::Node { ino, .. }) => ino,
                    _ => return Route::Done(err(libc::EXDEV)), // a mount point itself
                };
                (ino, parent, n.to_string())
            }
        };
        let (dp, dn) = match dst {
            Err(e) => return Route::Done(err(e)),
            Ok(Found::Outside(_)) => return Route::Done(err(libc::EXDEV)),
            Ok(Found::Missing { parent, name, .. }) => (parent, name),
            Ok(Found::Node { ino, path }) => {
                if ino == src_ino {
                    return Route::Done(0);
                }
                if let Some(Data::Dir(c)) = fs.node(ino).map(|n| &n.data) {
                    if !c.is_empty() {
                        return Route::Done(err(libc::ENOTEMPTY));
                    }
                }
                let (d, n) = path.rsplit_once('/').unwrap();
                let Ok(Found::Node { ino: parent, .. }) = fs.resolve(if d.is_empty() { "/" } else { d }, true) else {
                    return Route::Done(err(libc::EXDEV));
                };
                fs.unlink_name(parent, n);
                (parent, n.to_string())
            }
        };
        if let Some(Data::Dir(ch)) = fs.data_mut(sp) {
            ch.remove(&sn);
        }
        if let Some(Data::Dir(ch)) = fs.data_mut(dp) {
            ch.insert(dn, src_ino);
        }
        Route::Done(0)
    })
}

pub unsafe fn link(from: *const c_char, to: *const c_char) -> Route<c_int> {
    let a = normalize(&CStr::from_ptr(from).to_string_lossy());
    let b = normalize(&CStr::from_ptr(to).to_string_lossy());
    with(|fs| match (fs.resolve(&a, false), fs.resolve(&b, false)) {
        (Ok(Found::Outside(_)), Ok(Found::Outside(_))) => Route::Real(real(a)),
        (Ok(Found::Node { ino, .. }), Ok(Found::Missing { parent, name, .. })) => {
            if matches!(fs.node(ino).unwrap().data, Data::Dir(_)) {
                return Route::Done(err(libc::EPERM));
            }
            fs.node_mut(ino).unwrap().nlink += 1;
            if let Some(Data::Dir(ch)) = fs.data_mut(parent) {
                ch.insert(name, ino);
            }
            Route::Done(0)
        }
        (Ok(_), Ok(Found::Node { .. })) => Route::Done(err(libc::EEXIST)),
        (Err(e), _) | (_, Err(e)) => Route::Done(err(e)),
        _ => Route::Done(err(libc::EXDEV)),
    })
}

/// Sets permission bits and/or the modification time of a path or handle.
fn set_attr(ino: Ino, mode: Option<u32>, mtime: Option<i64>) {
    with(|fs| {
        if let Some(n) = fs.node_mut(ino) {
            if let Some(m) = mode {
                n.mode = m & 0o7777;
            }
            if let Some(t) = mtime {
                n.mtime = t;
            }
        }
    })
}

pub unsafe fn chmod(path: *const c_char, mode: u32) -> Route<c_int> {
    let p = normalize(&CStr::from_ptr(path).to_string_lossy());
    match with(|fs| fs.resolve(&p, true)) {
        Err(e) => Route::Done(err(e)),
        Ok(Found::Outside(p)) => Route::Real(real(p)),
        Ok(Found::Missing { .. }) => Route::Done(err(libc::ENOENT)),
        Ok(Found::Node { ino, .. }) => {
            set_attr(ino, Some(mode), None);
            Route::Done(0)
        }
    }
}

unsafe fn times_mtime(times: *const libc::timespec) -> Option<i64> {
    if times.is_null() {
        return Some(now());
    }
    let m = &*times.add(1);
    match m.tv_nsec {
        libc::UTIME_OMIT => None,
        libc::UTIME_NOW => Some(now()),
        _ => Some(m.tv_sec as i64),
    }
}

pub unsafe fn utimensat(dirfd: c_int, path: *const c_char, times: *const libc::timespec, flags: c_int) -> Route<c_int> {
    let p = match path_arg(dirfd, path) {
        Ok(p) => p,
        Err(e) => return Route::Done(err(e)),
    };
    match with(|fs| fs.resolve(&p, flags & libc::AT_SYMLINK_NOFOLLOW == 0)) {
        Err(e) => Route::Done(err(e)),
        Ok(Found::Outside(p)) => Route::Real(real(p)),
        Ok(Found::Missing { .. }) => Route::Done(err(libc::ENOENT)),
        Ok(Found::Node { ino, .. }) => {
            set_attr(ino, None, times_mtime(times));
            Route::Done(0)
        }
    }
}

pub unsafe fn chdir(path: *const c_char) -> c_int {
    let p = normalize(&CStr::from_ptr(path).to_string_lossy());
    let r = match with(|fs| fs.resolve(&p, true).map(|f| match f {
        Found::Node { ino, path } => Found::Node { ino: matches!(fs.node(ino).unwrap().data, Data::Dir(_)) as Ino, path },
        f => f,
    })) {
        Err(e) => Err(e),
        Ok(Found::Missing { .. }) => Err(libc::ENOENT),
        Ok(Found::Node { ino: 1, path }) => Ok(path),
        Ok(Found::Node { .. }) => Err(libc::ENOTDIR),
        // (not under the lock: libc's stat() comes back through our fstatat)
        Ok(Found::Outside(p)) => {
            let mut st: libc::stat = std::mem::zeroed();
            let c = real(p.clone());
            if crate::wrap::real_stat(&c, &mut st) != 0 {
                Err(crate::errno())
            } else if st.st_mode & libc::S_IFMT != libc::S_IFDIR {
                Err(libc::ENOTDIR)
            } else {
                Ok(p)
            }
        }
    };
    match r {
        Ok(p) => {
            crate::proc::set_cwd(p);
            0
        }
        Err(e) => err(e),
    }
}

pub unsafe fn realpath(path: *const c_char) -> Route<Result<String, c_int>> {
    let p = normalize(&CStr::from_ptr(path).to_string_lossy());
    match with(|fs| fs.resolve(&p, true)) {
        Err(e) => Route::Done(Err(e)),
        Ok(Found::Outside(p)) => Route::Real(real(p)),
        Ok(Found::Missing { .. }) => Route::Done(Err(libc::ENOENT)),
        Ok(Found::Node { path, .. }) => Route::Done(Ok(path)),
    }
}

// ---------------------------------------------------------------------------------------------
// fd-based calls (fd::Kind::Vfs handles)

pub fn flags(h: u32) -> c_int {
    with(|fs| fs.handles.get(&h).map_or(0, |x| x.flags & (libc::O_ACCMODE | libc::O_APPEND | libc::O_NONBLOCK)))
}

fn handle(h: u32) -> Option<(Ino, u64, c_int)> {
    with(|fs| fs.handles.get(&h).map(|x| (x.ino, x.pos, x.flags)))
}

fn set_pos(h: u32, pos: u64) {
    with(|fs| {
        if let Some(x) = fs.handles.get_mut(&h) {
            x.pos = pos;
        }
    })
}

pub unsafe fn pread(h: u32, buf: *mut c_void, len: size_t, off: Option<u64>) -> ssize_t {
    let Some((ino, pos, flags)) = handle(h) else { return err(libc::EBADF) };
    if flags & libc::O_ACCMODE == libc::O_WRONLY {
        return err(libc::EBADF);
    }
    let at = off.unwrap_or(pos);
    if trace() {
        eprintln!("vfs: pread {h} ino {ino} {len}@{at}");
    }
    match read_at(ino, std::slice::from_raw_parts_mut(buf as *mut u8, len), at) {
        Ok(n) => {
            if off.is_none() {
                set_pos(h, at + n as u64);
            }
            n as ssize_t
        }
        Err(e) => err(e),
    }
}

pub unsafe fn pwrite(h: u32, buf: *const c_void, len: size_t, off: Option<u64>) -> ssize_t {
    let Some((ino, pos, flags)) = handle(h) else { return err(libc::EBADF) };
    if flags & libc::O_ACCMODE == libc::O_RDONLY {
        return err(libc::EBADF);
    }
    let at = match off {
        Some(o) => o,
        None if flags & libc::O_APPEND != 0 => with(|fs| fs.file_size(ino)),
        None => pos,
    };
    match write_at(ino, std::slice::from_raw_parts(buf as *const u8, len), at) {
        Ok(n) => {
            if off.is_none() {
                set_pos(h, at + n as u64);
            }
            n as ssize_t
        }
        Err(e) => err(e),
    }
}

pub fn lseek(h: u32, off: off_t, whence: c_int) -> off_t {
    with(|fs| {
        let Some(x) = fs.handles.get(&h) else { return err(libc::EBADF) };
        let base = match whence {
            libc::SEEK_SET => 0,
            libc::SEEK_CUR => x.pos as i64,
            libc::SEEK_END => fs.file_size(x.ino) as i64,
            _ => return err(libc::EINVAL),
        };
        let new = base + off as i64;
        if new < 0 {
            return err(libc::EINVAL);
        }
        fs.handles.get_mut(&h).unwrap().pos = new as u64;
        new as off_t
    })
}

pub fn fstat(h: u32, st: &mut libc::stat) -> c_int {
    with(|fs| match fs.handles.get(&h) {
        Some(x) => {
            let ino = x.ino;
            fs.stat(ino, st);
            0
        }
        None => err(libc::EBADF),
    })
}

pub fn ftruncate(h: u32, size: off_t) -> c_int {
    let Some((ino, _, _)) = handle(h) else { return err(libc::EBADF) };
    match set_size(ino, size as u64) {
        Ok(()) => 0,
        Err(e) => err(e),
    }
}

pub fn fallocate(h: u32, off: off_t, len: off_t) -> c_int {
    let Some((ino, _, _)) = handle(h) else { return libc::EBADF };
    let size = with(|fs| fs.file_size(ino));
    let want = (off + len) as u64;
    if want > size {
        if let Err(e) = set_size(ino, want) {
            return e;
        }
    }
    0
}

pub fn fchmod(h: u32, mode: u32) -> c_int {
    let Some((ino, _, _)) = handle(h) else { return err(libc::EBADF) };
    set_attr(ino, Some(mode), None);
    0
}

pub unsafe fn futimens(h: u32, times: *const libc::timespec) -> c_int {
    let Some((ino, _, _)) = handle(h) else { return err(libc::EBADF) };
    set_attr(ino, None, times_mtime(times));
    0
}

pub fn fchdir(h: u32) -> c_int {
    let path = with(|fs| {
        let x = fs.handles.get(&h)?;
        matches!(fs.node(x.ino)?.data, Data::Dir(_)).then(|| x.path.clone())
    });
    match path {
        Some(p) => {
            crate::proc::set_cwd(p);
            0
        }
        None => err(libc::ENOTDIR),
    }
}

pub fn fstatfs(st: &mut libc::statfs) -> c_int {
    *st = unsafe { std::mem::zeroed() };
    st.f_type = 0x0102_1994; // tmpfs
    st.f_bsize = 4096;
    st.f_frsize = 4096;
    st.f_blocks = 1 << 20;
    st.f_bfree = 1 << 19;
    st.f_bavail = 1 << 19;
    st.f_files = 1 << 20;
    st.f_ffree = 1 << 19;
    st.f_namelen = 255;
    0
}

pub fn fstatvfs(st: &mut libc::statvfs) -> c_int {
    *st = unsafe { std::mem::zeroed() };
    st.f_bsize = 4096;
    st.f_frsize = 4096;
    st.f_blocks = 1 << 20;
    st.f_bfree = 1 << 19;
    st.f_bavail = 1 << 19;
    st.f_files = 1 << 20;
    st.f_ffree = 1 << 19;
    st.f_favail = 1 << 19;
    st.f_namemax = 255;
    0
}

// ---------------------------------------------------------------------------------------------
// directory streams

pub struct Dir {
    fd: c_int,
    names: Vec<(String, Ino, u8)>,
    next: usize,
    ent: libc::dirent,
}

/// DIR pointers we handed out (anything else is emscripten's).
static DIRS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

pub fn is_dir_stream(d: *mut c_void) -> bool {
    DIRS.lock().unwrap_or_else(|e| e.into_inner()).contains(&(d as usize))
}

/// fdopendir for one of our fds (the stream owns the fd).
pub fn fdopendir(fd: c_int) -> *mut c_void {
    let Some(h) = crate::fd::vfs_handle(fd) else { return err::<i8>(libc::EBADF) as usize as *mut c_void };
    let names = with(|fs| {
        let x = fs.handles.get(&h)?;
        let Data::Dir(ch) = &fs.node(x.ino)?.data else { return None };
        let mut v = vec![(".".to_string(), x.ino, libc::DT_DIR), ("..".to_string(), x.ino, libc::DT_DIR)];
        for (name, &ino) in ch {
            let t = match fs.node(ino).map(|n| &n.data) {
                Some(Data::Dir(_)) => libc::DT_DIR,
                Some(Data::Link(_)) => libc::DT_LNK,
                _ => libc::DT_REG,
            };
            v.push((name.clone(), ino, t));
        }
        Some(v)
    });
    let Some(names) = names else {
        set_errno(libc::ENOTDIR);
        return std::ptr::null_mut();
    };
    let d = Box::into_raw(Box::new(Dir { fd, names, next: 0, ent: unsafe { std::mem::zeroed() } }));
    DIRS.lock().unwrap_or_else(|e| e.into_inner()).push(d as usize);
    d as *mut c_void
}

pub unsafe fn readdir(d: *mut c_void) -> *mut libc::dirent {
    let d = &mut *(d as *mut Dir);
    let Some((name, ino, t)) = d.names.get(d.next) else { return std::ptr::null_mut() };
    d.next += 1;
    d.ent.d_ino = *ino as u64 + 1;
    d.ent.d_off = d.next as _;
    d.ent.d_type = *t;
    d.ent.d_reclen = std::mem::size_of::<libc::dirent>() as _;
    let n = name.len().min(255);
    std::ptr::copy_nonoverlapping(name.as_ptr() as *const c_char, d.ent.d_name.as_mut_ptr(), n);
    d.ent.d_name[n] = 0;
    &mut d.ent
}

pub unsafe fn rewinddir(d: *mut c_void) {
    (*(d as *mut Dir)).next = 0;
}

pub unsafe fn dirfd(d: *mut c_void) -> c_int {
    (*(d as *mut Dir)).fd
}

pub unsafe fn closedir(d: *mut c_void) -> c_int {
    DIRS.lock().unwrap_or_else(|e| e.into_inner()).retain(|&p| p != d as usize);
    let d = Box::from_raw(d as *mut Dir);
    crate::fd::close(d.fd)
}
