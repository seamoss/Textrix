//! Windows and how they're laid out: opening, closing and ordering them, split panes, saved layouts.

use super::*;

impl App {
    /// The mentions window, created at the end of the list if needed (so
    /// window numbers don't shift).
    pub(super) fn mentions_window(&mut self) -> usize {
        if let Some(idx) = self.windows.iter().position(|w| w.is_mentions) {
            return idx;
        }
        let mut w = Window::new(None, "mentions".into());
        w.is_mentions = true;
        w.lines.push_back(Line::info(
            "Messages mentioning you or your highlight words, from every room. Click one, or select it (Alt+Up) \
             and press Enter, to go to it.",
        ));
        self.windows.push(w);
        self.windows.len() - 1
    }

    pub(super) fn upsert_room(&mut self, account: &str, meta: RoomMeta) {
        if meta.is_space {
            // Spaces are listed by /spaces and group the window list instead.
            if let Some(idx) = self.window_idx(account, &meta.room_id) {
                self.remove_window(idx);
            }
            return;
        }
        let idx = match self.window_idx(account, &meta.room_id) {
            Some(idx) => idx,
            None => {
                let key = WinKey { account: account.to_owned(), room_id: meta.room_id.clone(), thread: None };
                if self.layout.closed.contains(&key) {
                    return;
                }
                self.insert_window(Window::new(Some(key), meta.name.clone()))
            }
        };
        let level = self.windows[idx].update(&meta);
        let person = self.windows[idx].dm_user.as_ref().and_then(|u| self.layout.nicknames.get(u)).cloned();
        self.windows[idx].person = person;
        if !self.on_screen(idx) {
            let w = &mut self.windows[idx];
            w.activity = w.activity.max(level);
        }
    }

    /// Bring back a closed room window, e.g. when someone talks in it.
    pub(super) fn reopen(&mut self, account: &str, room_id: &OwnedRoomId) -> Option<usize> {
        let meta = self.connected(account)?.quick_meta(room_id)?;
        let key = WinKey { account: account.to_owned(), room_id: room_id.clone(), thread: None };
        self.layout.closed.retain(|k| *k != key);
        let idx = self.insert_window(Window::room(key, &meta));
        self.windows[idx].unread = (0, 0);
        Some(idx)
    }

    /// Insert a room window at its saved position (or at the end).
    pub(super) fn insert_window(&mut self, mut window: Window) -> usize {
        if let Some(key) = &window.room {
            window.local_name = self.layout.names.iter().find(|(k, _)| k == key).map(|(_, n)| n.clone());
        }
        window.person = window.dm_user.as_ref().and_then(|u| self.layout.nicknames.get(u)).cloned();
        let rank = |k: &WinKey| self.layout.windows.iter().position(|x| x == k);
        let key = window.room.as_ref().expect("room window");
        let pos = match rank(key) {
            Some(r) => self
                .windows
                .iter()
                .position(|w| w.room.as_ref().is_some_and(|k| rank(k).is_none_or(|kr| kr > r)))
                .unwrap_or(self.windows.len()),
            None => self.windows.len(),
        };
        self.windows.insert(pos, window);
        if pos <= self.active {
            self.active += 1;
        }
        pos
    }

    pub(super) fn remove_window(&mut self, idx: usize) {
        if idx == 0 {
            return;
        }
        let removed = self.windows.remove(idx).id();
        if self.active >= idx && self.active > 0 {
            self.active -= 1;
        }
        // Its pane goes too, unless focus has to land somewhere new.
        let active = self.windows[self.active].id();
        if let Some(split) = &mut self.split
            && let Some(slot) = split.panes.iter().position(|k| *k == removed)
        {
            if split.panes.contains(&active) {
                split.remove(slot);
            } else {
                split.panes[slot] = active;
            }
        }
        if self.split.as_ref().is_some_and(|s| s.panes.len() < 2) {
            self.split = None;
        }
    }

