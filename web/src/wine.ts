// Runs Wine compiled to wasm (hangover/build.sh → public/wine/) in the page: the Wine root and
// the prefix are streamed from the store, windows are composed by vnhost
// (crates/vnhost/src/display.rs) and shown with WebGPU, input goes back to the browser display
// driver (dlls/winebrowser.drv).
//
//   /wine.html?run=C:\path\to.exe   program to start (default C:\wintest.exe)
//   &args=a,b                       its arguments
//   &debug=+relay,...               WINEDEBUG
//   &mount=/vnvm/game=tree          also mount a store tree (repeatable; Z:\vnvm\game\...)
//   &env=NAME=value                 extra environment variable (repeatable, e.g. VNVM_TRACE_PROC=1)

import { Presenter } from "./present";
import { linuxKeycode } from "./keymap";
import { AudioOut } from "./wine-audio";

const params = new URLSearchParams(location.search);
const status = document.querySelector<HTMLDivElement>("#status")!;
const canvas = document.querySelector<HTMLCanvasElement>("#screen")!;
const startButton = document.querySelector<HTMLButtonElement>("#start")!;

const WINE = new URL("wine/", location.href).href;

interface WineModule {
  FS: {
    mkdir(path: string): void;
    writeFile(path: string, data: Uint8Array): void;
  };
  HEAPU8: Uint8Array;
  HEAPU32: Uint32Array;
  HEAPF32: Float32Array;
  _malloc(size: number): number;
  _vnhost_display_size(w: number, h: number): void;
  _vnhost_display_frame(io: number): number;
  _vnhost_input_push(kind: number, x: number, y: number, data: number): number;
  _vnhost_audio_take(buf: number, frames: number): number;
  _vnhost_vfs_put_chunk(hash: number, data: number, len: number): void;
  wasmMemory: WebAssembly.Memory;
}

// input record kinds (crates/vnhost/src/display.rs)
const MOUSE_MOVE = 1,
  MOUSE_DOWN = 2,
  MOUSE_UP = 3,
  WHEEL = 4,
  KEY_DOWN = 5,
  KEY_UP = 6;

function log(line: string): void {
  console.log(line);
  status.textContent = line;
}

/** KeyboardEvent.code -> Windows virtual-key code. */
function virtualKey(code: string): number {
  let m;
  if ((m = /^Key([A-Z])$/.exec(code))) return m[1]!.charCodeAt(0);
  if ((m = /^Digit(\d)$/.exec(code))) return 0x30 + Number(m[1]);
  if ((m = /^Numpad(\d)$/.exec(code))) return 0x60 + Number(m[1]);
  if ((m = /^F(\d+)$/.exec(code))) return 0x6f + Number(m[1]);
  const keys: Record<string, number> = {
    Backspace: 0x08, Tab: 0x09, Enter: 0x0d, NumpadEnter: 0x0d, ShiftLeft: 0xa0, ShiftRight: 0xa1,
    ControlLeft: 0xa2, ControlRight: 0xa3, AltLeft: 0xa4, AltRight: 0xa5, Pause: 0x13, CapsLock: 0x14,
    Escape: 0x1b, Space: 0x20, PageUp: 0x21, PageDown: 0x22, End: 0x23, Home: 0x24, ArrowLeft: 0x25,
    ArrowUp: 0x26, ArrowRight: 0x27, ArrowDown: 0x28, PrintScreen: 0x2c, Insert: 0x2d, Delete: 0x2e,
    MetaLeft: 0x5b, MetaRight: 0x5c, ContextMenu: 0x5d, NumpadMultiply: 0x6a, NumpadAdd: 0x6b,
    NumpadSubtract: 0x6d, NumpadDecimal: 0x6e, NumpadDivide: 0x6f, NumLock: 0x90, ScrollLock: 0x91,
    Semicolon: 0xba, Equal: 0xbb, Comma: 0xbc, Minus: 0xbd, Period: 0xbe, Slash: 0xbf, Backquote: 0xc0,
    BracketLeft: 0xdb, Backslash: 0xdc, BracketRight: 0xdd, Quote: 0xde, IntlBackslash: 0xe2,
    IntlRo: 0xc1, IntlYen: 0xdc, Convert: 0x1c, NonConvert: 0x1d, KanaMode: 0x15,
  };
  return keys[code] ?? 0;
}

