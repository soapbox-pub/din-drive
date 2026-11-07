//! Decrypted file contents, cached segment by segment.
//!
//! Each file version (named by its first blob) is a sparse plaintext file plus a
//! map with one byte per segment, set once that segment has been fetched and
//! authenticated.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use crate::Result;
use crate::stream::{SEGMENT, segment_count};

pub struct Cache {
    dir: PathBuf,
}

impl Cache {
    pub fn new(dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    pub fn data_path(&self, x: &str) -> PathBuf {
        self.dir.join(x)
    }

    fn map_path(&self, x: &str) -> PathBuf {
        self.dir.join(format!("{x}.map"))
    }

    fn open_map(&self, x: &str, size: u64) -> Result<File> {
        let path = self.map_path(x);
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        let n = segment_count(size);
        if f.metadata()?.len() < n {
            f.set_len(n)?;
        }
        Ok(f)
    }

    /// Segments in `first..=last` that aren't cached yet.
    pub fn missing(&self, x: &str, size: u64, first: u64, last: u64) -> Result<Vec<u64>> {
        if !self.data_path(x).exists() {
            return Ok((first..=last).collect());
        }
        let mut map = self.open_map(x, size)?;
        let mut flags = vec![0u8; (last - first + 1) as usize];
        map.seek(SeekFrom::Start(first))?;
        map.read_exact(&mut flags)?;
        Ok(flags
            .iter()
            .enumerate()
            .filter(|(_, f)| **f == 0)
            .map(|(i, _)| first + i as u64)
            .collect())
    }

    pub fn write_segments(&self, x: &str, size: u64, first: u64, segments: &[Vec<u8>]) -> Result<()> {
        let data = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.data_path(x))?;
        if data.metadata()?.len() != size {
            data.set_len(size)?;
        }
        for (i, seg) in segments.iter().enumerate() {
            data.write_all_at(seg, (first + i as u64) * SEGMENT)?;
        }
        data.sync_data()?;
        let map = self.open_map(x, size)?;
        map.write_all_at(&vec![1u8; segments.len()], first)?;
        Ok(())
    }

    pub fn read(&self, x: &str, offset: u64, len: usize) -> Result<Vec<u8>> {
        let f = File::open(self.data_path(x))?;
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

    /// Take a complete plaintext file as the cached copy of version `x`.
    pub fn adopt(&self, x: &str, size: u64, plaintext: &Path) -> Result<()> {
        fs::rename(plaintext, self.data_path(x))?;
        let mut map = File::create(self.map_path(x))?;
        map.write_all(&vec![1u8; segment_count(size) as usize])?;
        Ok(())
    }

    pub fn remove(&self, x: &str) {
        let _ = fs::remove_file(self.data_path(x));
        let _ = fs::remove_file(self.map_path(x));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments() {
        let dir = tempfile::tempdir().unwrap();
        let c = Cache::new(dir.path().to_path_buf()).unwrap();
        let size = SEGMENT * 2 + 10;
        assert_eq!(c.missing("x", size, 0, 2).unwrap(), vec![0, 1, 2]);
        c.write_segments("x", size, 1, &[vec![7u8; SEGMENT as usize]])
            .unwrap();
        assert_eq!(c.missing("x", size, 0, 2).unwrap(), vec![0, 2]);
        assert_eq!(c.read("x", SEGMENT, 3).unwrap(), vec![7, 7, 7]);
    }
}
