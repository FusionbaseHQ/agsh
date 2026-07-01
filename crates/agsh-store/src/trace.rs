use std::collections::VecDeque;

use agsh_core::CommandId;

/// A captured command's exact stdout/stderr, addressable by `trace://<id>/...`.
#[derive(Debug, Clone)]
pub struct TraceRecord {
    pub cmd_id: String,
    pub command: String,
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl TraceRecord {
    pub fn new(
        cmd_id: &CommandId,
        command: impl Into<String>,
        exit_code: i32,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    ) -> Self {
        Self {
            cmd_id: cmd_id.to_string(),
            command: command.into(),
            exit_code,
            stdout,
            stderr,
        }
    }
}

/// Which stream of a trace to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceStream {
    Stdout,
    Stderr,
}

/// A bounded, in-memory ring of recent command traces. Raw bytes are preserved
/// exactly so `trace://` references resolve to the unmodified output.
#[derive(Debug)]
pub struct TraceStore {
    records: VecDeque<TraceRecord>,
    capacity: usize,
}

impl Default for TraceStore {
    fn default() -> Self {
        Self::with_capacity(200)
    }
}

impl TraceStore {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            records: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    pub fn record(&mut self, record: TraceRecord) {
        while self.records.len() >= self.capacity {
            self.records.pop_front();
        }
        self.records.push_back(record);
    }

    pub fn get(&self, cmd_id: &str) -> Option<&TraceRecord> {
        self.records.iter().rev().find(|r| r.cmd_id == cmd_id)
    }

    pub fn records(&self) -> impl Iterator<Item = &TraceRecord> {
        self.records.iter()
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Resolve a `trace://<id>/stdout`, `<id>/stderr`, or bare `<id>` reference
    /// to the captured bytes. A bare id defaults to stdout.
    pub fn resolve(&self, reference: &str) -> Option<&[u8]> {
        let (id, stream) = parse_trace_ref(reference);
        let record = self.get(id)?;
        Some(match stream {
            TraceStream::Stdout => &record.stdout,
            TraceStream::Stderr => &record.stderr,
        })
    }
}

/// Parse a trace reference into (id, stream). Accepts `trace://id/stdout`,
/// `trace://id/stderr`, `id/stdout`, `id/stderr`, or a bare `id` (stdout).
pub fn parse_trace_ref(reference: &str) -> (&str, TraceStream) {
    let body = reference.strip_prefix("trace://").unwrap_or(reference);
    if let Some(id) = body.strip_suffix("/stderr") {
        (id, TraceStream::Stderr)
    } else if let Some(id) = body.strip_suffix("/stdout") {
        (id, TraceStream::Stdout)
    } else {
        (body, TraceStream::Stdout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, stdout: &str, stderr: &str) -> TraceRecord {
        TraceRecord {
            cmd_id: id.to_string(),
            command: "cmd".to_string(),
            exit_code: 0,
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn resolves_references() {
        let mut store = TraceStore::default();
        store.record(record("abc", "out", "err"));
        assert_eq!(store.resolve("trace://abc/stdout"), Some(b"out".as_ref()));
        assert_eq!(store.resolve("trace://abc/stderr"), Some(b"err".as_ref()));
        assert_eq!(store.resolve("abc"), Some(b"out".as_ref()));
        assert_eq!(store.resolve("trace://missing/stdout"), None);
    }

    #[test]
    fn bounded_ring_drops_oldest() {
        let mut store = TraceStore::with_capacity(2);
        store.record(record("a", "1", ""));
        store.record(record("b", "2", ""));
        store.record(record("c", "3", ""));
        assert_eq!(store.len(), 2);
        assert!(store.get("a").is_none());
        assert!(store.get("c").is_some());
    }
}
