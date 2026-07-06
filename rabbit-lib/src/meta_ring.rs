use std::collections::VecDeque;
use std::sync::Mutex;

/// Bounded queue of recent meta-event wire frames, kept across WS
/// reconnect attempts so a flapping link doesn't lose structured events
/// the operator hasn't yet seen on the warren side.
///
/// Entries are stored as already-serialized JSON strings — the link hands
/// them to tungstenite verbatim, which is what we want to send over the
/// wire anyway. The seq is the value stamped into the envelope; it's also
/// the watermark Ack{seq} uses to clear entries.
///
/// Cap is bytes, not count: a usage snapshot is ~120 bytes but a transcript
/// message can be many KB, and we want a predictable memory ceiling.
pub struct MetaRing {
    state: Mutex<MetaRingState>,
    max_bytes: usize,
}

struct MetaRingState {
    entries: VecDeque<MetaEntry>,
    current_bytes: usize,
}

struct MetaEntry {
    seq: i64,
    bytes: usize,
    frame: String,
}

impl MetaRing {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            state: Mutex::new(MetaRingState {
                entries: VecDeque::new(),
                current_bytes: 0,
            }),
            max_bytes,
        }
    }

    /// Append a freshly-sent frame. Returns the seq the caller should
    /// stamp into the envelope (always equal to `seq` here; the caller's
    /// own seq allocator hands it in so we can audit it later).
    pub fn push(&self, seq: i64, frame: String) {
        let bytes = frame.len();
        let mut g = self.state.lock().expect("meta_ring poisoned");
        g.entries.push_back(MetaEntry { seq, bytes, frame });
        g.current_bytes += bytes;
        // Bound the queue. Drop oldest first — long disconnects lose old
        // events but the bounded terminal replay buffer (§A.6) preserves
        // the screen state regardless.
        while g.current_bytes > self.max_bytes {
            if let Some(dropped) = g.entries.pop_front() {
                g.current_bytes -= dropped.bytes;
            } else {
                break;
            }
        }
    }

    /// Drop everything with seq ≤ ack_seq. Called when warren sends
    /// `EnvelopeBody::Ack { ack_seq }`. Returns the number of bytes freed.
    pub fn trim_through(&self, ack_seq: i64) -> usize {
        let mut g = self.state.lock().expect("meta_ring poisoned");
        let mut freed = 0;
        while let Some(front) = g.entries.front() {
            if front.seq > ack_seq {
                break;
            }
            let dropped = g.entries.pop_front().expect("front was Some above");
            g.current_bytes -= dropped.bytes;
            freed += dropped.bytes;
        }
        freed
    }

    /// Snapshot of frames still buffered, in seq order. Used at the start
    /// of each WS attempt to replay anything warren hasn't acked yet.
    pub fn snapshot(&self) -> Vec<String> {
        let g = self.state.lock().expect("meta_ring poisoned");
        g.entries.iter().map(|e| e.frame.clone()).collect()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.state.lock().expect("meta_ring poisoned").entries.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.state
            .lock()
            .expect("meta_ring poisoned")
            .entries
            .is_empty()
    }

    #[cfg(test)]
    pub fn current_bytes(&self) -> usize {
        self.state.lock().expect("meta_ring poisoned").current_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_appends_and_counts_bytes() {
        let r = MetaRing::new(1024);
        r.push(1, r#"{"v":1,"seq":1,"t":"x"}"#.to_string());
        r.push(2, r#"{"v":1,"seq":2,"t":"y"}"#.to_string());
        assert_eq!(r.len(), 2);
        assert!(r.current_bytes() > 0);
    }

    #[test]
    fn trim_through_drops_acked_prefix() {
        let r = MetaRing::new(1024);
        r.push(1, "a".into());
        r.push(2, "b".into());
        r.push(3, "c".into());
        let freed = r.trim_through(2);
        assert_eq!(freed, 2);
        assert_eq!(r.len(), 1);
        assert_eq!(r.snapshot(), vec!["c".to_string()]);
    }

    #[test]
    fn trim_through_with_no_match_is_noop() {
        let r = MetaRing::new(1024);
        r.push(5, "x".into());
        r.push(6, "y".into());
        let freed = r.trim_through(3);
        assert_eq!(freed, 0);
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn trim_through_partial_keeps_tail() {
        let r = MetaRing::new(1024);
        r.push(10, "ten".into());
        r.push(11, "eleven".into());
        r.push(12, "twelve".into());
        r.trim_through(11);
        assert_eq!(r.snapshot(), vec!["twelve".to_string()]);
    }

    #[test]
    fn push_evicts_oldest_when_over_cap() {
        // 4-byte cap; pushing three 4-byte frames should drop the first.
        let r = MetaRing::new(8);
        r.push(1, "aaaa".into()); // 4 bytes
        r.push(2, "bbbb".into()); // 4 bytes
        r.push(3, "cccc".into()); // would push to 12, drop front
        assert_eq!(r.len(), 2, "oldest evicted to stay under cap");
        assert_eq!(r.snapshot(), vec!["bbbb".to_string(), "cccc".to_string()]);
    }

    #[test]
    fn snapshot_returns_empty_when_empty() {
        let r = MetaRing::new(1024);
        assert!(r.snapshot().is_empty());
    }
}
