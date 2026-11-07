//! File encryption, format version 1: AES-256-GCM STREAM over 64 KiB segments,
//! with the ciphertext cut into parts on segment boundaries.

use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, bail};
use sha2::{Digest, Sha256};

use crate::Result;

pub const SEGMENT: u64 = 65536;
pub const TAG: u64 = 16;
pub const SEALED: u64 = SEGMENT + TAG;
pub const SEGMENTS_PER_PART: u64 = 1024;

/// Number of segments in a file of `size` plaintext bytes.
pub fn segment_count(size: u64) -> u64 {
    size / SEGMENT + 1
}

/// Ciphertext length of a file of `size` plaintext bytes.
pub fn ciphertext_len(size: u64) -> u64 {
    size + segment_count(size) * TAG
}

/// Plaintext length of segment `index` in a file of `size` bytes.
pub fn segment_len(size: u64, index: u64) -> u64 {
    if index + 1 < segment_count(size) {
        SEGMENT
    } else {
        size % SEGMENT
    }
}

fn nonce(counter: u64, last: bool) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[3..11].copy_from_slice(&counter.to_be_bytes());
    n[11] = last as u8;
    n
}

fn cipher(key: &[u8; 32]) -> Aes256Gcm {
    Aes256Gcm::new_from_slice(key).expect("32-byte key")
}

pub fn seal_segment(key: &[u8; 32], index: u64, last: bool, plaintext: &[u8]) -> Vec<u8> {
    cipher(key)
        .encrypt(Nonce::from_slice(&nonce(index, last)), plaintext)
        .expect("AES-GCM encryption can't fail")
}

pub fn open_segment(key: &[u8; 32], index: u64, last: bool, sealed: &[u8]) -> Result<Vec<u8>> {
    cipher(key)
        .decrypt(Nonce::from_slice(&nonce(index, last)), sealed)
        .map_err(|_| anyhow::anyhow!("segment {index} failed authentication"))
}

/// Where a run of sealed segments lives in a file's parts.
#[derive(Debug, PartialEq)]
pub struct Span {
    pub part: usize,
    /// First segment index of the run.
    pub first: u64,
    /// Number of segments in the run.
    pub count: u64,
    /// Byte range within the part, inclusive start, exclusive end.
    pub start: u64,
    pub end: u64,
}

/// Split segments `first..first+count` of a file of `size` bytes, stored in
/// parts of the given ciphertext sizes, into per-part byte ranges.
pub fn locate(size: u64, part_sizes: &[u64], first: u64, count: u64) -> Result<Vec<Span>> {
    let total = segment_count(size);
    if first + count > total {
        bail!("segments {first}..{} out of range ({total})", first + count);
    }
    let mut spans = Vec::new();
    let mut part_start_seg = 0u64;
    let mut seg = first;
    let end_seg = first + count;
    for (i, &psize) in part_sizes.iter().enumerate() {
        let is_last_part = i + 1 == part_sizes.len();
        let segs_in_part = if is_last_part {
            total - part_start_seg
        } else {
            if psize % SEALED != 0 {
                bail!("part {i} is not a whole number of segments");
            }
            psize / SEALED
        };
        let part_end_seg = part_start_seg + segs_in_part;
        if seg < part_end_seg && seg < end_seg {
            let run_end = end_seg.min(part_end_seg);
            let start = (seg - part_start_seg) * SEALED;
            let last_len = segment_len(size, run_end - 1) + TAG;
            let end = (run_end - 1 - part_start_seg) * SEALED + last_len;
            if end > psize {
                bail!("part {i} is shorter than its segments");
            }
            spans.push(Span {
                part: i,
                first: seg,
                count: run_end - seg,
                start,
                end,
            });
            seg = run_end;
        }
        part_start_seg = part_end_seg;
        if seg >= end_seg {
            break;
        }
    }
    if seg < end_seg {
        bail!("parts don't cover segment {seg}");
    }
    Ok(spans)
}

/// Decrypt a run of sealed segments starting at segment `first`.
pub fn open_run(key: &[u8; 32], size: u64, first: u64, data: &[u8]) -> Result<Vec<Vec<u8>>> {
    let total = segment_count(size);
    let mut out = Vec::new();
    let mut offset = 0usize;
    let mut index = first;
    while offset < data.len() {
        if index >= total {
            bail!("too much ciphertext");
        }
        let len = (segment_len(size, index) + TAG) as usize;
        let sealed = data.get(offset..offset + len).context("truncated segment run")?;
        out.push(open_segment(key, index, index + 1 == total, sealed)?);
        offset += len;
        index += 1;
    }
    Ok(out)
}

pub struct EncryptedPart {
    pub path: PathBuf,
    pub sha256: String,
    pub size: u64,
}

