# Hangover 方式 (QEMU なし) の設計

2026-10-28 に方針転換した。フルシステム QEMU (vnvm 本体) はゲストカーネル・ソフトウェア TLB・
TCG 中間表現の膨張という天井があり、ネイティブ並みの速度に届かない。そこで Hangover
(ARM64 Linux 上で x86 Windows アプリを動かす Wine 派生) と同じ考え方で、

- **Wine 自体はブラウザでネイティブ (wasm) に動かす**
- **x86 のコードだけを x86→wasm JIT で動かす**

構成にする。Linux カーネル、Linux 版の Wine バイナリ、X11 はどれも動かさない。

## 全体像

```
 ブラウザのタブ (Worker 群, SharedArrayBuffer)
 ┌──────────────────────────── 1 本の wasm32 線形メモリ (最大 4GB) ───────────────────────────┐
 │ 0x00000000 ─ 0x7fffffff : ゲスト (Windows のユーザー空間 2GB と同じ)                        │
 │   game.exe, ゲームの DLL, vnbox の Wine の i386 PE DLL (ntdll/kernel32/user32/gdi32/…)     │
 │   → x86 JIT (crates/vnx86) が実行。ゲストのアドレス = wasm のアドレス (TLB なし)            │
 │ 0x80000000 ─            : ホスト (emscripten の静的データ・スタック・ヒープ)                │
 │   Wine の Unix 側 (ntdll.so, win32u.so, …) を wasm32 にビルドしたもの                      │
 │   ブラウザ用ドライバ (表示 = WebGPU/canvas, 音 = WebAudio, 入力)                             │
 └───────────────────────────────────────────────────────────────────────────────────────────┘
        ↑ wineserver は別インスタンス (別 Worker) で、要求はソケット互換層で送る
```

ポイント:

1. **ゲストとホストが同じ線形メモリを共有し、ゲストのポインタがそのままホストのポインタになる。**
   Wine の Unix 側は 32bit (i386 版 Wine の Unix 側と同じ ILP32) としてビルドするので、
   WoW64 の構造体変換は要らない。PE 側から Unix 側への呼び出しは引数のポインタを渡すだけになる。
   wasm32 の 8 バイト整数の構造体アラインは 8 で、MSVC (PE 側) と同じ。
2. **ホストはアドレスの上半分に置く。** emscripten の `GLOBAL_BASE` を 0x80000000 以上にして、
   ゲストの 2GB (0x400000 の exe、0x7b000000 台の Wine DLL など) とぶつからないようにする。
   メモリの少ない端末向けには、ゲスト 1GB・ホストを 0x40000000 から、という配置も選べるようにする
   (Wine の i386 DLL はすべて .reloc を持つので再配置できる)。
3. **PE 側は vnbox がビルドした Wine 11.20 の i386-windows DLL をそのまま使う。** 「vnbox と同じ Wine」は
   PE 側ではバイナリ単位で守る。Unix 側は同じソース (vnbox のパッチ適用済み 11.20) を wasm32 向けに
   ビルドする。Linux 依存の部分 (シグナル、mmap、プロセス起動、X11) だけをブラウザ向けに差し替える。
4. **システムコールはトラップで受ける。** i386 ntdll のスタブは `mov eax, 番号; mov edx, dispatcher;
   call edx; ret N` で、dispatcher のポインタは Unix 側が設定する。ここを JIT が認識する特別な
   アドレスにして、ゲストスタック上の引数をそのまま Unix 側の `NtXxx` に渡す (引数のバイト数は
   `dlls/ntdll/ntsyscalls.h`, `dlls/win32u/win32syscalls.h` にある)。`__wine_unix_call` も同様。
5. **Unix 側からゲストへのコールバック** (ウィンドウプロシージャ、APC、例外) は、JIT を入れ子で
   呼び出して戻ってくるまで実行する (`KiUserCallbackDispatcher` と同じ形)。
6. **スレッド** は Windows のスレッド 1 本 = emscripten の pthread 1 本 (Web Worker) とし、各スレッドが
   自分の x86 CPU 状態を持って JIT コードを実行する。

## x86 JIT (crates/vnx86, Rust)

- 対象は **i386 (32bit) のユーザーモードだけ**。VN の実行ファイルはほぼすべて 32bit。
  特権命令・ページング・セグメント記述子の大半は不要 (FS = TEB だけ扱う)。
- メモリアクセスは `i32.load offset=0 (guest_addr)` そのもの。境界チェックはブラウザの
  ガードページ任せで、ソフトウェア TLB は持たない。不正アクセスはゲストの例外
  (EXCEPTION_ACCESS_VIOLATION) に変換する。
- 実行は 2 段: まずインタプリタ (正しさの基準・コールドコード用)、熱いコードを wasm にコンパイル。
  - ゲストのレジスタは wasm のローカル、EFLAGS は遅延評価。
  - QEMU 版で分かった V8 の性質を前提に設計する。生成モジュールの数が増えるとモジュール間の
    ジャンプが 30〜450ns に悪化するので、熱いコードは少数の大きなモジュールにまとめる。
    Liftoff→TurboFan の階層化は既定設定が最良だった。
- SSE/SSE2 は wasm SIMD (v128) に直接対応付ける。x87 は f64 で近似 (VN では精度より速度)。
- テスト: ネイティブ (cargo test) とブラウザ (node) の両方で動かす。命令単位のテストベクタと、
  実機 (この x86-64 ホストで 32bit コードを実行) との差分テストを用意する。

## マイルストーン

1. **x86 コア (インタプリタ)**: crates/vnx86 にデコーダと i386 ユーザーモード命令の実行。
   ネイティブでテストし、実機との差分テストを通す。
