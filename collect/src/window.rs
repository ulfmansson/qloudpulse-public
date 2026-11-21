//! The collection window: the last full UTC hour, the same as the hourly collectors use.

use chrono::{DateTime, Duration, DurationRound, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

impl Window {
    /// The last full hour before `now`.
    pub fn last_full_hour(now: DateTime<Utc>) -> Window {
        let end = now
            .duration_trunc(Duration::hours(1))
            .expect("an hour divides a timestamp");
        Window {
            start: end - Duration::hours(1),
            end,
        }
    }

    /// `20261121T0600Z`, used in file names.
    pub fn label(&self) -> String {
        self.start.format("%Y%m%dT%H%MZ").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_full_hour_ends_at_the_hour() {
        let now = DateTime::parse_from_rfc3339("2026-11-21T07:30:12Z")
            .unwrap()
            .with_timezone(&Utc);
        let w = Window::last_full_hour(now);
        assert_eq!(w.start.to_rfc3339(), "2026-11-21T06:00:00+00:00");
        assert_eq!(w.end.to_rfc3339(), "2026-11-21T07:00:00+00:00");
        assert_eq!(w.label(), "20261121T0600Z");
    }
}
