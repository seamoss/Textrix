//! The screenshots in the README and on the website: the real interface,
//! drawn off-screen from made-up rooms and people, saved as SVG.
//!
//!     scripts/screenshots.sh
//!
//! Nothing connects anywhere; every name here is invented.

use std::{collections::HashMap, fmt::Write as _, path::Path};

use chrono::{DateTime, Local, TimeDelta};
use matrix_sdk::ruma::{OwnedEventId, OwnedMxcUri, OwnedRoomId, OwnedUserId, events::room::MediaSource};
use ratatui::{
    buffer::Buffer,
    layout::Size,
    style::{Color, Modifier},
};
use unicode_width::UnicodeWidthStr;

use super::*;
use crate::{
    matrix::AppEvent,
    model::{Fold, Item, Line, LineKind, Media, MediaKind, Member, Presence, PresenceKind, RoomMeta, SpaceInfo},
    picture::{Painter, TextMode},
    theme::{Theme, ThemeName},
};

const ACCOUNT: &str = "home";
const SERVER: &str = "example.org";

fn user(name: &str) -> OwnedUserId {
    format!("@{name}:{SERVER}").try_into().unwrap()
}

fn room_id(name: &str) -> OwnedRoomId {
    format!("!{}:{SERVER}", name.replace(' ', "").to_lowercase()).try_into().unwrap()
}

/// Made-up rooms, people and messages.
struct Scene {
    app: App,
    next: u32,
    /// The clock on the status bar, which the messages lead up to.
    now: DateTime<Local>,
}

impl Scene {
    fn new(theme: ThemeName) -> Self {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let dir = std::env::temp_dir().join(format!("textrix-shots-{}", std::process::id()));
        let paths = Paths { base: dir.clone(), env_file: dir.join("none") };
        // Full colour, whatever terminal this runs in.
        let cfg = Config { theme: theme.into(), palette: Theme::builtin(theme), ..Config::default() };
        let mut app = App::new(cfg, paths.clone(), EnvFile::new(paths.env_file), Servers::default(), tx);
        let picker = ratatui_image::picker::Picker::halfblocks();
        app.picker = Some(Painter { picker, text: TextMode::Sextants, truecolor: true });
        Self { app, next: 0, now: Local::now() }
    }

    fn rooms(&mut self, rooms: &[(&str, &str, Option<&str>)]) {
        let rooms = rooms
            .iter()
            .map(|&(name, topic, dm)| RoomMeta {
                room_id: room_id(name),
                name: name.into(),
                topic: (!topic.is_empty()).then(|| topic.into()),
                encrypted: true,
                is_dm: dm.is_some(),
                dm_user: dm.map(user),
                is_space: false,
                avatar: None,
                fully_read: None,
                unread: 0,
                highlights: 0,
                pinned: Vec::new(),
            })
            .collect();
        self.app.on_event(AppEvent::Rooms { account: ACCOUNT.into(), rooms });
    }

    fn spaces(&mut self, spaces: &[(&str, &[&str])]) {
        let spaces = spaces
            .iter()
            .map(|(name, children)| SpaceInfo {
                room_id: room_id(name),
                name: (*name).into(),
                children: children.iter().map(|c| room_id(c)).collect(),
            })
            .collect();
        self.app.on_event(AppEvent::Spaces { account: ACCOUNT.into(), spaces });
    }

    fn window(&self, name: &str) -> usize {
        self.app.windows.iter().position(|w| w.name == name).unwrap_or_else(|| panic!("no window {name}"))
    }

    /// A message from `mins` minutes ago.
    fn line(&mut self, mins: i64, nick: &str, body: &str) -> Line {
        self.next += 1;
        let mut line = Line::new(LineKind::Message, body);
        line.ts = self.now - TimeDelta::minutes(mins);
        line.event_id = Some(OwnedEventId::try_from(format!("$e{}", self.next)).unwrap());
        line.nick = nick.into();
        line.sender = Some(user(nick));
        line.own = nick == "you";
        line
    }

    fn post(&mut self, room: &str, lines: Vec<Line>) {
        let items = lines.into_iter().map(Item::Line).collect();
        let (account, room_id) = (ACCOUNT.into(), room_id(room));
        self.app.on_event(AppEvent::Timeline { account, room_id, items, live: true, limited: false });
    }

    fn members(&mut self, room: &str, people: &[(&str, char)]) {
        let members =
            people.iter().map(|&(name, prefix)| Member { name: name.into(), user_id: user(name), prefix }).collect();
        self.app.on_event(AppEvent::Members { account: ACCOUNT.into(), room_id: room_id(room), members, show: false });
    }

    fn presence(&mut self, people: &[(&str, PresenceKind, Option<&str>)]) {
        let updates = people
            .iter()
            .map(|&(name, kind, status)| {
                (user(name), Presence { kind, status: status.map(Into::into), last_active: None })
            })
            .collect();
        self.app.on_event(AppEvent::Presence { account: ACCOUNT.into(), updates });
    }

