//! irssi-style layout: topic bar, scrollback, status bar, input line.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Local};
use matrix_sdk::ruma::{EventId, OwnedEventId, UserId};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line as TLine, Span},
    widgets::{Block, Borders, Paragraph},
};
use unicode_width::UnicodeWidthChar;

use ratatui_image::Image;

use crate::{
    app::{App, ConnStatus, Preview},
    config::Config,
    input::display_width,
    links,
    model::{Activity, Line, LineKind, PresenceKind},
    richtext::Marks,
};

const BAR: Style = Style::new().fg(Color::White).bg(Color::Blue);
const BRACKET: Style = Style::new().fg(Color::Cyan).bg(Color::Blue);
const DIM: Style = Style::new().fg(Color::DarkGray);
const MARKER: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
const NICK_COLORS: [Color; 10] = [
    Color::Cyan,
    Color::Green,
    Color::Yellow,
    Color::Blue,
    Color::Red,
    Color::LightCyan,
    Color::LightGreen,
    Color::LightBlue,
    Color::LightRed,
    Color::LightYellow,
];

pub fn draw(f: &mut Frame, app: &mut App) {
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

    app.view_width = main.width;
    app.view_height = main.height;
    draw_topic(f, topic, app);
    if side_w > 0 {
        draw_sidebar(f, side, app);
    }
    draw_messages(f, main, app);
    if nick_w > 0 {
        draw_nicklist(f, nicks, app);
    }
    draw_status(f, status, app);
    draw_input(f, input, app);
}

fn activity_style(level: Activity) -> Style {
    match level {
        Activity::Highlight => Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        Activity::Message => Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        Activity::Event => Style::new().fg(Color::Cyan),
        Activity::None => Style::new().fg(Color::Gray),
    }
}

