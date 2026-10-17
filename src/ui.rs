//! irssi-style layout: topic bar, scrollback, status bar, input line.

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
};

use chrono::{DateTime, Local};
use matrix_sdk::{
    notification_settings::RoomNotificationMode,
    ruma::{EventId, OwnedEventId, UserId},
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect, Size},
    style::{Color, Modifier, Style},
    text::{Line as TLine, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use unicode_width::UnicodeWidthChar;

use ratatui_image::Image;

use crate::{
    app::{App, ConnStatus, HelpRow, Preview, help_rows},
    config::Config,
    input::display_width,
    links,
    model::{Activity, Fold, Line, LineKind, PresenceKind},
    richtext::Marks,
    switcher,
    theme::Theme,
    viewer,
};

/// Where things were drawn in the last frame, so mouse clicks can find
/// them.
#[derive(Default)]
pub struct Hits {
    /// Window list rows: (row, window index).
    pub sidebar: Vec<(Rect, usize)>,
    pub panes: Vec<PaneHits>,
    /// Member list rows: (row, index into the active window's members).
    pub nicklist: Vec<(Rect, usize)>,
    /// The input line, the column its text starts at, and how many
    /// characters are scrolled out of view on the left.
    pub input: Option<(Rect, u16, usize)>,
    /// The quick switcher's box, and its rows: (row, entry index).
    pub switcher: Option<(Rect, Vec<(Rect, usize)>)>,
}

pub struct PaneHits {
    pub area: Rect,
    pub window: usize,
    /// One per screen row, from the top of `area`.
    pub rows: Vec<RowHit>,
}

#[derive(Clone, Debug, Default)]
pub struct RowHit {
    /// The message drawn on this row.
    pub event: Option<OwnedEventId>,
    /// Underlined (link) stretches: (from column, to column, their text).
    pub links: Vec<(u16, u16, String)>,
    /// Part of an image preview.
    pub image: bool,
}

pub fn draw(f: &mut Frame, app: &mut App) {
    app.hits = Hits::default();
    if app.cfg.palette.base != Style::new() {
        f.render_widget(Block::new().style(app.cfg.palette.base), f.area());
    }
    let [topic, middle, status, input] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1), Constraint::Length(1)])
            .areas(f.area());

    // Side panes give way on narrow terminals.
    let total = middle.width;
    let side_w = if app.show_sidebar && total >= 60 { sidebar_width(app) } else { 0 };
    let win = &app.windows[app.active];
    let wants_nicks = app.show_nicklist && win.room.is_some() && !win.is_dm && win.members.len() > 2;
    let nick_w = if wants_nicks && total >= side_w + 70 { nicklist_width(app) } else { 0 };
    let [side, main, nicks] =
        Layout::horizontal([Constraint::Length(side_w), Constraint::Min(20), Constraint::Length(nick_w)]).areas(middle);

    draw_topic(f, topic, app);
    if side_w > 0 {
        app.hits.sidebar = draw_sidebar(f, side, app);
    }
    draw_panes(f, main, app);
    if nick_w > 0 {
        app.hits.nicklist = draw_nicklist(f, nicks, app);
    }
    draw_status(f, status, app);
    app.hits.input = Some(draw_input(f, input, app));
    if app.help.is_some() {
        draw_help(f, app);
    }
    if app.switcher.is_some() {
        app.hits.switcher = Some(draw_switcher(f, app));
    }
    if app.viewer.is_some() {
        draw_viewer(f, app);
    }
}

/// The full-screen image viewer: a title bar, and the image centred below.
fn draw_viewer(f: &mut Frame, app: &mut App) {
    let t = app.cfg.palette.clone();
    let screen = f.area();
    let Some(v) = app.viewer.as_mut() else { return };
    f.render_widget(Clear, screen);
    let [bar, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(screen);
    v.area = Size::new(body.width, body.height);
    let zoom = if v.zoom == 1 { "fit".to_owned() } else { format!("{}×", v.zoom) };
    let title: String = v.title.chars().filter(|c| !c.is_control()).collect();
    let text = format!(" {title} · {zoom} · +/- zoom · arrows move · 0 fit · Esc close");
    f.render_widget(Paragraph::new(text).style(t.bar), bar);
    match (&v.image, &v.frame) {
        (viewer::Loading::Failed(e), _) => {
            let msg = format!("  couldn't show it: {e}");
            f.render_widget(Paragraph::new(Span::styled(msg, t.error)), body);
        }
        (_, Some((_, protocol))) => {
            let size = protocol.size();
            let x = body.x + body.width.saturating_sub(size.width) / 2;
            let y = body.y + body.height.saturating_sub(size.height) / 2;
            f.render_widget(Image::new(protocol), Rect { x, y, width: size.width, height: size.height });
        }
        _ => f.render_widget(Paragraph::new(Span::styled("  loading the full image…", t.dim)), body),
    }
}

/// The quick switcher: what you've typed, and the best matches under it.
fn draw_switcher(f: &mut Frame, app: &mut App) -> (Rect, Vec<(Rect, usize)>) {
    let t = app.cfg.palette.clone();
    let entries = app.switcher_entries();
    let screen = f.area();
    let Some(sw) = app.switcher.as_mut() else { return (Rect::default(), Vec::new()) };
    sw.selected = sw.selected.min(entries.len().saturating_sub(1));
    let width = screen.width.saturating_sub(4).min(72);
    let height = (entries.len().clamp(1, 12) as u16 + 3).min(screen.height.saturating_sub(2));
    let area = Rect { x: screen.x + (screen.width - width) / 2, y: screen.y + 1, width, height };
    let block = Block::bordered()
        .border_style(t.frame)
        .title(Span::styled(
            if matches!(sw.purpose, switcher::Purpose::Forward { .. }) { " Forward to " } else { " Go to " },
            t.accent,
        ))
        .title_bottom(TLine::from(" ↑↓ choose · Enter go · Esc close ").right_aligned());
    let inner = block.inner(area);
    f.render_widget(Clear, area);
    f.render_widget(block, area);

    let query: String = sw.query.chars().filter(|c| !c.is_control()).collect();
    let prompt = TLine::from(vec![Span::styled("› ", t.accent), Span::raw(query.clone())]);
    f.render_widget(Paragraph::new(prompt), Rect { height: 1, ..inner });
    let cursor_x = (inner.x + 2 + display_len(&query) as u16).min(inner.right().saturating_sub(1));
    f.set_cursor_position(Position::new(cursor_x, inner.y));

    let list = Rect { y: inner.y + 1, height: inner.height.saturating_sub(1), ..inner };
    if entries.is_empty() {
        f.render_widget(Paragraph::new(Span::styled("  nothing matches", t.dim)), list);
        return (area, Vec::new());
    }
    let rows = list.height as usize;
    let start = (sw.selected + 1).saturating_sub(rows);
    let mut hits = Vec::new();
    for (row, (n, entry)) in entries.iter().enumerate().skip(start).take(rows).enumerate() {
        let rect = Rect { y: list.y + row as u16, height: 1, ..list };
        f.render_widget(Paragraph::new(switcher_row(&t, entry, list.width as usize, n == sw.selected)), rect);
        hits.push((rect, n));
    }
    (area, hits)
}

/// One switcher entry: window number, name (matched letters picked out),
/// and unread count and server or "closed" at the right.
fn switcher_row(t: &Theme, entry: &switcher::Entry, width: usize, selected: bool) -> TLine<'static> {
    let number = match entry.target {
        switcher::Target::Window(i) => format!("{:>3} ", i + 1),
        switcher::Target::Closed { .. } => "    ".to_owned(),
    };
    let right = match (entry.unread, entry.detail.as_str()) {
        (0, detail) => detail.to_owned(),
        (n, "") => n.to_string(),
        (n, detail) => format!("{n} · {detail}"),
    };
    let base = if selected { Style::new().add_modifier(Modifier::REVERSED) } else { activity_style(t, entry.activity) };
    let dim = if selected { base } else { t.dim };
    let name_w = width.saturating_sub(number.len() + display_len(&right) + 2);
    let mut spans = vec![Span::styled(number, dim)];
    let mut used = 0;
    for (i, c) in entry.name.chars().filter(|c| !c.is_control()).enumerate() {
        let w = c.width().unwrap_or(0);
        if used + w > name_w {
            break;
        }
        used += w;
        let style = if entry.matched.contains(&i) { base.patch(t.matched) } else { base };
        spans.push(Span::styled(c.to_string(), style));
    }
    spans.push(Span::styled(" ".repeat(width.saturating_sub(4 + used + display_len(&right))), base));
    spans.push(Span::styled(right, dim));
    TLine::from(spans)
}

