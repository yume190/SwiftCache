//! On-disk storage for the CAS objects and the action cache (key/value) table.
//!
//! Layout under `--store`:
//!
//! ```text
//! <root>/objects/<aa>/<full-hex>     CAS object  (refs + data, see encode())
//! <root>/kv/<aa>/<full-hex>          action cache entry (encoded `kv.Value`)
//! <root>/tmp/                        staging for atomic rename
//! ```
//!
//! Both tables are append-only and content-addressed, so concurrent writers of
//! the same id race harmlessly: the loser's rename simply overwrites identical
//! bytes.

use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use tokio::fs;

use crate::hash::hash_object;
use crate::peer::Peer;

const MAGIC: &[u8; 6] = b"SWCAS\x01";

#[derive(Debug, Default)]
pub struct Stats {
    pub cas_put: AtomicU64,
    pub cas_get_hit: AtomicU64,
    pub cas_get_miss: AtomicU64,
    pub kv_put: AtomicU64,
    pub kv_get_hit: AtomicU64,
    pub kv_get_miss: AtomicU64,
    pub bytes_in: AtomicU64,
    pub bytes_out: AtomicU64,
    /// Clients seen since the last session report, so a summary can name who
    /// caused the traffic. Contention is irrelevant: one lock per RPC.
    peers: Mutex<BTreeSet<Peer>>,
}

/// A point-in-time reading of the counters. Sessions are reported as the
/// difference between two of these, never as a running total.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    pub cas_put: u64,
    pub cas_get_hit: u64,
    pub cas_get_miss: u64,
    pub kv_put: u64,
    pub kv_get_hit: u64,
    pub kv_get_miss: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

impl Counters {
    pub fn delta(&self, base: &Counters) -> Counters {
        Counters {
            cas_put: self.cas_put - base.cas_put,
            cas_get_hit: self.cas_get_hit - base.cas_get_hit,
            cas_get_miss: self.cas_get_miss - base.cas_get_miss,
            kv_put: self.kv_put - base.kv_put,
            kv_get_hit: self.kv_get_hit - base.kv_get_hit,
            kv_get_miss: self.kv_get_miss - base.kv_get_miss,
            bytes_in: self.bytes_in - base.bytes_in,
            bytes_out: self.bytes_out - base.bytes_out,
        }
    }

    pub fn summary(&self) -> String {
        format!(
            "kv: {} hit / {} miss / {} put | cas: {} hit / {} miss / {} put | {} up, {} down",
            self.kv_get_hit,
            self.kv_get_miss,
            self.kv_put,
            self.cas_get_hit,
            self.cas_get_miss,
            self.cas_put,
            human_bytes(self.bytes_in),
            human_bytes(self.bytes_out),
        )
    }
}

impl Stats {
    pub fn snapshot(&self) -> Counters {
        let l = |c: &AtomicU64| c.load(Ordering::Relaxed);
        Counters {
            cas_put: l(&self.cas_put),
            cas_get_hit: l(&self.cas_get_hit),
            cas_get_miss: l(&self.cas_get_miss),
            kv_put: l(&self.kv_put),
            kv_get_hit: l(&self.kv_get_hit),
            kv_get_miss: l(&self.kv_get_miss),
            bytes_in: l(&self.bytes_in),
            bytes_out: l(&self.bytes_out),
        }
    }

    pub fn summary(&self) -> String {
        self.snapshot().summary()
    }

    pub fn note_peer(&self, peer: Peer) {
        if peer != Peer::Unknown {
            self.peers.lock().insert(peer);
        }
    }

    /// Drains the client list; the next session starts with a clean slate.
    pub fn take_peers(&self) -> Vec<Peer> {
        std::mem::take(&mut *self.peers.lock())
            .into_iter()
            .collect()
    }
}

/// Byte counts in a build run span bytes to gigabytes; integer MiB rounded
/// everything small to "0 MiB".
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

pub struct Store {
    objects: PathBuf,
    kv: PathBuf,
    tmp: PathBuf,
    /// Staging-file counter. A timestamp is not unique enough: concurrent
    /// writers routinely land in the same clock tick, and the loser's rename
    /// then fails with ENOENT because the winner already moved the file away.
    staging_seq: AtomicU64,
    pub stats: Stats,
}

/// A CAS object: opaque blob plus an ordered list of references to other
/// objects. References are the raw digest bytes previously returned by
/// `Put`/`Save`.
pub struct Object {
    pub data: Vec<u8>,
    pub refs: Vec<Vec<u8>>,
}

impl Store {
    pub async fn open(root: &Path) -> Result<Self> {
        let store = Store {
            objects: root.join("objects"),
            kv: root.join("kv"),
            tmp: root.join("tmp"),
            staging_seq: AtomicU64::new(0),
            stats: Stats::default(),
        };
        for d in [&store.objects, &store.kv, &store.tmp] {
            fs::create_dir_all(d)
                .await
                .with_context(|| format!("creating {}", d.display()))?;
        }
        Ok(store)
    }

    fn shard(base: &Path, id: &[u8]) -> PathBuf {
        let hex = hex::encode(id);
        base.join(&hex[..2.min(hex.len())]).join(hex)
    }