    fn picture(&mut self, line: &Line, cols: u16, rows: u16) {
        let painter = self.app.picker.clone().unwrap();
        let picture = painter.fit(sunset(), Size::new(cols, rows)).unwrap();
        let event_id = line.event_id.clone().unwrap();
        self.app.on_event(AppEvent::ImagePreview { event_id, protocol: Some(picture) });
    }

    fn draw(&mut self, width: u16, height: u16) -> Buffer {
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        // Twice: the first draw measures, as it does at startup.
        for _ in 0..2 {
            terminal.draw(|f| ui::draw(f, &mut self.app)).unwrap();
        }
        terminal.backend().buffer().clone()
    }

    fn title(&self) -> String {
        self.app.terminal_title().unwrap_or_else(|| "Textrix".into())
    }
}

/// Night Owls and the rest: a conversation, the window list in its
/// sections, the member list.
fn night_owls(theme: ThemeName, picture: bool) -> Scene {
    let mut s = Scene::new(theme);
    s.rooms(&[
        ("Night Owls", "Late-night hacking, snacks optional", None),
        ("Book Club", "This month: Neuromancer", None),
        ("Team Tests", "Scratch room for testing things", None),
        ("Synth Nerds", "", None),
        ("Garden Club", "", None),
        ("Rustaceans", "", None),
        ("sam", "", Some("sam")),
        ("jo_bloggs", "", Some("jo_bloggs")),
        ("alex", "", Some("alex")),
    ]);
    s.spaces(&[
        ("Hobbies", &["Night Owls", "Book Club", "Synth Nerds", "Garden Club"]),
        ("Work", &["Team Tests", "Rustaceans"]),
    ]);
    let owls = s.window("Night Owls");
    s.app.switch_to(owls);

    let mut lines = Vec::new();
    for name in ["alice", "bob", "carol", "dee", "eve"] {
        let mut l = s.line(12, name, "joined");
        l.kind = LineKind::Membership;
        l.fold = Some(Fold::Joined(name.into()));
        lines.push(l);
    }
    lines.push(s.line(10, "sam", "anyone up for a late one?"));
    lines.push(s.line(10, "jo_bloggs", "always. what are we building tonight"));
    lines.push(s.line(9, "you", "a matrix client. in the terminal. like irssi"));
    let mut reply = s.line(9, "sam", "of course you are");
    reply.reply_to = lines.last().and_then(|l| l.event_id.clone());
    lines.push(reply);
    let mut code = s.line(7, "you", "here's the main loop");
    code.rich = crate::richtext::render(
        "<p>here's the main loop</p><pre><code class=\"language-rust\">loop {\n    let event = events.next().await?;\n    \
         app.on_event(event); // redraws when needed\n}</code></pre>",
    );
    lines.push(code);
    lines.push(s.line(6, "jo_bloggs", "[voice] 0:12 ▁▂▄▆█▆▅▃▂▄▆▇▅▃▂▁▂▃▅▆▄▂▁"));
    let mut nice = s.line(5, "sam", "ok that's actually nice");
    nice.reactions = vec![("🔥".into(), vec!["jo_bloggs".into(), "you".into(), "alice".into()])];
    nice.thread_count = Some(2);
    lines.push(nice);
    lines.push(s.line(1, "alice", "does it do pictures?"));
    if picture {
        let mut image = s.line(0, "you", "[image] sunset.png");
        let mxc: OwnedMxcUri = format!("mxc://{SERVER}/sunset").into();
        image.media = Some(Media {
            source: MediaSource::Plain(mxc),
            filename: "sunset.png".into(),
            kind: MediaKind::Image,
            mimetype: Some("image/png".into()),
            thumbnail: None,
            size: Some(48_213),
        });
        s.picture(&image, 44, 11);
        lines.push(image);
    }
    s.post("Night Owls", lines);
    s.app.info("jo_bloggs is away: making tea");

    let spoilers = s.line(19, "bob", "chapter 3 spoilers incoming");
    s.post("Book Club", vec![spoilers]);
    let mut ping = s.line(14, "carol", "you: can you look at the flaky test?");
    ping.highlight = true;
    s.post("Team Tests", vec![ping]);
    let dm = s.line(11, "jo_bloggs", "did you push it yet");
    s.post("jo_bloggs", vec![dm]);

    s.members(
        "Night Owls",
        &[
            ("you", '@'),
            ("sam", '@'),
            ("jo_bloggs", '+'),
            ("alice", ' '),
            ("bob", ' '),
            ("carol", ' '),
            ("dee", ' '),
            ("eve", ' '),
        ],
    );
    s.presence(&[
        ("you", PresenceKind::Online, None),
        ("sam", PresenceKind::Online, None),
        ("jo_bloggs", PresenceKind::Away, Some("making tea")),
        ("alice", PresenceKind::Online, None),
        ("bob", PresenceKind::Offline, None),
        ("alex", PresenceKind::Offline, None),
    ]);
    let typing = vec![(user("jo_bloggs"), "jo_bloggs".into())];
    s.app.on_event(AppEvent::Typing { account: ACCOUNT.into(), room_id: room_id("Night Owls"), nicks: typing });
    s.app.input.set_text("and it runs in the background, like irssi in screen");
    s
}

