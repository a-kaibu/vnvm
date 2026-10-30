// Plays what vnhost's sound card mixed (crates/vnhost/src/audio.rs: 48 kHz stereo f32).
// The page drains it every few milliseconds and hands the chunks to an AudioWorklet, which
// plays them in order and keeps its queue short (vnhost's wall clock and the audio clock
// drift a little apart).

const RATE = 48000;
/** frames the worklet may hold before dropping the oldest */
const MAX_QUEUE = RATE / 5;

const worklet = `
class Sink extends AudioWorkletProcessor {
  constructor() {
    super();
    this.chunks = [];
    this.offset = 0;
    this.queued = 0;
    this.port.onmessage = (e) => {
      this.chunks.push(e.data);
      this.queued += e.data.length / 2;
      while (this.queued > ${MAX_QUEUE} && this.chunks.length > 1) {
        const c = this.chunks.shift();
        this.queued -= c.length / 2 - this.offset;
        this.offset = 0;
      }
    };
  }
  process(_inputs, outputs) {
    const [l, r] = outputs[0];
    for (let i = 0; i < l.length; i++) {
      const c = this.chunks[0];
      if (!c) break;
      l[i] = c[this.offset * 2];
      r[i] = c[this.offset * 2 + 1];
      this.queued--;
      if (++this.offset * 2 >= c.length) {
        this.chunks.shift();
        this.offset = 0;
      }
    }
    return true;
  }
}
registerProcessor("vnvm-sink", Sink);
`;

export interface AudioSource {
  HEAPF32: Float32Array;
  _malloc(size: number): number;
  _vnhost_audio_take(buf: number, frames: number): number;
}

/** Created from a user gesture (autoplay rules); `attach` starts draining a running Wine. */
export class AudioOut {
  private constructor(
    private ctx: AudioContext,
    private node: AudioWorkletNode,
  ) {}

  static async create(): Promise<AudioOut> {
    const ctx = new AudioContext({ sampleRate: RATE, latencyHint: "interactive" });
    const url = URL.createObjectURL(new Blob([worklet], { type: "text/javascript" }));
    await ctx.audioWorklet.addModule(url);
    URL.revokeObjectURL(url);
    const node = new AudioWorkletNode(ctx, "vnvm-sink", { numberOfInputs: 0, outputChannelCount: [2] });
    node.connect(ctx.destination);
    return new AudioOut(ctx, node);
  }

  attach(wine: AudioSource): void {
    const frames = 4096;
    const buf = wine._malloc(frames * 8) >>> 0;
    setInterval(() => {
      if (this.ctx.state === "suspended") void this.ctx.resume();
      const n = wine._vnhost_audio_take(buf, frames);
      if (n) this.node.port.postMessage(wine.HEAPF32.slice(buf >>> 2, (buf >>> 2) + n * 2));
    }, 10);
  }
}
