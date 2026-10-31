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

### 実装 (2026-10-30)

- crates/vnx86/src/jit: 熱いアドレス (2 回目のミス) から直接分岐をたどって最大 1500 命令の
  **リージョン** を 1 つの wasm 関数にする。ブロック間は br_table、自分の先頭へのループは
  wasm の loop。レジスタと遅延フラグ (crates/vnx86/src/lazy.rs) はリージョン内でローカルの
  まま。ret・間接ジャンプはハッシュ表を引き、自分のブロックなら関数内で続行、他リージョン
  なら `return_call_indirect` で直接移る (ディスパッチャに戻らない)。
- JIT 化している命令: 整数演算一式・lea・push/pop・call/ret/jcc/jmp・setcc/cmov・
  imul/div/idiv・x87 のロード/ストア/四則/fxch/fild・SSE の 128bit mov と論理演算 (wasm SIMD)。
  それ以外は `step` (インタプリタ 1 命令) をブロックの途中で呼ぶ。
- ホスト側 (crates/vnhost/src/jit.rs, hangover/vnjit.js): スレッドごとの funcref テーブルと
  ハッシュ表、ディスパッチャモジュール。`VNVM_JIT=0` でインタプリタのみ、`VNVM_JIT_STATS=1`
  でインタプリタに回った命令・ミス・実行ループの出入りを集計。
- リージョンを大きくしすぎると V8 の最適化コンパイルが超線形に重くなり、起動後もコンパイラ
  スレッドが 3 コア以上使い続けた (1500 命令で上限)。
- ついでに直した不具合: 新しいプロセスが KUSER_SHARED_DATA を消していた (SSE2 などの
  ProcessorFeatures が 0 になり msvcrt が SSE2 memmove を使わなかった)、Sleep() が即座に
  戻っていた (emscripten の select(0) が待たない)。

bench.exe (hangover/tests/bench.c、ms、チェックサムは全て一致):

| | crc32 | lz | blend | memcpy | sort | float | calls | 合計 |
|---|---|---|---|---|---|---|---|---|
| ネイティブ (vnbox の Wine) | 53 | 30 | 6 | 155 | 123 | 7 | 39 | 413 |
| インタプリタ (node) | 20000 | 44000 | | 474000 | | | | 696000 |
| JIT (node) | 90 | 170 | 13 | 180 | 830 | 75 | 150 | 1500 |
| JIT (Chromium) | 113 | 203 | 22 | 201 | 910 | 100 | 178 | 1726 |

残りの差は主にリージョンをまたぐ呼び出し (sort は qsort → ラッパ → 比較関数の往復) と、
生成コードの質 (メモリアクセスごとの範囲チェック、ローカルの多さ)。

### 実ゲームでの確認 (2026-10-30)

Reunion 体験版 (`Z:\home\user\trials\reunion\game\reunion.exe`) を node で動かし、
自動クリックで本編を 8 分ほど進められることを確認 (ボイス付きの行も止まらない)。

- lock 付き命令 (lock inc/xadd/cmpxchg/cmpxchg8b など) と xchg のメモリ版は JIT せず
  インタプリタに回し、ストアを compare-and-swap にして他スレッドに書き換えられていたら
  やり直す (`Memory::cmpxchg`、vnhost は AtomicU8..U64)。これが無いとクリティカル
  セクションが壊れ (`RtlpWaitForCriticalSection ... blocked by 0000`)、ゲームが固まっていた。
  検証: hangover/tests/atomics.c (4 スレッド × 20 万回)。
- 音声: winebrowser.drv が mmdevapi の Unix インターフェースも持つ (`dlls/winebrowser.drv/audio.c`、
  OSS ドライバを元にした)。プレフィックスに `HKCU\Software\Wine\Drivers\Audio=browser`。
  フレームは vnhost (crates/vnhost/src/audio.rs) に渡り、壁時計で再生済みにしながら 48kHz
  ステレオ f32 に混ぜる。ページは `vnhost_audio_take` で取り出して AudioWorklet で鳴らす
  (web/src/wine-audio.ts)。node では捨てるだけだが、時間の進み方は同じ。音声デバイスが
  無いと、ボイスの再生終了を待つ行で文字送りが止まっていた。
