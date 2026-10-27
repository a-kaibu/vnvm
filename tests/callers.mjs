// Who calls a function: aggregates the call stacks (bottom-up) leading to
// FUNC in a .cpuprofile from tests/profile.mjs.
//   node tests/callers.mjs FILE.cpuprofile FUNC [DEPTH]
import { readFileSync } from "node:fs";
const [file, fn, depth = "8"] = process.argv.slice(2);
const p = JSON.parse(readFileSync(file, "utf8"));
const byId = new Map(p.nodes.map((n) => [n.id, n]));
const parent = new Map();
for (const n of p.nodes) for (const c of n.children ?? []) parent.set(c, n.id);
const name = (n) => n.callFrame.functionName || `(${n.callFrame.url.split("/").pop()}:${n.callFrame.lineNumber})`;
const counts = new Map();
let hits = 0;
for (const id of p.samples) {
  const n = byId.get(id);
  if (name(n) !== fn) continue;
  hits++;
  const chain = [];
  let cur = parent.get(id);
  for (let i = 0; i < Number(depth) && cur; i++) {
    chain.push(name(byId.get(cur)));
    cur = parent.get(cur);
  }
  const k = chain.join(" < ");
  counts.set(k, (counts.get(k) ?? 0) + 1);
}
console.log(`${hits} samples of ${fn} (${((100 * hits) / p.samples.length).toFixed(1)}%)`);
for (const [k, c] of [...counts].sort((a, b) => b[1] - a[1]).slice(0, 15)) console.log(`${c.toString().padStart(6)}  ${k}`);