/// The F1 help screen: keys and commands, filtered by what you type.
fn draw_help(f: &mut Frame, app: &mut App) {
    let t = app.cfg.palette.clone();
    let screen = f.area();
    let width = screen.width.saturating_sub(4).min(110);
    let area = Rect {
        x: screen.x + (screen.width - width) / 2,
        y: screen.y + 1,
        width,
        height: screen.height.saturating_sub(2),
    };
    let Some(help) = app.help.as_mut() else { return };
    let rows = help_rows(&help.filter);
    let key_w = rows
        .iter()
        .map(|r| match r {
            HelpRow::Entry(k, _) => display_len(k),
            HelpRow::Section(_) => 0,
        })
        .max()
        .unwrap_or(0)
        .min(36);
    let mut lines: Vec<TLine> = Vec::new();
    for row in rows {
        match row {
            HelpRow::Section(title) => {
                if !lines.is_empty() {
                    lines.push(TLine::default());
                }
                lines.push(TLine::from(Span::styled(title, t.accent)));
            }
            HelpRow::Entry(key, what) => {
                let pad = " ".repeat(key_w.saturating_sub(display_len(&key)) + 2);
                lines.push(TLine::from(vec![
                    Span::styled(format!("  {key}"), t.matched.remove_modifier(Modifier::BOLD)),
                    Span::raw(pad),
                    Span::raw(what),
                ]));
            }
        }
    }
    if lines.is_empty() {
        lines.push(TLine::from(Span::styled("  nothing matches", t.dim)));
    }
    let inner_h = area.height.saturating_sub(2) as usize;
    help.scroll = help.scroll.min(lines.len().saturating_sub(inner_h));
    let filter =
        if help.filter.is_empty() { " type to filter ".to_owned() } else { format!(" filter: {} ", help.filter) };
    let block = Block::bordered()
        .border_style(t.frame)
        .title(Span::styled(" Textrix help ", t.accent))
        .title_bottom(TLine::from(format!("{filter}· ↑↓ PgUp/PgDn scroll · Esc closes ")).right_aligned());
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(lines).block(block).scroll((help.scroll as u16, 0)), area);
}

fn activity_style(t: &Theme, level: Activity) -> Style {
    match level {
        Activity::Highlight => t.highlight,
        Activity::Message => t.activity,
        Activity::Event => t.activity_event,
        Activity::None => t.inactive,
    }
}

fn sidebar_width(app: &App) -> u16 {
    let longest = app.windows.iter().map(|w| w.title().chars().count()).max().unwrap_or(6);
    let avatar = if app.cfg.sidebar_avatars { 3 } else { 0 };
    (longest.clamp(8, 22) + 9 + avatar) as u16
}

/// irssi adv_windowlist-style: window number, name and unread count,
/// coloured by activity, grouped by server when there are several.
/// One row of the window list.
#[derive(Debug, PartialEq, Eq)]
pub enum SidebarEntry {
    Header(String),
    Window(usize),
}

/// Window list layout: the status window, then each server's windows,
/// grouped under their spaces when there are any. Window numbers never
/// change, only the order they're listed in.
pub fn sidebar_entries(app: &App) -> Vec<SidebarEntry> {
    // The status window, then other windows that aren't a room (mentions).
    let mut out = vec![SidebarEntry::Window(0)];
    out.extend((1..app.windows.len()).filter(|&i| app.windows[i].room.is_none()).map(SidebarEntry::Window));
    let mut servers: Vec<&str> = Vec::new();
    for w in &app.windows[1..] {
        if let Some(server) = w.account()
            && !servers.contains(&server)
        {
            servers.push(server);
        }
    }
    for server in servers {
        if app.accounts.len() > 1 {
            out.push(SidebarEntry::Header(server.to_owned()));
        }
        let windows: Vec<usize> =
            (1..app.windows.len()).filter(|&i| app.windows[i].account() == Some(server)).collect();
        let spaces = app.spaces.get(server).map(Vec::as_slice).unwrap_or_default();
        let mut placed: HashSet<usize> = HashSet::new();
        for space in spaces {
            let members: Vec<usize> = windows
                .iter()
                .copied()
                .filter(|i| !placed.contains(i))
                .filter(|&i| app.windows[i].room.as_ref().is_some_and(|k| space.children.contains(&k.room_id)))
                .collect();
            if !members.is_empty() {
                out.push(SidebarEntry::Header(format!("◆ {}", space.name)));
                placed.extend(&members);
                out.extend(members.into_iter().map(SidebarEntry::Window));
            }
        }
        let rest: Vec<usize> = windows.into_iter().filter(|i| !placed.contains(i)).collect();
        if !rest.is_empty() && !placed.is_empty() {
            out.push(SidebarEntry::Header("◇ other".into()));
        }
        out.extend(rest.into_iter().map(SidebarEntry::Window));
    }
    out
}

