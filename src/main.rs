//! Textrix: an irssi-style terminal client for Matrix.

mod app;
mod audio;
mod chatlog;
mod compose;
mod config;
mod convert;
mod eggs;
mod emoji;
mod highlight;
mod index;
mod input;
mod links;
mod matrix;
mod model;
mod notify;
mod remote;
mod richtext;
mod script;
mod servers;
mod switcher;
mod term;
mod theme;
mod ui;
mod update;
mod vault;
mod viewer;

use std::process::ExitCode;

use std::path::PathBuf;

use anyhow::Result;
use clap::{CommandFactory, Parser};
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
    theme::{Theme, ThemeName},
};

/// `--check`: what Textrix would use, without connecting anywhere or
/// reading any secrets.
fn check(paths: &Paths) -> Result<()> {
    println!("textrix {}", env!("CARGO_PKG_VERSION"));
    let mut ok = true;
    let env = EnvFile::new(paths.env_file.clone());
    match env.servers() {
        Ok(servers) => {
            println!("dotfile   {}: {} server(s)", paths.env_file.display(), servers.defs.len());
            for def in &servers.defs {
                let home = def.homeserver_or_server_name().unwrap_or("(found from the user ID)");
                let dir = paths.account_dir(&def.name);
                let store = if dir.join("store-enc").exists() {
                    "encrypted store"
                } else if dir.join("store").exists() {
                    "unencrypted store (encrypted at next start)"
                } else {
                    "not signed in yet"
                };
                println!("  {:<12} {} on {home}; {store}", def.name, def.user);
            }
            for warning in &servers.warnings {
                println!("  warning: {warning}");
            }
        }
        Err(e) => {
            ok = false;
            println!("dotfile   {}: {e:#}", paths.env_file.display());
        }
    }
    let config = paths.config_file();
    match Config::load(&config) {
        Ok(cfg) if config.exists() => println!("config    {}: ok (theme {})", config.display(), cfg.theme.name()),
        Ok(_) => println!("config    {}: none, using defaults", config.display()),
        Err(e) => {
            ok = false;
            println!("config    {}: {e:#}", config.display());
        }
    }
    let keychain = if vault::available() { "available" } else { "not found (stores stay unencrypted)" };
    println!("keychain  {keychain}");
    if ok { Ok(()) } else { anyhow::bail!("fix the problems above") }
}

/// An irssi-style terminal client for Matrix.
#[derive(clap::Parser)]
#[command(name = "textrix", version, about, long_about = LONG_ABOUT)]
struct Cli {
    /// The dotfile listing your servers [default: ~/.textrix/.env; debug
    /// builds use .textrix in the repo; TEXTRIX_ENV also sets it]
    #[arg(long, value_name = "FILE")]
    env: Option<PathBuf>,
    /// Where Textrix keeps its state and config.toml [default: ~/.textrix;
    /// TEXTRIX_HOME also sets it]
    #[arg(long, value_name = "DIR")]
    home: Option<PathBuf>,
    /// Start with this colour theme, for this run only
    #[arg(long, value_name = "NAME", value_parser = theme_names())]
    theme: Option<String>,
    /// Check the dotfile, config.toml, keychain and stores, then exit
    #[arg(long)]
    check: bool,
    /// Print shell completions (bash, zsh, fish, elvish, powershell)
    #[arg(long, value_name = "SHELL")]
    completions: Option<clap_complete::Shell>,
    /// Print the man page
    #[arg(long)]
    man: bool,
    /// Run in the background, with no terminal (like irssi in screen):
    /// textrix --attach shows it, /detach leaves it running
    #[arg(long, conflicts_with_all = ["attach", "check"])]
    daemon: bool,
    /// Show the Textrix running in the background in this terminal (plain
    /// textrix does too, while one runs)
    #[arg(long)]
    attach: bool,
    /// Stop the Textrix running in the background
    #[arg(long, conflicts_with_all = ["daemon", "attach", "check"])]
    stop: bool,
    /// The background process itself (what --daemon starts)
    #[arg(long, hide = true)]
    daemon_run: bool,
}