/// Someone's picture: a smiling face on a teal ground.
fn face() -> image::DynamicImage {
    let size = 160u32;
    let image = image::RgbImage::from_fn(size, size, |x, y| {
        let (fx, fy) = (x as f32 - 80.0, y as f32 - 80.0);
        let d = (fx * fx + fy * fy).sqrt();
        let eye = |cx: f32| ((x as f32 - cx).powi(2) + (y as f32 - 64.0).powi(2)).sqrt() < 9.0;
        let smile = (d - 38.0).abs() < 5.0 && fy > 14.0;
        let rgb = if d > 62.0 {
            [20, 120, 120]
        } else if eye(58.0) || eye(102.0) || smile {
            [40, 30, 30]
        } else {
            [250, 200, 90]
        };
        image::Rgb(rgb)
    });
    image::DynamicImage::ImageRgb8(image)
}

/// A picture to show inline: a sunset over a grid, in the colours of 1986.
fn sunset() -> image::DynamicImage {
    let (w, h) = (480u32, 240u32);
    let horizon = h * 64 / 100;
    let mix = |a: [f32; 3], b: [f32; 3], t: f32| [0, 1, 2].map(|i| a[i] + (b[i] - a[i]) * t.clamp(0.0, 1.0));
    let image = image::RgbImage::from_fn(w, h, |x, y| {
        let (fx, fy) = (x as f32, y as f32);
        let rgb = if y < horizon {
            let t = fy / horizon as f32;
            let sky = if t < 0.6 {
                mix([24.0, 12.0, 64.0], [130.0, 36.0, 130.0], t / 0.6)
            } else {
                mix([130.0, 36.0, 130.0], [250.0, 110.0, 110.0], (t - 0.6) / 0.4)
            };
            let (cx, cy, r) = (w as f32 / 2.0, horizon as f32 - 46.0, 72.0);
            let inside = (fx - cx).powi(2) + (fy - cy).powi(2) <= r * r;
            // Stripes cut through the sun's lower half, widening downwards.
            let below = fy - (cy - 10.0);
            let striped = below > 0.0 && (below % 20.0) < 5.0 + below / 6.0;
            // Mountains in front of it.
            let ridge = horizon as f32 - 4.0 - 14.0 * ((fx / 37.0).sin() + (fx / 61.0 + 1.0).sin()).abs().powi(2);
            if fy > ridge {
                mix([40.0, 14.0, 70.0], [70.0, 20.0, 90.0], (fy - ridge) / 40.0)
            } else if inside && !striped {
                mix([255.0, 230.0, 100.0], [255.0, 70.0, 140.0], (fy - (cy - r)) / (2.0 * r))
            } else {
                sky
            }
        } else {
            // The grid, receding to the horizon.
            let depth = fy - horizon as f32 + 1.0;
            let row = (240.0 / depth) % 1.0;
            let col = ((fx - w as f32 / 2.0) / depth * 3.0).rem_euclid(8.0);
            let line = row < 0.12 * (depth / 20.0).min(1.0) + 0.04 || col < 0.35;
            if line { [255.0, 60.0, 200.0] } else { mix([30.0, 6.0, 50.0], [12.0, 2.0, 24.0], depth / 80.0) }
        };
        image::Rgb(rgb.map(|v| v as u8))
    });
    image::DynamicImage::ImageRgb8(image)
}

// ---- drawing a buffer as SVG -------------------------------------------

const CELL_W: f64 = 9.0;
const CELL_H: f64 = 19.0;
const FONT_SIZE: f64 = 15.0;
const PAD: f64 = 14.0;
const TITLE_BAR: f64 = 32.0;

/// A dark terminal's colours, for the themes that leave them to it.
const DEFAULT_FG: u32 = 0xd5d8de;
const DEFAULT_BG: u32 = 0x15161c;
/// A light terminal's, for the theme made for one.
const LIGHT_FG: u32 = 0x24292f;
const LIGHT_BG: u32 = 0xfafafa;

thread_local! {
    /// The terminal's own colours a screenshot is drawn in: (text, paper).
    static TERMINAL: std::cell::Cell<(u32, u32)> = const { std::cell::Cell::new((DEFAULT_FG, DEFAULT_BG)) };
}

/// Draw in a light terminal (for the light theme), or a dark one.
fn light_terminal(light: bool) -> (u32, u32) {
    let colours = if light { (LIGHT_FG, LIGHT_BG) } else { (DEFAULT_FG, DEFAULT_BG) };
    TERMINAL.with(|t| t.set(colours));
    colours
}
const ANSI: [u32; 16] = [
    0x15161c, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xabb2bf, 0x5c6370, 0xff8b94, 0xb5e890,
    0xffd68a, 0x8cc8ff, 0xe5a3f5, 0x7fdbe6, 0xf5f6f8,
];

