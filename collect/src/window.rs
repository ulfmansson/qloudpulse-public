//! The collection window: the last full UTC hour, the same as the hourly collectors use.

use jiff::{RoundMode, SignedDuration, Timestamp, TimestampRound, Unit};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    pub start: Timestamp,
    pub end: Timestamp,
}

pub const HOUR: SignedDuration = SignedDuration::from_hours(1);

/// `t` cut down to its whole UTC hour.
pub fn trunc_hour(t: Timestamp) -> Timestamp {
    t.round(TimestampRound::new().smallest(Unit::Hour).mode(RoundMode::Trunc))
        .expect("an hour divides a timestamp")
}

/// The UTC hour of day, 0-23.
pub fn utc_hour(t: Timestamp) -> u32 {
    t.to_zoned(jiff::tz::TimeZone::UTC).hour() as u32
}

impl Window {
    /// The last full hour before `now`.
    pub fn last_full_hour(now: Timestamp) -> Window {
        let end = trunc_hour(now);
        Window { start: end - HOUR, end }
    }

    /// `20261121T0600Z`, used in file names.
    pub fn label(&self) -> String {
        self.start.strftime("%Y%m%dT%H%MZ").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_full_hour_ends_at_the_hour() {
        let now: Timestamp = "2026-11-21T07:30:12Z".parse().unwrap();
        let w = Window::last_full_hour(now);
        assert_eq!(w.start.to_string(), "2026-11-21T06:00:00Z");
        assert_eq!(w.end.to_string(), "2026-11-21T07:00:00Z");
        assert_eq!(w.label(), "20261121T0600Z");
        assert_eq!(utc_hour(now), 7);
    }
}
