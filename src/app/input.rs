//! What you do at the terminal: keys, the mouse, selection mode, and the input line (completion, hints, submitting).

use super::*;

impl App {
    /// Once a second: last session's split if it's waiting, scripts' live
    /// status items, and their stores saved if they changed.
    /// You're here: back from auto-away, if it had set you away.
    pub(super) fn present(&mut self) {
        self.last_input = Instant::now();
        if std::mem::take(&mut self.auto_away) {
            // Every server, whether or not its away had come through yet.
            for acct in self.accounts.values().filter_map(|s| s.account.clone()) {
                acct.set_away(None);
            }
        }
    }

    pub fn on_terminal(&mut self, ev: Event) {
        // Whatever an Escape is held for, it goes first.
        if !matches!(ev, Event::Key(_)) {
            self.esc_timeout();
        }
        let here = match &ev {
            Event::Key(_) | Event::Paste(_) | Event::FocusGained => true,
            Event::Mouse(m) => m.kind != MouseEventKind::Moved,
            _ => false,
        };
        if here {
            self.present();
        }
        match ev {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.on_key_in(key),
            Event::Paste(text) => {
                let empty = self.input.is_empty();
                self.input.paste(&text);
                self.typed();
                // A dropped file arrives as its path.
                if empty {
                    self.offer_upload();
                }
            }
            Event::FocusGained => self.focused = true,
            Event::FocusLost => self.focused = false,
            Event::Mouse(m) => self.on_mouse(m),
            _ => {}
        }
    }

