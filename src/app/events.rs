//! What happens on its own: events from the servers and the background, the clock, notifications, and auto-away.

use super::*;

impl App {
    pub fn on_event(&mut self, ev: AppEvent) {
        let cap = self.cfg.scrollback;
        match ev {
            AppEvent::Update { latest, asked } => {
                let current = env!("CARGO_PKG_VERSION");
                match latest {
                    Some((version, url)) if update::newer(&version, current) => {
                        if self.released.as_deref() != Some(&version) {
                            self.status(Line::info(format!(
                                "Textrix {version} is out (you have {current}): brew update && brew upgrade textrix, or {url}"
                            )));
                        }
                        self.released = Some(version);
                    }
                    Some(_) if asked => self.info(format!("Textrix {current} is the latest")),
                    None if asked => self.error("couldn't reach GitHub to check for updates"),
                    _ => {}
                }
            }
            AppEvent::LinkTitle { url, title, picture } => {
                if self.link_titles.len() > LINK_TITLES_MAX {
                    self.link_titles.clear();
                }
                self.link_titles.insert(url, title.map_or(LinkTitle::None, |t| LinkTitle::Ready(t, picture)));
            }
            AppEvent::LinkTitlesOff { account } => {
                if self.link_titles_off.insert(account.to_string()) {
                    self.status(Line::info(format!(
                        "{account}: the server doesn't offer link previews, so links won't show page titles \
                         (Synapse: url_preview_enabled)"
                    )));
                }
            }
            AppEvent::Attached { picker } => {
                // Images are drawn afresh for this terminal.
                self.picker = picker.filter(|_| self.cfg.image_previews).map(|p| Painter::new(p, self.cfg.image_text));
                self.note_images();
                self.previews.clear();
                self.avatars.clear();
                self.viewer = None;
                self.notify_mode = notify::resolve(self.cfg.notifications);
                // This terminal may have more colours, or fewer.
                if let Ok(palette) = Theme::load(&self.cfg.theme, &self.cfg.colors, &self.paths.themes_dir()) {
                    (self.cfg.palette, self.palette_for) = (palette, self.cfg.theme.clone());
                }
                (self.redraw_all, self.last_title) = (true, None);
                if self.cfg.mouse {
                    set_mouse_capture(true);
                }
            }
            AppEvent::Quit => self.quit = true,
            AppEvent::Exec { into, send, command, output } => self.exec_done(into, send, &command, output),
            AppEvent::Changelog { into, releases } => self.changelog_done(into, releases),
            AppEvent::Directory { account, source, query, rooms } => {
                if let Some(d) = self.directory.as_mut().filter(|d| *d.account == *account) {
                    d.arrived(source, query, rooms);
                }
            }
            AppEvent::NowPlaying { into, track } => self.np_done(into, track),
            AppEvent::Voice { key, voice } => self.voice_done(key, voice),
            AppEvent::Pasted { image, picture } => self.pasted_image(image, picture),
            AppEvent::Plugin { name, run, message } => self.on_plugin(name, run, message),
            AppEvent::UserInfo { account, user, info } => {
                let Some(p) = self.profile.as_mut().filter(|p| p.user == user && *p.account == *account) else {
                    return;
                };
                let wants_picture = info.avatar.clone().zip(self.picker.clone());
                p.info = info;
                if let (Some((url, painter)), Some(acct)) = (wants_picture, self.connected(&account)) {
                    let (w, h) = PROFILE_PICTURE;
                    acct.fetch_user_picture(user, url, painter, Size::new(w, h));
                }
            }
            AppEvent::UserPicture { user, picture } => {
                if let Some(p) = self.profile.as_mut().filter(|p| p.user == user) {
                    p.picture = picture;
                }
            }
            AppEvent::Scheduled { account, items } => {
                for item in items {
                    if !self.later.iter().any(|(a, s)| *a == account && *s == item) {
                        self.later.push((account.clone(), item));
                    }
                }
                self.later.sort_by_key(|(_, s)| s.due);
            }
            AppEvent::CustomEmoji { account, global, rooms } => {
                self.custom_emoji.insert(account, (global, rooms));
            }
            AppEvent::Edited(saved) => {
                match saved {
                    Ok(text) => self.input.set_text(&text),
                    Err(e) => self.error(e),
                }
                // The editor had the screen: draw it all again.
                (self.redraw_all, self.last_title) = (true, None);
                if self.cfg.mouse {
                    set_mouse_capture(true);
                }
            }
            AppEvent::Installed { version } => {
                self.probing = false;
                let current = env!("CARGO_PKG_VERSION");
                let text = match version.as_deref() {
                    Some(v) if v != current => {
                        format!("Textrix {v} is installed (this is {current}): /restart switches to it")
                    }
                    _ => "a new build of Textrix is installed: /restart runs it".to_owned(),
                };
                if self.installed.is_none() {
                    self.status(Line::info(text));
                }
                self.installed = Some(version.filter(|v| v != current).unwrap_or_default());
            }
            AppEvent::Detached => {
                self.status(Line::info("detached: Textrix carries on in the background (textrix --attach)"));
            }
            AppEvent::Played { event_id, result } => {
                if self.playing.as_ref().is_some_and(|(playing, _)| *playing == event_id) {
                    self.playing = None;
                }
                if let Err(e) = result {
                    self.error(e);
                }
            }
            AppEvent::FullImage { event_id, image } => {
                if let Some(v) = self.viewer.as_mut().filter(|v| v.event == event_id) {
                    v.image = match image {
                        Ok(image) => viewer::Loading::Ready(image),
                        Err(e) => viewer::Loading::Failed(e),
                    };
                }
            }
            AppEvent::ViewerFrame { event_id, view, protocol } => {
                if let Some(v) = self.viewer.as_mut().filter(|v| v.event == event_id && v.pending == Some(view)) {
                    v.pending = None;
                    match protocol {
                        Ok(protocol) => v.frame = Some((view, protocol)),
                        Err(e) => v.image = viewer::Loading::Failed(e),
                    }
                }
            }
            AppEvent::PushRules { account, modes } => {
                self.push_modes.insert(account, modes);
            }
            AppEvent::Connected(account) => {
                let name = account.name.clone();
                self.status(Line::info(format!(
                    "{name}: logged in as {} on {} (device {})",
                    account.user_id,
                    account.homeserver,
                    account.client.device_id().map(|d| d.to_string()).unwrap_or_default()
                )));
                if let Some(state) = self.accounts.get_mut(&name) {
                    state.status = ConnStatus::Syncing;
                    account.start();
                    account.load_later();
                    state.account = Some(account);
                }
            }
            AppEvent::ConnectFailed { account, error } => {
                if let Some(state) = self.accounts.get_mut(&account) {
                    state.status = ConnStatus::Offline;
                    if let Some(acct) = state.account.take() {
                        acct.stop();
                    }
                }
                self.status(Line::error(format!("{account}: {error}")));
            }
            AppEvent::Synced { account, first } => {
                if let Some(state) = self.accounts.get_mut(&account) {
                    state.status = ConnStatus::Online;
                }
                if first && let Some(acct) = self.connected(&account) {
                    acct.load_custom_emoji();
                }
                match self.splits.remove(&*account) {
                    Some(since) if !first => {
                        let rooms = self.split_rooms(&account);
                        let text = format!("Netsplit over after {}, joins: {rooms}", short_duration(since.elapsed()));
                        self.status(Line::info(text));
                    }
                    Some(since) => {
                        let server = self.server_host(&account);
                        let after = short_duration(since.elapsed());
                        self.status(Line::info(format!("{server} reached after {after}: synced, and up to date")));
                    }
                    _ => self.status(Line::info(format!("{account}: synced"))),
                }
            }
            AppEvent::SyncError { account, error } => {
                let never_synced = self.accounts.get(&account).is_some_and(|s| s.status == ConnStatus::Syncing);
                if let Some(state) = self.accounts.get_mut(&account) {
                    state.status = ConnStatus::Lagging;
                }
                if never_synced && !self.splits.contains_key(&*account) {
                    // Started offline: what's saved is there to read.
                    self.splits.insert(account.to_string(), Instant::now());
                    let server = self.server_host(&account);
                    self.status(Line::error(format!(
                        "can't reach {server} ({error}): showing what's saved here; what you send waits, and goes \
                         when it's back"
                    )));
                } else if !self.splits.contains_key(&*account) {
                    // irssi-style: the server split from you, taking your rooms.
                    self.splits.insert(account.to_string(), Instant::now());
                    let server = self.server_host(&account);
                    let rooms = self.split_rooms(&account);
                    // In the status window only: in rooms they're noise.
                    let text = format!("Netsplit {server} ⇄ you, quits: {rooms} ({error}; rejoining when it's back)");
                    self.status(Line::error(text));
                }
            }
            AppEvent::Rooms { account, mut rooms } => {
                rooms.sort_by_cached_key(|m| (m.is_dm, m.name.to_lowercase()));
                for meta in rooms {
                    self.upsert_room(&account, meta);
                }
                self.rooms_loaded.insert(account.to_string());
                self.restore_split();
            }
            AppEvent::RoomGone { account, room_id } => {
                let key = WinKey { account: account.to_string(), room_id: room_id.clone(), thread: None };
                self.layout.closed.retain(|k| *k != key);
                if let Some(idx) = self.window_idx(&account, &room_id) {
                    let name = self.windows[idx].title().to_owned();
                    self.remove_window(idx);
                    self.status(Line::info(format!("{account}: you left {name}")));
                }
            }
            AppEvent::Timeline { account, room_id, items, live, limited } => {
                // Messages were skipped since the cache was saved: drop the
                // cached ones rather than show them joined to these, and
                // load history from the server as usual.
                if limited && let Some(idx) = self.window_idx(&account, &room_id) {
                    self.windows[idx].lines.retain(|l| !l.cached);
                }
                if let Some(acct) = self.connected(&account) {
                    acct.index_items(&room_id, &items);
                }
                self.on_timeline(&account, &room_id, items, live)
            }
            AppEvent::Cached { account, room_id, items } => {
                if let Some(idx) = self.window_idx(&account, &room_id) {
                    self.windows[idx].merge_history(items.clone());
                    self.record_old_mentions(&account, &room_id, &items);
                }
            }
            AppEvent::History { account, room_id, items, end, failed } => {
                if let Some(acct) = self.connected(&account) {
                    acct.index_items(&room_id, &items);
                }
                let Some(idx) = self.window_idx(&account, &room_id) else { return };
                let w = &mut self.windows[idx];
                if failed {
                    w.history = end.map_or(History::Unloaded, History::More);
                    w.history_retry = Some(Instant::now() + Duration::from_secs(15));
                } else {
                    w.history = end.map_or(History::Done, History::More);
                    w.history_retry = None;
                    w.merge_history(items.clone());
                    self.record_old_mentions(&account, &room_id, &items);
                }
            }
            AppEvent::Receipts { account, room_id, receipts, live } => {
                let Some(idx) = self.window_idx(&account, &room_id) else { return };
                let w = &mut self.windows[idx];
                for (user, nick, event_id, ts) in receipts {
                    // Receipts often point at events we don't draw (reactions,
                    // edits); a live one then covers what came before it,
                    // and never a message sent after it was read.
                    let target = match (w.contains(&event_id), live) {
                        (true, _) | (false, false) => event_id,
                        (false, true) => match w.latest_event_by(ts) {
                            Some(latest) => latest,
                            None => continue,
                        },
                    };
                    w.read_by.insert(user, (nick, target));
                }
            }
            AppEvent::UploadProgress { account, room_id, txn_id, percent } => {
                let Some(idx) = self.window_idx(&account, &room_id) else { return };
                if let Some(line) =
                    self.windows[idx].lines.iter_mut().rev().find(|l| l.pending && l.txn_id.as_ref() == Some(&txn_id))
                {
                    line.progress = Some(percent);
                }
            }
            AppEvent::SearchResults { account, room_id, hits } => {
                if let Some(acct) = self.connected(&account) {
                    acct.index_items(&room_id, &hits.iter().cloned().map(Item::Line).collect::<Vec<_>>());
                }
                self.search_pending = self.search_pending.saturating_sub(1);
                let key = WinKey { account: account.to_string(), room_id: room_id.clone(), thread: None };
                let room = self
                    .window_idx(&account, &room_id)
                    .map(|i| self.windows[i].title().to_owned())
                    .or_else(|| self.connected(&account).and_then(|a| a.quick_meta(&room_id)).map(|m| m.name))
                    .unwrap_or_else(|| room_id.to_string());
                let query = self.search_query.clone().unwrap_or_default();
                let rows: Vec<Line> = hits
                    .into_iter()
                    .filter(|line| self.search_matches(&query, &room, line))
                    .collect::<Vec<_>>()
                    .into_iter()
                    .filter_map(|line| {
                        let event = line.event_id.clone()?;
                        let n = self.search_hits.len() + 1;
                        self.search_hits.push((key.clone(), event));
                        Some(Line::info(search_row(n, &room, &line)))
                    })
                    .collect();
                let done = (self.search_pending == 0).then(|| {
                    Line::info(format!("search done: {} result(s); /jump N to go there", self.search_hits.len()))
                });
                let out = self.search_out.clone().and_then(|k| self.window_idx(&k.account, &k.room_id)).unwrap_or(0);
                for line in rows.into_iter().chain(done) {
                    self.windows[out].push(line, cap);
                }
            }
            AppEvent::Thread { account, room_id, root, items } => {
                let Some(idx) = self.thread_idx(&account, &room_id, &root) else { return };
                let w = &mut self.windows[idx];
                w.merge_history(items);
                // Root first, then replies in order (pages can arrive out of order).
                w.lines.make_contiguous().sort_by_key(|l| (l.event_id.as_ref() != Some(&root), l.ts));
                w.history = History::Done;
                if let Some(first) = w.lines.iter().find(|l| l.event_id.as_ref() == Some(&root)) {
                    let text = first.text();
                    w.name = thread_name(&text);
                    w.topic = Some(format!("Thread: <{}> {}", first.nick, text.lines().next().unwrap_or("")));
                }
            }
            AppEvent::Pins { account, room_id, lines } => {
                let key = WinKey { account: account.to_string(), room_id: room_id.clone(), thread: None };
                // Numbered like search results, so /jump N goes there.
                self.search_hits.clear();
                let mut rows = vec![Line::info(match lines.len() {
                    0 => "no pinned messages here; p on a message in selection mode pins it".to_owned(),
                    n => format!("{n} pinned message(s), newest pin first; /jump N goes there"),
                })];
                for line in &lines {
                    let Some(event) = line.event_id.clone() else { continue };
                    self.search_hits.push((key.clone(), event));
                    let when = line.ts.format("%Y-%m-%d %H:%M");
                    let n = self.search_hits.len();
                    let row = if line.nick.is_empty() {
                        format!("{n:>2}: {}", line.text())
                    } else {
                        format!("{n:>2}: {when} <{}> {}", line.nick, snippet(&line.text()))
                    };
                    rows.push(Line::info(row));
                }
                let out = self.window_idx(&account, &room_id).unwrap_or(0);
                for row in rows {
                    self.windows[out].push(row, cap);
                }
            }
            AppEvent::Threads { account, room_id, roots } => {
                let key = WinKey { account: account.to_string(), room_id: room_id.clone(), thread: None };
                self.thread_hits.clear();
                let mut rows = Vec::new();
                for (n, root) in roots.iter().enumerate() {
                    let Some(event) = root.event_id.clone() else { continue };
                    self.thread_hits.push((key.clone(), event));
                    let replies = match root.thread_count {
                        Some(1) => " [1 reply]".to_owned(),
                        Some(c) => format!(" [{c} replies]"),
                        None => String::new(),
                    };
                    rows.push(Line::info(format!("{:>2}: <{}> {}{replies}", n + 1, root.nick, snippet(&root.text()))));
                }
                let out = self.window_idx(&account, &room_id).unwrap_or(0);
                if rows.is_empty() {
                    rows.push(Line::info("no threads in this room"));
                } else {
                    rows.push(Line::info("/thread N opens one"));
                }
                for row in rows {
                    self.windows[out].push(row, cap);
                }
            }
            AppEvent::Spaces { account, mut spaces } => {
                spaces.sort_by_cached_key(|s| s.name.to_lowercase());
                self.spaces.insert(account, spaces);
            }
            AppEvent::Presence { account, updates } => {
                self.presence.entry(account).or_default().extend(updates);
            }
            AppEvent::Avatar { account, room_id, url, protocol } => {
                let preview = protocol.map_or(Preview::Failed, Preview::Ready);
                self.avatars.insert((account.to_string(), room_id), (url, preview));
            }
            AppEvent::Tile { event_id, picture } => {
                if let Some(g) = self.gallery.as_mut() {
                    g.store(event_id, picture);
                }
            }
            AppEvent::Invites { account, invites } => {
                // From those config.toml trusts: join (as bots do).
                for invite in &invites {
                    if home::accepted(&self.cfg.accept_invites_from, invite.inviter_id.as_deref())
                        && let Some(acct) = self.connected(&account)
                        && self.joining.insert(invite.room_id.clone())
                    {
                        acct.join(invite.room_id.to_string());
                        let who = invite.inviter.as_deref().unwrap_or("someone");
                        self.status(Line::info(format!(
                            "joining {}: {who} invited you (accept_invites_from)",
                            invite.name
                        )));
                    }
                }
                self.invites.retain(|i| *i.account != *account);
                self.invites.extend(invites.into_iter().filter(|i| !self.joining.contains(&i.room_id)));
            }
            AppEvent::ImagePreview { event_id, protocol } => {
                let preview = protocol.map_or(Preview::Failed, Preview::Ready);
                self.previews.insert(event_id.clone(), preview);
                self.preview_order.push_back(event_id);
                while self.preview_order.len() > PREVIEWS_MAX {
                    if let Some(old) = self.preview_order.pop_front() {
                        self.previews.remove(&old);
                    }
                }
            }
            AppEvent::Typing { account, room_id, nicks } => {
                for idx in self.room_windows(&account, &room_id) {
                    (self.windows[idx].typing, self.windows[idx].typing_at) = (nicks.clone(), Some(Instant::now()));
                }
            }
            AppEvent::Members { account, room_id, members, show } => {
                let Some(idx) = self.window_idx(&account, &room_id) else { return };
                self.windows[idx].members = members;
                if show {
                    let lines = self.names_lines(idx);
                    for line in lines {
                        self.windows[idx].push(line, cap);
                    }
                }
            }
            AppEvent::Permissions { account, room_id, perms } => {
                if let Some(idx) = self.window_idx(&account, &room_id) {
                    self.windows[idx].perms = Some(perms);
                }
            }
            AppEvent::Focus { account, room_id } => {
                let idx = match self.window_idx(&account, &room_id) {
                    Some(idx) => Some(idx),
                    None => self.reopen(&account, &room_id),
                };
                if let Some(idx) = idx {
                    self.switch_to(idx);
                }
            }
            AppEvent::Sent { account, room_id, txn_id, event_id } => {
                self.plugin_sent(&txn_id, Ok(&event_id));
                let Some(idx) = self.window_idx(&account, &room_id) else { return };
                let w = &mut self.windows[idx];
                if w.contains(&event_id) {
                    // The sync echo won the race; drop the stale local echo.
                    w.lines.retain(|l| !(l.pending && l.txn_id.as_ref() == Some(&txn_id)));
                } else if let Some(line) = w.lines.iter_mut().rev().find(|l| l.txn_id.as_ref() == Some(&txn_id)) {
                    (line.pending, line.waiting, line.progress) = (false, false, None);
                    line.event_id = Some(event_id.clone());
                } else {
                    // Sent before we heard the send queue's name for it.
                    self.sent_early.insert(txn_id, event_id);
                }
            }
            AppEvent::Unsent { account, room_id, lines } => {
                let nick = self.accounts.get(&*account).and_then(|s| s.nick.clone());
                let Some(idx) = self.window_idx(&account, &room_id) else { return };
                for mut line in lines {
                    let w = &mut self.windows[idx];
                    // Already back from the server, or sent while starting up.
                    if w.lines.iter().any(|l| l.txn_id.is_some() && l.txn_id == line.txn_id) {
                        continue;
                    }
                    if let Some(event_id) = line.txn_id.as_ref().and_then(|t| self.sent_early.remove(t)) {
                        if w.contains(&event_id) {
                            continue;
                        }
                        (line.pending, line.waiting, line.event_id) = (false, false, Some(event_id));
                    }
                    let localpart = || line.sender.as_ref().map(|u| u.localpart().to_owned()).unwrap_or_default();
                    line.nick = nick.clone().unwrap_or_else(localpart);
                    w.push(line, cap);
                }
            }
            AppEvent::Queued { account, room_id, txn_id, queued } => {
                self.plugin_renamed(&txn_id, queued.clone());
                let Some(idx) = self.window_idx(&account, &room_id) else { return };
                if let Some(line) =
                    self.windows[idx].lines.iter_mut().rev().find(|l| l.txn_id.as_ref() == Some(&txn_id))
                {
                    line.txn_id = Some(queued.clone());
                }
                if let Some(event_id) = self.sent_early.remove(&queued) {
                    self.on_event(AppEvent::Sent { account, room_id, txn_id: queued, event_id });
                }
            }
            AppEvent::SendWaiting { account, room_id, txn_id } => {
                let Some(idx) = self.window_idx(&account, &room_id) else { return };
                let w = &mut self.windows[idx];
                let first = !w.lines.iter().any(|l| l.waiting);
                let Some(line) = w.lines.iter_mut().rev().find(|l| l.txn_id.as_ref() == Some(&txn_id)) else { return };
                line.waiting = true;
                if first {
                    let server = self.server_host(&account);
                    let text = format!("can't reach {server}: what you send here waits, and goes as soon as it can");
                    self.windows[idx].push(Line::info(text), cap);
                }
            }
            AppEvent::Bell(kind) => self.ring(kind),
            AppEvent::SendFailed { account, room_id, txn_id, error } => {
                self.plugin_sent(&txn_id, Err(&error));
                self.ring(BellKind::FailedSends);
                let Some(idx) = self.window_idx(&account, &room_id) else { return };
                let w = &mut self.windows[idx];
                if let Some(line) = w.lines.iter_mut().rev().find(|l| l.txn_id.as_ref() == Some(&txn_id)) {
                    (line.pending, line.waiting, line.progress) = (false, false, None);
                    line.body.push_str("  [not sent]");
                }
                w.push(Line::error(format!("not sent: {error}")), cap);
            }
            AppEvent::KeysReceived { account, room_id, sessions } => {
                let mut events: Vec<OwnedEventId> = self
                    .room_windows(&account, &room_id)
                    .into_iter()
                    .flat_map(|i| self.windows[i].utd_events(Some(&sessions)))
                    .collect();
                events.dedup();
                if let (false, Some(acct)) = (events.is_empty(), self.connected(&account)) {
                    acct.redecrypt(room_id, events);
                }
            }
            AppEvent::Recovered { account } => {
                let Some(acct) = self.connected(&account) else { return };
                for w in &self.windows {
                    if let Some(key) = w.room.as_ref().filter(|k| *k.account == *account) {
                        let events = w.utd_events(None);
                        if !events.is_empty() {
                            acct.redecrypt(key.room_id.clone(), events);
                        }
                    }
                }
            }
            AppEvent::Redecrypted { account, room_id, item } => {
                for idx in self.room_windows(&account, &room_id) {
                    let w = &mut self.windows[idx];
                    match &item {
                        Item::Line(line) if line.kind != LineKind::Utd => {
                            let Some(id) = line.event_id.clone() else { continue };
                            if let Some(existing) = w.find_mut(&id) {
                                let reactions = std::mem::take(&mut existing.reactions);
                                *existing = line.clone();
                                existing.reactions = reactions;
                            }
                        }
                        Item::Line(_) => {}
                        other => w.annotate_existing(other),
                    }
                }
            }
            AppEvent::Profile { account, nick } => {
                if let Some(state) = self.accounts.get_mut(&account) {
                    state.nick = Some(nick);
                }
            }
            // How it ended only shows in a panel still open: not after Esc.
            AppEvent::Verify { stage: VerifyStage::Cancelled { .. }, .. } if self.verify_panel.is_none() => {}
            AppEvent::Verify { account, stage } => self.verify_panel = Some(VerifyPanel { account, stage }),
            AppEvent::Trust { account, verified } => {
                if let Some(state) = self.accounts.get_mut(&account) {
                    state.verified = Some(verified);
                }
            }
            AppEvent::Away { account, message } => {
                let text = match &message {
                    Some(msg) => format!("{account}: you are now away: {msg}"),
                    None => format!("{account}: you are back"),
                };
                let away = message.is_some();
                if let Some(state) = self.accounts.get_mut(&account) {
                    state.away = message;
                }
                self.status(Line::info(text));
                if away {
                    self.away_since.get_or_insert_with(Local::now);
                } else if !self.accounts.values().any(|s| s.away.is_some()) {
                    self.back_from_away();
                }
            }
            AppEvent::Ignored { account, user_id } => {
                for w in self.windows.iter_mut().filter(|w| w.account() == Some(&*account)) {
                    w.lines.retain(|l| l.sender.as_ref() != Some(&user_id));
                }
            }
            AppEvent::LoggedOut { account } => {
                if let Some(state) = self.accounts.get_mut(&account) {
                    state.account = None;
                    state.status = ConnStatus::Offline;
                }
                while let Some(idx) = self.windows.iter().position(|w| w.account() == Some(&*account)) {
                    self.remove_window(idx);
                }
                self.layout.windows.retain(|k| *k.account != *account);
                self.layout.closed.retain(|k| *k.account != *account);
                self.status(Line::info(format!("{account}: logged out; local session and keys removed")));
            }
            AppEvent::Notice { account, room_id, line } => {
                let idx = match (account, room_id) {
                    (Some(account), Some(room_id)) => self.window_idx(&account, &room_id),
                    _ => None,
                };
                match idx {
                    Some(idx) => {
                        self.windows[idx].push(line, cap);
                    }
                    None => self.status(line),
                }
            }
        }
    }

