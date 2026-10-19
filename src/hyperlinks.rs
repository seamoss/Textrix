//! Real hyperlinks (OSC 8) over the links on screen, so the terminal knows
//! where each goes: hover shows it, Cmd/Ctrl+click opens it, and a long
//! link wrapped over several lines is still one link. After each frame the
//! links' cells are printed again, as they are, inside the hyperlink
//! sequence; terminals without hyperlinks ignore it.

use std::fmt::Write;

use ratatui::{
    buffer::Buffer,
    style::{Color, Modifier, Style},
};

/// A stretch of one row that's (part of) a link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub url: String,
}

/// The sequences that turn `links` on screen into hyperlinks, leaving the
/// cursor and colours as they were.
pub fn overlay(buffer: &Buffer, links: &[Link]) -> String {
    let mut out = String::new();
    for link in links {
        // Nothing that could end the sequence early gets in.
        if link.url.chars().any(char::is_control) || link.width == 0 {
            continue;
        }
        if out.is_empty() {
            out.push_str("\x1b7"); // save the cursor (and colours)
        }
        let _ = write!(out, "\x1b[{};{}H\x1b]8;;{}\x1b\\", link.y + 1, link.x + 1, link.url);
        let mut style = None;
        for x in link.x..link.x.saturating_add(link.width) {
            let Some(cell) = buffer.cell((x, link.y)) else { break };
            // The second column of a wide character prints nothing.
            if cell.symbol().is_empty() {
                continue;
            }
            if style != Some(cell.style()) {
                style = Some(cell.style());
                out.push_str(&sgr(cell.style()));
            }
            out.push_str(cell.symbol());
        }
        out.push_str("\x1b]8;;\x1b\\\x1b[0m");
    }
    if !out.is_empty() {
        out.push_str("\x1b8"); // and back
    }
    out
}

/// A style as a "select graphic rendition" sequence, from a clean slate.
fn sgr(style: Style) -> String {
    let mut codes = vec!["0".to_owned()];
    for (modifier, code) in [
        (Modifier::BOLD, "1"),
        (Modifier::DIM, "2"),
        (Modifier::ITALIC, "3"),
        (Modifier::UNDERLINED, "4"),
        (Modifier::REVERSED, "7"),
        (Modifier::CROSSED_OUT, "9"),
    ] {
        if style.add_modifier.contains(modifier) {
            codes.push(code.to_owned());
        }
    }
    codes.extend(style.fg.and_then(|c| color(c, 30)));
    codes.extend(style.bg.and_then(|c| color(c, 40)));
    format!("\x1b[{}m", codes.join(";"))
}

/// A colour's code: `base` 30 for the foreground, 40 for the background.
fn color(color: Color, base: u8) -> Option<String> {
    let named = |n: u8| Some((base + n).to_string());
    let bright = |n: u8| Some((base + 60 + n).to_string());
    match color {
        Color::Reset => Some((base + 9).to_string()),
        Color::Black => named(0),
        Color::Red => named(1),
        Color::Green => named(2),
        Color::Yellow => named(3),
        Color::Blue => named(4),
        Color::Magenta => named(5),
        Color::Cyan => named(6),
        Color::Gray => named(7),
        Color::DarkGray => bright(0),
        Color::LightRed => bright(1),
        Color::LightGreen => bright(2),
        Color::LightYellow => bright(3),
        Color::LightBlue => bright(4),
        Color::LightMagenta => bright(5),
        Color::LightCyan => bright(6),
        Color::White => bright(7),
        Color::Indexed(n) => Some(format!("{};5;{n}", base + 8)),
        Color::Rgb(r, g, b) => Some(format!("{};2;{r};{g};{b}", base + 8)),
    }
}

#[cfg(test)]
mod tests {
    use ratatui::layout::Rect;

    use super::*;

    #[test]
    fn links_are_reprinted_inside_hyperlinks() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 30, 2));
        let style = Style::new().fg(Color::Blue).add_modifier(Modifier::UNDERLINED);
        buffer.set_string(4, 1, "example.org/a", style);
        let link = Link { x: 4, y: 1, width: 13, url: "https://example.org/a".into() };
        let out = overlay(&buffer, std::slice::from_ref(&link));
        assert!(out.starts_with("\x1b7\x1b[2;5H\x1b]8;;https://example.org/a\x1b\\"), "{out:?}");
        assert!(out.contains("\x1b[0;4;34;49mexample.org/a\x1b]8;;"), "the cells as they look: {out:?}");
        assert!(out.ends_with("\x1b]8;;\x1b\\\x1b[0m\x1b8"));

        let sneaky = Link { url: "https://x.org/\x1b]2;pwned\x07".into(), ..link };
        assert_eq!(overlay(&buffer, &[sneaky]), "", "a URL can't escape the sequence");
        assert_eq!(overlay(&buffer, &[]), "");
    }

    #[test]
    fn colours_become_sgr_codes() {
        assert_eq!(sgr(Style::new()), "\x1b[0m");
        assert_eq!(sgr(Style::new().fg(Color::Indexed(46)).bg(Color::Rgb(1, 2, 3))), "\x1b[0;38;5;46;48;2;1;2;3m");
        assert_eq!(sgr(Style::new().fg(Color::LightCyan).add_modifier(Modifier::BOLD)), "\x1b[0;1;96m");
    }
}
