//! Commands: the dispatcher every /command goes through, and the room, space, search, thread and other commands.

use super::*;

impl App {
    /// `/script [list|load <name>|unload <name>|reload [name]|exec <code>]`.
    pub(super) fn cmd_script(&mut self, args: &str) -> Result<(), String> {
        let (sub, rest) = split_first(args.trim());
        let rest = rest.trim();
        match sub {
            "" | "list" => {
                let scripts = self.scripts.list();
                if scripts.is_empty() {
                    self.info(format!("no scripts loaded; put .rhai files in {}", self.scripts_dir().display()));
                }
                for (name, path, commands, hooks) in scripts {
                    let commands: Vec<String> = commands.iter().map(|c| format!("/{c}")).collect();
                    let commands = if commands.is_empty() { "no commands".to_owned() } else { commands.join(" ") };
                    let hooks = if hooks > 0 { format!(", {hooks} message hook(s)") } else { String::new() };
                    let from = path.map(|p| format!(" ({})", p.display())).unwrap_or_default();
                    self.info(format!("  {name}: {commands}{hooks}{from}"));
                }
            }
            "load" if !rest.is_empty() => {
                let path = if rest.contains('/') || rest.ends_with(".rhai") {
                    parse_path(rest)
                } else {
                    self.scripts_dir().join(format!("{rest}.rhai"))
                };
                self.load_script(&path);
            }
            "unload" if !rest.is_empty() => {
                if !self.scripts.unload(rest) {
                    return Err(format!("no script {rest} loaded"));
                }
                self.info(format!("script {rest} unloaded"));
            }
            "reload" => {
                let paths: Vec<PathBuf> = if rest.is_empty() {
                    Scripts::available(&self.scripts_dir())
                } else {
                    vec![self.scripts.path_of(rest).ok_or_else(|| format!("no script {rest} loaded from a file"))?]
                };
                for path in paths {
                    self.load_script(&path);
                }
            }
            "exec" if !rest.is_empty() => {
                let effects = self.scripts.exec(rest).map_err(|e| format!("script: {e}"))?;
                let here = self.win().room.clone();
                self.apply_effects(effects, here);
            }
            _ => return Err("usage: /script [list | load <name> | unload <name> | reload [name] | exec <code>]".into()),
        }
        Ok(())
    }

    /// `/favourite [room]`: this room or DM (or the one named) a favourite,
    /// or not.
    pub(super) fn cmd_favourite(&mut self, args: &str) -> Result<(), String> {
        let idx = match args.trim() {
            "" => self.active,
            name => self.find_window(name)?,
        };
        let key = self.windows[idx].room.clone().ok_or("only rooms and DMs can be favourites")?;
        self.toggle_favourite(&key.account, &key.room_id)
    }

    /// Make a room or DM a favourite, or not: first in the window list,
    /// and in Element's Favourites everywhere (the `m.favourite` tag).
    pub(super) fn toggle_favourite(&mut self, account: &str, room_id: &OwnedRoomId) -> Result<(), String> {
        let acct = self.connected(account).ok_or_else(|| format!("{account} isn't connected"))?;
        let idx = self.window_idx(account, room_id);
        let (was, name) = match idx {
            Some(i) => (self.windows[i].favourite, self.windows[i].title().to_owned()),
            None => {
                let meta = acct.quick_meta(room_id).ok_or("that room isn't available")?;
                (meta.favourite, meta.name)
            }
        };
        acct.set_favourite(room_id.clone(), !was);
        if let Some(i) = idx {
            self.windows[i].favourite = !was;
        }
        self.info(if was {
            format!("{name} isn't a favourite any more")
        } else {
            format!("★ {name} is a favourite: first in the window list, and in Element's Favourites")
        });
        Ok(())
    }

    /// `/unread [room]`: this room (or another) marked unread, to come back
    /// to, or not; `m.marked_unread`, so the same in your other clients.
    pub(super) fn cmd_unread(&mut self, args: &str) -> Result<(), String> {
        let idx = match args.trim() {
            "" => self.active,
            name => self.find_window(name)?,
        };
        let key = self.windows[idx].room.clone().filter(|k| k.thread.is_none()).ok_or("that's for rooms and DMs")?;
        let acct = self.connected(&key.account).ok_or_else(|| format!("{} isn't connected", key.account))?;
        let w = &mut self.windows[idx];
        w.marked_unread = !w.marked_unread;
        acct.set_marked_unread(key.room_id, w.marked_unread);
        let name = w.title().to_owned();
        self.info(if self.windows[idx].marked_unread {
            format!("{name} is marked unread: in Unreads until you open it again")
        } else {
            format!("{name} isn't marked unread any more")
        });
        Ok(())
    }

    /// Opened (or read): a room marked unread isn't any more.
    pub(super) fn clear_marked_unread(&mut self, idx: usize) {
        let Some(w) = self.windows.get_mut(idx) else { return };
        let Some(key) = w.room.clone().filter(|k| k.thread.is_none() && w.marked_unread) else { return };
        w.marked_unread = false;
        if let Some(acct) = self.connected(&key.account) {
            acct.set_marked_unread(key.room_id, false);
        }
    }

