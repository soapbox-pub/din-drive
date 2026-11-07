//! Signing and self-encryption, either through nostrd's NIP-5F socket (so the
//! drive never holds the user's key) or with a local key.

use std::path::PathBuf;

use anyhow::{Context, bail};
use nostr::nips::nip44;
use nostr::prelude::{Event, FinalizeEvent, Keys, PublicKey, UnsignedEvent};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::Mutex;

use crate::Result;

pub enum Signer {
    Local(Keys),
    Nostrd(Nostrd),
}

impl Signer {
    pub async fn public_key(&self) -> Result<PublicKey> {
        match self {
            Signer::Local(keys) => Ok(keys.public_key()),
            Signer::Nostrd(n) => {
                let v = n.call("get_public_key", vec![]).await?;
                Ok(PublicKey::parse(
                    v.as_str().context("get_public_key: not a string")?,
                )?)
            }
        }
    }

    pub async fn sign(&self, unsigned: UnsignedEvent) -> Result<Event> {
        let event = match self {
            Signer::Local(keys) => unsigned.finalize(keys)?,
            Signer::Nostrd(n) => {
                let v = n
                    .call("sign_event", vec![serde_json::from_str(&unsigned.as_json())?])
                    .await?;
                let json = match v {
                    Value::String(s) => s,
                    other => other.to_string(),
                };
                Event::from_json(json)?
            }
        };
        event.verify()?;
        Ok(event)
    }

    /// NIP-44 encrypt to the user's own pubkey.
    pub async fn encrypt_to_self(&self, plaintext: &str) -> Result<String> {
        match self {
            Signer::Local(keys) => Ok(nip44::encrypt(
                keys.secret_key(),
                &keys.public_key(),
                plaintext,
                nip44::Version::V2,
            )?),
            Signer::Nostrd(n) => {
                let pk = self.public_key().await?.to_hex();
                let v = n.call("nip44_encrypt", vec![json!(pk), json!(plaintext)]).await?;
                Ok(v.as_str().context("nip44_encrypt: not a string")?.to_string())
            }
        }
    }

    /// NIP-44 decrypt something encrypted to the user's own pubkey.
    pub async fn decrypt_from_self(&self, payload: &str) -> Result<String> {
        match self {
            Signer::Local(keys) => Ok(nip44::decrypt(keys.secret_key(), &keys.public_key(), payload)?),
            Signer::Nostrd(n) => {
                let pk = self.public_key().await?.to_hex();
                let v = n.call("nip44_decrypt", vec![json!(pk), json!(payload)]).await?;
                Ok(v.as_str().context("nip44_decrypt: not a string")?.to_string())
            }
        }
    }
}

/// The default NIP-5F socket: `$DIN_NOSTRD_SOCKET`, else where clients look.
pub fn default_socket() -> PathBuf {
    if let Some(p) = std::env::var_os("DIN_NOSTRD_SOCKET") {
        return PathBuf::from(p);
    }
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("nostr/signer.sock")
}

struct Conn {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

/// A NIP-5F client. One connection, reopened if it breaks.
pub struct Nostrd {
    path: PathBuf,
    conn: Mutex<Option<Conn>>,
    next_id: std::sync::atomic::AtomicU64,
}

impl Nostrd {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            conn: Mutex::new(None),
            next_id: 1.into(),
        }
    }

    async fn connect(&self) -> Result<Conn> {
        let stream = UnixStream::connect(&self.path)
            .await
            .with_context(|| format!("connecting to signer at {}", self.path.display()))?;
        let (r, mut writer) = stream.into_split();
        let mut reader = BufReader::new(r);
        let mut handshake = String::new();
        reader.read_line(&mut handshake).await?;
        writer.write_all(b"{\"client\":\"din-drive\"}\n").await?;
        Ok(Conn { reader, writer })
    }

    pub async fn call(&self, method: &str, params: Vec<Value>) -> Result<Value> {
        let mut guard = self.conn.lock().await;
        for attempt in 0..2 {
            if guard.is_none() {
                *guard = Some(self.connect().await?);
            }
            match Self::request(guard.as_mut().unwrap(), &self.next_id, method, &params).await {
                Ok(v) => return v,
                Err(e) if attempt == 0 => {
                    tracing::debug!("signer connection broke ({e}), reconnecting");
                    *guard = None;
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }

    /// Outer error: the connection broke. Inner: the signer refused.
    async fn request(
        conn: &mut Conn,
        next_id: &std::sync::atomic::AtomicU64,
        method: &str,
        params: &[Value],
    ) -> Result<Result<Value>> {
        let id = next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .to_string();
        let mut line = serde_json::to_string(&json!({"id": id, "method": method, "params": params}))?;
        line.push('\n');
        conn.writer.write_all(line.as_bytes()).await?;
        loop {
            let mut buf = String::new();
            if conn.reader.read_line(&mut buf).await? == 0 {
                bail!("signer closed the connection");
            }
            let v: Value = serde_json::from_str(&buf)?;
            if v.get("id").and_then(Value::as_str) != Some(id.as_str()) {
                continue; // a subscription event or a stray line
            }
            if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
                let msg = err.get("message").and_then(Value::as_str).unwrap_or("error");
                return Ok(Err(anyhow::anyhow!("signer: {method}: {msg}")));
            }
            return Ok(Ok(v.get("result").cloned().unwrap_or(Value::Null)));
        }
    }
}
