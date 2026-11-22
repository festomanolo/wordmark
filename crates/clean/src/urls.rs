//! Web addresses in text: finding them and taking them out.

use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// What happens to links and web addresses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UrlAction {
    /// Links and addresses stay.
    Keep,
    /// Links stop being links; their text stays. Addresses typed as text stay.
    Unlink,
    /// Links stop being links and every web address in the text is deleted.
    #[default]
    Remove,
}

impl UrlAction {
    pub fn parse(s: &str) -> Option<UrlAction> {
        match s {
            "keep" => Some(UrlAction::Keep),
            "unlink" => Some(UrlAction::Unlink),
            "remove" => Some(UrlAction::Remove),
            _ => None,
        }
    }
}

/// Top-level domains a bare address (`example.com/page`, no scheme) must end in to count, so
/// "e.g." or "Fig.3" never do.
const TLDS: &str = "com|org|net|edu|gov|mil|int|info|biz|io|co|ai|app|dev|me|ly|tv|us|uk|de|fr|es|it|nl|eu|ru|cn|jp|in|br|au|ca|ch|se|no|fi|pl|tz|ke|za|ng|xyz|site|online|top";

static URL: LazyLock<Option<Regex>> = LazyLock::new(|| {
    let pat = format!(
        r#"(?i)(?:\b(?:https?|ftp)://|\bwww\.)[^\s<>"'(){{}}\[\]]+|\b[a-z0-9](?:[a-z0-9-]{{0,61}}[a-z0-9])?(?:\.[a-z0-9](?:[a-z0-9-]{{0,61}}[a-z0-9])?)*\.(?:{TLDS})\b(?:/[^\s<>"'(){{}}\[\]]*)?"#
    );
    Regex::new(&pat).ok()
});

/// Where web addresses are in `text` (byte ranges), trailing sentence punctuation left out.
pub fn find_urls(text: &str) -> Vec<std::ops::Range<usize>> {
    let Some(re) = URL.as_ref() else { return Vec::new() };
    re.find_iter(text)
        .filter_map(|m| {
            // An email address is not a web address: leave `name@example.com` alone.
            if text[..m.start()].ends_with('@') {
                return None;
            }
            let s = m.as_str();
            let trimmed = s.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '\'', '"']);
            (!trimmed.is_empty()).then(|| m.start()..m.start() + trimmed.len())
        })
        .collect()
}

pub fn contains_url(text: &str) -> bool {
    !find_urls(text).is_empty()
}

/// `text` with its web addresses deleted, and the spaces they leave tidied. Returns the text and
/// how many addresses went.
pub fn strip_urls(text: &str) -> (String, usize) {
    let found = find_urls(text);
    if found.is_empty() {
        return (text.to_string(), 0);
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for r in &found {
        out.push_str(&text[at..r.start]);
        at = r.end;
    }
    out.push_str(&text[at..]);
    (tidy_spaces(&out), found.len())
}

/// Collapse doubled spaces, and drop spaces before punctuation, left where text was cut out.
pub fn tidy_spaces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == ' ' && (out.ends_with(' ') || out.is_empty()) {
            continue;
        }
        if matches!(c, '.' | ',' | ';' | ':' | ')') && out.ends_with(' ') {
            out.pop();
        }
        out.push(c);
    }
    out.trim_end().to_string()
}
