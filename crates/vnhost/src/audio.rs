//! The sound card behind the browser audio driver (dlls/winebrowser.drv/audio.c in the Wine patch).
//!
//! Each render stream of a Wine process queues its frames here. The wall clock plays them: on
//! every call the elapsed time is mixed (resampled to 48 kHz stereo f32) into `out`, which the
//! page drains with `vnhost_audio_take` into WebAudio. Under node nobody drains it and `out` just
//! stays capped, so the timing games wait on is the same in both.

use std::collections::{HashMap, VecDeque};
use std::ffi::c_int;
use std::sync::Mutex;
use std::time::Instant;

pub const RATE: u32 = 48000;
/// frames kept for the page at most (older ones are dropped)
const OUT_CAP: usize = RATE as usize / 4;

struct Stream {
    rate: u32,
    channels: u32,
    bits: u32,
    float: bool,
    /// stereo frames at `rate`
    q: VecDeque<[f32; 2]>,
    /// position between q[0] and q[1], in source frames
    phase: f64,
}

struct Card {
    streams: HashMap<c_int, Stream>,
    next_id: c_int,
    /// time up to which `out` has been mixed
    clock: Instant,
    out: VecDeque<[f32; 2]>,
}

static CARD: Mutex<Option<Card>> = Mutex::new(None);

fn with<T>(f: impl FnOnce(&mut Card) -> T) -> T {
    let mut g = CARD.lock().unwrap_or_else(|e| e.into_inner());
    let c = g.get_or_insert_with(|| Card { streams: HashMap::new(), next_id: 1, clock: Instant::now(), out: VecDeque::new() });
    c.advance();
    f(c)
}

impl Card {
    /// Mixes the time elapsed since `clock`.
    fn advance(&mut self) {
        let now = Instant::now();
        let n = (now.duration_since(self.clock).as_secs_f64() * RATE as f64) as u64;
        if n == 0 {
            return;
        }
        self.clock += std::time::Duration::from_nanos(n * 1_000_000_000 / RATE as u64);
        if self.streams.values().all(|s| s.q.is_empty()) {
            return;
        }
        // a long stall (a hung page, a debugger) doesn't replay seconds at once
        let n = n.min(RATE as u64) as usize;
        let start = self.out.len();
        self.out.resize(start + n, [0.0; 2]);
        for s in self.streams.values_mut() {
            let step = s.rate as f64 / RATE as f64;
            for o in self.out.range_mut(start..) {
                let Some(a) = s.q.front().copied() else { break };
                let b = s.q.get(1).copied().unwrap_or(a);
                let t = s.phase as f32;
                o[0] += a[0] + (b[0] - a[0]) * t;
                o[1] += a[1] + (b[1] - a[1]) * t;
                s.phase += step;
                while s.phase >= 1.0 && !s.q.is_empty() {
                    s.phase -= 1.0;
                    s.q.pop_front();
                }
            }
            if s.q.is_empty() {
                s.phase = 0.0;
            }
        }
        if self.out.len() > OUT_CAP {
            let extra = self.out.len() - OUT_CAP;
            self.out.drain(..extra);
        }
    }
}

#[no_mangle]
pub extern "C" fn vnhost_audio_open(rate: u32, channels: u32, bits: u32, is_float: c_int) -> c_int {
    if rate == 0 || !(1..=2).contains(&channels) || !matches!((bits, is_float != 0), (8 | 16, false) | (32, true)) {
        return -1;
    }
    with(|c| {
        let id = c.next_id;
        c.next_id += 1;
        c.streams.insert(id, Stream { rate, channels, bits, float: is_float != 0, q: VecDeque::new(), phase: 0.0 });
        id
    })
}

#[no_mangle]
pub extern "C" fn vnhost_audio_close(id: c_int) {
    with(|c| c.streams.remove(&id));
}

/// Frames of stream `id` not played yet.
#[no_mangle]
pub extern "C" fn vnhost_audio_queued(id: c_int) -> u32 {
    with(|c| c.streams.get(&id).map_or(0, |s| s.q.len() as u32))
}

/// Queues `frames` frames in the stream's format; returns how many were taken (all of them).
#[no_mangle]
pub unsafe extern "C" fn vnhost_audio_write(id: c_int, data: *const u8, frames: u32) -> u32 {
    with(|c| {
        let Some(s) = c.streams.get_mut(&id) else { return 0 };
        let size = (s.bits / 8) as usize;
        let n = frames as usize * s.channels as usize;
        let bytes = std::slice::from_raw_parts(data, n * size);
        let sample = |i: usize| -> f32 {
            let b = &bytes[i * size..];
            match (s.bits, s.float) {
                (8, _) => (b[0] as f32 - 128.0) / 128.0,
                (16, _) => i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0,
                _ => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            }
        };
        let ch = s.channels as usize;
        let mut q = std::mem::take(&mut s.q);
        q.extend((0..frames as usize).map(|f| {
            let l = sample(f * ch);
            [l, if ch == 2 { sample(f * ch + 1) } else { l }]
        }));
        s.q = q;
        frames
    })
}

/// Moves up to `frames` mixed 48 kHz stereo frames (interleaved f32) to `buf`; returns how many.
#[no_mangle]
pub unsafe extern "C" fn vnhost_audio_take(buf: *mut f32, frames: u32) -> u32 {
    with(|c| {
        let n = c.out.len().min(frames as usize);
        let dst = std::slice::from_raw_parts_mut(buf, n * 2);
        for (i, f) in c.out.drain(..n).enumerate() {
            dst[i * 2] = f[0];
            dst[i * 2 + 1] = f[1];
        }
        n as u32
    })
}