fn sidebar_width(app: &App) -> u16 {
    let longest = app.windows.iter().map(|w| w.name.chars().count()).max().unwrap_or(6);
    (longest.clamp(8, 22) + 9) as u16
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
    let mut out = vec![SidebarEntry::Window(0)];
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
        let windows: Vec<usize> = (1..app.windows.len()).filter(|&i| app.windows[i].account() == Some(server)).collect();
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
fn draw_sidebar(f: &mut Frame, area: Rect, app: &App) {
    let inner = area.width.saturating_sub(1) as usize;
    let rows: Vec<(TLine<'static>, bool)> = sidebar_entries(app)
        .into_iter()
        .map(|entry| match entry {
            SidebarEntry::Header(text) => {
                let text: String = text.chars().take(inner).collect();
                (TLine::from(Span::styled(format!(" {text}"), DIM.add_modifier(Modifier::BOLD))), false)
            }
            SidebarEntry::Window(i) => (window_row(app, i, inner), i == app.active),
        })
        .collect();
    // Keep the active window in view.
    let height = area.height as usize;
    let active_row = rows.iter().position(|(_, a)| *a).unwrap_or(0);
    let start = (active_row + 1).saturating_sub(height);
    let shown: Vec<TLine> = rows.into_iter().skip(start).take(height).map(|(r, _)| r).collect();
    let block = Block::new().borders(Borders::RIGHT).border_style(DIM);
    f.render_widget(Paragraph::new(shown).block(block), area);
}

fn window_row(app: &App, i: usize, inner: usize) -> TLine<'static> {
    let w = &app.windows[i];
    let active = i == app.active;
    let unread = match (active, w.typing.is_empty(), w.unread()) {
        (false, false, _) => "✎".to_owned(),
        (false, true, n) if n > 0 => n.to_string(),
        _ => String::new(),
    };
    let number = format!("{:>2} ", i + 1);
    let dot = w.dm_presence(app).map(|p| presence_dot(p.kind));
    let dot_w = if dot.is_some() { 2 } else { 0 };
    let room_w = inner.saturating_sub(number.len() + dot_w + display_len(&unread) + 1);
    let name: String = w.name.chars().filter(|c| !c.is_control()).take(room_w).collect();
    let pad = inner.saturating_sub(number.len() + dot_w + display_len(&name) + display_len(&unread));
    let style = if active {
        Style::new().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        activity_style(w.activity)
    };
    let mut parts = vec![Span::styled(number, if active { style } else { DIM })];
    if let Some((symbol, color)) = dot {
        parts.push(Span::styled(format!("{symbol} "), if active { style } else { Style::new().fg(color) }));
    }
    parts.extend([
        Span::styled(name, style),
        Span::styled(" ".repeat(pad), style),
        Span::styled(unread, if active { style } else { activity_style(w.activity) }),
    ]);
    TLine::from(parts)
}

fn nicklist_width(app: &App) -> u16 {
    let win = &app.windows[app.active];
    let longest = win.members.iter().map(|m| m.name.chars().count()).max().unwrap_or(6);
    (longest.clamp(6, 18) + 5) as u16
}

fn presence_dot(kind: PresenceKind) -> (char, Color) {
    match kind {
        PresenceKind::Online => ('●', Color::Green),
        PresenceKind::Away => ('●', Color::Yellow),
        PresenceKind::Offline => ('○', Color::DarkGray),
    }
}

fn draw_nicklist(f: &mut Frame, area: Rect, app: &App) {
    let win = &app.windows[app.active];
    let inner = area.width.saturating_sub(1) as usize;
    let height = area.height as usize;
    let mut rows = vec![TLine::from(Span::styled(format!("Users ({})", win.members.len()), DIM))];
    let room = height.saturating_sub(1);
    let overflow = win.members.len() > room;
    let take = if overflow { room.saturating_sub(1) } else { room };
    for m in win.members.iter().take(take) {
        let prefix_style = match m.prefix {
            '@' => Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            '+' => Style::new().fg(Color::Green),
            _ => Style::new(),
        };
        let name: String = m.name.chars().filter(|c| !c.is_control()).take(inner.saturating_sub(3)).collect();
        let (symbol, color) = match win.account().and_then(|a| app.presence_of(a, &m.user_id)) {
            Some(p) => presence_dot(p.kind),
            None => (' ', Color::Reset),
        };
        rows.push(TLine::from(vec![
            Span::styled(m.prefix.to_string(), prefix_style),
            Span::styled(format!("{symbol} "), Style::new().fg(color)),
            Span::styled(name, Style::new().fg(nick_color(m.user_id.as_str()))),
        ]));
    }
    if overflow {
        rows.push(TLine::from(Span::styled(format!("… {} more", win.members.len() - take), DIM)));
    }
    let block = Block::new().borders(Borders::LEFT).border_style(DIM);
    f.render_widget(Paragraph::new(rows).block(block), area);
}

fn display_len(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

fn draw_topic(f: &mut Frame, area: Rect, app: &App) {
    let win = &app.windows[app.active];
    let mut text = match (&win.room, &win.topic) {
        (None, _) => format!(" Textrix {} — {}", env!("CARGO_PKG_VERSION"), app.servers_summary()),
        (Some(_), Some(topic)) => format!(" {topic}"),
        (Some(_), None) => format!(" {}", win.name),
    };
    if let Some(presence) = win.dm_presence(app) {
        text.push_str(&format!(" · {} is {}", win.name, presence.describe()));
    }
    let text: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    f.render_widget(Paragraph::new(text).style(BAR), area);
}

fn draw_messages(f: &mut Frame, area: Rect, app: &mut App) {
    let width = area.width as usize;
    let height = area.height as usize;
    let selected = app.focus_event();
    let images = app.picker.is_some();
    let (previews, requests) = (&mut app.previews, &mut app.preview_requests);
    let cfg = &app.cfg;
    let win = &mut app.windows[app.active];
    // Images sit under their message, lined up with the message text.
    let image_x = display_len(&Local::now().format(&cfg.timestamp_format).to_string()) as u16 + 1;
    let mut placements: Vec<(OwnedEventId, usize, u16)> = Vec::new();
    let visible: Vec<&Line> = win.lines.iter().filter(|l| cfg.show_joins || l.kind != LineKind::Membership).collect();

    // Who has read up to which event (not shown on their own messages).
    let mut readers: HashMap<&EventId, Vec<(&UserId, &str)>> = HashMap::new();
    for (user, (nick, event_id)) in &win.read_by {
        readers.entry(&**event_id).or_default().push((&**user, nick.as_str()));
    }

    // The "new messages" divider only matters if someone else said something.
    let last_from_others = visible.iter().rposition(|l| l.is_message() && !l.own && l.event_id.is_some());

    let mut need = height + win.scroll;
    let mut rows: Vec<TLine<'static>> = Vec::new();
    let mut found_selected = selected.is_none();
    let mut i = visible.len();
    while i > 0 && (rows.len() < need || !found_selected) {
        i -= 1;
        let line = visible[i];
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
                }
                Some(Preview::Loading | Preview::Failed) => {}
                None => {
                    previews.insert(id.clone(), Preview::Loading);
                    requests.push((id.clone(), media.clone()));
                }
            }
        }
        let below = rows.len();
        let mut rendered = render_line_with(line, width, cfg, &read);
        if is_selected {
            for row in &mut rendered {
                *row = row.clone().patch_style(Style::new().add_modifier(Modifier::REVERSED));
            }
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
            rows.push(day_changed(line.ts));
        }
        if i > 0
            && visible[i - 1].event_id.is_some()
            && visible[i - 1].event_id == win.marker
            && last_from_others.is_some_and(|j| j >= i)
        {
            rows.push(new_messages(width));
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
    let end = total - win.scroll.min(total);
    let start = end.saturating_sub(height);
    let shown = rows[start..end].to_vec();
    let pad = height - shown.len();
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
}

fn new_messages(width: usize) -> TLine<'static> {
    let label = " new messages ";
    let side = width.saturating_sub(label.len()) / 2;
    let rule = "─".repeat(side);
    TLine::from(Span::styled(format!("{rule}{label}{rule}"), Style::new().fg(Color::Red)))
}

