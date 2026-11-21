//! Mirrors collect-swamp's `aws-cur-collect`: the same Athena SQL over the customer's CUR 2.0 table, word for word,
//! with the same fallbacks, so the recorded answers match when qloudpulse replays them. Only query results leave:
//! never the CUR files themselves.
//!
//! The SQL is copied from `aws_cur.ts` (`curQuery`, `scopeTagColumns`) and `lib/aws_cur_commitments.ts`
//! (`commitmentQuery`, `coverageQuery`); `billTagKey` from `lib/ec2.ts`. A change there must be made here too; the
//! contract tests in collect-swamp catch one that is not.

use crate::aws::Caller;
use crate::aws::caller::Account;
use crate::body::Body;
use anyhow::{Result, bail};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

pub const COLLECTOR: &str = "aws-cur-collect";

/// The tags an instance's name is made of; their bill columns are read by every bill query.
pub const INSTANCE_NAME_TAGS: &[&str] = &["Name", "eks:cluster-name", "aws:eks:cluster-name"];

/// Where the bill is and what to read from it.
#[derive(Debug, Clone)]
pub struct CurSettings {
    pub region: String,
    pub workgroup: String,
    pub database: String,
    pub table: String,
    /// The company's scope tag keys, as qloudpulse has them (`scopeTagKeys`): they shape the SQL, so they must match.
    pub scope_tag_keys: Vec<String>,
    pub periods: Vec<String>,
    /// The first poll's wait; the collector's is one second.
    pub first_poll: Duration,
}

/// What the run read, for the manifest: the replay passes these as the collector's `billingPeriods`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CurOutcome {
    pub account: String,
    pub region: String,
    pub periods: Vec<String>,
    pub scope_tag_keys: Vec<String>,
    /// False when the export has no `resource_tags` column.
    pub tagged: bool,
    /// False when it has no `bill_invoice_id` column.
    pub counted: bool,
    pub commitments: bool,
}

/// A tag key as CUR 2.0 names it in `resource_tags`: `Name` → `user_name`, `aws:eks:cluster-name` →
/// `aws_eks_cluster_name`, `CostCenter` → `user_cost_center`.
pub fn bill_tag_key(key: &str) -> String {
    let prefixed = if key.starts_with("aws:") {
        key.to_string()
    } else {
        format!("user:{key}")
    };
    let mut spaced = String::new();
    for c in prefixed.chars() {
        if c.is_ascii_uppercase() {
            spaced.push('_');
        }
        spaced.push(c);
    }
    let lower = spaced.to_lowercase();
    let mut out = String::new();
    for c in lower.chars() {
        let c = if c.is_ascii_lowercase() || c.is_ascii_digit() {
            c
        } else {
            '_'
        };
        if c == '_' && out.ends_with('_') {
            continue;
        }
        out.push(c);
    }
    out.trim_matches('_').to_string()
}

pub fn bill_tag_column(key: &str) -> String {
    format!("resource_tags_{}", bill_tag_key(key))
}

/// The company's scope tag keys the bill can tell apart, each with its column (`scope_tag_<i>`). Two keys that map
/// to the same bill key are both left out, as the collector does.
pub fn scope_tag_columns(keys: &[String]) -> Vec<(String, String, String)> {
    let mut seen = Vec::<String>::new();
    for k in keys {
        if !seen.contains(k) {
            seen.push(k.clone());
        }
    }
    let mut by_bill: Vec<(String, Vec<String>)> = Vec::new();
    for key in seen {
        let bill = bill_tag_key(&key);
        match by_bill.iter_mut().find(|(b, _)| *b == bill) {
            Some((_, ks)) => ks.push(key),
            None => by_bill.push((bill, vec![key])),
        }
    }
    let valid = regex::Regex::new(r"^(user|aws)_[a-z0-9_]+$").expect("valid");
    by_bill
        .into_iter()
        .filter(|(bill, ks)| ks.len() == 1 && valid.is_match(bill))
        .enumerate()
        .map(|(i, (bill, ks))| (ks[0].clone(), bill, format!("scope_tag_{i}")))
        .collect()
}

