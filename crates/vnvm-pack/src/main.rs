//! Turns disk images into what the browser streams: a manifest plus
//! content-addressed, brotli-compressed chunks.
//!
//! ```text
//! store/
//!   chunks/ab/ab12...      brotli-compressed chunk, named by the BLAKE3 of its raw bytes
//!   disks/<name>/manifest.json
//! ```
//!
//! Identical chunks (across disks too) are stored once, all-zero chunks not at
//! all. The QEMU side is block/vnvm.c + web/qemu/vnvm-disk.js.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use rayon::prelude::*;
use serde::Serialize;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Parser)]
#[command(about)]
struct Cli {
    /// Output store (shared by all disks so chunks dedupe across them).
    #[arg(long, default_value = "store")]
    store: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Pack a raw disk image.
    Disk {
        image: PathBuf,
        name: String,
        /// Chunk size in KiB.
        #[arg(long, default_value_t = 256)]
        chunk_kib: u64,
        /// Virtual size reported to the guest (default: the image's size).
        #[arg(long)]
        size: Option<u64>,
    },
    /// Make a game disk: an uncompressed squashfs of DIR (the guest reads it
    /// without spending emulated cycles on decompression; the network transfer
    /// is compressed per chunk instead) plus .vnvm/launch.env.
    Game {
        dir: PathBuf,
        name: String,
        /// Executable to start, relative to DIR.
        #[arg(long)]
        exe: String,
        #[arg(long, default_value_t = 800)]
        width: u32,
        #[arg(long, default_value_t = 600)]
        height: u32,
        #[arg(long, default_value = "")]
        args: String,
        /// Every game disk has the same virtual size, so one VM snapshot fits all.
        #[arg(long, default_value_t = 8 << 30)]
        size: u64,
        #[arg(long, default_value_t = 256)]
        chunk_kib: u64,
    },
    /// An all-zero disk (the save disk; the guest formats it on first use).
    Blank {
        name: String,
        #[arg(long, default_value_t = 256 << 20)]
        size: u64,
        #[arg(long, default_value_t = 256)]
        chunk_kib: u64,
    },
}

#[derive(Serialize)]
struct Manifest {
    size: u64,
    chunk_size: u64,
    /// Relative to the manifest.
    chunks_url: String,
    /// BLAKE3 (hex, 32 chars) per chunk, "" for all zero.
    chunks: Vec<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Disk { image, name, chunk_kib, size } => {
            pack(&cli.store, &image, &name, chunk_kib << 10, size)?;
        }
        Cmd::Game { dir, name, exe, width, height, args, size, chunk_kib } => {
            if !dir.join(&exe).is_file() {
                bail!("{} not found in {}", exe, dir.display());
            }
            let tmp = cli.store.join("tmp");
            fs::create_dir_all(&tmp)?;
            let meta = tmp.join(format!("{name}-meta"));
            let _ = fs::remove_dir_all(&meta);
            fs::create_dir_all(meta.join(".vnvm"))?;
            fs::write(
                meta.join(".vnvm/launch.env"),
                format!(
                    "EXE={}\nWIDTH={width}\nHEIGHT={height}\nARGS={}\n",
                    shell_quote(&exe),
                    shell_quote(&args)
                ),
            )?;
            let img = tmp.join(format!("{name}.squashfs"));
            let _ = fs::remove_file(&img);
            // Pseudo files add .vnvm/ without copying the game folder.
            let env = meta.join(".vnvm/launch.env");
            let st = Command::new("mksquashfs")
                .arg(&dir)
                .arg(&img)
                .args(["-noI", "-noD", "-noF", "-noX", "-no-xattrs", "-all-root", "-quiet", "-no-progress"])
                .args(["-p", "/.vnvm d 755 0 0"])
                .arg("-p")
                .arg(format!("/.vnvm/launch.env f 644 0 0 cat {}", env.display()))
                .status()
                .context("mksquashfs (squashfs-tools)")?;
            if !st.success() {
                bail!("mksquashfs failed");
            }
            pack(&cli.store, &img, &name, chunk_kib << 10, Some(size))?;
            fs::remove_file(&img)?;
            fs::remove_dir_all(&meta)?;
        }
        Cmd::Blank { name, size, chunk_kib } => {
            let chunk = chunk_kib << 10;
            write_manifest(
                &cli.store,
                &name,
                &Manifest {
                    size,
                    chunk_size: chunk,
                    chunks_url: "../../chunks/".into(),
                    chunks: vec![String::new(); size.div_ceil(chunk) as usize],
                },
            )?;
        }
    }
    Ok(())
}

/// `launch.env` is sourced by the guest's vnvm-session, so values are single-quoted.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn pack(store: &Path, image: &Path, name: &str, chunk: u64, size: Option<u64>) -> Result<()> {
    let f = File::open(image).with_context(|| image.display().to_string())?;
    let len = f.metadata()?.len();
    let size = size.unwrap_or(len);
    if size < len {
        bail!("--size {size} is smaller than the image ({len})");
    }
    let n = size.div_ceil(chunk);
    let chunks_dir = store.join("chunks");
    fs::create_dir_all(&chunks_dir)?;

    let hashes: Vec<String> = (0..n)
        .into_par_iter()
        .map(|i| -> Result<String> {
            let off = i * chunk;
            if off >= len {
                return Ok(String::new());
            }
            let mut buf = vec![0u8; chunk.min(len - off) as usize];
            f.read_exact_at(&mut buf, off)?;
            buf.resize(chunk as usize, 0);
            if buf.iter().all(|&b| b == 0) {
                return Ok(String::new());
            }
            let h = blake3::hash(&buf).to_hex()[..32].to_string();
            let dir = chunks_dir.join(&h[..2]);
            let path = dir.join(&h);
            if !path.exists() {
                fs::create_dir_all(&dir)?;
                let tmp = dir.join(format!(".{h}.tmp"));
                let mut out = File::create(&tmp)?;
                let params = brotli::enc::BrotliEncoderParams { quality: 9, lgwin: 22, ..Default::default() };
                brotli::BrotliCompress(&mut &buf[..], &mut out, &params)?;
                out.flush()?;
                fs::rename(&tmp, &path)?;
            }
            Ok(h)
        })
        .collect::<Result<_>>()?;

    let used = hashes.iter().filter(|h| !h.is_empty()).count();
    write_manifest(
        store,
        name,
        &Manifest { size, chunk_size: chunk, chunks_url: "../../chunks/".into(), chunks: hashes },
    )?;
    eprintln!(
        "{name}: {} virtual, {used} chunks of {} KiB with data ({} MiB raw)",
        human(size),
        chunk >> 10,
        (used as u64 * chunk) >> 20
    );
    Ok(())
}

fn write_manifest(store: &Path, name: &str, m: &Manifest) -> Result<()> {
    let dir = store.join("disks").join(name);
    fs::create_dir_all(&dir)?;
    serde_json::to_writer(File::create(dir.join("manifest.json"))?, m)?;
    Ok(())
}

fn human(n: u64) -> String {
    if n >= 1 << 30 {
        format!("{:.1} GiB", n as f64 / (1u64 << 30) as f64)
    } else {
        format!("{} MiB", n >> 20)
    }
}
