// Boots the VM: QEMU Wasm (public/qemu/) with the streaming root disk, a game
// disk and a save disk, presenting through WebGPU.
//
//   /?game=<name>   game disk store/disks/<name>, save disk = blank template
//   &snapshot=<n>   resume store/snapshots/<n> instead of booting (see image/snapshot.sh)
//   &cmdline=...    extra kernel arguments (e.g. init=/usr/local/bin/vnvm-bench)
//   &jit=T,M        -accel tcg,jit-threshold=T,jit-max=M (see tcg/wasm32.h)

import { Presenter } from "./present";
import { Display } from "./display";
import { startAudio } from "./audio";

const params = new URLSearchParams(location.search);
const game = params.get("game") ?? "";
const snapshot = params.get("snapshot") ?? "";
const status = document.querySelector<HTMLDivElement>("#status")!;
const canvas = document.querySelector<HTMLCanvasElement>("#screen")!;
const startButton = document.querySelector<HTMLButtonElement>("#start")!;

const STORE = new URL("store/", location.href).href;
const QEMU = new URL("qemu/", location.href).href;

function log(line: string): void {
  console.log(line);
  status.textContent = line;
}

async function fetchBytes(url: string): Promise<Uint8Array> {
  const r = await fetch(url);
  if (!r.ok) throw new Error(`${url}: HTTP ${r.status}`);
  return new Uint8Array(await r.arrayBuffer());
}

interface QemuModule {
  FS: {
    mkdir(path: string): void;
    writeFile(path: string, data: Uint8Array): void;
  };
  HEAPU8: Uint8Array;
}

async function boot(audio: AudioContext): Promise<void> {
  const presenter = await Presenter.create(canvas);
  // Keep the GPU device reachable: Chromium destroys a collected one.
  (window as unknown as { vnvmPresenter: Presenter }).vnvmPresenter = presenter;
  const [qemuArgs, cmdline] = await Promise.all([
    fetch(`${STORE}boot/qemu-args.txt`).then((r) => r.text()),
    fetch(`${STORE}boot/kernel-cmdline.txt`).then((r) => r.text()),
  ]);
  const files: Record<string, Promise<Uint8Array>> = {
    "/pack/vmlinuz": fetchBytes(`${STORE}boot/vmlinuz`),
  };
  for (const f of ["bios-256k.bin", "vgabios-stdvga.bin", "kvmvapic.bin", "linuxboot_dma.bin"]) {
    files[`/pack/${f}`] = fetchBytes(`${QEMU}bios/${f}`);
  }
  if (snapshot) files["/pack/vm.state"] = fetchBytes(`${STORE}snapshots/${snapshot}/vm.state`);
  const loaded = Object.fromEntries(
    await Promise.all(Object.entries(files).map(async ([k, v]) => [k, await v] as const)),
  );
  log("QEMU を起動中");

  const disk = (name: string) => `vnvm:${STORE}disks/${name}/manifest.json`;
  const jit = params.get("jit")?.split(",") ?? [];
  const accel = jit.length === 2 ? `tcg,jit-threshold=${jit[0]},jit-max=${jit[1]}` : "tcg";
  const args = [
    ...qemuArgs.split(/\s+/).filter(Boolean).map((a, i, all) => (all[i - 1] === "-accel" ? accel : a)),
    ...["-L", "/pack", "-kernel", "/pack/vmlinuz", "-append", `${cmdline.trim()} ${params.get("cmdline") ?? ""}`],
    ...["-display", "wasm", "-audiodev", "wasm,id=snd0", "-serial", "file:/dev/stdout"],
    ...["-drive", `if=none,id=root,driver=vnvm,url=${disk("root").slice(5)}`],
    ...["-drive", `if=none,id=game,driver=vnvm,url=${disk(game || "empty-game").slice(5)}`],
    ...["-drive", `if=none,id=save,driver=vnvm,url=${disk("save").slice(5)}`],
    ...(snapshot ? ["-incoming", "file:/pack/vm.state"] : []),
  ];

  const scriptUrl = `${QEMU}qemu-system-x86_64.js`;
  const factory = (await import(/* @vite-ignore */ scriptUrl)).default as (m: object) => Promise<QemuModule>;
  const moduleArg = {
    arguments: args,
    mainScriptUrlOrBlob: scriptUrl,
    locateFile: (path: string) => `${QEMU}${path}`,
    preRun: [
      (m: QemuModule) => {
        m.FS.mkdir("/pack");
        for (const [path, data] of Object.entries(loaded)) m.FS.writeFile(path, data);
      },
    ],
    print: (line: string) => {
      if (line.startsWith("VNVM: ")) log(line.slice(6));
      else console.log(line);
    },
    printErr: (line: string) => console.warn(line),
    onVnvmDisplay: (base: number, ringSize: number) => {
      const display = new Display(() => (moduleArg as unknown as QemuModule).HEAPU8, base, ringSize, presenter);
      (window as unknown as { vnvmDisplay: Display }).vnvmDisplay = display;
    },
    onVnvmAudio: (base: number) => {
      void startAudio(audio, (moduleArg as unknown as QemuModule).HEAPU8.buffer as SharedArrayBuffer, base);
    },
  };
  await factory(moduleArg);
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
    const audio = new AudioContext({ sampleRate: 48000, latencyHint: "interactive" });
    boot(audio).catch((e) => log(`エラー: ${e instanceof Error ? e.message : e}`));
  };
  startButton.addEventListener("click", start);
  if (params.has("autostart")) start();
}
