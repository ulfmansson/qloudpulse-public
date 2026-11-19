//! Writes the pull request comment from the answer and what the client kept in CI.

use std::collections::HashMap;
use std::fmt::Write as _;

use crate::pulumi::{Estimate, Local};
use crate::request::{Basis, ChangeKind, MAX_RESOURCES, NoteKind, ResourceEstimate, Response, Risk};

/// A whole number with thousands separators.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Dollars as a comment shows them: cents under $10, whole dollars with thousands separators above.
pub fn money(usd: f64) -> String {
    let negative = usd < 0.0;
    let abs = usd.abs();
    let body = if abs < 10.0 {
        format!("{abs:.2}")
    } else {
        grouped(abs.round() as u64)
    };
    format!("{}${body}", if negative { "-" } else { "" })
}

/// A name from the customer's code, safe in a table cell: in code quotes, with nothing that ends them or the cell.
fn code(name: &str) -> String {
    let clean: String = name
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| match c {
            '`' => '\'',
            '|' => '/',
            c => c,
        })
        .collect();
    format!("`{clean}`")
}

fn text(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .map(|c| if c == '|' { '/' } else { c })
        .collect()
}

fn basis(b: Basis) -> &'static str {
    match b {
        Basis::CompanyRate => "your rates",
        Basis::ListPrice => "list price",
        Basis::NotPriced => "not priced",
    }
}

fn risk(r: Risk) -> &'static str {
    match r {
        Risk::Low => "low",
        Risk::Medium => "medium",
        Risk::High => "high",
    }
}

fn cell(v: Option<f64>) -> String {
    v.map_or_else(|| "–".to_string(), money)
}

/// A count and its noun, such as "1 priced resource" or "3 priced resources".
fn resources(n: usize, adjective: &str) -> String {
    format!("{n} {adjective} resource{}", if n == 1 { "" } else { "s" })
}

/// The line on what the whole stack costs, under the headline, or why it is left out.
fn stack_line(answers: &HashMap<&str, &ResourceEstimate>, estimate: &Estimate) -> Option<String> {
    if let Some(n) = estimate.stack_too_large {
        return Some(format!(
            "**The stack:** its cost is left out: with every unchanged resource the request would hold {} \
             resources, and one estimate takes {} at most.",
            grouped(n as u64),
            grouped(MAX_RESOURCES as u64)
        ));
    }
    if !estimate.stack_listed {
        return None;
    }
    let sames: Vec<&Local> = estimate
        .locals
        .iter()
        .filter(|l| l.change == ChangeKind::Same)
        .collect();
    if !sames.is_empty() && sames.iter().all(|l| !answers.contains_key(l.key.as_str())) {
        return Some(
            "**The stack:** its cost is left out: this endpoint does not price unchanged resources yet.".to_string(),
        );
    }

    let mut before = 0.0;
    let mut after = 0.0;
    let mut priced = 0;
    let mut not_priced = estimate.stack_not_priced as usize
        + estimate
            .request
            .not_priced
            .iter()
            .map(|n| n.count as usize)
            .sum::<usize>();
    for l in &estimate.locals {
        match answers.get(l.key.as_str()) {
            Some(a) if a.basis != Basis::NotPriced => {
                priced += 1;
                before += a.monthly_before.unwrap_or(0.0);
                after += a.monthly_after.unwrap_or(0.0);
            }
            _ => not_priced += 1,
        }
    }
    let unpriced = if not_priced > 0 {
        format!("; {not_priced} not priced")
    } else {
        String::new()
    };
    if priced == 0 {
        return Some(format!(
            "**The stack:** none of its resources could be priced{unpriced}."
        ));
    }
    Some(format!(
        "**The stack:** about {} a month now, about {} after this change, from {}{unpriced}.",
        money(before),
        money(after),
        resources(priced, "priced")
    ))
}

