//! Helpers for the `CASBytes` oneof, which carries a blob either inline or as
//! a path on a filesystem shared with the client.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use tokio::fs;

static RESPONSE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

use crate::proto::cas::{cas_bytes, CasBytes};

/// Materialises the blob of an incoming request, taking ownership so inline
/// bytes are moved rather than copied.
///
/// `file_path` is only usable when the client shares our filesystem (the
/// normal case: the compiler talks to a local daemon over a unix socket).
pub async fn read(bytes: Option<CasBytes>) -> Result<Vec<u8>> {
    match bytes.and_then(|b| b.contents) {
        Some(cas_bytes::Contents::Data(d)) => Ok(d),
        Some(cas_bytes::Contents::FilePath(p)) => fs::read(&p)
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
}

impl BlobDir {
    pub async fn new(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir)
            .await
            .with_context(|| format!("creating blob dir {}", dir.display()))?;
        Ok(BlobDir { dir: dir.to_path_buf() })
    }

    /// Writes `data` to a fresh file in one blocking job (tokio's async file
    /// would copy it through a 2 MiB buffer and schedule a job per chunk).
    /// A failed write removes the partial file.
    async fn materialise(&self, data: Vec<u8>) -> Result<String> {
        let dir = self.dir.clone();
        tokio::task::spawn_blocking(move || loop {
            let n = RESPONSE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = dir.join(format!("{}-{n}.blob", std::process::id()));
            let mut file = match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => file,
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err).with_context(|| format!("creating {}", path.display())),
            };
            // std::fs::File is unbuffered: write errors (ENOSPC) surface here.
            // No fsync; the client consumes the file immediately.
            let written = file.write_all(&data);
            drop(file);
            if let Err(err) = written {
                let _ = std::fs::remove_file(&path);
                return Err(err).with_context(|| format!("writing {}", path.display()));
            }
            return Ok(path.to_string_lossy().into_owned());
        })
        .await
        .context("blob writer task")?
    }
}

/// Builds the `CASBytes` for a response.
///
/// `write_to_disk` is a hint, not a requirement: the proto explicitly allows
/// returning inline bytes anyway, which is what we do when no blob directory
/// is configured.
pub async fn respond(data: Vec<u8>, write_to_disk: bool, blob_dir: Option<&BlobDir>) -> Result<CasBytes> {
    if write_to_disk {
        if let Some(bd) = blob_dir {
            let path = bd.materialise(data).await?;
            return Ok(CasBytes {
                contents: Some(cas_bytes::Contents::FilePath(path)),
            });
        }
    }
    Ok(CasBytes {
        contents: Some(cas_bytes::Contents::Data(data)),
    })
}
