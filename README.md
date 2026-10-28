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

## 方針の切り替え (2026-10-28)

QEMU でマシン全体をエミュレートする方式は性能の上限が低いため、今後は Hangover 方式
(Wine 自体を wasm 向けにビルドし、ゲームと i386 の PE DLL だけを x86→wasm JIT で動かす) に移ります。
設計は [docs/hangover.md](docs/hangover.md)、x86 コアは `crates/vnx86` です。
QEMU 版はこのまま互換用に残します。

## ディレクトリ

| パス | 内容 |
| --- | --- |
| `build/` | emsdk 用の依存ライブラリ (`deps.sh`)、QEMU fork の取得 (`fetch-qemu.sh`)、wasm ビルド (`qemu.sh`)、スナップショット用ネイティブビルド (`qemu-native.sh`) |
| `crates/vnx86` | Hangover 方式の i386 CPU コア (インタプリタ、ネイティブ CPU との差分テスト付き) |
| `docs/hangover.md` | Hangover 方式の設計 |
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
#   &jit=64,100000,256                        JIT しきい値 (-accel tcg,jit-threshold=,jit-max=,jit-batch=)
#   &ic=0                                      goto_ptr インラインキャッシュを無効化 (A/B 用)
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
* ヘルパー呼び出しは共有関数テーブル経由の `call_indirect` (C の関数ポインタ = テーブル添字)
  なので TB コードはモジュールに依存せず、しきい値に達した TB はキューに溜めてまとめて
  1 モジュールにします (バッチ JIT)。V8 はモジュールごとに機械語を別領域に置くため、
  TB ごとに 1 モジュールだと連鎖のたびに命令キャッシュ/TLB ミスが起きていました
  (1000 モジュール間のホップ ≈340ns、同一モジュール内 ≈5ns)。
* グループ JIT: バッチの TB 全部を 1 つの wasm 関数にします (`loop` + 入れ子 `block` +
  `br_table` で TB 番号へ分岐)。V8 の tier-up 予算は関数単位なので、小さな TB が Liftoff の
  まま残る問題が消えます。グループ内の TB への連鎖は `br` 1 命令 (goto_tb は flush 時に
  直接 `br` にパッチ、goto_ptr はサイトごとの (関数, TB 番号) キャッシュ)、他グループへは
  `return_call_indirect`。
* 間接ジャンプ (`helper_lookup_tb_ptr`) は TB ヘッダ内のインラインキャッシュ (eip, CS base,
  flags, TLB 世代, 対象 TB の cflags を比較) で解決し、ミス時だけ C に落ちます。その下の
  TB ジャンプキャッシュは 64K エントリ 2-way で、TLB フラッシュでは消さず世代番号で遅延検証
  します (Wine はコンテキストスイッチのたびに CR3 を書くため、消すと毎回ミスしていました)。
* 定数オペランドは `i64.const`、`local.set; local.get` は `local.tee` に畳む。
* `-accel tcg,jit-threshold=N,jit-max=M,jit-batch=B,jit-ic=I`: N 回実行された TB を wasm 化
  (それまでは TCI)、同時に生かしておく TB 数の上限 M、1 関数にまとめる TB 数 B (1–1024)、
  インラインキャッシュ I (0 無効 / 1 有効 / 2 A/B 用にヒットしない)。既定は 64 / 100000 / 256 / 1。
  翻訳キャッシュは `tb-size=768` (MB)、wasm ヒープは 2600MB (`build/qemu.sh` の
  `QEMU_WASM_MEMORY`)。URL では `&jit=N,M,B`、`&ic=I`、`&tbsize=MB`、`&mem=MB`。
* ディスクのチャンク取得 (同期 XHR) は QEMU のスレッドプールで行い、メインループと BQL を
  塞ぎません。

## 現状 (2026-10-28)

ゲストの処理時間。ネイティブは同じ QEMU の x86_64 ホスト TCG (JIT) 1 vCPU。
計測ホストが変わったので全列を同じホスト・同じ日に取り直しています
(ホストが変わると絶対値は 1.5 倍ほど動くので、比較は必ず同一ホストで)。

| 処理 | ネイティブ TCG | ブラウザ (Chromium headless) | インラインキャッシュ前 |
| --- | ---: | ---: | ---: |
| ブート → init 実行 | 約 8s | 36s | 38s |
| md5sum 32MiB | 0.81s | 4.2s | 4.9s |
| sh の while ループ 30 万回 | 10.0s | 40.6s | 50.0s |
| dd 256MiB (zero→null) | 0.20s | 1.2s | 1.3s |
| gzip -6 /usr/bin/bash | 0.54s | 1.9s | 2.0s |
| ls -lR /usr/lib/x86_64-linux-gnu | 2.0s | 5.2s | 5.0s |
| fork+exec 200 回 | 2.0s | 13.8s | 13.2s |
| `wine cmd /c exit` 初回 / 2 回目 | 38s / 2.2s | 96–103s / 14–16s | 96s / 15s |

バッチ JIT 前 (2026-10-27、別ホスト) は md5 16.4s、ループ 150s、Wine 380s / 290s でした。

プロファイル (sh ループの vCPU ワーカー): TB コード 75%、TB 検索 (インラインキャッシュのミス →
`tb_lookup`) 5%、SSE ヘルパー (`helper_gvec_eq8`、`pslldq`、`pcmpistri`) 5%、
`helper_ld_i128` + `mmu_lookup` 5%、futex の待ち/起床 4.5%。Wine 起動中は TB 検索が多く
(`ret` のような多態サイトはインラインキャッシュに乗らない)、TLB フィルと longjmp も見えます。
残りの差は主に TB コードそのものの質 (ゲスト命令あたりの wasm 命令数、eflags 計算、
64bit 演算) です。

できていないこと: スナップショットからの再開、ゲームの起動確認、表示/入力/音声の実機確認、
ディスクの先読みと OPFS キャッシュ、セーブディスクの永続化、iOS/Android での検証。

## 制約

* SharedArrayBuffer が要るので COOP/COEP ヘッダ付き (cross-origin isolated) で配信すること。
* wasm32 なのでゲスト RAM + JIT キャッシュで 4GB まで。既定は 2300MB の固定ヒープ。
* DRM (PlayDRM、ソフト電池など) の解析や回避は扱いません。
