// Disassemble the TB modules dumped by the JIT (console "VNVM-TB <base64>"
// lines in a boot test's console.log) with wabt.
//   node tests/tbdump.mjs OUT_DIR/console.log [INDEX]
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
const require = createRequire(import.meta.url + "/../../web/");
const wabt = await require("wabt")();
const [file, index = "0"] = process.argv.slice(2);
const lines = readFileSync(file, "utf8").split("\n").filter((l) => l.includes("VNVM-TB "));
const b64 = lines[Number(index)].split("VNVM-TB ")[1].trim();
const bytes = Uint8Array.from(Buffer.from(b64, "base64"));
const mod = wabt.readWasm(bytes, { readDebugNames: false, tail_call: true, threads: true });
console.log(`module ${index}: ${bytes.length} bytes`);
console.log(mod.toText({ foldExprs: false, inlineExport: false }));