fn day_changed(ts: DateTime<Local>) -> TLine<'static> {
    TLine::from(vec![Span::styled("-!- ", MARKER), Span::raw(format!("Day changed to {}", ts.format("%a %d %b %Y")))])
}

/// Format one scrollback line and wrap it to `width`, indenting
/// continuation rows under the message text.
pub fn render_line(line: &Line, width: usize, cfg: &Config) -> Vec<TLine<'static>> {
    render_line_with(line, width, cfg, &[])
}

/// `render_line`, plus the nicks whose read receipt sits on this line.
fn render_line_with(line: &Line, width: usize, cfg: &Config, read_by: &[&str]) -> Vec<TLine<'static>> {
    let mut cells: Vec<(char, Style)> = Vec::new();
    let push = |cells: &mut Vec<(char, Style)>, s: &str, style: Style| {
        cells.extend(s.chars().filter_map(|c| sanitize(c).map(|c| (c, style))));
    };

    push(&mut cells, &line.ts.format(&cfg.timestamp_format).to_string(), DIM);
    push(&mut cells, " ", Style::new());

    let nick_style = if line.highlight {
        Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD)
    } else if line.own {
        Style::new().add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(nick_color(line.sender.as_ref().map_or(line.nick.as_str(), |s| s.as_str())))
    };
    let mut body_style = match line.kind {
        LineKind::Error => Style::new().fg(Color::Red),
        LineKind::Utd => DIM.add_modifier(Modifier::ITALIC),
        _ if line.highlight => Style::new().fg(Color::Yellow),
        _ => Style::new(),
    };
    if line.pending || line.redacted {
        body_style = DIM;
    }

    match line.kind {
        LineKind::Message | LineKind::Utd => {
            push(&mut cells, "<", DIM);
            push(&mut cells, &line.nick, nick_style);
            push(&mut cells, "> ", DIM);
        }
        LineKind::Emote => {
            push(&mut cells, " * ", MARKER);
            push(&mut cells, &line.nick, nick_style);
            push(&mut cells, " ", Style::new());
        }
        LineKind::Notice => {
            push(&mut cells, "-", DIM);
            push(&mut cells, &line.nick, nick_style);
            push(&mut cells, "- ", DIM);
        }
        LineKind::Error => push(&mut cells, "-!- ", Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)),
        LineKind::Membership | LineKind::Event | LineKind::Info => push(&mut cells, "-!- ", MARKER),
    }
    if line.reply_to.is_some() || line.thread {
        let mark = if line.thread { '⤷' } else { '↪' };
        let to = match (&line.reply_nick, &line.reply_snippet) {
            (Some(nick), Some(text)) => format!("{mark} {nick} “{text}” "),
            (Some(nick), None) => format!("{mark} {nick}: "),
            _ => format!("{mark} "),
        };
        push(&mut cells, &to, DIM);
    }

    let prefix_width: usize = cells.iter().map(|(c, _)| display_width(*c)).sum();
    let indent = if prefix_width > width / 2 { 4.min(width.saturating_sub(1)) } else { prefix_width };

    match &line.rich {
        Some(spans) if !line.redacted => {
            for span in spans {
                let style = rich_style(body_style, span.marks, line.highlight);
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
                push(&mut cells, &line.body[range.clone()], body_style.fg(Color::Blue).add_modifier(Modifier::UNDERLINED));
                at = range.end;
            }
            push(&mut cells, &line.body[at..], body_style);
        }
    }
    if line.edited && !line.redacted {
        push(&mut cells, " (edited)", DIM);
    }
    if let Some(percent) = line.progress {
        push(&mut cells, &format!(" ({percent}%)"), DIM);
    }
    if !line.reactions.is_empty() {
        let summary: Vec<String> = line.reactions.iter().map(|(key, nicks)| format!("{key} {}", nicks.len())).collect();
        push(&mut cells, &format!(" [{}]", summary.join(" ")), DIM);
    }
    if !read_by.is_empty() {
        let shown = read_by.iter().take(3).copied().collect::<Vec<_>>().join(", ");
        let more = read_by.len().saturating_sub(3);
        let text = if more > 0 { format!(" ✓ {shown} +{more}") } else { format!(" ✓ {shown}") };
        push(&mut cells, &text, Style::new().fg(Color::Green).add_modifier(Modifier::DIM));
    }

    wrap(&cells, width, indent)
}

