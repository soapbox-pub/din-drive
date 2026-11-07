//! The drive engine: local state, sync with relays, the upload queue, and
//! reading file contents.
//!
//! Local changes apply immediately to SQLite and the in-memory tree, and are
//! published in the background, so the filesystem stays fast and works
//! offline. File contents are written to a staging file, then encrypted and
//! uploaded once the file has been closed for a moment.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use nostr::prelude::{
    Event, EventBuilder, Filter, FinalizeUnsignedEvent, Kind, PublicKey, SingleLetterTag, Tag, Timestamp,
};
use rand::RngCore;
use tokio::sync::Notify;

use crate::Result;
use crate::blossom::Blossom;
use crate::cache::Cache;
use crate::keys::{DriveKeys, ROOT, random_id};
use crate::model::*;
use crate::relays::Relays;
use crate::signer::Signer;
use crate::store::{NodeRow, Store};
use crate::stream::{self, SEGMENT, segment_count};
use crate::tree::Tree;

/// How long a file must stay closed before it's uploaded.
const UPLOAD_DEBOUNCE: u64 = 2;
/// Segments fetched beyond what a read asked for.
const READAHEAD: u64 = 15;
/// Trashed nodes are deleted for good after this long.
const TRASH_RETENTION: u64 = 30 * 24 * 3600;

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn tag(name: &str, value: &str) -> Tag {
    Tag::parse([name, value]).expect("valid tag")
}

fn tag_value<'a>(event: &'a Event, name: &str) -> Option<&'a str> {
    event.tags.iter().find_map(|t| {
        let s = t.as_slice();
        (s.first().map(String::as_str) == Some(name))
            .then(|| s.get(1).map(String::as_str))
            .flatten()
    })
}

pub struct Options {
    pub relays: Vec<String>,
    pub servers: Vec<String>,
    /// Where per-account state lives; the account's pubkey is appended.
    pub state_root: PathBuf,
    pub cache_limit: u64,
}

struct State {
    store: Store,
    tree: Tree,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind2 {
    Dir,
    File,
}

#[derive(Debug, Clone)]
pub struct Info {
    pub kind: Kind2,
    pub size: u64,
    pub mtime: u64,
}

pub struct Drive {
    pub keys: Arc<DriveKeys>,
    pub drive_id: String,
    pub pubkey: PublicKey,
    pub servers: Vec<String>,
    signer: Signer,
    relays: Relays,
    blossom: Blossom,
    state: Mutex<State>,
    cache: Cache,
    cache_limit: u64,
    staging_dir: PathBuf,
    snapshot_dir: PathBuf,
    work_dir: PathBuf,
    wake_publish: Notify,
    wake_upload: Notify,
    fetch_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    open_writers: Mutex<HashMap<String, usize>>,
    started: u64,
}

impl Drive {
    /// Open the user's home drive, creating it if `create` and none exists.
    pub async fn open(signer: Signer, opts: Options, create: bool) -> Result<Arc<Drive>> {
        let pubkey = signer.public_key().await?;
        let state_dir = opts.state_root.join(pubkey.to_hex());
        fs::create_dir_all(&state_dir)?;
        let store = Store::open(&state_dir.join("drive.db"))?;
        let relays = Relays::connect(&opts.relays).await?;

        let (drive_id, keys) = match (store.get_meta("drive_id")?, store.get_meta("dk")?) {
            (Some(id), Some(dk)) => (id, DriveKeys::from_hex(&dk)?),
            _ => {
                let (id, keys) = match find_home_drive(&signer, &relays, &pubkey).await? {
                    Some(found) => found,
                    None if create => create_home_drive(&signer, &relays, &pubkey).await?,
                    None => bail!("no drive found for this account; run `din-drive init` to create one"),
                };
                store.set_meta("drive_id", &id)?;
                store.set_meta("dk", &keys.dk_hex())?;
                (id, keys)
            }
        };
        let keys = Arc::new(keys);
        let mut tree = Tree::new(keys.clone());
        for row in store.all_nodes()? {
            tree.upsert(&row.d, &row.y, row.content);
        }
        let dir = |name: &str| -> Result<PathBuf> {
            let p = state_dir.join(name);
            fs::create_dir_all(&p)?;
            Ok(p)
        };
        let drive = Arc::new(Drive {
            keys,
            drive_id,
            pubkey,
            servers: opts.servers,
            signer,
            relays,
            blossom: Blossom::new()?,
            state: Mutex::new(State { store, tree }),
            cache: Cache::new(dir("cache")?)?,
            cache_limit: opts.cache_limit,
            staging_dir: dir("staging")?,
            snapshot_dir: dir("snapshots")?,
            work_dir: dir("work")?,
            wake_publish: Notify::new(),
            wake_upload: Notify::new(),
            fetch_locks: Mutex::new(HashMap::new()),
            open_writers: Mutex::new(HashMap::new()),
            started: now(),
        });
        // Leftover work files from a previous run.
        for e in fs::read_dir(&drive.work_dir)?.flatten() {
            let _ = fs::remove_dir_all(e.path());
        }
        Ok(drive)
    }

