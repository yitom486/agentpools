//! Cooperative turn-activity markers.
//!
//! Long `ask` calls only resolve with the final text, so hosts that enforce
//! first-response / stall timeouts need proof of life while a turn is running.
//! Sessions that observe protocol traffic append markers here; hosts drain
//! them concurrently through the lease. Markers are best-effort telemetry:
//! absence of markers never fails a turn, and runtimes without protocol
//! visibility simply record nothing.

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// One turn-lifecycle marker.
#[derive(Debug, Clone)]
pub struct ActivityEvent {
    /// `turn_started` (carries the native turn id) | `activity` | `turn_ended` | `cancelled`.
    pub kind: &'static str,
    /// Native turn id when known.
    pub turn_id: Option<String>,
    /// Unix millis when recorded.
    pub at_ms: u64,
}

impl ActivityEvent {
    pub fn new(kind: &'static str, turn_id: Option<String>) -> Self {
        Self {
            kind,
            turn_id,
            at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_millis() as u64)
                .unwrap_or(0),
        }
    }

    pub fn to_json_string(&self) -> String {
        let turn = self
            .turn_id
            .as_deref()
            .map(json_escape)
            .map(|escaped| format!("\"{escaped}\""))
            .unwrap_or_else(|| "null".to_owned());
        format!(
            "{{\"kind\":\"{}\",\"turnId\":{},\"atMs\":{}}}",
            self.kind, turn, self.at_ms
        )
    }
}

fn json_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

/// Shared, concurrently drainable activity buffer for one ask.
pub type ActivitySink = Arc<Mutex<Vec<ActivityEvent>>>;

/// Allocate an empty activity buffer shared between a run loop and its host.
pub fn activity_sink() -> ActivitySink {
    Arc::new(Mutex::new(Vec::new()))
}

/// Append a marker; lock poisoning degrades to dropping the marker, never to failing the turn.
pub fn record_activity(sink: &Option<ActivitySink>, kind: &'static str, turn_id: Option<String>) {
    if let Some(sink) = sink
        && let Ok(mut events) = sink.lock()
    {
        events.push(ActivityEvent::new(kind, turn_id));
    }
}

/// Take all buffered markers, leaving the buffer empty.
pub fn drain_activity(sink: &ActivitySink) -> Vec<ActivityEvent> {
    sink.lock()
        .map(|mut events| std::mem::take(&mut *events))
        .unwrap_or_default()
}
