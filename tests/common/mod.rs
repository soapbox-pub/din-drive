#![allow(dead_code)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use base64::Engine;
use din_drive::drive::{Drive, Options};
use din_drive::signer::Signer;
use nostr::prelude::{Event, Keys};
use nostr_sdk::prelude::MockRelay;
use sha2::{Digest, Sha256};

pub type Blobs = Arc<Mutex<HashMap<String, Vec<u8>>>>;

fn authorized(headers: &HeaderMap, verb: &str, hash: &str) -> bool {
    let Some(auth) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let Some(b64) = auth.strip_prefix("Nostr ") else {
        return false;
    };
    let Ok(json) = base64::engine::general_purpose::STANDARD.decode(b64) else {
        return false;
    };
    let Ok(event) = Event::from_json(json) else {
        return false;
    };
    let tag = |name: &str| {
        event.tags.iter().any(|t| {
            let s = t.as_slice();
            s.first().map(String::as_str) == Some(name)
                && s.get(1).map(String::as_str) == Some(if name == "t" { verb } else { hash })
        })
    };
    event.verify().is_ok() && event.kind.as_u16() == 24242 && tag("t") && tag("x")
}

async fn upload(State(blobs): State<Blobs>, headers: HeaderMap, body: Bytes) -> Response {
    let hash = hex::encode(Sha256::digest(&body));
    if !authorized(&headers, "upload", &hash) {
        return (StatusCode::UNAUTHORIZED, "bad auth").into_response();
    }
    let size = body.len();
    blobs.lock().unwrap().insert(hash.clone(), body.to_vec());
    axum::Json(serde_json::json!({"sha256": hash, "size": size})).into_response()
}

async fn fetch(State(blobs): State<Blobs>, UrlPath(hash): UrlPath<String>, headers: HeaderMap) -> Response {
    let Some(data) = blobs.lock().unwrap().get(&hash).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(range) = headers.get(header::RANGE).and_then(|v| v.to_str().ok())
        && let Some((a, b)) = range.strip_prefix("bytes=").and_then(|r| r.split_once('-'))
    {
        let start: usize = a.parse().unwrap();
        let end: usize = b.parse::<usize>().unwrap().min(data.len() - 1);
        return (StatusCode::PARTIAL_CONTENT, data[start..=end].to_vec()).into_response();
    }
    data.into_response()
}

async fn delete(State(blobs): State<Blobs>, UrlPath(hash): UrlPath<String>, headers: HeaderMap) -> Response {
    if !authorized(&headers, "delete", &hash) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    blobs.lock().unwrap().remove(&hash);
    StatusCode::OK.into_response()
}

/// A minimal Blossom server. Returns its URL and its blobs.
pub async fn blossom() -> (String, Blobs) {
    let blobs: Blobs = Default::default();
    let app = Router::new()
        .route("/upload", put(upload))
        .route("/{hash}", get(fetch).delete(delete))
        .with_state(blobs.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, blobs)
}

pub struct Env {
    pub relay: MockRelay,
    pub relay_url: String,
    pub blossom_url: String,
    pub blobs: Blobs,
    pub keys: Keys,
}

pub async fn env() -> Env {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let relay = MockRelay::run().await.unwrap();
    let relay_url = relay.url().await.to_string();
    let (blossom_url, blobs) = blossom().await;
    Env {
        relay,
        relay_url,
        blossom_url,
        blobs,
        keys: Keys::generate(),
    }
}

impl Env {
    /// A device: its own state directory, the same account.
    pub async fn device(&self, state: &Path, create: bool) -> Arc<Drive> {
        let opts = Options {
            relays: vec![self.relay_url.clone()],
            servers: vec![self.blossom_url.clone()],
            state_root: state.to_path_buf(),
            cache_limit: 1 << 30,
        };
        Drive::open(Signer::Local(self.keys.clone()), opts, create)
            .await
            .unwrap()
    }
}

pub fn data(n: usize, seed: u8) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

pub async fn write_file(drive: &Drive, d: &str, bytes: &[u8]) {
    use std::io::Write;
    let mut f = drive.open_write(d, true).await.unwrap();
    f.write_all(bytes).unwrap();
    drop(f);
    drive.release_write(d, true).unwrap();
}

pub async fn read_all(drive: &Drive, d: &str) -> Vec<u8> {
    let size = drive.info(d).unwrap().size;
    drive.read(d, 0, size as usize + 10).await.unwrap()
}