- 表示ドライバの初期化は Unix 呼び出し 37 番 (mmdevapi の関数表の後ろ) を PE 側の DllMain
  から呼ぶ。`__wine_unix_call_funcs` があると ntdll は `__wine_unix_lib_init` を呼ばないため。
- 調べ方: `VNVM_SCREENSHOT=f.ppm` で画面、`VNVM_INPUT="秒:種類,x,y,データ;..."` で入力
  (秒は最初のウィンドウから)、`VNVM_INPUT_TRACE=1` でパイプの未読バイト数とその位置の
  ウィンドウ、`VNVM_TRACE_EIP=秒` でスレッドごとの eip と ebp チェーン。LANG=ja_JP.UTF-8 が
  無いと Shift-JIS の文字が化ける。同じプレフィックスで 2 つ同時に動かすとレジストリが壊れる。

### 起動の短縮 (2026-10-30)

ページを開くたびに Wine を最初から起動する (wineboot → services → winedevice …) ので、
プロセスやスレッドを作るコストがそのまま待ち時間になる。`cmd /c echo` で 15 秒 → 8.5 秒、
Reunion の起動からタイトル画面まで 53 秒 → 34 秒。

- wineserver の待ち (crates/vnhost/src/fd.rs): 全員で 1 つの条件変数を待っていたのを、
  ソケットごとの待ち手に直接起こすようにし、起きる前に 50µs だけ回って待つ。サーバー呼び出し
  1 回 449µs → 42µs (`srv.exe`)。
- JIT の翻訳を全スレッド・全プロセスで共有 (crates/vnhost/src/jit.rs `SHARED`): 同じ起点の
  リージョンを元のゲストコードのバイト列と一緒に覚え、別のスレッドでバイト列が同じなら
  翻訳せずに読み込む (1 回目のミスで即)。DLL は同じアドレスに同じ内容で載るので効く。
  `VNVM_JIT_THRESHOLD` でコンパイルまでのミス回数を変えられる (既定 2)。
- プレフィックス (hangover/make-prefix32.py): Eventlog (svchost)・NDIS・nsiproxy を手動起動に
  し、USB/Bluetooth/HID バスのルートデバイス (それだけで winedevice が 1 つ増える) を入れない。
  ブラウザにはどれも裏側が無い。
- ブラウザ: ストアのチャンクは 1 つずつ同期 XHR で読む。vnvm-server が TCP_NODELAY を付けて
  いなかったため、使い回しの接続で毎回相手の遅延 ACK (~40ms) を待っていた (1 回 37ms → 5.6ms、
  起動中の取得待ちの合計 9.3 秒 → 2.3 秒)。ワーカーでは arraybuffer で受け取る。
  ブラウザの `cmd /c echo` 16 秒 → 12 秒、Reunion のタイトルまで約 50 秒 → 38 秒。
  `wine.html?env=NAME=value` で環境変数を渡せる (VNVM_TRACE_PROC など)。
- Unix 側モジュールを自分のプロセスのスレッドだけで読む (hangover/vndl.js): Emscripten は
  新しいスレッドごとに全プロセス分の wineunix.so (2.8MB) をインスタンス化していた (起動 CPU の
  約 4 割)。他プロセスのモジュールは関数テーブルの枠だけ確保し、そのプロセスに入ったとき
  (shim.c の `vnvm_dl_enter`) に読む。どのスレッドでもテーブルが同じ番号で伸びる必要があるので、
  モジュールの export には GOT を作らず、wineunix.so は `-Wl,-Bsymbolic` でリンクして GOT.func
  を無くした (build.sh が確認する)。これまでは FreeType の関数ポインタが GOT 経由で「そのスレッドが
  最初に読んだプロセス」のインスタンスを指していた。ワーカーは再利用されるので、同期の再実行は
  一度目の結果を返す。node の cmd で CPU 時間 40 秒 → 23 秒。
- pthread のワーカーを 16 個先に用意 (ブラウザで 1 秒ほど短縮)。
- JIT のコンパイルまでのミス回数を 2 → 50 (一度きりの初期化コードまで翻訳・インスタンス化
  していた。ゲームのメインスレッドではモジュールの Instance 作成だけで 3 秒以上)。リージョンの
  funcref テーブルは 1 つずつでなく 1.5 倍ずつ伸ばす (伸ばすたびにコピーされる)。