    /// `/nickname [who [name]]`: your own name for someone, shown in place of
    /// theirs everywhere in Textrix and never sent anywhere.
    pub(super) fn cmd_nickname(&mut self, args: &str) -> Result<(), String> {
        let (who, name) = split_first(args.trim());
        if who.is_empty() {
            if self.layout.nicknames.is_empty() {
                self.info("no nicknames yet; /nickname <nick|@user> <name> gives someone one");
            }
            let rows: Vec<String> = self.layout.nicknames.iter().map(|(u, n)| format!("  {n} → {u}")).collect();
            rows.into_iter().for_each(|row| self.info(row));
            return Ok(());
        }
        let user = self.resolve_user(who)?;
        let name: String = name.trim().chars().filter(|c| !c.is_control()).take(48).collect();
        let text = if name.is_empty() {
            match self.layout.nicknames.remove(&user) {
                Some(old) => format!("{user} is no longer “{old}” here"),
                None => return Err(format!("you haven't given {user} a nickname")),
            }
        } else {
            self.layout.nicknames.insert(user.clone(), name.clone());
            format!("{user} shows as “{name}” here (only you see it; /nickname {who} takes it off)")
        };
        for w in &mut self.windows {
            if w.dm_user.as_ref() == Some(&user) {
                w.person = self.layout.nicknames.get(&user).cloned();
            }
        }
        self.save_layout();
        self.info(text);
        Ok(())
    }

