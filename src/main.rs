use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use din_drive::drive::{Drive, Kind2, Options};
use din_drive::keys::ROOT;
use din_drive::signer::{Nostrd, Signer, default_socket};
use din_drive::{DEFAULT_SERVERS, Result, control, relays};
use nostr::prelude::{Keys, ToBech32};
use serde::Deserialize;

#[derive(Parser)]
#[command(
    name = "din-drive",
    version,
    about = "An encrypted drive on Nostr and Blossom"
)]
struct Cli {
    /// Relay to keep the drive on (repeatable). Default: your NIP-65 write relays.
    #[arg(long = "relay", global = true)]
    relays: Vec<String>,
    /// Blossom server to store files on (repeatable). Default: your kind 10063 list.
    #[arg(long = "server", global = true)]
    servers: Vec<String>,
    /// Sign with the key in this file instead of the nostrd signer.
    #[arg(long, global = true, env = "DIN_DRIVE_NSEC_FILE")]
    nsec_file: Option<PathBuf>,
    /// NIP-5F signer socket.
    #[arg(long, global = true)]
    signer: Option<PathBuf>,
    /// Where local state and the cache live.
    #[arg(long, global = true, env = "DIN_DRIVE_STATE")]
    state_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create your drive, or find the one you already have.
    Init,
    /// Mount the drive and keep it in sync until stopped.
    Mount {
        /// Default: ~/Drive
        path: Option<PathBuf>,
    },
    /// List a folder.
    Ls { path: Option<String> },
    /// Upload a local file into a folder.
    Put { file: PathBuf, folder: Option<String> },
    /// Download a file.
    Get { path: String, out: PathBuf },
    /// Make a folder.
    Mkdir { path: String },
    /// Move a file or folder to the trash.
    Rm { path: String },
    /// List the trash.
    Trash,
    /// Permanently delete everything in the trash.
    EmptyTrash,
    /// Restore an item from the trash, by the id `trash` shows.
    Restore { id: String },
    /// Show the account, relays, servers, and drive.
    Status {
        /// Print JSON (from the running drive, if it's mounted).
        #[arg(long)]
        json: bool,
    },
}

#[derive(Deserialize, Default)]
struct Config {
    #[serde(default)]
    relays: Vec<String>,
    #[serde(default)]
    servers: Vec<String>,
    mountpoint: Option<PathBuf>,
    cache_limit_gb: Option<u64>,
}

fn load_config() -> Result<Config> {
    let Some(dir) = dirs::config_dir() else {
        return Ok(Config::default());
    };
    let path = dir.join("din-drive/config.json");
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).with_context(|| format!("reading {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e.into()),
    }
}

async fn open(cli: &Cli, create: bool) -> Result<(Arc<Drive>, Config)> {
    let config = load_config()?;
    let signer = match &cli.nsec_file {
        Some(path) => {
            let nsec =
                std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
            Signer::Local(Keys::parse(nsec.trim())?)
        }
        None => Signer::Nostrd(Nostrd::new(cli.signer.clone().unwrap_or_else(default_socket))),
    };
    let pubkey = signer
        .public_key()
        .await
        .context("no signer available (is nostrd running?)")?;

    let mut relays = if cli.relays.is_empty() {
        config.relays.clone()
    } else {
        cli.relays.clone()
    };
    let mut servers = if cli.servers.is_empty() {
        config.servers.clone()
    } else {
        cli.servers.clone()
    };
    if relays.is_empty() || servers.is_empty() {
        let bootstrap: Vec<String> = relays::DEFAULT_BOOTSTRAP.iter().map(|s| s.to_string()).collect();
        let (found_relays, found_servers) = relays::discover(&bootstrap, &pubkey).await.unwrap_or_default();
        if relays.is_empty() {
            relays = if found_relays.is_empty() {
                bootstrap
            } else {
                found_relays
            };
        }
        if servers.is_empty() {
            servers = if found_servers.is_empty() {
                DEFAULT_SERVERS.iter().map(|s| s.to_string()).collect()
            } else {
                found_servers
            };
        }
    }
    let state_root = match &cli.state_dir {
        Some(p) => p.clone(),
        None => dirs::data_dir().context("no data directory")?.join("din-drive"),
    };
    let opts = Options {
        relays,
        servers,
        state_root,
        cache_limit: config.cache_limit_gb.unwrap_or(10) << 30,
    };
    let drive = Drive::open(signer, opts, create).await?;
    Ok((drive, config))
}

