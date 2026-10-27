// Headless boot test: opens the page, logs the guest's VNVM: lines with
// timestamps and saves a screenshot every INTERVAL seconds.
//
//   node tests/boot.mjs URL OUT_DIR [SECONDS] [INTERVAL]
// PLAYWRIGHT can point at a global install (e.g. $(npm root -g)/playwright/index.js).
const pw = await import(process.env.PLAYWRIGHT ?? "playwright");
const { chromium } = pw.default ?? pw;
import { mkdirSync, appendFileSync } from "node:fs";

const [url, out, seconds = "600", interval = "30"] = process.argv.slice(2);
mkdirSync(out, { recursive: true });
const t0 = Date.now();
const stamp = () => ((Date.now() - t0) / 1000).toFixed(1).padStart(7);
const logFile = `${out}/console.log`;
// CHROME_ARGS: extra flags, e.g. --js-flags=--no-liftoff to test V8 tiering.
const browser = await chromium.launch({
  args: ["--enable-unsafe-webgpu", "--autoplay-policy=no-user-gesture-required", ...(process.env.CHROME_ARGS?.split(" ") ?? [])],
});
const page = await browser.newPage({ viewport: { width: 1024, height: 768 } });
page.on("console", (m) => appendFileSync(logFile, `${stamp()} [${m.type()}] ${m.text()}\n`));
page.on("pageerror", (e) => appendFileSync(logFile, `${stamp()} [pageerror] ${e.message}\n`));
await page.goto(url);
const end = t0 + Number(seconds) * 1000;
let n = 0;
while (Date.now() < end) {
  await page.waitForTimeout(Number(interval) * 1000);
  await page.screenshot({ path: `${out}/shot-${String(++n).padStart(3, "0")}.png` });
  const status = await page.locator("#status").textContent();
  appendFileSync(logFile, `${stamp()} [status] ${status}\n`);
}
await browser.close();
