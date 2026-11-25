// SPDX-License-Identifier: AGPL-3.0-only

//! The server's recent log, kept in memory for the console and the API:
//! the events since this start (as many as fit), each with a rising id
//! to follow from. The system's journal keeps what goes before.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use serde::Serialize;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

/// Events kept at most, and their text in all.
const MAX_EVENTS: usize = 5000;
const MAX_BYTES: usize = 4 << 20;
/// One event's text at most.
const MAX_LINE: usize = 4096;

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub id: u64,
    /// Milliseconds since 1970.
    pub time: i64,
    /// error, warn, info, debug, trace
    pub level: &'static str,
    pub message: String,
}

struct Ring {
    started: i64,
    next: AtomicU64,
    kept: Mutex<(VecDeque<Entry>, usize)>,
}

fn ring() -> &'static Ring {
    static RING: OnceLock<Ring> = OnceLock::new();
    RING.get_or_init(|| Ring {
        started: now_ms(),
        next: AtomicU64::new(1),
        kept: Mutex::new((VecDeque::new(), 0)),
    })
}

fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

fn level_name(l: &Level) -> &'static str {
    match *l {
        Level::ERROR => "error",
        Level::WARN => "warn",
        Level::INFO => "info",
        Level::DEBUG => "debug",
        Level::TRACE => "trace",
    }
}

/// How severe: error 0 .. trace 4.
fn rank(level: &str) -> u8 {
    match level {
        "error" => 0,
        "warn" => 1,
        "info" => 2,
        "debug" => 3,
        _ => 4,
    }
}

/// The event's message, then its other fields as name=value.
#[derive(Default)]
struct Text {
    message: String,
    fields: String,
}

impl Visit for Text {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            let _ = write!(self.fields, " {}={value:?}", field.name());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            let _ = write!(self.fields, " {}={value:?}", field.name());
        }
    }
}

/// Puts the events the subscriber lets through into the ring.
pub struct RingLayer;

/// The layer to add to the server's subscriber (after its filter).
pub fn layer() -> RingLayer {
    ring();
    RingLayer
}

impl<S: Subscriber> Layer<S> for RingLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut t = Text::default();
        event.record(&mut t);
        let mut message = t.message;
        message.push_str(&t.fields);
        push(level_name(event.metadata().level()), message);
    }
}

fn push(level: &'static str, mut message: String) {
    if message.len() > MAX_LINE {
        let mut end = MAX_LINE;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
        message.push('…');
    }
    let r = ring();
    let e = Entry {
        id: r.next.fetch_add(1, Ordering::Relaxed),
        time: now_ms(),
        level,
        message,
    };
    let mut k = r.kept.lock().unwrap_or_else(|e| e.into_inner());
    k.1 += e.message.len();
    k.0.push_back(e);
    while k.0.len() > MAX_EVENTS || k.1 > MAX_BYTES {
        match k.0.pop_front() {
            Some(old) => k.1 -= old.message.len(),
            None => break,
        }
    }
}

/// What a read of the log gives.
#[derive(Debug, Clone, Serialize)]
pub struct Read {
    /// When this server started keeping its log (milliseconds); a
    /// follower that sees it change has missed a restart.
    pub started: i64,
    /// The oldest event still kept (0: none).
    pub oldest: u64,
    /// Newest last.
    pub entries: Vec<Entry>,
}

/// The newest `limit` events after `after`, at `level` or more severe,
/// with `q` in their text (any case).
pub fn read(after: u64, level: &str, q: &str, limit: usize) -> Read {
    let r = ring();
    let max = rank(level);
    let q = q.trim().to_lowercase();
    let k = r.kept.lock().unwrap_or_else(|e| e.into_inner());
    let mut entries: Vec<Entry> =
        k.0.iter()
            .rev()
            .filter(|e| e.id > after)
            .filter(|e| rank(e.level) <= max)
            .filter(|e| q.is_empty() || e.message.to_lowercase().contains(&q))
            .take(limit)
            .cloned()
            .collect();
    entries.reverse();
    Read {
        started: r.started,
        oldest: k.0.front().map_or(0, |e| e.id),
        entries,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_kept_filtered_and_followed() {
        push("info", "listener up".into());
        push("warn", "debian: upstream slow".into());
        push("error", "debian: sync failed".into());
        push("info", "x".repeat(MAX_LINE + 10));
        let all = read(0, "info", "", 100);
        let n = all.entries.len();
        assert!(n >= 4);
        let last = &all.entries[n - 1];
        assert!(last.message.ends_with('…') && last.message.len() <= MAX_LINE + 3);
        let warn = read(0, "warn", "debian", 100);
        assert!(warn.entries.iter().all(|e| e.level != "info"));
        assert!(
            warn.entries
                .iter()
                .any(|e| e.message == "debian: sync failed")
        );
        // From an id on: only what came after.
        let after = all.entries[n - 2].id;
        let next = read(after, "trace", "", 100);
        assert_eq!(next.entries.len(), 1);
        assert_eq!(next.entries[0].id, last.id);
        // Newest kept when more are asked for than the limit.
        let two = read(0, "trace", "", 2);
        assert_eq!(two.entries.last().unwrap().id, last.id);
    }
}
