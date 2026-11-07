//! Local state in SQLite: the latest version of every node, which nodes still
//! need publishing, the upload queue, and the read cache index.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use crate::Result;
use crate::model::NodeContent;

#[derive(Debug, Clone)]
pub struct NodeRow {
    pub d: String,
    pub created_at: u64,
    /// The published event, or `None` if this version exists only locally.
    pub event_id: Option<String>,
    pub author: String,
    pub y: String,
    pub content: NodeContent,
    /// Changed locally and not yet published.
    pub dirty: bool,
}

pub struct Store {
    db: Connection,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS nodes (
    d TEXT PRIMARY KEY,
    created_at INTEGER NOT NULL,
    event_id TEXT,
    author TEXT NOT NULL,
    y TEXT NOT NULL,
    content TEXT NOT NULL,
    dirty INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS nodes_dirty ON nodes(dirty) WHERE dirty = 1;
CREATE TABLE IF NOT EXISTS uploads (d TEXT PRIMARY KEY, queued_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS cache (x TEXT PRIMARY KEY, size INTEGER NOT NULL, last_access INTEGER NOT NULL);
";

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path)?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "NORMAL")?;
        db.execute_batch(SCHEMA)?;
        Ok(Self { db })
    }

    pub fn get_meta(&self, k: &str) -> Result<Option<String>> {
        Ok(self
            .db
            .query_row("SELECT v FROM meta WHERE k = ?", [k], |r| r.get(0))
            .optional()?)
    }

    pub fn set_meta(&self, k: &str, v: &str) -> Result<()> {
        self.db
            .execute("INSERT OR REPLACE INTO meta (k, v) VALUES (?, ?)", [k, v])?;
        Ok(())
    }

    fn row(r: &rusqlite::Row) -> rusqlite::Result<NodeRow> {
        let content: String = r.get(5)?;
        Ok(NodeRow {
            d: r.get(0)?,
            created_at: r.get::<_, i64>(1)? as u64,
            event_id: r.get(2)?,
            author: r.get(3)?,
            y: r.get(4)?,
            content: serde_json::from_str(&content).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(e))
            })?,
            dirty: r.get(6)?,
        })
    }

    pub fn all_nodes(&self) -> Result<Vec<NodeRow>> {
        let mut stmt = self
            .db
            .prepare("SELECT d, created_at, event_id, author, y, content, dirty FROM nodes")?;
        Ok(stmt
            .query_map([], Self::row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn get_node(&self, d: &str) -> Result<Option<NodeRow>> {
        Ok(self
            .db
            .query_row(
                "SELECT d, created_at, event_id, author, y, content, dirty FROM nodes WHERE d = ?",
                [d],
                Self::row,
            )
            .optional()?)
    }

    pub fn put_node(&self, n: &NodeRow) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO nodes (d, created_at, event_id, author, y, content, dirty)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![
                n.d,
                n.created_at as i64,
                n.event_id,
                n.author,
                n.y,
                serde_json::to_string(&n.content)?,
                n.dirty
            ],
        )?;
        Ok(())
    }

    pub fn delete_node(&self, d: &str) -> Result<()> {
        self.db.execute("DELETE FROM nodes WHERE d = ?", [d])?;
        Ok(())
    }

    pub fn dirty_nodes(&self) -> Result<Vec<String>> {
        let mut stmt = self.db.prepare("SELECT d FROM nodes WHERE dirty = 1")?;
        Ok(stmt
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Record that the version created at `created_at` was published, unless it
    /// has changed again since.
    pub fn mark_published(&self, d: &str, created_at: u64, event_id: &str) -> Result<()> {
        self.db.execute(
            "UPDATE nodes SET dirty = 0, event_id = ? WHERE d = ? AND created_at = ?",
            params![event_id, d, created_at as i64],
        )?;
        Ok(())
    }

    pub fn queue_upload(&self, d: &str, at: u64) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO uploads (d, queued_at) VALUES (?, ?)",
            params![d, at as i64],
        )?;
        Ok(())
    }

    pub fn uploads(&self) -> Result<Vec<(String, u64)>> {
        let mut stmt = self
            .db
            .prepare("SELECT d, queued_at FROM uploads ORDER BY queued_at")?;
        Ok(stmt
            .query_map([], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64)))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn has_upload(&self, d: &str) -> Result<bool> {
        Ok(self
            .db
            .query_row("SELECT 1 FROM uploads WHERE d = ?", [d], |_| Ok(()))
            .optional()?
            .is_some())
    }

    pub fn remove_upload(&self, d: &str) -> Result<()> {
        self.db.execute("DELETE FROM uploads WHERE d = ?", [d])?;
        Ok(())
    }

    pub fn touch_cache(&self, x: &str, size: u64, now: u64) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO cache (x, size, last_access) VALUES (?, ?, ?)",
            params![x, size as i64, now as i64],
        )?;
        Ok(())
    }

    /// Cache entries, least recently used first.
    pub fn cache_entries(&self) -> Result<Vec<(String, u64)>> {
        let mut stmt = self
            .db
            .prepare("SELECT x, size FROM cache ORDER BY last_access")?;
        Ok(stmt
            .query_map([], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64)))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn remove_cache(&self, x: &str) -> Result<()> {
        self.db.execute("DELETE FROM cache WHERE x = ?", [x])?;
        Ok(())
    }
}