    pub(super) fn on_timeline(&mut self, account: &str, room_id: &OwnedRoomId, items: Vec<Item>, live: bool) {
        let idx = match self.window_idx(account, room_id) {
            Some(idx) => idx,
            None => {
                let wakes = live && items.iter().any(|i| matches!(i, Item::Line(l) if l.is_message() && !l.own));
                match wakes.then(|| self.reopen(account, room_id)).flatten() {
                    Some(idx) => idx,
                    None => return,
                }
            }
        };
        let cap = self.cfg.scrollback;
        let width = match self.windows[idx].width {
            0 => self.view_width,
            w => w,
        };
        let viewing = self.on_screen(idx);
        let mode = self.room_mode(idx);
        let muted = mode == Some(RoomNotificationMode::Mute);
        // Mentions-only and muted rooms keep ordinary messages quiet.
        let quiet = muted || mode == Some(RoomNotificationMode::MentionsAndKeywordsOnly);
        let mut ring = false;
        let mut mentions = Vec::new();
        let mut hooks = Vec::new();
        let notifying = self.notifying();

        if live && let Some(log) = self.log.as_mut() {
            let name = self.windows[idx].name.clone();
            for item in &items {
                if let Err(e) = log.write(account, &name, room_id, item) {
                    self.log = None;
                    self.status(Line::error(format!("chat logging stopped: {e}")));
                    break;
                }
            }
        }

        // Open thread windows of this room get their thread's messages too.
        let threads: Vec<(usize, OwnedEventId)> = self
            .room_windows(account, room_id)
            .into_iter()
            .filter_map(|i| Some((i, self.windows[i].thread_root()?.to_owned())))
            .collect();
        for (tidx, root) in &threads {
            for item in &items {
                match item {
                    Item::Line(line)
                        if line.thread_root.as_ref() == Some(root) || line.event_id.as_ref() == Some(root) =>
                    {
                        let added = self.windows[*tidx].push(line.clone(), cap);
                        if added && live && !line.own && !muted && !self.on_screen(*tidx) {
                            let level = if line.highlight { Activity::Highlight } else { Activity::Message };
                            self.windows[*tidx].activity = self.windows[*tidx].activity.max(level);
                        }
                    }
                    Item::Line(_) => {}
                    other => self.windows[*tidx].annotate_existing(other),
                }
            }
        }

        let script_key = self.windows[idx].room.clone().map(|k| WinKey { thread: None, ..k });
        for item in items {
            if live
                && let Item::React { target, key, nick, own: false } = &item
                && let Some(room) = script_key.clone()
            {
                let text = self.windows[idx].find_mut(target).map(|l| l.text()).unwrap_or_default();
                let heard = Heard::Reaction { nick: nick.clone(), key: key.clone(), target: target.clone(), text };
                self.script_messages.push((room, heard));
            }
            let Some(line) = self.windows[idx].apply(item, cap, live) else { continue };
            if live
                && let Some(root) = &line.thread_root
                && let Some(root_line) = self.windows[idx].find_mut(root)
            {
                root_line.thread_count = Some(root_line.thread_count.unwrap_or(0) + 1);
            }
            // While you're away, DMs collect with your mentions too: the
            // away log.
            let away_dm =
                live && self.away_since.is_some() && self.windows[idx].is_dm && !line.own && line.is_message();
            if is_mention(&line) || away_dm {
                mentions.push(line.clone());
            }
            if live
                && !line.own
                && let Some(key) = script_key.clone()
            {
                let heard = match &line.fold {
                    _ if line.is_message() && !line.redacted => Some(Heard::Message(line.clone())),
                    Some(Fold::Joined(_)) => Some(Heard::Join(line.clone())),
                    Some(Fold::Left(_)) => Some(Heard::Leave(line.clone())),
                    _ => None,
                };
                self.script_messages.extend(heard.map(|h| (key, h)));
            }
            let w = &mut self.windows[idx];
            if w.scroll > 0 {
                w.scroll += ui::line_height(&line, width, &self.cfg);
                w.unseen_below = true;
            }
            // Someone who just sent a message has stopped typing.
            w.typing.retain(|(user, _)| line.sender.as_ref() != Some(user));
            // Sending a message means you've read everything before it.
            if let (Some(sender), Some(event_id)) = (&line.sender, &line.event_id)
                && line.is_message()
                && !line.own
            {
                w.read_by.insert(sender.clone(), (line.nick.clone(), event_id.clone()));
            }
            if !live || line.own {
                continue;
            }
            let mut level = if line.highlight || (w.is_dm && line.is_message() && !quiet) {
                Activity::Highlight
            } else if line.is_message() && !quiet {
                Activity::Message
            } else if line.kind == LineKind::Membership && !self.cfg.show_joins {
                Activity::None
            } else {
                Activity::Event
            };
            if muted {
                level = level.min(Activity::Event);
            }
            if !viewing || w.scroll > 0 {
                w.activity = w.activity.max(level);
            }
            // Highlights and DMs alert you, and so does every message in a
            // room set to /notify all.
            let alert =
                level == Activity::Highlight || (mode == Some(RoomNotificationMode::AllMessages) && line.is_message());
            if !alert || (viewing && self.focused) {
                continue;
            }
            ring = true;
            let nick = line.sender.as_ref().and_then(|u| self.layout.nicknames.get(u)).unwrap_or(&line.nick);
            let title = if w.is_dm { nick.clone() } else { format!("{nick} in {}", w.title()) };
            let body = if self.cfg.notify_preview { line.body.clone() } else { "new message".to_owned() };
            if !self.cfg.notify_command.trim().is_empty()
                && let Some(key) = &w.room
            {
                let kind = if line.highlight {
                    "mention"
                } else if w.is_dm {
                    "dm"
                } else {
                    "message"
                };
                let text = if self.cfg.notify_preview { line.body.clone() } else { String::new() };
                hooks.push(vec![
                    ("TEXTRIX_KIND", kind.to_owned()),
                    ("TEXTRIX_TITLE", title.clone()),
                    ("TEXTRIX_FROM", nick.clone()),
                    ("TEXTRIX_FROM_ID", line.sender.as_ref().map(ToString::to_string).unwrap_or_default()),
                    ("TEXTRIX_ROOM", w.title().to_owned()),
                    ("TEXTRIX_ROOM_ID", key.room_id.to_string()),
                    ("TEXTRIX_SERVER", key.account.clone()),
                    ("TEXTRIX_TEXT", text),
                    ("TEXTRIX_AWAY", if self.away_since.is_some() { "1" } else { "0" }.to_owned()),
                ]);
            }
            if notifying {
                self.notifications.push((title, body));
            }
        }
        // Like irssi's hilight window, but every mention, newest last.
        for line in mentions {
            self.record_mention(idx, &line, live && !viewing);
        }
        for vars in hooks {
            self.run_notify_command(vars);
        }
        if ring {
            self.ring(BellKind::Mentions);
        }
        self.run_message_scripts();
    }

