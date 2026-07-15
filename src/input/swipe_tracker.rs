use std::collections::VecDeque;
use std::time::Duration;

const HISTORY_LIMIT: Duration = Duration::from_millis(150);
const DECELERATION: f64 = 0.997;

#[derive(Debug)]
pub struct SwipeTracker {
    history: VecDeque<Event>,
    pos: f64,
}

#[derive(Debug, Clone, Copy)]
struct Event {
    delta: f64,
    timestamp: Duration,
}

impl SwipeTracker {
    #[allow(clippy::new_without_default)]
    #[must_use]
    pub const fn new() -> Self {
        Self {
            history: VecDeque::new(),
            pos: 0.,
        }
    }

    /// Pushes a new reading into the tracker.
    pub fn push(&mut self, delta: f64, timestamp: Duration) {
        // For the events that we care about, timestamps should always increase
        // monotonically.
        if let Some(last) = self.history.back()
            && timestamp < last.timestamp
        {
            trace!(
                "ignoring event with timestamp {timestamp:?} earlier than last {:?}",
                last.timestamp
            );
            return;
        }

        self.history.push_back(Event { delta, timestamp });
        self.pos += delta;

        self.trim_history();
    }

    /// Returns the current gesture position.
    #[must_use]
    pub const fn pos(&self) -> f64 {
        self.pos
    }

    /// Computes the current gesture velocity.
    ///
    /// # Panics
    ///
    /// Panics if the last recorded event's timestamp precedes the first one's, which
    /// should not happen since events are pushed in chronological order.
    #[must_use]
    pub fn velocity(&self) -> f64 {
        let (Some(first), Some(last)) = (self.history.front(), self.history.back()) else {
            return 0.;
        };

        let total_time = last.timestamp.checked_sub(first.timestamp).unwrap().as_secs_f64();
        if total_time == 0. {
            return 0.;
        }

        let total_delta = self.history.iter().map(|event| event.delta).sum::<f64>();
        total_delta / total_time
    }

    /// Computes the gesture end position after decelerating to a halt.
    #[must_use]
    pub fn projected_end_pos(&self) -> f64 {
        let vel = self.velocity();
        self.pos - vel / (1000. * DECELERATION.ln())
    }

    fn trim_history(&mut self) {
        let Some(&Event { timestamp, .. }) = self.history.back() else {
            return;
        };

        while let Some(first) = self.history.front() {
            if timestamp <= first.timestamp + HISTORY_LIMIT {
                break;
            }

            let _ = self.history.pop_front();
        }
    }
}
