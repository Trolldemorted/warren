use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Build the argv for a (re)spawn of `claude`.
///
/// Resume policy:
/// - `fresh=true` → no resume flag. Brand-new session; the operator asked for
///   it (warren's `restart{fresh:true}`), or we are crash-looping past the
///   threshold and want a clean slate.
/// - `fresh=false`, known `session_id` → `--resume <id>`. The
///   `SessionStart` hook gave us the id; continue the same conversation.
/// - `fresh=false`, no `session_id` yet → `--continue`. claude resumes its
///   most recent conversation (the §1 stable flag, useful when the hook
///   hasn't fired yet — e.g. very early restarts, or after `~/.claude` was
///   wiped but the encoded-cwd dir survived on disk).
pub fn effective_args(base: &[String], session_id: Option<&str>, fresh: bool) -> Vec<String> {
    if fresh {
        return base.to_vec();
    }
    match session_id {
        Some(id) if !id.is_empty() => base
            .iter()
            .cloned()
            .chain(std::iter::once("--resume".to_string()).chain(std::iter::once(id.to_string())))
            .collect(),
        _ => base
            .iter()
            .cloned()
            .chain(std::iter::once("--continue".to_string()))
            .collect(),
    }
}

#[derive(Debug, Clone)]
pub struct CrashWindow {
    pub window: Duration,
    pub threshold: usize,
    hits: VecDeque<Instant>,
}

impl CrashWindow {
    pub fn new(window: Duration, threshold: usize) -> Self {
        Self {
            window,
            threshold,
            hits: VecDeque::new(),
        }
    }

    pub fn record(&mut self, now: Instant) -> bool {
        while let Some(t) = self.hits.front() {
            if now.duration_since(*t) > self.window {
                self.hits.pop_front();
            } else {
                break;
            }
        }
        self.hits.push_back(now);
        self.hits.len() > self.threshold
    }

    pub fn len(&self) -> usize {
        self.hits.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.hits.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_args_fresh_true_ignores_session() {
        let base = vec!["--dangerously-skip-permissions".to_string()];
        let out = effective_args(&base, Some("sess-abc"), true);
        assert_eq!(out, base);
    }

    #[test]
    fn effective_args_appends_resume_when_session_and_not_fresh() {
        let base = vec!["--dangerously-skip-permissions".to_string()];
        let out = effective_args(&base, Some("sess-abc"), false);
        assert_eq!(
            out,
            vec![
                "--dangerously-skip-permissions".to_string(),
                "--resume".to_string(),
                "sess-abc".to_string(),
            ]
        );
    }

    #[test]
    fn effective_args_passthrough_when_no_session() {
        let base = vec!["--dangerously-skip-permissions".to_string()];
        assert_eq!(
            effective_args(&base, None, false),
            vec![
                "--dangerously-skip-permissions".to_string(),
                "--continue".to_string(),
            ]
        );
        assert_eq!(
            effective_args(&base, Some(""), false),
            vec![
                "--dangerously-skip-permissions".to_string(),
                "--continue".to_string(),
            ]
        );
    }

    #[test]
    fn effective_args_continue_when_session_unknown_fresh_false() {
        // Without a session_id, fresh=false should fall back to --continue,
        // not start a fresh session — the operator did not ask for one.
        let base = vec!["--dangerously-skip-permissions".to_string()];
        let out = effective_args(&base, None, false);
        assert_eq!(
            out,
            vec![
                "--dangerously-skip-permissions".to_string(),
                "--continue".to_string(),
            ]
        );
    }

    #[test]
    fn effective_args_preserves_base_order() {
        let base = vec![
            "--model".to_string(),
            "opus".to_string(),
            "--dangerously-skip-permissions".to_string(),
        ];
        let out = effective_args(&base, Some("sess-abc"), false);
        assert_eq!(
            out,
            vec![
                "--model".to_string(),
                "opus".to_string(),
                "--dangerously-skip-permissions".to_string(),
                "--resume".to_string(),
                "sess-abc".to_string(),
            ]
        );
    }

    #[test]
    fn crash_window_does_not_trip_below_threshold() {
        let mut w = CrashWindow::new(Duration::from_secs(300), 3);
        let t = Instant::now();
        assert!(!w.record(t));
        assert!(!w.record(t + Duration::from_secs(10)));
        assert!(!w.record(t + Duration::from_secs(20)));
        assert_eq!(w.len(), 3);
    }

    #[test]
    fn crash_window_trips_on_overshoot() {
        let mut w = CrashWindow::new(Duration::from_secs(300), 3);
        let t = Instant::now();
        assert!(!w.record(t));
        assert!(!w.record(t + Duration::from_secs(1)));
        assert!(!w.record(t + Duration::from_secs(2)));
        assert!(w.record(t + Duration::from_secs(3)));
        assert_eq!(w.len(), 4);
    }

    #[test]
    fn crash_window_evicts_old_entries_after_window() {
        let mut w = CrashWindow::new(Duration::from_secs(60), 3);
        let t = Instant::now();
        w.record(t);
        w.record(t + Duration::from_secs(30));
        w.record(t + Duration::from_secs(45));
        assert_eq!(w.len(), 3);
        let later = t + Duration::from_secs(120);
        assert!(!w.record(later));
        assert_eq!(w.len(), 1, "old entries evicted");
    }
}
