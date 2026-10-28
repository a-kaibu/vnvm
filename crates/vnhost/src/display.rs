//! The screen behind Wine's browser display driver (dlls/winebrowser.drv in the Wine patch).
//!
//! Every Wine process shares this state: top-level windows hand their pixels here when Wine
//! flushes their surface, and the screen is composed from them in z-order (bottom to top).
//! The page shows `vnhost_display_frame` (one BGRA buffer of the whole screen) and sends input
//! with `vnhost_input_push`; the driver's desktop thread reads it from `vnhost_input_fd`.
//!
//! Under node, `VNVM_SCREENSHOT=<file.ppm>` writes the screen out (see `start_screenshots`).

use std::collections::HashMap;
use std::ffi::c_int;
use std::sync::Mutex;

#[derive(Clone, Copy, Default, PartialEq, Debug)]
struct Rect {
    l: i32,
    t: i32,
    r: i32,
    b: i32,
}

impl Rect {
    fn w(&self) -> i32 {
        self.r - self.l
    }
    fn h(&self) -> i32 {
        self.b - self.t
    }
    fn empty(&self) -> bool {
        self.l >= self.r || self.t >= self.b
    }
    fn and(&self, o: &Rect) -> Rect {
        Rect { l: self.l.max(o.l), t: self.t.max(o.t), r: self.r.min(o.r), b: self.b.min(o.b) }
    }
    fn or(&self, o: &Rect) -> Rect {
        if self.empty() {
            return *o;
        }
        if o.empty() {
            return *self;
        }
        Rect { l: self.l.min(o.l), t: self.t.min(o.t), r: self.r.max(o.r), b: self.b.max(o.b) }
    }
}

struct Win {
    /// screen position of the window's surface
    rect: Rect,
    visible: bool,
    pixels: Vec<u32>,
}

struct Display {
    w: i32,
    h: i32,
    wins: HashMap<u32, Win>,
    /// bottom to top
    order: Vec<u32>,
    screen: Vec<u32>,
    serial: u32,
}

static DISPLAY: Mutex<Option<Display>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut Display) -> R) -> R {
    let mut g = DISPLAY.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(|| {
        let (w, h) = std::env::var("VNVM_SCREEN")
            .ok()
            .and_then(|s| {
                let (w, h) = s.split_once('x')?;
                Some((w.parse().ok()?, h.parse().ok()?))
            })
            .unwrap_or((1280, 720));
        Display {
            w,
            h,
            wins: HashMap::new(),
            order: Vec::new(),
            screen: vec![0xff00_0000; (w * h) as usize],
            serial: 0,
        }
    }))
}

/// `VNVM_SCREENSHOT`: a thread writes the screen out whenever it has changed, once a second.
fn start_screenshots() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    let Ok(path) = std::env::var("VNVM_SCREENSHOT") else { return };
    STARTED.call_once(|| {
        std::thread::spawn(move || {
            let mut written = 0;
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
                let shot = with(|d| {
                    (d.serial != written).then(|| {
                        written = d.serial;
                        let mut out = format!("P6\n{} {}\n255\n", d.w, d.h).into_bytes();
                        out.reserve(d.screen.len() * 3);
                        for p in &d.screen {
                            out.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, *p as u8]);
                        }
                        out
                    })
                });
                if let Some(out) = shot {
                    let tmp = format!("{path}.tmp");
                    if std::fs::write(&tmp, &out).is_ok() {
                        let _ = std::fs::rename(&tmp, &path);
                    }
                }
            }
        });
    });
}

impl Display {
    fn screen_rect(&self) -> Rect {
        Rect { l: 0, t: 0, r: self.w, b: self.h }
    }

    /// Redraws part of the screen from the windows.
    fn compose(&mut self, area: Rect) {
        let area = area.and(&self.screen_rect());
        if area.empty() {
            return;
        }
        let sw = self.w as usize;
        for y in area.t..area.b {
            let row = y as usize * sw;
            self.screen[row + area.l as usize..row + area.r as usize].fill(0xff00_0000);
        }
        for id in &self.order {
            let win = &self.wins[id];
            let r = win.rect.and(&area);
            if !win.visible || r.empty() || win.pixels.is_empty() {
                continue;
            }
            let ww = win.rect.w() as usize;
            for y in r.t..r.b {
                let src = (y - win.rect.t) as usize * ww + (r.l - win.rect.l) as usize;
                let dst = y as usize * sw + r.l as usize;
                let n = r.w() as usize;
                self.screen[dst..dst + n].copy_from_slice(&win.pixels[src..src + n]);
            }
        }
        self.serial = self.serial.wrapping_add(1);
    }
}

#[no_mangle]
pub unsafe extern "C" fn vnhost_display_size(w: *mut c_int, h: *mut c_int) {
    with(|d| {
        *w = d.w;
        *h = d.h;
    })
}

