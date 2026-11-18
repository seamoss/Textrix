//! Saying things: messages, emotes, replies, edits, reactions, uploads, voice, pasted images, /later, polls, /np, /exec, forwarding, and the hidden commands.

use super::*;

impl App {
    /// The hidden ones: mIRC's trout, emoticon classics, the 8-ball, and an
    /// excuse for today. `None` if `name` isn't one.
    pub(super) fn egg_command(&mut self, name: &str, args: &str) -> Option<Result<(), String>> {
        let args = args.trim();
        let with = |art: &str| if args.is_empty() { art.to_owned() } else { format!("{args} {art}") };
        Some(match name {
            "slap" if args.is_empty() => Err("slap whom?".into()),
            "slap" => {
                self.say_as_is(&eggs::slap(args), Say::Emote);
                Ok(())
            }
            "tableflip" => {
                self.say_as_is(&with("(╯°□°)╯︵ ┻━┻"), Say::Text);
                Ok(())
            }
            "unflip" => {
                self.say_as_is(&with("┬─┬ノ( º _ ºノ)"), Say::Text);
                Ok(())
            }
            "shrug" => {
                self.say_as_is(&with("¯\\_(ツ)_/¯"), Say::Text);
                Ok(())
            }
            "8ball" => {
                let asked = if args.is_empty() { String::new() } else { format!("“{args}” ") };
                self.info(format!("{asked}The 8-ball says: {}", eggs::pick(&eggs::EIGHT_BALL)));
                Ok(())
            }
            "bofh" => {
                self.info(format!("Today's excuse: {}.", eggs::pick(&eggs::EXCUSES)));
                Ok(())
            }
            _ => return None,
        })
    }

    /// Send text exactly as typed, without Markdown (for ¯\_(ツ)_/¯).
    pub(super) fn say_as_is(&mut self, text: &str, kind: Say) {
        let markdown = std::mem::replace(&mut self.cfg.markdown, false);
        self.say(text, kind);
        self.cfg.markdown = markdown;
    }

    /// Vote: the ticked answers, or the chosen one.
    pub(super) fn send_vote(&mut self) {
        let Some(v) = self.vote.take() else { return };
        let picked: Vec<usize> =
            if v.max > 1 && !v.picked.is_empty() { v.picked.iter().copied().collect() } else { vec![v.selected] };
        let answers: Vec<String> = picked.iter().filter_map(|&n| v.answers.get(n)).map(|(id, _)| id.clone()).collect();
        let texts: Vec<&str> = picked.iter().filter_map(|&n| v.answers.get(n)).map(|(_, t)| t.as_str()).collect();
        match self.connected(&v.key.account) {
            Some(acct) => {
                acct.vote(v.key.room_id.clone(), v.poll.clone(), answers, v.stable);
                self.select = None;
                self.info(format!("voted {} in “{}”", texts.join(", "), v.question));
            }
            None => self.error(format!("{} isn't connected", v.key.account)),
        }
    }

    /// `/voice`, Ctrl+R: record a voice message for this room.
    pub(super) fn start_voice(&mut self) -> Result<(), String> {
        if self.recording.is_some() {
            return Err("already recording".into());
        }
        let (acct, room_id) = self.room_ctx()?;
        if self.win().perms.is_some_and(|p| !p.can_send) {
            return Err("your power level doesn't allow posting here".into());
        }
        let key = WinKey { account: acct.name.to_string(), room_id, thread: None };
        let recording = crate::voice::start(&self.paths.base.join("tmp")).map_err(|e| format!("{e:#}"))?;
        self.recording = Some((key, recording));
        Ok(())
    }

