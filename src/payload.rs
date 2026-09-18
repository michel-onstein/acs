//! The payload set (DESIGN §8.1): slim acs builds for the remote targets,
//! gzip-compressed, carried inside a complete binary so any copy can install
//! acs on any supported remote.
//!
//! Layout of a payload blob:
//!
//! ```text
//! [gz entry 0][gz entry 1]...[index][index_len u64][blob_len u64]["ACSPAY01"]
//! index = count u32, then per entry:
//!         target (u16 len + bytes), offset u64, gz_len u64, raw_len u64, sha256 [32]
//! ```
//!
//! ELF builds carry the blob appended after the executable (the loader
//! ignores it); Mach-O builds link it in with `include_bytes!` because
//! code signing refuses appended data.

use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

const MAGIC: &[u8; 8] = b"ACSPAY01";
const FOOTER: usize = 8 + 8 + 8;

/// The target triple this binary was built for.
pub const OWN_TARGET: &str = env!("ACS_TARGET");

#[cfg(feature = "embed-payloads")]
static EMBEDDED: &[u8] = include_bytes!(env!("ACS_PAYLOADS_FILE"));
#[cfg(not(feature = "embed-payloads"))]
static EMBEDDED: &[u8] = &[];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub target: String,
    offset: u64,
    pub gz_len: u64,
    pub raw_len: u64,
    /// SHA-256 of the uncompressed slim binary.
    pub sha256: [u8; 32],
}

/// A parsed payload blob.
#[derive(Debug, Clone)]
pub struct Payloads {
    blob: Vec<u8>,
    pub entries: Vec<Entry>,
}

/// What goes into a blob: target, uncompressed slim binary, its gzip form.
pub struct Input<'a> {
    pub target: &'a str,
    pub raw: &'a [u8],
    pub gz: &'a [u8],
}

/// Assemble a payload blob.
pub fn build(inputs: &[Input<'_>]) -> Vec<u8> {
    let mut blob = Vec::new();
    let mut index = Vec::new();
    index.extend_from_slice(&(inputs.len() as u32).to_be_bytes());
    for i in inputs {
        let offset = blob.len() as u64;
        blob.extend_from_slice(i.gz);
        index.extend_from_slice(&(i.target.len() as u16).to_be_bytes());
        index.extend_from_slice(i.target.as_bytes());
        index.extend_from_slice(&offset.to_be_bytes());
        index.extend_from_slice(&(i.gz.len() as u64).to_be_bytes());
        index.extend_from_slice(&(i.raw.len() as u64).to_be_bytes());
        index.extend_from_slice(&crate::sha256::digest(i.raw));
    }
    let index_len = index.len() as u64;
    blob.extend_from_slice(&index);
    let total = (blob.len() + FOOTER) as u64;
    blob.extend_from_slice(&index_len.to_be_bytes());
    blob.extend_from_slice(&total.to_be_bytes());
    blob.extend_from_slice(MAGIC);
    blob
}

fn be_u64(b: &[u8]) -> u64 {
    u64::from_be_bytes(b[..8].try_into().unwrap())
}

impl Payloads {
    /// Parse a blob. Anything malformed is `None`, never a panic.
    pub fn parse(blob: &[u8]) -> Option<Payloads> {
        let (index_len, total) = footer(blob)?;
        if total != blob.len() as u64 {
            return None;
        }
        let index_end = blob.len() - FOOTER;
        let index_start = index_end.checked_sub(index_len as usize)?;
        let mut p = &blob[index_start..index_end];
        let take = |p: &mut &[u8], n: usize| -> Option<Vec<u8>> {
            if p.len() < n {
                return None;
            }
            let (a, b) = p.split_at(n);
            *p = b;
            Some(a.to_vec())
        };
        let count = u32::from_be_bytes(take(&mut p, 4)?.try_into().ok()?);
        let mut entries = Vec::new();
        for _ in 0..count {
            let tlen = u16::from_be_bytes(take(&mut p, 2)?.try_into().ok()?) as usize;
            let target = String::from_utf8(take(&mut p, tlen)?).ok()?;
            let offset = be_u64(&take(&mut p, 8)?);
            let gz_len = be_u64(&take(&mut p, 8)?);
            let raw_len = be_u64(&take(&mut p, 8)?);
            let sha256: [u8; 32] = take(&mut p, 32)?.try_into().ok()?;
            if offset.checked_add(gz_len)? > index_start as u64 {
                return None;
            }
            entries.push(Entry {
                target,
                offset,
                gz_len,
                raw_len,
                sha256,
            });
        }
        Some(Payloads {
            blob: blob.to_vec(),
            entries,
        })
    }

    /// The payloads this binary carries (embedded or appended), if any.
    pub fn from_self() -> Option<Payloads> {
        if !EMBEDDED.is_empty() {
            return Payloads::parse(EMBEDDED);
        }
        let exe = crate::sys::self_exe().ok()?;
        Payloads::from_file(&exe).ok().flatten().map(|(_, p)| p)
    }

    /// Read a blob appended to `path`: `(length of what precedes it, blob)`.
    pub fn from_file(path: &Path) -> io::Result<Option<(u64, Payloads)>> {
        let mut f = std::fs::File::open(path)?;
        let len = f.metadata()?.len();
        if len < FOOTER as u64 {
            return Ok(None);
        }
        f.seek(SeekFrom::End(-(FOOTER as i64)))?;
        let mut foot = [0u8; FOOTER];
        f.read_exact(&mut foot)?;
        let Some((_, total)) = footer(&foot) else {
            return Ok(None);
        };
        if total > len {
            return Ok(None);
        }
        f.seek(SeekFrom::Start(len - total))?;
        let mut blob = vec![0u8; total as usize];
        f.read_exact(&mut blob)?;
        Ok(Payloads::parse(&blob).map(|p| (len - total, p)))
    }

    pub fn get(&self, target: &str) -> Option<(&Entry, &[u8])> {
        let e = self.entries.iter().find(|e| e.target == target)?;
        let start = e.offset as usize;
        Some((e, &self.blob[start..start + e.gz_len as usize]))
    }

    /// The whole blob, as appended to a slim ELF binary to complete it.
    pub fn blob(&self) -> &[u8] {
        &self.blob
    }

    pub fn targets(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.target.as_str()).collect()
    }
}

