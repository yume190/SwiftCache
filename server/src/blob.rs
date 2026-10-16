//! Helpers for the `CASBytes` oneof, which carries a blob either inline or as
//! a path on a filesystem shared with the client.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use tokio::fs;

use crate::proto::cas::{cas_bytes, CasBytes};

/// Materialises the blob of an incoming request.
///
/// `file_path` is only usable when the client shares our filesystem (the
/// normal case: the compiler talks to a local daemon over a unix socket).
pub async fn read(bytes: &Option<CasBytes>) -> Result<Vec<u8>> {
    match bytes.as_ref().and_then(|b| b.contents.as_ref()) {
        Some(cas_bytes::Contents::Data(d)) => Ok(d.clone()),
        Some(cas_bytes::Contents::FilePath(p)) => fs::read(p)
            .await
            .with_context(|| format!("reading client blob file {p}")),
        None => Ok(Vec::new()),
    }
}

/// Size of an already-inlined blob, for logging. `file_path` blobs report 0:
/// the proxy inlines before asking, so this is only hit for empty payloads.
pub fn len(bytes: &Option<CasBytes>) -> usize {
    match bytes.as_ref().and_then(|b| b.contents.as_ref()) {
        Some(cas_bytes::Contents::Data(d)) => d.len(),
        _ => 0,
    }
}

/// Where `write_to_disk` responses are materialised, if enabled.
pub struct BlobDir {
    dir: PathBuf,
    counter: AtomicU64,
}

impl BlobDir {
    pub async fn new(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir)
            .await
            .with_context(|| format!("creating blob dir {}", dir.display()))?;
        Ok(BlobDir {
            dir: dir.to_path_buf(),
            counter: AtomicU64::new(0),
        })
    }

    async fn materialise(&self, data: &[u8]) -> Result<String> {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let path = self.dir.join(format!("{}-{n}.blob", std::process::id()));
        fs::write(&path, data)
            .await
            .with_context(|| format!("writing {}", path.display()))?;
        // The client is allowed to move the file away, so it must be writable
        // by it; same-uid is sufficient for the local-daemon topology.
        Ok(path.to_string_lossy().into_owned())
    }
}

/// Builds the `CASBytes` for a response.
///
/// `write_to_disk` is a hint, not a requirement: the proto explicitly allows
/// returning inline bytes anyway, which is what we do when no blob directory
/// is configured.
pub async fn respond(
    data: Vec<u8>,
    write_to_disk: bool,
    blob_dir: Option<&BlobDir>,
) -> Result<CasBytes> {
    if write_to_disk {
        if let Some(bd) = blob_dir {
            let path = bd.materialise(&data).await?;
            return Ok(CasBytes {
                contents: Some(cas_bytes::Contents::FilePath(path)),
            });
        }
    }
    Ok(CasBytes {
        contents: Some(cas_bytes::Contents::Data(data)),
    })
}