    pub(super) fn save_layout(&self) {
        // Thread windows are opened on demand and not restored.
        let mut windows: Vec<WinKey> =
            self.windows.iter().filter_map(|w| w.room.clone()).filter(|k| k.thread.is_none()).collect();
        // Keep saved positions for servers that are not connected right now.
        for key in &self.layout.windows {
            if !windows.contains(key) && self.connected(&key.account).is_none() {
                windows.push(key.clone());
            }
        }
        let split = match &self.pending_split {
            // Never restored (its server didn't connect): keep it for next time.
            Some(pending) => Some(pending.clone()),
            None => self.split.is_some().then(|| self.current_layout(false)),
        };
        let layout = Layout {
            windows,
            closed: self.layout.closed.clone(),
            names: self.layout.names.clone(),
            nicknames: self.layout.nicknames.clone(),
            split,
            layouts: self.layout.layouts.clone(),
            themes: self.layout.themes.clone(),
            recent_emoji: self.layout.recent_emoji.clone(),
        };
        if let Ok(json) = serde_json::to_vec_pretty(&layout)
            && let Err(e) = std::fs::write(self.paths.layout_file(), json)
        {
            tracing::warn!("saving layout: {e}");
        }
    }

    pub(super) fn names_lines(&self, idx: usize) -> Vec<Line> {
        let w = &self.windows[idx];
        let mut lines = vec![Line::info(format!("Users {} ({})", w.title(), w.members.len()))];
        let col = w.members.iter().map(|m| m.name.chars().count()).max().unwrap_or(0).clamp(4, 20);
        let per_row = ((self.view_width as usize).saturating_sub(10) / (col + 4)).max(1);
        for chunk in w.members.chunks(per_row) {
            let row: Vec<String> = chunk
                .iter()
                .map(|m| {
                    let name: String = m.name.chars().take(col).collect();
                    format!("[{}{name:<col$}]", m.prefix)
                })
                .collect();
            lines.push(Line::info(row.join(" ")));
        }
        lines
    }

    pub(super) fn switch_to(&mut self, idx: usize) {
        if idx >= self.windows.len() || idx == self.active {
            return;
        }
        self.stop_typing();
        self.select = None;
        self.confirm_delete = None;
        self.resume_edit = None;
        self.cancel_edit();
        // Unsent text (and a reply or reaction being written) stays with
        // the window you leave, and comes back when you return.
        let unsent = Unsent { text: self.input.stash(), compose: self.compose.take().map(|(_, c)| c) };
        let left_key = self.windows[self.active].id();
        self.recent.retain(|k| *k != left_key);
        self.recent.insert(0, left_key);
        self.recent.truncate(64);
        let left = &mut self.windows[self.active];
        left.unsent = (unsent.text.is_some() || unsent.compose.is_some()).then_some(unsent);
        // Everything in the window you leave counts as seen.
        if let Some(latest) = left.latest_event() {
            left.marker = Some(latest);
        }
        // A window already in a pane gets the focus; otherwise it replaces
        // the focused pane's window.
        let to = self.windows[idx].id();
        if let Some(split) = &mut self.split
            && !split.panes.contains(&to)
        {
            let from = self.windows[self.active].id();
            match split.panes.iter().position(|k| *k == from) {
                Some(slot) => split.panes[slot] = to,
                None => split.insert(split.panes.len(), to),
            }
        }
        self.active = idx;
        self.windows[idx].activity = Activity::None;
        if let Some(unsent) = self.windows[idx].unsent.take() {
            if let Some(text) = unsent.text {
                self.input.restore(text);
            }
            if let (Some(compose), Some(key)) = (unsent.compose, self.windows[idx].room.clone()) {
                self.compose = Some((key, compose));
            }
        }
    }

    /// Windows on screen, in pane order (just the active one unless split).
    pub fn screen_windows(&self) -> Vec<usize> {
        let Some(split) = &self.split else { return vec![self.active] };
        split.panes.iter().filter_map(|key| self.windows.iter().position(|w| w.id() == *key)).collect()
    }