/// Result column -> the tag key whose values it holds, for redaction.
pub fn tag_columns(scope_tag_keys: &[String]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = INSTANCE_NAME_TAGS
        .iter()
        .map(|k| (bill_tag_column(k), k.to_string()))
        .collect();
    out.extend(
        scope_tag_columns(scope_tag_keys)
            .into_iter()
            .map(|(key, _, column)| (column, key)),
    );
    out
}

/// Calls `visit(tag key, cell)` for every cell of a tag column in a `GetQueryResults` answer. `columns` carries the
/// column names from page to page of one query (only some pages may repeat them); the header row (the column names
/// as values, on the first page) is skipped.
pub fn for_each_tag_cell(
    answer: &mut Value,
    columns: &mut Vec<String>,
    tag_columns: &[(String, String)],
    visit: &mut dyn FnMut(&str, &mut Value),
) {
    if let Some(info) = answer["ResultSet"]["ResultSetMetadata"]["ColumnInfo"].as_array() {
        *columns = info
            .iter()
            .map(|c| c["Name"].as_str().unwrap_or_default().to_string())
            .collect();
    }
    let keyed: Vec<(usize, &str)> = columns
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            tag_columns
                .iter()
                .find(|(col, _)| col == c)
                .map(|(_, key)| (i, key.as_str()))
        })
        .collect();
    if keyed.is_empty() {
        return;
    }
    let Some(rows) = answer["ResultSet"]["Rows"].as_array_mut() else {
        return;
    };
    for row in rows.iter_mut() {
        let Some(cells) = row["Data"].as_array_mut() else {
            continue;
        };
        let header = cells.len() == columns.len()
            && cells
                .iter()
                .zip(columns.iter())
                .all(|(c, n)| c["VarCharValue"].as_str() == Some(n.as_str()));
        if header {
            continue;
        }
        for (i, key) in &keyed {
            if let Some(cell) = cells.get_mut(*i)
                && cell.get("VarCharValue").is_some()
            {
                visit(key, cell);
            }
        }
    }
}

fn periods_in(periods: &[String]) -> String {
    periods.iter().map(|p| format!("'{p}'")).collect::<Vec<_>>().join(", ")
}

pub fn cur_query(
    database: &str,
    table: &str,
    periods: &[String],
    tags: bool,
    scope_keys: &[String],
    invoices: bool,
) -> String {
    let tag_columns: String = if tags {
        INSTANCE_NAME_TAGS
            .iter()
            .map(|k| (bill_tag_key(k), bill_tag_column(k)))
            .chain(
                scope_tag_columns(scope_keys)
                    .into_iter()
                    .map(|(_, bill, column)| (bill, column)),
            )
            .map(|(bill, column)| format!("  max(element_at(resource_tags, '{bill}')) AS {column},\n"))
            .collect()
    } else {
        String::new()
    };
    let counts = if invoices {
        ",\n  count(*) AS lines,\n  count_if(coalesce(bill_invoice_id, '') <> '') AS invoiced_lines"
    } else {
        ""
    };
    format!(
        "SELECT
  billing_period,
  coalesce(date_format(line_item_usage_start_date, '%Y-%m-%d'), billing_period || '-01') AS day,
  coalesce(line_item_usage_account_id, '') AS account_id,
  coalesce(line_item_line_item_type, '') AS line_item_type,
  coalesce(line_item_product_code, '') AS product_code,
  coalesce(line_item_resource_id, '') AS resource_id,
  coalesce(line_item_usage_type, '') AS usage_type,
  coalesce(pricing_unit, '') AS pricing_unit,
  coalesce(product_region_code, '') AS region,
  coalesce(line_item_operation, '') AS operation,
{tag_columns}  coalesce(sum(line_item_usage_amount), 0) AS usage_amount,
  coalesce(sum(line_item_unblended_cost), 0) AS unblended_cost,
  coalesce(sum(reservation_effective_cost), 0) AS reservation_effective_cost,
  coalesce(sum(savings_plan_savings_plan_effective_cost), 0) AS savings_plan_effective_cost{counts}
FROM \"{database}\".\"{table}\"
WHERE billing_period IN ({})
GROUP BY 1, 2, 3, 4, 5, 6, 7, 8, 9, 10",
        periods_in(periods)
    )
}

