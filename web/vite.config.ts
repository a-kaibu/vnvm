import { defineConfig } from "vite-plus";

// Built into web/dist and served by vnvm-server next to the disk store. QEMU's
// own files (public/qemu/) are copied as-is by build/qemu.sh, Wine's (public/wine/) by
// hangover/build.sh.
export default defineConfig({
  base: "./",
  build: {
    outDir: "dist",
    emptyOutDir: true,
    target: "es2022",
    // index.html: the QEMU VM; wine.html: Wine built for wasm (hangover/, files in public/wine/)
    rollupOptions: { input: { main: "index.html", wine: "wine.html" } },
  },
  server: {
    headers: {
      "cross-origin-opener-policy": "same-origin",
      "cross-origin-embedder-policy": "require-corp",
    },
    proxy: { "/store": process.env.VNVM_SERVER ?? "http://127.0.0.1:8080" },
  },
  lint: { ignorePatterns: ["public/**"], options: { typeAware: true, typeCheck: true } },
  fmt: { printWidth: 110, ignorePatterns: ["public/**"] },
});
