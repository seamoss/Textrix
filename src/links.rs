//! Finding and opening http(s) links in messages.

use std::{ops::Range, process::Stdio};

/// Byte ranges of http(s) URLs in `text`, in order. Trailing punctuation
/// that usually ends a sentence is left out (keeping balanced parentheses,
/// as in Wikipedia links).
pub fn url_ranges(text: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(found) = next_scheme(&text[from..]) {
        let start = from + found;
        let boundary = text[..start].chars().next_back().is_none_or(|c| !c.is_alphanumeric());
        let len = text[start..].find(|c: char| c.is_whitespace() || "<>\"`".contains(c)).unwrap_or(text.len() - start);
        let mut end = start + len;
        while end > start {
            let url = &text[start..end];
            let last = url.chars().next_back().unwrap_or(' ');
            let unbalanced = last == ')' && url.matches('(').count() < url.matches(')').count();
            if ".,;:!?'".contains(last) || unbalanced || last == ']' || last == '}' {
                end -= last.len_utf8();
            } else {
                break;
            }
        }
        if boundary && end > start + "https://".len() - 1 && is_safe(&text[start..end]) {
            out.push(start..end);
        }
        from = end.max(start + 1);
    }
    out
}

pub fn urls(text: &str) -> Vec<&str> {
    url_ranges(text).into_iter().map(|r| &text[r]).collect()
}

fn next_scheme(s: &str) -> Option<usize> {
    let lower = s.to_ascii_lowercase();
    match (lower.find("https://"), lower.find("http://")) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Only plain web links, nothing that could make the opener run a program.
pub fn is_safe(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://"))
        && url.len() <= 4096
        && url.len() > lower.find("://").map_or(0, |i| i + 3)
        && !url.chars().any(|c| c.is_control() || c.is_whitespace())
}

/// Hand a link to the desktop's browser, without a shell.
pub fn open(url: &str) -> std::io::Result<()> {
    if !is_safe(url) {
        return Err(std::io::Error::other("only http(s) links can be opened"));
    }
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    std::process::Command::new(opener)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_links_and_trims_sentence_punctuation() {
        assert_eq!(urls("see https://example.org/a, then http://x.io."), vec!["https://example.org/a", "http://x.io"]);
        assert_eq!(
            urls("(https://en.wikipedia.org/wiki/Rust_(language))"),
            vec!["https://en.wikipedia.org/wiki/Rust_(language)"]
        );
        assert_eq!(urls("<https://a.b/c>"), vec!["https://a.b/c"]);
        assert!(urls("nohttps://x.y and https:// alone").is_empty());
    }

    #[test]
    fn only_web_links_are_safe() {
        assert!(is_safe("https://example.org/?q=1"));
        assert!(!is_safe("file:///etc/passwd"));
        assert!(!is_safe("javascript:alert(1)"));
        assert!(!is_safe("https://a.b/\u{1b}]8;;x"));
    }
}