pub fn commitment_query(database: &str, table: &str, periods: &[String]) -> String {
    format!(
        "SELECT
  billing_period,
  coalesce(date_format(line_item_usage_start_date, '%Y-%m-%d'), billing_period || '-01') AS day,
  line_item_line_item_type AS line_item_type,
  coalesce(line_item_usage_account_id, '') AS account_id,
  coalesce(savings_plan_savings_plan_a_r_n, reservation_reservation_a_r_n) AS commitment_id,
  max(coalesce(line_item_product_code, '')) AS product_code,
  max(coalesce(product_instance_type, '')) AS instance_type,
  max(coalesce(product_region_code, '')) AS region,
  max(coalesce(savings_plan_offering_type, '')) AS offering_type,
  max(coalesce(savings_plan_purchase_term, '')) AS purchase_term,
  max(coalesce(savings_plan_payment_option, '')) AS payment_option,
  max(coalesce(savings_plan_region, '')) AS plan_region,
  max(coalesce(savings_plan_instance_type_family, '')) AS instance_family,
  max(coalesce(savings_plan_start_time, reservation_start_time, '')) AS start_time,
  max(coalesce(savings_plan_end_time, reservation_end_time, '')) AS end_time,
  coalesce(sum(savings_plan_total_commitment_to_date), 0) AS plan_committed,
  coalesce(sum(savings_plan_used_commitment), 0) AS plan_used,
  coalesce(sum(line_item_unblended_cost), 0) AS recurring_fee,
  coalesce(sum(reservation_amortized_upfront_fee_for_billing_period), 0) AS upfront_fee,
  coalesce(sum(reservation_unused_amortized_upfront_fee_for_billing_period), 0) AS unused_upfront_fee,
  coalesce(sum(reservation_unused_recurring_fee), 0) AS unused_recurring_fee
FROM \"{database}\".\"{table}\"
WHERE billing_period IN ({})
  AND ((line_item_line_item_type = 'SavingsPlanRecurringFee' AND savings_plan_savings_plan_a_r_n IS NOT NULL)
    OR (line_item_line_item_type = 'RIFee' AND reservation_reservation_a_r_n IS NOT NULL))
GROUP BY 1, 2, 3, 4, 5",
        periods_in(periods)
    )
}

pub fn coverage_query(database: &str, table: &str, periods: &[String]) -> String {
    format!(
        "SELECT
  billing_period,
  coalesce(date_format(line_item_usage_start_date, '%Y-%m-%d'), billing_period || '-01') AS day,
  coalesce(line_item_usage_account_id, '') AS account_id,
  line_item_line_item_type AS line_item_type,
  coalesce(line_item_product_code, '') AS product_code,
  coalesce(line_item_usage_type, '') AS usage_type,
  coalesce(line_item_operation, '') AS operation,
  sum(pricing_public_on_demand_cost) AS on_demand_cost
FROM \"{database}\".\"{table}\"
WHERE billing_period IN ({})
  AND line_item_line_item_type IN ('Usage', 'SavingsPlanCoveredUsage', 'DiscountedUsage')
  AND pricing_public_on_demand_cost IS NOT NULL
GROUP BY 1, 2, 3, 4, 5, 6, 7",
        periods_in(periods)
    )
}

/// `back` months before the current UTC month, through the current one, oldest first: `back = 1` gives the
/// collector's default, the previous and the current month.
pub fn periods(now: Timestamp, back: u32) -> Vec<String> {
    let date = now.to_zoned(jiff::tz::TimeZone::UTC).date();
    let (mut y, mut m) = (date.year() as i32, date.month() as i32);
    let mut out = Vec::new();
    for _ in 0..=back {
        out.push(format!("{y:04}-{m:02}"));
        m -= 1;
        if m == 0 {
            m = 12;
            y -= 1;
        }
    }
    out.reverse();
    out
}

/// Why a query failed: its state and reason, as the collector's `AthenaError` message reads.
#[derive(Debug)]
pub struct QueryFailed(pub String);

impl std::fmt::Display for QueryFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Athena query failed: {}", self.0)
    }
}

impl std::error::Error for QueryFailed {}

