//! Event kinds and the decrypted content of drive events.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const KIND_NODE: u16 = 30440;
pub const KIND_DRIVE_KEY: u16 = 30441;
pub const KIND_DRIVE_INFO: u16 = 30442;
pub const KIND_VERSION: u16 = 1440;
pub const KIND_BLOSSOM_AUTH: u16 = 24242;
pub const KIND_RELAY_LIST: u16 = 10002;
pub const KIND_BLOSSOM_SERVERS: u16 = 10063;

pub const T_DIR: &str = "dir";
pub const T_FILE: &str = "file";
pub const T_MOUNT: &str = "mount";
pub const T_GONE: &str = "gone";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Enc {
    pub v: u32,
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Part {
    pub x: String,
    pub size: u64,
}

/// A node's decrypted content. Unknown fields are kept in `extra` and written
/// back when the node is republished.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct NodeContent {
    pub t: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub mtime: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trashed: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enc: Option<Enc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parts: Option<Vec<Part>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub servers: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drive: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl NodeContent {
    pub fn dir(name: &str, mtime: u64) -> Self {
        Self {
            t: T_DIR.into(),
            name: Some(name.into()),
            mtime,
            ..Default::default()
        }
    }

    pub fn file(name: &str, mtime: u64) -> Self {
        Self {
            t: T_FILE.into(),
            name: Some(name.into()),
            mtime,
            size: Some(0),
            ..Default::default()
        }
    }

    pub fn gone(mtime: u64) -> Self {
        Self {
            t: T_GONE.into(),
            mtime,
            ..Default::default()
        }
    }

    pub fn is_dir(&self) -> bool {
        self.t == T_DIR
    }

    pub fn is_file(&self) -> bool {
        self.t == T_FILE
    }

    pub fn is_gone(&self) -> bool {
        self.t == T_GONE
    }

    /// Whether this node has stored content to read (a file whose blobs exist).
    pub fn has_blobs(&self) -> bool {
        self.enc.is_some() && self.parts.as_ref().is_some_and(|p| !p.is_empty())
    }

    /// The cache key of this version: its first blob.
    pub fn version_key(&self) -> Option<&str> {
        self.parts.as_ref()?.first().map(|p| p.x.as_str())
    }

    pub fn file_key(&self) -> Option<[u8; 32]> {
        let enc = self.enc.as_ref()?;
        if enc.v != 1 {
            return None;
        }
        hex::decode(&enc.key).ok()?.try_into().ok()
    }
}

/// The decrypted content of a drive key event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriveKeyContent {
    pub key: String,
    #[serde(default)]
    pub name: String,
    pub owner: String,
    pub role: String,
    #[serde(default)]
    pub home: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub previous: Vec<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Valid node names.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\0')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_fields_survive() {
        let json = r#"{"t":"file","name":"a","mtime":1,"color":"red","v":3}"#;
        let n: NodeContent = serde_json::from_str(json).unwrap();
        let back: Value = serde_json::to_value(&n).unwrap();
        assert_eq!(back["color"], "red");
        assert_eq!(back["v"], 3);
    }
}
