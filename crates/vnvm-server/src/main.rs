//! Serves the vnvm web app and the disk store made by vnvm-pack.
//!
//! - `/` the built web app (web/dist)
//! - `/store/disks/<name>/manifest.json`
//! - `/store/chunks/ab/<hash>`: stored zstd-compressed, sent with
//!   `Content-Encoding: zstd` so the browser decodes them natively, and marked
//!   immutable so its HTTP cache keeps them.
//! - `?v=...` on any URL: kept by the browser for good (the page changes the version when
//!   the content changes)
//! - `/jit/<name>.wasm`, `/jit/<name>.idx`: the x86 regions runs of a game saved
//!   (vnx86::jit::saved); `PUT /jit-save/<name>` with (u32 module length, module, index)
//!   merges a run's new regions in; `PUT /jit-save/<name>.chunks` stores the store chunks a
//!   run read first (16-byte hashes), which the page fetches ahead next time.
//!
//! Every response carries COOP/COEP: QEMU's threads need SharedArrayBuffer,
//! which browsers only allow on cross-origin isolated pages.

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, State},
    http::{header, HeaderValue, Request, StatusCode},
    middleware::{self, Next},
    response::Response,
    routing::put,
    Router,
};
use clap::Parser;
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
use vnx86::jit::{saved, wasm};
use tower_http::services::ServeDir;

#[derive(Parser)]
struct Args {
    /// The built web app.
    #[arg(long, default_value = "web/dist")]
    web: PathBuf,
    /// The vnvm-pack store.
    #[arg(long, default_value = "store")]
    store: PathBuf,
    /// Saved x86 regions, per game.
    #[arg(long, default_value = "jit")]
    jit: PathBuf,
    #[arg(long, default_value = "127.0.0.1:8080")]
    listen: SocketAddr,
}

/// The jit directory; the lock keeps merges one at a time.
type Jit = Arc<(PathBuf, tokio::sync::Mutex<()>)>;

/// Merges a run's regions into `name`'s: those not saved already (same roots and code) go after
/// the others.
async fn save_jit(State(jit): State<Jit>, Path(name): Path<String>, body: Bytes) -> StatusCode {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.') {
        return StatusCode::BAD_REQUEST;
    }
    // the store chunks a run read first, as is (the page fetches them ahead)
    if name.ends_with(".chunks") {
        if body.len() % 16 != 0 {
            return StatusCode::BAD_REQUEST;
        }
        let _g = jit.1.lock().await;
        let _ = tokio::fs::create_dir_all(&jit.0).await;
        return match tokio::fs::write(jit.0.join(&name), &body).await {
            Ok(()) => StatusCode::NO_CONTENT,
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
    }
    let Some(n) = body.get(..4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize) else {
        return StatusCode::BAD_REQUEST;
    };
    let (Some(new_wasm), Some(new_idx)) = (body.get(4..4 + n), body.get(4 + n..).and_then(saved::decode)) else {
        return StatusCode::BAD_REQUEST;
    };
    // (a game's regions stop growing once its code paths have run; this only bounds a runaway)
    const MAX_BYTES: u64 = 256 << 20;
    let _g = jit.1.lock().await;
    let path = |ext: &str| jit.0.join(format!("{name}.{ext}"));
    let (wasm_path, idx_path) = (path("wasm"), path("idx"));
    if tokio::fs::metadata(&wasm_path).await.is_ok_and(|m| m.len() > MAX_BYTES) {
        return StatusCode::INSUFFICIENT_STORAGE;
    }
    let old_wasm = tokio::fs::read(&wasm_path).await.ok();
    let old_idx = tokio::fs::read(&idx_path).await.ok().and_then(|b| saved::decode(&b));
    let mut entries = old_idx.clone().unwrap_or_default();
    // the new module's functions to keep
    let keep: Vec<bool> = new_idx.iter().map(|e| !entries.contains(e)).collect();
    if !keep.iter().any(|&k| k) {
        return StatusCode::NO_CONTENT;
    }
    let all = keep.iter().all(|&k| k);
    let fresh = if all { Some(new_wasm.to_vec()) } else { wasm::select(new_wasm, &keep) };
    let Some(fresh) = fresh else { return StatusCode::BAD_REQUEST };
    entries.extend(new_idx.into_iter().zip(&keep).filter(|x| *x.1).map(|x| x.0));
    let names: Vec<String> = entries.iter().map(|e| e.name()).collect();
    let mods: Vec<&[u8]> = match (&old_wasm, &old_idx) {
        (Some(w), Some(_)) => vec![w, &fresh],
        _ => vec![&fresh],
    };
    let Some(merged) = wasm::merge(&mods, &names) else { return StatusCode::BAD_REQUEST };
    let write = async {
        tokio::fs::create_dir_all(&jit.0).await?;
        let (tw, ti) = (path("wasm.tmp"), path("idx.tmp"));
        tokio::fs::write(&tw, &merged).await?;
        tokio::fs::write(&ti, saved::encode(&entries)).await?;
        tokio::fs::rename(&tw, &wasm_path).await?;
        tokio::fs::rename(&ti, &idx_path).await
    };
    match write.await {
        Ok(()) => {
            eprintln!("jit {name}: {} regions, {} bytes", entries.len(), merged.len());
            StatusCode::NO_CONTENT
        }
        Err(e) => {
            eprintln!("jit {name}: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

async fn headers(req: Request<axum::body::Body>, next: Next) -> Response {
    let chunk = req.uri().path().starts_with("/store/chunks/");
    // a versioned URL (?v=...) names content that never changes: the browser keeps it without
    // asking again, and so keeps its compiled code (a revalidated .wasm loses it)
    let versioned = req.uri().query().is_some_and(|q| q.split('&').any(|p| p.starts_with("v=")));
    let mut res = next.run(req).await;
    let ok = res.status().is_success();
    let h = res.headers_mut();
    h.insert("cross-origin-opener-policy", HeaderValue::from_static("same-origin"));
    h.insert("cross-origin-embedder-policy", HeaderValue::from_static("require-corp"));
    h.insert("cross-origin-resource-policy", HeaderValue::from_static("same-origin"));
    if chunk && ok {
        h.insert(header::CONTENT_ENCODING, HeaderValue::from_static("zstd"));
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=31536000, immutable"));
    } else if versioned && ok {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=31536000, immutable"));
    } else {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    }
    res
}

use axum::serve::ListenerExt;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let jit: Jit = Arc::new((args.jit.clone(), tokio::sync::Mutex::new(())));
    let app = Router::new()
        .route("/jit-save/{name}", put(save_jit))
        .layer(DefaultBodyLimit::max(1 << 30))
        .with_state(jit)
        .nest_service("/jit", ServeDir::new(&args.jit))
        .nest_service("/store", ServeDir::new(&args.store))
        .fallback_service(ServeDir::new(&args.web))
        .layer(middleware::from_fn(headers));
    // without TCP_NODELAY a kept-alive connection waits out the peer's delayed ACK (~40ms) on
    // every small response; the page reads the store one chunk at a time
    let listener = tokio::net::TcpListener::bind(args.listen).await?.tap_io(|tcp| {
        let _ = tcp.set_nodelay(true);
    });
    eprintln!("vnvm-server on http://{}", args.listen);
    axum::serve(listener, app).await?;
    Ok(())
}