    fn st(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    fn node_filter(&self) -> Filter {
        Filter::new()
            .kind(Kind::Custom(KIND_NODE))
            .author(self.pubkey)
            .custom_tag(SingleLetterTag::from_char('j').expect("j"), self.keys.j.clone())
    }

    /// Fetch every node of the drive changed since the last sync.
    pub async fn sync(&self) -> Result<usize> {
        let last = self
            .st()
            .store
            .get_meta("last_sync")?
            .and_then(|s| s.parse::<u64>().ok());
        let started = now();
        let mut filter = self.node_filter();
        if let Some(t) = last {
            // Relays may accept events late; look back an hour.
            filter = filter.since(Timestamp::from(t.saturating_sub(3600)));
        }
        let events = self.relays.fetch(filter, Duration::from_secs(20)).await?;
        let mut applied = 0;
        for event in &events {
            match self.apply_remote(event) {
                Ok(true) => applied += 1,
                Ok(false) => {}
                Err(e) => tracing::debug!("ignoring event {}: {e}", event.id),
            }
        }
        self.st().store.set_meta("last_sync", &started.to_string())?;
        Ok(applied)
    }

    /// Follow live changes from other devices.
    pub fn spawn_live(self: &Arc<Self>) {
        let drive = self.clone();
        tokio::spawn(async move {
            loop {
                let filter = drive
                    .node_filter()
                    .since(Timestamp::from(now().saturating_sub(60)));
                match drive.relays.subscribe(filter).await {
                    Ok(mut rx) => {
                        while let Some(event) = rx.recv().await {
                            if let Err(e) = drive.apply_remote(&event) {
                                tracing::debug!("ignoring event {}: {e}", event.id);
                            }
                        }
                    }
                    Err(e) => tracing::warn!("subscription failed: {e}"),
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
                if let Err(e) = drive.sync().await {
                    tracing::warn!("sync failed: {e}");
                }
            }
        });
    }

    /// Apply a node event from a relay. Returns whether it changed anything.
    pub fn apply_remote(&self, event: &Event) -> Result<bool> {
        if event.kind.as_u16() != KIND_NODE || event.pubkey != self.pubkey {
            return Ok(false);
        }
        if tag_value(event, "j") != Some(self.keys.j.as_str()) {
            return Ok(false);
        }
        event.verify()?;
        let d = tag_value(event, "d").context("no d tag")?;
        let y = tag_value(event, "y").context("no y tag")?;
        if d == ROOT {
            bail!("node uses the reserved id");
        }
        let content: NodeContent = serde_json::from_str(&self.keys.decrypt(&event.content)?)?;
        let created_at = event.created_at.as_secs();
        let id = event.id.to_hex();

        let mut st = self.st();
        if let Some(cur) = st.store.get_node(d)? {
            let newer = created_at > cur.created_at
                || (created_at == cur.created_at && cur.event_id.as_ref().is_some_and(|cur_id| id < *cur_id));
            if !newer {
                return Ok(false);
            }
            if st.store.has_upload(d)? {
                // Unsaved local content wins; it gets a newer timestamp when uploaded.
                tracing::info!("keeping local changes to {d} over a newer remote version");
                return Ok(false);
            }
        }
        let row = NodeRow {
            d: d.to_string(),
            created_at,
            event_id: Some(id),
            author: event.pubkey.to_hex(),
            y: y.to_string(),
            content: content.clone(),
            dirty: false,
        };
        st.store.put_node(&row)?;
        st.tree.upsert(d, y, content);
        Ok(true)
    }

    // ---- Reading the tree ----

    pub fn children(&self, dir: &str) -> Vec<(String, String)> {
        self.st().tree.children(dir)
    }

    pub fn lookup(&self, dir: &str, name: &str) -> Option<String> {
        self.st().tree.lookup(dir, name)
    }

    pub fn parent(&self, d: &str) -> Option<String> {
        self.st().tree.parent(d)
    }

    pub fn is_visible(&self, d: &str) -> bool {
        self.st().tree.is_visible(d)
    }

    pub fn trash(&self) -> Vec<String> {
        self.st().tree.trash()
    }

    pub fn content(&self, d: &str) -> Option<NodeContent> {
        self.st().tree.get(d).map(|e| e.content.clone())
    }

    pub fn info(&self, d: &str) -> Option<Info> {
        if d == ROOT {
            return Some(Info {
                kind: Kind2::Dir,
                size: 0,
                mtime: self.started,
            });
        }
        let content = self.content(d)?;
        if content.is_dir() {
            return Some(Info {
                kind: Kind2::Dir,
                size: 0,
                mtime: content.mtime,
            });
        }
        if !content.is_file() {
            return None;
        }
        let size = self.local_size(d).unwrap_or(content.size.unwrap_or(0));
        Some(Info {
            kind: Kind2::File,
            size,
            mtime: content.mtime,
        })
    }

    /// Resolve a slash-separated path to a node id.
    pub fn resolve(&self, path: &str) -> Option<String> {
        let mut cur = ROOT.to_string();
        for part in path.split('/').filter(|p| !p.is_empty()) {
            cur = self.lookup(&cur, part)?;
        }
        Some(cur)
    }

    // ---- Changing the tree ----

    fn stamp(prev: Option<u64>) -> u64 {
        let t = now();
        match prev {
            Some(p) if p >= t => p + 1,
            _ => t,
        }
    }

    fn save_local(&self, st: &mut State, d: &str, y: String, content: NodeContent) -> Result<()> {
        let prev = st.store.get_node(d)?;
        let row = NodeRow {
            d: d.to_string(),
            created_at: Self::stamp(prev.as_ref().map(|r| r.created_at)),
            event_id: prev.and_then(|r| r.event_id),
            author: self.pubkey.to_hex(),
            y: y.clone(),
            content: content.clone(),
            dirty: true,
        };
        st.store.put_node(&row)?;
        st.tree.upsert(d, &y, content);
        self.wake_publish.notify_one();
        Ok(())
    }

    fn check_name(&self, st: &mut State, parent: &str, name: &str) -> Result<()> {
        if !valid_name(name) {
            bail!(Error::InvalidName);
        }
        if !st.tree.is_visible(parent)
            || !(parent == ROOT || st.tree.get(parent).is_some_and(|e| e.content.is_dir()))
        {
            bail!(Error::NotFound);
        }
        if st.tree.lookup(parent, name).is_some() {
            bail!(Error::Exists);
        }
        Ok(())
    }

    pub fn mkdir(&self, parent: &str, name: &str) -> Result<String> {
        let mut st = self.st();
        self.check_name(&mut st, parent, name)?;
        let d = random_id(8);
        self.save_local(
            &mut st,
            &d,
            self.keys.parent_tag(parent),
            NodeContent::dir(name, now()),
        )?;
        Ok(d)
    }

    /// Create an empty file, with an empty staging file to write into.
    pub fn create_file(&self, parent: &str, name: &str) -> Result<String> {
        let mut st = self.st();
        self.check_name(&mut st, parent, name)?;
        let d = random_id(8);
        File::create(self.staging_path(&d))?;
        let mut content = NodeContent::file(name, now());
        content.mime = guess_mime(name);
        self.save_local(&mut st, &d, self.keys.parent_tag(parent), content)?;
        st.store.queue_upload(&d, now())?;
        Ok(d)
    }

    pub fn set_mtime(&self, d: &str, mtime: u64) -> Result<()> {
        let mut st = self.st();
        let e = st.tree.get(d).context(Error::NotFound)?.clone();
        let mut content = e.content;
        content.mtime = mtime;
        self.save_local(&mut st, d, e.y, content)
    }

    /// Move and/or rename `d`. If a file is being moved over another file, the
    /// target keeps its identity (and history) and takes the source's content.
    pub fn rename(&self, d: &str, new_parent: &str, new_name: &str) -> Result<()> {
        if !valid_name(new_name) {
            bail!(Error::InvalidName);
        }
        let mut st = self.st();
        let src = st.tree.get(d).context(Error::NotFound)?.clone();
        if !(new_parent == ROOT || st.tree.get(new_parent).is_some_and(|e| e.content.is_dir())) {
            bail!(Error::NotFound);
        }
        // Refuse to move a folder into itself.
        if src.content.is_dir() {
            let mut cur = new_parent.to_string();
            while cur != ROOT {
                if cur == d {
                    bail!(Error::Invalid);
                }
                cur = st.tree.parent(&cur).unwrap_or_else(|| ROOT.to_string());
            }
        }
        if let Some(target) = st.tree.lookup(new_parent, new_name) {
            if target == d {
                return Ok(());
            }
            let t = st.tree.get(&target).unwrap().clone();
            match (src.content.is_dir(), t.content.is_dir()) {
                (false, false) => {
                    drop(st);
                    return self.replace_file(d, &target);
                }
                (true, true) => {
                    if !st.tree.children(&target).is_empty() {
                        bail!(Error::NotEmpty);
                    }
                    self.trash_locked(&mut st, &target)?;
                }
                (true, false) => bail!(Error::NotDir),
                (false, true) => bail!(Error::IsDir),
            }
        }
        let mut content = src.content;
        content.name = Some(new_name.to_string());
        self.save_local(&mut st, d, self.keys.parent_tag(new_parent), content)
    }

    /// `target` takes `src`'s content, and `src` goes away.
    fn replace_file(&self, src: &str, target: &str) -> Result<()> {
        let mut st = self.st();
        let s = st.tree.get(src).context(Error::NotFound)?.clone();
        let t = st.tree.get(target).context(Error::NotFound)?.clone();
        let mut content = t.content.clone();
        content.size = s.content.size;
        content.mime = s.content.mime.clone().or(content.mime);
        content.sha256 = s.content.sha256.clone();
        content.enc = s.content.enc.clone();
        content.parts = s.content.parts.clone();
        content.servers = s.content.servers.clone();
        content.mtime = s.content.mtime;
        content.trashed = None;

        // Move local data and pending uploads over.
        // The target's own unsaved content is replaced too.
        for dir in [&self.staging_dir, &self.snapshot_dir] {
            let _ = fs::remove_file(dir.join(target));
            let from = dir.join(src);
            if from.exists() {
                fs::rename(&from, dir.join(target))?;
            }
        }
        if st.store.has_upload(src)? {
            st.store.remove_upload(src)?;
            st.store.queue_upload(target, now())?;
        }
        {
            let mut writers = self.open_writers.lock().unwrap();
            if let Some(n) = writers.remove(src) {
                *writers.entry(target.to_string()).or_default() += n;
            }
        }
        self.save_local(&mut st, target, t.y, content)?;
        self.forget_locked(&mut st, src)
    }

    /// Remove a node that was never published, or tombstone it.
    fn forget_locked(&self, st: &mut State, d: &str) -> Result<()> {
        let row = st.store.get_node(d)?.context(Error::NotFound)?;
        if row.event_id.is_none() {
            st.store.delete_node(d)?;
            st.store.remove_upload(d)?;
            st.tree.remove(d);
            return Ok(());
        }
        self.save_local(st, d, row.y, NodeContent::gone(now()))
    }

    fn trash_locked(&self, st: &mut State, d: &str) -> Result<()> {
        let e = st.tree.get(d).context(Error::NotFound)?.clone();
        let mut content = e.content;
        content.trashed = Some(now());
        self.save_local(st, d, e.y, content)
    }

    /// Move to the trash. Folders must be empty, as with `rmdir`.
    pub fn remove(&self, d: &str, want_dir: bool) -> Result<()> {
        let mut st = self.st();
        let e = st.tree.get(d).context(Error::NotFound)?.clone();
        match (want_dir, e.content.is_dir()) {
            (true, false) => bail!(Error::NotDir),
            (false, true) => bail!(Error::IsDir),
            (true, true) if !st.tree.children(d).is_empty() => bail!(Error::NotEmpty),
            _ => {}
        }
        let row = st.store.get_node(d)?.context(Error::NotFound)?;
        if row.event_id.is_none() && !self.snapshot_dir.join(d).exists() {
            // Never left this machine: just drop it.
            let _ = fs::remove_file(self.staging_path(d));
            return self.forget_locked(&mut st, d);
        }
        self.trash_locked(&mut st, d)
    }

    pub fn restore(&self, d: &str) -> Result<()> {
        let mut st = self.st();
        let e = st.tree.get(d).context(Error::NotFound)?.clone();
        let mut content = e.content;
        content.trashed = None;
        self.save_local(&mut st, d, e.y, content)
    }

    /// Permanently delete trashed nodes (all, or those trashed before `before`).
    pub async fn empty_trash(&self, before: Option<u64>) -> Result<usize> {
        let mut blobs = Vec::new();
        let mut count = 0;
        {
            let mut st = self.st();
            for top in st.tree.trash() {
                let trashed = st.tree.get(&top).and_then(|e| e.content.trashed).unwrap_or(0);
                if before.is_some_and(|b| trashed >= b) {
                    continue;
                }
                for d in st.tree.subtree(&top) {
                    let Some(e) = st.tree.get(&d).cloned() else {
                        continue;
                    };
                    for p in e.content.parts.iter().flatten() {
                        blobs.push((p.x.clone(), e.content.servers.clone().unwrap_or_default()));
                    }
                    if let Some(x) = e.content.version_key() {
                        self.cache.remove(x);
                    }
                    let _ = fs::remove_file(self.staging_path(&d));
                    let _ = fs::remove_file(self.snapshot_dir.join(&d));
                    st.store.remove_upload(&d)?;
                    self.forget_locked(&mut st, &d)?;
                    count += 1;
                }
            }
        }
        for (x, servers) in blobs {
            for server in servers.iter().chain(self.servers.iter()) {
                if let Err(e) = self.blossom.delete(&self.signer, server, &x).await {
                    tracing::debug!("delete {x} from {server}: {e}");
                }
            }
        }
        Ok(count)
    }

    // ---- File contents ----

    fn staging_path(&self, d: &str) -> PathBuf {
        self.staging_dir.join(d)
    }

    fn local_size(&self, d: &str) -> Option<u64> {
        for p in [self.staging_path(d), self.snapshot_dir.join(d)] {
            if let Ok(m) = fs::metadata(&p) {
                return Some(m.len());
            }
        }
        None
    }

    /// Open a file for writing, preparing its staging file.
    pub async fn open_write(&self, d: &str, truncate: bool) -> Result<File> {
        let staging = self.staging_path(d);
        if !staging.exists() {
            let snapshot = self.snapshot_dir.join(d);
            let content = self.content(d).context(Error::NotFound)?;
            if truncate {
                File::create(&staging)?;
            } else if snapshot.exists() {
                fs::copy(&snapshot, &staging)?;
            } else if content.has_blobs() {
                let size = content.size.unwrap_or(0);
                self.ensure(&content, 0, segment_count(size) - 1).await?;
                let x = content.version_key().unwrap();
                let tmp = self.work_dir.join(format!("copy-{d}"));
                fs::copy(self.cache.data_path(x), &tmp)?;
                fs::rename(&tmp, &staging)?;
            } else {
                File::create(&staging)?;
            }
        }
        let f = OpenOptions::new().read(true).write(true).open(&staging)?;
        if truncate {
            f.set_len(0)?;
        }
        *self
            .open_writers
            .lock()
            .unwrap()
            .entry(d.to_string())
            .or_default() += 1;
        Ok(f)
    }

    /// A writer closed the file. If it changed anything, queue an upload.
    pub fn release_write(&self, d: &str, changed: bool) -> Result<()> {
        let last_writer = {
            let mut writers = self.open_writers.lock().unwrap();
            match writers.get_mut(d) {
                Some(n) if *n > 1 => {
                    *n -= 1;
                    false
                }
                _ => {
                    writers.remove(d);
                    true
                }
            }
        };
        let st = self.st();
        if changed {
            st.store.queue_upload(d, now())?;
        } else if last_writer && !st.store.has_upload(d)? {
            // Opened for writing but never changed: drop the copy, so reads
            // follow the synced version again.
            let _ = fs::remove_file(self.staging_path(d));
        }
        drop(st);
        self.wake_upload.notify_one();
        Ok(())
    }

    pub fn truncate(&self, d: &str, size: u64) -> Result<()> {
        let staging = self.staging_path(d);
        if !staging.exists() && size != 0 {
            bail!(Error::NeedsOpen);
        }
        let f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&staging)?;
        f.set_len(size)?;
        self.st().store.queue_upload(d, now())?;
        self.wake_upload.notify_one();
        Ok(())
    }

    /// Read plaintext bytes from a file.
    pub async fn read(&self, d: &str, offset: u64, len: usize) -> Result<Vec<u8>> {
        for p in [self.staging_path(d), self.snapshot_dir.join(d)] {
            if let Ok(f) = File::open(&p) {
                return read_at(&f, offset, len);
            }
        }
        let content = self.content(d).context(Error::NotFound)?;
        let size = content.size.unwrap_or(0);
        if !content.has_blobs() || offset >= size || len == 0 {
            return Ok(Vec::new());
        }
        let end = (offset + len as u64).min(size);
        let first = offset / SEGMENT;
        let last_needed = (end - 1) / SEGMENT;
        let last = (last_needed + READAHEAD).min(segment_count(size) - 1);
        self.ensure(&content, first, last).await?;
        let x = content.version_key().unwrap();
        self.st().store.touch_cache(x, size, now())?;
        self.cache.read(x, offset, (end - offset) as usize)
    }

    /// Make sure segments `first..=last` of a file version are cached.
    async fn ensure(&self, content: &NodeContent, first: u64, last: u64) -> Result<()> {
        let x = content.version_key().context("file has no content")?.to_string();
        let key = content.file_key().context("unsupported file encryption")?;
        let size = content.size.unwrap_or(0);
        let lock = self
            .fetch_locks
            .lock()
            .unwrap()
            .entry(x.clone())
            .or_default()
            .clone();
        let _guard = lock.lock().await;
        let missing = self.cache.missing(&x, size, first, last)?;
        if missing.is_empty() {
            return Ok(());
        }
        let parts = content.parts.as_ref().unwrap();
        let sizes: Vec<u64> = parts.iter().map(|p| p.size).collect();
        let mut servers = content.servers.clone().unwrap_or_default();
        for s in &self.servers {
            if !servers.contains(s) {
                servers.push(s.clone());
            }
        }
        // Fetch from the first missing segment to the last in one go per part.
        let (lo, hi) = (missing[0], *missing.last().unwrap());
        for span in stream::locate(size, &sizes, lo, hi - lo + 1)? {
            let data = self
                .blossom
                .get_range(&servers, &parts[span.part].x, span.start, span.end)
                .await?;
            let segs = stream::open_run(&key, size, span.first, &data)?;
            self.cache.write_segments(&x, size, span.first, &segs)?;
        }
        let total = self
            .st()
            .store
            .cache_entries()?
            .iter()
            .map(|(_, s)| s)
            .sum::<u64>();
        self.st().store.touch_cache(&x, size, now())?;
        if total > self.cache_limit {
            self.evict(&x)?;
        }
        Ok(())
    }

    fn evict(&self, keep: &str) -> Result<()> {
        let st = self.st();
        let entries = st.store.cache_entries()?;
        let mut total: u64 = entries.iter().map(|(_, s)| s).sum();
        for (x, size) in entries {
            if total <= self.cache_limit {
                break;
            }
            if x == keep {
                continue;
            }
            self.cache.remove(&x);
            st.store.remove_cache(&x)?;
            total -= size;
        }
        Ok(())
    }

    // ---- Background work ----

    pub fn spawn_workers(self: &Arc<Self>) {
        let d = self.clone();
        tokio::spawn(async move { d.publish_loop().await });
        let d = self.clone();
        tokio::spawn(async move { d.upload_loop().await });
        let d = self.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = d.empty_trash(Some(now().saturating_sub(TRASH_RETENTION))).await {
                    tracing::warn!("emptying old trash failed: {e}");
                }
                tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
            }
        });
    }

    async fn publish_loop(&self) {
        let mut backoff = 1;
        loop {
            match self.publish_pending().await {
                Ok(()) => backoff = 1,
                Err(e) => {
                    tracing::warn!("publishing failed: {e}");
                    backoff = (backoff * 2).min(300);
                }
            }
            let _ = tokio::time::timeout(Duration::from_secs(backoff), self.wake_publish.notified()).await;
        }
    }

    /// Publish every node changed locally. Files wait until their content is
    /// uploaded.
    pub async fn publish_pending(&self) -> Result<()> {
        let dirty = self.st().store.dirty_nodes()?;
        for d in dirty {
            let Some(row) = self.st().store.get_node(&d)? else {
                continue;
            };
            if !row.dirty || (row.content.is_file() && !row.content.has_blobs()) {
                continue;
            }
            let json = serde_json::to_value(&row.content)?;
            let created_at = Timestamp::from(row.created_at);
            let node = EventBuilder::new(Kind::Custom(KIND_NODE), self.keys.encrypt(&json.to_string())?)
                .tags([tag("d", &d), tag("j", &self.keys.j), tag("y", &row.y)])
                .custom_created_at(created_at)
                .finalize_unsigned(self.pubkey);
            let node = self.signer.sign(node).await?;
            self.relays.publish(&node).await?;
            self.st()
                .store
                .mark_published(&d, row.created_at, &node.id.to_hex())?;

            let mut version = json;
            version["d"] = d.clone().into();
            version["y"] = row.y.clone().into();
            let version = EventBuilder::new(
                Kind::Custom(KIND_VERSION),
                self.keys.encrypt(&version.to_string())?,
            )
            .tags([tag("j", &self.keys.j), tag("z", &self.keys.node_tag(&d))])
            .custom_created_at(created_at)
            .finalize_unsigned(self.pubkey);
            match self.signer.sign(version).await {
                Ok(v) => {
                    if let Err(e) = self.relays.publish(&v).await {
                        tracing::debug!("version event for {d} not published: {e}");
                    }
                }
                Err(e) => tracing::debug!("version event for {d} not signed: {e}"),
            }
        }
        Ok(())
    }

    async fn upload_loop(&self) {
        let mut retry_at: HashMap<String, u64> = HashMap::new();
        loop {
            let mut next_wake = 30;
            let queue = self.st().store.uploads().unwrap_or_default();
            for (d, queued_at) in queue {
                let t = now();
                if self.open_writers.lock().unwrap().contains_key(&d) {
                    continue;
                }
                if t < queued_at + UPLOAD_DEBOUNCE {
                    next_wake = next_wake.min(queued_at + UPLOAD_DEBOUNCE - t);
                    continue;
                }
                if retry_at.get(&d).is_some_and(|&r| t < r) {
                    next_wake = next_wake.min(retry_at[&d] - t);
                    continue;
                }
                match self.upload(&d, queued_at).await {
                    Ok(()) => {
                        retry_at.remove(&d);
                    }
                    Err(e) => {
                        tracing::warn!("upload of {d} failed: {e}");
                        retry_at.insert(d, t + 60);
                    }
                }
            }
            let _ = tokio::time::timeout(Duration::from_secs(next_wake.max(1)), self.wake_upload.notified())
                .await;
        }
    }

    /// Process every queued upload now, ignoring the debounce. For tests and
    /// one-shot commands.
    pub async fn flush(&self) -> Result<()> {
        let queue = self.st().store.uploads()?;
        for (d, queued_at) in queue {
            self.upload(&d, queued_at).await?;
        }
        self.publish_pending().await
    }

    async fn upload(&self, d: &str, queued_at: u64) -> Result<()> {
        let staging = self.staging_path(d);
        let snapshot = self.snapshot_dir.join(d);
        if staging.exists() {
            fs::rename(&staging, &snapshot)?;
        }
        if !snapshot.exists() || self.content(d).is_none_or(|c| !c.is_file()) {
            self.st().store.remove_upload(d)?;
            let _ = fs::remove_file(&snapshot);
            return Ok(());
        }
        let mtime = fs::metadata(&snapshot)?
            .modified()?
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_else(|_| now());

        let mut key = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut key);
        let work = self.work_dir.join(random_id(8));
        fs::create_dir_all(&work)?;
        let result = async {
            let (src, w) = (snapshot.clone(), work.clone());
            let enc = tokio::task::spawn_blocking(move || {
                stream::encrypt_file(&src, &key, &w, stream::SEGMENTS_PER_PART)
            })
            .await??;
            let mut stored_on: Option<Vec<String>> = None;
            for part in &enc.parts {
                let data = tokio::fs::read(&part.path).await?;
                let mut ok = Vec::new();
                for server in &self.servers {
                    match self
                        .blossom
                        .upload(&self.signer, server, &part.sha256, data.clone())
                        .await
                    {
                        Ok(()) => ok.push(server.clone()),
                        Err(e) => tracing::warn!("{e}"),
                    }
                }
                if ok.is_empty() {
                    bail!("no server accepted part {}", part.sha256);
                }
                stored_on = Some(match stored_on {
                    None => ok,
                    Some(prev) => prev.into_iter().filter(|s| ok.contains(s)).collect(),
                });
            }
            Ok::<_, anyhow::Error>((enc, stored_on.unwrap_or_default()))
        }
        .await;
        let _ = fs::remove_dir_all(&work);
        let (enc, servers) = result?;

        let mut st = self.st();
        let Some(e) = st.tree.get(d).cloned() else {
            st.store.remove_upload(d)?;
            return Ok(());
        };
        let mut content = e.content;
        content.size = Some(enc.size);
        content.sha256 = Some(enc.sha256);
        content.enc = Some(Enc {
            v: 1,
            key: hex::encode(key),
        });
        content.parts = Some(
            enc.parts
                .iter()
                .map(|p| Part {
                    x: p.sha256.clone(),
                    size: p.size,
                })
                .collect(),
        );
        content.servers = Some(servers);
        content.mtime = mtime;
        let x = content.version_key().unwrap().to_string();

        // Keep the plaintext as the new version's cache, unless it was
        // written to again while uploading.
        let requeued = st.store.uploads()?.iter().any(|(u, q)| u == d && *q != queued_at);
        if self.staging_path(d).exists() || requeued {
            fs::copy(&snapshot, self.work_dir.join(format!("adopt-{x}")))?;
            self.cache
                .adopt(&x, enc.size, &self.work_dir.join(format!("adopt-{x}")))?;
        } else {
            self.cache.adopt(&x, enc.size, &snapshot)?;
        }
        st.store.touch_cache(&x, enc.size, now())?;
        if !requeued {
            st.store.remove_upload(d)?;
        }
        self.save_local(&mut st, d, e.y, content)?;
        Ok(())
    }

    pub async fn shutdown(&self) {
        self.relays.shutdown().await;
    }
}

