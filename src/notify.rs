//! Desktop notifications through the terminal itself (no helper programs):
//! OSC 9 (iTerm2, Ghostty, WezTerm), OSC 777 (Ghostty, WezTerm, urxvt) and
//! OSC 99 (kitty). Inside tmux the sequence is wrapped for passthrough,
//! which needs `set -g allow-passthrough on`. In the background with no
//! terminal attached, the system shows them instead.

use crate::{config::NotifyMode, term};

/// Pick a protocol for `Auto` from the terminal Textrix shows in.
pub fn resolve(mode: NotifyMode) -> NotifyMode {
    if mode != NotifyMode::Auto {
        return mode;
    }
    let info = term::info();
    match info.term_program.as_str() {
        "iTerm.app" => NotifyMode::Osc9,
        "ghostty" | "WezTerm" => NotifyMode::Osc777,
        "Apple_Terminal" | "vscode" => NotifyMode::Off,
        _ if info.term.contains("kitty") || info.kitty => NotifyMode::Osc99,
        _ if info.term.contains("ghostty") => NotifyMode::Osc777,
        _ => NotifyMode::Osc9,
    }
}

/// A notification from the system (`osascript` on macOS, `notify-send`
/// elsewhere), for when no terminal is attached to show one. The text goes
/// as arguments, never as script.
pub fn send_system(title: &str, body: &str) {
    let (title, body) = (clean(title, 80), clean(body, 240));
    let mut command = if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("/usr/bin/osascript");
        c.args(["-e", "on run argv", "-e", "display notification (item 2 of argv) with title (item 1 of argv)"]);
        c.args(["-e", "end run", &title, &body]);
        c
    } else {
        let mut c = std::process::Command::new("notify-send");
        c.args(["--app-name=Textrix", "--", &title, &body]);
        c
    };
    let spawned = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    // Reap it without waiting here.
    if let Ok(mut child) = spawned {
        std::thread::spawn(move || child.wait());
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
    if let Some(seq) = sequence(mode, title, body, term::info().tmux) {
        term::write(seq.as_bytes());
    }
}

/// Set the terminal's title (the tab or window name; inside tmux, the
/// pane's). Room names can't escape it: control characters are dropped.
pub fn title_sequence(title: &str) -> String {
    format!("\x1b]2;{}\x07", clean(title, 200))
}

pub fn set_title(title: &str) {
    write(&title_sequence(title));
}

/// Save the title the terminal had (xterm's title stack, which most
/// terminals keep), and put it back on exit.
pub fn push_title() {
    write("\x1b[22;0t");
}

pub fn pop_title() {
    write("\x1b[23;0t");
}

fn write(seq: &str) {
    term::write(seq.as_bytes());
}

/// Put text on the system clipboard with OSC 52 (tmux forwards it when
/// `set-clipboard` is on).
pub fn copy_to_clipboard(text: &str) {
    write(&format!("\x1b]52;c;{}\x07", base64(text.as_bytes())));
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
    fn room_names_cannot_escape_the_title() {
        let title = title_sequence("evil\x07\x1b]0;pwned\x1b\\ room\u{9c}x");
        assert_eq!(title, "\x1b]2;evil  ]0;pwned \\ room x\x07");
        assert_eq!(title.matches('\x1b').count(), 1, "only the sequence's own escape");
    }

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