async function boot(audio: Promise<AudioOut>): Promise<void> {
  const presenter = await Presenter.create(canvas);
  // Keep the GPU device reachable: Chromium destroys a collected one.
  (window as unknown as { vnvmPresenter: Presenter }).vnvmPresenter = presenter;
  log("Wine を読み込み中");
  const unixModule = fetch(`${WINE}wineunix.so`).then(async (r) => {
    if (!r.ok) throw new Error(`wineunix.so: HTTP ${r.status}`);
    return new Uint8Array(await r.arrayBuffer());
  });
  const run = params.get("run") ?? "C:\\wintest.exe";
  // x86 regions earlier runs of this program saved (vnvm-server /jit); &jit=0 for none
  const jitName = params.get("jit") ?? run.replace(/[^A-Za-z0-9_.-]/g, "_");
  const jit = jitName === "0" ? Promise.resolve(undefined) : loadJit(jitName);
  const ahead = jitName === "0" ? undefined : readAhead(jitName);
  // the browser keeps a versioned wine.wasm with its compiled code (see vnvm-server)
  const wasmVersion = fetch(`${WINE}wine.wasm`, { method: "HEAD" }).then(
    (r) => `${r.headers.get("last-modified") ?? ""}-${r.headers.get("content-length") ?? ""}`,
    () => "",
  );
  const scriptUrl = `${WINE}wine.js`;
  const factory = (await import(/* @vite-ignore */ scriptUrl)).default as (m: object) => Promise<WineModule>;
  const unix = await unixModule;
  const version = await wasmVersion;
  const moduleArg = {
    arguments: [run, ...(params.get("args")?.split(",").filter(Boolean) ?? [])],
    mainScriptUrlOrBlob: scriptUrl,
    locateFile: (path: string) =>
      path === "wine.wasm" && version ? `${WINE}${path}?v=${encodeURIComponent(version)}` : `${WINE}${path}`,
    vnvmEnv: {
      VNVM_MOUNTS: ["/opt/wine-vnbox=wineroot", "/vnvm/prefix=prefix32", ...params.getAll("mount")].join(":"),
      LANG: "ja_JP.UTF-8",
      VNVM_UNIX_MODULE: "/vnvm/wineunix.so",
      WINEPREFIX: "/vnvm/prefix",
      WINEDEBUG: params.get("debug") ?? "",
      ...(jitName === "0" ? {} : { VNVM_JIT_URL: new URL(`jit-save/${jitName}`, location.href).href }),
      HOME: "/home/web",
      USER: "web",
      ...Object.fromEntries(params.getAll("env").map((e) => [e.slice(0, e.indexOf("=")), e.slice(e.indexOf("=") + 1)])),
    },
    preRun: [
      (m: WineModule) => {
        m.FS.mkdir("/vnvm");
        m.FS.mkdir("/home/web");
        m.FS.writeFile("/vnvm/wineunix.so", unix);
      },
    ],
    vnvmJit: await jit,
    print: (line: string) => console.log(line),
    printErr: (line: string) => console.warn(line),
  };
  log(`${run} を起動中`);
  const wine = await factory(moduleArg);
  // for tests/wine.mjs (headless Chromium can't present WebGPU canvases)
  (window as unknown as { vnvmWine: WineModule }).vnvmWine = wine;
  ahead?.(wine);
  attachInput(wine, presenter);
  present(wine, presenter);
  (await audio).attach(wine);
}

/** The saved regions `name` (compiled while they download: the browser caches the code), if any. */
async function loadJit(name: string): Promise<{ module: WebAssembly.Module; idx: Uint8Array } | undefined> {
  const ok = (r: Response) => (r.ok ? r : Promise.reject(new Error(`HTTP ${r.status}`)));
  try {
    // the index changes with the module: its hash versions the module's URL
    const idx = new Uint8Array(await (await fetch(`jit/${name}.idx`).then(ok)).arrayBuffer());
    let h = 0x811c9dc5;
    for (const b of idx) h = Math.imul(h ^ b, 0x01000193);
    const module = await WebAssembly.compileStreaming(fetch(`jit/${name}.wasm?v=${(h >>> 0).toString(16)}`).then(ok));
    return { module, idx };
  } catch (e) {
    console.log(`no saved regions for ${name}: ${e}`);
    return undefined;
  }
}

/**
 * Fetches the store chunks the last run of `name` read first (saved with its regions), several
 * at a time: Wine's threads read the store one synchronous request at a time, which took about
 * 2 s of the start (fonts are read whole). Returns the function that hands them to Wine.
 */
