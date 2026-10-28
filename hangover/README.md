# hangover/

Wine's unix side as wasm32 (see [docs/hangover.md](../docs/hangover.md)).

| file | |
| --- | --- |
| `build.sh` | configures and builds Wine's ntdll unix side and wineserver with emscripten, builds `crates/vnhost` for `wasm32-unknown-emscripten` and links `wine.js` (node) |
| `main.c` | entry point: wineserver thread + one Wine process |
| `pre.js` | passes node's environment to the program |
| `make-prefix32.py` | turns a WoW64 prefix made by vnbox's Wine into a 32-bit prefix |

Wine source: vnbox's Wine 11.20 tree with `patches/wine-wasm.patch` applied. The PE side is
vnbox's `lib/wine/i386-windows` as is.

```
EMSDK4=~/emsdk4 WINE_SRC=~/wine-src WINE_TOOLS=<native wine build tree> hangover/build.sh
# /opt/wine-vnbox must hold lib/wine/i386-windows, lib/wine/i386-unix/ntdll.so (empty file) and share/wine
WINEPREFIX=~/prefix32 node wine.js 'C:\windows\system32\cmd.exe' /c echo hello
```

Needs emscripten 4.x (Rust's LLVM emits features 3.1.x can't handle) and a Rust toolchain with
`rust-src` (std is rebuilt with atomics: `RUSTC_BOOTSTRAP=1 -Zbuild-std`).