/// One Textrix per home: two would share one store and can scramble its
/// encryption keys. Held until the process ends.
fn lock_instance(base: &std::path::Path) -> Result<std::fs::File> {
    let path = base.join("textrix.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => anyhow::bail!(
            "Textrix is already running with {} in another terminal; quit it first (two at once would share one \
             store and can scramble its encryption keys)",
            base.display()
        ),
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

/// The background Textrix: no terminal of its own, drawing for whichever
/// one attaches.
async fn run_daemon(
    app: App,
    rx: tokio::sync::mpsc::UnboundedReceiver<matrix::AppEvent>,
    tx: matrix::Tx,
    base: &std::path::Path,
) -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let socket = remote::socket_path(base);
    // Holding the lock means no other Textrix answers there: a leftover.
    let _ = std::fs::remove_file(&socket);
    let listener = tokio::net::UnixListener::bind(&socket)
        .map_err(|e| anyhow::anyhow!("listening on {}: {e}", socket.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    }
    // Closing the terminal it was started from doesn't stop it; being asked
    // to (textrix --stop) quits as /quit does, saving what it should.
    let mut hangups = signal(SignalKind::hangup())?;
    tokio::spawn(async move { while hangups.recv().await.is_some() {} });
    let mut stops = signal(SignalKind::terminate())?;
    let quit = tx.clone();
    tokio::spawn(async move {
        if stops.recv().await.is_some() {
            let _ = quit.send(matrix::AppEvent::Quit);
        }
    });
    config::write_private(&base.join("daemon.pid"), std::process::id().to_string().as_bytes())?;

    term::set_remote(None, None);
    let screen = remote::Screen::new();
    let mut terminal = ratatui::Terminal::new(remote::RemoteBackend::new(screen.clone()))?;
    let (input, mut input_rx) = tokio::sync::mpsc::unbounded_channel();
    remote::serve(listener, screen, input, tx);
    let keys = futures_util::stream::poll_fn(move |cx| input_rx.poll_recv(cx).map(|event| event.map(Ok)));
    let result = app::run(&mut terminal, app, rx, keys).await;
    remote::detach("Textrix has quit");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(base.join("daemon.pid"));
    result
}

/// `--stop`: ask the background Textrix to quit, and wait until it has.
fn stop_daemon(base: &std::path::Path) -> Result<()> {
    let socket = remote::socket_path(base);
    if !remote::alive(&socket) {
        anyhow::bail!("Textrix isn't running in the background");
    }
    let pid = std::fs::read_to_string(base.join("daemon.pid"))?.trim().parse::<u32>()?;
    let status = std::process::Command::new("kill").arg("-TERM").arg(pid.to_string()).status()?;
    anyhow::ensure!(status.success(), "couldn't stop process {pid}");
    for _ in 0..100 {
        if !remote::alive(&socket) {
            println!("stopped the Textrix running in the background");
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    anyhow::bail!("it's still running (process {pid})")
}

const LONG_ABOUT: &str = "\
An irssi-style terminal client for Matrix: numbered windows, a status bar, \
slash commands, and end-to-end encryption.

Textrix connects to every server defined in its dotfile (~/.textrix/.env), \
a private file of TEXTRIX_<NAME>_HOMESERVER, _USER, _PASSWORD and \
_RECOVERY_KEY lines; see the env.example installed with it. Secrets are read \
only for the call that needs them, and environment variables of the same \
name override the file. Preferences live in ~/.textrix/config.toml; inside \
Textrix, /set and /save change them, and F1 lists every key and command.";

fn theme_names() -> clap::builder::PossibleValuesParser {
    clap::builder::PossibleValuesParser::new(ThemeName::ALL.map(|(name, _)| name))
}

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
    let cli = Cli::parse();
    if let Some(shell) = cli.completions {
        clap_complete::generate(shell, &mut Cli::command(), "textrix", &mut std::io::stdout());
        return Ok(());
    }
    if cli.man {
        clap_mangen::Man::new(Cli::command()).render(&mut std::io::stdout())?;
        return Ok(());
    }

    let paths = Paths::discover(cli.home, cli.env)?;
    if cli.check {
        return check(&paths);
    }
    create_private_dir(&paths.base)?;
    let socket = remote::socket_path(&paths.base);
    if cli.stop {
        return stop_daemon(&paths.base);
    }
    if cli.daemon {
        if remote::alive(&socket) {
            anyhow::bail!("Textrix is already running in the background; textrix --attach shows it");
        }
        // Refuse here, not in the background, if another Textrix runs.
        drop(lock_instance(&paths.base)?);
        let mut args: Vec<std::ffi::OsString> =
            vec!["--home".into(), paths.base.clone().into(), "--env".into(), paths.env_file.clone().into()];
        if let Some(theme) = &cli.theme {
            args.extend(["--theme".into(), theme.into()]);
        }
        return remote::start(&paths.base, args);
    }
    if cli.attach || (!cli.daemon_run && remote::alive(&socket)) {
        let previews = Config::load(&paths.config_file()).map_or(true, |cfg| cfg.image_previews);
        return remote::attach(&socket, previews).await;
    }
    let _lock = lock_instance(&paths.base)?;
    let env = EnvFile::new(paths.env_file.clone());
    let servers = env.servers().map_err(|e| anyhow::anyhow!("{e:#}\n(see .env.example for the format)"))?;
    let mut cfg = Config::load(&paths.config_file())?;
    if let Some(theme) = cli.theme.as_deref().and_then(ThemeName::parse) {
        cfg.palette = Theme::build(theme, &cfg.colors)?;
        cfg.theme = theme;
    }

    let log = tracing_appender::rolling::never(&paths.base, "textrix.log");
    let (log, _log_guard) = tracing_appender::non_blocking(log);
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_env("TEXTRIX_LOG").unwrap_or_else(|_| EnvFilter::new("warn,textrix=info")))
        .with_writer(log)
        .with_ansi(false)
        .init();

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let previews = cfg.image_previews;
    let mouse = cfg.mouse;
    highlight::warm_up();
    let base = paths.base.clone();
    let mut app = App::new(cfg, paths, env, servers, tx.clone());
    if cli.daemon_run {
        return run_daemon(app, rx, tx, &base).await;
    }

    let mut terminal = ratatui::try_init().map_err(|e| anyhow::anyhow!("Textrix needs a terminal ({e})"))?;
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
    notify::push_title();
    if mouse {
        app::set_mouse_capture(true);
    }
    // A crash shouldn't leave the shell receiving mouse reports. A panic in
    // a background task only fails that task (Textrix carries on), so it
    // goes to the log rather than over the screen.
    let restore = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().name() != Some("main") {
            tracing::error!("background task: {info}");
            return;
        }
        app::set_mouse_capture(false);
        notify::pop_title();
        restore(info);
    }));
    let result = app::run(&mut terminal, app, rx, crossterm::event::EventStream::new()).await;
    app::set_mouse_capture(false);
    notify::pop_title();
    if enhanced_keys {
        let _ = crossterm::execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = crossterm::execute!(std::io::stdout(), DisableBracketedPaste, DisableFocusChange);
    ratatui::restore();
    result
}