    /// Ring the bell for `kind`, as `bell`, `bell_style` and `bell_for` say.
    /// Rings come at least [`BELL_GAP`] apart, so a burst rings once.
    pub(super) fn ring(&mut self, kind: BellKind) {
        let now = Instant::now();
        let allowed = match self.bell_mode {
            BellMode::On => true,
            BellMode::Off => false,
            BellMode::Away => !self.focused,
        };
        if !allowed || !self.cfg.bell_for.contains(&kind) || self.last_bell.is_some_and(|t| now - t < BELL_GAP) {
            return;
        }
        self.last_bell = Some(now);
        if matches!(self.bell_style, BellStyle::Sound | BellStyle::Both) {
            self.bell = true;
        }
        if matches!(self.bell_style, BellStyle::Visual | BellStyle::Both) {
            self.flash_until = Some(now + FLASH);
        }
    }

    /// Copy a message that mentions you into the mentions window (made at
    /// the end of the list the first time), in time order. `alert` lights
    /// the window up.
    pub(super) fn record_mention(&mut self, from: usize, line: &Line, alert: bool) {
        let (Some(key), Some(event)) = (self.windows[from].room.clone(), line.event_id.clone()) else { return };
        let key = WinKey { thread: None, ..key };
        if self.mention_origins.contains_key(&event) {
            return;
        }
        let room =
            self.window_idx(&key.account, &key.room_id).map_or(self.windows[from].title(), |i| self.windows[i].title());
        let mut copy = line.clone();
        let nick = self.display_name(line.sender.as_deref(), &line.nick);
        (copy.nick, copy.highlight, copy.cached) = (format!("{room}/{nick}"), true, false);
        let idx = self.mentions_window();
        let w = &mut self.windows[idx];
        // In time order, after the window's own notes.
        let at = w.lines.iter().rposition(|l| l.event_id.is_none() || l.ts <= copy.ts).map_or(0, |p| p + 1);
        w.lines.insert(at, copy);
        while w.lines.len() > MENTIONS_MAX {
            if let Some(old) = w.lines.pop_front().and_then(|l| l.event_id) {
                self.mention_origins.remove(&old);
            }
        }
        self.mention_origins.insert(event, key);
        if alert && !self.on_screen(idx) {
            self.windows[idx].activity = Activity::Highlight;
        }
    }