fn rich_style(base: Style, marks: Marks, highlight: bool) -> Style {
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
    if let Some((r, g, b)) = marks.color {
        style = style.fg(Color::Rgb(r, g, b));
    }
    if marks.code {
        style = style.fg(Color::LightYellow);
    }
    if marks.link {
        style = style.fg(Color::Blue).add_modifier(Modifier::UNDERLINED);
    }
    if marks.mention && !highlight {
        style = style.fg(Color::LightBlue).add_modifier(Modifier::BOLD);
    }
    if marks.dim || marks.spoiler {
        style = style.fg(Color::DarkGray);
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
        '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}' | '\u{fe0f}' => {
            None
        }
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

pub fn nick_color(key: &str) -> Color {
    let hash = key.bytes().fold(0x811c9dc5u32, |h, b| (h ^ b as u32).wrapping_mul(0x01000193));
    NICK_COLORS[hash as usize % NICK_COLORS.len()]
}

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let win = &app.windows[app.active];
    let mut spans: Vec<Span> = Vec::new();
    let item = |spans: &mut Vec<Span<'static>>, parts: Vec<Span<'static>>| {
        spans.push(Span::styled("[", BRACKET));
        spans.extend(parts);
        spans.push(Span::styled("] ", BRACKET));
    };

    item(&mut spans, vec![Span::styled(Local::now().format("%H:%M").to_string(), BAR)]);

    if let Some(state) = win.account().and_then(|a| app.accounts.get(a)) {
        let nick = state.nick.clone().unwrap_or_else(|| state.user.clone());
        item(&mut spans, vec![Span::styled(nick, BAR)]);
    }

    let mut name = format!("{}:{}", app.active + 1, win.name);
    if win.encrypted {
        name.push_str("(+E)");
    }
    item(&mut spans, vec![Span::styled(name, BAR)]);

    let active: Vec<(usize, Activity)> = app
        .windows
        .iter()
        .enumerate()
        .filter(|(i, w)| *i != app.active && w.activity > Activity::None)
        .map(|(i, w)| (i + 1, w.activity))
        .collect();
    if !active.is_empty() {
        let mut parts = vec![Span::styled("Act: ", BAR)];
        for (n, (num, level)) in active.into_iter().enumerate() {
            if n > 0 {
                parts.push(Span::styled(",", BAR));
            }
            let style = match level {
                Activity::Highlight => BAR.fg(Color::Magenta).add_modifier(Modifier::BOLD),
                Activity::Message => BAR.fg(Color::White).add_modifier(Modifier::BOLD),
                _ => BAR.fg(Color::Cyan),
            };
            parts.push(Span::styled(num.to_string(), style));
        }
        item(&mut spans, parts);
    }

    if !win.typing.is_empty() {
        item(&mut spans, vec![Span::styled(format!("typing: {}", win.typing.join(", ")), BAR)]);
    }

    for (name, state) in &app.accounts {
        if state.away.is_some() {
            item(&mut spans, vec![Span::styled(format!("{name}: away"), BAR.fg(Color::Yellow))]);
        }
        if state.verified == Some(false) {
            item(&mut spans, vec![Span::styled(format!("{name}: unverified"), BAR.fg(Color::Yellow))]);
        }
        let label = match &state.status {
            ConnStatus::Online => continue,
            ConnStatus::Connecting => "connecting",
            ConnStatus::Syncing => "syncing",
            ConnStatus::Lagging => "lag",
            ConnStatus::Offline => "offline",
        };
        item(&mut spans, vec![Span::styled(format!("{name}: {label}"), BAR.fg(Color::Yellow))]);
    }

    if win.scroll > 0 {
        let style = if win.unseen_below { BAR.add_modifier(Modifier::BOLD) } else { BAR };
        item(&mut spans, vec![Span::styled("more", style)]);
    }

    if app.confirm_delete.is_some() {
        let what = app.delete_summary().unwrap_or_else(|| "this message".into());
        let prompt = format!("delete {what}? y/n");
        item(&mut spans, vec![Span::styled(prompt, BAR.fg(Color::Yellow).add_modifier(Modifier::BOLD))]);
    } else if app.select.is_some() {
        let keys = "r reply  e edit  d delete  + react  s save  o open  v spoiler  c copy  i info  Esc";
        item(&mut spans, vec![Span::styled(keys, BAR.fg(Color::Yellow))]);
    } else if app.is_editing() {
        let keys = "editing: ↑/↓ pick  Enter save  clear+Enter delete  Esc cancel";
        item(&mut spans, vec![Span::styled(keys, BAR.fg(Color::Yellow))]);
    }

    f.render_widget(Paragraph::new(TLine::from(spans)).style(BAR), area);
}

fn draw_input(f: &mut Frame, area: Rect, app: &App) {
    let win = &app.windows[app.active];
    let label: String = match app.compose_label() {
        Some(label) => label.chars().take(32).collect(),
        None => win.name.chars().take(24).collect(),
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

    let mut spans = vec![Span::styled(prompt, Style::new().fg(Color::Cyan)), Span::raw(text)];
    if app.input.cursor() == chars.len()
        && let Some(hint) = app.input_hint()
    {
        let room = avail.saturating_sub(used + 1);
        let hint: String = hint.chars().take(room).collect();
        spans.push(Span::styled(hint, DIM));
    }
    f.render_widget(Paragraph::new(TLine::from(spans)), area);
    let x = area.x + (prompt_w + cursor_w - skip_w).min(area.width.saturating_sub(1) as usize) as u16;
    f.set_cursor_position(Position::new(x, area.y));
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
    fn control_characters_are_dropped() {
        assert_eq!(sanitize('\u{1b}'), None);
        assert_eq!(sanitize('\u{202e}'), None);
        assert_eq!(sanitize('\u{fe0f}'), None, "pencil + VS16 must not overflow the row");
        assert_eq!(sanitize('é'), Some('é'));
    }
}
