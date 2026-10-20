//! irssi-style layout: topic bar, scrollback, status bar, input line.

use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap, HashSet},
};

use chrono::{DateTime, Local};
use matrix_sdk::{
    notification_settings::RoomNotificationMode,
    ruma::{EventId, OwnedEventId, OwnedUserId, UserId},
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect, Size},
    style::{Color, Modifier, Style},
    text::{Line as TLine, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use unicode_width::UnicodeWidthChar;

use crate::{
    app::{App, ConnStatus, HelpRow, LinkTitle, Preview, help_rows},
    config::{Config, LinkTitles},
    input::display_width,
    links,
    model::{Activity, Fold, Line, LineKind, Poll, PresenceKind},
    richtext::Marks,
    switcher,
    theme::{Outlines, Theme},
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
    /// The theme picker's box, and its rows: (row, theme index).
    pub theme_picker: Option<(Rect, Vec<(Rect, usize)>)>,
    /// The profile pane.
    pub profile: Option<Rect>,
    /// Split panes as drawn, in order: (their whole area, their place in
    /// the split's panes).
    pub splits: Vec<(Rect, usize)>,
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
    /// The sender's nick on it, to click.
    pub nick: Option<NickHit>,
}

/// Where a nick is on a row: (from column, to column, who).
pub type NickHit = (u16, u16, OwnedUserId);

pub fn draw(f: &mut Frame, app: &mut App) {
    app.hits = Hits::default();
    if app.cfg.palette.base != Style::new() {
        f.render_widget(Block::new().style(app.cfg.palette.base), f.area());
    }
    let bars = if app.cfg.statusbar2.is_empty() { 1 } else { 2 };
    let [topic, middle, status, input] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(bars), Constraint::Length(1)])
            .areas(f.area());

    // Side panes give way on narrow terminals.
    let total = middle.width;
    let side_w = if app.show_sidebar && total >= 60 { sidebar_width(app) } else { 0 };
    let win = &app.windows[app.active];
    let wants_nicks = app.show_nicklist && win.room.is_some() && !win.is_dm && win.members.len() > 2;
    // The profile pane goes beside the member list, which stays to pick
    // from; on narrow screens the list gives way first.
    let profile_w = if app.profile.is_some() { 36.min(total.saturating_sub(side_w) / 2) } else { 0 };
    // With the pane open, the messages make do with 50 columns to keep
    // the list beside it.
    let fits = match profile_w {
        0 => total >= side_w + 70,
        _ => total >= side_w + profile_w + nicklist_width(app) + 50,
    };
    let nick_w = if wants_nicks && fits { nicklist_width(app) } else { 0 };
    let [side, main, profile, nicks] = Layout::horizontal([
        Constraint::Length(side_w),
        Constraint::Min(20),
        Constraint::Length(profile_w),
        Constraint::Length(nick_w),
    ])
    .areas(middle);

    draw_topic(f, topic, app);
    if side_w > 0 {
        app.hits.sidebar = draw_sidebar(f, side, app);
    }
    draw_panes(f, main, app);
    if nick_w > 0 {
        app.hits.nicklist = draw_nicklist(f, nicks, app);
    }
    if profile_w > 0 {
        app.hits.profile = Some(profile);
        draw_profile(f, profile, app);
    }
    draw_status(f, status, app);
    app.hits.input = Some(draw_input(f, input, app));
    if app.help.is_some() {
        draw_help(f, app);
    }
    if app.switcher.is_some() {
        app.hits.switcher = Some(draw_switcher(f, app));
    }
    app.hits.theme_picker = app.theme_picker.is_some().then(|| draw_theme_picker(f, app));
    if app.vote.is_some() {
        draw_vote(f, app);
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
            f.render_widget(protocol, Rect { x, y, width: size.width, height: size.height });
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
        .style(t.base)
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

/// `/themes`: a column at the right, so most of the screen it recolours
/// stays in view; themes under their shelves, and what the chosen one is
/// at the bottom.
fn draw_theme_picker(f: &mut Frame, app: &mut App) -> (Rect, Vec<(Rect, usize)>) {
    let t = app.cfg.palette.clone();
    let screen = f.area();
    let Some(p) = app.theme_picker.as_ref() else { return (Rect::default(), Vec::new()) };
    let shown = p.shown();
    // Each shelf gets a heading: (theme index, or None for a heading).
    let mut rows: Vec<(Option<usize>, &str)> = Vec::new();
    for (n, entry) in shown.iter().enumerate() {
        if n == 0 || shown[n - 1].shelf != entry.shelf {
            rows.push((None, entry.shelf));
        }
        rows.push((Some(n), entry.id.name()));
    }
    let width = screen.width.saturating_sub(2).min(34);
    // In a room, a row says who it's for: everywhere, or just the room.
    let scope = p.room.as_ref().map(|room| {
        if p.just_here {
            TLine::from(vec![Span::styled(format!("just {room}"), t.accent), Span::styled(" · ^T all", t.dim)])
        } else {
            TLine::from(vec![Span::styled("everywhere", t.accent), Span::styled(" · ^T just here", t.dim)])
        }
    });
    let extra = u16::from(scope.is_some());
    let height = (rows.len().max(1) as u16 + 4 + extra).min(screen.height.saturating_sub(2));
    let area = Rect { x: screen.right().saturating_sub(width), y: screen.y + 1, width, height };
    let block = Block::bordered()
        .style(t.base)
        .border_style(t.frame)
        .title(Span::styled(" Themes ", t.accent))
        .title_bottom(TLine::from(" ↑↓ try · Enter keep · Esc undo ").right_aligned());
    let inner = block.inner(area);
    f.render_widget(Clear, area);
    f.render_widget(block, area);

    let query: String = p.query.chars().filter(|c| !c.is_control()).collect();
    let prompt = TLine::from(vec![Span::styled("› ", t.accent), Span::raw(query.clone())]);
    f.render_widget(Paragraph::new(prompt), Rect { height: 1, ..inner });
    let cursor_x = (inner.x + 2 + display_len(&query) as u16).min(inner.right().saturating_sub(1));
    f.set_cursor_position(Position::new(cursor_x, inner.y));

    // What the chosen theme is (or why it can't be shown), on the last row.
    let about = match (&p.error, shown.get(p.selected)) {
        (Some(error), _) => Span::styled(error.clone(), t.error),
        (None, Some(entry)) => Span::styled(entry.about.clone(), t.dim),
        (None, None) => Span::styled("nothing matches", t.dim),
    };
    let bottom = Rect { y: inner.bottom().saturating_sub(1), height: 1, ..inner };
    f.render_widget(Paragraph::new(about), bottom);

    if let Some(scope) = scope {
        f.render_widget(Paragraph::new(scope), Rect { y: inner.y + 1, height: 1, ..inner });
    }
    let list = Rect { y: inner.y + 1 + extra, height: inner.height.saturating_sub(2 + extra), ..inner };
    let at = rows.iter().position(|(n, _)| *n == Some(p.selected)).unwrap_or(0);
    let start = (at + 1).saturating_sub(list.height as usize);
    let mut hits = Vec::new();
    for (row, &(n, name)) in rows.iter().skip(start).take(list.height as usize).enumerate() {
        let rect = Rect { y: list.y + row as u16, height: 1, ..list };
        let Some(n) = n else {
            let shelf = format!("{}{}", name[..1].to_uppercase(), &name[1..]);
            f.render_widget(Paragraph::new(Span::styled(shelf, t.dim.add_modifier(Modifier::BOLD))), rect);
            continue;
        };
        let mark = if shown[n].id == *p.had() { "•" } else { " " };
        let text = format!(" {mark} {name}");
        let style = if n == p.selected { Style::new().add_modifier(Modifier::REVERSED) } else { t.inactive };
        let pad = (list.width as usize).saturating_sub(display_len(&text));
        f.render_widget(Paragraph::new(Span::styled(format!("{text}{}", " ".repeat(pad)), style)), rect);
        hits.push((rect, n));
    }
    (area, hits)
}

/// Voting: the poll's question, and its answers to pick from.
fn draw_vote(f: &mut Frame, app: &App) {
    let t = &app.cfg.palette;
    let Some(v) = app.vote.as_ref() else { return };
    let screen = f.area();
    let width = screen.width.saturating_sub(4).min(64);
    let height = (v.answers.len() as u16 + 4).min(screen.height.saturating_sub(2));
    let area = Rect { x: screen.x + (screen.width - width) / 2, y: screen.y + 2, width, height };
    let keys = if v.max > 1 {
        format!(" ↑↓ choose · Space ticks (up to {}) · Enter votes · Esc ", v.max)
    } else {
        " ↑↓ choose · Enter votes · Esc ".to_owned()
    };
    let block = Block::bordered()
        .style(t.base)
        .border_style(t.frame)
        .title(Span::styled(" Vote ", t.accent))
        .title_bottom(TLine::from(keys).right_aligned());
    let inner = block.inner(area);
    f.render_widget(Clear, area);
    f.render_widget(block, area);
    let width = inner.width as usize;
    f.render_widget(Paragraph::new(Span::styled(cut(&v.question, width), t.accent)), Rect { height: 1, ..inner });
    for (n, (_, text)) in v.answers.iter().enumerate() {
        let y = inner.y + 2 + n as u16;
        if y >= inner.bottom() {
            break;
        }
        let tick = match (v.max > 1, v.picked.contains(&n)) {
            (false, _) => "",
            (true, true) => "[x] ",
            (true, false) => "[ ] ",
        };
        let row = cut(&format!(" {tick}{}. {text}", n + 1), width);
        let pad = width.saturating_sub(display_len(&row));
        let style = if n == v.selected { Style::new().add_modifier(Modifier::REVERSED) } else { Style::new() };
        f.render_widget(
            Paragraph::new(Span::styled(format!("{row}{}", " ".repeat(pad)), style)),
            Rect { y, height: 1, ..inner },
        );
    }
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
        .style(t.base)
        .border_style(t.frame)
        .title(Span::styled(" Textrix help ", t.accent))
        .title_bottom(TLine::from(format!("{filter}· ↑↓ PgUp/PgDn scroll · Esc closes ")).right_aligned());
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(lines).block(block).scroll((help.scroll as u16, 0)), area);
}