pub struct Encrypted {
    pub parts: Vec<EncryptedPart>,
    /// Plaintext sha256 and size.
    pub sha256: String,
    pub size: u64,
}

fn read_full(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// Encrypt `src` into part files in `out_dir`, never holding more than two
/// segments in memory.
pub fn encrypt_file(src: &Path, key: &[u8; 32], out_dir: &Path, segments_per_part: u64) -> Result<Encrypted> {
    let mut input = File::open(src).with_context(|| format!("opening {}", src.display()))?;
    let mut plain_hash = Sha256::new();
    let mut plain_size = 0u64;
    let mut parts = Vec::new();

    let mut cur = vec![0u8; SEGMENT as usize];
    let mut next = vec![0u8; SEGMENT as usize];
    let mut cur_len = read_full(&mut input, &mut cur)?;
    let mut index = 0u64;

    let mut writer: Option<(BufWriter<File>, Sha256, u64, PathBuf)> = None;
    loop {
        // A full segment is never the last: the last is always shorter.
        let last = cur_len < SEGMENT as usize;
        let next_len = if last {
            0
        } else {
            read_full(&mut input, &mut next)?
        };
        plain_hash.update(&cur[..cur_len]);
        plain_size += cur_len as u64;
        let sealed = seal_segment(key, index, last, &cur[..cur_len]);

        if writer.is_none() {
            let path = out_dir.join(format!("part-{}", parts.len()));
            writer = Some((BufWriter::new(File::create(&path)?), Sha256::new(), 0, path));
        }
        let (w, h, n, _) = writer.as_mut().unwrap();
        w.write_all(&sealed)?;
        h.update(&sealed);
        *n += sealed.len() as u64;

        if last || (index + 1).is_multiple_of(segments_per_part) {
            let (mut w, h, n, path) = writer.take().unwrap();
            w.flush()?;
            parts.push(EncryptedPart {
                path,
                sha256: hex::encode(h.finalize()),
                size: n,
            });
        }
        if last {
            break;
        }
        std::mem::swap(&mut cur, &mut next);
        cur_len = next_len;
        index += 1;
    }
    Ok(Encrypted {
        parts,
        sha256: hex::encode(plain_hash.finalize()),
        size: plain_size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(size: usize, per_part: u64) {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let data: Vec<u8> = (0..size).map(|i| (i * 31 % 251) as u8).collect();
        std::fs::write(&src, &data).unwrap();
        let key = [9u8; 32];
        let out = dir.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let enc = encrypt_file(&src, &key, &out, per_part).unwrap();
        assert_eq!(enc.size, size as u64);
        let total: u64 = enc.parts.iter().map(|p| p.size).sum();
        assert_eq!(total, ciphertext_len(size as u64));
        let sizes: Vec<u64> = enc.parts.iter().map(|p| p.size).collect();
        let blobs: Vec<Vec<u8>> = enc
            .parts
            .iter()
            .map(|p| std::fs::read(&p.path).unwrap())
            .collect();

        // Whole file.
        let count = segment_count(size as u64);
        let mut plain = Vec::new();
        for span in locate(size as u64, &sizes, 0, count).unwrap() {
            let run = &blobs[span.part][span.start as usize..span.end as usize];
            for seg in open_run(&key, size as u64, span.first, run).unwrap() {
                plain.extend(seg);
            }
        }
        assert_eq!(plain, data);

        // Each segment on its own.
        for i in 0..count {
            let spans = locate(size as u64, &sizes, i, 1).unwrap();
            assert_eq!(spans.len(), 1);
            let s = &spans[0];
            let segs = open_run(
                &key,
                size as u64,
                i,
                &blobs[s.part][s.start as usize..s.end as usize],
            )
            .unwrap();
            let start = (i * SEGMENT) as usize;
            assert_eq!(segs[0], data[start..start + segment_len(size as u64, i) as usize]);
        }
    }

    #[test]
    fn sizes() {
        for size in [0, 1, 65535, 65536, 65537, 131072, 300_000] {
            roundtrip(size, SEGMENTS_PER_PART);
            roundtrip(size, 1);
            roundtrip(size, 2);
        }
    }

    #[test]
    fn segment_counts() {
        assert_eq!(segment_count(0), 1);
        assert_eq!(segment_count(65535), 1);
        assert_eq!(segment_count(65536), 2);
        assert_eq!(segment_len(65536, 1), 0);
    }

    #[test]
    fn tampering_is_detected() {
        let key = [1u8; 32];
        let mut s = seal_segment(&key, 0, true, b"hello");
        assert!(open_segment(&key, 0, true, &s).is_ok());
        assert!(open_segment(&key, 1, true, &s).is_err(), "wrong position");
        assert!(open_segment(&key, 0, false, &s).is_err(), "truncation flag");
        s[0] ^= 1;
        assert!(open_segment(&key, 0, true, &s).is_err());
    }
}