    pub(super) fn on_mouse(&mut self, m: MouseEvent) {
        let at = Position::new(m.column, m.row);
        if let Some(p) = self.sticker_picker.as_mut() {
            let across = p.cols.max(1) as isize;
            match m.kind {
                MouseEventKind::ScrollUp => p.step(-across),
                MouseEventKind::ScrollDown => p.step(across),
                MouseEventKind::Down(MouseButton::Left) => self.on_sticker_click(at),
                _ => {}
            }
            return;
        }
        if let Some(d) = self.directory.as_mut() {
            match m.kind {
                MouseEventKind::ScrollUp => d.select(-3),
                MouseEventKind::ScrollDown => d.select(3),
                MouseEventKind::Down(MouseButton::Left) => self.on_directory_click(at),
                _ => {}
            }
            return;
        }
        match m.kind {
            MouseEventKind::ScrollUp => self.on_wheel(at, true),
            MouseEventKind::ScrollDown => self.on_wheel(at, false),
            // Dragging the line between two panes moves it.
            MouseEventKind::Down(MouseButton::Left) if let Some(k) = self.divider_at(at) => self.dragging = Some(k),
            MouseEventKind::Drag(MouseButton::Left) if let Some(k) = self.dragging => {
                let side = self.split.as_ref().is_some_and(|s| s.side_by_side);
                if let (Some((a, _)), Some((b, _))) =
                    (self.hits.splits.get(k).copied(), self.hits.splits.get(k + 1).copied())
                {
                    let (start, at, pair) =
                        if side { (a.x, at.x, a.width + b.width) } else { (a.y, at.y, a.height + b.height) };
                    let cells = self.clamp_pane(i32::from(at) - i32::from(start) + 1, pair);
                    self.set_divider(k, cells);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => self.dragging = None,
            MouseEventKind::Down(MouseButton::Left) => self.on_click(at),
            MouseEventKind::Down(MouseButton::Right) => self.on_right_click(at),
            _ => {}
        }
    }

    /// A right-click on a room or DM in the window list makes it a
    /// favourite, or not.
    pub(super) fn on_right_click(&mut self, at: Position) {
        let Some(&(_, idx)) = self.hits.sidebar.iter().find(|(r, _)| r.contains(at)) else { return };
        let Some(key) = self.windows[idx].room.clone() else { return };
        if let Err(e) = self.toggle_favourite(&key.account, &key.room_id) {
            self.error(e);
        }
    }

    /// The wheel scrolls the pane under the pointer (older history loads
    /// at the top), or moves the selection in selection mode.
    pub(super) fn on_wheel(&mut self, at: Position, up: bool) {
        if let Some(v) = self.viewer.as_mut() {
            v.zoom_by(if up { 1 } else { -1 });
            return;
        }
        if self.gallery.is_some() {
            let Some(idx) = self.gallery_window() else { return };
            let way = if up { gallery::Move::Up } else { gallery::Move::Down };
            if self.gallery.as_ref().is_some_and(|g| g.tab == gallery::Tab::Links) {
                let links = self.shown_links(idx);
                if let Some(g) = self.gallery.as_mut() {
                    g.step_link(&links, way);
                }
                return;
            }
            let items = gallery::items(&self.windows[idx].lines);
            if let Some(g) = self.gallery.as_mut() {
                g.step(&items, way);
            }
            return;
        }
        if let Some(h) = self.home.as_mut() {
            h.selected = if up { h.selected.saturating_sub(1) } else { h.selected + 1 };
            return;
        }
        if let Some(sw) = self.switcher.as_mut() {
            sw.selected = if up { sw.selected.saturating_sub(1) } else { sw.selected + 1 };
            return;
        }
        if let Some(p) = self.emoji_picker.as_mut() {
            let entries = p.entries();
            let rows = emoji_picker::layout(&entries, p.width);
            p.step(&entries, &rows, if up { emoji_picker::Way::Up } else { emoji_picker::Way::Down });
            return;
        }
        if let Some(p) = self.theme_picker.as_mut() {
            p.selected = if up { p.selected.saturating_sub(1) } else { p.selected + 1 };
            return self.try_picked_theme();
        }
        if let Some(help) = self.help.as_mut() {
            help.scroll =
                if up { help.scroll.saturating_sub(WHEEL_ROWS) } else { help.scroll.saturating_add(WHEEL_ROWS) };
            return;
        }
        let Some(idx) = self.hits.panes.iter().find(|p| p.area.contains(at)).map(|p| p.window) else { return };
        if idx == self.active && self.select.is_some() {
            return self.move_select(if up { -1 } else { 1 });
        }
        let w = &mut self.windows[idx];
        w.scroll = if up { w.scroll + WHEEL_ROWS } else { w.scroll.saturating_sub(WHEEL_ROWS) };
    }

    /// Clicks: a window in the list goes there; a pane gets the focus; a
    /// message is selected (again to unselect); a link opens; a member's
    /// name is added to your message; the input line moves the cursor.
    pub(super) fn on_click(&mut self, at: Position) {
        if self.viewer.is_some() {
            return;
        }
        let in_list = self.hits.sidebar.iter().any(|(r, _)| r.contains(at));
        if self.gallery.is_some() {
            // A tile is chosen, and opened when it already was; the window
            // list closes the gallery and goes there.
            if let Some(&(_, n)) = self.hits.gallery.iter().find(|(r, _)| r.contains(at)) {
                let Some(idx) = self.gallery_window() else { return };
                if self.gallery.as_ref().is_some_and(|g| g.tab == gallery::Tab::Links) {
                    let links = self.shown_links(idx);
                    let Some(g) = self.gallery.as_mut() else { return };
                    let again = g.link_index(&links) == n;
                    g.link = links.get(n).map(|l| (l.event_id.clone(), l.url.clone()));
                    if let (true, Some(link)) = (again, links.get(n)) {
                        self.open_url(link.url.clone());
                    }
                    return;
                }
                let items = gallery::items(&self.windows[idx].lines);
                let Some(g) = self.gallery.as_mut() else { return };
                let again = g.index(&items) == n;
                g.selected = items.get(n).and_then(|l| l.event_id.clone());
                if again {
                    self.gallery_open(idx, false);
                }
                return;
            }
            if !in_list {
                return;
            }
            self.close_gallery();
        }
        if self.home.is_some() {
            // A row goes there (an invitation is only chosen: Enter joins).
            if let Some(&(_, n)) = self.hits.home.iter().find(|(r, _)| r.contains(at)) {
                match self.home_rows().get(n).cloned() {
                    Some(home::Row::Invite(_)) => self.home.iter_mut().for_each(|h| h.selected = n),
                    Some(row) => self.home_go(row),
                    None => {}
                }
                return;
            }
            if !in_list {
                return;
            }
            self.home = None;
        }
        if self.theme_editor.is_some() {
            // A role picks it; elsewhere does nothing (Esc and s finish).
            let row = self
                .hits
                .theme_editor
                .as_ref()
                .and_then(|(_, rows)| rows.iter().find(|(r, _)| r.contains(at)))
                .map(|&(_, n)| n);
            if let (Some(n), Some(e)) = (row, self.theme_editor.as_mut())
                && e.typing.is_none()
                && e.naming.is_none()
            {
                e.selected = n;
            }
            return;
        }
        if self.theme_picker.is_some() {
            // A theme shows it, again keeps it; a click outside puts back
            // the one you had.
            let hits = self.hits.theme_picker.as_ref();
            let row = hits.and_then(|(_, rows)| rows.iter().find(|(r, _)| r.contains(at))).map(|&(_, n)| n);
            let inside = hits.is_some_and(|(area, _)| area.contains(at));
            match (row, self.theme_picker.as_mut()) {
                (Some(n), Some(p)) if p.selected == n => self.close_theme_picker(true),
                (Some(n), Some(p)) => {
                    p.selected = n;
                    self.try_picked_theme();
                }
                _ if !inside => self.close_theme_picker(false),
                _ => {}
            }
            return;
        }
        if self.emoji_picker.is_some() {
            // An emoji reacts with it; a click outside closes the picker.
            let hits = self.hits.emoji_picker.as_ref();
            let picked = hits.and_then(|(_, cells)| cells.iter().find(|(r, _)| r.contains(at))).map(|&(_, n)| n);
            let inside = hits.is_some_and(|(area, _)| area.contains(at));
            match (picked, self.emoji_picker.as_ref()) {
                (Some(n), Some(p)) => {
                    if let Some(entry) = p.entries().get(n) {
                        let label = entry.pick.label();
                        self.react_picked(&label);
                    }
                }
                _ if !inside => self.emoji_picker = None,
                _ => {}
            }
            return;
        }
        if self.switcher.is_some() {
            // A row goes there; a click anywhere else closes the switcher.
            let picked = self.hits.switcher.as_ref().and_then(|(_, rows)| rows.iter().find(|(r, _)| r.contains(at)));
            match picked {
                Some(&(_, n)) => self.switcher_go(Some(n)),
                None if !self.hits.switcher.as_ref().is_some_and(|(area, _)| area.contains(at)) => {
                    self.switcher = None;
                }
                None => {}
            }
            return;
        }
        if self.help.is_some()
            || self.confirm_delete.is_some()
            || self.confirm_paste.is_some()
            || self.confirm_upload.is_some()
        {
            return;
        }
        if let Some(&(_, idx)) = self.hits.sidebar.iter().find(|(r, _)| r.contains(at)) {
            return self.switch_to(idx);
        }
        if let Some(&(_, n)) = self.hits.nicklist.iter().find(|(r, _)| r.contains(at)) {
            return self.open_member(n);
        }
        if self.hits.profile.is_some_and(|area| area.contains(at)) {
            return;
        }
        if let Some((area, text_x, skip)) = self.hits.input
            && area.contains(at)
        {
            // Clicking where you type means typing: no message stays
            // selected.
            self.select = None;
            return self.input.click(skip, at.x.saturating_sub(text_x) as usize);
        }
        let Some(pane) = self.hits.panes.iter().find(|p| p.area.contains(at)) else { return };
        let idx = pane.window;
        let row = pane.rows.get((at.y - pane.area.y) as usize).cloned().unwrap_or_default();
        if idx != self.active {
            self.switch_to(idx);
        }
        if self.is_editing() {
            return;
        }
        // A nick: who's that?
        if let Some((from, to, user)) = row.nick.clone()
            && (from..to).contains(&at.x)
            && !self.win().is_mentions
        {
            let name = row
                .event
                .as_ref()
                .and_then(|e| self.win().lines.iter().rev().find(|l| l.event_id.as_ref() == Some(e)))
                .map_or_else(|| user.localpart().to_owned(), |l| l.nick.clone());
            let member = self.win().members.iter().position(|m| m.user_id == user);
            return self.open_profile(user, name, member);
        }
        // The picture itself, not the rest of its rows: a click beside it
        // is on its message like any other.
        if row.image.is_some_and(|(from, to)| (from..to).contains(&at.x))
            && let Some(line) = row
                .event
                .as_ref()
                .and_then(|e| self.win().lines.iter().rev().find(|l| l.event_id.as_ref() == Some(e)))
                .cloned()
        {
            if let Err(e) = self.open_viewer(&line) {
                self.error(e);
            }
            return;
        }
        if self.win().is_mentions
            && let Some(event) = row.event.clone()
            && let Some(key) = self.mention_origins.get(&event).cloned()
        {
            self.select = None;
            if let Err(e) = self.jump_to(key, event) {
                self.error(e);
            }
            return;
        }
        if let Some(event) = &row.event
            && let Some((_, _, text)) = row.links.iter().find(|(from, to, _)| (*from..*to).contains(&at.x))
            && let Some(url) = self.link_in(event, text)
        {
            return self.open_url(url);
        }
        self.select = match row.event {
            Some(event) if self.select.as_ref() != Some(&event) && self.selectable().contains(&event) => Some(event),
            _ => None,
        };
    }

    /// The link a clicked piece of underlined text belongs to (a wrapped
    /// link shows in pieces).
    pub(super) fn link_in(&self, event: &EventId, fragment: &str) -> Option<String> {
        self.link_in_window(self.active, event, fragment)
    }

    pub(super) fn link_in_window(&self, window: usize, event: &EventId, fragment: &str) -> Option<String> {
        let fragment = fragment.trim().trim_matches(|c: char| "()<>[]\"'".contains(c));
        if fragment.chars().count() < 3 {
            return None;
        }
        let line = self.windows.get(window)?.lines.iter().rev().find(|l| l.event_id.as_deref() == Some(event))?;
        let text = line.text();
        let url = links::urls(&line.body).into_iter().chain(links::urls(&text)).find(|u| u.contains(fragment));
        url.map(ToOwned::to_owned)
    }

    /// Close the profile pane with `command` in the input line to send.
    pub(super) fn prefill(&mut self, command: &str) {
        self.profile = None;
        self.input.set_text(command);
    }

    /// Add a mention of `user` (as `name`) to what you're typing.
    pub(super) fn mention_user(&mut self, name: String, user: OwnedUserId) {
        self.input.end_completion();
        let text = if self.input.is_empty() {
            format!("{name}{} ", self.cfg.completion_char)
        } else {
            let after_word = self.input.cursor() > 0
                && self.input.chars().get(self.input.cursor() - 1).is_some_and(|c| !c.is_whitespace());
            format!("{}{name} ", if after_word { " " } else { "" })
        };
        self.input.insert_str(&text);
        if !self.input.mentions.iter().any(|(_, u)| *u == user) {
            self.input.mentions.push((name, user));
        }
    }

    pub(super) fn on_key(&mut self, key: KeyEvent) {
        use KeyCode::*;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if !(ctrl && key.code == Char('c')) {
            self.quit_armed = None;
        }
        if self.recording.is_some() {
            // Recording: Enter sends it, Esc or Ctrl+C throws it away.
            match key.code {
                Enter => self.stop_voice(true),
                Esc => self.stop_voice(false),
                Char('c') if ctrl => self.stop_voice(false),
                _ => {}
            }
            return;
        }
        if let Some(v) = self.viewer.as_mut() {
            match key.code {
                Esc | Enter | Char('q' | 'z') => self.close_viewer(),
                Char('c') if ctrl => self.close_viewer(),
                Char('+' | '=') => v.zoom_by(1),
                Char('-' | '_') => v.zoom_by(-1),
                Char('0') => v.zoom_by(0),
                Left | Char('h') => v.pan(-1.0, 0.0),
                Right | Char('l') => v.pan(1.0, 0.0),
                Up | Char('k') => v.pan(0.0, -1.0),
                Down | Char('j') => v.pan(0.0, 1.0),
                _ => {}
            }
            return;
        }
        if let Some(help) = self.help.as_mut() {
            match key.code {
                Esc if !help.filter.is_empty() => {
                    help.filter.clear();
                    help.scroll = 0;
                }
                Esc | F(1) => self.help = None,
                Char('c') if ctrl => self.help = None,
                Up => help.scroll = help.scroll.saturating_sub(1),
                Down => help.scroll += 1,
                PageUp => help.scroll = help.scroll.saturating_sub(10),
                PageDown => help.scroll += 10,
                Home => help.scroll = 0,
                End => help.scroll = usize::MAX,
                Backspace => {
                    help.filter.pop();
                    help.scroll = 0;
                }
                Char(c) if !ctrl && !alt => {
                    help.filter.push(c);
                    help.scroll = 0;
                }
                _ => {}
            }
            return;
        }
        if self.verify_panel.is_some() {
            return self.on_verify_key(key);
        }
        if self.gallery.is_some() {
            return self.on_gallery_key(key);
        }
        if self.home.is_some() {
            return self.on_home_key(key);
        }
        if self.switcher.is_some() {
            return self.on_switcher_key(key);
        }
        if self.directory.is_some() {
            return self.on_directory_key(key);
        }
        if self.theme_picker.is_some() {
            return self.on_theme_picker_key(key);
        }
        if self.theme_editor.is_some() {
            return self.on_theme_editor_key(key);
        }
        if self.emoji_picker.is_some() {
            return self.on_emoji_key(key);
        }
        if self.sticker_picker.is_some() {
            return self.on_sticker_key(key);
        }
        if self.vote.is_some() {
            return self.on_vote_key(key);
        }
        if self.profile.is_some() {
            return self.on_profile_key(key);
        }
        if self.select.is_none() && (key.modifiers - KeyModifiers::SHIFT).is_empty() {
            self.konami = eggs::konami_step(self.konami, key.code);
            if self.konami == eggs::KONAMI.len() {
                self.konami = 0;
                return self.konami_unlocked();
            }
        } else {
            self.konami = 0;
        }
        if let Some(files) = self.confirm_upload.take() {
            // Dropping files and pressing Enter uploads them.
            if matches!(key.code, Char('y' | 'Y') | Enter) {
                self.input.take();
                for path in files {
                    if let Err(e) = self.upload_file(path) {
                        self.error(e);
                    }
                }
            } else {
                // Declined: it stays as text, and Enter sends it as text.
                self.upload_declined = Some(self.input.text());
            }
            return;
        }
        if self.confirm_paste.take().is_some() {
            // Declining leaves the text in the input line to edit.
            if matches!(key.code, Char('y' | 'Y')) {
                self.input.accept_paste();
                self.submit();
            }
            return;
        }
        if let Some(event) = self.confirm_delete.take() {
            let resume = self.resume_edit.take();
            if matches!(key.code, Char('y' | 'Y')) {
                self.select = None;
                match self.room_ctx() {
                    Ok((acct, room_id)) => acct.redact(room_id, event),
                    Err(e) => self.error(e),
                }
                if let Some(session) = resume {
                    self.input.set_text(&session.draft);
                }
            } else if let Some(session) = resume {
                // Declined: back to editing that message.
                self.input.set_text(&session.original);
                self.editing = Some(session);
            }
            return;
        }
        if self.select.is_some() {
            if !(alt && matches!(key.code, Char(_))) {
                self.on_select_key(key);
                return;
            }
            // Esc and the next key typed quickly can come in together, as
            // Alt+key: leave selecting, and the key does what it does
            // outside (Esc 1 still goes to window 1, as in irssi).
            self.select = None;
        }
        // Shift+Enter (or Alt+Enter) starts a new line in the message.
        if key.code == Enter && key.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) {
            self.input.end_completion();
            self.input.insert('\n');
            self.typed();
            return;
        }
        if self.editing.is_some() {
            match key.code {
                Up => return self.step_edit(true),
                Down => return self.step_edit(false),
                Esc => return self.cancel_edit(),
                Enter => return self.finish_edit(),
                _ => {}
            }
        }
        if key.code == Up && alt {
            self.enter_select();
            return;
        }
        if (key.code == Up && key.modifiers.contains(KeyModifiers::SHIFT))
            || (ctrl && key.code == Char('e') && self.input.is_empty())
        {
            self.start_edit(None);
            return;
        }
        if key.code == Esc {
            if self.pasted.is_some() {
                self.drop_pasted();
                self.info("pasted image dropped");
            } else if self.compose.take().is_some() {
                self.input.clear();
            } else if self.input.is_empty() {
                self.close_thread();
            }
            return;
        }
        if key.code != Tab {
            self.input.end_completion();
        }
        let mut edited = false;
        match key.code {
            Tab => {
                let candidates = self.candidates();
                self.input.complete(move || candidates);
            }
            Enter => self.submit(),
            Char(c) if alt => match c {
                '1'..='9' => self.switch_to(c as usize - '1' as usize),
                '0' => self.switch_to(9),
                'a' => self.next_active(),
                'r' => {
                    if let Err(e) = self.jump_to_unread() {
                        self.error(e);
                    }
                }
                'k' => self.switcher = Some(Switcher::default()),
                'b' => self.input.word_left(),
                'f' => self.input.word_right(),
                c => match "qwertyuiop".find(c) {
                    Some(i) => self.switch_to(10 + i),
                    // Nothing on Alt+c: it was Esc, then c, come in together
                    // (typed quickly, or through tmux), so "/quit" keeps its
                    // slash.
                    None => {
                        self.on_key(KeyEvent::new(Esc, KeyModifiers::NONE));
                        self.on_key(KeyEvent::new(Char(c), key.modifiers - KeyModifiers::ALT));
                    }
                },
            },
            Char(c) if ctrl => match c {
                'a' => self.input.home(),
                'e' => self.input.end(),
                'b' => self.input.left(),
                'f' => self.input.right(),
                'n' => self.cycle_window(1),
                'p' => self.cycle_window(-1),
                'l' => self.redraw_all = true,
                'r' => {
                    if let Err(e) = self.start_voice() {
                        self.error(e);
                    }
                }
                'v' => {
                    if let Err(e) = self.paste_image() {
                        self.error(e);
                    }
                }
                'x' => self.editor_request = Some(self.input.text()),
                // With nothing after the cursor to cut, Ctrl+K opens the
                // switcher.
                'k' if self.input.cursor() == self.input.chars().len() => {
                    self.switcher = Some(Switcher::default());
                }
                'c' if self.input.is_empty() && self.compose.is_none() => self.ctrl_c_quits(),
                'd' | 'h' | 'u' | 'k' | 'w' | 'y' | 'c' => {
                    match c {
                        'd' => self.input.delete(),
                        'h' => self.input.backspace(),
                        'u' => self.input.kill_to_start(),
                        'k' => self.input.kill_to_end(),
                        'w' => self.input.kill_word(),
                        'y' => self.input.yank(),
                        _ => self.input.clear(),
                    }
                    edited = true;
                }
                _ => {}
            },
            Char(c) => {
                self.input.insert(c);
                edited = true;
            }
            Backspace => {
                self.input.backspace();
                edited = true;
            }
            Delete => {
                self.input.delete();
                edited = true;
            }
            Left if alt => self.cycle_window(-1),
            Right if alt => self.cycle_window(1),
            Left if ctrl => self.input.word_left(),
            Right if ctrl => self.input.word_right(),
            Left => self.input.left(),
            Right => self.input.right(),
            Home => self.input.home(),
            End => self.input.end(),
            Up => self.input.history_prev(),
            Down => self.input.history_next(),
            PageUp => self.scroll(true),
            PageDown => self.scroll(false),
            F(1) => self.help = Some(HelpView::default()),
            F(2) => self.show_sidebar = !self.show_sidebar,
            F(3) => self.show_nicklist = !self.show_nicklist,
            F(4) => self.open_home(),
            F(6) if key.modifiers.contains(KeyModifiers::SHIFT) => self.cycle_pane(-1),
            F(6) => self.cycle_pane(1),
            _ => {}
        }
        if edited {
            self.typed();
        }
    }

    pub(super) fn scroll(&mut self, up: bool) {
        let page = (self.view_height as usize / 2).max(1);
        let w = self.win_mut();
        w.scroll = if up { w.scroll + page } else { w.scroll.saturating_sub(page) };
    }

    /// Send typing notifications while composing a message in a room.
    pub(super) fn typed(&mut self) {
        if !self.cfg.send_typing {
            return;
        }
        let Some(key) = self.win().room.clone() else { return };
        if self.input.is_empty() || self.input.chars().first() == Some(&'/') {
            self.stop_typing();
            return;
        }
        let now = Instant::now();
        if let Some((k, since)) = &self.typing_since
            && *k == key
            && now.duration_since(*since) < Duration::from_secs(3)
        {
            return;
        }
        if let Some(acct) = self.connected(&key.account) {
            acct.typing(key.room_id.clone(), true);
        }
        self.typing_since = Some((key, now));
    }

    pub(super) fn stop_typing(&mut self) {
        if let Some((key, _)) = self.typing_since.take()
            && let Some(acct) = self.connected(&key.account)
        {
            acct.typing(key.room_id, false);
        }
    }

    pub(super) fn selectable(&self) -> Vec<OwnedEventId> {
        self.win().lines.iter().filter(|l| l.is_message() && !l.pending).filter_map(|l| l.event_id.clone()).collect()
    }

    pub(super) fn selected_line(&self) -> Option<&Line> {
        let id = self.select.as_ref()?;
        self.win().lines.iter().rev().find(|l| l.event_id.as_ref() == Some(id))
    }

    pub(super) fn enter_select(&mut self) {
        match self.selectable().pop() {
            Some(last) => self.select = Some(last),
            None => self.info("no messages to select here"),
        }
    }

    /// Move the selection; moving below the newest message leaves selection
    /// mode. At the top, the renderer pulls in older history.
    pub(super) fn move_select(&mut self, delta: isize) {
        let ids = self.selectable();
        let Some(pos) = self.select.as_ref().and_then(|s| ids.iter().position(|id| id == s)) else {
            self.select = None;
            return;
        };
        let target = pos as isize + delta;
        if target >= ids.len() as isize {
            self.select = None;
            self.win_mut().scroll = 0;
        } else {
            self.select = Some(ids[target.max(0) as usize].clone());
        }
    }

    pub(super) fn on_select_key(&mut self, key: KeyEvent) {
        use KeyCode::*;
        let Some(line) = self.selected_line().cloned() else {
            self.select = None;
            return;
        };
        let event = line.event_id.clone().expect("selectable lines have ids");
        let key_here = self.win().room.clone();
        match key.code {
            Up | Char('k') => self.move_select(-1),
            Down | Char('j') => self.move_select(1),
            PageUp => self.move_select(-10),
            PageDown => self.move_select(10),
            Home | Char('g') => self.select = self.selectable().first().cloned(),
            End | Char('G') => self.select = self.selectable().pop(),
            Enter if line.poll.is_some() => self.open_vote(&line),
            Enter if self.win().is_mentions => {
                self.select = None;
                let key =
                    self.mention_origins.get(&event).cloned().ok_or_else(|| "that message's room is gone".to_owned());
                if let Err(e) = key.and_then(|key| self.jump_to(key, event)) {
                    self.error(e);
                }
            }
            Esc | Char('q') | Enter => self.select = None,
            Char('r') => match (line.sender.clone(), key_here) {
                (Some(sender), Some(here)) => {
                    self.compose = Some((here, Compose::Reply { event, sender, nick: line.nick.clone() }));
                    self.select = None;
                }
                _ => self.error("can't reply to that"),
            },
            Char('e') => {
                self.select = None;
                self.start_edit(Some(event));
            }
            Char('d') => self.request_delete(event, None),
            Char('+' | '=') => {
                if let Some(here) = key_here {
                    let custom = self.emoji_for(&here.account, Some(&here.room_id));
                    let nick = self.display_name(line.sender.as_deref(), &line.nick).to_owned();
                    let recent = self.layout.recent_emoji.clone();
                    self.emoji_picker = Some(EmojiPicker::new(event, nick, custom, recent));
                }
            }
            Char('s') => match (line.media.clone(), self.room_ctx()) {
                (Some(media), Ok((acct, room_id))) => acct.download(room_id, media),
                (None, _) => self.error("that message has no attachment"),
                (_, Err(e)) => self.error(e),
            },
            // Typing a command: stop selecting, and it goes in the input
            // line (rather than its letters being keys here: "/theme" was
            // opening a thread).
            Char('/') => {
                self.select = None;
                self.on_key(key);
            }
            Char('t') => {
                self.select = None;
                let root = line.thread_root.clone().unwrap_or(event);
                if let Err(e) = self.open_thread(root) {
                    self.error(e);
                }
            }
            Char('z' | ' ') if line.media.as_ref().is_some_and(|m| m.playable()) => {
                if let Err(e) = self.play(&line) {
                    self.error(e);
                }
            }
            Char('z') => {
                if let Err(e) = self.open_viewer(&line) {
                    self.error(e);
                }
            }
            Char('f') => {
                self.select = None;
                if let Err(e) = self.start_forward(&line) {
                    self.error(e);
                }
            }
            Char('p') => {
                let pin = !self.win().pinned.contains(&event);
                match (self.win().perms, self.room_ctx()) {
                    (Some(perms), _) if !perms.can_pin => {
                        self.error("your power level doesn't allow pinning messages here")
                    }
                    (_, Ok((acct, room_id))) => acct.set_pinned(room_id, event, pin),
                    (_, Err(e)) => self.error(e),
                }
            }
            Char('v') => {
                if let Some(target) = self.win_mut().find_mut(&event) {
                    target.reveal = !target.reveal;
                }
            }
            Char('o') => match links::urls(&line.body).first() {
                Some(url) => self.open_url((*url).to_owned()),
                None => self.error("that message has no link"),
            },
            Char('c') => {
                self.clipboard = Some(line.body.clone());
                self.info(format!("copied {} characters to the clipboard", line.body.chars().count()));
            }
            Char('u') => match line.sender.clone() {
                Some(sender) => {
                    let member = self.win().members.iter().position(|m| m.user_id == sender);
                    // In the mentions window, the server is the message's own.
                    let account = self
                        .win()
                        .account()
                        .map(ToOwned::to_owned)
                        .or_else(|| self.mention_origins.get(&event).map(|k| k.account.clone()));
                    self.select = None;
                    match account {
                        Some(account) => self.open_profile_on(account, sender, line.nick.clone(), member),
                        None => self.error("can't tell which server that's from"),
                    }
                }
                None => self.error("that message has no sender to show"),
            },
            Char('i') => {
                let sender = line.sender.as_ref().map(|s| s.to_string()).unwrap_or_default();
                self.info(format!("{} [{sender}] at {}", line.nick, line.ts.format("%Y-%m-%d %H:%M:%S")));
                self.info(format!(" event {event}"));
                // An edited message: what it said before, and when.
                if !line.versions.is_empty() {
                    let times = if line.versions.len() == 1 {
                        "once".to_owned()
                    } else {
                        format!("{} times", line.versions.len())
                    };
                    self.info(format!(" edited {times}:"));
                    let when = |t: &chrono::DateTime<Local>| t.format("%Y-%m-%d %H:%M").to_string();
                    for (at, text) in &line.versions {
                        self.info(format!("  {} {}", when(at), snippet(text)));
                    }
                    let now = line.edited_at.unwrap_or(line.ts);
                    self.info(format!("  {} {} (now)", when(&now), snippet(&line.body)));
                }
            }
            _ => {}
        }
    }

    /// `<nick> “first words…”` of the message awaiting delete confirmation.
    pub fn delete_summary(&self) -> Option<String> {
        let id = self.confirm_delete.as_ref()?;
        let line = self.win().lines.iter().rev().find(|l| l.event_id.as_ref() == Some(id))?;
        Some(format!("<{}> “{}”", line.nick, snippet(&line.body)))
    }

    /// Ask y/n before deleting, after checking your power level allows it.
    pub(super) fn request_delete(&mut self, event: OwnedEventId, resume: Option<EditSession>) {
        let own = self.win().lines.iter().rev().find(|l| l.event_id.as_ref() == Some(&event)).is_some_and(|l| l.own);
        let allowed = self.win().perms.is_none_or(|p| if own { p.can_redact_own } else { p.can_redact_other });
        if !allowed {
            if let Some(session) = resume {
                self.input.set_text(&session.original);
                self.editing = Some(session);
            }
            return self.error(if own {
                "your power level doesn't allow deleting messages here"
            } else {
                "only moderators can delete other people's messages here"
            });
        }
        self.confirm_delete = Some(event);
        self.resume_edit = resume;
    }

    /// The message to highlight: selected, being edited, or about to be
    /// deleted.
    pub fn focus_event(&self) -> Option<OwnedEventId> {
        let here = self.win().room.as_ref();
        self.confirm_delete
            .clone()
            .or_else(|| self.select.clone())
            .or_else(|| self.editing.as_ref().filter(|s| Some(&s.key) == here).map(|s| s.event.clone()))
    }

    /// Dim text after the cursor: matching commands while typing a command
    /// name, then that command's usage once it's complete.
    pub fn input_hint(&self) -> Option<String> {
        if let Some(hint) = self.input.completion_hint() {
            return Some(format!("  {hint}"));
        }
        if self.editing.is_some() {
            return Some("  Enter saves · ↑/↓ your other messages · Esc cancels".into());
        }
        if self.compose_label().is_some() {
            return Some("Enter sends · Esc cancels".into());
        }
        let text = self.input.text();
        if let Some(hint) = eggs::clippy(&text) {
            return Some(format!("  {hint}"));
        }
        let typed = text.strip_prefix('/').filter(|t| !t.starts_with('/') && !t.contains('\n'))?;
        let (word, rest) = match typed.find(' ') {
            Some(i) => (&typed[..i], Some(&typed[i + 1..])),
            None => (typed, None),
        };
        let word = word.to_lowercase();
        let real = ALIASES.iter().find(|(a, _)| *a == word).map_or(word.as_str(), |(_, r)| *r);
        let real = match real {
            "wc" | "wl" => "window",
            other => other,
        };
        let exact = COMMANDS.iter().find(|c| c.name == real);
        match (exact, rest) {
            (Some(cmd), Some(rest)) if rest.trim().is_empty() => Some(format!("{} — {}", cmd.args, cmd.help)),
            (Some(_), Some(_)) => None,
            (Some(cmd), None) => Some(format!(" {} — {}", cmd.args, cmd.help)),
            (None, _) if let Some(expansion) = self.cfg.aliases.get(&word) => Some(format!("  → {expansion}")),
            (None, _)
                if let Some((_, help, script)) = self.scripts.commands().into_iter().find(|(n, _, _)| *n == word) =>
            {
                Some(format!("  {help} ({script}.rhai)"))
            }
            (None, None) => {
                let names: Vec<&str> =
                    COMMANDS.iter().map(|c| c.name).filter(|n| n.starts_with(word.as_str())).collect();
                (!names.is_empty()).then(|| format!("  {}", names.join(" ")))
            }
            (None, _) if EGGS.contains(&word.as_str()) => None,
            (None, Some(_)) => Some("  unknown command; /help lists them".into()),
        }
    }

    /// The prompt label while a reply/edit/reaction target is chosen.
    pub fn compose_label(&self) -> Option<String> {
        if self.pasted.is_some() && self.editing.is_none() {
            return Some("image caption".into());
        }
        if let Some(session) = &self.editing {
            let editable = self.editable();
            let pos = editable.iter().position(|l| l.event_id.as_ref() == Some(&session.event))?;
            let when = editable[pos].ts.format("%H:%M");
            return Some(format!("edit {when} {}/{}", editable.len() - pos, editable.len()));
        }
        let (key, compose) = self.compose.as_ref()?;
        if self.win().room.as_ref() != Some(key) {
            return None;
        }
        Some(match compose {
            Compose::Reply { nick, .. } => format!("reply to {nick}"),
        })
    }

    pub(super) fn candidates(&self) -> Vec<Candidate> {
        let (word, at_start) = self.input.completion_word();
        let lower = word.to_lowercase();

        if at_start && let Some(prefix) = lower.strip_prefix('/') {
            let script_names: Vec<String> = self.scripts.commands().into_iter().map(|(n, _, _)| n).collect();
            let mut names: Vec<&str> = COMMANDS
                .iter()
                .map(|c| c.name)
                .chain(ALIASES.iter().map(|a| a.0))
                .chain(["wc", "wl"])
                .chain(self.cfg.aliases.keys().map(String::as_str))
                .chain(script_names.iter().map(String::as_str))
                .collect();
            names.sort_unstable();
            return names
                .into_iter()
                .filter(|n| n.starts_with(prefix))
                .map(|n| Candidate { text: format!("/{n} "), user: None, hint: None })
                .collect();
        }

        let text = self.input.text();
        let first = text.split_whitespace().next().unwrap_or("");
        if SERVER_COMMANDS.iter().any(|c| first == format!("/{c}")) {
            return self
                .accounts
                .keys()
                .filter(|n| n.starts_with(&lower))
                .map(|n| Candidate { text: format!("{n} "), user: None, hint: None })
                .collect();
        }
        let second = text.split_whitespace().nth(1).unwrap_or("");
        if first == "/set" && text.split_whitespace().count() <= 2 && !at_start {
            let mut names: Vec<String> =
                self.effective_config().to_table().map(|t| t.keys().cloned().collect()).unwrap_or_default();
            names.retain(|n| n != "script_settings");
            names.extend(self.scripts.settings().into_iter().map(|(name, _, _)| name));
            names.push("log_dir".into());
            names.sort();
            return names
                .into_iter()
                .filter(|n| n.starts_with(&lower))
                .map(|n| Candidate { text: format!("{n} "), user: None, hint: None })
                .collect();
        }
        if first == "/upload" || (first == "/keys" && matches!(second, "export" | "import") && !at_start) {
            return path_candidates(&word);
        }
        if matches!(first, "/avatar" | "/roomavatar") && !at_start {
            return path_candidates(&word);
        }
        if first == "/theme" && !at_start {
            let words = text.split_whitespace().count();
            if words <= 1 || (words == 2 && !text.ends_with(' ')) {
                let yours = crate::theme::files(&self.paths.themes_dir());
                let names = ThemeName::ALL.iter().map(|(n, _)| *n).chain(yours.iter().map(String::as_str));
                return names
                    .chain(THEME_WORDS)
                    .filter(|n| n.starts_with(&lower))
                    .map(|n| Candidate { text: format!("{n} "), user: None, hint: None })
                    .collect();
            }
            return if second == "import" { path_candidates(&word) } else { Vec::new() };
        }

        if word.is_empty() {
            return Vec::new();
        }
        if let Some(prefix) = word.strip_prefix(':') {
            let prefix = prefix.trim_end_matches(':');
            // Your custom emoji first: they stay :shortcodes: until sent.
            let here = self.win().room.as_ref().map(|k| (k.account.clone(), k.room_id.clone()));
            let mut custom: Vec<(String, OwnedMxcUri)> =
                here.map(|(account, room)| self.emoji_for(&account, Some(&room))).unwrap_or_default();
            custom.retain(|(code, _)| !prefix.is_empty() && code.to_lowercase().starts_with(&prefix.to_lowercase()));
            custom.sort_by_key(|(code, _)| (code.len(), code.clone()));
            let custom = custom.into_iter().map(|(code, _)| Candidate {
                text: format!(":{code}:"),
                user: None,
                hint: Some("custom emoji".into()),
            });
            let unicode = crate::emoji::complete(prefix).into_iter().map(|(emoji, code)| Candidate {
                text: emoji.to_owned(),
                user: None,
                hint: Some(format!(":{code}:")),
            });
            return custom.chain(unicode).collect();
        }
        let suffix = if at_start { format!("{} ", self.cfg.completion_char) } else { " ".to_owned() };
        let win = self.win();
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        // Recent speakers first, like irssi.
        let speakers = win
            .lines
            .iter()
            .rev()
            .filter(|l| !l.own && l.is_message())
            .filter_map(|l| Some((l.nick.as_str(), l.sender.as_ref()?)));
        let members = win.members.iter().map(|m| (m.name.as_str(), &m.user_id));
        for (nick, user) in speakers.chain(members) {
            // Your nickname for someone finds them too, but what's inserted
            // is their own name: nicknames never leave Textrix.
            let matches = if word.starts_with('@') {
                user.as_str().starts_with(&word)
            } else {
                nick.to_lowercase().starts_with(&lower)
                    || self.layout.nicknames.get(user).is_some_and(|n| n.to_lowercase().starts_with(&lower))
            };
            if matches && seen.insert(user.clone()) {
                let text = if word.starts_with('@') { user.to_string() } else { nick.to_owned() };
                out.push(Candidate { text: format!("{text}{suffix}"), user: Some(user.clone()), hint: None });
            }
        }
        out
    }

    /// Ctrl+C on an empty input line: once asks, and again within two
    /// seconds quits (in the background, detaches, as closing the terminal
    /// would).
    /// A key from the terminal. In a busy moment a mouse report can come
    /// apart, its Escape read on its own and the rest read as typing
    /// (`[<65;247;37M` in the input line), so an Escape waits a moment for
    /// what follows: a report that comes together is the mouse event it
    /// was, and anything else is keys as usual.
    pub(super) fn on_key_in(&mut self, key: KeyEvent) {
        if let Some(mut hold) = self.esc_hold.take() {
            if let KeyCode::Char(c) = key.code
                && (key.modifiers - KeyModifiers::SHIFT).is_empty()
            {
                hold.text.push(c);
                match split_mouse(&hold.text) {
                    Ok(Some(mouse)) => return self.on_mouse(mouse),
                    Ok(None) => {
                        self.esc_hold = Some(hold);
                        return;
                    }
                    Err(()) => drop(hold.text.pop()),
                }
            }
            self.release_esc(hold);
        }
        if key.code == KeyCode::Esc && key.modifiers.is_empty() {
            self.esc_hold = Some(EscHold { at: Instant::now(), text: String::new() });
        } else {
            self.on_key(key);
        }
    }

    /// The held Escape, and what came after it, as the keys they were.
    pub(super) fn release_esc(&mut self, hold: EscHold) {
        self.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        for c in hold.text.chars() {
            self.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
    }

    /// Nothing more came: a held Escape was a key.
    pub(super) fn esc_timeout(&mut self) {
        if let Some(hold) = self.esc_hold.take() {
            self.release_esc(hold);
        }
    }

    pub(super) fn ctrl_c_quits(&mut self) {
        if self.quit_armed.is_some_and(|at| at.elapsed() < QUIT_WINDOW) {
            self.quit_armed = None;
            if !crate::remote::detach("detached; textrix --attach comes back to it") {
                self.quit = true;
            }
        } else {
            self.quit_armed = Some(Instant::now());
        }
    }

    /// The status bar's "press Ctrl+C again" while it's waiting.
    pub fn quit_prompt(&self) -> Option<&'static str> {
        self.quit_armed.filter(|at| at.elapsed() < QUIT_WINDOW).map(|_| match crate::term::remote_client() {
            Some(_) => "press Ctrl+C again to detach (Textrix keeps running; /quit stops it)",
            None => "press Ctrl+C again to quit",
        })
    }

    /// Ask whether to upload what's in the input line, if it's nothing but
    /// paths to files (as dropping files on the terminal leaves it).
    pub(super) fn offer_upload(&mut self) {
        let text = self.input.text();
        if self.win().room.is_none() || self.upload_declined.as_ref() == Some(&text) {
            return;
        }
        if let Some(files) = dropped_files(&text) {
            self.confirm_upload = Some(files);
        }
    }

    /// What the upload prompt asks about: "notes.pdf (1.2 MB)" or "3 files
    /// (4.5 MB)".
    pub fn upload_summary(&self) -> Option<String> {
        let files = self.confirm_upload.as_ref()?;
        let size: u64 = files.iter().filter_map(|p| std::fs::metadata(p).ok()).map(|m| m.len()).sum();
        let what = match files.as_slice() {
            [one] => one.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            many => format!("{} files", many.len()),
        };
        Some(format!("{what} ({})", matrix::human_size(size)))
    }

    pub(super) fn submit(&mut self) {
        // A pasted image goes with what's typed as its caption; a command
        // still runs, and the image waits.
        if self.pasted.is_some() && !self.input.text().trim_start().starts_with('/') {
            return self.send_pasted();
        }
        // Terminals that type a dropped path rather than paste it.
        self.offer_upload();
        if self.confirm_upload.is_some() {
            return;
        }
        let limit = self.cfg.paste_confirm_lines;
        if let Some(lines) = self.input.pasted_lines()
            && limit > 0
            && lines > limit
        {
            self.confirm_paste = Some(lines);
            return;
        }
        self.mentions = std::mem::take(&mut self.input.mentions);
        let text = self.input.take();
        self.stop_typing();
        if text.trim().is_empty() {
            return;
        }
        // Paths you chose not to upload go as a message, not a command.
        if self.upload_declined.take().is_some_and(|declined| declined == text) {
            self.say(&text, Say::Text);
            return;
        }
        if let Some((key, target)) = self.compose.take()
            && self.win().room.as_ref() == Some(&key)
            && !text.starts_with('/')
        {
            let result = match target {
                Compose::Reply { event, sender, .. } => self.send_reply(event, sender, &text),
            };
            if let Err(e) = result {
                self.error(e);
            }
            self.mentions.clear();
            return;
        }
        self.win_mut().scroll = 0;
        if let Some(rest) = text.strip_prefix("//") {
            self.say(&format!("/{rest}"), Say::Text);
        } else if let Some(cmd) = text.strip_prefix('/') {
            self.command(cmd);
        } else {
            self.say(&text, Say::Text);
        }
        self.mentions.clear();
    }
}
