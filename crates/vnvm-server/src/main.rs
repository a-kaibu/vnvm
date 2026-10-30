//! Serves the vnvm web app and the disk store made by vnvm-pack.
//!
//! - `/` the built web app (web/dist)
//! - `/store/disks/<name>/manifest.json`
//! - `/store/chunks/ab/<hash>`: stored zstd-compressed, sent with
//!   `Content-Encoding: zstd` so the browser decodes them natively, and marked
//!   immutable so its HTTP cache keeps them.
//!
//! Every response carries COOP/COEP: QEMU's threads need SharedArrayBuffer,
//! which browsers only allow on cross-origin isolated pages.

use axum::{
    http::{header, HeaderValue, Request},
    middleware::{self, Next},
    response::Response,
    Router,
};
use clap::Parser;
use std::{net::SocketAddr, path::PathBuf};
use tower_http::services::ServeDir;

#[derive(Parser)]
struct Args {
    /// The built web app.
    #[arg(long, default_value = "web/dist")]
    web: PathBuf,
    /// The vnvm-pack store.
    #[arg(long, default_value = "store")]
    store: PathBuf,
    #[arg(long, default_value = "127.0.0.1:8080")]
    listen: SocketAddr,
}

async fn headers(req: Request<axum::body::Body>, next: Next) -> Response {
    let chunk = req.uri().path().starts_with("/store/chunks/");
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
    } else {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    }
    res
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let app = Router::new()
        .nest_service("/store", ServeDir::new(&args.store))
        .fallback_service(ServeDir::new(&args.web))
        .layer(middleware::from_fn(headers));
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    eprintln!("vnvm-server on http://{}", args.listen);
    axum::serve(listener, app).await?;
    Ok(())
}