fn xterm(n: u8) -> u32 {
    match n {
        0..16 => ANSI[usize::from(n)],
        16..232 => {
            let level = |v: u8| if v == 0 { 0 } else { 55 + 40 * u32::from(v) };
            let n = n - 16;
            (level(n / 36) << 16) | (level(n / 6 % 6) << 8) | level(n % 6)
        }
        _ => {
            let v = 8 + 10 * u32::from(n - 232);
            (v << 16) | (v << 8) | v
        }
    }
}

fn rgb(color: Color, default: u32) -> u32 {
    match color {
        Color::Reset => default,
        Color::Black => ANSI[0],
        Color::Red => ANSI[1],
        Color::Green => ANSI[2],
        Color::Yellow => ANSI[3],
        Color::Blue => ANSI[4],
        Color::Magenta => ANSI[5],
        Color::Cyan => ANSI[6],
        Color::Gray => ANSI[7],
        Color::DarkGray => ANSI[8],
        Color::LightRed => ANSI[9],
        Color::LightGreen => ANSI[10],
        Color::LightYellow => ANSI[11],
        Color::LightBlue => ANSI[12],
        Color::LightMagenta => ANSI[13],
        Color::LightCyan => ANSI[14],
        Color::White => ANSI[15],
        Color::Indexed(n) => xterm(n),
        Color::Rgb(r, g, b) => (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b),
    }
}

/// A cell's colours, after reverse video.
fn colors(cell: &ratatui::buffer::Cell) -> (u32, u32) {
    let (text, paper) = TERMINAL.with(|t| t.get());
    let (fg, bg) = (rgb(cell.fg, text), rgb(cell.bg, paper));
    if cell.modifier.contains(Modifier::REVERSED) { (bg, fg) } else { (fg, bg) }
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The one character a cell shows, if it's one.
fn single(symbol: &str) -> Option<char> {
    let mut chars = symbol.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    }
}

/// The filled parts of a block or sextant character, as fractions of the
/// cell: (x, y, width, height).
fn blocks(c: char) -> Option<Vec<(f64, f64, f64, f64)>> {
    let quad = |bits: [bool; 4]| {
        let parts = [(0.0, 0.0), (0.5, 0.0), (0.0, 0.5), (0.5, 0.5)];
        (0..4).filter(|&i| bits[i]).map(|i| (parts[i].0, parts[i].1, 0.5, 0.5)).collect()
    };
    let cp = c as u32;
    Some(match cp {
        0x2580 => vec![(0.0, 0.0, 1.0, 0.5)],
        0x2581..=0x2588 => {
            let h = f64::from(cp - 0x2580) / 8.0;
            vec![(0.0, 1.0 - h, 1.0, h)]
        }
        0x2589..=0x258f => vec![(0.0, 0.0, f64::from(0x2590 - cp) / 8.0, 1.0)],
        0x2590 => vec![(0.5, 0.0, 0.5, 1.0)],
        0x2594 => vec![(0.0, 0.0, 1.0, 0.125)],
        0x2595 => vec![(0.875, 0.0, 0.125, 1.0)],
        // Upper left, upper right, lower left, lower right.
        0x2596 => quad([false, false, true, false]),
        0x2597 => quad([false, false, false, true]),
        0x2598 => quad([true, false, false, false]),
        0x2599 => quad([true, false, true, true]),
        0x259a => quad([true, false, false, true]),
        0x259b => quad([true, true, true, false]),
        0x259c => quad([true, true, false, true]),
        0x259d => quad([false, true, false, false]),
        0x259e => quad([false, true, true, false]),
        0x259f => quad([false, true, true, true]),
        0x1fb00..=0x1fb3b => {
            // Counting order, skipping the halves that have their own
            // characters (21 ▌ and 42 ▐).
            let mut bits = cp - 0x1fb00 + 1;
            bits += u32::from(bits >= 21);
            bits += u32::from(bits >= 42);
            (0..6)
                .filter(|i| bits & (1 << i) != 0)
                .map(|i| (f64::from(i % 2) * 0.5, f64::from(i / 2) / 3.0, 0.5, 1.0 / 3.0))
                .collect()
        }
        _ => return None,
    })
}