/// irssi adv_windowlist-style: window number, name and unread count,
/// coloured by activity, grouped by server and space.
fn draw_sidebar(f: &mut Frame, area: Rect, app: &App) -> Vec<(Rect, usize)> {
    let t = &app.cfg.palette;
    let inner = area.width.saturating_sub(1) as usize;
    let entries = sidebar_entries(app);
    let rows: Vec<(TLine<'static>, bool)> = entries
        .iter()
        .map(|entry| match entry {
            SidebarEntry::Header(text) => {
                let text: String = text.chars().take(inner).collect();
                (TLine::from(Span::styled(format!(" {text}"), t.dim.add_modifier(Modifier::BOLD))), false)
            }
            SidebarEntry::Window(i) => (window_row(app, *i, inner), *i == app.active),
        })
        .collect();
    // Keep the active window in view.
    let height = area.height as usize;
    let active_row = rows.iter().position(|(_, a)| *a).unwrap_or(0);
    let start = (active_row + 1).saturating_sub(height);
    let shown: Vec<TLine> = rows.into_iter().skip(start).take(height).map(|(r, _)| r).collect();
    let block = Block::new().borders(Borders::RIGHT).border_style(t.dim);
    f.render_widget(Paragraph::new(shown).block(block), area);

    // Avatar images go into the gap left for them after the window number.
    let mut hits = Vec::new();
    for (row, entry) in entries.iter().skip(start).take(height).enumerate() {
        let SidebarEntry::Window(i) = entry else { continue };
        hits.push((Rect { y: area.y + row as u16, height: 1, ..area }, *i));
        if let Some(Preview::Ready(protocol)) = avatar_of(app, *i) {
            let rect = Rect { x: area.x + 3, y: area.y + row as u16, width: 2, height: 1 };
            f.render_widget(Image::new(protocol), rect);
        }
    }
    hits
}

fn avatar_of(app: &App, i: usize) -> Option<&Preview> {
    let key = app.windows[i].room.as_ref().filter(|k| k.thread.is_none())?;
    app.avatars.get(&(key.account.clone(), key.room_id.clone())).map(|(_, preview)| preview)
}

/// The 3-column avatar slot: blank for a drawn image, otherwise a coloured
/// initial tile.
fn avatar_slot(app: &App, i: usize) -> Vec<Span<'static>> {
    let t = &app.cfg.palette;
    let w = &app.windows[i];
    if matches!(avatar_of(app, i), Some(Preview::Ready(_))) && app.avatar_images() {
        return vec![Span::raw("   ")];
    }
    let Some(key) = &w.room else { return vec![Span::raw("   ")] };
    let initial = if key.thread.is_some() {
        '⤷'
    } else {
        w.title().chars().find(|c| c.is_alphanumeric()).map_or('#', |c| c.to_uppercase().next().unwrap_or(c))
    };
    let tile = match t.nick_color(key.room_id.as_str()) {
        Color::Reset => Style::new().add_modifier(Modifier::REVERSED),
        color => Style::new().fg(Color::Black).bg(color),
    }
    .add_modifier(Modifier::BOLD);
    vec![Span::styled(format!("{initial} "), tile), Span::raw(" ")]
}

fn window_row(app: &App, i: usize, inner: usize) -> TLine<'static> {
    let t = &app.cfg.palette;
    let w = &app.windows[i];
    let active = i == app.active;
    let unread = match (active, w.typing.is_empty(), w.unread()) {
        (false, false, _) => "✎".to_owned(),
        (false, true, n) if n > 0 => n.to_string(),
        // Something you started typing there and haven't sent.
        (false, true, _) if w.has_draft() => "…".to_owned(),
        _ => String::new(),
    };
    let number = format!("{:>2} ", i + 1);
    let avatar = if app.cfg.sidebar_avatars { avatar_slot(app, i) } else { Vec::new() };
    let dot = w.dm_presence(app).map(|p| presence_dot(t, p.kind));
    // Columns before the name: avatar slot and presence dot.
    let lead_w = if avatar.is_empty() { 0 } else { 3 } + if dot.is_some() { 2 } else { 0 };
    let room_w = inner.saturating_sub(number.len() + lead_w + display_len(&unread) + 1);
    let name: String = w.title().chars().filter(|c| !c.is_control()).take(room_w).collect();
    let pad = inner.saturating_sub(number.len() + lead_w + display_len(&name) + display_len(&unread));
    let muted = app.room_mode(i) == Some(RoomNotificationMode::Mute);
    let style = if active {
        t.current
    } else if muted && w.activity == Activity::None {
        t.dim
    } else {
        activity_style(t, w.activity)
    };
    let number_style = match (active, app.on_screen(i)) {
        (true, _) => style,
        (false, true) => t.shown,
        (false, false) => t.dim,
    };
    let mut parts = vec![Span::styled(number, number_style)];
    parts.extend(avatar);
    if let Some((symbol, color)) = dot {
        parts.push(Span::styled(format!("{symbol} "), if active { style } else { Style::new().fg(color) }));
    }
    parts.extend([
        Span::styled(name, style),
        Span::styled(" ".repeat(pad), style),
        Span::styled(unread, if active { style } else { activity_style(t, w.activity) }),
    ]);
    TLine::from(parts)
}

fn nicklist_width(app: &App) -> u16 {
    let win = &app.windows[app.active];
    let longest = win.members.iter().map(|m| m.name.chars().count()).max().unwrap_or(6);
    (longest.clamp(6, 18) + 5) as u16
}

fn presence_dot(t: &Theme, kind: PresenceKind) -> (char, Color) {
    match kind {
        PresenceKind::Online => ('●', t.online),
        PresenceKind::Away => ('●', t.away),
        PresenceKind::Offline => ('○', t.offline),
    }
}

