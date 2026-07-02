//! Durable sidecar persistence for Link context state.
//!
//! One JSON file per thread under `<codex_home>/link/context/`, written
//! atomically (temp file + rename) after each recorded mutation. This store is
//! deliberately Link-owned and outside Codex's native session schema so the
//! capsule, background-job table, and pending callback events survive process
//! restarts and thread resume, not just transcript compaction.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use crate::BackgroundTriggerEvent;
use crate::LinkContextState;
use crate::NativeBackgroundJob;

// v2: `verified_evidence` became typed `EvidenceRecord`s. v1 files fail to
// deserialize and are discarded with a warning rather than migrated; the
// format never shipped beyond this fork.
pub(crate) const PERSISTED_LINK_CONTEXT_VERSION: u32 = 2;

/// Serialized snapshot of everything `LinkContextStore` tracks in memory.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct PersistedLinkContext {
    pub(crate) version: u32,
    pub(crate) state: LinkContextState,
    pub(crate) background_jobs: BTreeMap<String, NativeBackgroundJob>,
    pub(crate) pending_background_events: Vec<BackgroundTriggerEvent>,
    pub(crate) background_event_keys: Vec<String>,
}

pub(crate) fn link_context_file_path(codex_home: &Path, thread_id: &str) -> PathBuf {
    codex_home
        .join("link")
        .join("context")
        .join(format!("{thread_id}.json"))
}

/// Loads a persisted snapshot, returning `None` when the file is absent or
/// unreadable. Corruption is downgraded to a warning: losing recorded context
/// must never block the thread from starting.
pub(crate) fn load(path: &Path) -> Option<PersistedLinkContext> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            tracing::warn!("failed to read Link context file {}: {err}", path.display());
            return None;
        }
    };
    match serde_json::from_slice::<PersistedLinkContext>(&bytes) {
        Ok(persisted) => Some(persisted),
        Err(err) => {
            tracing::warn!(
                "failed to parse Link context file {}: {err}",
                path.display()
            );
            None
        }
    }
}

/// Writes the snapshot atomically. Failures are downgraded to warnings so a
/// full disk or permission problem degrades to memory-only tracking instead of
/// failing tool calls.
pub(crate) fn save(path: &Path, persisted: &PersistedLinkContext) {
    let json = match serde_json::to_vec_pretty(persisted) {
        Ok(json) => json,
        Err(err) => {
            tracing::warn!(
                "failed to serialize Link context for {}: {err}",
                path.display()
            );
            return;
        }
    };
    if let Some(parent) = path.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        tracing::warn!(
            "failed to create Link context directory {}: {err}",
            parent.display()
        );
        return;
    }
    let tmp_path = path.with_extension("json.tmp");
    if let Err(err) = std::fs::write(&tmp_path, &json) {
        tracing::warn!(
            "failed to write Link context file {}: {err}",
            tmp_path.display()
        );
        return;
    }
    if let Err(err) = std::fs::rename(&tmp_path, path) {
        tracing::warn!(
            "failed to install Link context file {}: {err}",
            path.display()
        );
    }
}
