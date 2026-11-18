//! Panels over the windows: the quick switcher, the room browser (/list), the media gallery and links, home, the image viewer, someone's profile, the emoji picker, a poll's vote and verification.

use super::*;

impl App {
    /// Open the profile pane on someone in this room's member list.
    pub(super) fn open_member(&mut self, n: usize) {
        let Some(m) = self.win().members.get(n) else { return };
        let (user, name) = (m.user_id.clone(), m.name.clone());
        self.open_profile(user, name, Some(n));
    }

    /// The profile pane, on `user` (on this window's server).
    pub fn open_profile(&mut self, user: OwnedUserId, name: String, member: Option<usize>) {
        let Some(account) = self.win().account().map(ToOwned::to_owned) else { return };
        self.open_profile_on(account, user, name, member);
    }

    /// The profile pane, on `user` on `account`.
    pub(super) fn open_profile_on(&mut self, account: String, user: OwnedUserId, name: String, member: Option<usize>) {
        // Opened afresh unless it's already them (moving through the list
        // keeps what's loaded).
        if self.profile.as_ref().is_some_and(|p| p.user == user) {
            if let Some(p) = self.profile.as_mut() {
                p.member = member;
            }
            return;
        }
        if let Some(acct) = self.connected(&account) {
            acct.user_info(user.clone());
        }
        let info = crate::matrix::UserInfo::default();
        self.profile = Some(ProfileView { account, user, name, info, picture: None, member });
    }

    pub(super) fn on_profile_key(&mut self, key: KeyEvent) {
        use KeyCode::*;
        let Some(p) = self.profile.as_ref() else { return };
        let (user, name, member) = (p.user.clone(), p.name.clone(), p.member);
        let count = self.win().members.len();
        match key.code {
            Esc | Char('q') => self.profile = None,
            Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.profile = None,
            Up | Char('k') if count > 0 => self.open_member(member.map_or(0, |n| n.saturating_sub(1))),
            Down | Char('j') if count > 0 => self.open_member(member.map_or(0, |n| (n + 1).min(count - 1))),
            Char('m') => {
                self.profile = None;
                self.command(&format!("query {user}"));
            }
            Char('@') => {
                self.profile = None;
                self.mention_user(name, user);
            }
            // These change things, so they wait in the input line for Enter.
            Char('n') => self.prefill(&format!("/nickname {user} ")),
            Char('i') => self.prefill(&format!("/ignore {user}")),
            Char('v') => self.prefill(&format!("/verify {user}")),
            _ => {}
        }
    }