fn draw_nicklist(f: &mut Frame, area: Rect, app: &App) -> Vec<(Rect, usize)> {
    let t = &app.cfg.palette;
    let win = &app.windows[app.active];
    let inner = area.width.saturating_sub(1) as usize;
    let height = area.height as usize;
    let mut rows = vec![TLine::from(Span::styled(format!("Users ({})", win.members.len()), t.dim))];
    let room = height.saturating_sub(1);
    let overflow = win.members.len() > room;
    let take = if overflow { room.saturating_sub(1) } else { room };
    for m in win.members.iter().take(take) {
        let prefix_style = match m.prefix {
            '@' => t.op,
            '+' => t.voice,
            _ => Style::new(),
        };
        let name: String = m.name.chars().filter(|c| !c.is_control()).take(inner.saturating_sub(3)).collect();
        let (symbol, color) = match win.account().and_then(|a| app.presence_of(a, &m.user_id)) {
            Some(p) => presence_dot(t, p.kind),
            None => (' ', Color::Reset),
        };
        rows.push(TLine::from(vec![
            Span::styled(m.prefix.to_string(), prefix_style),
            Span::styled(format!("{symbol} "), Style::new().fg(color)),
            Span::styled(name, Style::new().fg(t.nick_color(m.user_id.as_str()))),
        ]));
    }
    if overflow {
        rows.push(TLine::from(Span::styled(format!("… {} more", win.members.len() - take), t.dim)));
    }
    let block = Block::new().borders(Borders::LEFT).border_style(t.dim);
    f.render_widget(Paragraph::new(rows).block(block), area);
    // Names start on the row after the "Users" header.
    (0..take.min(win.members.len())).map(|n| (Rect { y: area.y + 1 + n as u16, height: 1, ..area }, n)).collect()
}

