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
}

impl TermInfo {
    pub fn from_env() -> Self {
        Self {
            term: std::env::var("TERM").unwrap_or_default(),
            term_program: std::env::var("TERM_PROGRAM").unwrap_or_default(),
            kitty: std::env::var_os("KITTY_WINDOW_ID").is_some(),
            tmux: std::env::var_os("TMUX").is_some(),
            truecolor: std::env::var("COLORTERM").is_ok_and(|c| c == "truecolor" || c == "24bit"),
        }
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