fn resolve(drive: &Drive, path: &str) -> Result<String> {
    drive.resolve(path).with_context(|| format!("{path}: not found"))
}

fn split(path: &str) -> (String, String) {
    let path = path.trim_end_matches('/');
    match path.rsplit_once('/') {
        Some((dir, name)) => (dir.to_string(), name.to_string()),
        None => (String::new(), path.to_string()),
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("din-drive: {e:#}");
        // 3: there's no drive yet, so retrying won't help (systemd stops).
        let no_drive = matches!(
            e.downcast_ref::<din_drive::drive::Error>(),
            Some(din_drive::drive::Error::NoDrive)
        );
        std::process::exit(if no_drive { 3 } else { 1 });
    }
}

/// Ask the mounted drive, if there is one running.
async fn ask(method: &str, params: serde_json::Value) -> Option<Result<serde_json::Value>> {
    let socket = control::default_socket();
    if tokio::net::UnixStream::connect(&socket).await.is_err() {
        return None;
    }
    Some(control::request(&socket, method, params).await)
}

async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "din_drive=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();

    match &cli.command {
        Command::Init => {
            let (drive, _) = open(&cli, true).await?;
            println!("Drive {} for {}", drive.drive_id, drive.pubkey.to_bech32()?);
            drive.shutdown().await;
        }
        Command::Mount { path } => {
            // Never create a drive here: at boot the signer may be locked or
            // the network down, and that mustn't look like having no drive.
            let (drive, config) = open(&cli, false).await?;
            let mountpoint = path
                .clone()
                .or(config.mountpoint)
                .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join("Drive"));
            mount(drive, &mountpoint).await?;
        }
        Command::Ls { path } => {
            let (drive, _) = open(&cli, false).await?;
            drive.sync().await?;
            let dir = resolve(&drive, path.as_deref().unwrap_or(""))?;
            for (name, d) in drive.children(&dir) {
                let Some(info) = drive.info(&d) else { continue };
                match info.kind {
                    Kind2::Dir => println!("{:>12}  {name}/", "-"),
                    Kind2::File => println!("{:>12}  {name}", info.size),
                }
            }
            drive.shutdown().await;
        }
        Command::Put { file, folder } => {
            let (drive, _) = open(&cli, false).await?;
            drive.sync().await?;
            let dir = resolve(&drive, folder.as_deref().unwrap_or(""))?;
            let name = file
                .file_name()
                .and_then(|n| n.to_str())
                .context("bad file name")?;
            let d = match drive.lookup(&dir, name) {
                Some(d) => d,
                None => drive.create_file(&dir, name)?,
            };
            let mut out = drive.open_write(&d, true).await?;
            std::io::copy(&mut std::fs::File::open(file)?, &mut out)?;
            drop(out);
            drive.release_write(&d, true)?;
            drive.flush().await?;
            println!("Uploaded {name}");
            drive.shutdown().await;
        }
        Command::Get { path, out } => {
            let (drive, _) = open(&cli, false).await?;
            drive.sync().await?;
            let d = resolve(&drive, path)?;
            let size = drive.info(&d).context("not a file")?.size;
            let mut f = std::fs::File::create(out)?;
            let mut offset = 0;
            while offset < size {
                let chunk = drive.read(&d, offset, 4 << 20).await?;
                if chunk.is_empty() {
                    bail!("unexpected end of file");
                }
                std::io::Write::write_all(&mut f, &chunk)?;
                offset += chunk.len() as u64;
            }
            drive.shutdown().await;
        }
        Command::Mkdir { path } => {
            let (drive, _) = open(&cli, false).await?;
            drive.sync().await?;
            let (dir, name) = split(path);
            let parent = resolve(&drive, &dir)?;
            drive.mkdir(&parent, &name)?;
            drive.flush().await?;
            drive.shutdown().await;
        }
        Command::Rm { path } => {
            let (drive, _) = open(&cli, false).await?;
            drive.sync().await?;
            let d = resolve(&drive, path)?;
            let is_dir = drive.info(&d).is_some_and(|i| i.kind == Kind2::Dir);
            if is_dir {
                drive.remove(&d, true)?;
            } else {
                drive.remove(&d, false)?;
            }
            drive.flush().await?;
            drive.shutdown().await;
        }
        Command::Trash => {
            let items = match ask("trash", serde_json::Value::Null).await {
                Some(v) => v?,
                None => {
                    let (drive, _) = open(&cli, false).await?;
                    drive.sync().await?;
                    let items = drive.trash_items();
                    drive.shutdown().await;
                    serde_json::to_value(items)?
                }
            };
            for item in items.as_array().into_iter().flatten() {
                println!(
                    "{}  {}",
                    item["id"].as_str().unwrap_or(""),
                    item["name"].as_str().unwrap_or("")
                );
            }
        }
        Command::Restore { id } => match ask("restore", serde_json::json!({ "id": id })).await {
            Some(v) => {
                v?;
            }
            None => {
                let (drive, _) = open(&cli, false).await?;
                drive.sync().await?;
                drive.restore(id)?;
                drive.flush().await?;
                drive.shutdown().await;
            }
        },
        Command::EmptyTrash => {
            let n = match ask("empty_trash", serde_json::Value::Null).await {
                Some(v) => v?.as_u64().unwrap_or(0) as usize,
                None => {
                    let (drive, _) = open(&cli, false).await?;
                    drive.sync().await?;
                    let n = drive.empty_trash(None).await?;
                    drive.flush().await?;
                    drive.shutdown().await;
                    n
                }
            };
            println!("Deleted {n} item(s)");
        }
        Command::Status { json: true } => {
            let status = match ask("status", serde_json::Value::Null).await {
                Some(v) => v?,
                None => {
                    let (drive, _) = open(&cli, false).await?;
                    let status = serde_json::to_value(drive.status().await?)?;
                    drive.shutdown().await;
                    status
                }
            };
            println!("{}", serde_json::to_string_pretty(&status)?);
        }
        Command::Status { json: false } => {
            let (drive, _) = open(&cli, false).await?;
            let applied = drive.sync().await?;
            println!("account: {}", drive.pubkey.to_bech32()?);
            println!("drive:   {}", drive.drive_id);
            println!("servers: {}", drive.servers.join(", "));
            println!(
                "synced:  {applied} change(s); {} item(s) at the top level",
                drive.children(ROOT).len()
            );
            drive.shutdown().await;
        }
    }
    Ok(())
}