fn read_at(f: &File, offset: u64, len: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    let mut n = 0;
    while n < len {
        match f.read_at(&mut buf[n..], offset + n as u64)? {
            0 => break,
            k => n += k,
        }
    }
    buf.truncate(n);
    Ok(buf)
}

fn guess_mime(name: &str) -> Option<String> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    let mime = match ext.as_str() {
        "txt" | "md" => "text/plain",
        "html" | "htm" => "text/html",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "zip" => "application/zip",
        "json" => "application/json",
        _ => return None,
    };
    Some(mime.to_string())
}

/// Errors the filesystem maps to errno values.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not found")]
    NotFound,
    #[error("already exists")]
    Exists,
    #[error("invalid name")]
    InvalidName,
    #[error("invalid operation")]
    Invalid,
    #[error("folder not empty")]
    NotEmpty,
    #[error("not a folder")]
    NotDir,
    #[error("is a folder")]
    IsDir,
    #[error("file must be opened first")]
    NeedsOpen,
}

async fn find_home_drive(
    signer: &Signer,
    relays: &Relays,
    pubkey: &PublicKey,
) -> Result<Option<(String, DriveKeys)>> {
    let events = relays
        .fetch(
            Filter::new().kind(Kind::Custom(KIND_DRIVE_KEY)).author(*pubkey),
            Duration::from_secs(10),
        )
        .await?;
    let mut best: Option<(u64, String, DriveKeys)> = None;
    for e in events {
        let Some(id) = tag_value(&e, "d").map(str::to_string) else {
            continue;
        };
        let Ok(plain) = signer.decrypt_from_self(&e.content).await else {
            continue;
        };
        let Ok(c) = serde_json::from_str::<DriveKeyContent>(&plain) else {
            continue;
        };
        if !c.home || c.role != "owner" {
            continue;
        }
        let t = e.created_at.as_secs();
        if best.as_ref().is_none_or(|(bt, _, _)| t > *bt) {
            best = Some((t, id, DriveKeys::from_hex(&c.key)?));
        }
    }
    Ok(best.map(|(_, id, k)| (id, k)))
}

async fn create_home_drive(
    signer: &Signer,
    relays: &Relays,
    pubkey: &PublicKey,
) -> Result<(String, DriveKeys)> {
    let keys = DriveKeys::generate();
    let id = random_id(16);
    let content = DriveKeyContent {
        key: keys.dk_hex(),
        name: "My Drive".into(),
        owner: pubkey.to_hex(),
        role: "owner".into(),
        home: true,
        previous: Vec::new(),
        extra: Default::default(),
    };
    let encrypted = signer.encrypt_to_self(&serde_json::to_string(&content)?).await?;
    let unsigned = EventBuilder::new(Kind::Custom(KIND_DRIVE_KEY), encrypted)
        .tags([tag("d", &id)])
        .finalize_unsigned(*pubkey);
    let event = signer.sign(unsigned).await?;
    relays.publish(&event).await.context("publishing the drive key")?;
    Ok((id, keys))
}