- x87: ブロック内では ST レジスタを f64 のローカルに置き (FCache)、ブロックを出るとき・
  インタプリタを呼ぶときだけ Cpu に書き戻す (TOP、empty/exact ビットもまとめて)。
  bench の float 56ms → 28ms。
- MMX: mm0〜7 もブロック内では v128 のローカルに置く (上位 64 ビットは常に 0、書き戻しは
  x87 と同じ場所)。Reunion のメインスレッドで一番重い MMX の合成ループ (0x654640) が対象。
  JIT のリージョンには `name` セクションで `x86_<eip>` と名前を付ける (--cpu-prof で読める)。
- MMX の packed 命令では x87/MMX のキャッシュを書き戻さない (以前は命令ごとに全 mm を Cpu へ
  書いていた)。上位 64 ビットが 0 のままの演算ではクリアも省く。Reunion の合成ループ 3.2s → 1.9s
  (27 秒のプロファイル中)。
- 間接 call のインライン先: 合流したルートのうち、最後のルートへ末尾ジャンプするラッパー
  (msvcrt qsort の compare_wrapper) を優先する。比較関数を直接インラインしてもガードに外れ、
  毎回ハッシュ表を引いていた。bench sort (単独 5 回) 510 → 430ms。
- 測り方: `perf record -e cpu-clock` + `node --perf-basic-prof` で関数単位、`--print-wasm-code` の
  出力から目的のリージョンの機械語だけ抜けば命令単位まで見られる。sort のホットリージョンは
  サンプルの 4 割強が TurboFan のスピル/リロードとレジスタ間移動 (引数 16 個 + 遅延フラグ 5 個で
  レジスタが足りない)。bench を通しで 1 回だけ回すと、大きいリージョンの TurboFan 待ち (その間は
  Liftoff) の分ぶれる。
- タイトルまでの内訳 (node 約 20 秒): Wine 起動 ~4 秒 → ゲーム開始、最初の user32 呼び出しで
  explorer のデスクトップ初期化を ~1.2 秒待つ、KiriKiri 自身の初期化 (ゲスト CPU) でウィンドウが
  12.6 秒、その後ロゴとタイトルのフェード演出 (ゲームのタイマー) が ~7 秒。wineboot から explorer
  を先に起動しても起動全体は縮まなかった (CPU を取り合うだけ)。
- SSE の浮動小数点 (ps/pd/ss/sd の四則・sqrt・min/max・cmpps・shufps・movss) も v128 で JIT
  する。sqrt の NaN はインタプリタも x86 と同じ (負の入力は符号付きの既定 NaN、NaN はそのまま)。
- 起動中の CPU の 4 割強は V8 の TurboFan がリージョンをコンパイルする時間だった (node、最初の
  20 秒)。リージョン同士の合流は、同じジャンプがリングの集計で 8 回連続 1 位になってから
  (起動時に一瞬だけ熱いジャンプで作り直していた)。ESP はブロックの先頭で常にスタックを指すと
  みなし、範囲チェックは `mov esp, …`/`leave`/`sub esp, reg` などの直後に 1 回だけ (チェックの
  41% が ESP、EBP が 26%)。最初の 75 秒のリージョンの TurboFan 時間 26.6 → 14.2 秒、タイトルまでの
  CPU 時間 38.4 → 35.9 秒。`VNVM_JIT_LAYOUT=1` で命令ごとの wasm バイト数が出る (mov 1 命令
  約 30 バイト)。