/// Box-drawing lines: the arms (up, down, left, right), each 0 none,
/// 1 light, 2 heavy, 3 double; and whether the corner is rounded.
fn box_arms(c: char) -> Option<([u8; 4], bool)> {
    Some(match c {
        '─' => ([0, 0, 1, 1], false),
        '│' => ([1, 1, 0, 0], false),
        '┌' => ([0, 1, 0, 1], false),
        '┐' => ([0, 1, 1, 0], false),
        '└' => ([1, 0, 0, 1], false),
        '┘' => ([1, 0, 1, 0], false),
        '├' => ([1, 1, 0, 1], false),
        '┤' => ([1, 1, 1, 0], false),
        '┬' => ([0, 1, 1, 1], false),
        '┴' => ([1, 0, 1, 1], false),
        '┼' => ([1, 1, 1, 1], false),
        '╭' => ([0, 1, 0, 1], true),
        '╮' => ([0, 1, 1, 0], true),
        '╰' => ([1, 0, 0, 1], true),
        '╯' => ([1, 0, 1, 0], true),
        '━' => ([0, 0, 2, 2], false),
        '┃' => ([2, 2, 0, 0], false),
        '┏' => ([0, 2, 0, 2], false),
        '┓' => ([0, 2, 2, 0], false),
        '┗' => ([2, 0, 0, 2], false),
        '┛' => ([2, 0, 2, 0], false),
        '═' => ([0, 0, 3, 3], false),
        '║' => ([3, 3, 0, 0], false),
        '╔' => ([0, 3, 0, 3], false),
        '╗' => ([0, 3, 3, 0], false),
        '╚' => ([3, 0, 0, 3], false),
        '╝' => ([3, 0, 3, 0], false),
        '╠' => ([3, 3, 0, 3], false),
        '╣' => ([3, 3, 3, 0], false),
        '╦' => ([0, 3, 3, 3], false),
        '╩' => ([3, 0, 3, 3], false),
        '╬' => ([3, 3, 3, 3], false),
        _ => return None,
    })
}

/// SVG path data for a box-drawing character in the cell at (x, y), and
/// its stroke width.
fn box_path(arms: [u8; 4], rounded: bool, x: f64, y: f64) -> (String, f64) {
    let (x1, y1) = (x + CELL_W, y + CELL_H);
    let (cx, cy) = (x + CELL_W / 2.0, y + CELL_H / 2.0);
    let [up, _, left, _] = arms;
    let mut d = String::new();
    let line = |d: &mut String, points: &[(f64, f64)]| {
        for (i, (px, py)) in points.iter().enumerate() {
            let _ = write!(d, "{}{px:.1},{py:.1}", if i == 0 { "M" } else { "L" });
        }
    };
    if rounded {
        let r = CELL_W / 2.0;
        let (vy, hx) = (if up > 0 { y } else { y1 }, if left > 0 { x } else { x1 });
        let (sy, sx) = (if up > 0 { cy - r } else { cy + r }, if left > 0 { cx - r } else { cx + r });
        let _ = write!(d, "M{cx:.1},{vy:.1}L{cx:.1},{sy:.1}Q{cx:.1},{cy:.1} {sx:.1},{cy:.1}L{hx:.1},{cy:.1}");
        return (d, 1.3);
    }
    if arms.contains(&3) {
        // Double lines: two strokes either side of the middle.
        let g = 1.7;
        match arms {
            [0, 0, 3, 3] => {
                line(&mut d, &[(x, cy - g), (x1, cy - g)]);
                line(&mut d, &[(x, cy + g), (x1, cy + g)]);
            }
            [3, 3, 0, 0] => {
                line(&mut d, &[(cx - g, y), (cx - g, y1)]);
                line(&mut d, &[(cx + g, y), (cx + g, y1)]);
            }
            [0, 3, 0, 3] => {
                line(&mut d, &[(x1, cy - g), (cx - g, cy - g), (cx - g, y1)]);
                line(&mut d, &[(x1, cy + g), (cx + g, cy + g), (cx + g, y1)]);
            }
            [0, 3, 3, 0] => {
                line(&mut d, &[(x, cy - g), (cx + g, cy - g), (cx + g, y1)]);
                line(&mut d, &[(x, cy + g), (cx - g, cy + g), (cx - g, y1)]);
            }
            [3, 0, 0, 3] => {
                line(&mut d, &[(cx - g, y), (cx - g, cy + g), (x1, cy + g)]);
                line(&mut d, &[(cx + g, y), (cx + g, cy - g), (x1, cy - g)]);
            }
            [3, 0, 3, 0] => {
                line(&mut d, &[(cx + g, y), (cx + g, cy + g), (x, cy + g)]);
                line(&mut d, &[(cx - g, y), (cx - g, cy - g), (x, cy - g)]);
            }
            _ => {
                // Junctions: each arm as a pair, which is close enough.
                let ends =
                    [((cx, y), (1.0, 0.0)), ((cx, y1), (1.0, 0.0)), ((x, cy), (0.0, 1.0)), ((x1, cy), (0.0, 1.0))];
                for (arm, (end, (ox, oy))) in arms.iter().zip(ends) {
                    if *arm > 0 {
                        for side in [-g, g] {
                            line(&mut d, &[(cx + side * ox, cy + side * oy), (end.0 + side * ox, end.1 + side * oy)]);
                        }
                    }
                }
            }
        }
        return (d, 1.1);
    }
    for (arm, end) in arms.iter().zip([(cx, y), (cx, y1), (x, cy), (x1, cy)]) {
        if *arm > 0 {
            line(&mut d, &[(cx, cy), end]);
        }
    }
    (d, if arms.contains(&2) { 2.6 } else { 1.3 })
}