    /// Switcher entries for what's typed: every window, and closed rooms.
    /// With nothing typed the current window is left out, so Enter goes
    /// back to the previous one.
    pub fn switcher_entries(&self) -> Vec<switcher::Entry> {
        let Some(sw) = &self.switcher else { return Vec::new() };
        // Forwarding lists the other rooms on the message's server.
        let forward_from = match &sw.purpose {
            switcher::Purpose::Forward { account, room_id, .. } => Some((account.as_str(), room_id)),
            switcher::Purpose::Go => None,
        };
        let wanted = |key: Option<&WinKey>| match (forward_from, key) {
            (None, _) => true,
            (Some((account, from)), Some(k)) => k.account == account && k.room_id != *from && k.thread.is_none(),
            (Some(_), None) => false,
        };
        let several = self.accounts.len() > 1;
        let recency = |key: &WinRef| self.recent.iter().position(|k| k == key).unwrap_or(usize::MAX);
        let mut candidates = Vec::new();
        for (i, w) in self.windows.iter().enumerate() {
            if (i == self.active && sw.query.trim().is_empty()) || !wanted(w.room.as_ref()) {
                continue;
            }
            let detail = match w.account() {
                Some(account) if several => account.to_owned(),
                _ => String::new(),
            };
            candidates.push(switcher::Candidate {
                entry: switcher::Entry {
                    target: switcher::Target::Window(i),
                    name: w.title().to_owned(),
                    detail: {
                        let detail = match (&w.local_name, detail.is_empty()) {
                            (Some(_), true) => w.name.clone(),
                            (Some(_), false) => format!("{} · {detail}", w.name),
                            (None, _) => detail,
                        };
                        match (w.favourite, detail.is_empty()) {
                            (true, true) => "★".to_owned(),
                            (true, false) => format!("★ {detail}"),
                            (false, _) => detail,
                        }
                    },
                    activity: w.activity,
                    unread: w.unread(),
                    matched: Vec::new(),
                    aka: w.local_name.as_ref().map(|_| w.name.clone()),
                },
                number: Some(i + 1),
                recency: recency(&w.id()),
            });
        }
        for key in self.layout.closed.iter().filter(|k| k.thread.is_none() && wanted(Some(k))) {
            let Some(meta) = self.connected(&key.account).and_then(|a| a.quick_meta(&key.room_id)) else { continue };
            let detail = if several { format!("{} · closed", key.account) } else { "closed".to_owned() };
            candidates.push(switcher::Candidate {
                entry: switcher::Entry {
                    target: switcher::Target::Closed { account: key.account.clone(), room_id: key.room_id.clone() },
                    name: meta.name,
                    detail,
                    activity: Activity::None,
                    unread: meta.unread,
                    matched: Vec::new(),
                    aka: None,
                },
                number: None,
                recency: usize::MAX,
            });
        }
        switcher::rank(&sw.query, candidates)
    }

    /// `/list [-account] [server | search]`: the room browser, on your
    /// server's directory (or `server`'s), then your spaces with Tab.
    pub(super) fn open_directory(&mut self, args: &str) -> Result<(), String> {
        use crate::directory::{Directory, Source};
        let (acct, rest) = self.pick_account(args)?;
        let rest = rest.trim();
        let (server, query) = if looks_like_server(rest) { (Some(rest.to_owned()), "") } else { (None, rest) };
        let mut sources = vec![Source::Server(server)];
        if let Some(spaces) = self.spaces.get(&*acct.name) {
            sources.extend(spaces.iter().map(|s| Source::Space { id: s.room_id.clone(), name: s.name.clone() }));
        }
        self.directory = Some(Directory::new(acct.name.to_string(), sources, query.to_owned()));
        Ok(())
    }

    /// When the room browser asks next (typing waits for a pause).
    pub fn directory_due(&self) -> Option<Instant> {
        self.directory.as_ref().and_then(|d| d.due)
    }

    /// Ask for what the room browser wants now.
    pub fn fetch_directory(&mut self) {
        use crate::directory::Fetch;
        let Some(d) = self.directory.as_mut() else { return };
        let (account, at) = (d.account.clone(), d.at);
        let Some(fetch) = d.fetch() else { return };
        let Some(acct) = self.connected(&account) else {
            if let Some(d) = self.directory.as_mut() {
                (d.loading, d.error) = (false, Some(format!("{account} isn't connected")));
            }
            return;
        };
        match fetch {
            Fetch::Server { server, search, since } => acct.browse_directory(at, server, search, since),
            Fetch::Space { id, from } => acct.browse_space(at, id, from),
        }
    }

    pub(super) fn on_directory_key(&mut self, key: KeyEvent) {
        use KeyCode::*;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let Some(d) = self.directory.as_mut() else { return };
        match key.code {
            Esc => self.directory = None,
            Char('c') if ctrl => self.directory = None,
            Enter => self.directory_join(),
            Up => d.select(-1),
            Down => d.select(1),
            Char('p') if ctrl => d.select(-1),
            Char('n') if ctrl => d.select(1),
            PageUp => d.select(-10),
            PageDown => d.select(10),
            Tab => d.switch(false),
            BackTab => d.switch(true),
            Backspace => {
                d.query.pop();
                d.typed();
            }
            Char('u') if ctrl => {
                d.query.clear();
                d.typed();
            }
            Char(c) if !ctrl && !alt => {
                d.query.push(c);
                d.typed();
            }
            _ => {}
        }
    }