    /// F6 / Shift+F6: focus the next or previous pane.
    pub(super) fn cycle_pane(&mut self, step: isize) {
        let panes = self.screen_windows();
        if panes.len() < 2 {
            return;
        }
        let pos = panes.iter().position(|&i| i == self.active).unwrap_or(0) as isize;
        self.switch_to(panes[(pos + step).rem_euclid(panes.len() as isize) as usize]);
    }

    pub(super) fn cycle_window(&mut self, step: isize) {
        let n = self.windows.len() as isize;
        self.switch_to((self.active as isize + step).rem_euclid(n) as usize);
    }

    /// Alt+A: jump to the most important window with activity.
    pub(super) fn next_active(&mut self) {
        let best = self.windows.iter().map(|w| w.activity).max().unwrap_or_default();
        if best == Activity::None {
            return;
        }
        let n = self.windows.len();
        if let Some(idx) = (1..=n).map(|step| (self.active + step) % n).find(|&i| self.windows[i].activity == best) {
            self.switch_to(idx);
        }
    }

    pub(super) fn cmd_window(&mut self, args: &str) -> Result<(), String> {
        let (sub, rest) = split_first(args);
        match sub {
            "" | "list" => {
                let rows: Vec<String> = self
                    .windows
                    .iter()
                    .enumerate()
                    .map(|(i, w)| {
                        let server = w.account().map(|a| format!(" ({a})")).unwrap_or_default();
                        let enc = if w.encrypted { " [E2E]" } else { "" };
                        let real = if w.local_name.is_some() { format!(" ({})", w.name) } else { String::new() };
                        format!("{:>3}: {}{real}{server}{enc}", i + 1, w.title())
                    })
                    .collect();
                for row in rows {
                    self.info(row);
                }
                let closed = self.layout.closed.len();
                if closed > 0 {
                    self.info(format!("{closed} closed window(s) reopen when someone talks; /rooms lists all"));
                }
            }
            "close" => {
                if self.close_thread() {
                    return Ok(());
                }
                if self.win().is_mentions {
                    self.mention_origins.clear();
                    self.remove_window(self.active);
                    return Ok(());
                }
                let Some(key) = self.win().room.clone() else {
                    return Err("the status window can't be closed".into());
                };
                if key.thread.is_none() && !self.layout.closed.contains(&key) {
                    self.layout.closed.push(key);
                }
                self.remove_window(self.active);
                self.save_layout();
            }
            "move" => {
                let n: usize = rest.trim().parse().map_err(|_| "usage: /window move <n>")?;
                if self.active == 0 {
                    return Err("the status window stays at 1".into());
                }
                let target = n.clamp(2, self.windows.len()) - 1;
                let w = self.windows.remove(self.active);
                self.windows.insert(target, w);
                self.active = target;
                self.save_layout();
            }
            "goto" => self.goto_name(rest.trim())?,
            "name" => {
                let Some(key) = self.win().room.clone() else {
                    return Err("only room and conversation windows can be renamed".into());
                };
                let name: String = rest.trim().chars().filter(|c| !c.is_control()).take(64).collect();
                self.layout.names.retain(|(k, _)| *k != key);
                let text = if name.is_empty() {
                    self.win_mut().local_name = None;
                    format!("back to the room's own name, {}", self.win().name)
                } else {
                    self.layout.names.push((key, name.clone()));
                    let text = format!(
                        "{} is now {name} here (only you see this; /window name puts it back)",
                        self.win().name
                    );
                    self.win_mut().local_name = Some(name);
                    text
                };
                self.save_layout();
                self.info(text);
            }
            n if n.parse::<usize>().is_ok() => {
                let n: usize = n.parse().unwrap_or(1);
                if n == 0 || n > self.windows.len() {
                    return Err(format!("no window {n}"));
                }
                self.switch_to(n - 1);
            }
            name => self.goto_name(name)?,
        }
        Ok(())
    }

    pub(super) fn find_window(&self, name: &str) -> Result<usize, String> {
        if let Ok(n) = name.parse::<usize>() {
            return (1..=self.windows.len()).contains(&n).then(|| n - 1).ok_or_else(|| format!("no window {n}"));
        }
        let wanted = name.to_lowercase();
        self.windows
            .iter()
            .position(|w| w.names().any(|n| n.to_lowercase() == wanted))
            .or_else(|| self.windows.iter().position(|w| w.names().any(|n| n.to_lowercase().contains(&wanted))))
            .ok_or_else(|| format!("no window matching {name}"))
    }