2. **PE ローダと最小の Win32**: Wine の i386 ntdll.dll を読み込み、システムコールをトラップして
   ログに出す所まで (Unix 側はまだスタブ)。
3. **Wine の Unix 側を wasm32 でビルド**: ntdll.so を emscripten でビルドし、node 上 (NODERAWFS で
   本物の Wine プレフィックスを読む) で `wine cmd /c exit` 相当が完走する。
   wineserver は別インスタンスで動かし、ソケットと fd 受け渡しを互換層で提供する。
4. **画面・入力・音**: win32u + ブラウザ用ドライバ (GDI の描画結果を WebGPU で表示)、
   WebAudio、キーボード・マウス。ここで VN の体験版がタイトル画面まで動く。
5. **JIT**: 熱いコードの wasm 化、SIMD、フラグ最適化。ネイティブ (実機の Wine) との
   ベンチ比較を回して詰める。
6. **DirectX と動画**: d3d9/ddraw (WebGPU バックエンド)、ffmpeg (winedmo) の wasm 化。
7. **配信と保存**: Wine プレフィックスとゲームのストリーミング (vnvm の chunk store を流用)、
   OPFS キャッシュ、セーブデータの永続化。

QEMU 版 vnvm (Ubuntu + Wine をそのまま起動) は互換性の確認用として残す。

## 複数プロセス

Wine は 1 本のアプリでも複数のプロセスを使う (新しい wineserver で最初に `wineboot --init`、
GUI では `explorer.exe /desktop` がデスクトップウィンドウを持つ、services.exe など)。
プロセスごとに別の wasm メモリにすると、wineserver と共有するメモリ (USER_SHARED_DATA、
win32u のセッション共有メモリ) を別名で共有できない。そこで:

- **Wine の Unix 側 (ntdll.so + win32u.so + ドライバ) を 1 つの emscripten SIDE_MODULE にまとめ、
  プロセスごとに別名で dlopen してインスタンスを分ける。** 静的データとテーブルはインスタンスごとに
  別で、線形メモリは 1 本。共有メモリは本当に同じアドレスを指せる。
  (別名 dlopen で静的変数が独立し、pthread からも呼べることは試作で確認済み。)
- wineserver と vnhost (fd・mmap・シグナルの互換層) はメインモジュールに 1 つだけ置く。
- ゲストのアドレス空間はプロセスごとに区画を分ける。ゲーム本体は 0x400000 を含む低位の区画、
  Wine のヘルパープロセス (すべて .reloc 付き) はそれ以外の区画に再配置して載せる。
- `fork`/`exec` の代わりに vnhost がインスタンスを作って新しいスレッドで `__wine_main` を呼ぶ。
  `getpid` はスレッドが属するプロセスの番号を返し、プロセスの終了はそのプロセスのスレッドだけを止める。

## 現状 (2026-10-28)

- crates/vnx86: i386 ユーザーモードのインタプリタ。整数・x87・MMX・SSE・SSE2 を実装し、
  この x86-64 ホストの CPU との差分テスト (ランダム命令列) と、vnbox の i386 ntdll.dll の
  実関数 (sprintf, qsort など) の実行テストが通る。
- crates/vnhost: Wine の Unix 側を wasm で動かすための互換層 (Rust)。プロセス内の
  パイプ・UNIX ソケットと SCM_RIGHTS、ゲスト領域の mmap と共有ファイルマッピング、
  静的リンクした Unix ライブラリへの dlopen、シグナルのフラグ化、x86 実行ループ。
- patches/wine-wasm.patch: Wine 11.20 (vnbox のパッチ適用済み) への変更。i386 ABI のまま
  wasm32 でビルドし、`dlls/ntdll/unix/signal_wasm.c` がトラップ・コンテキスト・コールバック・
  例外を受け持つ。システムコールは型の合ったサンクを経由して呼ぶ (wasm の間接呼び出しは
  シグネチャが一致しないと落ちるため)。
- hangover/build.sh で node 用の `wine.js` を作り、vnbox のプレフィックスから作った 32bit
  プレフィックス (hangover/make-prefix32.py) で vnbox の `cmd.exe /c echo` が完走する。
- 複数プロセス対応済み: Unix 側は `wineunix.so` (SIDE_MODULE) 1 本で、プロセスごとに
  `/tmp/vnvm-<uid>/unix-*.so` へのシンボリックリンク経由で RTLD_LOCAL に dlopen する。
  プロセス番号は 100 から (ホストのスレッドは 42)。アプリは低位の 0x50000000 の区画、
  ヘルパーは上から 0x08000000 ずつの区画 (足りなければ半分ずつ、最小 32MB) を使う。
  環境変数・getpid・exit・kill・fd の所有はプロセス単位 (crates/vnhost/src/proc.rs)。
  `cmd.exe /c echo` を起動すると start.exe → `wineboot --init` (services.exe, svchost,
  winedevice) → cmd.exe まで完走する (約 26 秒、毎回 wineboot が走る)。
  vnbox は WoW64 ビルドで i386 版の wineboot などが無いので、hangover/make-wineroot.sh が
  同じソースから mingw で 10 本の i386 プログラムをビルドして足す。
- 残り: ドライバ (mountmgr, nsiproxy, winebus など) と win32u・dnsapi の Unix ライブラリ、
  書き込み可能な MAP_SHARED のプロセス間共有 (今はコピー)、起動時間 (sys 時間が多い)。

## 守ること

- DRM (PlayDRM、ソフト電池など) の解析・回避はしない。
- 新しく書くコードは Rust。Wine 本体は C のまま。