    /// A row chooses it, again joins it; outside the panel closes it.
    pub(super) fn on_directory_click(&mut self, at: Position) {
        let hits = self.hits.directory.as_ref();
        let row = hits.and_then(|(_, rows)| rows.iter().find(|(r, _)| r.contains(at))).map(|&(_, n)| n);
        let inside = hits.is_some_and(|(area, _)| area.contains(at));
        match (row, self.directory.as_mut()) {
            (Some(n), Some(d)) if d.selected == n => self.directory_join(),
            (Some(n), Some(d)) => d.selected = n,
            (None, _) if !inside => self.directory = None,
            _ => {}
        }
    }

    /// Enter in the room browser: join the chosen room (or go to it, if
    /// you're in it), and close.
    pub(super) fn directory_join(&mut self) {
        use crate::directory::Source;
        let Some(d) = self.directory.as_ref() else { return };
        let Some(room) = d.chosen().cloned() else { return };
        let account = d.account.clone();
        // A room known only by its ID on another server is reached through it.
        let mut via: Vec<matrix_sdk::ruma::OwnedServerName> = match d.source() {
            Source::Server(Some(server)) => {
                matrix_sdk::ruma::OwnedServerName::try_from(server.as_str()).into_iter().collect()
            }
            Source::Space { id, .. } => id.server_name().map(ToOwned::to_owned).into_iter().collect(),
            Source::Server(None) => Vec::new(),
        };
        via.extend(room.room_id.server_name().map(ToOwned::to_owned));
        via.dedup();
        self.directory = None;
        if room.joined {
            match self.window_idx(&account, &room.room_id).or_else(|| self.reopen(&account, &room.room_id)) {
                Some(idx) => self.switch_to(idx),
                None => self.info(format!("you're in {} already", room.name)),
            }
            return;
        }
        let Some(acct) = self.connected(&account) else {
            return self.error(format!("{account} isn't connected"));
        };
        let target = room.alias.clone().unwrap_or_else(|| room.room_id.to_string());
        acct.join_via(target, via);
        self.info(format!("joining {}…", room.name));
    }

