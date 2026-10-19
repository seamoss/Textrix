//! Background mode, like irssi in screen: `textrix --daemon` runs Textrix
//! with no terminal of its own, and `textrix --attach` (or plain `textrix`
//! while one runs) shows it in this one; `/detach` leaves it running.
//!
//! The background Textrix draws into a [`RemoteBackend`], which sends the
//! cells that changed, and the title, bell, notification and clipboard
//! sequences ([`crate::term`]), over a Unix socket only you can open
//! (`~/.textrix/daemon.sock`) to the attached terminal. The terminal sends
//! back its keys, mouse, pastes and size. One terminal at a time: a new one
//! takes over.

use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste, EnableFocusChange, Event,
    EventStream, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use futures_util::StreamExt;
use ratatui::{
    backend::{Backend, ClearType, WindowSize},
    buffer::Cell,
    layout::{Position, Size},
};
use ratatui_image::picker::{Picker, ProtocolType};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::mpsc::{self, UnboundedSender},
};

use crate::{
    matrix::{AppEvent, Tx},
    term::{self, TermInfo},
};

#[derive(Serialize, Deserialize)]
pub enum ToDaemon {
    Hello { version: String, size: (u16, u16), picker: Option<PickerInfo>, term: TermInfo },
    Event(Event),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ToClient {
    Draw(Vec<(u16, u16, Cell)>),
    Clear(u8),
    HideCursor,
    ShowCursor,
    MoveCursor(u16, u16),
    Flush,
    /// Control sequences for the terminal itself (title, bell, …).
    Raw(Vec<u8>),
    /// The end of this attachment, and why.
    Bye(String),
}

/// What the attached terminal can draw images with.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PickerInfo {
    font: (u16, u16),
    protocol: String,
}

impl PickerInfo {
    pub fn of(picker: &Picker) -> Self {
        let font = picker.font_size();
        let protocol = match picker.protocol_type() {
            ProtocolType::Halfblocks => "halfblocks",
            ProtocolType::Sixel => "sixel",
            ProtocolType::Kitty => "kitty",
            ProtocolType::Iterm2 => "iterm2",
        };
        Self { font: (font.width, font.height), protocol: protocol.to_owned() }
    }

    pub fn picker(&self) -> Picker {
        #[allow(deprecated)] // The only way to build one for another terminal.
        let mut picker = Picker::from_fontsize(ratatui_image::FontSize::new(self.font.0, self.font.1));
        picker.set_protocol_type(match self.protocol.as_str() {
            "sixel" => ProtocolType::Sixel,
            "kitty" => ProtocolType::Kitty,
            "iterm2" => ProtocolType::Iterm2,
            _ => ProtocolType::Halfblocks,
        });
        picker
    }
}

const MAX_MESSAGE: usize = 64 * 1024 * 1024;

async fn send<T: Serialize>(out: &mut (impl AsyncWrite + Unpin), message: &T) -> Result<()> {
    let bytes = rmp_serde::to_vec(message)?;
    out.write_u32(u32::try_from(bytes.len())?).await?;
    out.write_all(&bytes).await?;
    Ok(())
}

/// The next message, or `None` when the other side has gone.
async fn recv<T: DeserializeOwned>(input: &mut (impl AsyncRead + Unpin)) -> Result<Option<T>> {
    let len = match input.read_u32().await {
        Ok(len) => len as usize,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    ensure!(len <= MAX_MESSAGE, "a message too large to be Textrix's");
    let mut buf = vec![0; len];
    input.read_exact(&mut buf).await?;
    Ok(Some(rmp_serde::from_slice(&buf)?))
}

pub fn socket_path(base: &Path) -> PathBuf {
    base.join("daemon.sock")
}

/// Whether a background Textrix answers on `socket`.
pub fn alive(socket: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}

// ---- the background side ---------------------------------------------------

/// The attached terminal's size (80×24 while none is).
#[derive(Clone)]
pub struct Screen(Arc<Mutex<(u16, u16)>>);

impl Screen {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new((80, 24))))
    }
}

