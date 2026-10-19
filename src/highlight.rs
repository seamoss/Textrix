//! Syntax colouring for code blocks, in pure Rust: syntect with bat's
//! language definitions (two-face). Code is sorted into a few kinds
//! (keywords, strings, comments…) and the theme picks their colours, so a
//! monochrome theme stays one colour.

use std::{str::FromStr, sync::OnceLock};

use syntect::{
    highlighting::{
        Color, HighlightIterator, HighlightState, Highlighter, ScopeSelectors, StyleModifier, Theme, ThemeItem,
        ThemeSettings,
    },
    parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet},
    util::LinesWithEndings,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Syntax {
    Comment,
    Keyword,
    String,
    Number,
    Function,
    Type,
}

impl Syntax {
    pub const ALL: [Syntax; 6] =
        [Syntax::Comment, Syntax::Keyword, Syntax::String, Syntax::Number, Syntax::Function, Syntax::Type];

    /// As in the `[colors]` keys (`code_keyword`).
    pub fn name(self) -> &'static str {
        match self {
            Syntax::Comment => "comment",
            Syntax::Keyword => "keyword",
            Syntax::String => "string",
            Syntax::Number => "number",
            Syntax::Function => "function",
            Syntax::Type => "type",
        }
    }
}

/// Longer code stays plain: a pasted log doesn't need colours.
const MAX_CODE: usize = 32 * 1024;

/// Which kind each TextMate scope is. syntect picks the most specific
/// match, so "keyword.operator" beats "keyword", and the quotes around a
/// string ("punctuation.definition.string") stay with the string.
const SCOPES: &[(&str, Option<Syntax>)] = &[
    ("comment, punctuation.definition.comment", Some(Syntax::Comment)),
    ("keyword, storage, variable.language", Some(Syntax::Keyword)),
    ("keyword.operator, punctuation", None),
    ("string, constant.character, punctuation.definition.string, markup.raw", Some(Syntax::String)),
    ("constant.numeric, constant.language, constant.other, support.constant", Some(Syntax::Number)),
    ("entity.name.function, support.function, variable.function, support.macro", Some(Syntax::Function)),
    (
        "entity.name.type, entity.name.class, entity.name.struct, entity.name.enum, entity.name.trait, \
         entity.name.tag, entity.other.inherited-class, support.type, support.class, storage.type.primitive",
        Some(Syntax::Type),
    ),
];

fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(two_face::syntax::extra_newlines)
}

/// A syntect theme whose "colours" are our kinds: red 0 is plain, 1… is
/// `Syntax::ALL[n - 1]`.
fn kinds() -> &'static Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();
    THEME.get_or_init(|| {
        let code = |kind: Option<Syntax>| {
            let r = kind.map_or(0, |k| Syntax::ALL.iter().position(|&s| s == k).unwrap_or(0) as u8 + 1);
            Color { r, g: 0, b: 0, a: 0xff }
        };
        let scopes = SCOPES
            .iter()
            .map(|&(selector, kind)| ThemeItem {
                scope: ScopeSelectors::from_str(selector).expect("valid scope selectors"),
                style: StyleModifier { foreground: Some(code(kind)), background: None, font_style: None },
            })
            .collect();
        Theme {
            settings: ThemeSettings { foreground: Some(code(None)), ..ThemeSettings::default() },
            scopes,
            ..Theme::default()
        }
    })
}

/// Load the language definitions now (they take a moment), so the first
/// code block doesn't wait for them.
pub fn warm_up() {
    std::thread::spawn(|| {
        syntaxes();
        kinds();
    });
}

/// The syntax for a code block's language ("rust", "py", "shell"…), or
/// failing that a first line that gives it away (`#!/bin/sh`).
fn syntax_for<'a>(set: &'a SyntaxSet, code: &str, language: Option<&str>) -> Option<&'a SyntaxReference> {
    let Some(language) = language else {
        return set.find_syntax_by_first_line(code.lines().next()?);
    };
    let language = language.to_lowercase();
    let token = match language.as_str() {
        "text" | "plain" | "plaintext" | "txt" | "output" => return None,
        "shell" | "console" | "shellsession" | "zsh" | "sh" => "bash",
        "golang" => "go",
        "c++" => "cpp",
        "py3" | "python3" => "py",
        "jsonc" | "json5" => "json",
        "js" | "jsx" | "mjs" | "node" => "javascript",
        other => other,
    };
    set.find_syntax_by_token(token)
}

/// Code split into runs of one kind (`None`: plain), or `None` when the
/// language isn't known.
pub fn highlight(code: &str, language: Option<&str>) -> Option<Vec<(Option<Syntax>, String)>> {
    if code.len() > MAX_CODE {
        return None;
    }
    let set = syntaxes();
    let syntax = syntax_for(set, code, language)?;
    let highlighter = Highlighter::new(kinds());
    let mut parse = ParseState::new(syntax);
    let mut state = HighlightState::new(&highlighter, ScopeStack::new());
    let mut runs: Vec<(Option<Syntax>, String)> = Vec::new();
    for line in LinesWithEndings::from(code) {
        let ops = parse.parse_line(line, set).ok()?;
        for (style, text) in HighlightIterator::new(&mut state, &ops, line, &highlighter) {
            let kind = Syntax::ALL.get(usize::from(style.foreground.r).wrapping_sub(1)).copied();
            match runs.last_mut() {
                Some((last, run)) if *last == kind || text.trim().is_empty() => run.push_str(text),
                _ => runs.push((kind, text.to_owned())),
            }
        }
    }
    Some(runs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind_of(runs: &[(Option<Syntax>, String)], word: &str) -> Option<Syntax> {
        runs.iter().find(|(_, text)| text.trim() == word).unwrap_or_else(|| panic!("no {word} in {runs:?}")).0
    }

    #[test]
    fn code_is_sorted_into_kinds() {
        let code = "// add them\nfn add(a: u32) -> u32 {\n    let s = \"hi\";\n    a + 42\n}";
        let runs = highlight(code, Some("rust")).unwrap();
        assert_eq!(runs.iter().map(|(_, t)| t.as_str()).collect::<String>(), code, "nothing lost");
        assert_eq!(kind_of(&runs, "// add them"), Some(Syntax::Comment));
        assert_eq!(kind_of(&runs, "let"), Some(Syntax::Keyword));
        assert_eq!(kind_of(&runs, "\"hi\""), Some(Syntax::String));
        assert_eq!(kind_of(&runs, "42"), Some(Syntax::Number));
        assert_eq!(kind_of(&runs, "add"), Some(Syntax::Function));

        let py = highlight("def f():\n    return None  # nothing", Some("python")).unwrap();
        assert_eq!(kind_of(&py, "def"), Some(Syntax::Keyword));
        assert_eq!(kind_of(&py, "# nothing"), Some(Syntax::Comment));
        assert!(highlight("x = 1", Some("toml")).is_some(), "bat's extra languages are there");
        assert!(highlight("echo hi", Some("shell")).is_some(), "common names for languages work");
        assert!(highlight("#!/bin/sh\necho hi", None).is_some(), "a shebang gives it away");
        assert!(highlight("just words", None).is_none());
        assert!(highlight("just words", Some("text")).is_none());
        assert!(highlight("x", Some("no-such-language")).is_none());
    }
}
