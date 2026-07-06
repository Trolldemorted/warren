#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum State {
    Starting,
    Idle,
    Running,
    Ended,
    Dead,
}

impl State {
    pub fn as_str(&self) -> &'static str {
        match self {
            State::Starting => "starting",
            State::Idle => "idle",
            State::Running => "running",
            State::Ended => "ended",
            State::Dead => "dead",
        }
    }

    /// Inverse of [`State::as_str`]: parse a wire state label back into a
    /// `State`. Returns `None` for unknown labels so callers can leave the
    /// tracked state untouched rather than guessing. Lets the supervisor's own
    /// state transitions (which are expressed as wire strings) feed the same
    /// `latest_state` the hook events do.
    pub fn from_label(s: &str) -> Option<State> {
        match s {
            "starting" => Some(State::Starting),
            "idle" => Some(State::Idle),
            "running" => Some(State::Running),
            "ended" => Some(State::Ended),
            "dead" => Some(State::Dead),
            _ => None,
        }
    }

    /// Convert from the wire-typed [`crate::wire::AgentState`]. Always
    /// succeeds — the wire enum has the same five variants as this one.
    /// Lets `supervisor::send_state` route a typed frame through to
    /// the observer without a string round-trip.
    pub fn from_agent_state(s: crate::wire::AgentState) -> State {
        match s {
            crate::wire::AgentState::Starting => State::Starting,
            crate::wire::AgentState::Idle => State::Idle,
            crate::wire::AgentState::Running => State::Running,
            crate::wire::AgentState::Ended => State::Ended,
            crate::wire::AgentState::Dead => State::Dead,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_label_roundtrips_every_state() {
        for st in [
            State::Starting,
            State::Idle,
            State::Running,
            State::Ended,
            State::Dead,
        ] {
            assert_eq!(State::from_label(st.as_str()), Some(st));
        }
    }

    #[test]
    fn from_label_rejects_unknown() {
        assert_eq!(State::from_label("bogus"), None);
        assert_eq!(State::from_label(""), None);
    }
}