fn display_len(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

fn draw_topic(f: &mut Frame, area: Rect, app: &App) {
    let t = &app.cfg.palette;
    let win = &app.windows[app.active];
    let mut text = match (&win.room, &win.topic) {
        (None, _) => format!(" Textrix {} — {}", env!("CARGO_PKG_VERSION"), app.servers_summary()),
        (Some(_), Some(topic)) => format!(" {topic}"),
        (Some(_), None) => format!(" {}", win.title()),
    };
    if let Some(presence) = win.dm_presence(app) {
        text.push_str(&format!(" · {} is {}", win.title(), presence.describe()));
    }
    let text: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    f.render_widget(Paragraph::new(text).style(t.bar), area);
    // The pin count stays visible at the right, over a long topic.
    if !win.pinned.is_empty() && area.width > 40 {
        let pins = format!(" 📌 {} pinned · /pins ", win.pinned.len());
        let w = display_len(&pins) as u16;
        let right = Rect { x: area.right() - w, width: w, ..area };
        f.render_widget(Paragraph::new(pins).style(t.bar.add_modifier(Modifier::BOLD)), right);
    }
}

/// The message area: one window, or several split panes, each with a
/// title bar.
fn draw_panes(f: &mut Frame, area: Rect, app: &mut App) {
    let t = app.cfg.palette.clone();
    let panes = app.screen_windows();
    if panes.len() < 2 {
        let idx = app.active;
        (app.view_width, app.view_height, app.windows[idx].width) = (area.width, area.height, area.width);
        return draw_messages(f, area, app, idx);
    }
    let side_by_side = app.split.as_ref().is_some_and(|s| s.side_by_side);
    let fill = vec![Constraint::Fill(1); panes.len()];
    let areas = if side_by_side { Layout::horizontal(fill).split(area) } else { Layout::vertical(fill).split(area) };
    for (n, (&idx, &pane)) in panes.iter().zip(areas.iter()).enumerate() {
        let mut pane = pane;
        if side_by_side && n > 0 {
            let divider = Block::new().borders(Borders::LEFT).border_style(t.dim);
            f.render_widget(&divider, pane);
            pane = divider.inner(pane);
        }
        let [body, bar] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(pane);
        draw_pane_bar(f, bar, app, idx);
        if idx == app.active {
            (app.view_width, app.view_height) = (body.width, body.height);
        }
        app.windows[idx].width = body.width;
        draw_messages(f, body, app, idx);
    }
}

/// A split pane's own status line: which window it is, and whether it has
/// the focus.
fn draw_pane_bar(f: &mut Frame, area: Rect, app: &App, idx: usize) {
    let t = &app.cfg.palette;
    let w = &app.windows[idx];
    let focused = idx == app.active;
    let mut text = format!("{} {}:{}", if focused { '▶' } else { ' ' }, idx + 1, w.title());
    if w.encrypted {
        text.push_str("(+E)");
    }
    if !w.typing.is_empty() {
        text.push_str(&format!("  typing: {}", w.typing.join(", ")));
    }
    if w.scroll > 0 {
        text.push_str(if w.unseen_below { "  [more, new below]" } else { "  [more]" });
    }
    let text: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let style = if focused { t.bar.add_modifier(Modifier::BOLD) } else { t.pane };
    f.render_widget(Paragraph::new(text).style(style), area);
}

fn draw_messages(f: &mut Frame, area: Rect, app: &mut App, idx: usize) {
    let width = area.width as usize;
    let height = area.height as usize;
    let selected = if idx == app.active { app.focus_event() } else { None };
    let thread_window = app.windows[idx].thread_root().map(ToOwned::to_owned);
    let account = app.windows[idx].room.as_ref().map(|k| k.account.clone());
    let images = app.picker.is_some();
    let (previews, requests) = (&mut app.previews, &mut app.preview_requests);
    let cfg = &app.cfg;
    let win = &mut app.windows[idx];
    // Images sit under their message, lined up with the message text.
    let image_x = display_len(&Local::now().format(&cfg.timestamp_format).to_string()) as u16 + 1;
    let mut placements: Vec<(OwnedEventId, usize, u16)> = Vec::new();
    let shown_lines: Vec<&Line> =
        win.lines.iter().filter(|l| cfg.show_joins || l.kind != LineKind::Membership).collect();
    let visible =
        if cfg.fold_joins { fold_noise(shown_lines) } else { shown_lines.into_iter().map(Cow::Borrowed).collect() };

    // Who has read up to which event (not shown on their own messages).
    let mut readers: HashMap<&EventId, Vec<(&UserId, &str)>> = HashMap::new();
    for (user, (nick, event_id)) in &win.read_by {
        readers.entry(&**event_id).or_default().push((&**user, nick.as_str()));
    }

    // The "new messages" divider only matters if someone else said something.
    let last_from_others = visible.iter().rposition(|l| l.is_message() && !l.own && l.event_id.is_some());

    let mut need = height + win.scroll;
    let mut rows: Vec<TLine<'static>> = Vec::new();
    // The message on each row, and whether it's part of an image, for
    // mouse clicks.
    let mut row_events: Vec<(Option<OwnedEventId>, bool)> = Vec::new();
    let mut found_selected = selected.is_none();
    let mut i = visible.len();
    while i > 0 && (rows.len() < need || !found_selected) {
        i -= 1;
        let line: &Line = &visible[i];
        let read: Vec<&str> = line
            .event_id
            .as_ref()
            .and_then(|id| readers.get::<&EventId>(&&**id))
            .map(|users| {
                let mut nicks: Vec<&str> =
                    users.iter().filter(|(u, _)| line.sender.as_deref() != Some(*u)).map(|(_, n)| *n).collect();
                nicks.sort_unstable();
                nicks
            })
            .unwrap_or_default();
        let is_selected = selected.is_some() && line.event_id == selected;
        if images
            && !line.redacted
            && let (Some(media), Some(id)) = (line.media.as_ref().filter(|m| m.is_image), &line.event_id)
        {
            match previews.get(id) {
                Some(Preview::Ready(protocol)) => {
                    let height = protocol.size().height;
                    placements.push((id.clone(), rows.len(), height));
                    rows.extend((0..height).map(|_| TLine::default()));
                    row_events.extend((0..height).map(|_| (Some(id.clone()), true)));
                }
                Some(Preview::Loading | Preview::Failed) => {}
                None => {
                    if let Some(account) = &account {
                        previews.insert(id.clone(), Preview::Loading);
                        requests.push((account.clone(), id.clone(), media.clone()));
                    }
                }
            }
        }
        let below = rows.len();
        let pinned = line.event_id.as_ref().is_some_and(|id| win.pinned.contains(id));
        let mut rendered = render_line_with(line, width, cfg, &read, thread_window.as_deref(), pinned);
        if is_selected {
            for row in &mut rendered {
                *row = row.clone().patch_style(Style::new().add_modifier(Modifier::REVERSED));
            }
        }
        row_events.extend(rendered.iter().map(|_| (line.event_id.clone(), false)));
        rows.extend(rendered.into_iter().rev());
        if is_selected {
            // Scroll just enough to keep the selected message on screen.
            found_selected = true;
            let (lo, hi) = (below, rows.len());
            if lo < win.scroll {
                win.scroll = lo;
            } else if hi > win.scroll + height {
                win.scroll = hi.saturating_sub(height);
            }
            need = height + win.scroll;
        }
        if i > 0 && visible[i - 1].ts.date_naive() != line.ts.date_naive() {
            rows.push(day_changed(&cfg.palette, line.ts));
            row_events.push((None, false));
        }
        if i > 0
            && visible[i - 1].event_id.is_some()
            && visible[i - 1].event_id == win.marker
            && last_from_others.is_some_and(|j| j >= i)
        {
            rows.push(new_messages(&cfg.palette, width));
            row_events.push((None, false));
        }
    }
    let exhausted = i == 0;

    let max_scroll = rows.len().saturating_sub(height);
    if exhausted && win.scroll > max_scroll {
        win.scroll = max_scroll;
    }
    win.at_top = exhausted && win.scroll >= max_scroll;
    if win.scroll == 0 {
        win.unseen_below = false;
    }

    let total = rows.len();
    rows.reverse();
    row_events.reverse();
    let end = total - win.scroll.min(total);
    let start = end.saturating_sub(height);
    let shown = rows[start..end].to_vec();
    let pad = height - shown.len();
    let hits = PaneHits {
        area,
        window: idx,
        rows: std::iter::repeat_n(RowHit::default(), pad)
            .chain(shown.iter().zip(&row_events[start..end]).map(|(row, (event, image))| RowHit {
                event: event.clone(),
                links: underlined(row, area.x),
                image: *image,
            }))
            .collect(),
    };
    let area = Rect { y: area.y + pad as u16, height: shown.len() as u16, ..area };
    f.render_widget(Paragraph::new(shown), area);

    // Draw previews whose reserved rows are fully on screen.
    for (id, below, rows_tall) in placements {
        let Some(Preview::Ready(protocol)) = app.previews.get(&id) else { continue };
        let top = total - (below + rows_tall as usize);
        let bottom = total - 1 - below;
        if top < start || bottom >= end {
            continue;
        }
        let size = protocol.size();
        let x = area.x + image_x;
        if x + size.width > area.right() {
            continue;
        }
        let rect = Rect { x, y: area.y + (top - start) as u16, width: size.width, height: size.height };
        f.render_widget(Image::new(protocol), rect);
    }
    app.hits.panes.push(hits);
}

/// Underlined stretches of a drawn row (links), with their columns.
fn underlined(row: &TLine, x: u16) -> Vec<(u16, u16, String)> {
    let mut out: Vec<(u16, u16, String)> = Vec::new();
    let mut at = x;
    for span in &row.spans {
        let w = display_len(&span.content) as u16;
        if span.style.add_modifier.contains(Modifier::UNDERLINED) {
            match out.last_mut() {
                Some(last) if last.1 == at => {
                    last.1 += w;
                    last.2.push_str(&span.content);
                }
                _ => out.push((at, at + w, span.content.to_string())),
            }
        }
        at += w;
    }
    out
}

/// Runs of two or more joins, leaves and renames on the same day, shown as
/// one line ("alice, bob and carol joined · dave left · eve → erin").
fn fold_noise(lines: Vec<&Line>) -> Vec<Cow<'_, Line>> {
    let mut out = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let start = i;
        while i < lines.len()
            && lines[i].fold.is_some()
            && (i == start || lines[i - 1].ts.date_naive() == lines[i].ts.date_naive())
        {
            i += 1;
        }
        match i - start {
            0 => {
                out.push(Cow::Borrowed(lines[i]));
                i += 1;
            }
            1 => out.push(Cow::Borrowed(lines[start])),
            _ => out.push(Cow::Owned(fold_summary(&lines[start..i]))),
        }
    }
    out
}

