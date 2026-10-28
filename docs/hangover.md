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

## 守ること

- DRM (PlayDRM、ソフト電池など) の解析・回避はしない。
- 新しく書くコードは Rust。Wine 本体は C のまま。