/// The screen as an SVG: a terminal window with `title`, every cell in
/// its colours.
fn svg(buf: &Buffer, title: &str) -> String {
    let (cols, rows) = (buf.area.width, buf.area.height);
    let cell = |x: u16, y: u16| &buf[(x, y)];
    let at = |x: u16, y: u16| (PAD + f64::from(x) * CELL_W, TITLE_BAR + PAD / 2.0 + f64::from(y) * CELL_H);
    // The commonest background is the terminal's, padding included.
    let mut counts: HashMap<u32, usize> = HashMap::new();
    for y in 0..rows {
        for x in 0..cols {
            *counts.entry(colors(cell(x, y)).1).or_default() += 1;
        }
    }
    let paper = counts.into_iter().max_by_key(|&(c, n)| (n, c)).map_or(TERMINAL.with(|t| t.get()).1, |(c, _)| c);
    let (width, height) = (f64::from(cols) * CELL_W + 2.0 * PAD, f64::from(rows) * CELL_H + TITLE_BAR + PAD);

    let mut out = String::new();
    let _ = writeln!(
        out,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\" \
         viewBox=\"0 0 {width} {height}\" role=\"img\" aria-label=\"{}\">",
        escape(title)
    );
    let _ = writeln!(
        out,
        "<rect width=\"{width}\" height=\"{height}\" rx=\"10\" fill=\"#{paper:06x}\"/>\n\
         <path d=\"M0,{TITLE_BAR}V10a10,10 0 0 1 10,-10H{}a10,10 0 0 1 10,10V{TITLE_BAR}Z\" fill=\"#2b2d35\"/>\n\
         <circle cx=\"20\" cy=\"16\" r=\"6\" fill=\"#ff5f57\"/><circle cx=\"40\" cy=\"16\" r=\"6\" fill=\"#febc2e\"/>\
         <circle cx=\"60\" cy=\"16\" r=\"6\" fill=\"#28c840\"/>\n\
         <text x=\"{}\" y=\"21\" text-anchor=\"middle\" fill=\"#b9bcc6\" \
         font-family=\"-apple-system, 'Segoe UI', Helvetica, Arial, sans-serif\" font-size=\"13\">{}</text>",
        width - 10.0,
        width / 2.0,
        escape(title),
    );

    // Backgrounds in runs, then blocks and underlines, all as rectangles.
    out.push_str("<g shape-rendering=\"crispEdges\">\n");
    for y in 0..rows {
        let mut x = 0;
        while x < cols {
            let bg = colors(cell(x, y)).1;
            let start = x;
            while x < cols && colors(cell(x, y)).1 == bg {
                x += 1;
            }
            if bg != paper {
                let (px, py) = at(start, y);
                let w = f64::from(x - start) * CELL_W;
                let _ = writeln!(
                    out,
                    "<rect x=\"{px:.1}\" y=\"{py:.1}\" width=\"{w:.1}\" height=\"{CELL_H}\" fill=\"#{bg:06x}\"/>"
                );
            }
        }
    }
    for y in 0..rows {
        for x in 0..cols {
            let c = cell(x, y);
            let fg = colors(c).0;
            let (px, py) = at(x, y);
            for (bx, by, bw, bh) in single(c.symbol()).and_then(blocks).unwrap_or_default() {
                let _ = writeln!(
                    out,
                    "<rect x=\"{:.2}\" y=\"{:.2}\" width=\"{:.2}\" height=\"{:.2}\" fill=\"#{fg:06x}\"/>",
                    px + bx * CELL_W,
                    py + by * CELL_H,
                    bw * CELL_W,
                    bh * CELL_H
                );
            }
            if c.modifier.contains(Modifier::UNDERLINED) {
                let _ = writeln!(
                    out,
                    "<rect x=\"{px:.1}\" y=\"{:.1}\" width=\"{CELL_W}\" height=\"1\" fill=\"#{fg:06x}\"/>",
                    py + CELL_H - 3.0
                );
            }
        }
    }
    out.push_str("</g>\n<g fill=\"none\" stroke-linecap=\"square\">\n");
    for y in 0..rows {
        for x in 0..cols {
            let c = cell(x, y);
            if let Some((arms, rounded)) = single(c.symbol()).and_then(box_arms) {
                let (px, py) = at(x, y);
                let (d, stroke) = box_path(arms, rounded, px, py);
                let _ = writeln!(out, "<path d=\"{d}\" stroke=\"#{:06x}\" stroke-width=\"{stroke}\"/>", colors(c).0);
            }
        }
    }
    out.push_str("</g>\n");

    // Text, in runs of one style, every character placed on its cell.
    let _ = writeln!(
        out,
        "<g font-family=\"ui-monospace, 'SF Mono', Menlo, 'Cascadia Mono', Consolas, 'DejaVu Sans Mono', monospace\" \
         font-size=\"{FONT_SIZE}\">"
    );
    let baseline = CELL_H * 0.73;
    for y in 0..rows {
        let mut run: Option<(String, String, Vec<f64>)> = None;
        let flush = |out: &mut String, run: &mut Option<(String, String, Vec<f64>)>| {
            if let Some((attrs, text, xs)) = run.take() {
                let xs: Vec<String> = xs.iter().map(|x| format!("{x:.1}")).collect();
                let _ = writeln!(
                    out,
                    "<text x=\"{}\" y=\"{:.1}\"{attrs}>{}</text>",
                    xs.join(" "),
                    at(0, y).1 + baseline,
                    escape(&text)
                );
            }
        };
        let mut x = 0;
        while x < cols {
            let c = cell(x, y);
            let symbol = c.symbol();
            let (px, _) = at(x, y);
            let wide = symbol.width() > 1;
            // A wide character covers the next cell too.
            x += if wide { 2 } else { 1 };
            let ch = single(symbol);
            let shape = ch.is_some_and(|ch| blocks(ch).is_some() || box_arms(ch).is_some());
            if symbol.trim().is_empty() || shape || c.modifier.contains(Modifier::HIDDEN) {
                flush(&mut out, &mut run);
                continue;
            }
            let mut attrs = format!(" fill=\"#{:06x}\"", colors(c).0);
            for (modifier, attr) in [
                (Modifier::BOLD, " font-weight=\"bold\""),
                (Modifier::ITALIC, " font-style=\"italic\""),
                (Modifier::DIM, " opacity=\"0.6\""),
                (Modifier::CROSSED_OUT, " text-decoration=\"line-through\""),
            ] {
                if c.modifier.contains(modifier) {
                    attrs.push_str(attr);
                }
            }
            // Emoji and other clusters stand alone, so the rest stay on the
            // grid.
            if ch.is_none() || wide {
                flush(&mut out, &mut run);
                run = Some((attrs, symbol.to_owned(), vec![px]));
                flush(&mut out, &mut run);
                continue;
            }
            match &mut run {
                Some((a, text, xs)) if *a == attrs => {
                    text.push_str(symbol);
                    xs.push(px);
                }
                _ => {
                    flush(&mut out, &mut run);
                    run = Some((attrs, symbol.to_owned(), vec![px]));
                }
            }
        }
        flush(&mut out, &mut run);
    }
    out.push_str("</g>\n</svg>\n");
    out
}

