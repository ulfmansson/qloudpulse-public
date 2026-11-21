//! Raw and redacted answers side by side: both pretty-printed the same way, then lined up by a line diff, so a
//! removed tag leaves a gap on the right and a changed value sits next to the original.

use quick_xml::events::Event;
use quick_xml::{Reader, Writer};

/// Answers longer than this are shown without lining up: the diff is quadratic.
const MAX_LINES: usize = 4000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Same,
    Changed,
    Removed,
    Added,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub left: Option<String>,
    pub right: Option<String>,
    pub kind: Kind,
}

impl Row {
    pub fn class(&self) -> &'static str {
        match self.kind {
            Kind::Same => "",
            Kind::Changed => "chg",
            Kind::Removed => "drop",
            Kind::Added => "add",
        }
    }
}

/// JSON indented, XML indented, anything else as it is.
pub fn pretty(body: &str) -> String {
    let trimmed = body.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
            return serde_json::to_string_pretty(&v).unwrap_or_else(|_| body.to_string());
        }
    } else if trimmed.starts_with('<')
        && let Some(x) = pretty_xml(body)
    {
        return x;
    }
    body.to_string()
}

fn pretty_xml(body: &str) -> Option<String> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);
    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(e) => writer.write_event(e).ok()?,
            Err(_) => return None,
        }
    }
    String::from_utf8(writer.into_inner()).ok()
}

/// Lines both sides share stay side by side; a run of removed lines next to a run of added ones pairs up as changed.
pub fn side_by_side(left: &str, right: &str) -> Vec<Row> {
    let a: Vec<&str> = left.lines().collect();
    let b: Vec<&str> = right.lines().collect();
    if a.len() > MAX_LINES || b.len() > MAX_LINES {
        let n = a.len().max(b.len());
        return (0..n)
            .map(|i| {
                let (l, r) = (a.get(i).map(|s| s.to_string()), b.get(i).map(|s| s.to_string()));
                let kind = if l == r { Kind::Same } else { Kind::Changed };
                Row {
                    left: l,
                    right: r,
                    kind,
                }
            })
            .collect();
    }
    // Longest common subsequence, from the end.
    let (n, m) = (a.len(), b.len());
    let mut lcs = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut rows = Vec::new();
    let (mut removed, mut added): (Vec<&str>, Vec<&str>) = (Vec::new(), Vec::new());
    let flush = |rows: &mut Vec<Row>, removed: &mut Vec<&str>, added: &mut Vec<&str>| {
        let k = removed.len().max(added.len());
        for i in 0..k {
            let (l, r) = (removed.get(i), added.get(i));
            let kind = match (l, r) {
                (Some(_), Some(_)) => Kind::Changed,
                (Some(_), None) => Kind::Removed,
                _ => Kind::Added,
            };
            rows.push(Row {
                left: l.map(|s| s.to_string()),
                right: r.map(|s| s.to_string()),
                kind,
            });
        }
        removed.clear();
        added.clear();
    };
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            flush(&mut rows, &mut removed, &mut added);
            rows.push(Row {
                left: Some(a[i].to_string()),
                right: Some(b[j].to_string()),
                kind: Kind::Same,
            });
            i += 1;
            j += 1;
        } else if j < m && (i == n || lcs[i][j + 1] >= lcs[i + 1][j]) {
            added.push(b[j]);
            j += 1;
        } else {
            removed.push(a[i]);
            i += 1;
        }
    }
    flush(&mut rows, &mut removed, &mut added);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pretty_prints_xml_and_json() {
        let x = pretty("<R><a>1</a><b><c>2</c></b></R>");
        assert_eq!(x, "<R>\n  <a>1</a>\n  <b>\n    <c>2</c>\n  </b>\n</R>");
        assert_eq!(pretty(r#"{"a":1}"#), "{\n  \"a\": 1\n}");
        assert_eq!(pretty("plain"), "plain");
    }

    #[test]
    fn removed_lines_leave_a_gap_and_changes_pair_up() {
        let rows = side_by_side("a\nkey\nTeam\nb\nc", "a\nb\nC");
        let kinds: Vec<Kind> = rows.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            vec![Kind::Same, Kind::Removed, Kind::Removed, Kind::Same, Kind::Changed]
        );
        assert_eq!(rows[4].left.as_deref(), Some("c"));
        assert_eq!(rows[4].right.as_deref(), Some("C"));
        assert!(rows[1].right.is_none());
    }
}
