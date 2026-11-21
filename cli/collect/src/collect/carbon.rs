//! Mirrors collect-swamp's `aws-carbon-collect` (daily): AWS's estimate of each account's carbon emissions, monthly
//! since 2022-01, by account, region and service, from the Sustainability API in us-east-1. The collector asks for
//! the whole history every day, so the bundle carries it every day; the answer is small.

use crate::aws::Caller;
use crate::aws::caller::Account;
use anyhow::Result;
use jiff::Timestamp;
use serde_json::json;

pub const COLLECTOR: &str = "aws-carbon-collect";
pub const HISTORY_START: &str = "2022-01-01";
const PAGE_SIZE: u32 = 5000;
pub const EMISSIONS_TYPES: &[&str] = &[
    "TOTAL_SCOPE_1_CARBON_EMISSIONS",
    "TOTAL_SCOPE_2_MBM_CARBON_EMISSIONS",
    "TOTAL_SCOPE_2_LBM_CARBON_EMISSIONS",
    "TOTAL_SCOPE_3_MBM_CARBON_EMISSIONS",
    "TOTAL_SCOPE_3_LBM_CARBON_EMISSIONS",
    "TOTAL_MBM_CARBON_EMISSIONS",
    "TOTAL_LBM_CARBON_EMISSIONS",
];

/// The start of the month after `now`'s: AWS answers the months it has published up to there.
pub fn end_of_history(now: Timestamp) -> String {
    let first = now.to_zoned(jiff::tz::TimeZone::UTC).date().first_of_month();
    first
        .checked_add(jiff::Span::new().months(1))
        .expect("in range")
        .to_string()
}

/// One account's emissions, every page. An account whose role lacks the action answers AccessDenied, which is
/// recorded and replayed as such: the collector counts it as refused, not failed.
pub fn collect(caller: &Caller, account: &Account, now: Timestamp) -> Result<()> {
    let params = json!({
        "TimePeriod": {
            "Start": format!("{HISTORY_START}T00:00:00Z"),
            "End": format!("{}T00:00:00Z", end_of_history(now)),
        },
        "Granularity": "MONTHLY",
        "GroupBy": ["USAGE_ACCOUNT_ID", "REGION", "SERVICE"],
        "EmissionsTypes": EMISSIONS_TYPES,
        "MaxResults": PAGE_SIZE,
    });
    caller.call(
        COLLECTOR,
        account,
        "us-east-1",
        "sustainability",
        "GetEstimatedCarbonEmissions",
        params,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asks_the_global_api_host_with_a_json_body() {
        let plan = crate::callplan::CallPlan::embedded();
        let (service, op) = plan.operation("sustainability", "GetEstimatedCarbonEmissions").unwrap();
        let r = crate::aws::protocol::build(
            service,
            op,
            "us-east-1",
            &serde_json::json!({"MaxResults": 5000}),
            Some("t2"),
        )
        .unwrap();
        assert_eq!(
            r.url(),
            "https://api.sustainability.us-east-1.api.aws/v1/estimated-carbon-emissions"
        );
        assert_eq!(r.method, "POST");
        assert!(r.body.contains(r#""NextToken":"t2""#));
    }

    #[test]
    fn history_runs_to_the_next_month() {
        assert_eq!(end_of_history("2026-12-31T10:20:00Z".parse().unwrap()), "2027-01-01");
    }
}