    pub(super) fn cmd_split(&mut self, args: &str, side_by_side: bool) -> Result<(), String> {
        let target = args.trim();
        if target.is_empty() {
            let Some(split) = &mut self.split else {
                let name = if side_by_side { "vsplit" } else { "split" };
                return Err(format!("usage: /{name} <window number or name>"));
            };
            split.side_by_side = side_by_side;
            return Ok(());
        }
        let idx = self.find_window(target)?;
        if self.on_screen(idx) {
            return Err(format!("{} is already on screen", self.windows[idx].title()));
        }
        let current = self.windows[self.active].id();
        let split = self.split.get_or_insert_with(|| Split::new(vec![current.clone()], side_by_side));
        if split.panes.len() >= PANES_MAX {
            return Err(format!("{PANES_MAX} panes at most; /unsplit closes this one"));
        }
        split.side_by_side = side_by_side;
        let slot = split.panes.iter().position(|k| *k == current).map_or(split.panes.len(), |i| i + 1);
        split.insert(slot, self.windows[idx].id());
        self.windows[idx].activity = Activity::None;
        Ok(())
    }

    /// What's on screen, as a layout to save.
    pub(super) fn current_layout(&self, panels: bool) -> SavedLayout {
        SavedLayout {
            panes: self.split.as_ref().map(|s| s.panes.clone()).unwrap_or_default(),
            side_by_side: self.split.as_ref().is_some_and(|s| s.side_by_side),
            focus: Some(self.windows[self.active].id()),
            sizes: self.split.as_ref().map(Split::weights).unwrap_or_default(),
            sidebar: panels.then_some(self.show_sidebar),
            nicklist: panels.then_some(self.show_nicklist),
        }
    }

    /// Put a layout on screen: its panes whose windows are open, its focus
    /// and its panels. Returns how many panes couldn't be shown.
    pub(super) fn apply_layout(&mut self, saved: &SavedLayout) -> usize {
        let open = |app: &Self, r: &WinRef| app.windows.iter().position(|w| w.id() == *r);
        let present: Vec<WinRef> = saved.panes.iter().filter(|r| open(self, r).is_some()).cloned().collect();
        // Sizes for the panes that are there (if they were saved).
        let sizes: Vec<u16> = match saved.sizes.len() == saved.panes.len() {
            true => {
                saved.panes.iter().zip(&saved.sizes).filter(|(r, _)| open(self, r).is_some()).map(|(_, s)| *s).collect()
            }
            false => vec![PANE_SHARE; present.len()],
        };
        let missing = saved.panes.len() - present.len();
        self.show_sidebar = saved.sidebar.unwrap_or(self.show_sidebar);
        self.show_nicklist = saved.nicklist.unwrap_or(self.show_nicklist);
        self.split = None;
        if let Some(idx) = saved.focus.as_ref().and_then(|f| open(self, f)) {
            self.switch_to(idx);
        }
        if present.len() >= 2 {
            let first = present[0].clone();
            self.split = Some(Split { panes: present, side_by_side: saved.side_by_side, sizes });
            // The focus is always one of the panes.
            if !self.split.as_ref().is_some_and(|s| s.panes.contains(&self.windows[self.active].id()))
                && let Some(idx) = open(self, &first)
            {
                self.switch_to(idx);
            }
        } else if let Some(idx) = present.first().and_then(|r| open(self, r)) {
            self.switch_to(idx);
        }
        missing
    }