fn save(dir: &Path, name: &str, scene: &mut Scene, (cols, rows): (u16, u16)) {
    let buf = scene.draw(cols, rows);
    let path = dir.join(format!("{name}.svg"));
    std::fs::write(&path, svg(&buf, &scene.title())).unwrap();
    println!("wrote {}", path.display());
}

#[test]
#[ignore = "writes the screenshots: scripts/screenshots.sh"]
fn screenshots() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("site/screenshots");
    std::fs::create_dir_all(dir.join("themes")).unwrap();

    save(&dir, "main", &mut night_owls(ThemeName::Default, true), (124, 33));

    // Two rooms side by side, in neon.
    let mut panes = night_owls(ThemeName::BlueTron, false);
    let club = [
        panes.line(16, "bob", "ok who else finished it"),
        panes.line(15, "dee", "the ending!! no spoilers but wow"),
        panes.line(13, "you", "the sky above the port, etc"),
        panes.line(2, "carol", "next month: Snow Crash? https://example.org/books/snow-crash"),
    ];
    panes.post("Book Club", club.to_vec());
    // Unencrypted, so its links get titles (the server reads it anyway).
    let book_club = panes.window("Book Club");
    panes.app.windows[book_club].encrypted = false;
    // The link's title and picture, from the homeserver's preview.
    let painter = panes.app.picker.clone().unwrap();
    let (w, h) = super::LINK_PICTURE;
    panes.app.on_event(AppEvent::LinkTitle {
        url: "https://example.org/books/snow-crash".into(),
        title: Some("Snow Crash, by Neal Stephenson — Example Books".into()),
        picture: painter.fit(sunset(), Size::new(w, h)).ok(),
    });
    panes.app.cmd_split("Book Club", true).unwrap();
    save(&dir, "panes", &mut panes, (124, 30));

    // /themes, trying turbo.
    let mut picking = night_owls(ThemeName::Default, true);
    picking.app.open_theme_picker();
    let picker = picking.app.theme_picker.as_mut().unwrap();
    picker.selected = picker.shown().iter().position(|e| e.id == ThemeName::Turbo.into()).unwrap();
    picking.app.try_picked_theme();
    save(&dir, "picker", &mut picking, (124, 33));

    // Someone's profile, from the member list.
    let mut profile = night_owls(ThemeName::Default, false);
    let sam = profile.app.windows[profile.window("Night Owls")].members.iter().position(|m| m.name == "sam");
    profile.app.open_profile(user("sam"), "sam".into(), sam);
    let painter = profile.app.picker.clone().unwrap();
    let (w, h) = super::PROFILE_PICTURE;
    profile.app.on_event(AppEvent::UserInfo {
        account: ACCOUNT.into(),
        user: user("sam"),
        info: crate::matrix::UserInfo {
            display: Some("Sam Okafor".into()),
            identity: Some(true),
            devices: vec![("QWERTYUIOP".into(), "phone".into(), true), ("ASDFGHJKLZ".into(), "laptop".into(), true)],
            fetched: true,
            ..Default::default()
        },
    });
    let picture = painter.fit(face(), Size::new(w, h)).ok();
    profile.app.on_event(AppEvent::UserPicture { user: user("sam"), picture });
    save(&dir, "profile", &mut profile, (124, 30));

    // /media: Night Owls' pictures and files.
    let mut media = night_owls(ThemeName::Default, true);
    let extra = [
        (8, "dee", "[file] keymap.txt", "keymap.txt", MediaKind::File, 2_048),
        (7, "carol", "[image] sunrise.png", "sunrise.png", MediaKind::Image, 51_002),
        (6, "bob", "[video] demo.mp4", "demo.mp4", MediaKind::Video, 12_400_000),
        (5, "you", "[voice] 0:12 ▁▂▄▆█▆▅▃▂", "voice.ogg", MediaKind::Audio, 19_310),
        (4, "alice", "[image] neon-grid.png", "neon-grid.png", MediaKind::Image, 64_900),
        (3, "sam", "[file] setlist.pdf", "setlist.pdf", MediaKind::File, 84_211),
        (2, "jo_bloggs", "[image] smile.png", "smile.png", MediaKind::Image, 9_812),
    ];
    let lines: Vec<Line> = extra
        .into_iter()
        .map(|(mins, nick, body, name, kind, size)| {
            let mut line = media.line(mins, nick, body);
            line.media = Some(Media {
                source: MediaSource::Plain(format!("mxc://{SERVER}/{name}").into()),
                filename: name.into(),
                kind,
                mimetype: None,
                thumbnail: None,
                size: Some(size),
            });
            line
        })
        .collect();
    media.post("Night Owls", lines);
    media.app.open_gallery().unwrap();
    media.draw(124, 33);
    let painter = media.app.picker.clone().unwrap();
    let (w, h) = gallery::PICTURE;
    for (event_id, m) in std::mem::take(&mut media.app.tile_requests) {
        let image = match m.filename.as_str() {
            "smile.png" => face(),
            "neon-grid.png" => sunset().huerotate(150),
            "sunrise.png" => sunset().huerotate(60).fliph(),
            _ => sunset(),
        };
        let picture = painter.fit(image, Size::new(w, h)).ok();
        media.app.on_event(AppEvent::Tile { event_id, picture });
    }
    media.app.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    save(&dir, "gallery", &mut media, (124, 33));

    // Home (F4): what's waiting.
    let mut home = night_owls(ThemeName::Default, false);
    home.app.on_event(AppEvent::Invites {
        account: ACCOUNT.into(),
        invites: vec![crate::home::Invitation {
            account: ACCOUNT.into(),
            room_id: room_id("Chess Club"),
            name: "Chess Club".into(),
            inviter: Some("alex".into()),
        }],
    });
    home.app.open_home();
    save(&dir, "home", &mut home, (124, 30));

    for (name, theme) in ThemeName::ALL {
        light_terminal(theme == ThemeName::Light);
        save(&dir.join("themes"), name, &mut night_owls(theme, true), (100, 32));
    }
    light_terminal(false);
    // The website's themes page lists them from this: shelf, what each is,
    // and its colours for a swatch.
    let colour = |c: Option<Color>, default| format!("#{:06x}", rgb(c.unwrap_or(Color::Reset), default));
    let list: Vec<serde_json::Value> = ThemeName::ALL
        .iter()
        .map(|&(name, theme)| {
            let t = Theme::builtin(theme);
            let (text, paper) = if theme == ThemeName::Light { (LIGHT_FG, LIGHT_BG) } else { (DEFAULT_FG, DEFAULT_BG) };
            serde_json::json!({
                "name": name,
                "shelf": theme.group(),
                "about": theme.about(),
                "bg": colour(t.base.bg, paper),
                "fg": colour(t.base.fg, text),
                "accent": colour(t.accent.fg, text),
            })
        })
        .collect();
    let path = dir.join("themes/themes.json");
    std::fs::write(&path, serde_json::to_string_pretty(&list).unwrap() + "\n").unwrap();
    println!("wrote {}", path.display());
}

#[test]
fn block_characters_fill_their_parts() {
    assert_eq!(blocks('█'), Some(vec![(0.0, 0.0, 1.0, 1.0)]));
    assert_eq!(blocks('▌'), Some(vec![(0.0, 0.0, 0.5, 1.0)]));
    // 🬀 is the top-left sextant; 🬻 everything but it.
    assert_eq!(blocks('\u{1fb00}'), Some(vec![(0.0, 0.0, 0.5, 1.0 / 3.0)]));
    assert_eq!(blocks('\u{1fb3b}').unwrap().len(), 5);
    assert!(blocks('a').is_none() && box_arms('a').is_none());
    let svg = svg(&Buffer::with_lines(["a<b ╭─╮"]), "x & y");
    assert!(svg.contains("a&lt;b") && svg.contains("x &amp; y") && svg.contains("<path"));
}
