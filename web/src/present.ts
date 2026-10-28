// Shows the guest framebuffer on the page's canvas with WebGPU, letterboxed.
// Only the rows QEMU reported dirty are uploaded each frame.

const SHADER = /* wgsl */ `
struct Out { @builtin(position) position: vec4f, @location(0) uv: vec2f };

@vertex fn vs(@builtin(vertex_index) i: u32) -> Out {
  let p = vec2f(f32((i << 1u) & 2u), f32(i & 2u));
  var out: Out;
  out.position = vec4f(p * 2.0 - 1.0, 0.0, 1.0);
  out.uv = vec2f(p.x, 1.0 - p.y);
  return out;
}

@group(0) @binding(0) var frame: texture_2d<f32>;
@group(0) @binding(1) var smp: sampler;

@fragment fn fs(in: Out) -> @location(0) vec4f {
  return vec4f(textureSample(frame, smp, in.uv).rgb, 1.0);
}
`;

export interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

/** Where a frame of `width`×`height` goes on a canvas of `cw`×`ch` device pixels. */
export function letterbox(cw: number, ch: number, width: number, height: number): Rect {
  let scale = Math.min(cw / width, ch / height);
  const integer = Math.floor(scale);
  if (integer >= 1 && integer / scale > 0.92) scale = integer;
  const w = Math.round(width * scale);
  const h = Math.round(height * scale);
  return { x: Math.floor((cw - w) / 2), y: Math.floor((ch - h) / 2), w, h };
}

export class Presenter {
  width = 1;
  height = 1;
  private rect: Rect = { x: 0, y: 0, w: 1, h: 1 };
  private texture: GPUTexture | null = null;
  private bindGroup: GPUBindGroup | null = null;
  // WebGPU may refuse views on SharedArrayBuffer; then rows are copied here first.
  private staging: Uint8Array<ArrayBuffer> | null = null;
  private sharedOk = true;

  private constructor(
    readonly canvas: HTMLCanvasElement,
    readonly adapter: GPUAdapter,
    readonly device: GPUDevice,
    private readonly context: GPUCanvasContext,
    private readonly pipeline: GPURenderPipeline,
    private readonly samplers: { nearest: GPUSampler; linear: GPUSampler },
  ) {}

  static async create(canvas: HTMLCanvasElement): Promise<Presenter> {
    const adapter = await navigator.gpu?.requestAdapter();
    if (!adapter) throw new Error("WebGPU が使えません");
    const device = await adapter.requestDevice();
    const context = canvas.getContext("webgpu")!;
    const format = navigator.gpu.getPreferredCanvasFormat();
    context.configure({ device, format, alphaMode: "opaque" });
    const module = device.createShaderModule({ code: SHADER });
    const pipeline = device.createRenderPipeline({
      layout: "auto",
      vertex: { module, entryPoint: "vs" },
      fragment: { module, entryPoint: "fs", targets: [{ format }] },
      primitive: { topology: "triangle-list" },
    });
    const samplers = {
      nearest: device.createSampler({ magFilter: "nearest", minFilter: "nearest" }),
      linear: device.createSampler({ magFilter: "linear", minFilter: "linear" }),
    };
    void device.lost.then((info) => console.error(`WebGPU device lost (${info.reason}): ${info.message}`));
    device.addEventListener("uncapturederror", (e) => console.error(`WebGPU: ${(e as GPUUncapturedErrorEvent).error.message}`));
    return new Presenter(canvas, adapter, device, context, pipeline, samplers);
  }

  /** The frame pixel under a pointer event, clamped to the frame. */
  toFrame(event: { clientX: number; clientY: number }): [number, number] {
    const bounds = this.canvas.getBoundingClientRect();
    const dpr = this.canvas.width / Math.max(1, bounds.width);
    const r = this.rect;
    const x = Math.floor((((event.clientX - bounds.left) * dpr - r.x) * this.width) / Math.max(1, r.w));
    const y = Math.floor((((event.clientY - bounds.top) * dpr - r.y) * this.height) / Math.max(1, r.h));
    return [Math.max(0, Math.min(this.width - 1, x)), Math.max(0, Math.min(this.height - 1, y))];
  }

  private layout(width: number, height: number): boolean {
    const dpr = window.devicePixelRatio || 1;
    const cw = Math.max(1, Math.floor(this.canvas.clientWidth * dpr));
    const ch = Math.max(1, Math.floor(this.canvas.clientHeight * dpr));
    let changed = width !== this.width || height !== this.height;
    if (this.canvas.width !== cw || this.canvas.height !== ch) {
      this.canvas.width = cw;
      this.canvas.height = ch;
      changed = true;
    }
    this.width = width;
    this.height = height;
    this.rect = letterbox(cw, ch, width, height);
    return changed;
  }

  /**
   * Uploads rows [y0, y1) of a BGRX frame that lives in `heap` at `offset`
   * (`stride` bytes per row) and draws it.
   */
  update(heap: Uint8Array, offset: number, width: number, height: number, stride: number, y0: number, y1: number): void {
    if (!width || !height) return;
    const resized = this.layout(width, height);
    const nearest = this.rect.w % width === 0 && this.rect.h % height === 0;
    if (!this.texture || this.texture.width !== width || this.texture.height !== height || resized) {
      this.texture?.destroy();
      this.texture = this.device.createTexture({
        size: [width, height],
        format: "bgra8unorm",
        usage: GPUTextureUsage.TEXTURE_BINDING | GPUTextureUsage.COPY_DST,
      });
      this.bindGroup = this.device.createBindGroup({
        layout: this.pipeline.getBindGroupLayout(0),
        entries: [
          { binding: 0, resource: this.texture.createView() },
          { binding: 1, resource: nearest ? this.samplers.nearest : this.samplers.linear },
        ],
      });
      y0 = 0;
      y1 = height;
    }
    if (y1 > y0) this.upload(heap, offset, width, stride, y0, y1);
    this.draw();
  }

  private upload(heap: Uint8Array, offset: number, width: number, stride: number, y0: number, y1: number): void {
    const rows = heap.subarray(offset + y0 * stride, offset + y1 * stride);
    const dst = { texture: this.texture!, origin: { x: 0, y: y0 } };
    const size = [width, y1 - y0];
    if (this.sharedOk) {
      try {
        this.device.queue.writeTexture(dst, rows as Uint8Array<ArrayBuffer>, { bytesPerRow: stride }, size);
        return;
      } catch {
        this.sharedOk = false;
      }
    }
    if (!this.staging || this.staging.length < rows.length) this.staging = new Uint8Array(rows.length);
    this.staging.set(rows);
    this.device.queue.writeTexture(dst, this.staging, { bytesPerRow: stride }, size);
  }

  draw(): void {
    if (!this.bindGroup) return;
    const encoder = this.device.createCommandEncoder();
    const pass = encoder.beginRenderPass({
      colorAttachments: [
        {
          view: this.context.getCurrentTexture().createView(),
          loadOp: "clear",
          storeOp: "store",
          clearValue: { r: 0, g: 0, b: 0, a: 1 },
        },
      ],
    });
    const r = this.rect;
    pass.setViewport(r.x, r.y, r.w, r.h, 0, 1);
    pass.setPipeline(this.pipeline);
    pass.setBindGroup(0, this.bindGroup);
    pass.draw(3);
    pass.end();
    this.device.queue.submit([encoder.finish()]);
  }
}
