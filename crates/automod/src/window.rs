use std::collections::VecDeque;

/// Sliding window of event timestamps (unix ms); used for raid, panic and mention-lockdown triggers.
#[derive(Debug, Default, Clone)]
pub struct Window(VecDeque<i64>);

impl Window {
    /// Record an event and return how many events fall within the last `span_ms`.
    pub fn push(&mut self, now: i64, span_ms: i64) -> usize {
        self.0.push_back(now);
        while self.0.front().is_some_and(|&t| now - t > span_ms) {
            self.0.pop_front();
        }
        self.0.len()
    }
}
