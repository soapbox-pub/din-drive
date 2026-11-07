//! A Blossom client: BUD-01 fetch (with ranges), BUD-02 upload and delete,
//! BUD-11 authorization.

use std::time::Duration;

use anyhow::{Context, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use nostr::prelude::{EventBuilder, FinalizeUnsignedEvent, Kind, Tag, Timestamp};
use reqwest::{StatusCode, header};
use sha2::{Digest, Sha256};

use crate::Result;
use crate::model::KIND_BLOSSOM_AUTH;
use crate::signer::Signer;

pub struct Blossom {
    http: reqwest::Client,
}

fn url(server: &str, path: &str) -> String {
    format!("{}/{}", server.trim_end_matches('/'), path)
}

impl Blossom {
    pub fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .user_agent(concat!("din-drive/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("setting up HTTPS (are CA certificates installed?)")?;
        Ok(Self { http })
    }

    async fn auth(&self, signer: &Signer, verb: &str, hash: &str) -> Result<String> {
        let pubkey = signer.public_key().await?;
        let expiration = Timestamp::now().as_secs() + 300;
        let unsigned = EventBuilder::new(Kind::Custom(KIND_BLOSSOM_AUTH), format!("{verb} blob"))
            .tags([
                Tag::parse(["t", verb])?,
                Tag::parse(["x", hash])?,
                Tag::parse(["expiration".to_string(), expiration.to_string()])?,
            ])
            .finalize_unsigned(pubkey);
        let event = signer.sign(unsigned).await?;
        Ok(format!("Nostr {}", B64.encode(event.as_json())))
    }

    /// Upload `data`, whose sha256 is `hash`, to `server`.
    pub async fn upload(&self, signer: &Signer, server: &str, hash: &str, data: Vec<u8>) -> Result<()> {
        let auth = self.auth(signer, "upload", hash).await?;
        let res = self
            .http
            .put(url(server, "upload"))
            .header(header::AUTHORIZATION, auth)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header("X-SHA-256", hash)
            .timeout(Duration::from_secs(600))
            .body(data)
            .send()
            .await?;
        let status = res.status();
        if !status.is_success() {
            let reason = res
                .headers()
                .get("x-reason")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
                .unwrap_or_default();
            bail!("{server} rejected upload: {status} {reason}");
        }
        let desc: serde_json::Value = res.json().await.unwrap_or_default();
        if let Some(got) = desc.get("sha256").and_then(|v| v.as_str())
            && got != hash
        {
            bail!("{server} stored {got}, expected {hash}");
        }
        Ok(())
    }

    /// Fetch a whole blob from the first server that has it, verifying its hash.
    pub async fn get(&self, servers: &[String], hash: &str) -> Result<Vec<u8>> {
        let mut last_err = anyhow::anyhow!("no servers for blob {hash}");
        for server in servers {
            match self.get_from(server, hash).await {
                Ok(bytes) => return Ok(bytes),
                Err(e) => last_err = e,
            }
        }
        Err(last_err)
    }

    async fn get_from(&self, server: &str, hash: &str) -> Result<Vec<u8>> {
        let res = self
            .http
            .get(url(server, hash))
            .timeout(Duration::from_secs(600))
            .send()
            .await?;
        if !res.status().is_success() {
            bail!("{server}: {}", res.status());
        }
        let bytes = res.bytes().await?;
        if hex::encode(Sha256::digest(&bytes)) != hash {
            bail!("{server} returned the wrong bytes for {hash}");
        }
        Ok(bytes.to_vec())
    }

    /// Fetch bytes `start..end` of a blob from the first server that has it.
    /// Callers authenticate the bytes themselves (segment tags).
    pub async fn get_range(&self, servers: &[String], hash: &str, start: u64, end: u64) -> Result<Vec<u8>> {
        let mut last_err = anyhow::anyhow!("no servers for blob {hash}");
        for server in servers {
            match self.get_range_from(server, hash, start, end).await {
                Ok(bytes) => return Ok(bytes),
                Err(e) => {
                    tracing::debug!("range fetch from {server} failed: {e}");
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }

    async fn get_range_from(&self, server: &str, hash: &str, start: u64, end: u64) -> Result<Vec<u8>> {
        let res = self
            .http
            .get(url(server, hash))
            .header(header::RANGE, format!("bytes={}-{}", start, end - 1))
            .timeout(Duration::from_secs(300))
            .send()
            .await?;
        let status = res.status();
        let bytes = match status {
            StatusCode::PARTIAL_CONTENT => res.bytes().await?.to_vec(),
            StatusCode::OK => {
                // The server ignored the range. Verify the whole blob, then slice.
                let all = res.bytes().await?;
                if hex::encode(Sha256::digest(&all)) != hash {
                    bail!("{server} returned the wrong bytes for {hash}");
                }
                all.get(start as usize..end as usize)
                    .context("blob shorter than expected")?
                    .to_vec()
            }
            s => bail!("{server}: {s}"),
        };
        if bytes.len() as u64 != end - start {
            bail!(
                "{server} returned {} bytes, expected {}",
                bytes.len(),
                end - start
            );
        }
        Ok(bytes)
    }

    pub async fn delete(&self, signer: &Signer, server: &str, hash: &str) -> Result<()> {
        let auth = self.auth(signer, "delete", hash).await?;
        let res = self
            .http
            .delete(url(server, hash))
            .header(header::AUTHORIZATION, auth)
            .timeout(Duration::from_secs(60))
            .send()
            .await?;
        if !res.status().is_success() && res.status() != StatusCode::NOT_FOUND {
            bail!("{server} rejected delete: {}", res.status());
        }
        Ok(())
    }
}
