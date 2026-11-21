//! Writes the pull request comment from the answer and what the client kept in CI.

use std::collections::HashMap;
use std::fmt::Write as _;

use crate::link::page_link;
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

/// How the comment is written: Markdown for a pull request, or plain text for a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Markdown,
    Text,
}

impl Style {
    /// A name from the customer's code or the preview: without control characters, so it cannot move a terminal's
    /// cursor or end a line; in Markdown in code quotes, with nothing that ends them or a table cell.
    pub fn code(self, name: &str) -> String {
        let clean = name.chars().filter(|c| !c.is_control());
        match self {
            Style::Markdown => {
                let quoted: String = clean
                    .map(|c| match c {
                        '`' => '\'',
                        '|' => '/',
                        c => c,
                    })
                    .collect();
                format!("`{quoted}`")
            }
            Style::Text => clean.collect(),
        }
    }

    /// Text from the answer, such as a note: without control characters, and in Markdown without a cell's end.
    fn text(self, s: &str) -> String {
        let clean = s.chars().filter(|c| !c.is_control());
        match self {
            Style::Markdown => clean.map(|c| if c == '|' { '/' } else { c }).collect(),
            Style::Text => clean.collect(),
        }
    }

    fn strong(self, s: &str) -> String {
        match self {
            Style::Markdown => format!("**{s}**"),
            Style::Text => s.to_string(),
        }
    }

    /// A line of small print, such as the footer.
    pub fn small(self, s: &str) -> String {
        match self {
            Style::Markdown => format!("<sub>{s}</sub>"),
            Style::Text => s.to_string(),
        }
    }

    fn heading(self, s: &str) -> String {
        match self {
            Style::Markdown => format!("### qloudpulse: {s}"),
            Style::Text => format!("qloudpulse: {s}"),
        }
    }

    /// A list under a title, and a blank line after it.
    fn section(self, title: &str, lines: &[String]) -> String {
        match self {
            Style::Markdown => format!("**{title}**\n\n{}\n\n", lines.join("\n")),
            Style::Text => format!("{title}:\n{}\n\n", lines.join("\n")),
        }
    }

