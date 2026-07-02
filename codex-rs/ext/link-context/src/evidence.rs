//! Typed evidence records with provenance for the Link context capsule.
//!
//! Evidence is stored as small typed records instead of prose lines so each
//! fact keeps its provenance: what kind of fact it is, whether the host
//! observed it or the model reported it, which turn/call/process produced it,
//! and whether it predates the current process (`stale`). The capsule renders
//! records as bounded one-liners.

use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    Decision,
    #[default]
    Observation,
    PlanUpdate,
    Blocker,
    TestResult,
    ToolResult,
    FileChange,
    JobEvent,
}

impl EvidenceKind {
    fn label(self) -> &'static str {
        match self {
            EvidenceKind::Decision => "decision",
            EvidenceKind::Observation => "observation",
            EvidenceKind::PlanUpdate => "plan",
            EvidenceKind::Blocker => "blocker",
            EvidenceKind::TestResult => "test",
            EvidenceKind::ToolResult => "tool",
            EvidenceKind::FileChange => "file",
            EvidenceKind::JobEvent => "job",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource {
    #[default]
    HostObserved,
    ModelReported,
}

impl EvidenceSource {
    fn label(self) -> &'static str {
        match self {
            EvidenceSource::HostObserved => "host",
            EvidenceSource::ModelReported => "model",
        }
    }
}

/// How much the record should be trusted before re-verification. Host-observed
/// records default to `High`; model-reported records default to `Medium`
/// because the model may misremember or overstate what it verified.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceConfidence {
    High,
    #[default]
    Medium,
    Low,
}

impl EvidenceConfidence {
    fn label(self) -> &'static str {
        match self {
            EvidenceConfidence::High => "high",
            EvidenceConfidence::Medium => "medium",
            EvidenceConfidence::Low => "low",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EvidenceRecord {
    pub kind: EvidenceKind,
    pub source: EvidenceSource,
    pub summary: String,
    /// Turn, call, or process identifier that produced this record.
    pub source_ref: Option<String>,
    pub related_paths: Vec<String>,
    pub confidence: EvidenceConfidence,
    /// Host clock at recording time (Unix millis); informational only.
    pub created_at_ms: Option<i64>,
    /// Set when the record predates the current process. Stale facts must be
    /// re-verified against live state before they are load-bearing.
    pub stale: bool,
}

impl EvidenceRecord {
    pub fn host(kind: EvidenceKind, summary: impl Into<String>) -> Self {
        Self {
            kind,
            source: EvidenceSource::HostObserved,
            summary: summary.into(),
            source_ref: None,
            related_paths: Vec::new(),
            confidence: EvidenceConfidence::High,
            created_at_ms: now_ms(),
            stale: false,
        }
    }

    pub fn model(kind: EvidenceKind, summary: impl Into<String>) -> Self {
        Self {
            source: EvidenceSource::ModelReported,
            confidence: EvidenceConfidence::Medium,
            ..Self::host(kind, summary)
        }
    }

    pub fn with_confidence(mut self, confidence: EvidenceConfidence) -> Self {
        self.confidence = confidence;
        self
    }

    pub fn with_source_ref(mut self, source_ref: impl Into<String>) -> Self {
        self.source_ref = Some(source_ref.into());
        self
    }

    pub fn with_related_paths(mut self, paths: Vec<String>) -> Self {
        self.related_paths = paths;
        self
    }

    /// Identity used for deduplication; deliberately ignores `created_at_ms`
    /// and `stale` so re-recording the same fact does not duplicate it.
    pub(crate) fn dedup_key(&self) -> (EvidenceKind, &str, Option<&str>) {
        (self.kind, self.summary.as_str(), self.source_ref.as_deref())
    }

    /// Renders the record as one capsule line (unbounded; the capsule renderer
    /// applies char and token limits).
    pub(crate) fn render(&self) -> String {
        let mut tags = format!("{}, {}", self.kind.label(), self.source.label());
        if self.confidence != EvidenceConfidence::High {
            tags.push_str(&format!(", conf={}", self.confidence.label()));
        }
        if self.stale {
            tags.push_str(", stale");
        }
        let mut out = format!("[{tags}] {}", self.summary);
        if let Some(source_ref) = self.source_ref.as_deref() {
            out.push_str(&format!(" ({source_ref})"));
        }
        if !self.related_paths.is_empty() {
            out.push_str(&format!(" paths: {}", self.related_paths.join(", ")));
        }
        out
    }
}

/// Appends a record unless an equivalent fact is already present, dropping the
/// oldest records beyond `max_len`.
pub(crate) fn push_evidence(
    records: &mut Vec<EvidenceRecord>,
    record: EvidenceRecord,
    max_len: usize,
) {
    if record.summary.trim().is_empty() {
        return;
    }
    if records
        .iter()
        .any(|existing| existing.dedup_key() == record.dedup_key())
    {
        return;
    }
    records.push(record);
    let overflow = records.len().saturating_sub(max_len);
    if overflow > 0 {
        records.drain(0..overflow);
    }
}

fn now_ms() -> Option<i64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
}