async fn mount(drive: Arc<Drive>, mountpoint: &Path) -> Result<()> {
    // A mount left behind by a crash answers "transport endpoint is not
    // connected"; clear it first.
    if let Err(e) = std::fs::metadata(mountpoint)
        && e.raw_os_error() == Some(libc::ENOTCONN)
    {
        let _ = std::process::Command::new("fusermount3")
            .arg("-uz")
            .arg(mountpoint)
            .status();
    }
    std::fs::create_dir_all(mountpoint)?;
    match drive.sync().await {
        Ok(n) => tracing::info!("synced {n} change(s)"),
        Err(e) => tracing::warn!("initial sync failed, starting offline: {e}"),
    }
    drive.spawn_workers();
    drive.spawn_live();

    let fs = din_drive::fs::DriveFs::new(drive.clone(), tokio::runtime::Handle::current());
    let mut config = fuser::Config::default();
    config.mount_options = vec![
        fuser::MountOption::FSName("din-drive".into()),
        fuser::MountOption::Subtype("din-drive".into()),
        fuser::MountOption::NoAtime,
    ];
    config.n_threads = Some(4);
    let session = fuser::spawn_mount(fs, mountpoint, &config)
        .with_context(|| format!("mounting at {}", mountpoint.display()))?;
    tracing::info!("mounted at {}", mountpoint.display());
    control::serve(
        drive.clone(),
        &control::default_socket(),
        mountpoint.to_path_buf(),
    )?;

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("unmounting");
    drop(session);
    let _ = std::fs::remove_file(control::default_socket());
    // Publish whatever is ready before exiting; uploads resume on next start.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), drive.publish_pending()).await;
    drive.shutdown().await;
    Ok(())
}