fn fold_summary(run: &[&Line]) -> Line {
    let (mut joined, mut left, mut renamed): (Vec<&str>, Vec<&str>, Vec<String>) = Default::default();
    for line in run {
        match &line.fold {
            Some(Fold::Joined(name)) if !joined.contains(&name.as_str()) => joined.push(name),
            Some(Fold::Left(name)) if !left.contains(&name.as_str()) => left.push(name),
            Some(Fold::Renamed { from, to }) => renamed.push(format!("{from} → {to}")),
            _ => {}
        }
    }
    let mut parts = Vec::new();
    if !joined.is_empty() {
        parts.push(format!("{} joined", name_list(&joined)));
    }
    if !left.is_empty() {
        parts.push(format!("{} left", name_list(&left)));
    }
    if renamed.len() > 3 {
        parts.push(format!("{}, and {} more renames", renamed[..3].join(", "), renamed.len() - 3));
    } else if !renamed.is_empty() {
        parts.push(renamed.join(", "));
    }
    let last = run[run.len() - 1];
    let mut line = Line::new(LineKind::Membership, parts.join(" · "));
    (line.ts, line.event_id) = (last.ts, last.event_id.clone());
    line
}

/// "alice", "alice and bob", "alice, bob and carol", "alice, bob and 5 others".
fn name_list(names: &[&str]) -> String {
    match names {
        [one] => (*one).to_owned(),
        [init @ .., last] if names.len() <= 3 => format!("{} and {last}", init.join(", ")),
        [a, b, rest @ ..] => format!("{a}, {b} and {} others", rest.len()),
        [] => String::new(),
    }
}

fn new_messages(t: &Theme, width: usize) -> TLine<'static> {
    let label = " new messages ";
    let side = width.saturating_sub(label.len()) / 2;
    let rule = "─".repeat(side);
    TLine::from(Span::styled(format!("{rule}{label}{rule}"), t.divider))
}

fn day_changed(t: &Theme, ts: DateTime<Local>) -> TLine<'static> {
    TLine::from(vec![Span::styled("-!- ", t.accent), Span::raw(format!("Day changed to {}", ts.format("%a %d %b %Y")))])
}

/// Format one scrollback line and wrap it to `width`, indenting
/// continuation rows under the message text.
pub fn render_line(line: &Line, width: usize, cfg: &Config) -> Vec<TLine<'static>> {
    render_line_with(line, width, cfg, &[], None, false)
}

/// `render_line`, plus the nicks whose read receipt sits on this line, the
/// thread being shown when this is a thread window, and whether the message
/// is pinned.
fn render_line_with(
    line: &Line,
    width: usize,
    cfg: &Config,
    read_by: &[&str],
    thread_window: Option<&EventId>,
    pinned: bool,
) -> Vec<TLine<'static>> {
    let t = &cfg.palette;
    let mut cells: Vec<(char, Style)> = Vec::new();
    let push = |cells: &mut Vec<(char, Style)>, s: &str, style: Style| {
        cells.extend(s.chars().filter_map(|c| sanitize(c).map(|c| (c, style))));
    };

    push(&mut cells, &line.ts.format(&cfg.timestamp_format).to_string(), t.dim);
    push(&mut cells, " ", Style::new());

    let nick_style = if line.highlight {
        t.highlight
    } else if line.own {
        t.own
    } else {
        Style::new().fg(t.nick_color(line.sender.as_ref().map_or(line.nick.as_str(), |s| s.as_str())))
    };
    let mut body_style = match line.kind {
        LineKind::Error => t.error,
        LineKind::Utd => t.dim.add_modifier(Modifier::ITALIC),
        _ if line.highlight => t.highlight_text,
        _ => Style::new(),
    };
    if line.pending || line.redacted {
        body_style = t.dim;
    }

    match line.kind {
        LineKind::Message | LineKind::Utd => {
            push(&mut cells, "<", t.dim);
            push(&mut cells, &line.nick, nick_style);
            push(&mut cells, "> ", t.dim);
        }
        LineKind::Emote => {
            push(&mut cells, " * ", t.accent);
            push(&mut cells, &line.nick, nick_style);
            push(&mut cells, " ", Style::new());
        }
        LineKind::Notice => {
            push(&mut cells, "-", t.dim);
            push(&mut cells, &line.nick, nick_style);
            push(&mut cells, "- ", t.dim);
        }
        LineKind::Error => push(&mut cells, "-!- ", t.error.add_modifier(Modifier::BOLD)),
        LineKind::Membership | LineKind::Event | LineKind::Info => push(&mut cells, "-!- ", t.accent),
    }
    // Inside its own thread window a thread message needs no "in thread"
    // context; only explicit replies to a later message say what they answer.
    let in_this_thread = thread_window.is_some() && line.thread_root.as_deref() == thread_window;
    let context = if in_this_thread {
        line.reply_to.is_some() && line.reply_to != line.thread_root
    } else {
        line.reply_to.is_some() || line.thread
    };
    if context {
        let mark = if line.thread && !in_this_thread { '⤷' } else { '↪' };
        let to = match (&line.reply_nick, &line.reply_snippet) {
            (Some(nick), Some(text)) => format!("{mark} {nick} “{text}” "),
            (Some(nick), None) => format!("{mark} {nick}: "),
            _ => format!("{mark} "),
        };
        push(&mut cells, &to, t.dim);
    }

    let prefix_width: usize = cells.iter().map(|(c, _)| display_width(*c)).sum();
    let indent = if prefix_width > width / 2 { 4.min(width.saturating_sub(1)) } else { prefix_width };

    match &line.rich {
        Some(spans) if !line.redacted => {
            for span in spans {
                let style = rich_style(t, body_style, span.marks, line.highlight);
                if span.marks.spoiler && !line.reveal {
                    let hidden: String = span.text.chars().map(|c| if c.is_whitespace() { c } else { '░' }).collect();
                    push(&mut cells, &hidden, style);
                } else {
                    push(&mut cells, &span.text, style);
                }
            }
        }
        _ => {
            let mut at = 0;
            for range in links::url_ranges(&line.body) {
                push(&mut cells, &line.body[at..range.start], body_style);
                push(&mut cells, &line.body[range.clone()], body_style.patch(t.link));
                at = range.end;
            }
            push(&mut cells, &line.body[at..], body_style);
        }
    }
    if line.edited && !line.redacted {
        push(&mut cells, " (edited)", t.dim);
    }
    if let Some(count) = line.thread_count {
        let text = if count == 1 { " [1 reply]".to_owned() } else { format!(" [{count} replies]") };
        push(&mut cells, &text, t.badge);
    }
    if pinned {
        push(&mut cells, " [pinned]", t.pinned);
    }
    if let Some(percent) = line.progress {
        push(&mut cells, &format!(" ({percent}%)"), t.dim);
    }
    if !line.reactions.is_empty() {
        let summary: Vec<String> = line.reactions.iter().map(|(key, nicks)| format!("{key} {}", nicks.len())).collect();
        push(&mut cells, &format!(" [{}]", summary.join(" ")), t.dim);
    }
    if !read_by.is_empty() {
        let shown = read_by.iter().take(3).copied().collect::<Vec<_>>().join(", ");
        let more = read_by.len().saturating_sub(3);
        let text = if more > 0 { format!(" ✓ {shown} +{more}") } else { format!(" ✓ {shown}") };
        push(&mut cells, &text, t.receipts);
    }

    wrap(&cells, width, indent)
}