    pub(super) fn on_switcher_key(&mut self, key: KeyEvent) {
        use KeyCode::*;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(sw) = self.switcher.as_mut() else { return };
        match key.code {
            Esc => self.switcher = None,
            Char('c' | 'k') if ctrl => self.switcher = None,
            Enter => self.switcher_go(None),
            Up | BackTab => sw.selected = sw.selected.saturating_sub(1),
            Char('p') if ctrl => sw.selected = sw.selected.saturating_sub(1),
            Down | Tab => sw.selected += 1,
            Char('n') if ctrl => sw.selected += 1,
            Backspace => {
                sw.query.pop();
                sw.selected = 0;
            }
            Char('u') if ctrl => {
                sw.query.clear();
                sw.selected = 0;
            }
            Char('f') if ctrl && matches!(sw.purpose, switcher::Purpose::Go) => self.switcher_favourite(),
            Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                sw.query.push(c);
                sw.selected = 0;
            }
            _ => {}
        }
    }

    /// Ctrl+F in the switcher: the chosen room or DM a favourite, or not.
    pub(super) fn switcher_favourite(&mut self) {
        let entries = self.switcher_entries();
        let Some(entry) = self.switcher.as_ref().and_then(|sw| entries.get(sw.selected)) else { return };
        let (account, room_id) = match &entry.target {
            switcher::Target::Window(i) => match &self.windows[*i].room {
                Some(key) => (key.account.clone(), key.room_id.clone()),
                None => return,
            },
            switcher::Target::Closed { account, room_id } => (account.clone(), room_id.clone()),
        };
        if let Err(e) = self.toggle_favourite(&account, &room_id) {
            self.error(e);
        }
    }

    /// Go to the chosen switcher entry (or entry `pick`) and close it.
    pub(super) fn switcher_go(&mut self, pick: Option<usize>) {
        let entries = self.switcher_entries();
        let Some(sw) = self.switcher.take() else { return };
        let Some(entry) = entries.get(pick.unwrap_or(sw.selected).min(entries.len().saturating_sub(1))) else {
            return;
        };
        if let switcher::Purpose::Forward { account, room_id, event, edited } = sw.purpose {
            let to = match &entry.target {
                switcher::Target::Window(idx) => self.windows[*idx].room.as_ref().map(|k| k.room_id.clone()),
                switcher::Target::Closed { room_id, .. } => Some(room_id.clone()),
            };
            match (self.connected(&account), to) {
                (Some(acct), Some(to)) => acct.forward(room_id, event, edited, to, entry.name.clone()),
                (None, _) => self.error(format!("{account} is not connected")),
                (_, None) => {}
            }
            return;
        }
        match entry.target.clone() {
            switcher::Target::Window(idx) => self.switch_to(idx),
            switcher::Target::Closed { account, room_id } => match self.reopen(&account, &room_id) {
                Some(idx) => self.switch_to(idx),
                None => self.error(format!("{} isn't available right now", entry.name)),
            },
        }
    }

    /// The poll on the selected message, if it starts one.
    pub fn selected_poll(&self) -> Option<&Poll> {
        self.selected_line()?.poll.as_deref()
    }

    /// Enter on a poll: its answers to vote from, your vote ticked.
    pub(super) fn open_vote(&mut self, line: &Line) {
        let (Some(poll), Some(key), Some(id)) = (line.poll.as_deref(), self.win().room.clone(), line.event_id.clone())
        else {
            return;
        };
        if poll.ended.is_some() {
            return self.info("that poll has ended");
        }
        let mine = poll.mine();
        let picked =
            poll.answers.iter().enumerate().filter(|(_, (a, _))| mine.contains(&a.as_str())).map(|(n, _)| n).collect();
        self.vote = Some(VotePicker {
            question: poll.question.clone(),
            answers: poll.answers.clone(),
            max: poll.max.max(1),
            picked,
            selected: 0,
            key: WinKey { thread: None, ..key },
            poll: id,
            stable: poll.stable,
        });
    }

    /// The verification panel: one key for each step.
    pub(super) fn on_verify_key(&mut self, key: KeyEvent) {
        use KeyCode::*;
        let Some(panel) = self.verify_panel.as_mut() else { return };
        let acct = self.accounts.get(&*panel.account).and_then(|s| s.account.clone());
        let answer = |a| {
            if let Some(acct) = &acct {
                acct.verify_answer(a);
            }
        };
        let yes = matches!(key.code, Enter | Char('y' | 'Y'));
        match (&panel.stage, key.code) {
            (VerifyStage::Asked { who }, _) if yes => {
                if let Some(acct) = &acct {
                    acct.verify_accept();
                }
                panel.stage = VerifyStage::Waiting { who: who.clone(), what: "to answer" };
            }
            (VerifyStage::Asked { .. }, Char('n' | 'N')) => {
                answer(VerifyAnswer::Cancel);
                self.verify_panel = None;
            }
            (VerifyStage::Asked { .. }, Esc) => {
                self.verify_panel = None;
                self.info("verification left for later: /verify accept takes it up");
            }
            (VerifyStage::Compare { who, .. }, _) if yes => {
                answer(VerifyAnswer::Confirm);
                panel.stage = VerifyStage::Waiting { who: who.clone(), what: "to confirm too" };
            }
            (VerifyStage::Compare { .. }, Char('n' | 'N')) => answer(VerifyAnswer::Mismatch),
            (VerifyStage::Compare { .. } | VerifyStage::Waiting { .. }, Esc) => {
                answer(VerifyAnswer::Cancel);
                self.verify_panel = None;
            }
            (VerifyStage::Done { .. } | VerifyStage::Cancelled { .. }, _) => self.verify_panel = None,
            _ => {}
        }
    }

    pub(super) fn on_vote_key(&mut self, key: KeyEvent) {
        use KeyCode::*;
        let Some(v) = self.vote.as_mut() else { return };
        let last = v.answers.len().saturating_sub(1);
        match key.code {
            Esc => self.vote = None,
            Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.vote = None,
            Up | Char('k') => v.selected = v.selected.saturating_sub(1),
            Down | Char('j') => v.selected = (v.selected + 1).min(last),
            // Picking several: Space ticks one.
            Char(' ') if v.max > 1 => {
                if !v.picked.remove(&v.selected) && v.picked.len() < v.max {
                    v.picked.insert(v.selected);
                }
            }
            Char(c @ '1'..='9') if (c as usize - '1' as usize) <= last => {
                v.selected = c as usize - '1' as usize;
                if v.max == 1 {
                    self.send_vote();
                }
            }
            Enter => self.send_vote(),
            _ => {}
        }
    }

    /// The emoji picker: arrows choose, typing searches, Enter reacts
    /// (with what's typed, when nothing matches it).
    pub(super) fn on_emoji_key(&mut self, key: KeyEvent) {
        use KeyCode::*;
        use emoji_picker::Way;
        let Some(p) = self.emoji_picker.as_mut() else { return };
        let ctrl_or_alt = key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let way = match key.code {
            Left => Some(Way::Left),
            Right => Some(Way::Right),
            Up => Some(Way::Up),
            Down => Some(Way::Down),
            PageUp => Some(Way::PageUp),
            PageDown => Some(Way::PageDown),
            Home => Some(Way::First),
            End => Some(Way::Last),
            _ => None,
        };
        if let Some(way) = way {
            let entries = p.entries();
            let rows = emoji_picker::layout(&entries, p.width);
            p.step(&entries, &rows, way);
            return;
        }
        match key.code {
            Esc => self.emoji_picker = None,
            Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.emoji_picker = None,
            Enter => {
                let entries = p.entries();
                let label = match entries.get(p.selected) {
                    Some(entry) => entry.pick.label(),
                    None => p.query.trim().to_owned(),
                };
                self.react_picked(&label);
            }
            Backspace => {
                p.query.pop();
                (p.selected, p.top) = (0, 0);
            }
            Char(c) if !ctrl_or_alt => {
                p.query.push(c);
                (p.selected, p.top) = (0, 0);
            }
            _ => {}
        }
    }

    /// React with the picker's choice, and leave selection mode.
    pub(super) fn react_picked(&mut self, label: &str) {
        let Some(p) = self.emoji_picker.take() else { return };
        if let Err(e) = self.send_react(p.event, label) {
            self.error(e);
        }
        self.select = None;
    }

    /// The custom emoji you can use in `room`: its own pack's, then yours
    /// and the global ones.
    pub(super) fn emoji_for(&self, account: &str, room: Option<&RoomId>) -> Vec<(String, OwnedMxcUri)> {
        let Some((global, rooms)) = self.custom_emoji.get(account) else { return Vec::new() };
        let mut out: Vec<(String, OwnedMxcUri)> = room.and_then(|r| rooms.get(r)).cloned().unwrap_or_default();
        for (code, picture) in global {
            if !out.iter().any(|(c, _)| c == code) {
                out.push((code.clone(), picture.clone()));
            }
        }
        out
    }

    /// Show `line`'s image full screen; the original is fetched for it.
    pub(super) fn open_viewer(&mut self, line: &Line) -> Result<(), String> {
        let media = line
            .media
            .clone()
            .filter(|m| m.kind == MediaKind::Image && !line.redacted)
            .ok_or("that message has no image")?;
        let event = line.event_id.clone().ok_or("that image isn't sent yet")?;
        if self.picker.is_none() {
            return Err("images are off (image_previews = false in config.toml)".into());
        }
        let key = self.win().room.clone().or_else(|| self.mention_origins.get(&event).cloned());
        let acct = key.and_then(|k| self.connected(&k.account)).ok_or("that image's server isn't connected")?;
        let moving = self.cfg.image_animation && self.picker.as_ref().is_some_and(Painter::moves);
        acct.fetch_full_image(event.clone(), media.clone(), moving);
        self.viewer = Some(Viewer::new(event, format!("{} · {}", line.nick, media.filename)));
        Ok(())
    }

    /// `/media`: the room's media as tiles, over its messages; `/links`:
    /// its links, as a list.
    pub(super) fn open_gallery(&mut self, tab: gallery::Tab) -> Result<(), String> {
        let key = self.win().room.clone().ok_or("/media and /links show a room's: go to the room first")?;
        (self.home, self.select) = (None, None);
        let mut gallery = Gallery::new(key);
        gallery.tab = tab;
        self.gallery = Some(gallery);
        // Pictures in the messages under it would linger.
        self.redraw_all = true;
        Ok(())
    }

    pub(super) fn close_gallery(&mut self) {
        self.gallery = None;
        self.tile_requests.clear();
        self.windows.iter_mut().for_each(|w| w.wants_older = false);
        // Terminal graphics can outlive the frame they were drawn in.
        self.redraw_all = true;
    }

    pub(super) fn gallery_window(&self) -> Option<usize> {
        let key = &self.gallery.as_ref()?.key;
        self.windows.iter().position(|w| w.room.as_ref() == Some(key))
    }

    /// The links tab's links: the room's, narrowed by the filter.
    pub fn shown_links(&self, idx: usize) -> Vec<gallery::Link> {
        let filter = self.gallery.as_ref().map_or("", |g| g.filter.as_str());
        let links = gallery::links(&self.windows[idx].lines);
        if filter.is_empty() {
            return links;
        }
        let nicknames = self.nicknames();
        links
            .into_iter()
            .filter(|l| {
                let title = match self.link_titles.get(&l.url) {
                    Some(LinkTitle::Ready(title, _)) => Some(title.as_str()),
                    _ => None,
                };
                let who = l.sender.as_ref().and_then(|u| nicknames.get(u)).unwrap_or(&l.nick);
                gallery::wanted(l, title, who, filter)
            })
            .collect()
    }

    pub(super) fn on_gallery_key(&mut self, key: KeyEvent) {
        use KeyCode::*;
        use gallery::Move;
        let Some(idx) = self.gallery_window() else { return self.close_gallery() };
        let filtering = self.gallery.as_ref().is_some_and(|g| g.filtering);
        if matches!(key.code, Tab | BackTab) && !filtering {
            if let Some(g) = self.gallery.as_mut() {
                g.tab = if g.tab == gallery::Tab::Media { gallery::Tab::Links } else { gallery::Tab::Media };
            }
            // Terminal graphics can outlive the frame they were drawn in.
            self.redraw_all = true;
            return;
        }
        if self.gallery.as_ref().is_some_and(|g| g.tab == gallery::Tab::Links) {
            return self.on_links_key(idx, key);
        }
        let way = match key.code {
            Left | Char('h') => Some(Move::Left),
            Right | Char('l') => Some(Move::Right),
            Up | Char('k') => Some(Move::Up),
            Down | Char('j') => Some(Move::Down),
            PageUp => Some(Move::PageUp),
            PageDown => Some(Move::PageDown),
            Home | Char('g') => Some(Move::First),
            End | Char('G') => Some(Move::Last),
            _ => None,
        };
        if let Some(way) = way {
            let items = gallery::items(&self.windows[idx].lines);
            if let Some(g) = self.gallery.as_mut() {
                g.step(&items, way);
            }
            return;
        }
        match key.code {
            Esc | Char('q') => self.close_gallery(),
            Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.close_gallery(),
            Enter | Char('z' | ' ') => self.gallery_open(idx, false),
            Char('s') => self.gallery_open(idx, true),
            _ => {}
        }
    }

    /// The links tab: move, open, copy, go to the message, filter.
    pub(super) fn on_links_key(&mut self, idx: usize, key: KeyEvent) {
        use KeyCode::*;
        use gallery::Move;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if let Some(g) = self.gallery.as_mut().filter(|g| g.filtering) {
            match key.code {
                Char(c) if !ctrl => g.filter.push(c),
                Backspace => drop(g.filter.pop()),
                Enter => g.filtering = false,
                Esc => (g.filter, g.filtering) = (String::new(), false),
                _ => {}
            }
            return;
        }
        let links = self.shown_links(idx);
        let way = match key.code {
            Up | Char('k') => Some(Move::Up),
            Down | Char('j') => Some(Move::Down),
            PageUp => Some(Move::PageUp),
            PageDown => Some(Move::PageDown),
            Home | Char('g') => Some(Move::First),
            End | Char('G') => Some(Move::Last),
            _ => None,
        };
        let Some(g) = self.gallery.as_mut() else { return };
        if let Some(way) = way {
            return g.step_link(&links, way);
        }
        let chosen = links.get(g.link_index(&links)).cloned();
        match (key.code, chosen) {
            (Esc | Char('q'), _) if !g.filter.is_empty() => g.filter.clear(),
            (Esc | Char('q'), _) => self.close_gallery(),
            (Char('c'), _) if ctrl => self.close_gallery(),
            (Char('/'), _) => g.filtering = true,
            (Enter | Char('o'), Some(link)) => self.open_url(link.url),
            (Char('c'), Some(link)) => {
                self.info(format!("copied {}", link.url));
                self.clipboard = Some(link.url);
            }
            (Char('m'), Some(link)) => {
                let key = g.key.clone();
                self.close_gallery();
                if let Err(e) = self.jump_to(key, link.event_id) {
                    self.error(e);
                }
            }
            _ => {}
        }
    }

    /// The chosen tile: a picture in the viewer, audio or video played, a
    /// file saved; `save` saves whatever it is.
    pub(super) fn gallery_open(&mut self, idx: usize, save: bool) {
        let line = {
            let items = gallery::items(&self.windows[idx].lines);
            let Some(g) = &self.gallery else { return };
            match items.get(g.index(&items)) {
                Some(line) => (*line).clone(),
                None => return,
            }
        };
        let Some(media) = line.media.clone() else { return };
        let done = if save || media.kind == MediaKind::File {
            self.room_ctx().map(|(acct, room_id)| acct.download(room_id, media))
        } else if media.playable() {
            self.play(&line)
        } else {
            self.open_viewer(&line)
        };
        if let Err(e) = done {
            self.error(e);
        }
    }

    /// `/home`, F4: what's waiting, over the messages.
    pub(super) fn open_home(&mut self) {
        if self.gallery.is_some() {
            self.close_gallery();
        }
        self.select = None;
        self.home = Some(Home::default());
        self.redraw_all = true;
        for acct in self.accounts.values().filter_map(|a| a.account.as_ref()) {
            acct.fetch_invites();
        }
    }

    /// The home window's rows: the latest mentions, rooms with unread
    /// messages (mentions first, then the most recent), invitations.
    pub fn home_rows(&self) -> Vec<home::Row> {
        let now = Local::now();
        let mut rows = Vec::new();
        if let Some(m) = self.windows.iter().find(|w| w.is_mentions) {
            let recent = m.lines.iter().rev().filter(|l| l.is_message() && now - l.ts <= home::MENTIONS_FOR);
            for line in recent.take(home::MENTIONS) {
                let Some(event) = line.event_id.clone() else { continue };
                let Some(key) = self.mention_origins.get(&event).cloned() else { continue };
                let room = self
                    .window_idx(&key.account, &key.room_id)
                    .map_or_else(|| key.room_id.to_string(), |i| self.windows[i].title().to_owned());
                // Copies are named "room/nick".
                let nick = line.nick.rsplit_once('/').map_or(line.nick.as_str(), |(_, n)| n).to_owned();
                rows.push(home::Row::Mention { key, event, room, nick, text: line.body.clone(), ts: line.ts });
            }
        }
        let last = |i: usize| self.windows[i].lines.iter().rev().find(|l| l.is_message());
        let mut unread: Vec<usize> = (1..self.windows.len())
            .filter(|&i| self.windows[i].room.is_some() && self.windows[i].activity >= Activity::Message)
            .collect();
        unread.sort_by(|&a, &b| {
            let newest = |i: usize| last(i).map(|l| l.ts);
            self.windows[b].activity.cmp(&self.windows[a].activity).then_with(|| newest(b).cmp(&newest(a)))
        });
        for i in unread {
            let w = &self.windows[i];
            let last =
                last(i).map(|l| (self.display_name(l.sender.as_deref(), &l.nick).to_owned(), l.body.clone(), l.ts));
            rows.push(home::Row::Unread {
                window: i,
                name: w.title().to_owned(),
                count: w.unread(),
                activity: w.activity,
                last,
            });
        }
        rows.extend(self.invites.iter().cloned().map(home::Row::Invite));
        rows
    }

    pub(super) fn on_home_key(&mut self, key: KeyEvent) {
        use KeyCode::*;
        let rows = self.home_rows();
        let Some(h) = self.home.as_mut() else { return };
        // `d` asks; anything else but a second `d` takes it back.
        let declining = h.declining.take();
        h.note = None;
        let last = rows.len().saturating_sub(1);
        match key.code {
            Esc | Char('q') | F(4) => self.home = None,
            Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.home = None,
            Up | Char('k') => h.selected = h.selected.saturating_sub(1),
            Down | Char('j') => h.selected = (h.selected + 1).min(last),
            PageUp => h.selected = h.selected.saturating_sub(10),
            PageDown => h.selected = (h.selected + 10).min(last),
            Home | Char('g') => h.selected = 0,
            End | Char('G') => h.selected = last,
            Enter => {
                if let Some(row) = rows.get(h.selected).cloned() {
                    self.home_go(row);
                }
            }
            Char(c @ '1'..='9') => {
                if let Some(row) = rows.get(c as usize - '1' as usize).cloned() {
                    self.home_go(row);
                }
            }
            Char('d') => match rows.get(h.selected) {
                Some(home::Row::Invite(invite)) if declining.as_ref() == Some(&invite.room_id) => {
                    let invite = invite.clone();
                    match self.connected(&invite.account) {
                        Some(acct) => {
                            acct.decline(None, invite.room_id.clone());
                            self.invites.retain(|i| i.room_id != invite.room_id);
                            let note = format!("declined the invitation to {}", invite.name);
                            self.home.iter_mut().for_each(|h| h.note = Some(note.clone()));
                            self.info(note);
                        }
                        None => self.error(format!("{} isn't connected", invite.account)),
                    }
                }
                Some(home::Row::Invite(invite)) => h.declining = Some(invite.room_id.clone()),
                _ => {}
            },
            _ => {}
        }
    }

    /// Go where a home row leads (joining, for an invitation).
    pub(super) fn home_go(&mut self, row: home::Row) {
        self.home = None;
        let done = match row {
            home::Row::Mention { key, event, .. } => self.jump_to(key, event),
            home::Row::Unread { window, .. } => {
                self.switch_to(window);
                Ok(())
            }
            home::Row::Invite(invite) => match self.connected(&invite.account) {
                Some(acct) => {
                    acct.join(invite.room_id.to_string());
                    self.invites.retain(|i| i.room_id != invite.room_id);
                    self.info(format!("joining {}…", invite.name));
                    Ok(())
                }
                None => Err(format!("{} isn't connected", invite.account)),
            },
        };
        if let Err(e) = done {
            self.error(e);
        }
    }

    pub(super) fn close_viewer(&mut self) {
        self.viewer = None;
        // Terminal graphics can outlive the frame they were drawn in.
        self.redraw_all = true;
    }
}