    /// Stop recording: send it (`keep`), or throw it away.
    pub(super) fn stop_voice(&mut self, keep: bool) {
        let Some((key, recording)) = self.recording.take() else { return };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let voice = crate::voice::finish(recording, keep).await.map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::Voice { key, voice });
        });
    }

    /// Ctrl+V or `/paste`: the image on the clipboard, to send with Enter
    /// (terminals only paste text, so Textrix asks the system).
    pub(super) fn paste_image(&mut self) -> Result<(), String> {
        self.room_ctx()?;
        let dir = self.paths.base.join("tmp");
        let tx = self.tx.clone();
        let painter = self.picker.clone();
        let cells = Size::new(self.view_width.saturating_sub(8).clamp(8, 40), PASTED_ROWS);
        tokio::spawn(async move {
            let image = crate::clipboard::image(&dir).await.map_err(|e| format!("{e:#}"));
            // A picture of it, to see what's about to go.
            let picture = match (&image, painter) {
                (Ok(Some(file)), Some(painter)) => {
                    let file = file.clone();
                    tokio::task::spawn_blocking(move || {
                        image::open(&file).ok().and_then(|img| painter.fit(img, cells).ok())
                    })
                    .await
                    .ok()
                    .flatten()
                }
                _ => None,
            };
            let _ = tx.send(AppEvent::Pasted { image, picture });
        });
        Ok(())
    }

    /// Forget the pasted image, and its file.
    pub(super) fn drop_pasted(&mut self) {
        if let Some(pasted) = self.pasted.take() {
            let _ = std::fs::remove_file(pasted.file);
        }
    }

    /// Enter with an image pasted: it goes, with what's typed as its caption.
    pub(super) fn send_pasted(&mut self) {
        let (acct, room_id) = match self.room_ctx() {
            Ok(here) => here,
            Err(e) => return self.error(e),
        };
        let Some(pasted) = self.pasted.take() else { return };
        let typed = self.input.take();
        let typed = crate::emoji::expand(typed.trim()).into_owned();
        let name = pasted.file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut line = Line::new(
            LineKind::Message,
            attachment("image", Some(&name), if typed.is_empty() { &name } else { &typed }),
        );
        line.progress = Some(0);
        let txn_id = TransactionId::new();
        self.echo(&acct, line, &txn_id);
        let caption = (!typed.is_empty())
            .then(|| if self.cfg.markdown { matrix::Caption::markdown(typed) } else { matrix::Caption::plain(typed) });
        acct.upload_pasted(room_id, pasted.file, txn_id, caption);
    }

    /// `/exec [-o] <command>`, irssi's: run it with `sh -c`, and show what
    /// it prints in this window, or (`-o`) send that to the room. Not for
    /// scripts, which don't run programs.
    pub(super) fn cmd_exec(&mut self, args: &str) -> Result<(), String> {
        if self.in_script > 0 {
            return Err("scripts can't run programs, so not /exec".into());
        }
        let args = args.trim();
        let (send, command) = match args.strip_prefix("-o") {
            Some(rest) if rest.starts_with(char::is_whitespace) => (true, rest.trim()),
            _ => (false, args),
        };
        if command.is_empty() {
            return Err("usage: /exec [-o] <command>".into());
        }
        let into = self.win().room.clone();
        if send {
            self.room_ctx()?;
        }
        let (tx, command) = (self.tx.clone(), command.to_owned());
        tokio::spawn(async move {
            let output = run_exec(&command).await;
            let _ = tx.send(AppEvent::Exec { into, send, command, output });
        });
        Ok(())
    }

    /// `/np`: ask the player what's playing (off the UI thread: osascript
    /// takes a moment), then say it in this room; in the status window,
    /// just show it.
    pub(super) fn cmd_np(&mut self, args: &str) -> Result<(), String> {
        if !args.trim().is_empty() {
            return Err(format!("/np takes nothing after it (it had “{}”); np_format says how it reads", args.trim()));
        }
        if self.in_script > 0 {
            return Err("scripts can't run programs, so not /np".into());
        }
        let into = self.win().room.clone();
        let (tx, command) = (self.tx.clone(), self.cfg.np_command.clone());
        tokio::spawn(async move {
            let track = crate::np::now_playing(&command).await;
            let _ = tx.send(AppEvent::NowPlaying { into, track });
        });
        Ok(())
    }

    /// `/later <when> <text>` sends it then; `/later` lists what's
    /// waiting, `/later cancel <N|all>` takes it back.
    pub(super) fn cmd_later(&mut self, args: &str) -> Result<(), String> {
        let usage =
            "usage: /later <10m | 1h30m | 18:30 | tomorrow 9:00> <text>; /later lists them; /later cancel <N|all>";
        let args = args.trim();
        if args.is_empty() {
            if self.later.is_empty() {
                self.info("nothing scheduled; /later 10m <text> sends it in 10 minutes");
            }
            let now = Local::now();
            let rows: Vec<String> = self
                .later
                .iter()
                .enumerate()
                .map(|(n, (_, s))| {
                    let due = Local.timestamp_opt(s.due, 0).single().unwrap_or(now);
                    let when = if due <= now {
                        "as soon as it's connected".to_owned()
                    } else {
                        format!(
                            "at {} (in {})",
                            due.format("%a %H:%M"),
                            short_duration((due - now).to_std().unwrap_or_default())
                        )
                    };
                    format!("{:>2}. {} {when}: {}", n + 1, s.room, snippet(&s.text))
                })
                .collect();
            for row in rows {
                self.info(row);
            }
            return Ok(());
        }
        if let Some(which) = args.strip_prefix("cancel") {
            let which = which.trim();
            let gone: Vec<AccountName> = if which == "all" {
                self.later.drain(..).map(|(a, _)| a).collect()
            } else {
                let n: usize = which.parse().map_err(|_| usage.to_owned())?;
                if n == 0 || n > self.later.len() {
                    return Err(format!("there's no {n}; /later lists them"));
                }
                vec![self.later.remove(n - 1).0]
            };
            self.info(match gone.len() {
                0 => "nothing to cancel".to_owned(),
                1 => "cancelled".to_owned(),
                n => format!("cancelled {n}"),
            });
            for account in gone {
                self.keep_later(&account);
            }
            return Ok(());
        }
        let (due, text) = parse_later(args, Local::now()).ok_or(usage)?;
        let (acct, room_id) = self.room_ctx()?;
        let room = self.win().title().to_owned();
        let account = acct.name.clone();
        self.later.push((
            account.clone(),
            Scheduled { room_id, room: room.clone(), due: due.timestamp(), text: text.to_owned() },
        ));
        self.later.sort_by_key(|(_, s)| s.due);
        let kept = if acct.keeps_later() {
            ""
        } else {
            " (kept until Textrix closes: there's no keychain to keep it safe on disk)"
        };
        self.info(format!("{room} gets it {}{kept}; /later lists what's waiting", due.format("at %a %H:%M")));
        self.keep_later(&account);
        Ok(())
    }

    /// `/poll <question> | <answer> | <answer>…` starts one; `/poll end`
    /// ends your latest open one here.
    pub(super) fn cmd_poll(&mut self, args: &str) -> Result<(), String> {
        let usage = "usage: /poll <question> | <answer> | <answer> … (2 to 20 answers), or /poll end";
        if args.trim() == "end" {
            let (acct, room_id) = self.room_ctx()?;
            let latest =
                self.win().lines.iter().rev().find(|l| l.own && l.poll.as_ref().is_some_and(|p| p.ended.is_none()));
            let Some((poll, id)) = latest.and_then(|l| Some((l.poll.as_deref()?, l.event_id.clone()?))) else {
                return Err("you have no open poll here".into());
            };
            let counts = poll.counts();
            let top = counts.iter().max().copied().unwrap_or(0);
            let winners: Vec<&str> = poll
                .answers
                .iter()
                .zip(&counts)
                .filter(|(_, c)| **c == top && top > 0)
                .map(|((_, a), _)| a.as_str())
                .collect();
            let text = match winners.as_slice() {
                [] => "The poll has ended. Nobody voted.".to_owned(),
                [one] => format!("The poll has ended. Top answer: {one}"),
                many => format!("The poll has ended. Top answers: {}", many.join(", ")),
            };
            acct.end_poll(room_id, id, text, poll.stable);
            return Ok(());
        }
        let mut parts = args.split('|').map(str::trim).filter(|p| !p.is_empty());
        let question = parts.next().ok_or(usage)?.to_owned();
        let answers: Vec<String> = parts.map(str::to_owned).collect();
        if !(2..=20).contains(&answers.len()) {
            return Err(usage.into());
        }
        let (acct, room_id) = self.room_ctx()?;
        acct.start_poll(room_id, question, answers);
        Ok(())
    }

    /// Your own text messages in this window, oldest first.
    pub(super) fn editable(&self) -> Vec<&Line> {
        self.win()
            .lines
            .iter()
            .filter(|l| l.own && is_target(l) && l.media.is_none())
            .filter(|l| matches!(l.kind, LineKind::Message | LineKind::Emote | LineKind::Notice))
            .collect()
    }

    /// Matrix only accepts edits from a message's sender, and only if your
    /// power level lets you post in the room.
    pub(super) fn check_can_edit(&self) -> Result<(), String> {
        self.room_ctx()?;
        if self.win().perms.is_some_and(|p| !p.can_send) {
            return Err("your power level here doesn't allow sending messages, so you can't edit either".into());
        }
        Ok(())
    }

    /// Shift+Up, Ctrl+E on an empty line, `/edit`, or `e` in selection mode.
    pub(super) fn start_edit(&mut self, at: Option<OwnedEventId>) {
        if let Err(e) = self.check_can_edit() {
            return self.error(e);
        }
        let editable = self.editable();
        let target = match &at {
            Some(id) => editable.iter().find(|l| l.event_id.as_ref() == Some(id)).copied(),
            None => editable.last().copied(),
        };
        let Some(line) = target.cloned() else {
            return self.error(match at {
                Some(_) => "only your own text messages can be edited",
                None => "you have no message here to edit",
            });
        };
        let draft = match self.editing.take() {
            Some(session) => session.draft,
            None => self.input.text(),
        };
        self.load_edit(&line, draft);
    }

    pub(super) fn load_edit(&mut self, line: &Line, draft: String) {
        let (Some(key), Some(event)) = (self.win().room.clone(), line.event_id.clone()) else { return };
        let emote = line.kind == LineKind::Emote;
        self.editing = Some(EditSession { key, event, original: line.body.clone(), emote, draft });
        self.input.set_text(&line.body);
    }

    /// ↑ to an older message of yours, ↓ to a newer one; ↓ past the newest
    /// stops editing.
    pub(super) fn step_edit(&mut self, older: bool) {
        let Some(session) = self.editing.clone() else { return };
        let editable: Vec<Line> = self.editable().into_iter().cloned().collect();
        let Some(pos) = editable.iter().position(|l| l.event_id.as_ref() == Some(&session.event)) else {
            return self.cancel_edit();
        };
        match (older, pos) {
            (true, 0) => self.info("that's your oldest loaded message here; PgUp loads more history"),
            (true, p) => self.load_edit(&editable[p - 1], session.draft),
            (false, p) if p + 1 < editable.len() => self.load_edit(&editable[p + 1], session.draft),
            (false, _) => self.cancel_edit(),
        }
    }

    pub(super) fn cancel_edit(&mut self) {
        if let Some(session) = self.editing.take() {
            self.input.set_text(&session.draft);
        }
    }

    /// Enter: send the edit; unchanged text just closes the editor, and an
    /// emptied message offers to delete it instead.
    pub(super) fn finish_edit(&mut self) {
        let Some(session) = self.editing.take() else { return };
        self.mentions = std::mem::take(&mut self.input.mentions);
        let text = self.input.text();
        if text.trim().is_empty() {
            let event = session.event.clone();
            return self.request_delete(event, Some(session));
        }
        self.input.set_text(&session.draft);
        if text != session.original
            && let Err(e) = self.send_edit(session.event, session.emote, &text)
        {
            self.error(e);
        }
        self.mentions.clear();
    }

    pub fn is_editing(&self) -> bool {
        self.editing.is_some()
    }

    pub(super) fn say(&mut self, text: &str, kind: Say) {
        if text.trim().is_empty() {
            return;
        }
        let (acct, room_id) = match self.room_ctx() {
            Ok(ctx) => ctx,
            Err(e) => return self.error(e),
        };
        let content =
            self.with_custom_emoji(&acct.name, &room_id, compose(text, kind, &self.mentions, self.cfg.markdown));
        let content = self.as_thread_message(content);
        self.send_content(acct, room_id, content, text, kind.line_kind(), None);
        self.note_away(None);
    }

    /// Like irssi's "nick is away": after you write to someone (in their
    /// DM, by mentioning them, or replying to them) who's away or gone, say
    /// so, when it's news. Presence is a guess (a missed heartbeat reads as
    /// offline, and some people and servers don't share it), so only a
    /// status they set or a long absence counts; never for someone who's
    /// just been talking; and the same note at most once an hour.
    pub(super) fn note_away(&mut self, replied_to: Option<OwnedUserId>) {
        if !self.cfg.away_notes {
            return;
        }
        let Some(account) = self.win().account().map(ToOwned::to_owned) else { return };
        let own = self.connected(&account).map(|a| a.user_id);
        let mut people: Vec<OwnedUserId> = self.win().dm_user.iter().cloned().collect();
        people.extend(self.mentions.iter().map(|(_, user)| user.clone()));
        people.extend(replied_to);
        people.dedup();
        for user in people.into_iter().filter(|u| Some(u) != own.as_ref()) {
            let Some(presence) = self.presence_of(&account, &user) else { continue };
            let status = presence.status.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(ToOwned::to_owned);
            let gone = presence.last_active.map(|at| Local::now() - at).filter(|g| *g >= AWAY_NEWS);
            let text = match (presence.kind, gone) {
                (PresenceKind::Online, _) => continue,
                (PresenceKind::Away, Some(gone)) => format!("away, last active {}", ago(gone)),
                (PresenceKind::Away, None) if status.is_some() => "away".to_owned(),
                (PresenceKind::Offline, Some(gone)) => format!("offline, last seen {}", ago(gone)),
                // A missed heartbeat, or presence they don't share.
                _ => continue,
            };
            if self.spoke_lately(&account, &user) {
                continue;
            }
            if let Some((said, at)) = self.away_noted.get(&user) {
                let new_status = status.as_ref().is_some_and(|s| s != said);
                if at.elapsed() < AWAY_REPEAT && !new_status {
                    continue;
                }
            }
            let fallback = user.localpart().to_owned();
            let nick = self.win().members.iter().find(|m| m.user_id == user).map_or(fallback, |m| m.name.clone());
            let name = self.display_name(Some(&user), &nick).to_owned();
            let line = match &status {
                Some(status) => format!("{name} is {text}: {status}"),
                None => format!("{name} is {text}"),
            };
            self.away_noted.insert(user, (status.unwrap_or_default(), Instant::now()));
            self.info(line);
        }
    }

    /// Whether `user` said something in the last few minutes, in any of
    /// `account`'s windows: then they're around, whatever their presence
    /// says.
    pub(super) fn spoke_lately(&self, account: &str, user: &UserId) -> bool {
        let since = Local::now() - SPOKE_LATELY;
        self.windows.iter().filter(|w| w.account() == Some(account)).any(|w| {
            w.lines
                .iter()
                .rev()
                .take_while(|l| l.ts >= since)
                .any(|l| l.is_message() && l.sender.as_deref() == Some(user))
        })
    }

    /// In a thread window: (root, latest event, its sender), for sending
    /// into the thread.
    pub(super) fn thread_context(&self) -> Option<(OwnedEventId, OwnedEventId, OwnedUserId)> {
        let w = self.win();
        let root = w.thread_root()?.to_owned();
        let latest = w.lines.iter().rev().find(|l| l.event_id.is_some() && !l.pending && l.is_message());
        let fallback_sender = || self.connected(w.account()?).map(|a| a.user_id);
        let (event, sender) = match latest {
            Some(l) => (l.event_id.clone()?, l.sender.clone().or_else(fallback_sender)?),
            None => (root.clone(), fallback_sender()?),
        };
        Some((root, event, sender))
    }

    /// Wrap content as a thread message when sending from a thread window.
    pub(super) fn as_thread_message(&self, content: RoomMessageEventContent) -> RoomMessageEventContent {
        let Some((root, latest, sender)) = self.thread_context() else { return content };
        let thread = Thread::plain(root.clone(), latest.clone());
        let metadata = if latest == root {
            ReplyMetadata::new(&root, &sender, None)
        } else {
            ReplyMetadata::new(&latest, &sender, Some(&thread))
        };
        content.make_for_thread(metadata, ReplyWithinThread::No, AddMentions::No)
    }

    /// Show a local echo, then send.
    pub(super) fn send_content(
        &mut self,
        acct: Account,
        room_id: OwnedRoomId,
        content: RoomMessageEventContent,
        text: &str,
        kind: LineKind,
        reply_to: Option<OwnedEventId>,
    ) {
        let txn_id = TransactionId::new();
        let mut line = Line::new(kind, crate::emoji::expand(text).into_owned());
        line.reply_to = reply_to;
        if let Some(root) = self.win().thread_root() {
            line.thread = true;
            line.thread_root = Some(root.to_owned());
        }
        line.rich = crate::convert::rich_of(&content.msgtype);
        self.echo(&acct, line, &txn_id);
        acct.send(room_id, content, txn_id);
    }

    /// Show our own pending line. Talking also means you've caught up, so
    /// the "new messages" divider goes away.
    pub(super) fn echo(&mut self, acct: &Account, mut line: Line, txn_id: &TransactionId) {
        line.own = true;
        line.pending = true;
        line.txn_id = Some(txn_id.to_owned());
        line.nick = self.own_nick(acct);
        line.sender = Some(acct.user_id.clone());
        let w = self.win_mut();
        w.marker = w.latest_event();
        self.print(line);
    }

    pub(super) fn cmd_msg(&mut self, args: &str) -> Result<(), String> {
        let (acct, args) = self.pick_account(args)?;
        let (target, text) = split_first(args);
        if target.is_empty() || text.trim().is_empty() {
            return Err("usage: /msg <@user|#alias|!room> <text>".into());
        }
        let target = if target.starts_with(['@', '#', '!']) {
            target.to_owned()
        } else {
            self.resolve_user(target)?.to_string()
        };
        acct.send_to(target, compose(text, Say::Text, &self.mentions, self.cfg.markdown));
        Ok(())
    }

    pub(super) fn cmd_reply(&mut self, args: &str) -> Result<(), String> {
        let (target, text) = self.win().answer_target(args);
        if text.trim().is_empty() {
            return Err("usage: /reply [nick] <text>".into());
        }
        let target = target.ok_or("nothing to reply to")?;
        let (event_id, sender) =
            (target.event_id.clone().expect("filtered"), target.sender.clone().ok_or("unknown sender")?);
        let text = text.to_owned();
        self.send_reply(event_id, sender, &text)
    }

    pub(super) fn send_reply(&mut self, event_id: OwnedEventId, sender: OwnedUserId, text: &str) -> Result<(), String> {
        let (acct, room_id) = self.room_ctx()?;
        // In a thread window the reply stays in the thread.
        let thread = self.win().thread_root().map(|root| Thread::plain(root.to_owned(), event_id.clone()));
        let content = compose(text, Say::Text, &self.mentions, self.cfg.markdown);
        let content = self.with_custom_emoji(&acct.name, &room_id, content).make_reply_to(
            ReplyMetadata::new(&event_id, &sender, thread.as_ref()),
            ForwardThread::Yes,
            AddMentions::Yes,
        );
        self.send_content(acct, room_id, content, text, LineKind::Message, Some(event_id));
        self.note_away(Some(sender));
        Ok(())
    }

    pub(super) fn cmd_edit(&mut self, text: &str) -> Result<(), String> {
        if text.trim().is_empty() {
            return Err("usage: /edit <text>".into());
        }
        let target = self.win().last_message(true).ok_or("you have no message here to edit")?;
        let (event_id, emote) = (target.event_id.clone().expect("filtered"), target.kind == LineKind::Emote);
        self.send_edit(event_id, emote, text)
    }

    pub(super) fn send_edit(&mut self, event_id: OwnedEventId, emote: bool, text: &str) -> Result<(), String> {
        if text.trim().is_empty() {
            return Err("an edit can't be empty; use d in selection mode or /redact to delete".into());
        }
        let (acct, room_id) = self.room_ctx()?;
        let kind = if emote { Say::Emote } else { Say::Text };
        let content = compose(text, kind, &self.mentions, self.cfg.markdown);
        let content = self
            .with_custom_emoji(&acct.name, &room_id, content)
            .make_replacement(ReplacementMetadata::new(event_id.clone(), None));
        let rich = crate::convert::rich_of(&content.msgtype);
        if let Some(line) = self.win_mut().find_mut(&event_id) {
            line.body = crate::emoji::expand(text).into_owned();
            line.rich = rich;
            line.edited = true;
        }
        acct.send(room_id, content, TransactionId::new());
        Ok(())
    }

    pub(super) fn cmd_redact(&mut self) -> Result<(), String> {
        let (acct, room_id) = self.room_ctx()?;
        let target = self.win().last_message(true).ok_or("you have no message here to delete")?;
        acct.redact(room_id, target.event_id.clone().expect("filtered"));
        Ok(())
    }

    pub(super) fn cmd_react(&mut self, args: &str) -> Result<(), String> {
        let (target, key) = self.win().answer_target(args.trim());
        let key = key.trim();
        if key.is_empty() {
            return Err("usage: /react [nick] <emoji>".into());
        }
        let target = target.ok_or("nothing to react to")?;
        let event_id = target.event_id.clone().expect("filtered");
        let key = key.to_owned();
        self.send_react(event_id, &key)
    }

    pub(super) fn send_react(&mut self, event_id: OwnedEventId, key: &str) -> Result<(), String> {
        if key.is_empty() {
            return Err("type an emoji (or any text) to react with".into());
        }
        let (acct, room_id) = self.room_ctx()?;
        let code = key.trim_matches(':');
        let custom =
            self.emoji_for(&acct.name, Some(&room_id)).into_iter().find(|(c, _)| c == code && key.starts_with(':'));
        let label = match custom {
            Some((code, picture)) => {
                acct.react_custom(room_id, event_id, picture, code.clone());
                format!(":{code}:")
            }
            None => {
                let key = crate::emoji::expand(key).into_owned();
                acct.react(room_id, event_id, key.clone());
                key
            }
        };
        // The picker's Recent: emoji and custom ones, not words.
        if label.starts_with(':') || emojis::get(&label).is_some() {
            let recent = &mut self.layout.recent_emoji;
            recent.retain(|r| *r != label);
            recent.insert(0, label);
            recent.truncate(emoji_picker::RECENT);
            self.save_layout();
        }
        Ok(())
    }

    /// Custom emoji in a message for `room`, as their pictures.
    pub(super) fn with_custom_emoji(
        &self,
        account: &str,
        room: &RoomId,
        content: RoomMessageEventContent,
    ) -> RoomMessageEventContent {
        crate::compose::custom_emoji(content, &self.emoji_for(account, Some(room)))
    }

    pub(super) fn cmd_upload(&mut self, args: &str) -> Result<(), String> {
        if args.trim().is_empty() {
            return Err("usage: /upload <path>".into());
        }
        self.upload_file(parse_path(args))
    }

    pub(super) fn upload_file(&mut self, path: PathBuf) -> Result<(), String> {
        let (acct, room_id) = self.room_ctx()?;
        let meta = std::fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if !meta.is_file() {
            return Err(format!("{} is not a file", path.display()));
        }
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let kind = match mime_guess::from_path(&path).first_or_octet_stream().type_().as_str() {
            kind @ ("image" | "video" | "audio") => kind.to_owned(),
            _ => "file".to_owned(),
        };
        let txn_id = TransactionId::new();
        let mut line = Line::new(LineKind::Message, attachment(&kind, None, &name));
        line.progress = Some(0);
        self.echo(&acct, line, &txn_id);
        self.info(format!("uploading {name} ({})", matrix::human_size(meta.len())));
        acct.upload(room_id, path, txn_id);
        Ok(())
    }

    /// Open the switcher to choose where `line` goes.
    pub(super) fn start_forward(&mut self, line: &Line) -> Result<(), String> {
        let event = line.event_id.clone().ok_or("that message isn't sent yet")?;
        // From the mentions window, the message's own room.
        let key = self.win().room.clone().or_else(|| self.mention_origins.get(&event).cloned());
        let key = key.ok_or("open the message's room to forward it")?;
        let edited = line.edited.then(|| line.body.clone());
        let purpose = switcher::Purpose::Forward { account: key.account, room_id: key.room_id, event, edited };
        self.switcher = Some(Switcher { purpose, ..Switcher::default() });
        Ok(())
    }

    /// `/forward [room]`: the latest message here, to the named room or one
    /// picked in the switcher.
    pub(super) fn cmd_forward(&mut self, args: &str) -> Result<(), String> {
        let line = self.win().lines.iter().rev().find(|l| is_target(l)).cloned().ok_or("no message here to forward")?;
        self.start_forward(&line)?;
        if !args.trim().is_empty() {
            let sw = self.switcher.as_mut().expect("just opened");
            sw.query = args.trim().to_owned();
            if self.switcher_entries().is_empty() {
                self.switcher = None;
                return Err(format!("no room matching {} to forward to", args.trim()));
            }
            self.switcher_go(None);
        }
        Ok(())
    }
}

