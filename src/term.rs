//! Where terminal control sequences go (the title, the bell, notifications,
//! the clipboard, mouse reporting): straight to stdout, or, when Textrix runs
//! in the background, to the terminal attached to it, and nowhere while none
//! is.

use std::{
    io::Write,
    sync::{LazyLock, Mutex},
};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

use crate::remote::ToClient;

/// What a terminal says about itself in its environment, which decides
/// how to notify through it and whether to wrap sequences for tmux.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TermInfo {
    pub term: String,
    pub term_program: String,
    pub kitty: bool,
    pub tmux: bool,
    /// 24-bit colour (`COLORTERM=truecolor`).
    pub truecolor: bool,
    /// Whether the background is light, once the terminal said (or
    /// `COLORFGBG` does), for `theme = "auto"`.
    #[serde(default)]
    pub light: Option<bool>,
}

impl TermInfo {
    pub fn from_env() -> Self {
        Self {
            term: std::env::var("TERM").unwrap_or_default(),
            term_program: std::env::var("TERM_PROGRAM").unwrap_or_default(),
            kitty: std::env::var_os("KITTY_WINDOW_ID").is_some(),
            tmux: std::env::var_os("TMUX").is_some(),
            truecolor: std::env::var("COLORTERM").is_ok_and(|c| c == "truecolor" || c == "24bit"),
            light: std::env::var("COLORFGBG").ok().and_then(|v| light_from_colorfgbg(&v)),
        }
    }
}

/// `COLORFGBG` ("15;0", rxvt's and Konsole's): the last number is the
/// background's colour, 7 and 15 being the light ones.
fn light_from_colorfgbg(value: &str) -> Option<bool> {
    let background: u8 = value.rsplit(';').next()?.trim().parse().ok()?;
    Some(matches!(background, 7 | 15))
}

/// Whether a background colour is a light one.
pub fn is_light((r, g, b): (u8, u8, u8)) -> bool {
    0.2126 * f64::from(r) + 0.7152 * f64::from(g) + 0.0722 * f64::from(b) > 127.5
}

#[cfg(test)]
#[test]
fn light_and_dark_backgrounds() {
    assert_eq!(light_from_colorfgbg("15;0"), Some(false));
    assert_eq!(light_from_colorfgbg("0;default;15"), Some(true));
    assert_eq!(light_from_colorfgbg("default;default"), None);
    assert!(is_light((0xfd, 0xf6, 0xe3)) && !is_light((0x28, 0x2a, 0x36)));
}

/// What the terminal said about its background (`None`: it didn't).
pub fn set_light(light: Option<bool>) {
    if light.is_some() {
        INFO.lock().unwrap().light = light;
    }
}

enum Sink {
    Stdout,
    /// Background mode: the attached terminal, if one is.
    Remote(Option<UnboundedSender<ToClient>>),
}

static SINK: LazyLock<Mutex<Sink>> = LazyLock::new(|| Mutex::new(Sink::Stdout));
static INFO: LazyLock<Mutex<TermInfo>> = LazyLock::new(|| Mutex::new(TermInfo::from_env()));

pub fn write(bytes: &[u8]) {
    match &*SINK.lock().unwrap() {
        Sink::Stdout => {
            let mut out = std::io::stdout();
            let _ = out.write_all(bytes).and_then(|()| out.flush());
        }
        Sink::Remote(Some(client)) => {
            let _ = client.send(ToClient::Raw(bytes.to_vec()));
        }
        Sink::Remote(None) => {}
    }
}

/// The terminal Textrix shows in now.
pub fn info() -> TermInfo {
    INFO.lock().unwrap().clone()
}

/// Whether a terminal is showing Textrix (always, unless it runs in the
/// background with none attached).
pub fn attached() -> bool {
    !matches!(&*SINK.lock().unwrap(), Sink::Remote(None))
}

/// Background mode: send output to this terminal (`None`: detached).
pub fn set_remote(client: Option<UnboundedSender<ToClient>>, info: Option<TermInfo>) {
    *SINK.lock().unwrap() = Sink::Remote(client);
    if let Some(info) = info {
        *INFO.lock().unwrap() = info;
    }
}

/// Background mode: the attached terminal, to say goodbye to.
pub fn remote_client() -> Option<UnboundedSender<ToClient>> {
    match &*SINK.lock().unwrap() {
        Sink::Remote(client) => client.clone(),
        Sink::Stdout => None,
    }
}