fn missing_column(reason: &str, column: &str) -> bool {
    regex::RegexBuilder::new(&format!(
        r#"column ['"`]?(?:[a-z0-9_]+\.)?{column}['"`]? (cannot be resolved|does not exist)"#
    ))
    .case_insensitive(true)
    .build()
    .expect("valid")
    .is_match(reason)
}

/// A random version-4 UUID, as the SDK fills `ClientRequestToken`. The replay ignores it.
fn request_token() -> String {
    let mut b: [u8; 16] = rand::random();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex::encode(b);
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// Max total wait before the collector stops a query: 10 minutes.
const QUERY_TIMEOUT: Duration = Duration::from_secs(600);

/// Runs one query as `runAthenaQuery` does: start, poll with a growing wait, then every page of results.
/// Polls are recorded as the last answer only, so a replay answers every poll with the final state.
pub fn run_query(caller: &Caller, account: &Account, s: &CurSettings, sql: &str) -> Result<Vec<Body>> {
    let start = json!({
        "QueryString": sql,
        "ClientRequestToken": request_token(),
        "QueryExecutionContext": {"Catalog": "AwsDataCatalog", "Database": s.database},
        "WorkGroup": s.workgroup,
    });
    let pages = caller.call(COLLECTOR, account, &s.region, "athena", "StartQueryExecution", start)?;
    let Some(id) = pages
        .first()
        .and_then(Body::json)
        .and_then(|v| v["QueryExecutionId"].as_str())
        .map(str::to_string)
    else {
        bail!("StartQueryExecution answered without an id");
    };
    let mut delay = s.first_poll;
    let mut waited = Duration::ZERO;
    loop {
        let polled = caller.call_latest(
            COLLECTOR,
            account,
            &s.region,
            "athena",
            "GetQueryExecution",
            json!({"QueryExecutionId": id}),
        )?;
        let status = polled
            .first()
            .and_then(Body::json)
            .map(|v| v["QueryExecution"]["Status"].clone())
            .unwrap_or(Value::Null);
        match status["State"].as_str().unwrap_or("UNKNOWN") {
            "SUCCEEDED" => break,
            state @ ("FAILED" | "CANCELLED") => {
                let reason = status["StateChangeReason"]
                    .as_str()
                    .or(status["AthenaError"]["ErrorMessage"].as_str())
                    .unwrap_or("no reason given");
                return Err(QueryFailed(format!("{state}: {reason}")).into());
            }
            state => {
                if waited >= QUERY_TIMEOUT {
                    let _ = caller.call(
                        COLLECTOR,
                        account,
                        &s.region,
                        "athena",
                        "StopQueryExecution",
                        json!({"QueryExecutionId": id}),
                    );
                    return Err(QueryFailed(format!("still {state} after 10 minutes, stopped")).into());
                }
                std::thread::sleep(delay);
                waited += delay.max(Duration::from_millis(1));
                delay = Duration::from_millis(((delay.as_millis() as f64 * 1.5).round() as u64).min(5000))
                    .min(s.first_poll * 5);
            }
        }
    }
    caller.call(
        COLLECTOR,
        account,
        &s.region,
        "athena",
        "GetQueryResults",
        json!({"QueryExecutionId": id, "MaxResults": 1000}),
    )
}

/// One row of the bill query, as the infra collector reads `aws_cost.cur_daily`. Local only: it plans the infra
/// calls and is never written anywhere.
#[derive(Debug, Clone, PartialEq)]
pub struct BillRow {
    pub day: String,
    pub account_id: String,
    pub line_item_type: String,
    pub product_code: String,
    pub resource_id: String,
    pub region: String,
    /// Amortized, as `amortizedCost` values a usage line.
    pub cost: f64,
}

/// The rows of the bill query's result pages.
pub fn bill_rows(pages: &[Body]) -> Vec<BillRow> {
    let mut columns: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for page in pages.iter().filter_map(Body::json) {
        if let Some(info) = page["ResultSet"]["ResultSetMetadata"]["ColumnInfo"].as_array() {
            columns = info
                .iter()
                .map(|c| c["Name"].as_str().unwrap_or_default().to_string())
                .collect();
        }
        let Some(rows) = page["ResultSet"]["Rows"].as_array() else {
            continue;
        };
        for row in rows {
            let cells: Vec<Option<&str>> = row["Data"]
                .as_array()
                .map(|d| d.iter().map(|c| c["VarCharValue"].as_str()).collect())
                .unwrap_or_default();
            if cells.len() == columns.len() && cells.iter().zip(&columns).all(|(c, n)| *c == Some(n.as_str())) {
                continue;
            }
            let get = |name: &str| {
                columns
                    .iter()
                    .position(|c| c == name)
                    .and_then(|i| cells.get(i).copied().flatten())
                    .unwrap_or("")
                    .to_string()
            };
            let num = |name: &str| get(name).parse::<f64>().unwrap_or(0.0);
            let line_item_type = get("line_item_type");
            let cost = match line_item_type.as_str() {
                "DiscountedUsage" => num("reservation_effective_cost"),
                "SavingsPlanCoveredUsage" => num("savings_plan_effective_cost"),
                _ => num("unblended_cost"),
            };
            out.push(BillRow {
                day: get("day"),
                account_id: get("account_id"),
                line_item_type,
                product_code: get("product_code"),
                resource_id: get("resource_id"),
                region: get("region"),
                cost,
            });
        }
    }
    out
}

/// The bill, then commitments and coverage, as `runCollect` reads them for `periods`. Also the bill's rows, to plan
/// the infra calls.
pub fn collect(caller: &Caller, account: &Account, s: &CurSettings) -> Result<(CurOutcome, Vec<BillRow>)> {
    let (mut tagged, mut counted) = (true, true);
    let rows = loop {
        let sql = cur_query(&s.database, &s.table, &s.periods, tagged, &s.scope_tag_keys, counted);
        match run_query(caller, account, s, &sql) {
            Ok(pages) => break bill_rows(&pages),
            Err(e) => match e.downcast_ref::<QueryFailed>() {
                Some(QueryFailed(reason)) if tagged && missing_column(reason, "resource_tags") => tagged = false,
                Some(QueryFailed(reason)) if counted && missing_column(reason, "bill_invoice_id") => counted = false,
                _ => return Err(e),
            },
        }
    };
    let commitments = run_query(caller, account, s, &commitment_query(&s.database, &s.table, &s.periods)).is_ok();
    if commitments {
        run_query(caller, account, s, &coverage_query(&s.database, &s.table, &s.periods))?;
    }
    let outcome = CurOutcome {
        account: account.id.clone(),
        region: s.region.clone(),
        periods: s.periods.clone(),
        scope_tag_keys: s.scope_tag_keys.clone(),
        tagged,
        counted,
        commitments,
    };
    Ok((outcome, rows))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::{HttpRequest, HttpResponse, Transport};
    use crate::callplan::CallPlan;
    use aws_credential_types::Credentials;
    use std::sync::Mutex;

    #[test]
    fn bill_tag_keys_match_the_collector() {
        assert_eq!(bill_tag_key("Name"), "user_name");
        assert_eq!(bill_tag_key("eks:cluster-name"), "user_eks_cluster_name");
        assert_eq!(bill_tag_key("aws:eks:cluster-name"), "aws_eks_cluster_name");
        assert_eq!(bill_tag_key("CostCenter"), "user_cost_center");
    }

    #[test]
    fn scope_keys_that_collide_are_both_left_out() {
        let keys: Vec<String> = ["Team", "team", "CostCenter", "Team"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let cols = scope_tag_columns(&keys);
        assert_eq!(
            cols,
            vec![(
                "CostCenter".to_string(),
                "user_cost_center".to_string(),
                "scope_tag_0".to_string()
            )]
        );
    }

    #[test]
    fn bill_query_reads_as_the_collector_writes_it() {
        let periods = vec!["2026-10".to_string(), "2026-11".to_string()];
        let sql = cur_query(
            "qloudpulse_cur",
            "cur2",
            &periods,
            true,
            &["CostCenter".to_string()],
            true,
        );
        assert!(sql.starts_with("SELECT\n  billing_period,\n"));
        assert!(sql.contains(
            "  coalesce(line_item_operation, '') AS operation,\n  max(element_at(resource_tags, 'user_name')) AS resource_tags_user_name,\n"
        ));
        assert!(sql.contains("  max(element_at(resource_tags, 'user_cost_center')) AS scope_tag_0,\n  coalesce(sum(line_item_usage_amount), 0)"));
        assert!(sql.contains("AS savings_plan_effective_cost,\n  count(*) AS lines,\n"));
        assert!(sql.ends_with("FROM \"qloudpulse_cur\".\"cur2\"\nWHERE billing_period IN ('2026-10', '2026-11')\nGROUP BY 1, 2, 3, 4, 5, 6, 7, 8, 9, 10"));
        let plain = cur_query("d", "t", &periods, false, &[], false);
        assert!(plain.contains("AS operation,\n  coalesce(sum(line_item_usage_amount)"));
        assert!(plain.contains("savings_plan_effective_cost\nFROM"));
    }

    #[test]
    fn periods_run_back_across_a_year() {
        let now: Timestamp = "2026-01-15T07:00:00Z".parse().unwrap();
        assert_eq!(periods(now, 1), ["2025-12", "2026-01"]);
        assert_eq!(periods(now, 3), ["2025-10", "2025-11", "2025-12", "2026-01"]);
    }

    /// An Athena that fails the first bill query for lack of resource_tags, and answers RUNNING once per query.
    struct FakeAthena {
        polls: Mutex<std::collections::HashMap<String, u32>>,
        starts: Mutex<Vec<String>>,
    }

    impl Transport for FakeAthena {
        fn send(&self, request: &HttpRequest) -> Result<HttpResponse> {
            let target = request
                .headers
                .iter()
                .find(|(k, _)| k == "x-amz-target")
                .unwrap()
                .1
                .clone();
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let answer = match target.rsplit('.').next().unwrap() {
                "StartQueryExecution" => {
                    let mut starts = self.starts.lock().unwrap();
                    starts.push(body["QueryString"].as_str().unwrap().to_string());
                    json!({"QueryExecutionId": format!("q{}", starts.len())})
                }
                "GetQueryExecution" => {
                    let id = body["QueryExecutionId"].as_str().unwrap().to_string();
                    let mut polls = self.polls.lock().unwrap();
                    let n = polls.entry(id.clone()).or_default();
                    *n += 1;
                    let state = if *n == 1 {
                        "RUNNING"
                    } else if id == "q1" {
                        "FAILED"
                    } else {
                        "SUCCEEDED"
                    };
                    json!({"QueryExecution": {"Status": {"State": state,
                        "StateChangeReason": "COLUMN_NOT_FOUND: line 12:15: Column 'resource_tags' cannot be resolved"}}})
                }
                "GetQueryResults" => json!({"ResultSet": {"Rows": []}}),
                other => panic!("unexpected {other}"),
            };
            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: answer.to_string().into_bytes(),
            })
        }
    }

    #[test]
    fn falls_back_without_tags_and_records_only_the_final_poll() {
        let fake = FakeAthena {
            polls: Mutex::new(Default::default()),
            starts: Mutex::new(vec![]),
        };
        let caller = Caller::new(CallPlan::embedded(), &fake, vec![]);
        let account = Account {
            id: "111122223333".into(),
            credentials: Credentials::new("A", "s", None, None, "t"),
        };
        let s = CurSettings {
            region: "eu-west-1".into(),
            workgroup: "qloudpulse-cost".into(),
            database: "qloudpulse_cur".into(),
            table: "cur2".into(),
            scope_tag_keys: vec![],
            periods: vec!["2026-10".into(), "2026-11".into()],
            first_poll: Duration::ZERO,
        };
        let (outcome, _) = collect(&caller, &account, &s).unwrap();
        assert!(!outcome.tagged && outcome.counted && outcome.commitments);
        let starts = fake.starts.lock().unwrap().clone();
        assert_eq!(starts.len(), 4, "bill, bill without tags, commitments, coverage");
        assert!(starts[0].contains("resource_tags_user_name") && !starts[1].contains("resource_tags"));
        let records = caller.into_records();
        let polls: Vec<_> = records.iter().filter(|r| r.operation == "GetQueryExecution").collect();
        assert_eq!(polls.len(), 4, "one recorded poll per query");
        assert!(
            polls.iter().all(|r| !r.response.body.contains("RUNNING")),
            "the final state only"
        );
    }
}
