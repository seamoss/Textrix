//! `:shortcode:` emoji (GitHub/Slack names, e.g. `:fire:`, `:+1:`).

use std::borrow::Cow;

/// Replace known `:shortcode:`s with their emoji, leaving code spans and
/// blocks (backticks) and unknown names (`12:30:45`) alone.
pub fn expand(text: &str) -> Cow<'_, str> {
    if !text.contains(':') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    // Length of the backtick run that opened the current code span.
    let mut fence: Option<usize> = None;
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if c == '`' {
            let run = rest.len() - rest.trim_start_matches('`').len();
            fence = match fence {
                None => Some(run),
                Some(open) if open == run => None,
                keep => keep,
            };
            out.push_str(&rest[..run]);
            rest = &rest[run..];
            continue;
        }
        if c == ':'
            && fence.is_none()
            && let Some((len, emoji)) = shortcode_at(rest)
        {
            out.push_str(emoji);
            rest = &rest[len..];
            changed = true;
            continue;
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    if changed { Cow::Owned(out) } else { Cow::Borrowed(text) }
}

/// `:name:` at the start of `s`: (its length, the emoji).
fn shortcode_at(s: &str) -> Option<(usize, &'static str)> {
    let len = s[1..].find(':')?;
    let name = &s[1..1 + len];
    if !is_shortcode(name) {
        return None;
    }
    Some((len + 2, emojis::get_by_shortcode(name)?.as_str()))
}

/// Emoji whose shortcodes start with `prefix`: (emoji, shortcode), exact
/// match first, then shortest names.
pub fn complete(prefix: &str) -> Vec<(&'static str, &'static str)> {
    let prefix = prefix.to_lowercase();
    if prefix.is_empty() || !is_shortcode(&prefix) {
        return Vec::new();
    }
    let mut out: Vec<(&str, &str)> = emojis::iter()
        .flat_map(|e| e.shortcodes().map(move |code| (e.as_str(), code)))
        .filter(|(_, code)| code.starts_with(&prefix))
        .collect();
    out.sort_by_key(|(_, code)| (*code != prefix, code.len(), *code));
    let mut seen = std::collections::HashSet::new();
    out.retain(|(emoji, _)| seen.insert(*emoji));
    out.truncate(40);
    out
}

fn is_shortcode(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || "_+-".contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_known_shortcodes_outside_code() {
        assert_eq!(expand("nice :+1: :fire:!"), "nice 👍 🔥!");
        assert_eq!(expand(":thumbsup::tada:"), "👍🎉");
        assert_eq!(expand("at 12:30:45 see :nope_not_real: ok"), "at 12:30:45 see :nope_not_real: ok");
        assert_eq!(expand("run `a :fire: b` then :fire:"), "run `a :fire: b` then 🔥");
        assert_eq!(expand("```\n:fire:\n```\n:fire:"), "```\n:fire:\n```\n🔥");
        assert_eq!(expand("no colon here"), "no colon here");
        assert_eq!(expand("smile: :) :"), "smile: :) :");
    }

    #[test]
    fn completes_by_prefix_exact_first() {
        let fire = complete("fire");
        assert_eq!(fire[0], ("🔥", "fire"));
        assert!(complete("thumbs").iter().any(|(e, _)| *e == "👍"));
        assert!(complete("").is_empty());
        assert!(complete("a b").is_empty());
        assert!(complete("zzzzqq").is_empty());
    }
}