    /// Last session's split, once its windows are open (or their servers
    /// have said which rooms they have: a room you've left is skipped).
    pub(super) fn restore_split(&mut self) {
        self.resume();
        let Some(saved) = &self.pending_split else { return };
        let waiting = saved.panes.iter().chain(&saved.focus).any(|r| match r {
            WinRef::Room(key) => {
                !self.windows.iter().any(|w| w.id() == *r)
                    && !self.rooms_loaded.contains(&key.account)
                    && self.accounts.get(key.account.as_str()).is_some_and(|s| s.status != ConnStatus::Offline)
            }
            _ => false,
        });
        if waiting {
            return;
        }
        if let Some(saved) = self.pending_split.take() {
            self.apply_layout(&saved);
        }
    }

    /// `/layout [save|load|delete <name>]`: named arrangements of split
    /// panes and panels.
    pub(super) fn cmd_layout(&mut self, args: &str) -> Result<(), String> {
        let (sub, name) = split_first(args.trim());
        let name = name.trim();
        match (sub, name) {
            ("" | "list", _) => {
                if self.layout.layouts.is_empty() {
                    self.info("no saved layouts; /layout save <name> keeps the panes and panels you have now");
                }
                let rows: Vec<String> = self
                    .layout
                    .layouts
                    .iter()
                    .map(|(name, l)| {
                        let panes: Vec<String> = l.panes.iter().map(|r| self.ref_title(r)).collect();
                        let panes = if panes.is_empty() { "one window".to_owned() } else { panes.join(" | ") };
                        format!("  {name}: {panes}{}", if l.side_by_side { " (side by side)" } else { "" })
                    })
                    .collect();
                rows.into_iter().for_each(|row| self.info(row));
            }
            ("save", name) if !name.is_empty() => {
                self.layout.layouts.insert(name.to_owned(), self.current_layout(true));
                self.save_layout();
                self.info(format!("layout {name} saved; /layout load {name} brings it back"));
            }
            ("load", name) if !name.is_empty() => {
                let saved = self.layout.layouts.get(name).cloned().ok_or_else(|| format!("no layout {name}"))?;
                self.pending_split = None;
                match self.apply_layout(&saved) {
                    0 => {}
                    n => self.info(format!("{n} of its windows aren't open, so they were left out")),
                }
            }
            ("delete", name) if !name.is_empty() => {
                self.layout.layouts.remove(name).ok_or_else(|| format!("no layout {name}"))?;
                self.save_layout();
                self.info(format!("layout {name} deleted"));
            }
            (name, "") if self.layout.layouts.contains_key(name) => return self.cmd_layout(&format!("load {name}")),
            _ => return Err("usage: /layout [<name> | save <name> | load <name> | delete <name>]".into()),
        }
        Ok(())
    }

    /// A window's name, by identity.
    pub(super) fn ref_title(&self, r: &WinRef) -> String {
        match self.windows.iter().find(|w| w.id() == *r) {
            Some(w) => w.title().to_owned(),
            None => match r {
                WinRef::Status => "status".into(),
                WinRef::Mentions => "mentions".into(),
                WinRef::Room(key) => key.room_id.to_string(),
            },
        }
    }

    /// `/resize [+N | -N | =]`: grow or shrink the focused pane by N rows
    /// (columns side by side), or make them all the same.
    pub(super) fn cmd_resize(&mut self, args: &str) -> Result<(), String> {
        const USAGE: &str = "usage: /resize +N | -N | = (the focused pane, in rows or columns)";
        let Some(split) = &mut self.split else { return Err("nothing is split; /split <window> adds a pane".into()) };
        let arg = args.trim();
        if arg == "=" {
            split.sizes = vec![PANE_SHARE; split.panes.len()];
            return Ok(());
        }
        let delta: i32 = arg.strip_prefix('+').unwrap_or(arg).parse().map_err(|_| USAGE)?;
        let active = self.windows[self.active].id();
        let slot = split.panes.iter().position(|k| *k == active).ok_or(USAGE)?;
        let k = self.hits.splits.iter().position(|(_, s)| *s == slot).ok_or("the panes aren't drawn yet")?;
        // Against the pane after it, or the one before for the last.
        let (a, b) = if k + 1 < self.hits.splits.len() { (k, k + 1) } else { (k.saturating_sub(1), k) };
        let cells = |r: Rect| if split.side_by_side { r.width } else { r.height };
        let (ca, cb) = (cells(self.hits.splits[a].0), cells(self.hits.splits[b].0));
        let grown = i32::from(if a == k { ca } else { cb }) + delta;
        let new_k = self.clamp_pane(grown, ca + cb);
        let new_a = if a == k { new_k } else { ca + cb - new_k };
        self.set_divider(a, new_a);
        Ok(())
    }