    /// Writes `bytes` to `dest`, publishing with a rename so readers never see
    /// a partial record.
    ///
    /// The store is documented as safe to prune from the outside, and a `rm
    /// -rf` of `objects/` / `tmp/` under a running server used to turn every
    /// later request into a bare `No such file or directory`. Recreate the
    /// directories and retry once instead.
    async fn write_atomic(&self, dest: &Path, bytes: &[u8]) -> Result<()> {
        let staging = self.tmp.join(format!(
            "{}.{}",
            std::process::id(),
            self.staging_seq.fetch_add(1, Ordering::Relaxed)
        ));
        if let Err(e) = fs::write(&staging, bytes).await {
            if e.kind() != ErrorKind::NotFound {
                return Err(e).with_context(|| format!("staging {}", staging.display()));
            }
            fs::create_dir_all(&self.tmp)
                .await
                .with_context(|| format!("recreating {}", self.tmp.display()))?;
            fs::write(&staging, bytes)
                .await
                .with_context(|| format!("staging {}", staging.display()))?;
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        match fs::rename(&staging, dest).await {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = fs::remove_file(&staging).await;
                Err(e).with_context(|| format!("publishing {}", dest.display()))
            }
        }
    }

    // ---- CAS ------------------------------------------------------------

    /// Stores `data`/`refs` and returns the LLVM-compatible digest.
    pub async fn put_object(&self, refs: Vec<Vec<u8>>, data: Vec<u8>) -> Result<Vec<u8>> {
        let id = hash_object(&refs, &data);
        let path = Self::shard(&self.objects, &id);
        self.stats
            .bytes_in
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        self.stats.cas_put.fetch_add(1, Ordering::Relaxed);
        if fs::metadata(&path).await.is_ok() {
            return Ok(id.to_vec());
        }
        self.write_atomic(&path, &encode(&refs, &data)).await?;
        Ok(id.to_vec())
    }

    pub async fn get_object(&self, id: &[u8]) -> Result<Option<Object>> {
        let path = Self::shard(&self.objects, id);
        let raw = match fs::read(&path).await {
            Ok(b) => b,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                self.stats.cas_get_miss.fetch_add(1, Ordering::Relaxed);
                return Ok(None);
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let obj = decode(&raw).with_context(|| format!("decoding {}", path.display()))?;
        self.stats.cas_get_hit.fetch_add(1, Ordering::Relaxed);
        self.stats
            .bytes_out
            .fetch_add(obj.data.len() as u64, Ordering::Relaxed);
        Ok(Some(obj))
    }

    // ---- Action cache ---------------------------------------------------

    pub async fn put_value(&self, key: &[u8], encoded_value: &[u8]) -> Result<()> {
        self.stats.kv_put.fetch_add(1, Ordering::Relaxed);
        self.write_atomic(&Self::shard(&self.kv, key), encoded_value)
            .await
    }

    pub async fn get_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let path = Self::shard(&self.kv, key);
        match fs::read(&path).await {
            Ok(b) => {
                self.stats.kv_get_hit.fetch_add(1, Ordering::Relaxed);
                Ok(Some(b))
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {
                self.stats.kv_get_miss.fetch_add(1, Ordering::Relaxed);
                Ok(None)
            }
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }
}

fn encode(refs: &[Vec<u8>], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        MAGIC.len() + 4 + refs.iter().map(|r| r.len() + 4).sum::<usize>() + 8 + data.len(),
    );
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(refs.len() as u32).to_le_bytes());
    for r in refs {
        out.extend_from_slice(&(r.len() as u32).to_le_bytes());
        out.extend_from_slice(r);
    }
    out.extend_from_slice(&(data.len() as u64).to_le_bytes());
    out.extend_from_slice(data);
    out
}

fn decode(raw: &[u8]) -> Result<Object> {
    let mut p = 0usize;
    let take = |p: &mut usize, n: usize| -> Result<&[u8]> {
        if raw.len() < *p + n {
            bail!("truncated CAS object record");
        }
        let s = &raw[*p..*p + n];
        *p += n;
        Ok(s)
    };
    if take(&mut p, MAGIC.len())? != MAGIC {
        bail!("bad CAS object magic");
    }
    let ref_count = u32::from_le_bytes(take(&mut p, 4)?.try_into().unwrap()) as usize;
    let mut refs = Vec::with_capacity(ref_count);
    for _ in 0..ref_count {
        let len = u32::from_le_bytes(take(&mut p, 4)?.try_into().unwrap()) as usize;
        refs.push(take(&mut p, len)?.to_vec());
    }
    let data_len = u64::from_le_bytes(take(&mut p, 8)?.try_into().unwrap()) as usize;
    let data = take(&mut p, data_len)?.to_vec();
    Ok(Object { data, refs })
}

/// CAS ids are opaque to the protocol: the toolchain plugin uses its own
/// digest scheme (`luxon.blake2b`, 65 bytes) while our own `Put`/`Save`
/// responses use LLVM's builtin BLAKE3 (32 bytes). We only reject lengths that
/// cannot be a digest, so a malformed request becomes a clean
/// `OBJECT_NOT_FOUND` rather than a filesystem probe with a caller-chosen
/// name. 80 bytes is the cap LLVM's own gRPC relay CAS imposes.
pub fn valid_id(id: &[u8]) -> bool {
    !id.is_empty() && id.len() <= 80
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_record() {
        let refs = vec![vec![7u8; 32], vec![9u8; 32]];
        let obj = decode(&encode(&refs, b"payload")).unwrap();
        assert_eq!(obj.data, b"payload");
        assert_eq!(obj.refs, refs);
    }

    #[test]
    fn roundtrip_empty() {
        let obj = decode(&encode(&[], b"")).unwrap();
        assert!(obj.data.is_empty());
        assert!(obj.refs.is_empty());
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode(b"nope").is_err());
        assert!(decode(b"SWCAS\x01\x05\x00\x00\x00").is_err());
    }
}