    /// `/changelog [N]`: what changed in the latest N releases, from GitHub.
    pub(super) fn cmd_changelog(&mut self, args: &str) -> Result<(), String> {
        let count = match args.trim() {
            "" => 10,
            n => n.parse().ok().filter(|n| (1..=50).contains(n)).ok_or("usage: /changelog [1–50]")?,
        };
        let (tx, into) = (self.tx.clone(), self.win().room.clone());
        self.info(format!("asking GitHub for the latest {count} releases…"));
        tokio::spawn(async move {
            let releases = update::releases(count).await.map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::Changelog { into, releases });
        });
        Ok(())
    }

    pub(super) fn command(&mut self, line: &str) {
        let (name, args) = split_first(line);
        let name = name.to_lowercase();
        let name = ALIASES.iter().find(|(alias, _)| *alias == name).map_or(name.as_str(), |(_, real)| *real);
        let result = match name {
            "help" => {
                self.cmd_help(args);
                Ok(())
            }
            "say" => {
                self.say(args, Say::Text);
                Ok(())
            }
            "me" => {
                self.say(args, Say::Emote);
                Ok(())
            }
            "notice" => {
                self.say(args, Say::Notice);
                Ok(())
            }
            "np" => self.cmd_np(args),
            "msg" => self.cmd_msg(args),
            "reply" => self.cmd_reply(args),
            "edit" if args.trim().is_empty() => {
                self.start_edit(None);
                Ok(())
            }
            "edit" => self.cmd_edit(args),
            "redact" => self.cmd_redact(),
            "react" => self.cmd_react(args),
            "upload" => self.cmd_upload(args),
            "lastlog" => self.cmd_lastlog(args),
            "search" => self.cmd_search(args),
            "jump" => self.cmd_jump(args),
            "urls" => {
                self.cmd_urls();
                Ok(())
            }
            "open" => self.cmd_open(args),
            "list" => self.open_directory(args),
            "away" => self.cmd_away(args),
            "notify" => self.cmd_notify(args),
            "script" => self.cmd_script(args),
            "version" => {
                self.info(format!("Textrix {}", env!("CARGO_PKG_VERSION")));
                match args.trim() {
                    "check" => {
                        let tx = self.tx.clone();
                        tokio::spawn(async move {
                            let latest = update::latest().await.ok();
                            let _ = tx.send(AppEvent::Update { latest, asked: true });
                        });
                        Ok(())
                    }
                    "" => Ok(()),
                    _ => Err("usage: /version [check]".into()),
                }
            }
            "avatar" => self.pick_account(args).and_then(|(acct, image)| {
                let image = avatar_arg(image, "/avatar")?;
                acct.set_avatar(self.here(), image);
                Ok(())
            }),
            "roomavatar" => {
                let image = avatar_arg(args, "/roomavatar");
                image.and_then(|image| self.room_ctx().map(|(acct, room_id)| acct.set_room_avatar(room_id, image)))
            }
            "set" => self.cmd_set(args),
            "save" => self.cmd_save(),
            "nickname" => self.cmd_nickname(args),
            "bell" => self.cmd_bell(args),
            "keys" => self.cmd_keys(args),
            "forward" => self.cmd_forward(args),
            "markread" => self.cmd_markread(args),
            "theme" => self.cmd_theme(args),
            "poll" => self.cmd_poll(args),
            "later" => self.cmd_later(args),
            "exec" => self.cmd_exec(args),
            "voice" => self.start_voice(),
            "paste" => self.paste_image(),
            "plugin" | "plugins" => self.cmd_plugin(args),
            "bot" | "bots" => self.cmd_bot(args),
            "codebot" => self.cmd_codebot(args),
            "changelog" => self.cmd_changelog(args),
            "themes" if args.trim().is_empty() => {
                self.open_theme_picker();
                Ok(())
            }
            "themes" => self.cmd_theme(args),
            "devices" => self.cmd_devices(args),
            "hilight" => self.pick_account(args).map(|(acct, word)| match word.trim() {
                "" => acct.list_highlights(self.here()),
                word => acct.set_highlight(self.here(), word.to_owned(), true),
            }),
            "dehilight" => self.pick_account(args).and_then(|(acct, word)| {
                if word.trim().is_empty() {
                    return Err("usage: /dehilight <word>".to_owned());
                }
                acct.set_highlight(self.here(), word.trim().to_owned(), false);
                Ok(())
            }),
            "ignore" => self.pick_account(args).and_then(|(acct, who)| {
                match who.trim() {
                    "" => acct.list_ignored(self.here()),
                    who => acct.ignore(self.here(), self.resolve_user(who)?, true),
                }
                Ok(())
            }),
            "unignore" => self.pick_account(args).and_then(|(acct, who)| {
                let who = who.trim();
                if who.is_empty() {
                    return Err("usage: /unignore <@user>".into());
                }
                acct.ignore(self.here(), self.resolve_user(who)?, false);
                Ok(())
            }),
            "download" => self.cmd_download(args),
            "play" => self.cmd_play(args),
            "join" => self.pick_account(args).and_then(|(acct, target)| {
                let target = target.trim();
                if target.is_empty() {
                    return Err("usage: /join <#alias:server|!room>".into());
                }
                acct.join(target.to_owned());
                Ok(())
            }),
            "create" => self.cmd_create(args),
            "spaces" => self.cmd_spaces(),
            "space" => self.cmd_space(args),
            "name" => self.room_ctx().and_then(|(acct, room_id)| {
                let name = args.trim();
                if name.is_empty() {
                    return Err("usage: /name <new name>".into());
                }
                acct.rename(room_id, name.to_owned());
                Ok(())
            }),
            "info" => self.room_ctx().map(|(acct, room_id)| acct.room_info(room_id)),
            "mentions" => {
                let idx = self.mentions_window();
                self.switch_to(idx);
                Ok(())
            }
            "media" => self.open_gallery(gallery::Tab::Media),
            "links" => self.open_gallery(gallery::Tab::Links),
            "favourite" => self.cmd_favourite(args),
            "unread" => self.cmd_unread(args),
            "home" => {
                self.open_home();
                Ok(())
            }
            "pins" => self.room_ctx().map(|(acct, room_id)| acct.pins(room_id)),
            "joinrule" => self.cmd_joinrule(args),
            "guests" => self.room_ctx().and_then(|(acct, room_id)| {
                acct.set_guest_access(room_id, on_off(args, "usage: /guests <on|off>")?);
                Ok(())
            }),
            "history" => self.room_ctx().and_then(|(acct, room_id)| {
                let visibility = match args.trim() {
                    "joined" => HistoryVisibility::Joined,
                    "invited" => HistoryVisibility::Invited,
                    "shared" => HistoryVisibility::Shared,
                    "world" | "world_readable" => HistoryVisibility::WorldReadable,
                    _ => return Err("usage: /history <joined|invited|shared|world>".into()),
                };
                acct.set_history_visibility(room_id, visibility);
                Ok(())
            }),
            "publish" => self.room_ctx().and_then(|(acct, room_id)| {
                acct.set_published(room_id, on_off(args, "usage: /publish <on|off>")?);
                Ok(())
            }),
            // In a thread window these close it, never the room.
            "close" | "part" if self.win().thread_root().is_some() => {
                self.close_thread();
                Ok(())
            }
            "close" => self.room_ctx().map(|(acct, room_id)| acct.close_room(room_id, args.trim() == "confirm")),
            "upgrade" => self.room_ctx().map(|(acct, room_id)| {
                let (version, rest) = split_first(args);
                let version = Some(version).filter(|v| !v.is_empty()).map(str::to_owned);
                acct.upgrade(room_id, version, rest.trim() == "confirm");
            }),
            "rooms" => self.pick_account(args).map(|(acct, _)| acct.list_rooms(self.here())),
            "invites" => self.pick_account(args).map(|(acct, _)| acct.list_invites(self.here())),
            "decline" => self.pick_account(args).and_then(|(acct, room)| {
                let room_id =
                    RoomId::parse(room.trim()).map_err(|_| "usage: /decline <!room:server> (see /invites)")?;
                acct.decline(self.here(), room_id);
                Ok(())
            }),
            "part" => self.room_ctx().map(|(acct, room_id)| acct.leave(room_id)),
            "thread" => self.cmd_thread(args),
            "threads" => self.room_ctx().map(|(acct, room_id)| acct.list_threads(room_id)),
            "forget" => self.room_ctx().map(|(acct, room_id)| acct.forget(room_id)),
            "query" => self.pick_account(args).and_then(|(acct, who)| {
                let user = self.resolve_user(who.trim())?;
                acct.query(user);
                Ok(())
            }),
            "topic" => self.cmd_topic(args),
            "names" => self.room_ctx().map(|(acct, room_id)| acct.members(room_id, true)),
            "whois" => self.pick_account(args).and_then(|(acct, who)| {
                let user = self.resolve_user(who.trim())?;
                acct.whois(self.here(), user);
                Ok(())
            }),
            "nick" => self.pick_account(args).and_then(|(acct, nick)| {
                let nick = nick.trim();
                if nick.is_empty() {
                    return Err("usage: /nick <name>".into());
                }
                acct.set_nick(nick.to_owned());
                Ok(())
            }),
            "invite" => self.cmd_moderate(Moderation::Invite, args),
            "kick" => self.cmd_moderate(Moderation::Kick, args),
            "ban" => self.cmd_moderate(Moderation::Ban, args),
            "unban" => self.cmd_moderate(Moderation::Unban, args),
            "window" => self.cmd_window(args),
            "split" => self.cmd_split(args, false),
            "vsplit" => self.cmd_split(args, true),
            "unsplit" => self.cmd_unsplit(args),
            "resize" => self.cmd_resize(args),
            "layout" => self.cmd_layout(args),
            "wc" => self.cmd_window("close"),
            "wl" => self.cmd_window("list"),
            "clear" => {
                let w = self.win_mut();
                w.lines.clear();
                w.scroll = 0;
                w.history = History::Done;
                Ok(())
            }
            "servers" => {
                self.cmd_servers();
                Ok(())
            }
            "sidebar" => {
                self.show_sidebar = !self.show_sidebar;
                Ok(())
            }
            "nicklist" => {
                self.show_nicklist = !self.show_nicklist;
                Ok(())
            }
            "reload" => {
                self.cmd_reload();
                Ok(())
            }
            "log" => {
                match args.trim() {
                    "on" => {
                        self.cfg.log = true;
                        self.apply_logging();
                    }
                    "off" => {
                        self.cfg.log = false;
                        self.apply_logging();
                    }
                    "" => {
                        let text = match &self.log {
                            Some(log) => format!("logging to {}", log.dir().display()),
                            None => "chat logging is off (/log on)".to_owned(),
                        };
                        self.info(text);
                    }
                    _ => return self.error("usage: /log [on|off]"),
                }
                Ok(())
            }
            "connect" => self.cmd_connect(args),
            "disconnect" => self.cmd_disconnect(args),
            "logout" => self.server_arg(args).map(|acct| acct.logout()),
            "recover" => self.server_arg(args).map(|acct| acct.recover()),
            "verify" => self.cmd_verify(args),
            "detach" => match crate::remote::detach("detached; textrix --attach comes back to it") {
                true => Ok(()),
                false => Err("/detach is for Textrix in the background: start it with textrix --daemon".into()),
            },
            "restart" => self.cmd_restart(),
            "quit" => {
                self.quit = true;
                Ok(())
            }
            "alias" => self.cmd_alias(args),
            "unalias" => self.cmd_unalias(args),
            other => match self.cfg.aliases.get(other).cloned() {
                Some(template) => self.run_alias(&template, args),
                None => {
                    let other = other.to_owned();
                    let here = self.win().room.clone();
                    let context = self.script_context(here.as_ref());
                    match self.scripts.run_command(&other, args, &context) {
                        Some(Ok(effects)) => {
                            self.apply_effects(effects, here);
                            Ok(())
                        }
                        Some(Err(e)) => Err(format!("script {e}")),
                        None => match self.plugin_command(&other, args) {
                            Some(done) => done,
                            None => self.egg_command(&other, args).unwrap_or_else(|| {
                                Err(if other.contains('/') {
                                    format!(
                                        "/{other} isn't a command, and not a file here either: drop files on an empty \
                                     input line, or /upload <path>"
                                    )
                                } else {
                                    format!("unknown command /{other}; try /help")
                                })
                            }),
                        },
                    }
                }
            },
        };
        if let Err(e) = result {
            self.error(e);
        }
    }

    /// `/play [nick | stop]`: the latest audio or video here (from nick).
    pub(super) fn cmd_play(&mut self, args: &str) -> Result<(), String> {
        let arg = args.trim();
        if arg == "stop" {
            let (_, playing) = self.playing.take().ok_or("nothing is playing")?;
            playing.abort();
            self.info("stopped");
            return Ok(());
        }
        let nick = Some(arg).filter(|n| !n.is_empty());
        let line = self
            .win()
            .lines
            .iter()
            .rev()
            .find(|l| {
                l.media.as_ref().is_some_and(|m| m.playable())
                    && !l.redacted
                    && nick.is_none_or(|n| l.nick.eq_ignore_ascii_case(n))
            })
            .cloned()
            .ok_or_else(|| match nick {
                Some(nick) => format!("no audio or video from {nick} here"),
                None => "no audio or video here".to_owned(),
            })?;
        self.play(&line)
    }

    /// Play a voice message, other audio, or a video. Audio already
    /// playing stops (and playing it again just stops it).
    pub(super) fn play(&mut self, line: &Line) -> Result<(), String> {
        let (media, event) = match (&line.media, &line.event_id) {
            (Some(media), Some(event)) if media.playable() && !line.redacted => (media.clone(), event.clone()),
            _ => return Err("that message isn't audio or video".into()),
        };
        let nick = self.display_name(line.sender.as_deref(), &line.nick).to_owned();
        if media.kind == MediaKind::Video {
            let (acct, _) = self.room_ctx()?;
            let player = audio::video_player(&self.cfg.video_player)
                .ok_or("no video player found; set video_player in config.toml (e.g. \"mpv\")")?;
            let size = media.size.map(|s| format!(" ({})", matrix::human_size(s))).unwrap_or_default();
            self.info(format!("fetching {nick}'s {}{size} for {}…", media.filename, player[0]));
            acct.play(event, media, player, self.paths.base.join("tmp"));
            return Ok(());
        }
        if let Some((playing, handle)) = self.playing.take() {
            handle.abort();
            if playing == event {
                self.info("stopped");
                return Ok(());
            }
        }
        let (acct, _) = self.room_ctx()?;
        let player = audio::player(&self.cfg.audio_player)
            .ok_or("no audio player found; set audio_player in config.toml (e.g. \"mpv --no-video\")")?;
        let what = if line.body.starts_with("[voice]") { "voice message".to_owned() } else { media.filename.clone() };
        self.playing = Some((event.clone(), acct.play(event, media, player, self.paths.base.join("tmp"))));
        self.info(format!("playing {nick}'s {what}; /play stop stops it"));
        Ok(())
    }

    pub(super) fn cmd_download(&mut self, args: &str) -> Result<(), String> {
        let (acct, room_id) = self.room_ctx()?;
        let nick = Some(args.trim()).filter(|n| !n.is_empty());
        let line = self.win().last_media(nick).ok_or_else(|| match nick {
            Some(nick) => format!("no attachment from {nick} here"),
            None => "no attachment here".to_owned(),
        })?;
        let media = line.media.clone().expect("filtered");
        acct.download(room_id, media);
        Ok(())
    }

    pub(super) fn cmd_lastlog(&mut self, args: &str) -> Result<(), String> {
        const USAGE: &str = "usage: /lastlog [-n N] [-nick nick] <text>";
        let (mut limit, mut nick, mut rest) = (None, None, args.trim());
        loop {
            if let Some(r) = rest.strip_prefix("-n ") {
                let (n, r) = split_first(r);
                limit = Some(n.parse::<usize>().map_err(|_| USAGE)?);
                rest = r;
            } else if let Some(r) = rest.strip_prefix("-nick ") {
                let (n, r) = split_first(r);
                nick = Some(n.to_lowercase());
                rest = r;
            } else {
                break;
            }
        }
        let needle = rest.trim().to_lowercase();
        if needle.is_empty() && nick.is_none() {
            return Err(USAGE.into());
        }
        let today = Local::now().date_naive();
        let w = self.win();
        let loaded = w.lines.iter().filter(|l| l.event_id.is_some()).count();
        let mut hits: Vec<String> = w
            .lines
            .iter()
            .filter(|l| l.event_id.is_some() && !l.redacted)
            .filter(|l| nick.as_ref().is_none_or(|n| l.nick.to_lowercase() == *n))
            .filter(|l| needle.is_empty() || l.body.to_lowercase().contains(&needle))
            .map(|l| {
                let when = if l.ts.date_naive() == today { l.ts.format("%H:%M") } else { l.ts.format("%b %d %H:%M") };
                let who = match l.kind {
                    LineKind::Message | LineKind::Utd => format!("<{}> ", l.nick),
                    LineKind::Emote => format!("* {} ", l.nick),
                    LineKind::Notice => format!("-{}- ", l.nick),
                    _ => String::new(),
                };
                format!("{when} {who}{}", l.body)
            })
            .collect();
        let found = hits.len();
        if let Some(n) = limit {
            hits = hits.split_off(found.saturating_sub(n));
        }
        if found == 0 {
            self.info(format!("Lastlog: no matches in {loaded} loaded lines (PgUp loads older history)"));
            return Ok(());
        }
        self.info(format!("Lastlog: {found} match(es) in {loaded} loaded lines"));
        for hit in hits {
            self.info(hit);
        }
        self.info("End of Lastlog");
        Ok(())
    }

    /// Links in this window, newest first, without repeats.
    pub(super) fn window_urls(&self) -> Vec<(String, String)> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for line in self.win().lines.iter().rev().filter(|l| l.event_id.is_some() && !l.redacted) {
            for url in links::urls(&line.body).into_iter().rev() {
                if seen.insert(url.to_owned()) {
                    out.push((url.to_owned(), line.nick.clone()));
                }
            }
        }
        out
    }

    pub(super) fn cmd_urls(&mut self) {
        let urls = self.window_urls();
        if urls.is_empty() {
            return self.info("no links in this window's loaded history");
        }
        let rows: Vec<String> =
            urls.iter().take(20).enumerate().map(|(i, (url, nick))| format!("{:>2}: {url}  <{nick}>", i + 1)).collect();
        for row in rows {
            self.info(row);
        }
        self.info("/open N opens one in your browser");
    }

    pub(super) fn cmd_open(&mut self, args: &str) -> Result<(), String> {
        let n: usize = match args.trim() {
            "" => 1,
            n => n.parse().map_err(|_| "usage: /open [N] (numbers from /urls)")?,
        };
        let urls = self.window_urls();
        let (url, _) = urls.get(n.saturating_sub(1)).ok_or_else(|| format!("no link {n} here; see /urls"))?;
        self.open_url(url.clone());
        Ok(())
    }

    pub(super) fn open_url(&mut self, url: String) {
        match links::open(&url) {
            Ok(()) => self.info(format!("opening {url}")),
            Err(e) => self.error(format!("couldn't open {url}: {e}")),
        }
    }

    pub(super) fn cmd_search(&mut self, args: &str) -> Result<(), String> {
        let (deep, rest) = match args.trim().strip_prefix("-deep") {
            Some(rest) => (true, rest.trim()),
            None => (false, args.trim()),
        };
        let query = crate::search::Query::parse(rest)?;
        if query.is_empty() {
            return Err(crate::search::USAGE.into());
        }
        let needle = query.describe();
        self.search_hits.clear();
        self.search_out = self.win().room.clone();

        if deep {
            let rooms: Vec<(Account, OwnedRoomId)> = self
                .windows
                .iter()
                .filter(|w| w.room.as_ref().is_some_and(|k| k.thread.is_none()))
                .filter(|w| query.room.as_deref().is_none_or(|r| w.title().to_lowercase().contains(r)))
                .filter_map(|w| {
                    let key = w.room.as_ref()?;
                    Some((self.connected(&key.account)?, key.room_id.clone()))
                })
                .collect();
            if rooms.is_empty() {
                return Err("no connected rooms to search".into());
            }
            self.search_pending = rooms.len();
            self.info(format!("searching the last 500 messages of {} room(s) for {needle}…", rooms.len()));
            for (acct, room_id) in rooms {
                acct.search_history(room_id, query.text.clone(), 5);
            }
            self.search_query = Some(query);
            return Ok(());
        }

        // Newest first across every window's loaded history.
        let this: &Self = self;
        let mut found: Vec<(WinKey, String, Line)> = this
            .windows
            .iter()
            .filter_map(|w| Some((w.room.clone()?, w)))
            .flat_map(|(key, w)| {
                let query = &query;
                w.lines
                    .iter()
                    .filter(|l| l.event_id.is_some() && l.is_message() && !l.redacted)
                    .filter(move |l| this.search_matches(query, w.title(), l))
                    .map(move |l| (key.clone(), w.title().to_owned(), l.clone()))
            })
            .collect();
        let (indexed, reading) = self.search_indexes(&query, &mut found);
        found.sort_by_key(|(_, _, line)| std::cmp::Reverse(line.ts));
        let more = found.len().saturating_sub(50);
        found.truncate(50);
        let searched = match (indexed, reading) {
            (0, _) => String::new(),
            (n, false) => format!(" (searched {n} indexed messages)"),
            (n, true) => format!(" (searched {n} indexed messages; older history is still being indexed)"),
        };
        if found.is_empty() {
            let hint = if indexed == 0 { "; try /search -deep" } else { "" };
            self.info(format!("no messages match {needle}{searched}{hint}"));
            return Ok(());
        }
        let rows: Vec<String> = found
            .into_iter()
            .enumerate()
            .map(|(i, (key, room, line))| {
                self.search_hits.push((key, line.event_id.clone().expect("filtered")));
                search_row(i + 1, &room, &line)
            })
            .collect();
        for row in rows {
            self.info(row);
        }
        let more = if more > 0 { format!("{more} more not shown; ") } else { String::new() };
        self.info(format!("{more}/jump N goes to a result{searched}"));
        Ok(())
    }

    /// Add each connected server's search index hits to `found` (the ones
    /// not already there). Returns how many messages the indexes hold, and
    /// whether any is still reading older history.
    /// Whether a loaded message matches a search, by its room's name.
    pub(super) fn search_matches(&self, query: &crate::search::Query, room: &str, line: &Line) -> bool {
        let sender = line.sender.as_ref().map(|s| s.as_str()).unwrap_or_default();
        let nickname = self.display_name(line.sender.as_deref(), &line.nick);
        query.matches(room, &[&line.nick, nickname], sender, line.ts.timestamp_millis(), &line.text())
    }

    pub(super) fn search_indexes(
        &self,
        query: &crate::search::Query,
        found: &mut Vec<(WinKey, String, Line)>,
    ) -> (usize, bool) {
        let (mut indexed, mut reading) = (0, false);
        let have: HashSet<OwnedEventId> = found.iter().filter_map(|(_, _, l)| l.event_id.clone()).collect();
        for (name, state) in &self.accounts {
            let Some(acct) = state.account.as_ref() else { continue };
            // Room names, looked up once each, and only for `in:`.
            let titles: std::cell::RefCell<HashMap<String, String>> = Default::default();
            let title = |room: &str| -> String {
                if let Some(title) = titles.borrow().get(room) {
                    return title.clone();
                }
                let title = OwnedRoomId::try_from(room)
                    .ok()
                    .and_then(|id| {
                        let open = self.window_idx(name, &id).map(|i| self.windows[i].title().to_owned());
                        open.or_else(|| acct.quick_meta(&id).map(|m| m.name))
                    })
                    .unwrap_or_else(|| room.to_owned());
                titles.borrow_mut().insert(room.to_owned(), title.clone());
                title
            };
            let matches = |e: &crate::index::Entry| {
                let room = if query.room.is_some() { title(&e.room) } else { String::new() };
                let nickname =
                    OwnedUserId::try_from(e.sender.as_str()).ok().and_then(|u| self.layout.nicknames.get(&u).cloned());
                let names = [e.nick.as_str(), nickname.as_deref().unwrap_or_default()];
                query.matches(&room, &names, &e.sender, e.ts, &e.body)
            };
            let Some((hits, len, more)) = acct.search_index(matches, 200) else {
                continue;
            };
            (indexed, reading) = (indexed + len, reading || more);
            for entry in hits {
                let (Ok(room_id), Ok(event)) = (OwnedRoomId::try_from(entry.room), OwnedEventId::try_from(entry.event))
                else {
                    continue;
                };
                if have.contains(&event) {
                    continue;
                }
                let room = self
                    .window_idx(name, &room_id)
                    .map(|i| self.windows[i].title().to_owned())
                    .or_else(|| state.account.as_ref()?.quick_meta(&room_id).map(|m| m.name))
                    .unwrap_or_else(|| room_id.to_string());
                let mut line = Line::new(LineKind::Message, entry.body);
                line.event_id = Some(event);
                line.nick = entry.nick;
                line.sender = OwnedUserId::try_from(entry.sender).ok();
                if let Some(ts) = chrono::TimeZone::timestamp_millis_opt(&Local, entry.ts).single() {
                    line.ts = ts;
                }
                found.push((WinKey { account: name.to_string(), room_id, thread: None }, room, line));
            }
        }
        (indexed, reading)
    }

    pub(super) fn cmd_jump(&mut self, args: &str) -> Result<(), String> {
        if args.trim() == "new" {
            return self.jump_to_unread();
        }
        let n: usize = args.trim().parse().map_err(|_| "usage: /jump <N> (numbers from /search) | new")?;
        let (key, event) = self.search_hits.get(n.wrapping_sub(1)).cloned().ok_or_else(|| format!("no result {n}"))?;
        self.jump_to(key, event)
    }

    /// Alt+R, `/jump new`: to the new-messages line, with the last message
    /// you'd read selected just above it (older history loads until it's
    /// there).
    pub(super) fn jump_to_unread(&mut self) -> Result<(), String> {
        let w = self.win();
        let key = w.room.clone().filter(|k| k.thread.is_none()).ok_or("that's for a room's own window")?;
        let marker = w.marker.clone().ok_or("no read marker here yet: you haven't read anything in this room")?;
        if w.latest_event().as_ref() == Some(&marker) {
            return Err("nothing unread here".into());
        }
        self.jump_to(key, marker)
    }

    /// Go to a message: its room's window (reopened if closed), selected, or
    /// older history loads until it's there.
    pub(super) fn jump_to(&mut self, key: WinKey, event: OwnedEventId) -> Result<(), String> {
        let idx = match self.window_idx(&key.account, &key.room_id) {
            Some(idx) => idx,
            None => self.reopen(&key.account, &key.room_id).ok_or("that room isn't available")?,
        };
        self.switch_to(idx);
        if self.win().contains(&event) {
            self.select = Some(event);
        } else {
            self.win_mut().seek = Some(event);
            self.info("loading older history to reach that message…");
        }
        Ok(())
    }

    pub(super) fn cmd_away(&mut self, args: &str) -> Result<(), String> {
        let (all, message) = match args.trim().strip_prefix("-all") {
            Some(rest) => (true, rest.trim()),
            None => (false, args.trim()),
        };
        let message = Some(message).filter(|m| !m.is_empty()).map(str::to_owned);
        if all {
            let accounts: Vec<Account> = self.accounts.values().filter_map(|s| s.account.clone()).collect();
            if accounts.is_empty() {
                return Err("no server is connected".into());
            }
            accounts.iter().for_each(|acct| acct.set_away(message.clone()));
        } else {
            self.pick_account("")?.0.set_away(message);
        }
        Ok(())
    }

    /// Open (or switch to) the window for the thread starting at `root` in
    /// the current room.
    /// In a thread window: close it and go back to its room's window.
    /// Whether it was one.
    pub(super) fn close_thread(&mut self) -> bool {
        let Some(key) = self.win().room.clone().filter(|k| k.thread.is_some()) else { return false };
        self.remove_window(self.active);
        if let Some(idx) = self.window_idx(&key.account, &key.room_id) {
            self.switch_to(idx);
        }
        true
    }

    pub(super) fn open_thread(&mut self, root: OwnedEventId) -> Result<(), String> {
        let (acct, room_id) = self.room_ctx()?;
        let account = acct.name.to_string();
        if let Some(idx) = self.thread_idx(&account, &room_id, &root) {
            self.switch_to(idx);
            return Ok(());
        }
        let key = WinKey { account: account.clone(), room_id: room_id.clone(), thread: Some(root.clone()) };
        let mut w = Window::new(Some(key), thread_name(""));
        if let Some(room) = self.window_idx(&account, &room_id).map(|i| &self.windows[i]) {
            (w.encrypted, w.is_dm, w.members, w.perms) = (room.encrypted, room.is_dm, room.members.clone(), room.perms);
            w.topic = Some(format!("Thread in {} · Esc closes it", room.name));
            if let Some(first) = room.lines.iter().find(|l| l.event_id.as_ref() == Some(&root)) {
                w.name = thread_name(&first.text());
            }
        }
        // History comes from the thread's relations, not room pagination.
        w.members_requested = true;
        w.history = History::Loading;
        let idx = self.insert_window(w);
        self.switch_to(idx);
        acct.load_thread(room_id, root);
        Ok(())
    }

    pub(super) fn cmd_thread(&mut self, args: &str) -> Result<(), String> {
        match args.trim() {
            "" => {
                if self.win().thread_root().is_some() {
                    return Err("you're in a thread; /threads lists the room's others".into());
                }
                let root = self
                    .win()
                    .lines
                    .iter()
                    .rev()
                    .find_map(|l| l.thread_root.clone().or_else(|| l.thread_count.and(l.event_id.clone())))
                    .ok_or("no threads in the loaded history; /threads asks the server")?;
                self.open_thread(root)
            }
            n => {
                let n: usize = n.parse().map_err(|_| "usage: /thread [N] (numbers from /threads)")?;
                let (key, root) =
                    self.thread_hits.get(n.wrapping_sub(1)).cloned().ok_or_else(|| format!("no thread {n}"))?;
                let idx = self.window_idx(&key.account, &key.room_id).ok_or("that room's window is closed")?;
                self.switch_to(idx);
                self.open_thread(root)
            }
        }
    }

    pub(super) fn cmd_joinrule(&mut self, args: &str) -> Result<(), String> {
        let (acct, room_id) = self.room_ctx()?;
        let (rule, rest) = split_first(args);
        let rule = match rule {
            "invite" => JoinRule::Invite,
            "public" => JoinRule::Public,
            "knock" => JoinRule::Knock,
            "restricted" => {
                let (_, space) = self.find_space(rest.trim())?;
                JoinRule::Restricted(Restricted::new(vec![AllowRule::room_membership(space.room_id)]))
            }
            _ => return Err("usage: /joinrule <invite|public|knock|restricted <space>>".into()),
        };
        acct.set_join_rule(room_id, rule);
        Ok(())
    }

    pub(super) fn cmd_create(&mut self, args: &str) -> Result<(), String> {
        let (acct, mut rest) = self.pick_account(args)?;
        let (mut public, mut space) = (false, false);
        loop {
            rest = rest.trim_start();
            if let Some(r) = rest.strip_prefix("-public") {
                (public, rest) = (true, r);
            } else if let Some(r) = rest.strip_prefix("-space") {
                (space, rest) = (true, r);
            } else {
                break;
            }
        }
        let name = rest.trim();
        if name.is_empty() {
            return Err("usage: /create [-public] [-space] <name>".into());
        }
        acct.create_room(name.to_owned(), public, space);
        Ok(())
    }

    /// The space a name refers to on the current window's server.
    pub(super) fn find_space(&self, name: &str) -> Result<(Account, SpaceInfo), String> {
        let (acct, _) = self.pick_account("")?;
        let wanted = name.to_lowercase();
        let spaces = self.spaces.get(&acct.name).map(Vec::as_slice).unwrap_or_default();
        let space = spaces
            .iter()
            .find(|s| s.name.to_lowercase() == wanted)
            .or_else(|| spaces.iter().find(|s| s.name.to_lowercase().contains(&wanted)))
            .cloned()
            .ok_or_else(|| format!("no space matching {name}; /spaces lists them"))?;
        Ok((acct, space))
    }

    pub(super) fn cmd_spaces(&mut self) -> Result<(), String> {
        let (acct, _) = self.pick_account("")?;
        let spaces = self.spaces.get(&acct.name).cloned().unwrap_or_default();
        if spaces.is_empty() {
            self.info("you're not in any spaces; /create -space <name> makes one");
            return Ok(());
        }
        for space in spaces {
            self.info(format!("◆ {} ({} room(s))", space.name, space.children.len()));
            for child in &space.children {
                let row = match self.window_idx(&acct.name, child) {
                    Some(idx) => format!("   {:>2}: {}", idx + 1, self.windows[idx].title()),
                    None => match acct.quick_meta(child) {
                        Some(meta) => format!("       {} (window closed)", meta.name),
                        None => format!("       {child} (not joined)"),
                    },
                };
                self.info(row);
            }
        }
        self.info("/space <name> browses one, including rooms you haven't joined");
        Ok(())
    }

    pub(super) fn cmd_space(&mut self, args: &str) -> Result<(), String> {
        let args = args.trim();
        let (name, action) = match args.rsplit_once(' ') {
            Some((name, action @ ("add" | "remove" | "leave"))) => (name.trim(), Some(action)),
            _ => (args, None),
        };
        if name.is_empty() {
            return Err("usage: /space <name> [add|remove|leave]".into());
        }
        let (acct, space) = self.find_space(name)?;
        match action {
            None => acct.space_hierarchy(self.here(), space.room_id),
            Some("leave") => acct.forget(space.room_id),
            Some(action) => {
                let (_, room_id) = self.room_ctx()?;
                acct.space_child(self.here(), space.room_id, room_id, action == "add");
            }
        }
        Ok(())
    }

    pub(super) fn cmd_topic(&mut self, args: &str) -> Result<(), String> {
        let (acct, room_id) = self.room_ctx()?;
        if args.trim().is_empty() {
            let text = match &self.win().topic {
                Some(topic) => format!("Topic for {}: {topic}", self.win().title()),
                None => format!("No topic set for {}", self.win().title()),
            };
            self.info(text);
        } else {
            acct.set_topic(room_id, args.trim().to_owned());
        }
        Ok(())
    }

    pub(super) fn cmd_moderate(&mut self, action: Moderation, args: &str) -> Result<(), String> {
        let (acct, room_id) = self.room_ctx()?;
        let (who, reason) = split_first(args);
        if who.is_empty() {
            return Err(format!("usage: /{} <nick|@user>", format!("{action:?}").to_lowercase()));
        }
        let user = self.resolve_user(who)?;
        let reason = Some(reason.trim()).filter(|r| !r.is_empty()).map(str::to_owned);
        acct.moderate(room_id, action, user, reason);
        Ok(())
    }

    /// Clear activity and unread counts, move the "new messages" divider to
    /// the end, and tell the server (read marker, plus a receipt as set by
    /// `read_receipts`).
    pub(super) fn cmd_markread(&mut self, args: &str) -> Result<(), String> {
        let all = match args.trim() {
            "" => false,
            "all" => true,
            _ => return Err("usage: /markread [all]".into()),
        };
        let targets: Vec<usize> = if all { (0..self.windows.len()).collect() } else { vec![self.active] };
        let mode = self.cfg.read_receipts;
        let mut rooms = 0;
        for idx in targets {
            self.clear_marked_unread(idx);
            let w = &mut self.windows[idx];
            w.activity = Activity::None;
            let latest = w.latest_event();
            if let Some(latest) = &latest {
                w.marker = Some(latest.clone());
            }
            // Thread windows share their room's read marker.
            let Some(key) = w.room.clone().filter(|k| k.thread.is_none()) else { continue };
            w.unread = (0, 0);
            w.last_receipt = latest.clone();
            if let Some(acct) = self.connected(&key.account) {
                acct.mark_room_read(key.room_id, latest, mode);
                rooms += 1;
            }
        }
        if all {
            self.info(format!("marked {rooms} room(s) read"));
        }
        Ok(())
    }
}