/// A titled frame, when the theme draws outlines.
fn outline(t: &Theme, title: &str, focused: bool) -> Option<Block<'static>> {
    let lines = t.outlines.set()?;
    let style = if focused { t.border_focus } else { t.border };
    let title: String = title.chars().filter(|c| !c.is_control()).collect();
    Some(Block::bordered().border_set(lines).border_style(style).title(Span::styled(format!(" {title} "), style)))
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
    let frame = if app.cfg.palette.outlines.framed() { 1 } else { 0 };
    (longest.clamp(8, 22) + 9 + avatar + frame) as u16
}

/// irssi adv_windowlist-style: window number, name and unread count,
/// coloured by activity, grouped by server when there are several.
/// One row of the window list.
#[derive(Debug, PartialEq, Eq)]
pub enum SidebarEntry {
    Header(String),
    Window(usize),
}

/// Window list layout: the status window; then Unreads (any room or DM
/// with messages you haven't seen, mentions first), which go back where
/// they belong once read; then each server's Rooms (grouped under their
/// spaces when there are any) and DMs, by name. Window numbers never
/// change, only where they're listed.
pub fn sidebar_entries(app: &App) -> Vec<SidebarEntry> {
    // The status window, then other windows that aren't a room (mentions).
    let mut out = vec![SidebarEntry::Window(0)];
    out.extend((1..app.windows.len()).filter(|&i| app.windows[i].room.is_none()).map(SidebarEntry::Window));
    let rooms = |i: &usize| app.windows[*i].room.is_some();
    let unread = |i: &usize| app.windows[*i].activity >= Activity::Message;
    let by_name = |a: &usize, b: &usize| {
        let name = |i: &usize| app.windows[*i].title().to_lowercase();
        name(a).cmp(&name(b)).then(a.cmp(b))
    };

    let mut unreads: Vec<usize> = (1..app.windows.len()).filter(rooms).filter(unread).collect();
    unreads.sort_by(|a, b| app.windows[*b].activity.cmp(&app.windows[*a].activity).then(by_name(a, b)));
    if !unreads.is_empty() {
        out.push(SidebarEntry::Header("Unreads".into()));
        out.extend(unreads.into_iter().map(SidebarEntry::Window));
    }

    let mut servers: Vec<&str> = Vec::new();
    for w in &app.windows[1..] {
        if let Some(server) = w.account()
            && !servers.contains(&server)
        {
            servers.push(server);
        }
    }
    let several = app.accounts.len() > 1 || servers.len() > 1;
    for server in servers {
        let windows: Vec<usize> = (1..app.windows.len())
            .filter(|&i| app.windows[i].account() == Some(server))
            .filter(|i| !unread(i))
            .collect();
        let (mut dms, mut rooms): (Vec<usize>, Vec<usize>) = windows.into_iter().partition(|&i| app.windows[i].is_dm);
        rooms.sort_by(by_name);
        dms.sort_by(by_name);
        let label = |section: &str| if several { format!("{section} · {server}") } else { section.to_owned() };

        if !rooms.is_empty() {
            out.push(SidebarEntry::Header(label("Rooms")));
            let spaces = app.spaces.get(server).map(Vec::as_slice).unwrap_or_default();
            let mut placed: HashSet<usize> = HashSet::new();
            for space in spaces {
                let members: Vec<usize> = rooms
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
            let rest: Vec<usize> = rooms.into_iter().filter(|i| !placed.contains(i)).collect();
            if !rest.is_empty() && !placed.is_empty() {
                out.push(SidebarEntry::Header("◇ other".into()));
            }
            out.extend(rest.into_iter().map(SidebarEntry::Window));
        }
        if !dms.is_empty() {
            out.push(SidebarEntry::Header(label("DMs")));
            out.extend(dms.into_iter().map(SidebarEntry::Window));
        }
    }
    out
}

/// irssi adv_windowlist-style: window number, name and unread count,
/// coloured by activity, grouped by server and space.
fn draw_sidebar(f: &mut Frame, area: Rect, app: &App) -> Vec<(Rect, usize)> {
    let t = &app.cfg.palette;
    let framed = outline(t, "windows", false);
    let body = match &framed {
        Some(block) => block.inner(area),
        None => Rect { width: area.width.saturating_sub(1), ..area },
    };
    let inner = body.width as usize;
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
    let height = body.height as usize;
    let active_row = rows.iter().position(|(_, a)| *a).unwrap_or(0);
    let start = (active_row + 1).saturating_sub(height);
    let shown: Vec<TLine> = rows.into_iter().skip(start).take(height).map(|(r, _)| r).collect();
    match framed {
        Some(block) => {
            f.render_widget(block, area);
            f.render_widget(Paragraph::new(shown), body);
        }
        None => {
            let block = Block::new().borders(Borders::RIGHT).border_style(t.dim);
            f.render_widget(Paragraph::new(shown).block(block), area);
        }
    }

    // Avatar images go into the gap left for them after the window number.
    let mut hits = Vec::new();
    for (row, entry) in entries.iter().skip(start).take(height).enumerate() {
        let SidebarEntry::Window(i) = entry else { continue };
        hits.push((Rect { y: body.y + row as u16, height: 1, ..body }, *i));
        if let Some(Preview::Ready(protocol)) = avatar_of(app, *i) {
            let rect = Rect { x: body.x + 3, y: body.y + row as u16, width: 2, height: 1 };
            f.render_widget(protocol, rect);
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
    let frame = if app.cfg.palette.outlines.framed() { 1 } else { 0 };
    (longest.clamp(6, 18) + 5 + frame) as u16
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
    // Framed, the count goes in the title instead of a header row.
    let framed = outline(t, &format!("users ({})", win.members.len()), false);
    let body = match &framed {
        Some(block) => block.inner(area),
        None => Rect { x: area.x + 1, width: area.width.saturating_sub(1), ..area },
    };
    let header = usize::from(framed.is_none());
    let inner = body.width as usize;
    let height = body.height as usize;
    let mut rows = Vec::new();
    if header == 1 {
        rows.push(TLine::from(Span::styled(format!("Users ({})", win.members.len()), t.dim)));
    }
    let room = height.saturating_sub(header);
    let overflow = win.members.len() > room;
    let take = if overflow { room.saturating_sub(1) } else { room };
    // The one the profile pane shows, kept in view.
    let chosen = app.profile.as_ref().and_then(|p| p.member).filter(|n| *n < win.members.len());
    let start = chosen.map_or(0, |n| (n + 1).saturating_sub(take));
    for (n, m) in win.members.iter().enumerate().skip(start).take(take) {
        let prefix_style = match m.prefix {
            '@' => t.op,
            '+' => t.voice,
            _ => Style::new(),
        };
        let name: String = app
            .display_name(Some(&m.user_id), &m.name)
            .chars()
            .filter(|c| !c.is_control())
            .take(inner.saturating_sub(3))
            .collect();
        let (symbol, color) = match win.account().and_then(|a| app.presence_of(a, &m.user_id)) {
            Some(p) => presence_dot(t, p.kind),
            None => (' ', Color::Reset),
        };
        let row = TLine::from(vec![
            Span::styled(m.prefix.to_string(), prefix_style),
            Span::styled(format!("{symbol} "), Style::new().fg(color)),
            Span::styled(name, Style::new().fg(t.nick_color(m.user_id.as_str()))),
        ]);
        rows.push(if chosen == Some(n) { row.patch_style(Style::new().add_modifier(Modifier::REVERSED)) } else { row });
    }
    if overflow {
        let more = win.members.len().saturating_sub(start + take);
        rows.push(TLine::from(Span::styled(format!("… {more} more"), t.dim)));
    }
    match framed {
        Some(block) => {
            f.render_widget(block, area);
            f.render_widget(Paragraph::new(rows), body);
        }
        None => {
            let block = Block::new().borders(Borders::LEFT).border_style(t.dim);
            f.render_widget(Paragraph::new(rows).block(block), area);
        }
    }
    (0..take.min(win.members.len().saturating_sub(start)))
        .map(|n| (Rect { y: body.y + (header + n) as u16, height: 1, ..body }, start + n))
        .collect()
}

/// The profile pane: someone's picture, names, presence, place in the
/// room and devices, and what you can do from here.
fn draw_profile(f: &mut Frame, area: Rect, app: &App) {
    let t = &app.cfg.palette;
    let Some(p) = app.profile.as_ref() else { return };
    let block = outline(t, "profile", true).unwrap_or_else(|| Block::new().borders(Borders::LEFT).border_style(t.dim));
    let block = block.style(t.base);
    let inner = block.inner(area);
    f.render_widget(Clear, area);
    f.render_widget(block, area);
    let body = Rect { x: inner.x + 1, width: inner.width.saturating_sub(1), ..inner };
    let width = body.width as usize;
    let mut y = body.y;
    if let Some(picture) = &p.picture {
        let size = picture.size();
        let rect = Rect { x: body.x, y, width: size.width.min(body.width), height: size.height.min(body.height) };
        f.render_widget(picture, rect);
        y += rect.height + 1;
    }

    let win = &app.windows[app.active];
    let member = win.members.iter().find(|m| m.user_id == p.user);
    let shown = app.display_name(Some(&p.user), &p.name).to_owned();
    let theirs = p.info.display.clone().unwrap_or_else(|| p.name.clone());
    let mut rows: Vec<TLine> = vec![
        TLine::from(Span::styled(
            cut(&shown, width),
            Style::new().fg(t.nick_color(p.user.as_str())).add_modifier(Modifier::BOLD),
        )),
        TLine::from(Span::styled(cut(p.user.as_str(), width), t.dim)),
    ];
    if shown != theirs {
        rows.push(TLine::from(Span::styled(cut(&format!("(they're {theirs})"), width), t.dim)));
    }
    rows.push(TLine::default());
    if let Some(presence) = app.presence_of(&p.account, &p.user) {
        let (dot, color) = presence_dot(t, presence.kind);
        rows.push(TLine::from(vec![
            Span::styled(format!("{dot} "), Style::new().fg(color)),
            Span::raw(cut(&presence.describe(), width.saturating_sub(2))),
        ]));
    }
    let role = match member.map(|m| m.prefix) {
        Some('@') => "admin here",
        Some('+') => "moderator here",
        Some(_) => "member here",
        None if win.room.is_some() => "not in this room",
        None => "",
    };
    if !role.is_empty() {
        rows.push(TLine::from(Span::raw(role)));
    }
    rows.push(TLine::default());
    if !p.info.fetched {
        rows.push(TLine::from(Span::styled("asking their server…", t.dim)));
    } else {
        let (identity, style) = match p.info.identity {
            Some(true) => ("✓ verified by you", t.accent),
            Some(false) => ("identity not verified", t.highlight_text),
            None => ("no cross-signing identity", t.dim),
        };
        rows.push(TLine::from(Span::styled(identity, style)));
        let verified = p.info.devices.iter().filter(|d| d.2).count();
        let devices = match p.info.devices.len() {
            0 => "no devices known".to_owned(),
            1 => format!("1 device, {verified} verified"),
            n => format!("{n} devices, {verified} verified"),
        };
        rows.push(TLine::from(Span::styled(devices, t.dim)));
        for (id, name, ok) in &p.info.devices {
            let label = if name.is_empty() { id.clone() } else { format!("{name} ({id})") };
            let mark = if *ok { Span::styled("✓ ", t.accent) } else { Span::raw("  ") };
            rows.push(TLine::from(vec![mark, Span::raw(cut(&label, width.saturating_sub(2)))]));
        }
    }

    let mut keys = vec!["m message  @ mention", "n nickname  i ignore", "v verify  Esc close"];
    if p.member.is_some() {
        keys.insert(0, "↑/↓ the next member");
    }
    let bottom = body.bottom().saturating_sub(keys.len() as u16);
    let text = Rect { y, height: bottom.saturating_sub(y + 1), ..body };
    f.render_widget(Paragraph::new(rows), text);
    for (n, k) in keys.iter().enumerate() {
        let row = Rect { y: bottom + n as u16, height: 1, ..body };
        f.render_widget(Paragraph::new(Span::styled(cut(k, width), t.dim)), row);
    }
}

/// Who's typing, by your nicknames where you've given them.
fn typing_names(app: &App, typing: &[(OwnedUserId, String)]) -> String {
    typing.iter().map(|(user, nick)| app.display_name(Some(user), nick)).collect::<Vec<_>>().join(", ")
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
    // With outlines, each pane is framed with its name in the frame.
    let frame_of = |app: &App, idx: usize| {
        let w = &app.windows[idx];
        let lock = if w.encrypted { " (+E)" } else { "" };
        outline(&t, &format!("{}:{}{lock}", idx + 1, w.title()), idx == app.active)
    };
    if panes.len() < 2 {
        let idx = app.active;
        let body = match frame_of(app, idx) {
            Some(block) => {
                let body = block.inner(area);
                f.render_widget(block, area);
                body
            }
            None => area,
        };
        (app.view_width, app.view_height, app.windows[idx].width) = (body.width, body.height, body.width);
        return draw_messages(f, body, app, idx);
    }
    let side_by_side = app.split.as_ref().is_some_and(|s| s.side_by_side);
    // Each drawn pane's place in the split, and its share of the room.
    let slots: Vec<usize> = panes
        .iter()
        .map(|&idx| {
            app.split.as_ref().and_then(|s| s.panes.iter().position(|r| *r == app.windows[idx].id())).unwrap_or(0)
        })
        .collect();
    let weights = app.split.as_ref().map(|s| s.weights()).unwrap_or_default();
    let fill: Vec<Constraint> =
        slots.iter().map(|&slot| Constraint::Fill(weights.get(slot).copied().unwrap_or(1).max(1))).collect();
    let areas = if side_by_side { Layout::horizontal(fill).split(area) } else { Layout::vertical(fill).split(area) };
    app.hits.splits = areas.iter().copied().zip(slots).collect();
    for (n, (&idx, &pane)) in panes.iter().zip(areas.iter()).enumerate() {
        let body = if let Some(block) = frame_of(app, idx) {
            let body = block.inner(pane);
            f.render_widget(block, pane);
            body
        } else {
            let mut pane = pane;
            if side_by_side && n > 0 {
                let divider = Block::new().borders(Borders::LEFT).border_style(t.dim);
                f.render_widget(&divider, pane);
                pane = divider.inner(pane);
            }
            let [body, bar] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(pane);
            draw_pane_bar(f, bar, app, idx);
            body
        };
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
        text.push_str(&format!("  typing: {}", typing_names(app, &w.typing)));
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
    // The mentions window's copies were named when they were made.
    let nicknames = if app.windows[idx].is_mentions { BTreeMap::new() } else { app.nicknames().clone() };
    let account = app.windows[idx].room.as_ref().map(|k| k.account.clone());
    let images = app.picker.is_some();
    let (previews, requests) = (&mut app.previews, &mut app.preview_requests);
    let (titles, title_requests) = (&mut app.link_titles, &mut app.link_title_requests);
    let cfg = &app.cfg;
    let win = &mut app.windows[idx];
    // Images and link titles sit under their message, where its text
    // starts (past the nick column's bar, when there is one).
    let under = gutter(cfg, width);
    let image_x = under.0 as u16;
    // Textrix's own lines in a room, set apart from the conversation: a
    // blank row before and after each run of them, and quieter.
    let set_apart = win.room.is_some();
    let own = |line: &Line| matches!(line.kind, LineKind::Info | LineKind::Error);
    // A server preview of a link in an encrypted room would show it the link.
    let titles_here = match cfg.link_titles {
        LinkTitles::Off => false,
        LinkTitles::Unencrypted => !win.encrypted && win.room.is_some(),
        LinkTitles::All => win.room.is_some(),
    };
    let mut placements: Vec<(OwnedEventId, usize, u16)> = Vec::new();
    let shown_lines: Vec<&Line> =
        win.lines.iter().filter(|l| cfg.show_joins || l.kind != LineKind::Membership).collect();
    let visible =
        if cfg.fold_joins { fold_noise(shown_lines) } else { shown_lines.into_iter().map(Cow::Borrowed).collect() };

    // Who has read up to which event (not shown on their own messages).
    let mut readers: HashMap<&EventId, Vec<(&UserId, &str)>> = HashMap::new();
    for (user, (nick, event_id)) in &win.read_by {
        let name = nicknames.get(user).map_or(nick.as_str(), String::as_str);
        readers.entry(&**event_id).or_default().push((&**user, name));
    }

    // The "new messages" divider only matters if someone else said something.
    let last_from_others = visible.iter().rposition(|l| l.is_message() && !l.own && l.event_id.is_some());

    let mut need = height + win.scroll;
    let mut rows: Vec<TLine<'static>> = Vec::new();
    // The message on each row, and whether it's part of an image, for
    // mouse clicks.
    let mut row_events: Vec<(Option<OwnedEventId>, bool, Option<NickHit>)> = Vec::new();
    let mut found_selected = selected.is_none();
    // Whether the last row added was a divider.
    let mut divided = false;
    let mut i = visible.len();
    while i > 0 && (rows.len() < need || !found_selected) {
        i -= 1;
        let line: &Line = &visible[i];
        // Where Textrix's own lines meet the conversation (and there's no
        // divider there already), a blank row.
        if set_apart
            && visible.get(i + 1).is_some_and(|next| own(next) != own(line))
            && rows.last().is_none_or(|row| row.width() > 0 && !divided)
        {
            rows.push(TLine::default());
            row_events.push((None, false, None));
        }
        divided = false;
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
            && let (Some(media), Some(id)) = (line.media.as_ref().filter(|m| m.has_picture()), &line.event_id)
        {
            match previews.get(id) {
                Some(Preview::Ready(protocol)) => {
                    let height = protocol.size().height;
                    placements.push((id.clone(), rows.len(), height));
                    rows.extend((0..height).map(|_| TLine::from(under.1.clone())));
                    row_events.extend((0..height).map(|_| (Some(id.clone()), true, None)));
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
        // The page title of the message's first link, just under its text.
        if titles_here
            && line.is_message()
            && !line.redacted
            && let Some(url) = links::urls(&line.body).into_iter().find(|u| links::is_safe(u))
        {
            match titles.get(url) {
                Some(LinkTitle::Ready(title)) => {
                    rows.push(link_title_row(&cfg.palette, title, width, &under));
                    row_events.push((line.event_id.clone(), false, None));
                }
                Some(_) => {}
                None => {
                    if let Some(account) = &account {
                        titles.insert(url.to_owned(), LinkTitle::Loading);
                        title_requests.push((account.to_string(), url.to_owned()));
                    }
                }
            }
        }
        let below = rows.len();
        let pinned = line.event_id.as_ref().is_some_and(|id| win.pinned.contains(id));
        let (mut rendered, nick_at) =
            render_line_and_nick(line, width, cfg, &read, thread_window.as_deref(), pinned, &nicknames);
        if set_apart && line.kind == LineKind::Info {
            for span in rendered.iter_mut().flat_map(|row| row.spans.iter_mut()) {
                span.style = span.style.patch(cfg.palette.dim);
            }
        }
        if is_selected {
            for row in &mut rendered {
                *row = row.clone().patch_style(Style::new().add_modifier(Modifier::REVERSED));
            }
        }
        // Bottom up, like the rows: the nick is on the line's first.
        for k in (0..rendered.len()).rev() {
            let nick = nick_at
                .filter(|_| k == 0)
                .zip(line.sender.clone())
                .map(|((x, w), user)| (area.x + x as u16, area.x + (x + w) as u16, user));
            row_events.push((line.event_id.clone(), false, nick));
        }
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
            rows.push(day_changed(cfg, width, line.ts, set_apart));
            row_events.push((None, false, None));
            divided = true;
        }
        if i > 0
            && visible[i - 1].event_id.is_some()
            && visible[i - 1].event_id == win.marker
            && last_from_others.is_some_and(|j| j >= i)
        {
            rows.push(new_messages(&cfg.palette, width));
            row_events.push((None, false, None));
            divided = true;
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
            .chain(shown.iter().zip(&row_events[start..end]).map(|(row, (event, image, nick))| RowHit {
                event: event.clone(),
                links: underlined(row, area.x),
                image: *image,
                nick: nick.clone(),
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
        f.render_widget(protocol, rect);
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
    // Only joins, or only leaves, get their arrow in the nick column.
    line.fold = match (joined.is_empty(), left.is_empty(), renamed.is_empty()) {
        (false, true, true) => Some(Fold::Joined(String::new())),
        (true, false, true) => Some(Fold::Left(String::new())),
        _ => None,
    };
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

/// "↳ Page title — Site", dim, cut to fit, after `gutter` (as wide as
/// `indent`).
fn link_title_row(
    t: &Theme,
    title: &str,
    width: usize,
    (indent, gutter): &(usize, Vec<Span<'static>>),
) -> TLine<'static> {
    let avail = width.saturating_sub(indent + 3);
    let mut shown = String::new();
    let mut used = 0;
    for c in title.chars().filter_map(sanitize) {
        let w = c.width().unwrap_or(0);
        if used + w > avail {
            shown.push('…');
            break;
        }
        used += w;
        shown.push(c);
    }
    let mut spans = gutter.clone();
    spans.push(Span::styled(format!("↳ {shown}"), t.dim.add_modifier(Modifier::ITALIC)));
    TLine::from(spans)
}

/// How many characters of nick the nick column holds at this width (none
/// when it's off, or the pane is too narrow for it).
fn nick_column(cfg: &Config, width: usize) -> Option<usize> {
    let n = cfg.nick_column;
    let stamp = display_len(&Local::now().format(&cfg.timestamp_format).to_string());
    (n > 0 && width >= stamp + n + 6 + 20).then_some(n)
}

/// The nick column's separator.
fn column_bar(t: &Theme) -> &'static str {
    if t.outlines == Outlines::Ascii { "|" } else { "│" }
}

/// What starts the rows under a message's first: blank to where the text
/// starts, with the nick column's bar when there is one. Also how wide it
/// is, which is where pictures and link titles go.
pub fn gutter(cfg: &Config, width: usize) -> (usize, Vec<Span<'static>>) {
    let stamp = display_len(&Local::now().format(&cfg.timestamp_format).to_string());
    match nick_column(cfg, width) {
        Some(n) => {
            let pad = stamp + 1 + n + 3;
            let spans = vec![
                Span::raw(" ".repeat(pad)),
                Span::styled(format!("{} ", column_bar(&cfg.palette)), cfg.palette.border),
            ];
            (pad + 2, spans)
        }
        None => (stamp + 1, vec![Span::raw(" ".repeat(stamp + 1))]),
    }
}

/// `name`, cut to `max` columns with "… " if it's longer: the space keeps
/// what follows clear of fonts whose "…" spills out of its cell.
fn cut(name: &str, max: usize) -> String {
    if display_len(name) <= max {
        return name.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in name.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 2 > max {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push_str("… ");
    out
}

fn new_messages(t: &Theme, width: usize) -> TLine<'static> {
    let label = " new messages ";
    let side = width.saturating_sub(label.len()) / 2;
    let rule = "─".repeat(side);
    TLine::from(Span::styled(format!("{rule}{label}{rule}"), t.divider))
}

/// "-!- Day changed to …", laid out like Textrix's other lines (in the
/// nick column, when there is one) with no time of its own; `quiet` in
/// rooms, as they are, and in italic like them.
fn day_changed(cfg: &Config, width: usize, ts: DateTime<Local>, quiet: bool) -> TLine<'static> {
    let t = &cfg.palette;
    let stamp = display_len(&ts.format(&cfg.timestamp_format).to_string());
    let mut spans = vec![Span::raw(" ".repeat(stamp + 1)), Span::styled("-!-", t.accent)];
    match nick_column(cfg, width) {
        Some(n) => {
            spans.push(Span::raw(" ".repeat(n)));
            spans.push(Span::styled(format!("{} ", column_bar(t)), t.border));
        }
        None => spans.push(Span::raw(" ")),
    }
    spans.push(Span::raw(format!("Day changed to {}", ts.format("%a %d %b %Y"))));
    let italic = if cfg.system_italic { Style::new().add_modifier(Modifier::ITALIC) } else { Style::new() };
    let style = if quiet { t.dim.patch(italic) } else { italic };
    for span in spans.iter_mut().skip(1) {
        span.style = span.style.patch(style);
    }
    TLine::from(spans)
}

/// Format one scrollback line and wrap it to `width`, indenting
/// continuation rows under the message text.
pub fn render_line(line: &Line, width: usize, cfg: &Config) -> Vec<TLine<'static>> {
    render_line_with(line, width, cfg, &[], None, false, &BTreeMap::new())
}

/// A line as text, with your nicknames (for tests).
#[cfg(test)]
pub fn render_line_with_names(
    line: &Line,
    width: usize,
    cfg: &Config,
    names: &BTreeMap<OwnedUserId, String>,
) -> String {
    let rows = render_line_with(line, width, cfg, &[], None, false, names);
    rows.iter().map(|r| r.spans.iter().map(|s| s.content.as_ref()).collect::<String>()).collect::<Vec<_>>().join("\n")
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
    nicknames: &BTreeMap<OwnedUserId, String>,
) -> Vec<TLine<'static>> {
    render_line_and_nick(line, width, cfg, read_by, thread_window, pinned, nicknames).0
}

/// `render_line_with`, and where on the first row the sender's nick is:
/// (column, width), for clicking.
fn render_line_and_nick(
    line: &Line,
    width: usize,
    cfg: &Config,
    read_by: &[&str],
    thread_window: Option<&EventId>,
    pinned: bool,
    nicknames: &BTreeMap<OwnedUserId, String>,
) -> (Vec<TLine<'static>>, Option<(usize, usize)>) {
    let name_for = |user: Option<&OwnedUserId>, fallback: &str| -> String {
        user.and_then(|u| nicknames.get(u)).map_or_else(|| fallback.to_owned(), Clone::clone)
    };
    let nick = name_for(line.sender.as_ref(), &line.nick);
    let t = &cfg.palette;
    let mut cells: Vec<(char, Style)> = Vec::new();
    let push = |cells: &mut Vec<(char, Style)>, s: &str, style: Style| {
        cells.extend(s.chars().filter_map(|c| sanitize(c).map(|c| (c, style))));
    };

    push(&mut cells, &line.ts.format(&cfg.timestamp_format).to_string(), t.dim);
    push(&mut cells, " ", Style::new());
    let after_stamp = cells.len();

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

    let column = nick_column(cfg, width);
    let at = |cells: &Vec<(char, Style)>| cells.iter().map(|(c, _)| display_width(*c)).sum::<usize>();
    let mut nick_at = None;
    if let Some(n) = column {
        // <nick>, * nick, -nick-, -->, -!-… then spaces to the bar.
        let short = cut(&nick, n);
        let parts: Vec<(&str, Style)> = match line.kind {
            LineKind::Message | LineKind::Utd => vec![("<", t.dim), (&short, nick_style), (">", t.dim)],
            LineKind::Emote => vec![("* ", t.accent), (&short, nick_style)],
            LineKind::Notice => vec![("-", t.dim), (&short, nick_style), ("-", t.dim)],
            LineKind::Error => vec![("-!-", t.error.add_modifier(Modifier::BOLD))],
            LineKind::Membership => match line.fold {
                Some(Fold::Joined(_)) => vec![("-->", t.accent)],
                Some(Fold::Left(_)) => vec![("<--", t.dim)],
                _ => vec![("-!-", t.accent)],
            },
            LineKind::Event | LineKind::Info => vec![("-!-", t.accent)],
        };
        let used: usize = parts.iter().map(|(s, _)| display_len(s)).sum();
        for (text, style) in parts {
            if text == short && nick_at.is_none() {
                nick_at = Some((at(&cells), display_len(&short)));
            }
            push(&mut cells, text, style);
        }
        push(&mut cells, &" ".repeat((n + 2).saturating_sub(used) + 1), Style::new());
        push(&mut cells, &format!("{} ", column_bar(t)), t.border);
        if line.kind == LineKind::Emote {
            push(&mut cells, &nick, nick_style);
            push(&mut cells, " ", Style::new());
        }
    } else {
        match line.kind {
            LineKind::Message | LineKind::Utd => {
                push(&mut cells, "<", t.dim);
                nick_at = Some((at(&cells), display_len(&nick)));
                push(&mut cells, &nick, nick_style);
                push(&mut cells, "> ", t.dim);
            }
            LineKind::Emote => {
                push(&mut cells, " * ", t.accent);
                nick_at = Some((at(&cells), display_len(&nick)));
                push(&mut cells, &nick, nick_style);
                push(&mut cells, " ", Style::new());
            }
            LineKind::Notice => {
                push(&mut cells, "-", t.dim);
                nick_at = Some((at(&cells), display_len(&nick)));
                push(&mut cells, &nick, nick_style);
                push(&mut cells, "- ", t.dim);
            }
            LineKind::Error => push(&mut cells, "-!- ", t.error.add_modifier(Modifier::BOLD)),
            LineKind::Membership | LineKind::Event | LineKind::Info => push(&mut cells, "-!- ", t.accent),
        }
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
        let reply_nick = line.reply_nick.as_deref().map(|n| name_for(line.reply_sender.as_ref(), n));
        let to = match (&reply_nick, &line.reply_snippet) {
            (Some(nick), Some(text)) => format!("{mark} {nick} “{text}” "),
            (Some(nick), None) => format!("{mark} {nick}: "),
            _ => format!("{mark} "),
        };
        push(&mut cells, &to, t.dim);
    }

    let gutter = if column.is_some() {
        gutter(cfg, width).1
    } else {
        // Wrapped text lines up after the nick, unless that's half the row.
        let prefix_width: usize = cells.iter().map(|(c, _)| display_width(*c)).sum();
        let indent = if prefix_width > width / 2 { 4.min(width.saturating_sub(1)) } else { prefix_width };
        vec![Span::raw(" ".repeat(indent))]
    };

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
    if let Some(poll) = line.poll.as_deref().filter(|_| !line.redacted) {
        poll_rows(&mut cells, poll, t);
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

    // System messages in italic, all but the time.
    if cfg.system_italic && is_system(line) {
        for (_, style) in &mut cells[after_stamp..] {
            *style = style.add_modifier(Modifier::ITALIC);
        }
    }
    let nick_at = nick_at.filter(|_| line.sender.is_some());
    (wrap(&cells, width, &gutter), nick_at)
}

/// A poll's answers under its question, a row each: the results as bars
/// (when they show), ✓ by yours, and how many voted.
fn poll_rows(cells: &mut Vec<(char, Style)>, poll: &Poll, t: &Theme) {
    let mut push = |s: &str, style: Style| cells.extend(s.chars().filter_map(|c| sanitize(c).map(|c| (c, style))));
    let shown = poll.results_shown();
    let (counts, voters, mine) = (poll.counts(), poll.voters(), poll.mine());
    let wide = poll.answers.iter().map(|(_, a)| display_len(a)).max().unwrap_or(0).min(24);
    for (n, ((id, text), count)) in poll.answers.iter().zip(&counts).enumerate() {
        let text = cut(text, wide);
        push("\n", Style::new());
        push(&format!("{:>2}. ", n + 1), t.dim);
        push(&text, Style::new());
        push(&" ".repeat(wide.saturating_sub(display_len(&text)) + 1), Style::new());
        if shown {
            let filled = (count * 10 + voters / 2).checked_div(voters).unwrap_or(0).min(10);
            push(&"█".repeat(filled), t.accent);
            push(&"░".repeat(10 - filled), t.dim);
            push(&format!(" {count}"), t.dim);
        }
        if mine.contains(&id.as_str()) {
            push(" ✓", t.accent.add_modifier(Modifier::BOLD));
        }
    }
    let mut footer = if voters == 1 { "1 vote".to_owned() } else { format!("{voters} votes") };
    if poll.ended.is_some() {
        footer.push_str(" · ended");
    } else if !shown {
        footer.push_str(" · results when it ends");
    }
    push("\n", Style::new());
    push(&footer, t.dim);
}

/// A `-!-` line: Textrix's own, or what happened in the room.
fn is_system(line: &Line) -> bool {
    matches!(line.kind, LineKind::Info | LineKind::Error | LineKind::Event | LineKind::Membership)
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
        if let Some(kind) = marks.syntax {
            style = style.patch(t.syntax[kind as usize]);
        }
    }
    if marks.link {
        style = style.patch(t.link);
    }
    if marks.mention && !highlight {
        style = style.patch(t.mention);
    }
    if marks.emoji {
        style = style.patch(t.accent);
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

/// `cells` in rows of `width`, broken at spaces where it can be; the rows
/// after the first start with `gutter`.
fn wrap(cells: &[(char, Style)], width: usize, gutter: &[Span<'static>]) -> Vec<TLine<'static>> {
    let width = width.max(1);
    let indent: usize = gutter.iter().map(|s| display_len(&s.content)).sum();
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
            spans.extend(gutter.iter().cloned());
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

/// The status bar (`statusbar`), and the second one above it
/// (`statusbar2`) when there is one. Prompts always come at the end of
/// the first.
fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let t = &app.cfg.palette;
    let flashing = app.flash_until.is_some_and(|until| std::time::Instant::now() < until);
    let mut rows = Vec::new();
    if !app.cfg.statusbar2.is_empty() {
        rows.push(status_items(app, &app.cfg.statusbar2));
    }
    let mut main = status_items(app, &app.cfg.statusbar);
    if let Some(prompt) = status_prompt(app) {
        main.extend([Span::styled("[", t.bar_accent), prompt, Span::styled("] ", t.bar_accent)]);
    }
    rows.push(main);
    for (n, spans) in rows.into_iter().enumerate() {
        let row = Rect { y: area.y + n as u16, height: 1, ..area };
        let style = if flashing { t.bar.add_modifier(Modifier::REVERSED) } else { t.bar };
        let mut line = TLine::from(spans);
        if flashing {
            line = line.patch_style(Style::new().add_modifier(Modifier::REVERSED));
        }
        f.render_widget(Paragraph::new(line).style(style), row);
    }
}

/// `[a] [b] …`: the status bar `items`, those with something to show.
fn status_items(app: &App, items: &[String]) -> Vec<Span<'static>> {
    let t = &app.cfg.palette;
    let win = &app.windows[app.active];
    let mut spans: Vec<Span> = Vec::new();
    let item = |spans: &mut Vec<Span<'static>>, parts: Vec<Span<'static>>| {
        spans.push(Span::styled("[", t.bar_accent));
        spans.extend(parts);
        spans.push(Span::styled("] ", t.bar_accent));
    };
    for name in items {
        match name.as_str() {
            "time" => {
                let now = Local::now().format("%H:%M").to_string();
                // Elite o'clock.
                let clock = if now == "13:37" { "1337".to_owned() } else { now };
                item(&mut spans, vec![Span::styled(clock, t.bar)]);
            }
            "date" => item(&mut spans, vec![Span::styled(Local::now().format("%a %d %b").to_string(), t.bar)]),
            "nick" => {
                if let Some(state) = win.account().and_then(|a| app.accounts.get(a)) {
                    let nick = state.nick.clone().unwrap_or_else(|| state.user.clone());
                    item(&mut spans, vec![Span::styled(nick, t.bar)]);
                }
            }
            "window" => {
                let mut name = format!("{}:{}", app.active + 1, win.title());
                if win.encrypted {
                    name.push_str("(+E)");
                }
                item(&mut spans, vec![Span::styled(name, t.bar)]);
            }
            "users" if win.room.is_some() && !win.members.is_empty() => {
                item(&mut spans, vec![Span::styled(format!("{} users", win.members.len()), t.bar)]);
            }
            "activity" => {
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
            }
            "typing" if !win.typing.is_empty() => {
                item(&mut spans, vec![Span::styled(format!("typing: {}", typing_names(app, &win.typing)), t.bar)]);
            }
            "scripts" => {
                for text in app.scripts.status_items() {
                    item(&mut spans, vec![Span::styled(text, t.bar)]);
                }
            }
            "upgrade" => {
                if let Some(note) = app.upgrade_note() {
                    item(&mut spans, vec![Span::styled(note, t.bar_alert)]);
                }
            }
            "playing" if app.playing.is_some() => {
                item(&mut spans, vec![Span::styled("▶ playing", t.bar.add_modifier(Modifier::BOLD))]);
            }
            "servers" => {
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
            }
            "theme" => item(&mut spans, vec![Span::styled(app.theme_on_screen().to_string(), t.bar)]),
            "more" if win.scroll > 0 => {
                let style = if win.unseen_below { t.bar.add_modifier(Modifier::BOLD) } else { t.bar };
                item(&mut spans, vec![Span::styled("more", style)]);
            }
            _ => {}
        }
    }
    spans
}

/// What the status bar asks or explains right now (quitting, a paste, an
/// upload, a deletion, the keys in selection mode…).
fn status_prompt(app: &App) -> Option<Span<'static>> {
    let t = &app.cfg.palette;
    let win = &app.windows[app.active];
    let loud = t.bar_alert.add_modifier(Modifier::BOLD);
    let text = if let Some(prompt) = app.quit_prompt() {
        return Some(Span::styled(prompt, loud));
    } else if let Some(lines) = app.confirm_paste {
        format!("send {lines} pasted lines to {}? y/n", win.title())
    } else if let Some(what) = app.upload_summary() {
        format!("upload {what} to {}? Enter/y uploads, n keeps it as text", win.title())
    } else if app.confirm_delete.is_some() {
        let what = app.delete_summary().unwrap_or_else(|| "this message".into());
        format!("delete {what}? y/n")
    } else if app.select.is_some() && win.is_mentions {
        return Some(Span::styled("↑/↓ choose  Enter go to it  Esc", t.bar_alert));
    } else if app.select.is_some() {
        let keys = "r reply  e edit  d delete  + react  t thread  f forward  p pin  z zoom/play  s save  o open  v spoiler  c copy  i info  Esc";
        let keys = if app.selected_poll().is_some() { format!("Enter vote  {keys}") } else { keys.to_owned() };
        return Some(Span::styled(keys, t.bar_alert));
    } else if app.is_editing() {
        return Some(Span::styled("editing: ↑/↓ pick  Enter save  clear+Enter delete  Esc cancel", t.bar_alert));
    } else {
        return None;
    };
    Some(Span::styled(text, loud))
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
    fn system_messages_are_italic() {
        let italic = |line: &Line, cfg: &Config| {
            let rows = render_line(line, 80, cfg);
            let body: Vec<&Span> = rows[0].spans.iter().skip(1).filter(|s| !s.content.trim().is_empty()).collect();
            let time = &rows[0].spans[0];
            assert!(!time.style.add_modifier.contains(Modifier::ITALIC), "the time stays upright");
            body.iter().all(|s| s.style.add_modifier.contains(Modifier::ITALIC))
        };
        let cfg = Config::default();
        let mut said = Line::new(LineKind::Message, "hi");
        said.nick = "sam".into();
        let mut joined = Line::new(LineKind::Membership, "sam joined");
        joined.fold = Some(Fold::Joined("sam".into()));
        for line in
            [Line::info("theme: dracula"), Line::error("no such room"), joined, Line::new(LineKind::Event, "topic")]
        {
            assert!(italic(&line, &cfg), "{:?}", line.kind);
        }
        assert!(!italic(&said, &cfg), "messages aren't");
        let upright = Config { system_italic: false, ..Config::default() };
        assert!(!italic(&Line::info("theme: dracula"), &upright));
        let day = day_changed(&cfg, 80, said.ts, false);
        assert!(day.spans.last().unwrap().style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn nicks_line_up_in_a_column() {
        let cfg = Config::default();
        let said = |nick: &str, body: &str| {
            let mut line = Line::new(LineKind::Message, body);
            line.nick = nick.into();
            line
        };
        let mut join = Line::new(LineKind::Membership, "bbartek joined");
        join.fold = Some(Fold::Joined("bbartek".into()));
        let mut emote = Line::new(LineKind::Emote, "slaps jo around a bit");
        emote.nick = "sam".into();
        let lines = [
            said("nesbitt", "can anyone tell me how to get my resolv.conf file to have the right servers in it"),
            said("a_very_long_nickname", "hi"),
            join,
            emote,
            Line::info("theme: dracula"),
        ];
        let rows: Vec<String> = lines.iter().flat_map(|l| text(&render_line(l, 60, &cfg))).collect();
        let bar = |row: &String| row.chars().position(|c| c == '│');
        assert!(rows.iter().all(|r| bar(r) == Some(21)), "every row's bar in line: {rows:#?}");
        assert!(rows[0].ends_with("<nesbitt>      │ can anyone tell me how to get my"), "{}", rows[0]);
        assert!(rows[1].trim_start().starts_with('│'), "wrapped text keeps to its side of the bar");
        assert!(rows.iter().any(|r| r.contains("<a_very_lon… > │ hi")), "long nicks are cut short, clear of the >");
        assert!(rows.iter().any(|r| r.contains("-->            │ bbartek joined")));
        assert!(rows.iter().any(|r| r.contains("* sam          │ sam slaps jo")));
        assert!(rows.iter().any(|r| r.contains("-!-            │ theme: dracula")));

        // The day changing is one of Textrix's lines too.
        let day = text(&[day_changed(&cfg, 60, lines[0].ts, false)])[0].clone();
        assert_eq!(bar(&day), Some(21), "{day}");
        assert!(day.starts_with("      -!-") && day.contains("│ Day changed to"), "{day}");

        // Narrow panes, and nick_column = 0, keep nicks inline.
        assert!(text(&render_line(&lines[1], 40, &cfg))[0].contains("<a_very_long_nickname> hi"));
        let inline = Config { nick_column: 0, ..Config::default() };
        assert!(text(&render_line(&lines[0], 60, &inline))[0].contains("<nesbitt> can"));
    }

    #[test]
    fn code_takes_the_themes_syntax_colours() {
        use crate::{highlight::Syntax, richtext::Marks};
        let t = Theme::default();
        let keyword = Marks { code: true, syntax: Some(Syntax::Keyword), ..Marks::default() };
        assert_eq!(rich_style(&t, Style::new(), keyword, false).fg, t.syntax[Syntax::Keyword as usize].fg);
        let plain = Marks { code: true, ..Marks::default() };
        assert_eq!(rich_style(&t, Style::new(), plain, false).fg, t.code.fg);
    }

    #[test]
    fn wraps_at_spaces_with_hanging_indent() {
        let cells: Vec<(char, Style)> = "ab: one two three".chars().map(|c| (c, Style::new())).collect();
        assert_eq!(text(&wrap(&cells, 10, &[Span::raw("    ")])), vec!["ab: one", "    two", "    three"]);
    }

    #[test]
    fn newline_forces_break_and_long_words_split() {
        let cells: Vec<(char, Style)> = "x\nabcdefgh".chars().map(|c| (c, Style::new())).collect();
        assert_eq!(text(&wrap(&cells, 4, &[Span::raw(" ")])), vec!["x", " abc", " def", " gh"]);
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
