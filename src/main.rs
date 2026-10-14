//! Textrix: an irssi-style terminal client for Matrix.

mod app;
mod chatlog;
mod compose;
mod config;
mod convert;
mod emoji;
mod input;
mod links;
mod matrix;
mod model;
mod notify;
mod richtext;
mod servers;
mod ui;

use std::process::ExitCode;

use anyhow::{Result, bail};
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui_image::picker::Picker;
use tracing_subscriber::EnvFilter;

use crate::{
    app::App,
    config::{Config, Paths, create_private_dir},
    servers::EnvFile,
};

const USAGE: &str = "\
usage: textrix [--help] [--version]

Connects to every server defined in the dotfile:
  release builds: ~/.textrix/.env   debug builds: ./.textrix in the repo
  (override with TEXTRIX_ENV=<path>; state lives in TEXTRIX_HOME, default ~/.textrix)

See .env.example for the format.";

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("textrix: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    if let Some(arg) = std::env::args().nth(1) {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            "-V" | "--version" => {
                println!("textrix {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            other => bail!("unknown argument {other}\n\n{USAGE}"),
        }
    }

    let paths = Paths::discover()?;
    create_private_dir(&paths.base)?;
    let env = EnvFile::new(paths.env_file.clone());
    let servers = env.servers().map_err(|e| anyhow::anyhow!("{e:#}\n(see .env.example for the format)"))?;
    let cfg = Config::load(&paths.config_file())?;

    let log = tracing_appender::rolling::never(&paths.base, "textrix.log");
    let (log, _log_guard) = tracing_appender::non_blocking(log);
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_env("TEXTRIX_LOG").unwrap_or_else(|_| EnvFilter::new("warn,textrix=info")))
        .with_writer(log)
        .with_ansi(false)
        .init();

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let previews = cfg.image_previews;
    let mut app = App::new(cfg, paths, env, servers, tx);

    let mut terminal = ratatui::init();
    // Ask the terminal about graphics and key reporting before anything
    // else reads stdin.
    if previews {
        app.picker = Some(Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks()));
    }
    // Lets terminals that support it (kitty, Ghostty, WezTerm, …) report
    // Shift+Enter distinctly; elsewhere Alt+Enter inserts a newline.
    let enhanced_keys = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    if enhanced_keys {
        crossterm::execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }
    crossterm::execute!(std::io::stdout(), EnableBracketedPaste, EnableFocusChange)?;
    let result = app::run(&mut terminal, app, rx).await;
    if enhanced_keys {
        let _ = crossterm::execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = crossterm::execute!(std::io::stdout(), DisableBracketedPaste, DisableFocusChange);
    ratatui::restore();
    result
}