    /// Mentions found in loaded history (the cache, scrolling back) join the
    /// window quietly.
    pub(super) fn record_old_mentions(&mut self, account: &str, room_id: &OwnedRoomId, items: &[Item]) {
        let Some(idx) = self.window_idx(account, room_id) else { return };
        for item in items {
            if let Item::Line(line) = item
                && is_mention(line)
            {
                self.record_mention(idx, line, false);
            }
        }
    }

    /// Back from /away, irssi's awaylog: what came in meanwhile, and the
    /// mentions window, which collected it, to read it in.
    pub(super) fn back_from_away(&mut self) {
        let Some(since) = self.away_since.take() else { return };
        let gone = short_duration((Local::now() - since).to_std().unwrap_or_default());
        let (mut mentions, mut dms) = (0, 0);
        let (mut rooms, mut people): (Vec<String>, Vec<String>) = Default::default();
        let mentions_idx = self.windows.iter().position(|w| w.is_mentions);
        let caught = mentions_idx.map(|i| &self.windows[i].lines).into_iter().flatten();
        for line in caught.filter(|l| l.ts >= since) {
            let Some(from) = line.event_id.as_ref().and_then(|e| self.mention_origins.get(e)) else { continue };
            let Some(w) = self.window_idx(&from.account, &from.room_id).map(|i| &self.windows[i]) else { continue };
            // A DM's window is named for the person.
            let (count, names) = if w.is_dm { (&mut dms, &mut people) } else { (&mut mentions, &mut rooms) };
            *count += 1;
            if !names.iter().any(|n| n == w.title()) {
                names.push(w.title().to_owned());
            }
        }
        let part = |n: usize, one: &str, many: &str, names: &[String]| match n {
            0 => None,
            1 => Some(format!("1 {one} ({})", names.join(", "))),
            n => Some(format!("{n} {many} ({})", names.join(", "))),
        };
        let parts: Vec<String> = [part(mentions, "mention", "mentions", &rooms), part(dms, "DM", "DMs", &people)]
            .into_iter()
            .flatten()
            .collect();
        if parts.is_empty() {
            self.status(Line::info(format!("while you were away ({gone}): nothing for you")));
            return;
        }
        let text = format!("while you were away ({gone}): {}, in the mentions window", parts.join(" and "));
        self.status(Line::info(text));
        if let Some(idx) = mentions_idx {
            self.switch_to(idx);
        }
    }

