// The page side of QEMU's `-audiodev wasm` (audio/wasmaudio.c): an
// AudioWorklet that drains the S16 stereo ring in the shared wasm heap.

// Byte offsets in WasmAudioShared.
const FRAMES = 12,
  WRITE_POS = 16,
  READ_POS = 20,
  UNDERRUNS = 28,
  RING = 32;

const WORKLET = /* js */ `
class VnvmAudio extends AudioWorkletProcessor {
  constructor(options) {
    super();
    const { buffer, base, frames } = options.processorOptions;
    this.i32 = new Int32Array(buffer);
    this.i16 = new Int16Array(buffer);
    this.base = base;
    this.frames = frames;
    this.ring = (base + ${RING}) >> 1;
  }
  process(_inputs, outputs) {
    const [l, r] = outputs[0];
    const i32 = this.i32, b = this.base >> 2;
    const w = Atomics.load(i32, b + ${WRITE_POS / 4}) >>> 0;
    let rd = Atomics.load(i32, b + ${READ_POS / 4}) >>> 0;
    const avail = (w - rd) >>> 0;
    const n = Math.min(avail, l.length);
    for (let i = 0; i < n; i++) {
      const at = this.ring + (((rd + i) & (this.frames - 1)) << 1);
      l[i] = this.i16[at] / 32768;
      r[i] = this.i16[at + 1] / 32768;
    }
    if (n < l.length) {
      l.fill(0, n);
      r.fill(0, n);
      if (avail === 0) Atomics.add(i32, b + ${UNDERRUNS / 4}, 1);
    }
    Atomics.store(i32, b + ${READ_POS / 4}, (rd + n) | 0);
    return true;
  }
}
registerProcessor("vnvm-audio", VnvmAudio);
`;

export async function startAudio(ctx: AudioContext, buffer: SharedArrayBuffer, base: number): Promise<void> {
  const frames = new Int32Array(buffer)[(base + FRAMES) >> 2];
  const url = URL.createObjectURL(new Blob([WORKLET], { type: "text/javascript" }));
  await ctx.audioWorklet.addModule(url);
  URL.revokeObjectURL(url);
  const node = new AudioWorkletNode(ctx, "vnvm-audio", {
    numberOfInputs: 0,
    outputChannelCount: [2],
    processorOptions: { buffer, base, frames },
  });
  node.connect(ctx.destination);
}