/// The Markdown comment for a change: what the change adds, from the resources it changes, and what the whole stack
/// costs, from those it leaves the same too.
pub fn comment(response: &Response, estimate: &Estimate) -> String {
    let answers: HashMap<&str, &ResourceEstimate> = response.resources.iter().map(|r| (r.key.as_str(), r)).collect();
    let locals: Vec<&Local> = estimate
        .locals
        .iter()
        .filter(|l| l.change != ChangeKind::Same)
        .collect();
    let not_priced = &estimate.request.not_priced;

    let priced: Vec<(&Local, &ResourceEstimate)> = locals
        .iter()
        .filter_map(|l| answers.get(l.key.as_str()).map(|a| (*l, *a)))
        .filter(|(_, a)| a.basis != Basis::NotPriced)
        .collect();
    let delta: f64 = priced
        .iter()
        .map(|(_, a)| a.monthly_after.unwrap_or(0.0) - a.monthly_before.unwrap_or(0.0))
        .sum();

    let mut out = String::new();
    let headline = if priced.is_empty() {
        "nothing in this change could be priced".to_string()
    } else if delta >= 0.5 {
        format!("this change adds about {} a month", money(delta))
    } else if delta <= -0.5 {
        format!("this change saves about {} a month", money(-delta))
    } else {
        "this change moves the monthly cost by less than $1".to_string()
    };
    let _ = writeln!(out, "### qloudpulse: {headline}\n");
    if let Some(line) = stack_line(&answers, estimate) {
        let _ = writeln!(out, "{line}\n");
    }

    if !priced.is_empty() {
        out.push_str("| Resource | Change | Monthly now | Monthly after | Priced from |\n");
        out.push_str("| --- | --- | --- | --- | --- |\n");
        for (l, a) in &priced {
            let _ = writeln!(
                out,
                "| {} {} | {} | {} | {} | {} |",
                code(&l.name),
                l.label,
                l.change.label(),
                cell(a.monthly_before),
                cell(a.monthly_after),
                basis(a.basis)
            );
        }
        out.push('\n');
    }

    let notes_of = |kind: NoteKind| -> Vec<String> {
        let mut lines = Vec::new();
        for l in &locals {
            let Some(a) = answers.get(l.key.as_str()) else { continue };
            for n in a.notes.iter().filter(|n| n.kind == kind) {
                let mut line = format!("- {}: {}", code(&l.name), text(&n.text));
                if let Some(s) = n.monthly_saving {
                    let _ = write!(line, " About {} a month less.", money(s));
                }
                if let Some(r) = n.risk {
                    let _ = write!(line, " Risk: {}.", risk(r));
                }
                lines.push(line);
            }
        }
        lines
    };
    for (kind, title) in [
        (NoteKind::Saving, "Savings to consider"),
        (NoteKind::Usage, "What it uses today"),
        (NoteKind::Info, "Notes"),
    ] {
        let lines = notes_of(kind);
        if !lines.is_empty() {
            let _ = writeln!(out, "**{title}**\n\n{}\n", lines.join("\n"));
        }
    }

    let mut unpriced: Vec<String> = Vec::new();
    for l in &locals {
        match answers.get(l.key.as_str()) {
            None => unpriced.push(format!("- {} {}: no answer for it", code(&l.name), l.label)),
            Some(a) if a.basis == Basis::NotPriced => {
                let reason = a.reason.as_deref().map_or("not priced".to_string(), text);
                unpriced.push(format!("- {} {}: {reason}", code(&l.name), l.label));
            }
            Some(_) => {}
        }
    }
    for n in not_priced {
        unpriced.push(format!(
            "- {} × {}: not priced by qloudpulse yet",
            n.count,
            code(&n.type_token)
        ));
    }
    if !unpriced.is_empty() {
        let _ = writeln!(out, "**Not priced**\n\n{}\n", unpriced.join("\n"));
    }

    if !estimate.stack_listed && estimate.stack_too_large.is_none() {
        out.push_str("<sub>Run `pulumi preview --json --show-sames` to see the whole stack's monthly cost.</sub>\n\n");
    }

    let mut footer = String::from("Monthly figures in USD, before tax.");
    if let Some(id) = &response.estimate_id {
        let _ = write!(footer, " Estimate {}.", code(id));
    }
    let _ = writeln!(out, "<sub>{footer}</sub>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn money_formats() {
        assert_eq!(money(4.376), "$4.38");
        assert_eq!(money(0.0), "$0.00");
        assert_eq!(money(412.4), "$412");
        assert_eq!(money(1204.6), "$1,205");
        assert_eq!(money(1_234_567.0), "$1,234,567");
        assert_eq!(money(-120.0), "-$120");
    }

    #[test]
    fn names_cannot_break_the_table() {
        assert_eq!(code("a|b`c"), "`a/b'c`");
        assert_eq!(code("x\ny"), "`xy`");
    }
}