    /// Fetch avatars for rooms in the window list that don't have one yet
    /// (or whose avatar changed).
    pub(super) fn request_avatars(&mut self) {
        let Some(picker) = self.picker.clone().filter(|_| self.show_sidebar && self.avatar_images()) else { return };
        let wanted: Vec<(String, OwnedRoomId, OwnedMxcUri)> = self
            .windows
            .iter()
            .filter_map(|w| {
                let key = w.room.as_ref().filter(|k| k.thread.is_none())?;
                let url = w.avatar.clone()?;
                let current = self.avatars.get(&(key.account.clone(), key.room_id.clone()));
                (current.is_none_or(|(have, _)| *have != url)).then(|| (key.account.clone(), key.room_id.clone(), url))
            })
            .collect();
        for (account, room_id, url) in wanted {
            let Some(acct) = self.connected(&account) else { continue };
            self.avatars.insert((account, room_id.clone()), (url.clone(), Preview::Loading));
            acct.fetch_avatar(room_id, url, picker.clone(), Size::new(2, 1));
        }
    }

    /// Say how pictures are drawn in this terminal.
    pub fn note_images(&mut self) {
        let Some(painter) = &self.picker else { return };
        let note = if painter.graphics() {
            format!("images: {}", painter.describe())
        } else {
            format!(
                "images: {}, as this terminal can't show pictures (kitty, Ghostty, WezTerm and iTerm2 can); \
                 image_text in /set changes how",
                painter.describe()
            )
        };
        self.status(Line::info(note));
    }

