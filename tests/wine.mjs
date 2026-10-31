// Headless test of web/wine.html (Wine in wasm): logs the console with timestamps and saves the
// screen vnhost composes (read from the wasm heap: headless Chromium can't show WebGPU canvases)
// as a PNG every INTERVAL seconds. After the first shot, CLICKS="x,y;x,y" clicks there (page
// pixels) and KEYS="KeyA Enter" presses those keys; CLICK_EVERY="x,y" clicks there after every
// shot (to page through a game). CHROME_ARGS adds browser flags.
//
//   node tests/wine.mjs URL OUT_DIR [SECONDS] [INTERVAL]
const pw = await import(process.env.PLAYWRIGHT ?? "playwright");
const { chromium } = pw.default ?? pw;
import { mkdirSync, appendFileSync, writeFileSync } from "node:fs";
import { deflateSync } from "node:zlib";

const [url, out, seconds = "120", interval = "15"] = process.argv.slice(2);
mkdirSync(out, { recursive: true });
const t0 = Date.now();
const stamp = () => ((Date.now() - t0) / 1000).toFixed(1).padStart(7);
const logFile = `${out}/console.log`;
const browser = await chromium.launch({ args: ["--enable-unsafe-webgpu", ...(process.env.CHROME_ARGS ?? "").split(" ").filter(Boolean)] });
const page = await browser.newPage({ viewport: { width: 1280, height: 720 } });
page.on("console", (m) => appendFileSync(logFile, `${stamp()} [${m.type()}] ${m.text()}\n`));
page.on("pageerror", (e) => appendFileSync(logFile, `${stamp()} [pageerror] ${e.message}\n`));
await page.goto(url);

const CRC = new Int32Array(256).map((_, n) => {
  for (let k = 0; k < 8; k++) n = n & 1 ? 0xedb88320 ^ (n >>> 1) : n >>> 1;
  return n;
});
function crc32(b) {
  let c = -1;
  for (const x of b) c = CRC[(c ^ x) & 0xff] ^ (c >>> 8);
  return (c ^ -1) >>> 0;
}
function png(w, h, bgra) {
  const raw = Buffer.alloc((w * 3 + 1) * h);
  for (let y = 0; y < h; y++)
    for (let x = 0; x < w; x++) {
      const s = (y * w + x) * 4, d = y * (w * 3 + 1) + 1 + x * 3;
      raw[d] = bgra[s + 2];
      raw[d + 1] = bgra[s + 1];
      raw[d + 2] = bgra[s];
    }
  const chunk = (type, data) => {
    const b = Buffer.alloc(12 + data.length);
    b.writeUInt32BE(data.length, 0);
    b.write(type, 4, "ascii");
    data.copy(b, 8);
    b.writeUInt32BE(crc32(b.subarray(4, 8 + data.length)), 8 + data.length);
    return b;
  };
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(w, 0);
  ihdr.writeUInt32BE(h, 4);
  ihdr.set([8, 2, 0, 0, 0], 8);
  return Buffer.concat([Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]), chunk("IHDR", ihdr), chunk("IDAT", deflateSync(raw)), chunk("IEND", Buffer.alloc(0))]);
}

async function shot(n) {
  const f = await page.evaluate(() => {
    const m = window.vnvmWine;
    if (!m) return null;
    const p = m._malloc(12) >>> 0;
    m._vnhost_display_size(p, p + 4);
    const w = m.HEAPU32[p >>> 2], h = m.HEAPU32[(p + 4) >>> 2];
    const bits = m._vnhost_display_frame(p + 8) >>> 0;
    const px = m.HEAPU8.slice(bits, bits + w * h * 4);
    let s = "";
    for (let i = 0; i < px.length; i += 0x8000) s += String.fromCharCode(...px.subarray(i, i + 0x8000));
    return { w, h, data: btoa(s) };
  });
  if (f) writeFileSync(`${out}/screen-${String(n).padStart(3, "0")}.png`, png(f.w, f.h, Buffer.from(f.data, "base64")));
}

const end = t0 + Number(seconds) * 1000;
let n = 0;
while (Date.now() < end) {
  await page.waitForTimeout(Number(interval) * 1000);
  await shot(++n);
  if (n === 1) {
    for (const xy of process.env.CLICKS?.split(";") ?? []) {
      const [x, y] = xy.split(",").map(Number);
      await page.mouse.click(x, y);
    }
    for (const key of process.env.KEYS?.split(" ").filter(Boolean) ?? []) await page.keyboard.press(key);
  }
  if (process.env.CLICK_EVERY) {
    const [x, y] = process.env.CLICK_EVERY.split(",").map(Number);
    await page.mouse.click(x, y);
  }
}
await browser.close();
