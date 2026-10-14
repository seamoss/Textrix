//! Desktop notifications through the terminal itself (no helper programs):
//! OSC 9 (iTerm2, Ghostty, WezTerm), OSC 777 (Ghostty, WezTerm, urxvt) and
//! OSC 99 (kitty). Inside tmux the sequence is wrapped for passthrough,
//! which needs `set -g allow-passthrough on`.

use std::io::Write;

use crate::config::NotifyMode;

/// Pick a protocol for `Auto` from the environment.
pub fn resolve(mode: NotifyMode) -> NotifyMode {
    if mode != NotifyMode::Auto {
        return mode;
    }
    let program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    let term = std::env::var("TERM").unwrap_or_default();
    match program.as_str() {
        "iTerm.app" => NotifyMode::Osc9,
        "ghostty" | "WezTerm" => NotifyMode::Osc777,
        "Apple_Terminal" | "vscode" => NotifyMode::Off,
        _ if term.contains("kitty") || std::env::var_os("KITTY_WINDOW_ID").is_some() => NotifyMode::Osc99,
        _ if term.contains("ghostty") => NotifyMode::Osc777,
        _ => NotifyMode::Osc9,
    }
}

/// The escape sequence for one notification, or `None` when disabled.
pub fn sequence(mode: NotifyMode, title: &str, body: &str, in_tmux: bool) -> Option<String> {
    let title = clean(title, 80);
    let body = clean(body, 240);
    let seq = match mode {
        NotifyMode::Osc9 => format!("\x1b]9;{title}: {body}\x07"),
        NotifyMode::Osc777 => format!("\x1b]777;notify;{};{body}\x07", title.replace(';', ",")),
        NotifyMode::Osc99 => format!("\x1b]99;;{title}: {body}\x1b\\"),
        NotifyMode::Auto | NotifyMode::Off => return None,
    };
    Some(if in_tmux { format!("\x1bPtmux;{}\x1b\\", seq.replace('\x1b', "\x1b\x1b")) } else { seq })
}

pub fn send(mode: NotifyMode, title: &str, body: &str) {
    let in_tmux = std::env::var_os("TMUX").is_some();
    if let Some(seq) = sequence(mode, title, body, in_tmux) {
        let mut out = std::io::stdout();
        let _ = out.write_all(seq.as_bytes()).and_then(|()| out.flush());
    }
}

/// Put text on the system clipboard with OSC 52 (tmux forwards it when
/// `set-clipboard` is on).
pub fn copy_to_clipboard(text: &str) {
    let mut out = std::io::stdout();
    let _ = out.write_all(format!("\x1b]52;c;{}\x07", base64(text.as_bytes())).as_bytes()).and_then(|()| out.flush());
}

fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Message text can't be allowed to end the escape sequence early: drop
/// every control character and keep it short.
fn clean(s: &str, max: usize) -> String {
    let flat: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let mut out: String = flat.trim().chars().take(max).collect();
    if flat.trim().chars().count() > max {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_text_cannot_escape_the_sequence() {
        let seq = sequence(NotifyMode::Osc9, "room", "evil\x07\x1b]0;pwned\x07", false).unwrap();
        assert_eq!(seq, "\x1b]9;room: evil  ]0;pwned\x07");
        assert_eq!(seq.matches('\x07').count(), 1);
    }

    #[test]
    fn base64_matches_rfc4648() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn osc777_fields_and_tmux_wrapping() {
        let seq = sequence(NotifyMode::Osc777, "a;b", "hi", false).unwrap();
        assert_eq!(seq, "\x1b]777;notify;a,b;hi\x07");
        let wrapped = sequence(NotifyMode::Osc9, "t", "b", true).unwrap();
        assert_eq!(wrapped, "\x1bPtmux;\x1b\x1b]9;t: b\x07\x1b\\");
        assert!(sequence(NotifyMode::Off, "t", "b", false).is_none());
    }
}