fn rich_style(t: &Theme, base: Style, marks: Marks, highlight: bool) -> Style {
    let mut style = base;
    for (on, modifier) in [
        (marks.bold, Modifier::BOLD),
        (marks.italic, Modifier::ITALIC),
        (marks.strike, Modifier::CROSSED_OUT),
        (marks.underline, Modifier::UNDERLINED),
    ] {
        if on {
            style = style.add_modifier(modifier);
        }
    }
    if let Some((r, g, b)) = marks.color
        && t.message_colors
    {
        style = style.fg(Color::Rgb(r, g, b));
    }
    if marks.code {
        style = style.patch(t.code);
    }
    if marks.link {
        style = style.patch(t.link);
    }
    if marks.mention && !highlight {
        style = style.patch(t.mention);
    }
    if marks.dim || marks.spoiler {
        style = style.patch(t.dim);
    }
    style
}

/// Drop characters that could corrupt the terminal or the layout.
///
/// U+FE0F (emoji presentation) makes terminals draw the preceding symbol
/// two columns wide while width tables count it as one, which shifts or
/// clips the rest of the row; without it the symbol renders as text.
fn sanitize(c: char) -> Option<char> {
    match c {
        '\n' => Some('\n'),
        '\t' => Some(' '),
        '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}' | '\u{fe0f}' => None,
        c if c.is_control() => None,
        c => Some(c),
    }
}

fn wrap(cells: &[(char, Style)], width: usize, indent: usize) -> Vec<TLine<'static>> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut i = 0;
    let mut first = true;
    while i < cells.len() || first {
        let avail = if first { width } else { width.saturating_sub(indent).max(1) };
        let mut used = 0;
        let mut j = i;
        let mut last_space = None;
        while j < cells.len() && cells[j].0 != '\n' {
            let w = cells[j].0.width().unwrap_or(0);
            if used + w > avail {
                break;
            }
            if cells[j].0 == ' ' {
                last_space = Some(j);
            }
            used += w;
            j += 1;
        }
        let (end, next) = if j >= cells.len() {
            (j, j)
        } else if cells[j].0 == '\n' {
            (j, j + 1)
        } else if let Some(space) = last_space.filter(|s| *s > i) {
            (space, space + 1)
        } else if j == i {
            (i + 1, i + 1) // a single character wider than the row
        } else {
            (j, j)
        };

        let mut spans = Vec::new();
        if !first && indent > 0 {
            spans.push(Span::raw(" ".repeat(indent)));
        }
        let mut k = i;
        while k < end {
            let style = cells[k].1;
            let mut text = String::new();
            while k < end && cells[k].1 == style {
                text.push(cells[k].0);
                k += 1;
            }
            spans.push(Span::styled(text, style));
        }
        rows.push(TLine::from(spans));
        i = next;
        first = false;
    }
    rows
}

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let t = &app.cfg.palette;
    let win = &app.windows[app.active];
    let mut spans: Vec<Span> = Vec::new();
    let item = |spans: &mut Vec<Span<'static>>, parts: Vec<Span<'static>>| {
        spans.push(Span::styled("[", t.bar_accent));
        spans.extend(parts);
        spans.push(Span::styled("] ", t.bar_accent));
    };

    let now = Local::now().format("%H:%M").to_string();
    // Elite o'clock.
    let clock = if now == "13:37" { "1337".to_owned() } else { now };
    item(&mut spans, vec![Span::styled(clock, t.bar)]);

    if let Some(state) = win.account().and_then(|a| app.accounts.get(a)) {
        let nick = state.nick.clone().unwrap_or_else(|| state.user.clone());
        item(&mut spans, vec![Span::styled(nick, t.bar)]);
    }

    let mut name = format!("{}:{}", app.active + 1, win.title());
    if win.encrypted {
        name.push_str("(+E)");
    }
    item(&mut spans, vec![Span::styled(name, t.bar)]);

    let active: Vec<(usize, Activity)> = app
        .windows
        .iter()
        .enumerate()
        .filter(|(i, w)| !app.on_screen(*i) && w.activity > Activity::None)
        .map(|(i, w)| (i + 1, w.activity))
        .collect();
    if !active.is_empty() {
        let mut parts = vec![Span::styled("Act: ", t.bar)];
        for (n, (num, level)) in active.into_iter().enumerate() {
            if n > 0 {
                parts.push(Span::styled(",", t.bar));
            }
            let style = match level {
                Activity::Highlight => t.bar.patch(t.highlight),
                Activity::Message => t.bar.patch(t.activity),
                _ => t.bar_accent,
            };
            parts.push(Span::styled(num.to_string(), style));
        }
        item(&mut spans, parts);
    }

    if !win.typing.is_empty() {
        item(&mut spans, vec![Span::styled(format!("typing: {}", win.typing.join(", ")), t.bar)]);
    }

    for (name, state) in &app.accounts {
        if state.away.is_some() {
            item(&mut spans, vec![Span::styled(format!("{name}: away"), t.bar_alert)]);
        }
        if state.verified == Some(false) {
            item(&mut spans, vec![Span::styled(format!("{name}: unverified"), t.bar_alert)]);
        }
        let label = match &state.status {
            ConnStatus::Online => continue,
            ConnStatus::Connecting => "connecting",
            ConnStatus::Syncing => "syncing",
            ConnStatus::Lagging => "lag",
            ConnStatus::Offline => "offline",
        };
        item(&mut spans, vec![Span::styled(format!("{name}: {label}"), t.bar_alert)]);
    }

    if win.scroll > 0 {
        let style = if win.unseen_below { t.bar.add_modifier(Modifier::BOLD) } else { t.bar };
        item(&mut spans, vec![Span::styled("more", style)]);
    }

    if let Some(lines) = app.confirm_paste {
        let prompt = format!("send {lines} pasted lines to {}? y/n", win.title());
        item(&mut spans, vec![Span::styled(prompt, t.bar_alert.add_modifier(Modifier::BOLD))]);
    } else if app.confirm_delete.is_some() {
        let what = app.delete_summary().unwrap_or_else(|| "this message".into());
        let prompt = format!("delete {what}? y/n");
        item(&mut spans, vec![Span::styled(prompt, t.bar_alert.add_modifier(Modifier::BOLD))]);
    } else if app.select.is_some() && win.is_mentions {
        item(&mut spans, vec![Span::styled("↑/↓ choose  Enter go to it  Esc", t.bar_alert)]);
    } else if app.select.is_some() {
        let keys = "r reply  e edit  d delete  + react  t thread  f forward  p pin  z zoom  s save  o open  v spoiler  c copy  i info  Esc";
        item(&mut spans, vec![Span::styled(keys, t.bar_alert)]);
    } else if app.is_editing() {
        let keys = "editing: ↑/↓ pick  Enter save  clear+Enter delete  Esc cancel";
        item(&mut spans, vec![Span::styled(keys, t.bar_alert)]);
    }

    let flashing = app.flash_until.is_some_and(|until| std::time::Instant::now() < until);
    let line = if flashing {
        TLine::from(spans).patch_style(Style::new().add_modifier(Modifier::REVERSED))
    } else {
        TLine::from(spans)
    };
    f.render_widget(
        Paragraph::new(line).style(if flashing { t.bar.add_modifier(Modifier::REVERSED) } else { t.bar }),
        area,
    );
}

