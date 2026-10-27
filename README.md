# vnvm

ブラウザの中で x86_64 の Linux VM を動かし、その上で vnbox と同じ Wine でビジュアルノベルを
動かすための実験リポジトリです。ゲストは無改造の Ubuntu 26.04 で、ディスクはサーバから
チャンク単位でストリーミングします。

```
ブラウザ (cross-origin isolated なページ)
├─ main thread      WebGPU 表示 (present.ts/display.ts)、入力、AudioWorklet
└─ Web Workers      QEMU (qemu-system-x86_64.wasm, emscripten + pthreads)
    ├─ QEMU main loop   デバイス、ブロック層、タイマ (futex ベースの poll)
    ├─ vCPU thread      TCG → wasm JIT (TB ごとに WebAssembly.Module を生成)
    └─ ディスク          block/vnvm.c → vnvm-disk.js (同期 XHR でチャンク取得)

サーバ (crates/vnvm-server, axum)
└─ store/   vnvm-pack が作る content-addressed なチャンク (256KiB, brotli)
```

* QEMU は [ktock/qemu-wasm](https://github.com/ktock/qemu-wasm) (QEMU 8.2 + TCG→wasm バックエンド) を
  ベースに、`patches/qemu-wasm-vnvm.patch` で JIT の書き直し・futex イベントループ・ブラウザ向け
  表示/音声/ブロックドライバなどを当てています。
* 描画は WebGPU のみ (WebGL2/Canvas2D へのフォールバックはしません)。
* ゲストの Wine は vnbox が使っている Wine 11.20 (vnbox のパッチ入り) をそのまま入れます。

## ディレクトリ

| パス | 内容 |
| --- | --- |
| `build/` | emsdk 用の依存ライブラリ (`deps.sh`)、QEMU fork の取得 (`fetch-qemu.sh`)、wasm ビルド (`qemu.sh`)、スナップショット用ネイティブビルド (`qemu-native.sh`) |
| `patches/qemu-wasm-vnvm.patch` | qemu-wasm への変更すべて |
| `image/` | ゲストイメージ: debootstrap (`build-rootfs.sh`)、vnbox Wine の組み込み (`install-wine.sh`)、ディスク化 (`make-disk.sh`)、ネイティブ QEMU でのスナップショット (`snapshot.sh`) |
| `crates/vnvm-pack` | ディスクイメージをチャンクに分割する CLI (`disk` / `game` / `blank`) |
| `crates/vnvm-server` | web/dist と store を COOP/COEP 付きで配信 |
| `web/` | Vite+ (`vp`) のフロントエンド (TypeScript) |
| `tests/` | Playwright のヘッドレス起動テスト (`boot.mjs`)、V8 プロファイラ (`profile.mjs`)、JIT が出した TB の逆アセンブル (`tbdump.mjs`) |

## ビルド

必要なもの: emsdk 3.1.50、Rust、Node 22 + [Vite+](https://viteplus.dev)、debootstrap (ゲストイメージ用)。

```sh
export EMSDK=~/emsdk WORK=~/work
build/deps.sh                                   # zlib, libffi, glib, pixman を emscripten でビルド
build/fetch-qemu.sh                             # third_party/qemu-wasm に fork + パッチ
QEMU_SRC=third_party/qemu-wasm build/qemu.sh    # web/public/qemu/ に qemu-system-x86_64.{js,wasm,worker.js}

cargo build --release                           # vnvm-pack, vnvm-server
(cd web && vp install && vp build)              # web/dist
```

ゲストイメージ (root 権限で):

```sh
image/build-rootfs.sh ~/img                     # Ubuntu 26.04 + Xorg
image/install-wine.sh ~/img/rootfs /opt/wine-vnbox   # vnbox の Wine と prefix
image/make-disk.sh ~/img/rootfs ~/img/root.img  # ext4 (ジャーナルなし)
target/release/vnvm-pack --store store disk ~/img/root.img root
target/release/vnvm-pack --store store blank save                     # 256MiB の空ディスク (セーブ用)
target/release/vnvm-pack --store store game --exe game.exe /path/to/game naru2   # squashfs + .vnvm/launch.env
cp ~/img/rootfs/boot/vmlinuz-* store/boot/vmlinuz
cp image/kernel-cmdline.txt image/qemu-args.txt store/boot/
```

起動:

```sh
target/release/vnvm-server --web web/dist --store store --listen 127.0.0.1:8080
# http://127.0.0.1:8080/?game=naru2
#   &cmdline=init=/usr/local/bin/vnvm-bench   ベンチマーク (systemd なし)
#   &jit=64,100000                            JIT しきい値 (-accel tcg,jit-threshold=,jit-max=)
#   &snapshot=<name>                           store/snapshots/<name> から再開
```

ヘッドレス確認:

```sh
PLAYWRIGHT=$(npm root -g)/playwright/index.js node tests/boot.mjs "http://127.0.0.1:8080/?autostart" out 600 30
node tests/profile.mjs "http://127.0.0.1:8080/?autostart&cmdline=..." 150,420 60 prof.cpuprofile
```

## JIT の設計 (patches/ 内の tcg/wasm32)

ktock/qemu-wasm は TB を 1 つの wasm 関数にし、Asyncify で vCPU スレッドを unwind できる設計
でした。vnvm ではそれを捨てています:

* vCPU スレッドは決して unwind しない。コルーチン (emscripten fiber = Asyncify) を vCPU
  スレッドで切り替えないように virtio-blk の kick はメインループに委譲し、
  `qemu_coroutine_switch` は vCPU スレッドで呼ばれたら警告を出します。
  そのおかげで x86 ヘルパー/TCG/softfloat は Asyncify 計装なしでビルドできます
  (`build/asyncify-remove.sh`)。
* TCG レジスタは wasm のローカル変数、TB は `loop` + 直線コード、ラベルは `if (bp <= n)` ブロック。
* TLB の fast path はインライン、slow path は `else` でヘルパー呼び出し。
* TB 間は `return_call_indirect` で tail call 連鎖 (TB → instance_info → 関数テーブル)。
* 定数オペランドは `i64.const`、`local.set; local.get` は `local.tee` に畳む。
* ヘルパー呼び出しは共有関数テーブル経由の `call_indirect` (C の関数ポインタ = テーブル添字)
  なので TB コードはモジュールに依存せず、しきい値に達した TB はキューに溜めてまとめて
  1 モジュールにします (バッチ JIT)。V8 はモジュールごとに機械語を別領域に置くため、
  TB ごとに 1 モジュールだと連鎖のたびに命令キャッシュ/TLB ミスが起きていました
  (1000 モジュール間のホップ ≈340ns、同一モジュール内 ≈5ns)。
* `-accel tcg,jit-threshold=N,jit-max=M,jit-batch=B`: N 回実行された TB を wasm 化
  (それまでは TCI)、同時に生かしておく TB 数の上限 M、1 モジュールにまとめる TB 数 B。
  既定は 64 / 100000 / 64。

## 現状 (2026-10-27)

ゲストの処理時間。ネイティブは同じ QEMU の x86_64 ホスト TCG (JIT) 1 vCPU。

| 処理 | ネイティブ TCG | ブラウザ (Chromium headless) | バッチ JIT 前 |
| --- | ---: | ---: | ---: |
| ブート → init 実行 | 約 8s | 41s | 69s |
| md5sum 32MiB | 0.72s | 6.3s | 16.4s |
| sh の while ループ 30 万回 | 7.7s | 71s | 150s |
| dd 256MiB (zero→null) | 0.18s | 1.7s | 3.8s |
| gzip -6 /usr/bin/bash | 0.44s | 3.1s | 4.0s |
| ls -lR /usr/lib/x86_64-linux-gnu | 0.61s | 14s | 17s |
| fork+exec 200 回 | 2.1s | 21s | 40s |
| `wine cmd /c exit` 初回 / 2 回目 | 14.4s / 2.1s | 194s / 30s | 380s / 290s |

プロファイル (Wine 起動中の vCPU ワーカー): TB コード 60–65%、TB 検索 (qht、
`helper_lookup_tb_ptr`) 11%、`cpu_tb_exec` 4–8%。残る大きな差は V8 の tier-up
(関数ごとの実行バイト数予算なので小さな TB 関数はなかなか TurboFan に上がらない;
`--js-flags=--no-liftoff` だと Wine 2 回目が 227s → 34s だった) で、バッチの TB を
1 関数にまとめてグループ内の連鎖を `br` にする変更を進めています。

できていないこと: スナップショットからの再開、ゲームの起動確認、表示/入力/音声の実機確認、
ディスクの先読みと OPFS キャッシュ、セーブディスクの永続化、iOS/Android での検証。

## 制約

* SharedArrayBuffer が要るので COOP/COEP ヘッダ付き (cross-origin isolated) で配信すること。
* wasm32 なのでゲスト RAM + JIT キャッシュで 4GB まで。既定は 2300MB の固定ヒープ。
* DRM (PlayDRM、ソフト電池など) の解析や回避は扱いません。