    /// Whether to notify: as the terminal allows, or, in the background
    /// with no terminal attached, through the system (unless notifications
    /// are off altogether).
    pub(super) fn notifying(&self) -> bool {
        if crate::term::attached() {
            self.notify_mode != NotifyMode::Off
        } else {
            self.cfg.notifications != NotifyMode::Off
        }
    }

    /// Whether you've been idle long enough for auto-away: it's on, it
    /// isn't a bot (no one at its keyboard), and you aren't away already
    /// (your own /away stays as it is).
    pub(super) fn idle_for_away(&self) -> bool {
        let minutes = self.cfg.auto_away;
        minutes > 0
            && !self.auto_away
            && !self.is_bot()
            && !self.accounts.values().any(|s| s.away.is_some())
            && self.last_input.elapsed() >= Duration::from_secs(u64::from(minutes) * 60)
    }

    /// A bot's Textrix (`~/.textrix/bots/<name>`).
    pub(super) fn is_bot(&self) -> bool {
        self.paths.base.parent().and_then(|p| p.file_name()).is_some_and(|n| n == "bots")
    }

    /// Auto-away: away on every connected server once you've been idle.
    pub(super) fn check_idle(&mut self) {
        if !self.idle_for_away() {
            return;
        }
        let accounts: Vec<Account> = self.accounts.values().filter_map(|s| s.account.clone()).collect();
        if accounts.is_empty() {
            return;
        }
        self.auto_away = true;
        for acct in accounts {
            acct.set_away(Some(self.cfg.auto_away_message.clone()));
        }
    }

    pub fn on_clock(&mut self) {
        self.check_idle();
        if self.recording.as_ref().is_some_and(|(_, r)| r.started.elapsed() >= crate::voice::MAX) {
            self.info("that's ten minutes, the most a voice message can be: sending it");
            self.stop_voice(true);
        }
        // A server that never connects shouldn't hold the split back.
        self.restore_split();
        self.send_due_later();
        self.clock_ticks += 1;
        if self.clock_ticks.is_multiple_of(30) {
            self.watch_install();
        }
        let (effects, mut errors) = self.scripts.refresh_status();
        self.apply_effects(effects, None);
        let (effects, timer_errors) = self.scripts.run_timers();
        self.apply_effects(effects, None);
        errors.extend(timer_errors);
        errors.extend(self.scripts.save_stores());
        for e in errors {
            self.status(Line::error(format!("script {e}")));
        }
    }

    pub fn after_draw(&mut self) {
        self.request_avatars();
        for (account, url) in std::mem::take(&mut self.link_title_requests) {
            match self.connected(&account) {
                Some(acct) if !self.link_titles_off.contains(&account) => {
                    let (w, h) = LINK_PICTURE;
                    let picture = self.picker.clone().filter(|_| self.cfg.link_images).map(|p| (p, Size::new(w, h)));
                    acct.fetch_link_title(url, picture)
                }
                _ => drop(self.link_titles.remove(&url)),
            }
        }
        if let (Some(v), Some(picker)) = (self.viewer.as_mut(), self.picker.clone())
            && let Some((image, view)) = v.wanted()
        {
            v.pending = Some(view);
            let (tx, event_id) = (self.tx.clone(), v.event.clone());
            tokio::task::spawn_blocking(move || {
                let protocol = viewer::render(&image, &view, &picker).map_err(|e| format!("{e:#}"));
                let _ = tx.send(AppEvent::ViewerFrame { event_id, view, protocol });
            });
        }
        // Typing notices normally end with an update from the server; don't
        // let a missed one leave someone "typing" forever.
        for w in &mut self.windows {
            if !w.typing.is_empty() && w.typing_at.is_some_and(|t| t.elapsed() > TYPING_EXPIRY) {
                w.typing.clear();
            }
        }
        if !self.preview_requests.is_empty()
            && let Some(picker) = self.picker.clone()
        {
            let cells = Size::new(self.view_width.saturating_sub(8).clamp(8, 64), self.cfg.image_rows.max(2));
            for (account, event_id, media) in std::mem::take(&mut self.preview_requests) {
                match self.connected(&account) {
                    Some(acct) => {
                        acct.fetch_preview(event_id, media, picker.clone(), cells, self.cfg.image_animation);
                    }
                    // Asked again once the server is back.
                    None => drop(self.previews.remove(&event_id)),
                }
            }
        }
        if let Some(g) = &self.gallery
            && !self.tile_requests.is_empty()
        {
            let (w, h) = gallery::PICTURE;
            match (self.picker.clone(), self.connected(&g.key.account)) {
                (Some(picker), Some(acct)) => {
                    for (event_id, media) in std::mem::take(&mut self.tile_requests) {
                        acct.fetch_tile(event_id, media, picker.clone(), Size::new(w, h));
                    }
                }
                // Asked again once the server is back.
                _ => {
                    let asked = std::mem::take(&mut self.tile_requests);
                    if let Some(g) = self.gallery.as_mut() {
                        asked.iter().for_each(|(id, _)| drop(g.pictures.remove(id)));
                    }
                }
            }
        }
        for idx in self.screen_windows() {
            self.window_upkeep(idx);
        }
    }

