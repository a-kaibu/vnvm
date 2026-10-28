# hangover/

Wine's unix side as wasm32 (see [docs/hangover.md](../docs/hangover.md)).

| file | |
| --- | --- |
| `build.sh` | configures and builds Wine's ntdll unix side and wineserver with emscripten, builds `crates/vnhost` for `wasm32-unknown-emscripten` and links `wine.js` (node) |
| `main.c` | entry point: wineserver thread + the first Wine process |
| `pre.js` | passes node's environment to the program; `VNVM_UNIX_MODULE` defaults to `wineunix.so` next to `wine.js` |
| `vnfs.js` | `vnvm_fetch()`: reads trees and chunks of the store for the browser file system (`crates/vnhost/src/vfs.rs`) |
| `wrap-imports.py` | renames a side module's imports to `__wrap_*` (wasm-ld's `--wrap` crashes on side modules) |
| `make-wineroot.sh` | builds the i386 programs vnbox's WoW64 build lacks (wineboot, services, …) with mingw and assembles the Wine root |
| `make-prefix32.py` | turns a WoW64 prefix made by vnbox's Wine into a 32-bit prefix |

Wine's unix side is `wineunix.so`, an emscripten SIDE_MODULE; every Wine process dlopens its
own instance of it (own statics, shared memory/wineserver/fds). `VNVM_TRACE_PROC=1` traces
process creation, exits and thread starts.

Wine source: vnbox's Wine 11.20 tree with `patches/wine-wasm.patch` applied. The PE side is
vnbox's `lib/wine/i386-windows` as is.

```
EMSDK4=~/emsdk4 WINE_SRC=~/wine-src WINE_TOOLS=<native wine build tree> hangover/build.sh
# the Wine root (hangover/make-wineroot.sh) must hold lib/wine/i386-windows, lib/wine/i386-unix/ntdll.so (empty file) and share/wine
WINEPREFIX=~/prefix32 node wine.js 'C:\windows\system32\cmd.exe' /c echo hello
```

### From the store (as in the browser)

The Wine root and the prefix can come from a chunk store instead of the host's files: pack them
with `vnvm-pack tree` and mount them with `VNVM_MOUNTS` (`<path>=<tree>`, `:`-separated). Files
are fetched chunk by chunk when read; writes stay in memory.

```
vnvm-pack --store store tree --follow ~/wineroot wineroot   # 663MB of Wine root + prefix -> 108MB
vnvm-pack --store store tree ~/prefix32 prefix32
VNVM_MOUNTS=/opt/wine-vnbox=wineroot:/vnvm/prefix=prefix32 VNVM_STORE=store WINEPREFIX=/vnvm/prefix \
  node wine.js 'C:\windows\system32\cmd.exe' /c echo hello
```

`VNVM_INPUT="<s>:<kind>,<x>,<y>,<data>;..."` sends input records (crates/vnhost/src/display.rs)
at those times and `VNVM_SCREENSHOT=<file.ppm>` saves the screen, for testing without a page.

### In the browser

build.sh also links an ES module build into `web/public/wine/` (with `wineunix.so`);
`web/wine.html?autostart&run=C:\wintest.exe&args=600000` starts it with the store served by
vnvm-server at `/store`. `tests/wine.mjs` drives it in headless Chromium (screens are read
from the wasm heap there, since headless Chromium can't present WebGPU canvases).

`VNVM_CHUNK_CACHE_MB` (256) bounds the decompressed chunks kept; `VNVM_TRACE_VFS=1` traces paths
and fetches.

Needs emscripten 4.x (Rust's LLVM emits features 3.1.x can't handle) and a Rust toolchain with
`rust-src` (std is rebuilt with atomics: `RUSTC_BOOTSTRAP=1 -Zbuild-std`).