fn draw_input(f: &mut Frame, area: Rect, app: &App) -> (Rect, u16, usize) {
    let t = &app.cfg.palette;
    let win = &app.windows[app.active];
    let label: String = match app.compose_label() {
        Some(label) => label.chars().take(32).collect(),
        None => win.title().chars().take(24).collect(),
    };
    let prompt = format!("[{label}] ");
    let prompt_w: usize = prompt.chars().map(|c| c.width().unwrap_or(0)).sum();
    let avail = (area.width as usize).saturating_sub(prompt_w).max(1);

    let chars = app.input.chars();
    let cursor_w = app.input.cursor_width();
    let mut skip_w = 0;
    let mut skip = 0;
    if cursor_w >= avail {
        let target = cursor_w + 1 - avail;
        while skip < chars.len() && skip_w < target {
            skip_w += display_width(chars[skip]);
            skip += 1;
        }
    }
    let mut text = String::new();
    let mut used = 0;
    for &c in &chars[skip..] {
        let w = display_width(c);
        if used + w > avail {
            break;
        }
        text.push(match c {
            '\n' => '↵',
            c if c.is_control() => ' ',
            c => c,
        });
        used += w;
    }

    let mut spans = vec![Span::styled(prompt, t.frame), Span::raw(text)];
    if app.input.cursor() == chars.len()
        && let Some(hint) = app.input_hint()
    {
        let room = avail.saturating_sub(used + 1);
        let hint: String = hint.chars().take(room).collect();
        spans.push(Span::styled(hint, t.dim));
    }
    f.render_widget(Paragraph::new(TLine::from(spans)), area);
    let x = area.x + (prompt_w + cursor_w - skip_w).min(area.width.saturating_sub(1) as usize) as u16;
    f.set_cursor_position(Position::new(x, area.y));
    (area, area.x + prompt_w as u16, skip)
}

/// Rows a line occupies at `width`, for keeping the view steady while
/// scrolled up.
pub fn line_height(line: &Line, width: u16, cfg: &Config) -> usize {
    if !cfg.show_joins && line.kind == LineKind::Membership {
        return 0;
    }
    render_line(line, width as usize, cfg).len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(rows: &[TLine]) -> Vec<String> {
        rows.iter().map(|r| r.spans.iter().map(|s| s.content.as_ref()).collect()).collect()
    }

    #[test]
    fn wraps_at_spaces_with_hanging_indent() {
        let cells: Vec<(char, Style)> = "ab: one two three".chars().map(|c| (c, Style::new())).collect();
        assert_eq!(text(&wrap(&cells, 10, 4)), vec!["ab: one", "    two", "    three"]);
    }

    #[test]
    fn newline_forces_break_and_long_words_split() {
        let cells: Vec<(char, Style)> = "x\nabcdefgh".chars().map(|c| (c, Style::new())).collect();
        assert_eq!(text(&wrap(&cells, 4, 1)), vec!["x", " abc", " def", " gh"]);
    }

    #[test]
    fn underlined_stretches_are_found_by_column() {
        let u = Style::new().add_modifier(Modifier::UNDERLINED);
        let row = TLine::from(vec![
            Span::raw("12:00 "),
            Span::styled("https://a.example/", u),
            Span::styled("x", u),
            Span::raw(" and "),
            Span::styled("日本", u),
        ]);
        assert_eq!(underlined(&row, 2), vec![(8, 27, "https://a.example/x".to_owned()), (32, 36, "日本".to_owned())]);
    }

    #[test]
    fn joins_and_leaves_fold_into_one_line() {
        let noise = |fold: Fold, id: &str| {
            let mut line = Line::new(LineKind::Membership, "x");
            (line.fold, line.event_id) = (Some(fold), Some(id.try_into().unwrap()));
            line
        };
        let lines = [
            noise(Fold::Joined("alice".into()), "$1"),
            noise(Fold::Joined("bob".into()), "$2"),
            noise(Fold::Left("dave".into()), "$3"),
            noise(Fold::Renamed { from: "eve".into(), to: "erin".into() }, "$4"),
            noise(Fold::Joined("alice".into()), "$5"),
            Line::new(LineKind::Message, "hello"),
            noise(Fold::Left("bob".into()), "$6"),
        ];
        let folded = fold_noise(lines.iter().collect());
        assert_eq!(folded.len(), 3, "the run, the message, and the lone leave");
        assert_eq!(folded[0].body, "alice and bob joined · dave left · eve → erin");
        assert_eq!(folded[0].event_id.as_ref().map(|e| e.as_str()), Some("$5"), "the run ends at its last event");
        assert!(matches!(folded[2], Cow::Borrowed(_)), "a single change stays as it was");
        let names = ["a", "b", "c", "d", "e"];
        assert_eq!(name_list(&names[..3]), "a, b and c");
        assert_eq!(name_list(&names), "a, b and 3 others");
    }

    #[test]
    fn control_characters_are_dropped() {
        assert_eq!(sanitize('\u{1b}'), None);
        assert_eq!(sanitize('\u{202e}'), None);
        assert_eq!(sanitize('\u{fe0f}'), None, "pencil + VS16 must not overflow the row");
        assert_eq!(sanitize('é'), Some('é'));
    }
}
