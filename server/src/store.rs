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
use std::io::{ErrorKind, Write};
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

    pub fn note_peer(&self, peer: Peer) {
        if peer != Peer::Unknown {
            self.peers.lock().insert(peer);
        }
    }

    /// Drains the client list; the next session starts with a clean slate.
    pub fn take_peers(&self) -> Vec<Peer> {
        std::mem::take(&mut *self.peers.lock()).into_iter().collect()
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

    /// A unique staging file name. A timestamp is not unique enough (see
    /// `staging_seq`).
    fn staging(&self) -> PathBuf {
        self.tmp.join(format!(
            "{}.{}",
            std::process::id(),
            self.staging_seq.fetch_add(1, Ordering::Relaxed)
        ))
    }

    // ---- CAS ------------------------------------------------------------

    /// Stores `data`/`refs` and returns the LLVM-compatible digest. Hashing
    /// and the write run as one blocking job, off the async workers.
    pub async fn put_object(&self, refs: Vec<Vec<u8>>, data: Vec<u8>) -> Result<Vec<u8>> {
        let (objects, tmp, staging) = (self.objects.clone(), self.tmp.clone(), self.staging());
        let len = data.len() as u64;
        let id = tokio::task::spawn_blocking(move || -> Result<_> {
            let id = hash_object(&refs, &data);
            let path = Self::shard(&objects, &id);
            if !path.exists() {
                write_atomic(&tmp, &staging, &path, &[&encode_header(&refs, data.len()), &data])?;
            }
            Ok(id)
        })
        .await
        .context("store task")??;
        self.stats.bytes_in.fetch_add(len, Ordering::Relaxed);
        self.stats.cas_put.fetch_add(1, Ordering::Relaxed);
        Ok(id.to_vec())
    }

    pub async fn get_object(&self, id: &[u8]) -> Result<Option<Object>> {
        let Some(obj) = read_record(&Self::shard(&self.objects, id)).await? else {
            self.stats.cas_get_miss.fetch_add(1, Ordering::Relaxed);
            return Ok(None);
        };
        self.stats.cas_get_hit.fetch_add(1, Ordering::Relaxed);
        self.stats.bytes_out.fetch_add(obj.data.len() as u64, Ordering::Relaxed);
        Ok(Some(obj))
    }

    // ---- Read-through cache (proxy) --------------------------------------
    //
    // A proxy stores what its upstream returned under the upstream's own id,
    // which it cannot recompute (ids are opaque). Complete objects live at
    // `objects/<aa>/<hex>`; a `CAS.Load` response carries no references, so
    // it is kept apart at `objects/<aa>/<hex>.blob` and can never satisfy a
    // later `CAS.Get`. A complete object does satisfy `Load`.

    fn blob_path(&self, id: &[u8]) -> PathBuf {
        let mut p = Self::shard(&self.objects, id).into_os_string();
        p.push(".blob");
        p.into()
    }

    /// Data of `id` for `CAS.Load`: a complete object, else a blob-only record.
    pub async fn get_blob(&self, id: &[u8]) -> Result<Option<Vec<u8>>> {
        let found = match read_record(&Self::shard(&self.objects, id)).await? {
            Some(obj) => Some(obj),
            None => read_record(&self.blob_path(id)).await?,
        };
        let Some(obj) = found else {
            self.stats.cas_get_miss.fetch_add(1, Ordering::Relaxed);
            return Ok(None);
        };
        self.stats.cas_get_hit.fetch_add(1, Ordering::Relaxed);
        self.stats.bytes_out.fetch_add(obj.data.len() as u64, Ordering::Relaxed);
        Ok(Some(obj.data))
    }

    /// Caches a complete object under an externally assigned `id`. `data` is
    /// moved through the blocking write and handed back, never copied.
    pub async fn cache_object(&self, id: &[u8], refs: Vec<Vec<u8>>, data: Vec<u8>) -> Result<Vec<u8>> {
        let (tmp, staging, dest) = (self.tmp.clone(), self.staging(), Self::shard(&self.objects, id));
        tokio::task::spawn_blocking(move || -> Result<_> {
            if !dest.exists() {
                write_atomic(&tmp, &staging, &dest, &[&encode_header(&refs, data.len()), &data])?;
            }
            Ok(data)
        })
        .await
        .context("store task")?
    }

    /// Caches a `CAS.Load` result (data without references) under `id`.
    pub async fn cache_blob(&self, id: &[u8], data: Vec<u8>) -> Result<Vec<u8>> {
        let (tmp, staging) = (self.tmp.clone(), self.staging());
        let (object, dest) = (Self::shard(&self.objects, id), self.blob_path(id));
        tokio::task::spawn_blocking(move || -> Result<_> {
            if !object.exists() && !dest.exists() {
                write_atomic(&tmp, &staging, &dest, &[&encode_header(&[], data.len()), &data])?;
            }
            Ok(data)
        })
        .await
        .context("store task")?
    }

    /// Writes an upload to a staging file before the upstream id is known.
    /// `data` is moved through the blocking write and handed back. Dropping
    /// the [`Staged`] without [`Staged::commit`] removes the file.
    pub async fn stage(&self, refs: Vec<Vec<u8>>, data: Vec<u8>) -> Result<(Staged, Vec<u8>)> {
        let (tmp, staging, objects) = (self.tmp.clone(), self.staging(), self.objects.clone());
        tokio::task::spawn_blocking(move || -> Result<_> {
            stage_file(&tmp, &staging, &[&encode_header(&refs, data.len()), &data])?;
            Ok((
                Staged {
                    objects,
                    path: Some(staging),
                },
                data,
            ))
        })
        .await
        .context("store task")?
    }

    // ---- Action cache ---------------------------------------------------

    pub async fn put_value(&self, key: &[u8], encoded_value: Vec<u8>) -> Result<()> {
        self.stats.kv_put.fetch_add(1, Ordering::Relaxed);
        let (tmp, staging, dest) = (self.tmp.clone(), self.staging(), Self::shard(&self.kv, key));
        tokio::task::spawn_blocking(move || write_atomic(&tmp, &staging, &dest, &[&encoded_value]))
            .await
            .context("store task")?
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

/// A fully written, not yet published complete object (see [`Store::stage`]).
pub struct Staged {
    objects: PathBuf,
    path: Option<PathBuf>,
}

impl Staged {
    /// Atomically publishes the staged object under `id`.
    pub async fn commit(mut self, id: &[u8]) -> Result<()> {
        let staging = self.path.take().expect("staged file present until commit");
        let dest = Store::shard(&self.objects, id);
        tokio::task::spawn_blocking(move || {
            if dest.exists() {
                let _ = std::fs::remove_file(&staging);
                return Ok(());
            }
            publish(&staging, &dest)
        })
        .await
        .context("store task")?
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Reads and decodes one record; `None` when it does not exist.
async fn read_record(path: &Path) -> Result<Option<Object>> {
    let raw = match fs::read(path).await {
        Ok(b) => b,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    decode(raw)
        .map(Some)
        .with_context(|| format!("decoding {}", path.display()))
}

/// Writes `parts` to `staging`, then publishes it at `dest` with a rename so
/// readers never see a partial record. Blocking.
///
/// The store is documented as safe to prune from the outside, and a `rm -rf`
/// of `objects/` / `tmp/` under a running server used to turn every later
/// request into a bare `No such file or directory`. Missing directories are
/// recreated (only when a write actually hits ENOENT) and the step retried.
fn write_atomic(tmp: &Path, staging: &Path, dest: &Path, parts: &[&[u8]]) -> Result<()> {
    stage_file(tmp, staging, parts)?;
    publish(staging, dest)
}

/// First half of [`write_atomic`]: writes `parts` to `staging`. Blocking.
fn stage_file(tmp: &Path, staging: &Path, parts: &[&[u8]]) -> Result<()> {
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::File::create(staging)?;
        for part in parts {
            f.write_all(part)?;
        }
        Ok(())
    };
    let staged = match write() {
        Err(e) if e.kind() == ErrorKind::NotFound => std::fs::create_dir_all(tmp).and_then(|()| write()),
        other => other,
    };
    if let Err(e) = staged {
        let _ = std::fs::remove_file(staging);
        return Err(e).with_context(|| format!("staging {}", staging.display()));
    }
    Ok(())
}

/// Second half of [`write_atomic`]: renames `staging` onto `dest`, removing
/// `staging` if that fails. Blocking.
fn publish(staging: &Path, dest: &Path) -> Result<()> {
    let published = match std::fs::rename(staging, dest) {
        Err(e) if e.kind() == ErrorKind::NotFound => match dest.parent() {
            Some(parent) => std::fs::create_dir_all(parent).and_then(|()| std::fs::rename(staging, dest)),
            None => Err(e),
        },
        other => other,
    };
    if let Err(e) = published {
        let _ = std::fs::remove_file(staging);
        return Err(e).with_context(|| format!("publishing {}", dest.display()));
    }
    Ok(())
}

/// Record header; the data follows it verbatim (written separately, so the
/// payload is never copied into a combined buffer).
fn encode_header(refs: &[Vec<u8>], data_len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(MAGIC.len() + 4 + refs.iter().map(|r| r.len() + 4).sum::<usize>() + 8);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(refs.len() as u32).to_le_bytes());
    for r in refs {
        out.extend_from_slice(&(r.len() as u32).to_le_bytes());
        out.extend_from_slice(r);
    }
    out.extend_from_slice(&(data_len as u64).to_le_bytes());
    out
}

/// Parses a record, reusing `raw`'s allocation for the data (the header is
/// shifted out in place instead of copying the data into a new buffer).
fn decode(mut raw: Vec<u8>) -> Result<Object> {
    let mut p = 0usize;
    let take = |raw: &[u8], p: &mut usize, n: usize| -> Result<std::ops::Range<usize>> {
        if raw.len() < *p + n {
            bail!("truncated CAS object record");
        }
        *p += n;
        Ok(*p - n..*p)
    };
    if raw[take(&raw, &mut p, MAGIC.len())?] != MAGIC[..] {
        bail!("bad CAS object magic");
    }
    let le32 = |b: &[u8]| u32::from_le_bytes(b.try_into().unwrap()) as usize;
    let ref_count = le32(&raw[take(&raw, &mut p, 4)?]);
    let mut refs = Vec::with_capacity(ref_count.min(1024));
    for _ in 0..ref_count {
        let len = le32(&raw[take(&raw, &mut p, 4)?]);
        refs.push(raw[take(&raw, &mut p, len)?].to_vec());
    }
    let data_len = u64::from_le_bytes(raw[take(&raw, &mut p, 8)?].try_into().unwrap()) as usize;
    let data = take(&raw, &mut p, data_len)?;
    raw.truncate(data.end);
    raw.drain(..data.start);
    Ok(Object { data: raw, refs })
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

    fn record(refs: &[Vec<u8>], data: &[u8]) -> Vec<u8> {
        [encode_header(refs, data.len()).as_slice(), data].concat()
    }

    #[test]
    fn roundtrip_record() {
        let refs = vec![vec![7u8; 32], vec![9u8; 32]];
        let obj = decode(record(&refs, b"payload")).unwrap();
        assert_eq!(obj.data, b"payload");
        assert_eq!(obj.refs, refs);
    }

    #[test]
    fn roundtrip_empty() {
        let obj = decode(record(&[], b"")).unwrap();
        assert!(obj.data.is_empty());
        assert!(obj.refs.is_empty());
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode(b"nope".to_vec()).is_err());
        assert!(decode(b"SWCAS\x01\x05\x00\x00\x00".to_vec()).is_err());
    }
}