/// A top-level window moved, resized, was shown/hidden or changed its z-order. `rect` is where
/// its surface is on the screen; with `restack`, it goes just below `above` (0: on top).
#[no_mangle]
pub unsafe extern "C" fn vnhost_window_pos(hwnd: u32, rect: *const i32, visible: c_int, restack: c_int, above: u32) {
    start_screenshots();
    let r = std::slice::from_raw_parts(rect, 4);
    let rect = Rect { l: r[0], t: r[1], r: r[2], b: r[3] };
    with(|d| {
        let old = d.wins.get(&hwnd).map(|w| if w.visible { w.rect } else { Rect::default() }).unwrap_or_default();
        let win = d.wins.entry(hwnd).or_insert_with(|| Win { rect: Rect::default(), visible: false, pixels: Vec::new() });
        if win.rect.w() != rect.w() || win.rect.h() != rect.h() {
            win.pixels = vec![0xff00_0000; (rect.w().max(0) * rect.h().max(0)) as usize];
        }
        win.rect = rect;
        win.visible = visible != 0;
        if restack != 0 || !d.order.contains(&hwnd) {
            d.order.retain(|&h| h != hwnd);
            let at = d.order.iter().position(|&h| h == above).unwrap_or(d.order.len());
            d.order.insert(at, hwnd);
        }
        let new = if visible != 0 { rect } else { Rect::default() };
        d.compose(old.or(&new));
    })
}

/// New pixels for part of a window: `dirty` in window coordinates, `bits` (BGRA rows of
/// `stride` pixels) start at window coordinates (`ox`, `oy`).
#[no_mangle]
pub unsafe extern "C" fn vnhost_window_draw(hwnd: u32, dirty: *const i32, bits: *const u32, stride: c_int, ox: c_int, oy: c_int) {
    let r = std::slice::from_raw_parts(dirty, 4);
    let dirty = Rect { l: r[0], t: r[1], r: r[2], b: r[3] };
    with(|d| {
        let Some(win) = d.wins.get_mut(&hwnd) else { return };
        let dirty = dirty.and(&Rect { l: 0, t: 0, r: win.rect.w(), b: win.rect.h() });
        if dirty.empty() || win.pixels.is_empty() {
            return;
        }
        let ww = win.rect.w() as usize;
        for y in dirty.t..dirty.b {
            let src = bits.offset(((y - oy) * stride + (dirty.l - ox)) as isize);
            let dst = y as usize * ww + dirty.l as usize;
            let n = dirty.w() as usize;
            let row = &mut win.pixels[dst..dst + n];
            row.copy_from_slice(std::slice::from_raw_parts(src, n));
            for p in row {
                *p |= 0xff00_0000;
            }
        }
        let at = Rect { l: dirty.l + win.rect.l, t: dirty.t + win.rect.t, r: dirty.r + win.rect.l, b: dirty.b + win.rect.t };
        if win.visible {
            d.compose(at);
        }
    })
}

#[no_mangle]
pub extern "C" fn vnhost_window_destroy(hwnd: u32) {
    with(|d| {
        if let Some(win) = d.wins.remove(&hwnd) {
            d.order.retain(|&h| h != hwnd);
            if win.visible {
                d.compose(win.rect);
            }
        }
    })
}

/// The screen for the page: returns the BGRA buffer (width*height pixels) and stores the
/// frame's serial number, which changes whenever the screen does.
#[no_mangle]
pub unsafe extern "C" fn vnhost_display_frame(serial: *mut u32) -> *const u32 {
    with(|d| {
        *serial = d.serial;
        d.screen.as_ptr()
    })
}

// ---------------------------------------------------------------------------------------------
// input: 16-byte records { kind, x, y, data } through a pipe the driver's desktop thread polls

/// Input record kinds, shared with the driver.
pub const INPUT_MOUSE_MOVE: u32 = 1;
pub const INPUT_MOUSE_DOWN: u32 = 2; // data: button 0 left, 1 right, 2 middle
pub const INPUT_MOUSE_UP: u32 = 3;
pub const INPUT_WHEEL: u32 = 4; // data: delta (120 per notch)
pub const INPUT_KEY_DOWN: u32 = 5; // data: vk | scan << 16
pub const INPUT_KEY_UP: u32 = 6;

static INPUT: Mutex<Option<(c_int, c_int)>> = Mutex::new(None);

fn input_pipe() -> Option<(c_int, c_int)> {
    let mut g = INPUT.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_none() {
        let mut fds = [0; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return None;
        }
        unsafe { libc::fcntl(fds[0], libc::F_SETFL, libc::O_NONBLOCK) };
        unsafe { libc::fcntl(fds[1], libc::F_SETFL, libc::O_NONBLOCK) };
        // not the calling process's: they outlive it
        crate::fd::give(fds[0], crate::proc::HOST_PID);
        crate::fd::give(fds[1], crate::proc::HOST_PID);
        *g = Some((fds[0], fds[1]));
    }
    *g
}

/// The read end of the input pipe (for the desktop process's driver).
#[no_mangle]
pub extern "C" fn vnhost_input_fd() -> c_int {
    input_pipe().map_or(-1, |p| p.0)
}

#[no_mangle]
pub extern "C" fn vnhost_input_push(kind: u32, x: i32, y: i32, data: u32) -> c_int {
    let Some((_, w)) = input_pipe() else { return -1 };
    let rec = [kind, x as u32, y as u32, data];
    let n = unsafe { libc::write(w, rec.as_ptr() as *const libc::c_void, 16) };
    (n == 16) as c_int - 1
}
