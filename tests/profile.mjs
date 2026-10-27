// CPU profile of the QEMU workers: boots the page, waits, samples every worker
// with the V8 profiler and prints the top functions by self time.
//
//   node tests/profile.mjs URL [WAIT_SECONDS[,WAIT2,...]] [PROFILE_SECONDS] [OUT.cpuprofile]
//
// Several comma-separated WAIT values profile several windows in one boot
// (each PROFILE_SECONDS long, timed from page load).
const pw = await import(process.env.PLAYWRIGHT ?? "playwright");
const { chromium } = pw.default ?? pw;
import { writeFileSync } from "node:fs";

const [url, wait = "60", secs = "20", out = ""] = process.argv.slice(2);
const browser = await chromium.launch({ args: ["--enable-unsafe-webgpu"] });
const page = await browser.newPage();
page.on("console", (m) => {
  const t = m.text();
  if (t.includes("VNVM") || t.includes("panic")) console.log(t);
});
const cdp = await page.context().newCDPSession(page);
const sessions = [];
// Non-flat auto-attach: worker sessions are reached through the page session.
cdp.on("Target.attachedToTarget", ({ sessionId, targetInfo }) => {
  if (targetInfo.type === "worker") sessions.push({ sessionId, title: targetInfo.url });
});
await cdp.send("Target.setAutoAttach", { autoAttach: true, waitForDebuggerOnStart: false, flatten: false });
await page.goto(url);
const waits = wait.split(",").map(Number);
const t0 = Date.now();

// Raw CDP over the flat session ids.
const send = (sessionId, method, params) =>
  cdp.send("Target.sendMessageToTarget", { sessionId, message: JSON.stringify({ id: 1, method, params }) });
let results = new Map();
cdp.on("Target.receivedMessageFromTarget", ({ sessionId, message }) => {
  const m = JSON.parse(message);
  if (m.result?.profile) results.set(sessionId, m.result.profile);
});

function report(profile) {
  const self = new Map();
  const byId = new Map(profile.nodes.map((n) => [n.id, n]));
  const counts = new Map();
  for (const id of profile.samples) counts.set(id, (counts.get(id) ?? 0) + 1);
  let total = 0;
  for (const [id, c] of counts) {
    const n = byId.get(id);
    let name = n.callFrame.functionName || `(${n.callFrame.url.split("/").pop()}:${n.callFrame.lineNumber})`;
    // JIT TB modules are anonymous wasm functions loaded from blobs.
    if (/^wasm-function\[\d+\]$/.test(name) && !n.callFrame.url.includes("qemu-system")) name = "(TB code)";
    self.set(name, (self.get(name) ?? 0) + c);
    total += c;
  }
  if (total < 50) return null;
  const lines = [...self].sort((a, b) => b[1] - a[1]).slice(0, 30)
    .map(([name, c]) => `${((100 * c) / total).toFixed(1).padStart(6)}%  ${name}`);
  return { total, lines };
}

for (const [wi, w] of waits.entries()) {
  const until = t0 + w * 1000;
  if (Date.now() < until) await page.waitForTimeout(until - Date.now());
  results = new Map();
  console.log(`\n=== window ${wi}: t=${w}s, ${sessions.length} workers for ${secs}s`);
  for (const s of sessions) {
    await send(s.sessionId, "Profiler.enable", {}).catch(() => {});
    await send(s.sessionId, "Profiler.setSamplingInterval", { interval: 200 }).catch(() => {});
    await send(s.sessionId, "Profiler.start", {}).catch(() => {});
  }
  await page.waitForTimeout(Number(secs) * 1000);
  for (const s of sessions) await send(s.sessionId, "Profiler.stop", {}).catch(() => {});
  await page.waitForTimeout(3000);
  for (const [sid, profile] of results) {
    const r = report(profile);
    if (!r) continue;
    console.log(`\nworker ${sid.slice(0, 8)}: ${r.total} samples`);
    for (const l of r.lines) console.log(l);
    if (out) writeFileSync(out.replace(".cpuprofile", `-w${wi}-${sid.slice(0, 8)}.cpuprofile`), JSON.stringify(profile));
  }
}
await browser.close();