impl App {
    /// `/sticker [name]`: send the sticker called that, or choose one from
    /// their pictures.
    pub(super) fn cmd_sticker(&mut self, args: &str) -> Result<(), String> {
        let key = self.win().room.clone().ok_or("stickers go in a room or a DM")?;
        if !self.stickers.contains_key(key.account.as_str()) {
            return Err(format!("{}'s image packs aren't loaded yet: try again in a moment", key.account));
        }
        let stickers = self.stickers_for(&key.account, &key.room_id);
        if stickers.is_empty() {
            return Err("none of your image packs (or this room's) has stickers".into());
        }
        match args.trim() {
            "" => {
                self.sticker_picker = Some(crate::stickers::StickerPicker::new(key, stickers));
                Ok(())
            }
            name => {
                let sticker = crate::stickers::find(&stickers, name)?.clone();
                self.send_sticker(&sticker)
            }
        }
    }

    /// The stickers you can send in `room`: its own pack's, then yours and
    /// the global ones (by name, the room's first).
    pub(super) fn stickers_for(&self, account: &str, room: &RoomId) -> Vec<crate::stickers::Sticker> {
        let Some((global, rooms)) = self.stickers.get(account) else { return Vec::new() };
        let mut out = rooms.get(room).cloned().unwrap_or_default();
        for sticker in global {
            if !out.iter().any(|s| s.code == sticker.code) {
                out.push(sticker.clone());
            }
        }
        out
    }

    /// Send `sticker` here (into the thread, in a thread window), shown at
    /// once and marked sent when it is.
    pub(super) fn send_sticker(&mut self, sticker: &crate::stickers::Sticker) -> Result<(), String> {
        use matrix_sdk::ruma::events::{
            room::{ImageInfo, message::Relation},
            sticker::StickerEventContent,
        };
        let (acct, room_id) = self.room_ctx()?;
        let info: ImageInfo = serde_json::from_value(sticker.info.clone()).unwrap_or_default();
        let mut content = StickerEventContent::new(sticker.body.clone(), info, sticker.url.clone());
        if let Some((root, latest, _)) = self.thread_context() {
            content.relates_to = Some(Relation::Thread(Thread::plain(root, latest)));
        }
        let txn_id = TransactionId::new();
        let mut line = Line::new(LineKind::Message, "");
        crate::convert::sticker_line(&content, &mut line);
        if let Some(root) = self.win().thread_root() {
            line.thread = true;
            line.thread_root = Some(root.to_owned());
        }
        self.echo(&acct, line, &txn_id);
        acct.send(room_id, content, txn_id);
        Ok(())
    }
}
