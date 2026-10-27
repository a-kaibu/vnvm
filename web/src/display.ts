// The page side of QEMU's `-display wasm` (ui/wasm.c): polls the shared
// struct every animation frame for a new dirty rectangle, and feeds pointer and
// keyboard input into its ring.

import type { Presenter } from "./present";
import { linuxKeycode } from "./keymap";

// Byte offsets in VnvmShared (ui/wasm.c).
const SEQ = 4,
  DATA = 8,
  WIDTH = 12,
  HEIGHT = 16,
  STRIDE = 20,
  DY0 = 32,
  DX1 = 36,
  DY1 = 40,
  IN_HEAD = 48,
  IN_TAIL = 52,
  RING = 56;

const EV_KEY = 1,
  EV_ABS = 2,
  EV_BTN = 3,
  EV_WHEEL = 4;

export interface DisplayStats {
  frames: number;
  uploadedRows: number;
}

export class Display {
  readonly stats: DisplayStats = { frames: 0, uploadedRows: 0 };
  private seq = 0;
  private i32: Int32Array;
  private lastFull = 0;

  constructor(
    private readonly heap: () => Uint8Array,
    private readonly base: number,
    private readonly ringSize: number,
    private readonly presenter: Presenter,
  ) {
    this.i32 = new Int32Array(heap().buffer);
    requestAnimationFrame(this.frame);
    this.bindInput();
  }

  private load(off: number): number {
    return Atomics.load(this.i32, (this.base + off) >> 2);
  }

  private frame = (t: number): void => {
    requestAnimationFrame(this.frame);
    const seq = this.load(SEQ);
    // A full refresh now and then covers a dirty rect lost to a race with QEMU.
    const full = t - this.lastFull > 2000;
    if (seq === this.seq && !full) return;
    this.seq = seq;
    const data = this.load(DATA) >>> 0;
    const width = this.load(WIDTH);
    const height = this.load(HEIGHT);
    const stride = this.load(STRIDE);
    if (!data || !width) return;
    let y0 = this.load(DY0);
    let y1 = this.load(DY1);
    Atomics.store(this.i32, (this.base + DX1) >> 2, 0);
    if (full) {
      y0 = 0;
      y1 = height;
      this.lastFull = t;
    }
    this.presenter.update(this.heap(), data, width, height, stride, Math.max(0, y0), Math.min(height, y1));
    this.stats.frames++;
    this.stats.uploadedRows += Math.max(0, y1 - y0);
  };

  private push(type: number, a: number, b = 0): void {
    const head = this.load(IN_HEAD) >>> 0;
    const tail = this.load(IN_TAIL) >>> 0;
    if (head - tail >= this.ringSize) return; // full: drop
    const at = (this.base + RING + (head % this.ringSize) * 16) >> 2;
    this.i32[at] = type;
    this.i32[at + 1] = a;
    this.i32[at + 2] = b;
    this.i32[at + 3] = 0;
    Atomics.store(this.i32, (this.base + IN_HEAD) >> 2, (head + 1) | 0);
  }

  private bindInput(): void {
    const canvas = this.presenter.canvas;
    canvas.style.touchAction = "none";
    const move = (e: PointerEvent): void => {
      const [x, y] = this.presenter.toFrame(e);
      this.push(EV_ABS, x, y);
    };
    // QEMU InputButton: left 0, middle 1, right 2.
    const button = (b: number): number => (b === 1 ? 1 : b === 2 ? 2 : 0);
    canvas.addEventListener("pointermove", move);
    canvas.addEventListener("pointerdown", (e) => {
      canvas.setPointerCapture(e.pointerId);
      move(e);
      this.push(EV_BTN, button(e.button), 1);
      e.preventDefault();
    });
    canvas.addEventListener("pointerup", (e) => {
      move(e);
      this.push(EV_BTN, button(e.button), 0);
      e.preventDefault();
    });
    canvas.addEventListener("contextmenu", (e) => e.preventDefault());
    canvas.addEventListener(
      "wheel",
      (e) => {
        this.push(EV_WHEEL, e.deltaY > 0 ? 1 : -1);
        e.preventDefault();
      },
      { passive: false },
    );
    const key = (down: boolean) => (e: KeyboardEvent) => {
      const code = linuxKeycode(e.code);
      if (!code) return;
      this.push(EV_KEY, code, down ? 1 : 0);
      e.preventDefault();
    };
    window.addEventListener("keydown", key(true));
    window.addEventListener("keyup", key(false));
  }
}