- リージョンには「すでに実行されたブロック」だけを入れる。インタプリタは分岐ごとに止まって JIT を
  引くので、コンパイル前に走ったブロックは必ずミスの記録に残る。静的に届くブロックのうち実際に
  走るのは 1/3 程度で、残りも V8 が毎回コンパイルしていた。走っていない分岐先はリージョンの出口。
  最初の 75 秒のリージョンの TurboFan 14.2 → 5.1 秒、タイトルまでの CPU 35.9 → 約 30 秒、起動から
  40 秒までの CPU 58 → 51 秒。本編再生中 (40〜90 秒) の CPU は約 31 秒で変わらず。bench の lz は
  63 → 約 85ms に落ちた (ホットループは 1 つのリージョンに収まっていて、原因は未特定)。
  あわせて既存のバグを修正: RET の戻り先キャッシュ (L_RA1) がディスパッチ経由で飛ぶとき x87/MMX
  キャッシュを書き戻していなかった (呼び出し先の ST0 の結果が消え、プラグインでゼロ除算)。
  側出口の先が温まったら元のリージョンを作り直す方式も試したが、作り直しが多すぎて逆効果
  (TurboFan 26 秒)。
- インタプリタに落ちていた命令を JIT へ (`VNVM_JIT_STATS=1` で多い順。lock/rep 付きは別に数える):
  fwait・fldcw/fnstcw・fist/fistp (RC に従う丸め、範囲外は整数不定値、FILD m64 の正確な整数は
  そのまま)・emms・cld/std・movntps 系、movs/stos/lods (rep の movs は memory.copy、rep stosb は
  memory.fill。DF=1・前方への重なり・ECX が大きいときはインタプリタ)、1 オペランドの mul/imul、
  lock 付き add/sub/and/or/xor/inc/dec/xadd/cmpxchg と xchg (4 バイト境界に揃った dword は
  wasm atomics、揃っていなければインタプリタ)。Reunion の本編 90 秒でインタプリタ実行は
  1200 万命令超 → 400 万未満。タイトルまでの CPU 約 30 → 27.8 秒、本編再生の CPU 約 30 → 27.3 秒。
  テストは hangover/tests/intops.c (JIT とインタプリタで同じチェックサム)。
- node で測るときの注意: node 22 (V8 12.4) は table を import するモジュールのインスタンス化が
  table の長さに比例する (5 万要素で 2.2ms/回)。Chromium 153 は長さに関係なく 0.03ms。node の
  プロファイルではゲームのスレッドで約 1 秒これに使っていたが、ブラウザでは起きない。
- 現在: cmd /c echo は node 6〜7 秒・ブラウザ 9 秒。Reunion のタイトル画面が揃うまで node 約 16.5 秒・
  Chromium 約 16 秒 (クリックで本編も進む)。ゲームのスレッドの内訳 (node、最初の 20 秒): x86 の
  リージョン 42% (最大は MMX のクロスフェードのループ 0x654640 で全体の 7%)、Wine の unix 側 wasm
  20%、memmove 5%
  (ゲーム側のタイマー待ちで 0.5 秒刻みに揃うので、細かい比較は CPU 時間で見る)。

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
- Unix ライブラリ (win32u, ws2_32, dnsapi, nsiproxy, mountmgr, winebus など 13 本) を
  同じ side module に入れ、wineserver の共有メモリは 0x7c000000 以降の共有窓で全プロセスが
  同じアドレスを見る。
- 表示ドライバ winebrowser.drv: トップレベルウィンドウの画面を vnhost (display.rs) が
  z 順に合成し、ページは `vnhost_display_frame` を WebGPU で表示する。入力は 16 バイトの
  レコードをパイプでデスクトップスレッドのキューに流す。
- ブラウザ用ファイルシステム (crates/vnhost/src/vfs.rs): `vnvm-pack tree` で Wine 本体と
  プレフィックスを圧縮・重複除去したチャンクにし (663MB → 108MB)、読んだところだけ
  チャンク単位で取得する。書き込みはメモリ上。
- ブラウザ: web/wine.html が `public/wine/wine.js` (ES モジュール版) を読み、ストアから
  Wine を起動する。ヘッドレス Chromium でテストウィンドウの描画・クリック・キー入力を確認
  (tests/wine.mjs。ヘッドレスでは WebGPU の canvas が使えないので wasm ヒープから画面を読む)。
- 残り: 起動時間 (wineboot 込みで 20 秒前後)、JIT の詰め (上の表)、書き込み可能な
  MAP_SHARED のプロセス間共有 (今はコピー)、モバイル (メモリ 2GB 超を前提にしている)。

## 守ること

- DRM (PlayDRM、ソフト電池など) の解析・回避はしない。
- 新しく書くコードは Rust。Wine 本体は C のまま。