    /// Members, older history and read markers for a window on screen.
    pub(super) fn window_upkeep(&mut self, idx: usize) {
        let page = self.cfg.history_page;
        let receipts = self.cfg.read_receipts;
        let focused = idx == self.active;
        let Some(key) = self.windows[idx].room.clone() else { return };
        let Some(acct) = self.connected(&key.account) else { return };
        // Offline, older history can't come; what's cached is there.
        let online = self.accounts.get(key.account.as_str()).is_some_and(|a| a.status == ConnStatus::Online);
        let w = &mut self.windows[idx];

        if !w.members_requested {
            w.members_requested = true;
            acct.members(key.room_id.clone(), false);
        }

        // A /jump target that isn't loaded yet: keep paging back until it is.
        if let Some(target) = w.seek.clone() {
            if w.contains(&target) {
                w.seek = None;
                if focused {
                    self.select = Some(target);
                }
                return;
            }
            if matches!(w.history, History::Done) {
                w.seek = None;
                self.info("couldn't reach that message in this room's history");
                return;
            }
        }
        let retry_ok = w.history_retry.is_none_or(|t| Instant::now() >= t);
        if (w.at_top || w.seek.is_some() || w.wants_older) && retry_ok && online {
            let from = match &w.history {
                History::Unloaded => Some(None),
                History::More(token) => Some(Some(token.clone())),
                History::Loading | History::Done => None,
            };
            if let Some(from) = from {
                w.history = History::Loading;
                acct.paginate(key.room_id.clone(), from, page);
            }
        }

        if w.scroll == 0
            && w.thread_root().is_none()
            && let Some(event_id) = w.latest_event()
            && w.last_receipt.as_ref() != Some(&event_id)
        {
            w.last_receipt = Some(event_id.clone());
            acct.mark_read(key.room_id, event_id, receipts);
        }
    }

    /// The releases `/changelog` asked for, oldest first, so the newest is
    /// at the bottom where you're reading; each its notes as formatted.
    pub(super) fn changelog_done(&mut self, into: Option<WinKey>, releases: Result<Vec<update::Release>, String>) {
        let idx = into.as_ref().and_then(|k| self.windows.iter().position(|w| w.room.as_ref() == Some(k))).unwrap_or(0);
        let cap = self.cfg.scrollback;
        let releases = match releases {
            Ok(releases) if !releases.is_empty() => releases,
            Ok(_) => {
                self.windows[idx].push(Line::info("GitHub lists no releases"), cap);
                return;
            }
            Err(e) => {
                self.windows[idx].push(Line::error(format!("couldn't get the changelog: {e}")), cap);
                return;
            }
        };
        let current = env!("CARGO_PKG_VERSION");
        let intro = format!(
            "the latest {} releases, oldest first (all of them: https://github.com/seamoss/Textrix/releases)",
            releases.len()
        );
        self.windows[idx].push(Line::info(intro), cap);
        for release in releases.iter().rev() {
            let note = if release.version == current {
                " · the one you're running"
            } else if update::newer(&release.version, current) {
                " · newer than yours: brew update && brew upgrade textrix"
            } else {
                ""
            };
            let notes = update::unwrapped(&release.notes);
            let text = format!("**Textrix {}** · {}{note}\n\n{notes}", release.version, release.date);
            let mut line = Line::info(text.clone());
            line.rich = matrix_sdk::ruma::events::room::message::FormattedBody::markdown(&text)
                .and_then(|html| crate::richtext::render(&html.body));
            self.windows[idx].push(line, cap);
        }
    }

    pub(super) fn pasted_image(&mut self, image: Result<Option<PathBuf>, String>, picture: Option<Picture>) {
        let file = match image {
            Ok(Some(file)) => file,
            Ok(None) => return self.info("there's no image on the clipboard (text pastes as usual)"),
            Err(e) => return self.error(e),
        };
        let kind = file.extension().map(|e| e.to_string_lossy().to_uppercase()).unwrap_or_default();
        let size = std::fs::metadata(&file).map(|m| matrix::human_size(m.len())).unwrap_or_default();
        let summary = match imagesize::size(&file) {
            Ok(dims) => format!("{}×{} {kind}, {size}", dims.width, dims.height),
            Err(_) => format!("{kind}, {size}"),
        };
        self.drop_pasted();
        self.pasted = Some(Pasted { file, summary, picture });
    }

    /// A recording, done: shown as it'll look, and sent.
    pub(super) fn voice_done(&mut self, key: WinKey, voice: Result<Option<crate::voice::Voice>, String>) {
        let voice = match voice {
            Ok(Some(voice)) => voice,
            Ok(None) => return self.info("voice message thrown away"),
            Err(e) => return self.error(e),
        };
        let Some(acct) = self.connected(&key.account) else {
            let _ = std::fs::remove_file(&voice.file);
            return self.error(format!("{} isn't connected; the voice message is gone", key.account));
        };
        let bars: Vec<u64> = voice.waveform.iter().map(|v| (v * 1024.0) as u64).collect();
        let text = format!("[voice] {} {}", audio::duration(voice.duration), audio::waveform(&bars, 24));
        let mut line = Line::new(LineKind::Message, text);
        (line.own, line.pending, line.progress) = (true, true, Some(0));
        let txn_id = TransactionId::new();
        line.txn_id = Some(txn_id.clone());
        (line.nick, line.sender) = (self.own_nick(&acct), Some(acct.user_id.clone()));
        if let Some(idx) = self.window_idx(&key.account, &key.room_id) {
            let cap = self.cfg.scrollback;
            self.windows[idx].push(line, cap);
        }
        acct.upload_voice(key.room_id, voice.file, txn_id, voice.duration, voice.waveform);
    }

