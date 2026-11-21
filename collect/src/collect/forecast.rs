//! Mirrors collect-swamp's `aws-forecast-collect` (daily, opt-in): Cost Explorer in the payer account, the spend
//! of this month and last per linked account, then AWS's forecast to the end of the month for each. Cost Explorer
//! bills $0.01 a request, so this runs only with `forecast = true` under `[cur]` (qloudpulse ADR 0046).

use crate::aws::Caller;
use crate::aws::caller::{Account, AwsError};
use crate::body::Body;
use anyhow::Result;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const COLLECTOR: &str = "aws-forecast-collect";

/// The days a run on `now` asks about (`monthSpan`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonthSpan {
    pub today: String,
    pub previous: String,
    pub first: String,
    pub next: String,
}

pub fn month_span(now: Timestamp) -> MonthSpan {
    let d = now.to_zoned(jiff::tz::TimeZone::UTC).date();
    let first = d.first_of_month();
    let previous = first.checked_sub(jiff::Span::new().months(1)).expect("in range");
    let next = first.checked_add(jiff::Span::new().months(1)).expect("in range");
    MonthSpan {
        today: d.to_string(),
        previous: previous.to_string(),
        first: first.to_string(),
        next: next.to_string(),
    }
}

/// What the forecast run read, for the manifest: the replay runs the collector on `day` with this interval.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ForecastOutcome {
    pub account: String,
    pub day: String,
    pub prediction_interval: u32,
    pub accounts: usize,
}

/// Leaves out tax, credits and refunds, as qloudpulse's own figure does (`COMPARABLE`).
fn comparable() -> Value {
    json!({"Not": {"Dimensions": {"Key": "RECORD_TYPE", "Values": ["Tax", "Credit", "Refund"]}}})
}

pub fn collect(caller: &Caller, payer: &Account, now: Timestamp, prediction_interval: u32) -> Result<ForecastOutcome> {
    let span = month_span(now);
    let spent = caller.call(
        COLLECTOR,
        payer,
        "us-east-1",
        "ce",
        "GetCostAndUsage",
        json!({
            "TimePeriod": {"Start": span.previous, "End": span.today},
            "Granularity": "MONTHLY",
            "Metrics": ["AmortizedCost"],
            "GroupBy": [{"Type": "DIMENSION", "Key": "LINKED_ACCOUNT"}],
            "Filter": comparable(),
        }),
    )?;
    let mut accounts: Vec<String> = spent
        .iter()
        .filter_map(Body::json)
        .flat_map(|p| p["ResultsByTime"].as_array().cloned().unwrap_or_default())
        .flat_map(|period| period["Groups"].as_array().cloned().unwrap_or_default())
        .filter_map(|g| g["Keys"][0].as_str().map(str::to_string))
        .collect();
    accounts.sort();
    accounts.dedup();
    for account in &accounts {
        let params = json!({
            "TimePeriod": {"Start": span.today, "End": span.next},
            "Metric": "AMORTIZED_COST",
            "Granularity": "MONTHLY",
            "PredictionIntervalLevel": prediction_interval,
            "Filter": {"And": [comparable(), {"Dimensions": {"Key": "LINKED_ACCOUNT", "Values": [account]}}]},
        });
        match caller.call(COLLECTOR, payer, "us-east-1", "ce", "GetCostForecast", params) {
            // Too little history for a forecast: the collector reads it as none, not as a failure.
            Err(e)
                if e.downcast_ref::<AwsError>()
                    .is_some_and(|a| a.code == "DataUnavailableException") => {}
            Err(e) => return Err(e),
            Ok(_) => {}
        }
    }
    Ok(ForecastOutcome {
        account: payer.id.clone(),
        day: span.today,
        prediction_interval,
        accounts: accounts.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::{HttpRequest, HttpResponse, Transport};
    use crate::callplan::CallPlan;
    use aws_credential_types::Credentials;
    use std::sync::Mutex;

    /// Two linked accounts; the second has too little history for a forecast.
    struct FakeCe(Mutex<Vec<String>>);

    impl Transport for FakeCe {
        fn send(&self, request: &HttpRequest) -> Result<HttpResponse> {
            let body = String::from_utf8_lossy(&request.body).to_string();
            let target = request
                .headers
                .iter()
                .find(|(k, _)| k == "x-amz-target")
                .unwrap()
                .1
                .clone();
            self.0.lock().unwrap().push(format!("{} {target} {body}", request.url));
            let (status, answer) = if target.ends_with("GetCostAndUsage") {
                (
                    200,
                    r#"{"ResultsByTime":[{"TimePeriod":{"Start":"2026-11-01"},"Groups":[{"Keys":["444455556666"]},{"Keys":["111122223333"]}]}]}"#,
                )
            } else if body.contains("444455556666") {
                (400, r#"{"__type":"DataUnavailableException"}"#)
            } else {
                (200, r#"{"Total":{"Amount":"12.3"}}"#)
            };
            Ok(HttpResponse {
                status,
                headers: vec![],
                body: answer.into(),
            })
        }
    }

    #[test]
    fn spend_then_a_forecast_per_account() {
        let fake = FakeCe(Mutex::new(vec![]));
        let caller = Caller::new(CallPlan::embedded(), &fake, vec!["forecast".into()]);
        let payer = Account {
            id: "111122223333".into(),
            credentials: Credentials::new("A", "s", None, None, "t"),
        };
        let out = collect(&caller, &payer, "2026-11-21T09:00:00Z".parse().unwrap(), 80).unwrap();
        assert_eq!(out.accounts, 2);
        let seen = fake.0.lock().unwrap().clone();
        assert!(seen[0].starts_with("https://ce.us-east-1.amazonaws.com/ AWSInsightsIndexService.GetCostAndUsage"));
        assert!(
            seen[0].contains(r#""Start":"2026-10-01","End":"2026-11-21""#),
            "{}",
            seen[0]
        );
        assert!(
            seen[1].contains("111122223333") && seen[1].contains(r#""End":"2026-12-01""#),
            "accounts sorted: {}",
            seen[1]
        );
        assert_eq!(seen.len(), 3);
    }

    #[test]
    fn off_unless_the_option_is_on() {
        let fake = FakeCe(Mutex::new(vec![]));
        let caller = Caller::new(CallPlan::embedded(), &fake, vec![]);
        let payer = Account {
            id: "111122223333".into(),
            credentials: Credentials::new("A", "s", None, None, "t"),
        };
        assert!(collect(&caller, &payer, "2026-11-21T09:00:00Z".parse().unwrap(), 80).is_err());
        assert!(
            fake.0.lock().unwrap().is_empty(),
            "nothing is sent, so nothing is billed"
        );
    }

    #[test]
    fn month_span_crosses_years() {
        let s = month_span("2026-01-01T09:00:00Z".parse().unwrap());
        assert_eq!(
            s,
            MonthSpan {
                today: "2026-01-01".into(),
                previous: "2025-12-01".into(),
                first: "2026-01-01".into(),
                next: "2026-02-01".into()
            }
        );
    }
}