fn footer(b: &[u8]) -> Option<(u64, u64)> {
    if b.len() < FOOTER || &b[b.len() - 8..] != MAGIC {
        return None;
    }
    let f = &b[b.len() - FOOTER..];
    Some((be_u64(&f[0..8]), be_u64(&f[8..16])))
}

/// This binary without any appended payloads: `(bytes, was_complete)`.
pub fn own_slim_bytes() -> io::Result<(Vec<u8>, bool)> {
    let exe = crate::sys::self_exe()?;
    let all = std::fs::read(&exe)?;
    match Payloads::from_file(&exe)? {
        Some((slim_len, _)) => Ok((all[..slim_len as usize].to_vec(), true)),
        None => Ok((all, !EMBEDDED.is_empty())),
    }
}

/// Map a remote's `uname -s` / `uname -m` to a target triple.
pub fn target_for_uname(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("Linux", "x86_64" | "amd64") => Some("x86_64-unknown-linux-musl"),
        ("Linux", "aarch64" | "arm64") => Some("aarch64-unknown-linux-musl"),
        ("Linux", "armv7l" | "armv7") => Some("armv7-unknown-linux-musleabihf"),
        ("Darwin", "arm64") => Some("aarch64-apple-darwin"),
        ("Darwin", "x86_64") => Some("x86_64-apple-darwin"),
        _ => None,
    }
}

/// Is this an ELF target (payloads appended) rather than Mach-O?
pub fn is_elf_target(target: &str) -> bool {
    target.contains("-linux-")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn sample() -> Vec<u8> {
        build(&[
            Input {
                target: "x86_64-unknown-linux-musl",
                raw: b"slim-x86",
                gz: b"GZ-X86-BYTES",
            },
            Input {
                target: "aarch64-unknown-linux-musl",
                raw: b"slim-arm-longer",
                gz: b"GZ-ARM",
            },
        ])
    }

    #[test]
    fn build_and_parse_round_trip() {
        let p = Payloads::parse(&sample()).unwrap();
        assert_eq!(
            p.targets(),
            ["x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl"]
        );
        let (e, gz) = p.get("aarch64-unknown-linux-musl").unwrap();
        assert_eq!(gz, b"GZ-ARM");
        assert_eq!(e.raw_len, 15);
        assert_eq!(e.sha256, crate::sha256::digest(b"slim-arm-longer"));
        assert!(p.get("riscv64").is_none());
        assert_eq!(p.blob(), &sample()[..]);
    }

    #[test]
    fn appended_trailer_is_found_behind_a_binary() {
        let t = TempDir::new();
        let f = t.path().join("acs");
        let mut data = b"\x7fELF...pretend executable...".to_vec();
        let slim_len = data.len() as u64;
        data.extend_from_slice(&sample());
        std::fs::write(&f, &data).unwrap();
        let (len, p) = Payloads::from_file(&f).unwrap().unwrap();
        assert_eq!(len, slim_len);
        assert_eq!(p.entries.len(), 2);
    }

    #[test]
    fn corrupt_or_missing_trailer_means_slim() {
        let t = TempDir::new();
        let f = t.path().join("acs");
        std::fs::write(&f, b"just a binary").unwrap();
        assert!(Payloads::from_file(&f).unwrap().is_none());

        let mut bad = sample();
        let n = bad.len();
        bad[n - 1] ^= 0xff; // magic
        assert!(Payloads::parse(&bad).is_none());

        let mut bad = sample();
        let n = bad.len();
        bad[n - 9] = 0xff; // total length
        assert!(Payloads::parse(&bad).is_none());

        // Truncated anywhere: never a panic.
        let s = sample();
        for cut in 0..s.len() {
            let _ = Payloads::parse(&s[cut..]);
            let _ = Payloads::parse(&s[..cut]);
        }
        let mut huge = sample();
        let n = huge.len();
        huge[n - 24..n - 16].copy_from_slice(&u64::MAX.to_be_bytes());
        assert!(Payloads::parse(&huge).is_none());
    }

    #[test]
    fn uname_mapping() {
        assert_eq!(
            target_for_uname("Linux", "x86_64"),
            Some("x86_64-unknown-linux-musl")
        );
        assert_eq!(
            target_for_uname("Linux", "aarch64"),
            Some("aarch64-unknown-linux-musl")
        );
        assert_eq!(
            target_for_uname("Darwin", "arm64"),
            Some("aarch64-apple-darwin")
        );
        assert_eq!(target_for_uname("SunOS", "i86pc"), None);
        assert!(is_elf_target("aarch64-unknown-linux-musl"));
        assert!(!is_elf_target("aarch64-apple-darwin"));
    }

    #[test]
    fn this_build_knows_its_target() {
        assert!(OWN_TARGET.contains('-'), "{OWN_TARGET}");
    }
}