    /// `notify_command`, for one notification: `sh -c` with the details in
    /// its environment (never in the command, so a message can't inject
    /// anything). A few at a time; each gets 30 seconds. A failure is
    /// reported once, until one works again.
    pub(super) fn run_notify_command(&mut self, vars: Vec<(&'static str, String)>) {
        const AT_ONCE: usize = 4;
        let command = self.cfg.notify_command.clone();
        let (running, failing) = (self.hooks.0.clone(), self.hooks.1.clone());
        if running.fetch_add(1, Ordering::SeqCst) >= AT_ONCE {
            running.fetch_sub(1, Ordering::SeqCst);
            tracing::warn!("notify_command: {AT_ONCE} already running, skipping one");
            return;
        }
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let mut child = tokio::process::Command::new("sh");
            child.arg("-c").arg(&command).envs(vars).kill_on_drop(true);
            child.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null());
            child.stderr(std::process::Stdio::piped());
            let outcome = match tokio::time::timeout(EXEC_LIMIT, child.output()).await {
                Ok(Ok(out)) if out.status.success() => Ok(()),
                Ok(Ok(out)) => {
                    let said = String::from_utf8_lossy(&out.stderr);
                    let said = said.lines().next().map(|l| format!(": {}", strip_escapes(l))).unwrap_or_default();
                    Err(format!("{}{said}", out.status))
                }
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err(format!("still running after {}s, stopped", EXEC_LIMIT.as_secs())),
            };
            running.fetch_sub(1, Ordering::SeqCst);
            match outcome {
                Ok(()) => failing.store(false, Ordering::SeqCst),
                Err(e) if !failing.swap(true, Ordering::SeqCst) => {
                    let line = Line::error(format!("notify_command failed ({e}); said once until it works again"));
                    let _ = tx.send(AppEvent::Notice { account: None, room_id: None, line });
                }
                Err(_) => {}
            }
        });
    }

    pub(super) fn np_done(&mut self, into: Option<WinKey>, track: Result<crate::np::Track, String>) {
        let idx = into.as_ref().and_then(|k| self.windows.iter().position(|w| w.room.as_ref() == Some(k)));
        let cap = self.cfg.scrollback;
        let track = match track {
            Ok(track) => track,
            Err(e) => {
                self.windows[idx.unwrap_or(0)].push(Line::error(format!("/np: {e}")), cap);
                return;
            }
        };
        let (text, action) = track.say(&self.cfg.np_format);
        let (Some(idx), Some(key)) = (idx, into) else {
            let from = if track.player.is_empty() { String::new() } else { format!(" ({})", track.player) };
            return self.status(Line::info(format!("now playing: {}{from}", track.line())));
        };
        if text.is_empty() {
            self.windows[idx].push(Line::error("/np: np_format came out empty"), cap);
            return;
        }
        let kind = if action { Say::Emote } else { Say::Text };
        if self.active == idx {
            // As typed here: into its thread, when it's a thread window.
            return self.say_as_is(&text, kind);
        }
        match self.connected(&key.account) {
            Some(acct) => {
                let content = compose(&text, kind, &[], false);
                self.send_content(acct, key.room_id, content, &text, kind.line_kind(), None);
            }
            None => {
                self.windows[idx].push(Line::error(format!("{} isn't connected", key.account)), cap);
            }
        }
    }

    /// `/exec`'s output: shown in the window it ran in, or sent there.
    pub(super) fn exec_done(
        &mut self,
        into: Option<WinKey>,
        send: bool,
        command: &str,
        output: Result<(String, Option<i32>), String>,
    ) {
        let idx = into.as_ref().and_then(|k| self.windows.iter().position(|w| w.room.as_ref() == Some(k))).unwrap_or(0);
        let cap = self.cfg.scrollback;
        let (text, code) = match output {
            Ok(done) => done,
            Err(e) => {
                self.windows[idx].push(Line::error(format!("/exec {command}: {e}")), cap);
                return;
            }
        };
        let mut notes = Vec::new();
        if send && let Some(key) = into.filter(|_| !text.trim().is_empty()) {
            match self.connected(&key.account) {
                Some(acct) => {
                    // Several lines go as a code block, to keep their shape.
                    let body = if text.trim_end().contains('\n') {
                        format!("```\n{}\n```", text.trim_end())
                    } else {
                        text.trim().to_owned()
                    };
                    acct.send(key.room_id.clone(), compose(&body, Say::Text, &[], true), TransactionId::new());
                }
                None => notes.push(Line::error(format!("{} isn't connected", key.account))),
            }
        } else if text.trim().is_empty() {
            notes.push(Line::info(format!("/exec {command}: no output")));
        } else {
            notes.extend(text.lines().map(Line::info));
        }
        if let Some(code) = code.filter(|c| *c != 0) {
            notes.push(Line::error(format!("/exec {command}: exit {code}")));
        }
        for note in notes {
            self.windows[idx].push(note, cap);
        }
    }

    /// Save `account`'s scheduled messages (sealed, where it can).
    pub(super) fn keep_later(&self, account: &AccountName) {
        if let Some(acct) = self.connected(account) {
            acct.save_later(self.later.iter().filter(|(a, _)| a == account).map(|(_, s)| s.clone()).collect());
        }
    }

    /// Send scheduled messages that are due, on servers that are connected.
    pub(super) fn send_due_later(&mut self) {
        let now = Local::now().timestamp();
        let due: Vec<usize> = (0..self.later.len())
            .filter(|&i| self.later[i].1.due <= now && self.connected(&self.later[i].0).is_some())
            .collect();
        let mut accounts = Vec::new();
        for i in due.into_iter().rev() {
            let (account, item) = self.later.remove(i);
            let Some(acct) = self.connected(&account) else { continue };
            let content =
                self.with_custom_emoji(&account, &item.room_id, compose(&item.text, Say::Text, &[], self.cfg.markdown));
            acct.send(item.room_id.clone(), content, TransactionId::new());
            let note = Line::info(format!("sent your scheduled message: {}", snippet(&item.text)));
            match self.window_idx(&account, &item.room_id) {
                Some(idx) => {
                    let cap = self.cfg.scrollback;
                    self.windows[idx].push(note, cap);
                }
                None => self.status(note),
            }
            accounts.push(account);
        }
        accounts.dedup();
        for account in accounts {
            self.keep_later(&account);
        }
    }

    /// After `/restart`: back to the window you were in, with what you were
    /// typing, once that window's there (or its server has said it isn't).
    pub(super) fn resume(&mut self) {
        let Some(mut resume) = self.pending_resume.take() else { return };
        let open = |app: &Self, r: &WinRef| app.windows.iter().position(|w| w.id() == *r);
        // A room whose server hasn't said what rooms it has may still come.
        let coming = |app: &Self, r: &WinRef| {
            matches!(r, WinRef::Room(key)
                if !app.rooms_loaded.contains(&key.account)
                    && app.accounts.get(key.account.as_str()).is_some_and(|s| s.status != ConnStatus::Offline))
        };
        // The window first: switching saves the input line into the window
        // left, which would cover a draft put back there.
        if let Some(window) = resume.window.take() {
            match open(self, &window) {
                Some(idx) => self.switch_to(idx),
                None if coming(self, &window) => resume.window = Some(window),
                None => {}
            }
        }
        let mut later = Vec::new();
        for (window, text) in std::mem::take(&mut resume.drafts) {
            match open(self, &window) {
                // The window you're in has its draft in the input line.
                Some(idx) if idx == self.active => self.input.restore(crate::input::Draft::from_text(&text)),
                Some(idx) => {
                    let draft = Some(crate::input::Draft::from_text(&text));
                    self.windows[idx].unsent = Some(Unsent { text: draft, compose: None });
                }
                None if coming(self, &window) => later.push((window, text)),
                None => {}
            }
        }
        resume.drafts = later;
        if resume.window.is_some() || !resume.drafts.is_empty() {
            self.pending_resume = Some(resume);
        }
    }

    /// Every half minute: has the textrix on disk changed (`brew upgrade`,
    /// `cargo build`)? Then ask it its version.
    pub(super) fn watch_install(&mut self) {
        let Some(install) = self.install.clone().filter(|i| i.replaced()) else { return };
        if std::mem::replace(&mut self.probing, true) {
            return;
        }
        // From now on, changes are measured from this one.
        self.install = crate::install::Install::at(install.path.clone()).or(Some(install.clone()));
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let version = install.version().await;
            let _ = tx.send(AppEvent::Installed { version });
        });
    }

    pub fn upgrade_note(&self) -> Option<String> {
        match (&self.installed, &self.released) {
            (Some(v), _) if v.is_empty() => Some("new build: /restart".into()),
            (Some(v), _) => Some(format!("{v} installed: /restart")),
            (None, Some(v)) => Some(format!("{v} is out")),
            _ => None,
        }
    }
}