fn to_client(message: ToClient) {
    if let Some(client) = term::remote_client() {
        let _ = client.send(message);
    }
}

/// Draws by sending to the attached terminal (and nowhere while none is).
pub struct RemoteBackend {
    screen: Screen,
    cursor: Position,
}

impl RemoteBackend {
    pub fn new(screen: Screen) -> Self {
        Self { screen, cursor: Position::ORIGIN }
    }
}

impl Backend for RemoteBackend {
    type Error = std::io::Error;

    fn draw<'a, I>(&mut self, content: I) -> std::io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let cells: Vec<(u16, u16, Cell)> = content.map(|(x, y, cell)| (x, y, cell.clone())).collect();
        if !cells.is_empty() {
            to_client(ToClient::Draw(cells));
        }
        Ok(())
    }

    fn hide_cursor(&mut self) -> std::io::Result<()> {
        to_client(ToClient::HideCursor);
        Ok(())
    }

    fn show_cursor(&mut self) -> std::io::Result<()> {
        to_client(ToClient::ShowCursor);
        Ok(())
    }

    fn get_cursor_position(&mut self) -> std::io::Result<Position> {
        Ok(self.cursor)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> std::io::Result<()> {
        self.cursor = position.into();
        to_client(ToClient::MoveCursor(self.cursor.x, self.cursor.y));
        Ok(())
    }

    fn clear(&mut self) -> std::io::Result<()> {
        self.clear_region(ClearType::All)
    }

    fn clear_region(&mut self, clear_type: ClearType) -> std::io::Result<()> {
        let code = match clear_type {
            ClearType::All => 0,
            ClearType::AfterCursor => 1,
            ClearType::BeforeCursor => 2,
            ClearType::CurrentLine => 3,
            ClearType::UntilNewLine => 4,
        };
        to_client(ToClient::Clear(code));
        Ok(())
    }

    fn size(&self) -> std::io::Result<Size> {
        let (width, height) = *self.screen.0.lock().unwrap();
        Ok(Size::new(width, height))
    }

    fn window_size(&mut self) -> std::io::Result<WindowSize> {
        Ok(WindowSize { columns_rows: self.size()?, pixels: Size::new(0, 0) })
    }

    fn flush(&mut self) -> std::io::Result<()> {
        to_client(ToClient::Flush);
        Ok(())
    }
}

/// Which attachment is current, so a terminal that's been replaced (or let
/// go) can't detach the one after it as it leaves.
static ATTACHMENT: AtomicU64 = AtomicU64::new(0);

/// Accept terminals on `listener`: each one that says hello is attached
/// (the one before is told why it was let go). Its input goes to `input`,
/// and the app hears when one attaches or leaves.
pub fn serve(listener: UnixListener, screen: Screen, input: UnboundedSender<Event>, app: Tx) {
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(attachment(stream, screen.clone(), input.clone(), app.clone()));
        }
    });
}

async fn attachment(stream: UnixStream, screen: Screen, input: UnboundedSender<Event>, app: Tx) {
    let (mut from, mut to) = stream.into_split();
    let hello = match tokio::time::timeout(Duration::from_secs(5), recv::<ToDaemon>(&mut from)).await {
        Ok(Ok(Some(ToDaemon::Hello { version, size, picker, term }))) => {
            if version != env!("CARGO_PKG_VERSION") {
                let why = format!(
                    "the Textrix running in the background is version {}, and this one is {version}: textrix \
                     --stop stops it, and textrix --daemon starts this one",
                    env!("CARGO_PKG_VERSION")
                );
                let _ = send(&mut to, &ToClient::Bye(why)).await;
                return;
            }
            (size, picker, term)
        }
        // Someone checking it's alive, or not a Textrix.
        _ => return,
    };
    let (size, picker, info) = hello;
    let (tx, mut rx) = mpsc::unbounded_channel::<ToClient>();
    tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            let bye = matches!(message, ToClient::Bye(_));
            if send(&mut to, &message).await.is_err() || bye {
                break;
            }
        }
    });

    let me = ATTACHMENT.fetch_add(1, Ordering::SeqCst) + 1;
    if let Some(before) = term::remote_client() {
        let _ = before.send(ToClient::Bye("attached in another terminal".into()));
    }
    *screen.0.lock().unwrap() = size;
    term::set_remote(Some(tx), Some(info));
    let _ = app.send(AppEvent::Attached { picker: picker.map(|p| p.picker()) });

    while let Ok(Some(ToDaemon::Event(event))) = recv::<ToDaemon>(&mut from).await {
        if let Event::Resize(width, height) = event {
            *screen.0.lock().unwrap() = (width, height);
        }
        if input.send(event).is_err() {
            break;
        }
    }
    if ATTACHMENT.load(Ordering::SeqCst) == me {
        term::set_remote(None, None);
        let _ = app.send(AppEvent::Detached);
    }
}

