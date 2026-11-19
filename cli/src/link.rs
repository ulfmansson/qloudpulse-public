//! The link from the comment to the estimate's page in qloudpulse (qloudpulse's ADR 0133): the answer's `url`, and
//! after `#` the names of the resources sent, `names=<base64url of {"r…": "name"}>`. A browser never sends what follows
//! `#` to a server, so the names reach only the browser of whoever opens the link; the page (`namesFromHash`) shows
//! them beside the keys qloudpulse holds.

use serde_json::{Map, Value};

use crate::pulumi::Local;
use crate::request::ChangeKind;

/// The longest link the client writes, so it stays within what browsers and pull request hosts take.
pub const MAX_LINK: usize = 8000;
/// The longest name the page takes.
pub const MAX_NAME: usize = 300;

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Base64url without padding, as the page decodes it.
pub fn base64url(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(encoded_len(bytes.len()));
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(chunk.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        for i in 0..=chunk.len() {
            out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

/// How long [`base64url`] makes `n` bytes.
fn encoded_len(n: usize) -> usize {
    (4 * n).div_ceil(3)
}

/// A URL the comment can show as it is: http(s), and nothing that ends a Markdown link or a line.
fn is_plain_url(url: &str) -> bool {
    (url.starts_with("https://") || url.starts_with("http://"))
        && url
            .chars()
            .all(|c| c.is_ascii_graphic() && !matches!(c, '(' | ')' | '<' | '>' | '[' | ']' | '`' | '"' | '\'' | '\\'))
}

/// A name as the page takes it: no control characters, at most [`MAX_NAME`] characters, not empty.
fn page_name(name: &str) -> Option<String> {
    let clean: String = name.chars().filter(|c| !c.is_control()).take(MAX_NAME).collect();
    (!clean.is_empty()).then_some(clean)
}

/// The link to the estimate's page with the names of what was sent: the changed resources' always, then the unchanged
/// ones while the link stays within [`MAX_LINK`]. None without a URL the comment can show.
pub fn page_link(url: Option<&str>, locals: &[Local]) -> Option<String> {
    let url = url.filter(|u| is_plain_url(u))?;
    let changed = locals.iter().filter(|l| l.change != ChangeKind::Same);
    let same = locals.iter().filter(|l| l.change == ChangeKind::Same);

    let mut names = Map::new();
    // The JSON's length as it grows: `{}`, and per entry `"key":"name"` and a comma between.
    let mut json_len = 2;
    let base = url.len() + "#names=".len();
    for (l, must) in changed.map(|l| (l, true)).chain(same.map(|l| (l, false))) {
        let Some(name) = page_name(&l.name) else { continue };
        if names.contains_key(&l.key) {
            continue;
        }
        let entry = Value::String(l.key.clone()).to_string().len() + 1 + Value::String(name.clone()).to_string().len();
        let grown = json_len + entry + usize::from(!names.is_empty());
        if !must && base + encoded_len(grown) > MAX_LINK {
            break;
        }
        json_len = grown;
        names.insert(l.key.clone(), Value::String(name));
    }
    if names.is_empty() {
        return Some(url.to_string());
    }
    let json = Value::Object(names).to_string();
    debug_assert_eq!(json.len(), json_len);
    let joiner = if url.contains('#') { '&' } else { '#' };
    Some(format!("{url}{joiner}names={}", base64url(json.as_bytes())))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The page's decoding, `namesFromHash`, for the round trip.
    fn decode(link: &str) -> Map<String, Value> {
        let encoded = link.split_once("names=").unwrap().1;
        let mut bits = 0u32;
        let mut count = 0;
        let mut bytes = Vec::new();
        for c in encoded.bytes() {
            let v = ALPHABET.iter().position(|&a| a == c).unwrap() as u32;
            bits = (bits << 6) | v;
            count += 6;
            if count >= 8 {
                count -= 8;
                bytes.push((bits >> count) as u8);
                bits &= (1 << count) - 1;
            }
        }
        serde_json::from_slice::<Value>(&bytes)
            .unwrap()
            .as_object()
            .unwrap()
            .clone()
    }

    fn local(i: usize, name: &str, change: ChangeKind) -> Local {
        Local {
            key: format!("r{i:016x}"),
            name: name.to_string(),
            label: "EBS volume".to_string(),
            change,
        }
    }

    #[test]
    fn base64url_without_padding() {
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(b"foob"), "Zm9vYg");
        assert_eq!(base64url(&[0xfb, 0xff, 0xbf]), "-_-_");
        for n in 0..10 {
            assert_eq!(base64url(&vec![0; n]).len(), encoded_len(n));
        }
    }

    #[test]
    fn the_names_go_round_and_come_back() {
        let locals = [
            local(1, "orders", ChangeKind::Update),
            local(2, "wëb \"api\"\u{1b}[2J", ChangeKind::Same),
            local(3, "", ChangeKind::Create),
        ];
        let link = page_link(Some("https://app.qloudpulse.com/ui/?estimate=est_01"), &locals).unwrap();
        assert!(
            link.starts_with("https://app.qloudpulse.com/ui/?estimate=est_01#names="),
            "{link}"
        );
        assert!(
            link["https://".len()..]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "/.?=_#-".contains(c))
        );
        let names = decode(&link);
        assert_eq!(
            Value::Object(names),
            serde_json::json!({"r0000000000000001": "orders", "r0000000000000002": "wëb \"api\"[2J"}),
            "the changed one first, control characters dropped, an empty name left out"
        );
    }

    #[test]
    fn a_long_name_is_cut_to_what_the_page_takes() {
        let long = "n".repeat(400);
        let link = page_link(
            Some("https://q.example/ui/?estimate=e"),
            &[local(1, &long, ChangeKind::Update)],
        )
        .unwrap();
        assert_eq!(decode(&link)["r0000000000000001"].as_str().unwrap().len(), MAX_NAME);
    }

    #[test]
    fn the_unchanged_stop_before_the_cap_and_the_changed_always_go() {
        let url = "https://app.qloudpulse.com/ui/?estimate=est_01";
        let mut locals: Vec<Local> = (0..40)
            .map(|i| local(i, &format!("changed-{i}"), ChangeKind::Update))
            .collect();
        locals.extend((100..2100).map(|i| local(i, &format!("volume-{i}"), ChangeKind::Same)));
        let link = page_link(Some(url), &locals).unwrap();
        assert!(link.len() <= MAX_LINK, "{}", link.len());
        let names = decode(&link);
        assert!((0..40).all(|i| names.contains_key(&format!("r{i:016x}"))));
        assert!(names.len() > 40 && names.len() < locals.len(), "{}", names.len());
        let next = local(100 + names.len() - 40, "volume-x", ChangeKind::Same);
        assert!(!names.contains_key(&next.key), "they stop, in order");

        // So many changed ones that they pass the cap alone: they still all go.
        let changed: Vec<Local> = (0..400)
            .map(|i| local(i, &"c".repeat(30), ChangeKind::Update))
            .collect();
        let link = page_link(Some(url), &changed).unwrap();
        assert!(link.len() > MAX_LINK);
        assert_eq!(decode(&link).len(), 400);
    }

    #[test]
    fn no_url_or_one_the_comment_cannot_show_is_no_link() {
        let locals = [local(1, "orders", ChangeKind::Update)];
        assert_eq!(page_link(None, &locals), None);
        for bad in [
            "javascript:alert(1)",
            "ftp://q.example/x",
            "https://q.example/x) [evil](https://e.example",
            "https://q.example/a b",
            "https://q.example/\n",
        ] {
            assert_eq!(page_link(Some(bad), &locals), None, "{bad:?}");
        }
        assert_eq!(
            page_link(Some("https://q.example/ui/?estimate=e"), &[]).as_deref(),
            Some("https://q.example/ui/?estimate=e")
        );
        assert!(
            page_link(Some("https://q.example/ui/?estimate=e#tab"), &locals)
                .unwrap()
                .contains("#tab&names=")
        );
    }
}
