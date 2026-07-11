use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Build the argv for a (re)spawn of `claude`.
///
/// Resume policy:
/// - `fresh=true` → no resume flag. Brand-new session; the operator asked for
///   it (warren's `restart{fresh:true}`), or we are crash-looping past the
///   threshold and want a clean slate.
/// - `cold_start=true` → no resume flag. The rabbit process just started;
///   no operator-issued Restart is in flight, so trying to `--continue`
///   against an empty `~/.claude/projects/<encoded-cwd>/` would only print
///   "No conversation found to continue" and crash-loop the supervisor.
/// - `cold_start=false`, known `session_id` → `--resume <id>`. The
///   `SessionStart` hook gave us the id; continue the same conversation.
/// - `cold_start=false`, no `session_id` yet → `--continue`. Operator asked
///   for a Restart but the hook hasn't fired yet (e.g. very early restart
///   window); `claude --continue` reaches into the encoded-cwd dir and
///   picks the most recent conversation. This is the only place the flag
///   remains — cold-start deliberately skips it.
pub fn effective_args(
    base: &[String],
    session_id: Option<&str>,
    fresh: bool,
    cold_start: bool,
) -> Vec<String> {
    if fresh {
        return base.to_vec();
    }
    if cold_start {
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

    fn base() -> Vec<String> {
        vec!["--dangerously-skip-permissions".to_string()]
    }

    #[test]
    fn effective_args_fresh_true_ignores_session() {
        let out = effective_args(&base(), Some("sess-abc"), true, false);
        assert_eq!(out, base());
    }

    #[test]
    fn effective_args_appends_resume_when_session_and_not_fresh() {
        let out = effective_args(&base(), Some("sess-abc"), false, false);
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
        assert_eq!(
            effective_args(&base(), None, false, false),
            vec![
                "--dangerously-skip-permissions".to_string(),
                "--continue".to_string(),
            ]
        );
        assert_eq!(
            effective_args(&base(), Some(""), false, false),
            vec![
                "--dangerously-skip-permissions".to_string(),
                "--continue".to_string(),
            ]
        );
    }

    #[test]
    fn effective_args_continue_when_session_unknown_fresh_false() {
        // Operator-issued Restart with no SessionStart fired yet: the
        // encoded-cwd dir may still have a most-recent conversation
        // --continue can pick up. `--continue` is appropriate here.
        let out = effective_args(&base(), None, false, false);
        assert_eq!(
            out,
            vec![
                "--dangerously-skip-permissions".to_string(),
                "--continue".to_string(),
            ]
        );
    }

    #[test]
    fn effective_args_cold_start_skips_continue() {
        // Cold start (rabbit process just started, no operator Restart in
        // flight) with no known session_id: do NOT pass --continue. The
        // encoded-cwd conversation store is almost certainly empty, so
        // claude would print "No conversation found to continue" and exit
        // non-zero, putting the supervisor into a --continue/-exit crash
        // loop. Just spawn claude fresh; the SessionStart hook will then
        // populate `latest_session` for the next restart.
        let out = effective_args(&base(), None, false, true);
        assert_eq!(
            out,
            base(),
            "cold start with no session must not pass --continue"
        );

        let out = effective_args(&base(), Some(""), false, true);
        assert_eq!(
            out,
            base(),
            "empty session id on cold start must not pass --continue"
        );
    }

    #[test]
    fn effective_args_cold_start_with_stale_session_still_skips() {
        // Edge case: session_id is somehow populated at cold start
        // (e.g. persisted via a future enhancement). Don't trust it —
        // that conversation was tied to a previous rabbit process and
        // may not exist on this run's filesystem. Cold-start clean.
        let out = effective_args(&base(), Some("stale-id"), false, true);
        assert_eq!(out, base());
    }

    #[test]
    fn effective_args_preserves_base_order() {
        let base_args = vec![
            "--model".to_string(),
            "opus".to_string(),
            "--dangerously-skip-permissions".to_string(),
        ];
        let out = effective_args(&base_args, Some("sess-abc"), false, false);
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