/// Let the attached terminal go, saying why (`/detach`, or quitting).
pub fn detach(why: &str) -> bool {
    let Some(client) = term::remote_client() else { return false };
    ATTACHMENT.fetch_add(1, Ordering::SeqCst);
    let _ = client.send(ToClient::Bye(why.to_owned()));
    term::set_remote(None, None);
    true
}

/// Start Textrix in the background (this program again, with
/// `--daemon-run`), and wait until it answers.
pub fn start(base: &Path, args: Vec<std::ffi::OsString>) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let socket = socket_path(base);
    if alive(&socket) {
        bail!("Textrix is already running in the background; textrix --attach shows it");
    }
    let errors = base.join("daemon.err");
    let _ = std::fs::remove_file(&errors);
    let err_file = std::fs::File::create(&errors).with_context(|| format!("creating {}", errors.display()))?;
    let mut child = std::process::Command::new(std::env::current_exe()?)
        .arg("--daemon-run")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(err_file)
        // Its own process group: closing this terminal doesn't stop it.
        .process_group(0)
        .spawn()
        .context("starting Textrix in the background")?;
    for _ in 0..150 {
        if alive(&socket) {
            println!("Textrix is running in the background; textrix --attach shows it, and /detach leaves it running.");
            return Ok(());
        }
        if child.try_wait()?.is_some() {
            let why = std::fs::read_to_string(&errors).unwrap_or_default();
            bail!("the background Textrix stopped: {}", why.trim().trim_start_matches("textrix: "));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("the background Textrix didn't answer; see {}", base.join("textrix.log").display())
}

// ---- the terminal side -------------------------------------------------------

/// Show the background Textrix in this terminal until it lets go (or quits).
pub async fn attach(socket: &Path, previews: bool) -> Result<()> {
    let stream = UnixStream::connect(socket)
        .await
        .context("Textrix isn't running in the background (textrix --daemon starts it)")?;
    let (mut from, mut to) = stream.into_split();

    let mut terminal = ratatui::try_init().map_err(|e| anyhow::anyhow!("Textrix needs a terminal ({e})"))?;
    let picker = previews.then(|| Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks()));
    let enhanced_keys = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    let mut out = std::io::stdout();
    if enhanced_keys {
        crossterm::execute!(out, PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES))?;
    }
    crossterm::execute!(out, EnableBracketedPaste, EnableFocusChange)?;
    crate::notify::push_title();
    let restore = |enhanced_keys: bool| {
        let mut out = std::io::stdout();
        let _ = crossterm::execute!(out, DisableMouseCapture, DisableBracketedPaste, DisableFocusChange);
        crate::notify::pop_title();
        if enhanced_keys {
            let _ = crossterm::execute!(out, PopKeyboardEnhancementFlags);
        }
        ratatui::restore();
    };

    let result = async {
        let size = terminal.size()?;
        let hello = ToDaemon::Hello {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            size: (size.width, size.height),
            picker: picker.as_ref().map(PickerInfo::of),
            term: TermInfo::from_env(),
        };
        send(&mut to, &hello).await?;
        // Read messages in a task of their own: a half-read one must never be
        // dropped by select! below.
        let (msg_tx, mut messages) = mpsc::unbounded_channel::<Result<Option<ToClient>>>();
        tokio::spawn(async move {
            loop {
                let message = recv::<ToClient>(&mut from).await;
                let last = !matches!(message, Ok(Some(_)));
                if msg_tx.send(message).is_err() || last {
                    break;
                }
            }
        });
        let mut events = EventStream::new();
        let backend = terminal.backend_mut();
        loop {
            tokio::select! {
                event = events.next() => match event {
                    Some(Ok(event)) => send(&mut to, &ToDaemon::Event(event)).await?,
                    Some(Err(e)) => return Err(e.into()),
                    None => return Ok("the terminal closed".to_owned()),
                },
                message = messages.recv() => match message {
                    Some(Ok(Some(ToClient::Draw(cells)))) => backend.draw(cells.iter().map(|(x, y, c)| (*x, *y, c)))?,
                    Some(Ok(Some(ToClient::Clear(code)))) => backend.clear_region(match code {
                        1 => ClearType::AfterCursor,
                        2 => ClearType::BeforeCursor,
                        3 => ClearType::CurrentLine,
                        4 => ClearType::UntilNewLine,
                        _ => ClearType::All,
                    })?,
                    Some(Ok(Some(ToClient::HideCursor))) => backend.hide_cursor()?,
                    Some(Ok(Some(ToClient::ShowCursor))) => backend.show_cursor()?,
                    Some(Ok(Some(ToClient::MoveCursor(x, y)))) => backend.set_cursor_position(Position::new(x, y))?,
                    Some(Ok(Some(ToClient::Flush))) => Backend::flush(backend)?,
                    Some(Ok(Some(ToClient::Raw(bytes)))) => {
                        let mut out = std::io::stdout();
                        out.write_all(&bytes)?;
                        out.flush()?;
                    }
                    Some(Ok(Some(ToClient::Bye(why)))) => return Ok(why),
                    Some(Err(e)) => return Err(e),
                    Some(Ok(None)) | None => return Ok("the background Textrix went away".to_owned()),
                },
            }
        }
    }
    .await;
    restore(enhanced_keys);
    println!("{}", result?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Modifier, Style};

    use super::*;

    #[tokio::test]
    async fn messages_cross_the_socket_intact() {
        let (mut a, mut b) = tokio::io::duplex(1 << 16);
        let mut cell = Cell::new("界");
        cell.set_style(Style::new().fg(Color::Indexed(46)).bg(Color::Rgb(1, 2, 3)).add_modifier(Modifier::BOLD));
        send(&mut a, &ToClient::Draw(vec![(3, 4, cell.clone())])).await.unwrap();
        send(&mut a, &ToClient::Raw(b"\x1b]2;Night Owls\x07".to_vec())).await.unwrap();
        let key = Event::Key(crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Char('q')));
        send(&mut a, &ToDaemon::Event(key.clone())).await.unwrap();
        drop(a);
        match recv::<ToClient>(&mut b).await.unwrap() {
            Some(ToClient::Draw(cells)) => assert_eq!(cells, [(3, 4, cell)]),
            other => panic!("{other:?}"),
        }
        assert!(matches!(recv::<ToClient>(&mut b).await.unwrap(), Some(ToClient::Raw(raw)) if raw.ends_with(b"\x07")));
        assert!(matches!(recv::<ToDaemon>(&mut b).await.unwrap(), Some(ToDaemon::Event(e)) if e == key));
        assert!(recv::<ToClient>(&mut b).await.unwrap().is_none(), "the end is None, not an error");
    }

    #[test]
    fn pickers_survive_the_trip() {
        let mut picker = PickerInfo { font: (9, 18), protocol: "kitty".into() }.picker();
        assert_eq!(picker.protocol_type(), ProtocolType::Kitty);
        assert_eq!((picker.font_size().width, picker.font_size().height), (9, 18));
        picker.set_protocol_type(ProtocolType::Sixel);
        assert_eq!(PickerInfo::of(&picker).protocol, "sixel");
    }
}
