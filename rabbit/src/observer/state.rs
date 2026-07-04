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
}