function readAhead(name: string): (wine: WineModule) => void {
  const store = new URL("../store/", WINE).href;
  const got: [Uint8Array, Uint8Array][] = [];
  let wine: WineModule | undefined;
  let p = 0;
  const put = (w: WineModule, hash: Uint8Array, data: Uint8Array) => {
    p ||= w._malloc(16) >>> 0;
    const d = w._malloc(Math.max(data.length, 1)) >>> 0;
    // (a fresh view: the memory may have grown, and HEAPU8 is this thread's old one)
    const heap = new Uint8Array(w.wasmMemory.buffer);
    heap.set(hash, p);
    heap.set(data, d);
    w._vnhost_vfs_put_chunk(p, d, data.length);
  };
  (async () => {
    const r = await fetch(`jit/${name}.chunks`);
    if (!r.ok) return;
    const list = new Uint8Array(await r.arrayBuffer());
    let next = 0;
    const one = async () => {
      while (next * 16 < list.length) {
        const hash = list.subarray(next * 16, ++next * 16);
        const hex = Array.from(hash, (b) => b.toString(16).padStart(2, "0")).join("");
        const c = await fetch(`${store}chunks/${hex.slice(0, 2)}/${hex}`);
        if (!c.ok) continue;
        const data = new Uint8Array(await c.arrayBuffer());
        if (wine) put(wine, hash, data);
        else got.push([hash, data]);
      }
    };
    await Promise.all(Array.from({ length: 6 }, one));
  })().catch((e) => console.log(`read-ahead: ${e}`));
  return (w) => {
    wine = w;
    for (const [hash, data] of got.splice(0)) put(w, hash, data);
  };
}

/** Shows the screen whenever it changes. */
function present(wine: WineModule, presenter: Presenter): void {
  const scratch = wine._malloc(20) >>> 0;
  wine._vnhost_display_size(scratch, scratch + 4);
  const width = wine.HEAPU32[scratch >>> 2]!;
  const height = wine.HEAPU32[(scratch + 4) >>> 2]!;
  let shown = -1;
  const io = (scratch + 8) >>> 2;
  const frame = () => {
    wine.HEAPU32[io] = shown;
    const bits = wine._vnhost_display_frame(scratch + 8) >>> 0;
    const serial = wine.HEAPU32[io]!;
    if (serial !== shown) {
      shown = serial;
      presenter.update(wine.HEAPU8, bits, width, height, width * 4, wine.HEAPU32[io + 1]!, wine.HEAPU32[io + 2]!);
      if (status.textContent) log("");
    }
    requestAnimationFrame(frame);
  };
  requestAnimationFrame(frame);
}

function attachInput(wine: WineModule, presenter: Presenter): void {
  const push = (kind: number, x: number, y: number, data: number) => wine._vnhost_input_push(kind, x, y, data);
  // DOM button -> 0 left, 1 right, 2 middle
  const button = (b: number) => [0, 2, 1][b] ?? -1;
  canvas.addEventListener("pointermove", (e) => {
    for (const ev of e.getCoalescedEvents?.() ?? [e]) push(MOUSE_MOVE, ...presenter.toFrame(ev), 0);
  });
  canvas.addEventListener("pointerdown", (e) => {
    canvas.setPointerCapture(e.pointerId);
    if (button(e.button) >= 0) push(MOUSE_DOWN, ...presenter.toFrame(e), button(e.button));
    e.preventDefault();
  });
  canvas.addEventListener("pointerup", (e) => {
    if (button(e.button) >= 0) push(MOUSE_UP, ...presenter.toFrame(e), button(e.button));
  });
  canvas.addEventListener("contextmenu", (e) => e.preventDefault());
  canvas.addEventListener(
    "wheel",
    (e) => {
      push(WHEEL, ...presenter.toFrame(e), (e.deltaY < 0 ? 120 : -120) >>> 0);
      e.preventDefault();
    },
    { passive: false },
  );
  const key = (kind: number) => (e: KeyboardEvent) => {
    const vk = virtualKey(e.code);
    if (!vk) return;
    // PC scan code set 1 matches the Linux keycodes below 89
    const scan = linuxKeycode(e.code);
    push(kind, 0, 0, vk | ((scan < 89 ? scan : 0) << 16));
    e.preventDefault();
  };
  window.addEventListener("keydown", key(KEY_DOWN));
  window.addEventListener("keyup", key(KEY_UP));
}

function checkSupport(): string | null {
  if (!crossOriginIsolated) return "SharedArrayBuffer が使えません (COOP/COEP ヘッダが必要)";
  if (!("gpu" in navigator)) return "WebGPU に対応していないブラウザです";
  return null;
}

const problem = checkSupport();
if (problem) {
  log(problem);
  startButton.disabled = true;
} else {
  const start = () => {
    startButton.hidden = true;
    // the AudioContext comes from this click (autoplay rules)
    boot(AudioOut.create()).catch((e) => log(`エラー: ${e instanceof Error ? e.message : e}`));
  };
  startButton.addEventListener("click", start);
  if (params.has("autostart")) start();
}
