//! The control socket: a running drive answers JSON requests, one per line, so
//! the desktop can show its state and manage the trash.
//!
//! `{"method":"status"}` → `{"result":{…}}` or `{"error":"…"}`. Methods:
//! `status`, `trash`, `restore` (`{"id":…}`), `empty_trash`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::Result;
use crate::drive::Drive;

/// `$XDG_RUNTIME_DIR/din-drive.sock`.
pub fn default_socket() -> PathBuf {
    dirs::runtime_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("din-drive.sock")
}

#[derive(Deserialize)]
struct Request {
    method: String,
    #[serde(default)]
    params: Value,
}

pub fn serve(drive: Arc<Drive>, socket: &Path, mountpoint: PathBuf) -> Result<()> {
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    }
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let drive = drive.clone();
                    let mountpoint = mountpoint.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle(stream, &drive, &mountpoint).await {
                            tracing::debug!("control connection: {e}");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!("control socket: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }
    });
    Ok(())
}

async fn handle(stream: UnixStream, drive: &Drive, mountpoint: &Path) -> Result<()> {
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    while let Some(line) = lines.next_line().await? {
        let reply = match serde_json::from_str::<Request>(&line) {
            Ok(req) => match call(drive, mountpoint, &req).await {
                Ok(v) => json!({ "result": v }),
                Err(e) => json!({ "error": format!("{e:#}") }),
            },
            Err(e) => json!({ "error": format!("bad request: {e}") }),
        };
        let mut out = serde_json::to_string(&reply)?;
        out.push('\n');
        w.write_all(out.as_bytes()).await?;
    }
    Ok(())
}

async fn call(drive: &Drive, mountpoint: &Path, req: &Request) -> Result<Value> {
    Ok(match req.method.as_str() {
        "status" => {
            let mut v = serde_json::to_value(drive.status().await?)?;
            v["mountpoint"] = mountpoint.display().to_string().into();
            v
        }
        "trash" => serde_json::to_value(drive.trash_items())?,
        "restore" => {
            let id = req.params.get("id").and_then(Value::as_str).unwrap_or_default();
            drive.restore(id)?;
            Value::Bool(true)
        }
        "empty_trash" => json!(drive.empty_trash(None).await?),
        other => anyhow::bail!("unknown method {other}"),
    })
}

/// Send one request to a running drive.
pub async fn request(socket: &Path, method: &str, params: Value) -> Result<Value> {
    let stream = UnixStream::connect(socket).await?;
    let (r, mut w) = stream.into_split();
    let mut line = serde_json::to_string(&json!({ "method": method, "params": params }))?;
    line.push('\n');
    w.write_all(line.as_bytes()).await?;
    let mut lines = BufReader::new(r).lines();
    let reply: Value = serde_json::from_str(&lines.next_line().await?.unwrap_or_default())?;
    if let Some(e) = reply.get("error").and_then(Value::as_str) {
        anyhow::bail!("{e}");
    }
    Ok(reply.get("result").cloned().unwrap_or(Value::Null))
}