    /// A table: in Markdown with pipes, in text with columns padded to their widest cell, the figures to the right.
    fn table(self, header: &[&str], rows: &[Vec<String>], right: &[bool]) -> String {
        let mut out = String::new();
        match self {
            Style::Markdown => {
                let _ = writeln!(out, "| {} |", header.join(" | "));
                let _ = writeln!(out, "|{}", " --- |".repeat(header.len()));
                for row in rows {
                    let _ = writeln!(out, "| {} |", row.join(" | "));
                }
            }
            Style::Text => {
                let width = |i: usize| {
                    rows.iter()
                        .map(|r| r[i].chars().count())
                        .chain([header[i].chars().count()])
                        .max()
                        .unwrap_or(0)
                };
                let widths: Vec<usize> = (0..header.len()).map(width).collect();
                let line = |cells: Vec<&str>| {
                    let padded: Vec<String> = cells
                        .iter()
                        .enumerate()
                        .map(|(i, c)| {
                            let pad = " ".repeat(widths[i] - c.chars().count());
                            if right[i] {
                                format!("{pad}{c}")
                            } else {
                                format!("{c}{pad}")
                            }
                        })
                        .collect();
                    padded.join("  ").trim_end().to_string()
                };
                let _ = writeln!(out, "{}", line(header.to_vec()));
                for row in rows {
                    let _ = writeln!(out, "{}", line(row.iter().map(String::as_str).collect()));
                }
            }
        }
        out.push('\n');
        out
    }
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

/// The line on what the whole stack costs, under the headline, or why it is left out. None when the preview does not
/// list the stack: the hint under the comment then says how to get it.
fn stack_line(answers: &HashMap<&str, &ResourceEstimate>, estimate: &Estimate, style: Style) -> Option<String> {
    let label = style.strong("The stack:");
    if let Some(n) = estimate.stack_too_large {
        return Some(format!(
            "{label} its cost is left out: with every unchanged resource the request would hold {} resources, and \
             one estimate takes {} at most.",
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
        return Some(format!(
            "{label} its cost is left out: this endpoint does not price unchanged resources yet."
        ));
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
        return Some(format!("{label} none of its resources could be priced{unpriced}."));
    }
    Some(format!(
        "{label} about {} a month now, about {} after this change, from {}{unpriced}.",
        money(before),
        money(after),
        resources(priced, "priced")
    ))
}

/// Why some resources cannot be priced, and how to give them a region: for the comment and stderr.
pub fn no_region(n: usize, style: Style) -> String {
    let (count, verb, them) = if n == 1 {
        ("1 resource".to_string(), "has", "it")
    } else {
        (format!("{} resources", grouped(n as u64)), "have", "them")
    };
    format!(
        "{count} {verb} no AWS region, so qloudpulse cannot price {them}: neither the preview, its provider, the \
         stack's config nor this machine's AWS settings name one. Set one with {}, or run with {}.",
        style.code("pulumi config set aws:region <region>"),
        style.code("--region <region>")
    )
}

/// The Markdown comment for a change: what the change adds, from the resources it changes, and what the whole stack
/// costs, from those it leaves the same too.
pub fn comment(response: &Response, estimate: &Estimate) -> String {
    render(response, estimate, Style::Markdown)
}

/// The comment for a change in either style.
pub fn render(response: &Response, estimate: &Estimate, style: Style) -> String {
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
    let headline = if priced.is_empty() && locals.is_empty() && not_priced.is_empty() && !estimate.free.is_empty() {
        // Only what AWS charges nothing for: no cost, not "not priced".
        "this change adds no monthly cost".to_string()
    } else if priced.is_empty() {
        "nothing in this change could be priced".to_string()
    } else if delta >= 0.5 {
        format!("this change adds about {} a month", money(delta))
    } else if delta <= -0.5 {
        format!("this change saves about {} a month", money(-delta))
    } else {
        "this change moves the monthly cost by less than $1".to_string()
    };
    let _ = writeln!(out, "{}\n", style.heading(&headline));
    // In a pull request the link goes under the headline, where a reviewer looks first; at a terminal it is the last
    // line, where the eye ends up once the table has scrolled past.
    let link = page_link(response.url.as_deref(), response.stored, &estimate.locals);
    if let (Some(link), Style::Markdown) = (&link, style) {
        let _ = writeln!(out, "[See it in qloudpulse]({link})\n");
    }
    if let Some(line) = stack_line(&answers, estimate, style) {
        let _ = writeln!(out, "{line}\n");
    }
    if estimate.no_region > 0 {
        let _ = writeln!(
            out,
            "{} {}\n",
            style.strong("No region:"),
            no_region(estimate.no_region, style)
        );
    }

    if !priced.is_empty() {
        let rows: Vec<Vec<String>> = priced
            .iter()
            .map(|(l, a)| {
                vec![
                    format!("{} {}", style.code(&l.name), l.label),
                    l.change.label().to_string(),
                    cell(a.monthly_before),
                    cell(a.monthly_after),
                    basis(a.basis).to_string(),
                ]
            })
            .collect();
        out.push_str(&style.table(
            &["Resource", "Change", "Monthly now", "Monthly after", "Priced from"],
            &rows,
            &[false, false, true, true, false],
        ));
    }

    let notes_of = |kind: NoteKind| -> Vec<String> {
        let mut lines = Vec::new();
        for l in &locals {
            let Some(a) = answers.get(l.key.as_str()) else { continue };
            for n in a.notes.iter().filter(|n| n.kind == kind) {
                let mut line = format!("- {}: {}", style.code(&l.name), style.text(&n.text));
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
            out.push_str(&style.section(title, &lines));
        }
    }

    let mut unpriced: Vec<String> = Vec::new();
    for l in &locals {
        match answers.get(l.key.as_str()) {
            None => unpriced.push(format!("- {} {}: no answer for it", style.code(&l.name), l.label)),
            Some(a) if a.basis == Basis::NotPriced => {
                let reason = a.reason.as_deref().map_or("not priced".to_string(), |r| style.text(r));
                unpriced.push(format!("- {} {}: {reason}", style.code(&l.name), l.label));
            }
            Some(_) => {}
        }
    }
    for n in not_priced {
        unpriced.push(format!(
            "- {} × {}: not priced by qloudpulse yet",
            n.count,
            style.code(&n.type_token)
        ));
    }
    if !unpriced.is_empty() {
        out.push_str(&style.section("Not priced", &unpriced));
    }

    if !estimate.free.is_empty() {
        match style {
            Style::Markdown => {
                let lines: Vec<String> = estimate
                    .free
                    .iter()
                    .map(|f| format!("- {} × {}: {}", f.count, style.code(f.label()), style.text(&f.reason)))
                    .collect();
                out.push_str(&style.section("No charge of their own", &lines));
            }
            Style::Text => {
                let counts: Vec<String> = estimate
                    .free
                    .iter()
                    .map(|f| format!("{} × {}", f.count, style.code(f.label())))
                    .collect();
                let _ = writeln!(out, "{}: no charge of their own.\n", counts.join(", "));
            }
        }
    }

    if !estimate.stack_listed && estimate.stack_too_large.is_none() {
        let hint = format!(
            "Run {} to see the whole stack's monthly cost.",
            style.code("pulumi preview --json --show-sames")
        );
        let _ = writeln!(out, "{}\n", style.small(&hint));
    }

    let mut footer = String::from("Monthly figures in USD, before tax.");
    if let Some(id) = &response.estimate_id {
        let _ = write!(footer, " Estimate {}.", style.code(id));
    }
    if response.stored == Some(false) {
        footer.push_str(" qloudpulse kept nothing of it: there is no page to open.");
    }
    let _ = writeln!(out, "{}", style.small(&footer));
    // Only an endpoint from before ADR 0140 is sent the names in the link alone.
    if link.is_some() && style == Style::Markdown && response.stored.is_none() {
        let _ = writeln!(
            out,
            "{}",
            style.small(
                "The resource names in the link stay in your browser: they come after its #, which a browser never \
                 sends to a server."
            )
        );
    }
    if let (Some(link), Style::Text) = (&link, style) {
        let _ = writeln!(out, "\nSee it in qloudpulse: {link}");
    }
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
    fn names_cannot_break_the_table_or_the_terminal() {
        assert_eq!(Style::Markdown.code("a|b`c"), "`a/b'c`");
        assert_eq!(Style::Markdown.code("x\ny"), "`xy`");
        assert_eq!(Style::Text.code("x\ny\u{1b}[2J"), "xy[2J");
        assert_eq!(Style::Text.text("a\rb|c"), "ab|c");
    }

    #[test]
    fn a_text_table_pads_its_columns() {
        let rows = vec![
            vec!["orders RDS instance".to_string(), "$398".to_string()],
            vec!["data".to_string(), "$11".to_string()],
        ];
        assert_eq!(
            Style::Text.table(&["Resource", "Monthly"], &rows, &[false, true]),
            "Resource             Monthly\norders RDS instance     $398\ndata                     $11\n\n"
        );
    }
}