    /// A pane's size kept between the smallest a pane can be and what
    /// leaves the other pane its smallest.
    pub(super) fn clamp_pane(&self, cells: i32, pair: u16) -> u16 {
        let side = self.split.as_ref().is_some_and(|s| s.side_by_side);
        let min = if side { PANE_MIN.1 } else { PANE_MIN.0 };
        (cells.max(i32::from(min)) as u16).min(pair.saturating_sub(min).max(min))
    }

    /// Give drawn pane `a` (of `hits.splits`) `cells`, and the one after it
    /// the rest of what the two had.
    pub(super) fn set_divider(&mut self, a: usize, cells: u16) {
        let Some(split) = &mut self.split else { return };
        if a + 1 >= self.hits.splits.len() {
            return;
        }
        let size = |r: Rect| if split.side_by_side { r.width } else { r.height };
        let pair = size(self.hits.splits[a].0) + size(self.hits.splits[a + 1].0);
        let mut sizes = split.weights();
        // Shares in cells, as drawn, so the two change and the rest stay.
        for (area, slot) in &self.hits.splits {
            if let Some(s) = sizes.get_mut(*slot) {
                *s = size(*area).max(1);
            }
        }
        let (sa, sb) = (self.hits.splits[a].1, self.hits.splits[a + 1].1);
        if let (true, true) = (sa < sizes.len(), sb < sizes.len()) {
            sizes[sa] = cells.max(1);
            sizes[sb] = pair.saturating_sub(cells).max(1);
        }
        split.sizes = sizes;
    }

    /// The divider (between drawn panes `k` and `k + 1`) at `at`, if any:
    /// the line or bar between them.
    pub(super) fn divider_at(&self, at: Position) -> Option<usize> {
        let side = self.split.as_ref()?.side_by_side;
        // Framed panes meet at two frame lines; plain ones at the line on
        // the right-hand pane's edge, or the upper pane's title bar.
        let framed = self.cfg.palette.outlines.framed();
        self.hits.splits.windows(2).position(|pair| {
            let (a, b) = (pair[0].0, pair[1].0);
            if side {
                let from = if framed { a.right().saturating_sub(1) } else { b.x };
                (from..=b.x).contains(&at.x) && (a.y..a.bottom()).contains(&at.y)
            } else {
                let to = if framed { b.y } else { a.bottom().saturating_sub(1) };
                (a.bottom().saturating_sub(1)..=to).contains(&at.y) && (a.x..a.right()).contains(&at.x)
            }
        })
    }

    pub(super) fn cmd_unsplit(&mut self, args: &str) -> Result<(), String> {
        let Some(split) = &mut self.split else { return Err("nothing is split; /split <window> adds a pane".into()) };
        if args.trim() == "all" {
            self.split = None;
            return Ok(());
        }
        let current = self.windows[self.active].id();
        let slot = split.panes.iter().position(|k| *k == current).unwrap_or(0);
        split.remove(slot);
        let next = split.panes[slot.min(split.panes.len() - 1)].clone();
        if split.panes.len() < 2 {
            self.split = None;
        }
        if let Some(idx) = self.windows.iter().position(|w| w.id() == next) {
            self.switch_to(idx);
        }
        Ok(())
    }

    pub(super) fn goto_name(&mut self, name: &str) -> Result<(), String> {
        let wanted = name.to_lowercase();
        let idx = self
            .windows
            .iter()
            .position(|w| w.names().any(|n| n.to_lowercase() == wanted))
            .or_else(|| self.windows.iter().position(|w| w.names().any(|n| n.to_lowercase().contains(&wanted))))
            .ok_or_else(|| format!("no window matching {name}"))?;
        self.switch_to(idx);
        Ok(())
    }
}
