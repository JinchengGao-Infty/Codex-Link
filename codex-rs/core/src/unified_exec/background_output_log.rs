use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;

/// Append-only durable output log for background unified-exec processes.
#[derive(Clone, Debug)]
pub(crate) struct BackgroundOutputLog {
    path: Arc<PathBuf>,
    written_bytes: Arc<AtomicU64>,
}

impl BackgroundOutputLog {
    pub(crate) async fn create(path: PathBuf) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        Ok(Self {
            path: Arc::new(path),
            written_bytes: Arc::new(AtomicU64::new(0)),
        })
    }

    pub(crate) fn path(&self) -> &Path {
        self.path.as_path()
    }

    pub(crate) async fn append(&self, chunk: &[u8]) {
        let result = async {
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.path.as_ref())
                .await?;
            file.write_all(chunk).await
        }
        .await;

        if let Err(err) = result {
            tracing::warn!(
                path = %self.path.display(),
                "failed to append unified exec background output log: {err}"
            );
        } else {
            self.written_bytes
                .fetch_add(chunk.len() as u64, Ordering::AcqRel);
        }
    }
}
