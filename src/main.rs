//! Textrix: an irssi-style terminal client for Matrix.

mod app;
mod compose;
mod config;
mod convert;
mod input;
mod matrix;
mod model;
mod notify;
mod servers;
mod ui;

use std::process::ExitCode;

use anyhow::{Result, bail};
use crossterm::event::{DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange};
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
        .with_env_filter(
            EnvFilter::try_from_env("TEXTRIX_LOG").unwrap_or_else(|_| EnvFilter::new("warn,textrix=info")),
        )
        .with_writer(log)
        .with_ansi(false)
        .init();

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let previews = cfg.image_previews;
    let mut app = App::new(cfg, paths, env, servers, tx);

    let mut terminal = ratatui::init();
    // Ask the terminal what graphics it supports before anything reads stdin.
    if previews {
        app.picker = Some(Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks()));
    }
    crossterm::execute!(std::io::stdout(), EnableBracketedPaste, EnableFocusChange)?;
    let result = app::run(&mut terminal, app, rx).await;
    let _ = crossterm::execute!(std::io::stdout(), DisableBracketedPaste, DisableFocusChange);
    ratatui::restore();
    result
}
