use super::*;

fn msg(id: &str, body: &str) -> Line {
    let mut line = Line::new(LineKind::Message, body);
    line.event_id = Some(OwnedEventId::try_from(id).unwrap());
    line.nick = "alice".into();
    line
}

fn ids(w: &Window) -> Vec<&str> {
    w.lines.iter().map(|l| l.event_id.as_ref().unwrap().as_str()).collect()
}

#[test]
fn mouse_reports_that_came_apart_are_read_back() {
    let at = |kind, column, row, modifiers| Ok(Some(MouseEvent { kind, column, row, modifiers }));
    assert_eq!(split_mouse("[<65;247;37M"), at(MouseEventKind::ScrollDown, 246, 36, KeyModifiers::NONE));
    assert_eq!(split_mouse("[<64;1;1M"), at(MouseEventKind::ScrollUp, 0, 0, KeyModifiers::NONE));
    let shift_click = at(MouseEventKind::Down(MouseButton::Left), 9, 4, KeyModifiers::SHIFT);
    assert_eq!(split_mouse("[<4;10;5M"), shift_click);
    assert_eq!(split_mouse("[<2;3;4m"), at(MouseEventKind::Up(MouseButton::Right), 2, 3, KeyModifiers::NONE));
    assert_eq!(split_mouse("[<32;3;4M"), at(MouseEventKind::Drag(MouseButton::Left), 2, 3, KeyModifiers::NONE));
    for partial in ["[", "[<", "[<65", "[<65;", "[<65;247;37"] {
        assert_eq!(split_mouse(partial), Ok(None), "{partial}");
    }
    for not in ["x", "[a", "[<6x", "[<65;;", "[<;", "[<1;2;3;4M", "[<1;2M"] {
        assert_eq!(split_mouse(not), Err(()), "{not}");
    }
}

#[test]
fn an_escape_waits_a_moment_for_the_rest_of_a_mouse_report() {
    let mut app = test_app();
    let a = room_window(&mut app, "!a:x.org", "Night Owls");
    app.windows[a].lines.push_back(msg("$1", "first"));
    app.windows[a].lines.push_back(msg("$2", "second"));
    app.switch_to(a);
    let typed = |app: &mut App, text: &str| {
        for c in text.chars() {
            app.on_terminal(Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)));
        }
    };
    let esc = |app: &mut App| app.on_terminal(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));

    // Its Escape alone, then the rest: the wheel, not typing, not Escape.
    app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
    assert!(app.select.is_some());
    esc(&mut app);
    typed(&mut app, "[<65;10;5M");
    assert_eq!(app.input.text(), "");
    assert!(app.select.is_some(), "still selecting");

    // Escape, then typing: both, in order.
    esc(&mut app);
    typed(&mut app, "[x");
    assert!(app.select.is_none());
    assert_eq!(app.input.text(), "[x");

    // Escape on its own goes once the moment's passed.
    app.input.clear();
    app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
    esc(&mut app);
    assert!(app.select.is_some(), "held");
    app.esc_timeout();
    assert!(app.select.is_none());
}

#[test]
fn local_echo_is_replaced_by_server_copy() {
    let mut w = Window::status();
    let txn = TransactionId::new();
    let mut echo = Line::new(LineKind::Message, "hi");
    (echo.own, echo.pending, echo.txn_id) = (true, true, Some(txn.clone()));
    assert!(w.push(echo, 100));

    let mut server = msg("$a", "hi");
    server.txn_id = Some(txn);
    assert!(!w.push(server, 100), "echo is merged, not appended");
    assert_eq!(w.lines.len(), 1);
    assert!(!w.lines[0].pending);
}

#[test]
fn server_copy_upgrades_an_acknowledged_echo() {
    let mut w = Window::status();
    let txn = TransactionId::new();
    let mut echo = Line::new(LineKind::Message, "[file] notes.txt");
    (echo.own, echo.pending, echo.txn_id) = (true, true, Some(txn.clone()));
    w.push(echo, 100);
    // The send acknowledgement lands first...
    let line = w.lines.back_mut().unwrap();
    (line.pending, line.event_id) = (false, Some("$up".try_into().unwrap()));
    // ...then the synced copy, which knows about the attachment.
    let mut server = msg("$up", "[file] notes.txt");
    (server.own, server.txn_id) = (true, Some(txn));
    server.media = Some(crate::model::Media {
        source: matrix_sdk::ruma::events::room::MediaSource::Plain("mxc://x/y".into()),
        filename: "notes.txt".into(),
        kind: crate::model::MediaKind::File,
        mimetype: None,
        thumbnail: None,
        size: None,
        animated: None,
    });
    assert!(!w.push(server, 100));
    assert_eq!(w.lines.len(), 1);
    assert!(w.last_media(None).is_some());
}

#[test]
fn history_dedupes_and_applies_edits_and_orphans() {
    let mut w = Window::status();
    w.push(msg("$c", "newest"), 100);
    // A reaction arrives live for a message that is not loaded yet.
    w.apply(
        Item::React { target: "$a".try_into().unwrap(), key: "👍".into(), nick: "bob".into(), own: false },
        100,
        true,
    );
    let edit = Item::Edit { target: "$a".try_into().unwrap(), body: "fixed".into(), fallback: msg("$e", "fixed") };
    w.merge_history(vec![Item::Line(msg("$a", "typo")), edit, Item::Line(msg("$c", "newest"))]);

    let bodies: Vec<&str> = w.lines.iter().map(|l| l.body.as_str()).collect();
    assert_eq!(bodies, vec!["fixed", "newest"]);
    assert!(w.lines[0].edited);
    assert_eq!(w.lines[0].reactions, vec![("👍".to_owned(), vec!["bob".to_owned()])]);
}

#[test]
fn first_history_page_can_be_newer_than_sync() {
    // Sync delivered the first two events; the latest-events page also
    // carries the ones after them.
    let mut w = Window::status();
    w.push(msg("$1", "created"), 100);
    w.push(msg("$2", "joined"), 100);
    let page = ["$0", "$1", "$2", "$3", "$4"].map(|id| Item::Line(msg(id, id))).to_vec();
    w.merge_history(page);
    assert_eq!(ids(&w), vec!["$0", "$1", "$2", "$3", "$4"]);
}

#[test]
fn older_page_goes_on_top() {
    let mut w = Window::status();
    w.push(msg("$5", "five"), 100);
    w.merge_history(["$3", "$4"].map(|id| Item::Line(msg(id, id))).to_vec());
    assert_eq!(ids(&w), vec!["$3", "$4", "$5"]);
}

#[test]
fn replies_carry_parent_context() {
    let mut w = Window::status();
    w.push(msg("$p", "the original question, which is fairly long indeed"), 100);
    let mut reply = msg("$r", "answer");
    reply.reply_to = Some("$p".try_into().unwrap());
    w.push(reply, 100);
    let r = &w.lines[1];
    assert_eq!(r.reply_nick.as_deref(), Some("alice"));
    assert_eq!(r.reply_snippet.as_deref(), Some("the original question, which is fairly l…"));
}

#[test]
fn answer_target_picks_named_nick() {
    let mut w = Window::status();
    let mut bob = msg("$b", "from bob");
    bob.nick = "Bob Smith".into();
    w.push(bob, 100);
    w.push(msg("$a", "from alice"), 100);

    let (target, rest) = w.answer_target("bob smith: sure thing");
    assert_eq!(target.unwrap().body, "from bob");
    assert_eq!(rest, "sure thing");

    let (target, rest) = w.answer_target("sure thing");
    assert_eq!(target.unwrap().body, "from alice", "defaults to the last message");
    assert_eq!(rest, "sure thing");
}

#[test]
fn dropped_files_are_offered_for_upload() {
    let dir = std::env::temp_dir().join(format!("textrix-drop-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (a, b) = (dir.join("My Notes.pdf"), dir.join("song.flac"));
    std::fs::write(&a, b"%PDF").unwrap();
    std::fs::write(&b, vec![0u8; 2048]).unwrap();
    let escaped = |p: &std::path::Path| p.display().to_string().replace(' ', "\\ ");
    let both = format!("{} '{}'", escaped(&a), b.display());
    assert_eq!(dropped_files(&both), Some(vec![a.clone(), b.clone()]), "escaped and quoted, any kind of file");
    let urls = format!("file://{}\nfile://{}", a.display().to_string().replace(' ', "%20"), b.display());
    assert_eq!(dropped_files(&urls), Some(vec![a.clone(), b.clone()]), "file:// URLs, one per line");
    assert_eq!(dropped_files(&format!("see {}", escaped(&a))), None, "text with a path in it is text");
    assert_eq!(dropped_files(&dir.display().to_string()), None, "folders aren't uploaded");
    assert_eq!(dropped_files("Cargo.toml"), None, "only absolute paths, as terminals drop them");
    let shot = dir.join("Screenshot 2026-10-18 at 9.31.02\u{202f}PM.png");
    std::fs::write(&shot, b"png").unwrap();
    assert_eq!(dropped_files(&escaped(&shot)), Some(vec![shot.clone()]), "a narrow space isn't a separator");
    assert_eq!(
        dropped_files(&shot.display().to_string()),
        Some(vec![shot.clone()]),
        "nor are unescaped spaces in one path"
    );
    app_hint_for_paths(&dir);

    let mut app = test_app();
    let room = room_window(&mut app, "!a:x.org", "Night Owls");
    app.switch_to(room);
    app.on_terminal(Event::Paste(both.clone()));
    assert_eq!(app.confirm_upload.as_deref(), Some(&[a.clone(), b.clone()][..]));
    assert_eq!(app.upload_summary().as_deref(), Some("2 files (2.0 KB)"));
    app.on_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    assert!(app.confirm_upload.is_none());
    assert_eq!(app.input.text(), both, "declined: the paths stay, as text");
    app.submit();
    assert!(app.confirm_upload.is_none(), "and Enter doesn't ask again");
    let last = &app.windows[room].lines.back().unwrap().body;
    assert!(!last.contains("command"), "it's sent as a message, not run as a command: {last}");

    app.input.take();
    app.input.set_text(&escaped(&a));
    app.submit();
    assert!(app.confirm_upload.is_some(), "a typed path asks on Enter instead of running /Users…");
    app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.input.is_empty() && app.confirm_upload.is_none());
    let last = &app.windows[room].lines.back().unwrap().body;
    assert!(last.contains("not connected"), "it tried to upload: {last}");

    app.switch_to(0);
    app.on_terminal(Event::Paste(escaped(&a)));
    assert!(app.confirm_upload.is_none(), "not outside a room");
    std::fs::remove_dir_all(dir).unwrap();
}

/// A path that isn't a file, typed as if a command, says so.
fn app_hint_for_paths(dir: &std::path::Path) {
    let mut app = test_app();
    app.command(format!("{}/nothing.pdf", dir.display()).trim_start_matches('/'));
    let last = &app.windows[0].lines.back().unwrap().body;
    assert!(last.contains("isn't a command, and not a file here"), "{last}");
}

#[test]
fn dropped_paths_are_unescaped() {
    assert_eq!(parse_path(r"/tmp/My\ Cat.png"), PathBuf::from("/tmp/My Cat.png"));
    assert_eq!(parse_path("'/tmp/a b.txt'"), PathBuf::from("/tmp/a b.txt"));
    assert_eq!(parse_path("file:///tmp/a%20b.txt"), PathBuf::from("/tmp/a b.txt"));
    let home = dirs::home_dir().unwrap();
    assert_eq!(parse_path("~/x.png"), home.join("x.png"));
}

fn test_app() -> App {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let paths = Paths { base: std::env::temp_dir(), env_file: std::env::temp_dir().join("none") };
    App::new(Config::default(), paths.clone(), EnvFile::new(paths.env_file), Servers::default(), tx)
}

fn room_window(app: &mut App, room: &str, name: &str) -> usize {
    let key = WinKey { account: "t".into(), room_id: room.try_into().unwrap(), thread: None };
    app.windows.push(Window::new(Some(key), name.into()));
    app.windows.len() - 1
}

#[test]
fn bridged_rooms_say_where_their_messages_go() {
    let mut app = test_app();
    let a = room_window(&mut app, "!a:x.org", "Night Owls");
    let b = room_window(&mut app, "!b:x.org", "Book Club");
    app.windows[a].topic = Some("late-night chat".into());
    app.windows[a].bridges = vec![crate::model::Bridge {
        protocol: "Discord".into(),
        network: Some("Owls Guild".into()),
        channel: Some("#general".into()),
        bot: None,
    }];
    app.switch_to(a);
    let rows = |app: &mut App| {
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 12)).unwrap();
        terminal.draw(|f| ui::draw(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..12).map(|y| (0..90).map(|x| buf[(x, y)].symbol()).collect::<String>()).collect::<Vec<_>>()
    };
    let shown = rows(&mut app);
    assert!(shown[0].starts_with(" ⇄ Discord · Owls Guild · #general │ late-night chat"), "{}", shown[0]);
    let row = |name: &str| shown.iter().find(|r| r.contains(name)).cloned().unwrap();
    assert!(row("Night Owls").contains("Night Owls ⇄"), "{}", row("Night Owls"));
    assert!(!row("Book Club").contains('⇄'), "not bridged");
    // The window list's edge stays put whether or not a room's bridged.
    let edge = |r: &str| r.chars().position(|c| c == '│');
    assert!(edge(&row("Night Owls")).is_some());
    assert_eq!(edge(&row("Night Owls")), edge(&row("Book Club")), "{shown:#?}");
    app.switch_to(b);
    assert!(!rows(&mut app)[0].contains('⇄'), "the topic bar's own room only");
}

#[test]
fn the_theme_editor_changes_colours_as_you_watch_and_saves_a_theme() {
    use crate::theme_editor::ROLES;
    let base = std::env::temp_dir().join(format!("textrix-theme-edit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let paths = Paths { base: base.clone(), env_file: base.join("none") };
    let mut app = App::new(Config::default(), paths.clone(), EnvFile::new(paths.env_file), Servers::default(), tx);
    let key = |app: &mut App, code: KeyCode| app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    let typed = |app: &mut App, text: &str| {
        for c in text.chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
    };
    let full = |app: &App, role: &str| app.theme_editor.as_ref().unwrap().full.color_of(role).flatten();
    let before = app.cfg.palette.clone();

    app.command("theme edit");
    let own_text = ROLES.iter().position(|(_, key, _)| *key == "own_text").unwrap();
    app.theme_editor.as_mut().unwrap().selected = own_text;
    let was = full(&app, "own_text");
    key(&mut app, KeyCode::Right);
    assert_ne!(full(&app, "own_text"), was, "round the wheel");
    assert!(app.theme_editor.as_ref().unwrap().colors.contains_key("own_text"), "as a [colors] key");
    key(&mut app, KeyCode::Char('r'));
    assert_eq!(full(&app, "own_text"), was, "the theme's own again");

    // Typed: a colour, then one that isn't.
    key(&mut app, KeyCode::Enter);
    for _ in 0..20 {
        key(&mut app, KeyCode::Backspace);
    }
    typed(&mut app, "#ff0000");
    key(&mut app, KeyCode::Enter);
    assert_eq!(full(&app, "own_text"), Some(ratatui::style::Color::Rgb(255, 0, 0)));
    key(&mut app, KeyCode::Enter);
    for _ in 0..20 {
        key(&mut app, KeyCode::Backspace);
    }
    typed(&mut app, "nope");
    key(&mut app, KeyCode::Enter);
    let e = app.theme_editor.as_ref().unwrap();
    assert!(e.error.as_deref().is_some_and(|e| e.contains("isn't a colour")), "{:?}", e.error);
    assert_eq!(full(&app, "own_text"), Some(ratatui::style::Color::Rgb(255, 0, 0)), "unchanged");

    // Drawn: the panel, with the change marked.
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 60)).unwrap();
    terminal.draw(|f| ui::draw(f, &mut app)).unwrap();
    let buf = terminal.backend().buffer().clone();
    let rows: Vec<String> = (0..60).map(|y| (0..100).map(|x| buf[(x, y)].symbol()).collect()).collect();
    assert!(rows.iter().any(|r| r.contains("Editing default")), "{rows:#?}");
    assert!(rows.iter().any(|r| r.contains("•own_text") && r.contains("#ff0000")), "{rows:#?}");

    // Esc: all as it was.
    key(&mut app, KeyCode::Esc);
    assert!(app.theme_editor.is_none());
    assert_eq!(app.cfg.palette.own_text, before.own_text);
    assert!(app.cfg.colors.is_empty(), "config.toml's [colors] never changed");

    // Again, and saved as a theme: in use from then on.
    app.command("theme edit");
    let link = ROLES.iter().position(|(_, key, _)| *key == "link").unwrap();
    app.theme_editor.as_mut().unwrap().selected = link;
    key(&mut app, KeyCode::Char('+'));
    let lighter = full(&app, "link");
    key(&mut app, KeyCode::Char('s'));
    assert_eq!(app.theme_editor.as_ref().unwrap().naming.as_deref(), Some("default-mine"));
    for _ in 0..20 {
        key(&mut app, KeyCode::Backspace);
    }
    typed(&mut app, "list");
    key(&mut app, KeyCode::Enter);
    let e = app.theme_editor.as_ref().expect("still open");
    assert!(e.error.as_deref().is_some_and(|e| e.contains("means something else")), "{:?}", e.error);
    for _ in 0..4 {
        key(&mut app, KeyCode::Backspace);
    }
    typed(&mut app, "midnight-mine");
    key(&mut app, KeyCode::Enter);
    assert!(app.theme_editor.is_none(), "saved and closed");
    assert_eq!(app.cfg.theme, ThemeId::File("midnight-mine".into()));
    let saved = std::fs::read_to_string(base.join("themes/midnight-mine.toml")).unwrap();
    assert!(saved.contains("link"), "{saved}");
    let reloaded = Theme::resolve(&app.cfg.theme, &BTreeMap::new(), &base.join("themes")).unwrap();
    assert_eq!(reloaded.color_of("link").flatten(), lighter, "the file has the change");
    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn auto_away_comes_when_youre_idle_and_goes_when_youre_back() {
    let mut app = test_app();
    let ago = |minutes: u64| Instant::now().checked_sub(Duration::from_secs(minutes * 60)).unwrap();
    assert_eq!(app.cfg.auto_away, 15, "on unless you say");
    app.last_input = ago(14);
    assert!(!app.idle_for_away(), "not yet");
    app.last_input = ago(16);
    assert!(app.idle_for_away());
    // Already away (your own /away): left as it is.
    let def = ServerDef { name: "t".into(), homeserver: None, user: "@me:example.org".into() };
    let state = AccountState {
        user: def.user.clone(),
        def,
        nick: None,
        status: ConnStatus::Online,
        verified: None,
        away: None,
        account: None,
    };
    app.accounts.insert("t".into(), state);
    app.accounts.get_mut("t").unwrap().away = Some("lunch".into());
    assert!(!app.idle_for_away());
    app.accounts.get_mut("t").unwrap().away = None;
    // Set by auto-away already; or off.
    app.auto_away = true;
    assert!(!app.idle_for_away());
    app.auto_away = false;
    app.cfg.auto_away = 0;
    assert!(!app.idle_for_away(), "0: never");
    app.cfg.auto_away = 15;
    // A key brings you back: counting starts again.
    app.auto_away = true;
    app.on_terminal(Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)));
    assert!(!app.auto_away && !app.idle_for_away());
    // A mouse moving over it isn't you being back.
    app.last_input = ago(16);
    app.on_terminal(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: 1,
        row: 1,
        modifiers: KeyModifiers::NONE,
    }));
    assert!(app.idle_for_away());
    app.on_terminal(Event::FocusGained);
    assert!(!app.idle_for_away(), "coming back to the terminal is");

    // A bot never: nobody's at its keyboard.
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let base = std::env::temp_dir().join("bots").join("helper");
    let paths = Paths { base: base.clone(), env_file: base.join(".env") };
    let mut bot = App::new(Config::default(), paths.clone(), EnvFile::new(paths.env_file), Servers::default(), tx);
    bot.last_input = ago(60);
    assert!(bot.is_bot() && !bot.idle_for_away());
}

#[test]
fn the_room_browser_lists_searches_and_goes_to_rooms() {
    use crate::directory::{DirRoom, Page, Source};
    assert!(looks_like_server("matrix.org") && looks_like_server("example.org:8448"));
    assert!(!looks_like_server("rust") && !looks_like_server("rust lang") && !looks_like_server("#room:x.org"));

    let mut app = test_app();
    let owls = room_window(&mut app, "!owls:x.org", "Night Owls");
    app.switch_to(0);
    app.spaces.insert(
        "t".into(),
        vec![SpaceInfo { room_id: "!hobbies:x.org".try_into().unwrap(), name: "Hobbies".into(), children: Vec::new() }],
    );
    // /list on another server is that server's directory; Tab, your spaces.
    assert!(app.open_directory("matrix.org").is_err(), "nobody is connected in a test");
    app.directory = Some(crate::directory::Directory::new(
        "t".into(),
        vec![Source::Server(None), Source::Space { id: "!hobbies:x.org".try_into().unwrap(), name: "Hobbies".into() }],
        String::new(),
    ));
    let room = |id: &str, name: &str, joined: bool| DirRoom {
        room_id: id.try_into().unwrap(),
        alias: Some(format!("#{}:x.org", name.to_lowercase().replace(' ', "-"))),
        name: name.into(),
        topic: Some(format!("{name}, the room for it")),
        members: 42,
        joined,
        space: false,
        how: "public".into(),
        readable: true,
    };
    app.on_event(AppEvent::Directory {
        account: "t".into(),
        source: 0,
        query: String::new(),
        rooms: Ok(Page {
            rooms: vec![room("!owls:x.org", "Night Owls", true), room("!books:x.org", "Book Club", false)],
            next: None,
            total: Some(2),
        }),
    });
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
    terminal.draw(|f| ui::draw(f, &mut app)).unwrap();
    let buf = terminal.backend().buffer().clone();
    let rows: Vec<String> = (0..30).map(|y| (0..100).map(|x| buf[(x, y)].symbol()).collect()).collect();
    assert!(rows.iter().any(|r| r.contains("Rooms · your server")), "{rows:#?}");
    assert!(rows.iter().any(|r| r.contains("✓ Night Owls") && r.contains("#night-owls:x.org")), "{rows:#?}");
    assert!(rows.iter().any(|r| r.contains("Tab Hobbies")), "where Tab goes: {rows:#?}");
    assert!(rows.iter().any(|r| r.contains("you're in it: Enter goes there")), "{rows:#?}");
    // Typing: a server search waits for a pause.
    app.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
    assert!(app.directory_due().is_some_and(|at| at > Instant::now()));
    // Enter on a room you're in: there, and the browser closes.
    app.directory.as_mut().unwrap().selected = 0;
    app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.directory.is_none());
    assert_eq!(app.active, owls);
}

#[test]
fn rooms_marked_unread_wait_in_unreads_until_opened() {
    let mut app = test_app();
    let owls = room_window(&mut app, "!owls:x.org", "Night Owls");
    let books = room_window(&mut app, "!books:x.org", "Book Club");
    app.switch_to(owls);
    // Marked in another client: it comes with the room's details.
    let meta = |name: &str, id: &str, marked: bool| RoomMeta {
        room_id: id.try_into().unwrap(),
        name: name.into(),
        topic: None,
        encrypted: false,
        is_dm: false,
        dm_user: None,
        is_space: false,
        avatar: None,
        fully_read: None,
        unread: 0,
        highlights: 0,
        pinned: Vec::new(),
        favourite: false,
        bridges: Vec::new(),
        marked_unread: marked,
    };
    app.on_event(AppEvent::Rooms { account: "t".into(), rooms: vec![meta("Book Club", "!books:x.org", true)] });
    assert!(app.windows[books].marked_unread);
    let draw = |app: &mut App| {
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 14)).unwrap();
        terminal.draw(|f| ui::draw(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..14).map(|y| (0..90).map(|x| buf[(x, y)].symbol()).collect::<String>()).collect::<Vec<_>>()
    };
    let rows = draw(&mut app);
    let unreads = rows.iter().position(|r| r.contains("Unreads")).expect("an Unreads section");
    assert!(rows[unreads + 1].contains("Book Club") && rows[unreads + 1].contains('•'), "{rows:#?}");
    // Opening it: not any more (and nothing new to show for it).
    app.switch_to(books);
    assert!(!app.windows[books].marked_unread);
    assert!(!draw(&mut app).iter().any(|r| r.contains("Unreads")), "back where it was");
    // /unread needs the server, which says so.
    app.switch_to(owls);
    assert!(app.cmd_unread("").unwrap_err().contains("isn't connected"));
    // /markread clears it too.
    app.windows[owls].marked_unread = true;
    app.cmd_markread("").unwrap();
    assert!(!app.windows[owls].marked_unread);
}

#[test]
fn the_status_bars_show_what_you_list() {
    let mut app = test_app();
    let a = room_window(&mut app, "!a:x.org", "Night Owls");
    app.switch_to(a);
    app.cfg.statusbar = vec!["window".into(), "theme".into()];
    app.cfg.statusbar2 = vec!["date".into()];
    let rows = |app: &mut App| {
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 12)).unwrap();
        terminal.draw(|f| ui::draw(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..12).map(|y| (0..90).map(|x| buf[(x, y)].symbol()).collect::<String>()).collect::<Vec<_>>()
    };
    let shown = rows(&mut app);
    assert!(shown[10].starts_with("[2:Night Owls] [default]"), "{}", shown[10]);
    assert!(shown[9].starts_with(&format!("[{}]", Local::now().format("%a %d %b"))), "a second bar: {}", shown[9]);
    app.confirm_paste = Some(7);
    assert!(rows(&mut app)[10].contains("[send 7 pasted lines to Night Owls? y/n]"), "prompts always show");

    let mut table = app.cfg.to_table().unwrap();
    table.insert("statusbar".into(), toml::Value::Array(vec![toml::Value::String("tme".into())]));
    let refused = Config::from_table(table, std::path::Path::new("")).map(|_| ()).unwrap_err().to_string();
    assert!(refused.contains("“tme”") && refused.contains("time"), "{refused}");
}

#[test]
fn rooms_and_spaces_can_keep_their_own_theme() {
    use crate::theme::ThemeName;
    let dir = std::env::temp_dir().join(format!("textrix-room-themes-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut app = test_app();
    app.paths.base = dir.clone();
    let owls = room_window(&mut app, "!a:x.org", "Night Owls");
    let tests = room_window(&mut app, "!b:x.org", "Team Tests");
    let showing = |app: &mut App, idx: usize| {
        app.switch_to(idx);
        app.follow_room_theme();
        app.palette_for.clone()
    };
    let yours = ThemeId::default();

    app.switch_to(owls);
    app.command("theme here amber-deep");
    assert_eq!(showing(&mut app, owls), ThemeName::AmberDeep.into());
    assert_eq!(app.cfg.theme, yours, "yours stays as it was");
    assert_eq!(showing(&mut app, tests), yours, "other rooms show yours");

    app.spaces.insert(
        "t".into(),
        vec![SpaceInfo {
            room_id: "!work:x.org".try_into().unwrap(),
            name: "Work".into(),
            children: vec!["!b:x.org".try_into().unwrap()],
        }],
    );
    app.command("theme space Work turbo");
    assert_eq!(showing(&mut app, tests), ThemeName::Turbo.into(), "a space's rooms take its theme");
    assert_eq!(showing(&mut app, owls), ThemeName::AmberDeep.into());

    // Changing yours in a room with its own keeps the room's, and says so.
    app.command("theme dracula");
    assert_eq!((showing(&mut app, owls), &app.cfg.theme), (ThemeName::AmberDeep.into(), &ThemeName::Dracula.into()));
    assert!(app.windows[owls].lines.back().unwrap().body.contains("has a theme of its own"));

    // The picker starts on "just here" in a room with a theme, and keeps
    // its pick for the room only.
    app.command("theme");
    assert!(app.theme_picker.as_ref().unwrap().just_here);
    app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.cfg.theme, ThemeName::Dracula.into(), "yours untouched");
    assert_eq!(showing(&mut app, owls), ThemeName::AmberTron.into(), "the next one, for Night Owls");

    app.command("theme here off");
    assert_eq!(showing(&mut app, owls), ThemeName::Dracula.into());
    assert!(app.layout.themes.iter().any(|(k, t)| k.room_id == "!work:x.org" && *t == ThemeName::Turbo.into()));
    let saved = std::fs::read_to_string(app.paths.layout_file()).unwrap();
    assert!(saved.contains("turbo") && !saved.contains("amber"), "kept in layout.json: {saved}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn coming_back_from_away_says_what_came_in() {
    let mut app = test_app();
    let room = room_window(&mut app, "!a:x.org", "Team Tests");
    let dm = room_window(&mut app, "!dm:x.org", "sam");
    app.windows[dm].is_dm = true;
    let post = |app: &mut App, room: &str, line: Line| {
        let room_id: OwnedRoomId = room.try_into().unwrap();
        let items = vec![Item::Line(line)];
        app.on_event(AppEvent::Timeline { account: "t".into(), room_id, items, live: true, limited: false });
    };
    let from_sam = |id: &str, body: &str| {
        let mut line = msg(id, body);
        (line.nick, line.sender) = ("sam".into(), Some("@sam:x.org".try_into().unwrap()));
        line
    };
    post(&mut app, "!dm:x.org", from_sam("$0", "before you went"));
    app.on_event(AppEvent::Away { account: "t".into(), message: Some("lunch".into()) });
    let mut ping = msg("$1", "you: flaky test again");
    ping.highlight = true;
    post(&mut app, "!a:x.org", ping);
    post(&mut app, "!dm:x.org", from_sam("$2", "lunch?"));
    app.switch_to(room);
    app.on_event(AppEvent::Away { account: "t".into(), message: None });

    let said = &app.windows[0].lines.back().unwrap().body;
    assert!(said.contains("while you were away") && said.contains("1 mention (Team Tests) and 1 DM (sam)"), "{said}");
    assert!(app.win().is_mentions, "and there it is");
    let collected: Vec<&str> = app.win().lines.iter().map(|l| l.body.as_str()).collect();
    assert!(collected.contains(&"lunch?") && !collected.contains(&"before you went"), "{collected:?}");

    // Back, DMs don't collect there any more.
    post(&mut app, "!dm:x.org", from_sam("$3", "after"));
    assert!(!app.win().lines.iter().any(|l| l.body == "after"));
    app.on_event(AppEvent::Away { account: "t".into(), message: Some("again".into()) });
    app.on_event(AppEvent::Away { account: "t".into(), message: None });
    assert!(app.windows[0].lines.back().unwrap().body.ends_with("nothing for you"));
}

#[test]
fn polls_show_results_and_take_votes() {
    let mut app = test_app();
    let a = room_window(&mut app, "!a:x.org", "Night Owls");
    app.switch_to(a);
    let room: OwnedRoomId = "!a:x.org".try_into().unwrap();
    let mut start = msg("$p", "[poll] Pizza tonight?");
    start.poll = Some(Box::new(Poll {
        question: "Pizza tonight?".into(),
        answers: vec![("a1".into(), "Yes".into()), ("a2".into(), "No".into()), ("a3".into(), "Maybe".into())],
        max: 1,
        disclosed: true,
        ..Poll::default()
    }));
    let vote = |user: &str, at: u64, answer: &str, own: bool| Item::PollVote {
        target: "$p".try_into().unwrap(),
        user: format!("@{user}:x.org").try_into().unwrap(),
        at,
        answers: vec![answer.into()],
        own,
    };
    let items =
        vec![Item::Line(start), vote("sam", 2, "a1", false), vote("jo", 3, "a1", false), vote("me", 4, "a2", true)];
    app.on_event(AppEvent::Timeline { account: "t".into(), room_id: room, items, live: true, limited: false });
    app.show_sidebar = false;
    let screen = |app: &mut App| {
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 14)).unwrap();
        terminal.draw(|f| ui::draw(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..14).map(|y| (0..80).map(|x| buf[(x, y)].symbol()).collect::<String>()).collect::<Vec<_>>().join("\n")
    };
    let shown = screen(&mut app);
    assert!(shown.contains("1. Yes   ███████░░░ 2"), "{shown}");
    assert!(shown.contains("2. No    ███░░░░░░░ 1 ✓"), "{shown}");
    assert!(shown.contains("3 votes"), "{shown}");

    // Select it, Enter: the answers, yours ticked; a number votes.
    app.enter_select();
    app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let v = app.vote.as_ref().expect("the vote list");
    assert_eq!((v.answers.len(), v.max), (3, 1));
    assert!(screen(&mut app).contains(" Vote "));
    app.on_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
    assert!(app.vote.is_none(), "one pick votes straight away");
    assert!(app.win().lines.back().unwrap().body.contains("isn't connected"), "it tried to send");

    assert!(app.cmd_poll("Lunch? | soup").unwrap_err().contains("usage"), "two answers at least");
    assert!(app.cmd_poll("end").is_err(), "not connected, nothing to end");
}

#[test]
fn later_takes_durations_and_times() {
    let now = Local.with_ymd_and_hms(2026, 10, 20, 17, 0, 0).unwrap();
    let at =
        |args: &str| parse_later(args, now).map(|(due, text)| (due.format("%d %H:%M:%S").to_string(), text.to_owned()));
    assert_eq!(at("10m stretch"), Some(("20 17:10:00".into(), "stretch".into())));
    assert_eq!(at("1h30m call mum"), Some(("20 18:30:00".into(), "call mum".into())));
    assert_eq!(at("90s go"), Some(("20 17:01:30".into(), "go".into())));
    assert_eq!(at("18:30 dinner?"), Some(("20 18:30:00".into(), "dinner?".into())), "later today");
    assert_eq!(at("9:00 morning all"), Some(("21 09:00:00".into(), "morning all".into())), "passed today: tomorrow");
    assert_eq!(at("tomorrow 18:30 dinner"), Some(("21 18:30:00".into(), "dinner".into())));
    for bad in ["10m", "soon hello", "10x hi", "25:00 hi", "tomorrow hi", "0m hi", "10"] {
        assert_eq!(at(bad), None, "{bad}");
    }
}

#[test]
fn scheduled_messages_list_and_cancel() {
    let mut app = test_app();
    let soon = Local::now().timestamp() + 600;
    for (n, text) in ["first", "second"].into_iter().enumerate() {
        let item = Scheduled {
            room_id: "!a:x.org".try_into().unwrap(),
            room: "Night Owls".into(),
            due: soon + n as i64,
            text: text.into(),
        };
        app.later.push(("t".into(), item));
    }
    app.cmd_later("").unwrap();
    let shown: Vec<&str> = app.windows[0].lines.iter().rev().take(2).map(|l| l.body.as_str()).collect();
    assert!(shown[1].starts_with(" 1. Night Owls at ") && shown[1].ends_with(": first"), "{shown:?}");
    app.cmd_later("cancel 1").unwrap();
    assert_eq!(app.later.len(), 1);
    assert_eq!(app.later[0].1.text, "second");
    assert!(app.cmd_later("cancel 5").is_err());
    app.cmd_later("cancel all").unwrap();
    assert!(app.later.is_empty());
    assert!(app.cmd_later("in a bit hello").unwrap_err().starts_with("usage"));
    // Due, but not connected: it waits.
    app.later.push((
        "t".into(),
        Scheduled { room_id: "!a:x.org".try_into().unwrap(), room: "x".into(), due: 0, text: "hi".into() },
    ));
    app.on_clock();
    assert_eq!(app.later.len(), 1, "sent once the server's connected");
}

#[test]
fn custom_emoji_complete_with_tab() {
    let mut app = test_app();
    let a = room_window(&mut app, "!a:x.org", "Night Owls");
    app.switch_to(a);
    let mxc = |s: &str| OwnedMxcUri::from(format!("mxc://x.org/{s}"));
    let room: OwnedRoomId = "!a:x.org".try_into().unwrap();
    app.on_event(AppEvent::CustomEmoji {
        account: "t".into(),
        global: vec![("blobcat".into(), mxc("g")), ("party".into(), mxc("p"))],
        rooms: HashMap::from([(room.clone(), vec![("blobcat".into(), mxc("room")), ("blobfox".into(), mxc("f"))])]),
    });
    let mine = app.emoji_for("t", Some(&room));
    assert_eq!(mine[0], ("blobcat".to_owned(), mxc("room")), "the room's own wins");
    assert_eq!(mine.len(), 3);
    app.input.set_text(":blob");
    let offered: Vec<String> = app.candidates().into_iter().take(2).map(|c| c.text).collect();
    assert_eq!(offered, [":blobcat:", ":blobfox:"]);
}

#[test]
fn u_in_the_mentions_window_knows_the_server() {
    let mut app = test_app();
    let room = room_window(&mut app, "!a:x.org", "Team Tests");
    let mut ping = msg("$m", "you: look");
    (ping.highlight, ping.sender) = (true, Some("@carol:x.org".try_into().unwrap()));
    let room_id: OwnedRoomId = "!a:x.org".try_into().unwrap();
    app.on_event(AppEvent::Timeline {
        account: "t".into(),
        room_id,
        items: vec![Item::Line(ping)],
        live: true,
        limited: false,
    });
    let mentions = app.windows.iter().position(|w| w.is_mentions).unwrap();
    assert_ne!(mentions, room);
    app.switch_to(mentions);
    app.enter_select();
    app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE));
    let p = app.profile.as_ref().expect("their profile");
    assert_eq!((p.account.as_str(), p.user.as_str()), ("t", "@carol:x.org"));

    let help: Vec<String> = help_rows("ctrl+t")
        .into_iter()
        .filter_map(|r| if let HelpRow::Entry(k, _) = r { Some(k) } else { None })
        .collect();
    assert!(help.contains(&"Ctrl+T".to_owned()), "F1 has the theme list's keys: {help:?}");
    assert!(!help_rows("ignore").is_empty() && !help_rows("tick").is_empty(), "and the profile's and voting's");
}

#[test]
fn names_open_a_profile_pane() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let mut app = test_app();
    let a = room_window(&mut app, "!a:x.org", "Night Owls");
    app.switch_to(a);
    let user = |n: &str| OwnedUserId::try_from(format!("@{n}:x.org")).unwrap();
    app.windows[a].members = ["alice", "bob", "carol", "dee"]
        .iter()
        .map(|n| Member { name: (*n).into(), user_id: user(n), prefix: if *n == "alice" { '@' } else { ' ' } })
        .collect();
    let mut said = msg("$1", "hello there");
    said.sender = Some(user("bob"));
    said.nick = "bob".into();
    let room: OwnedRoomId = "!a:x.org".try_into().unwrap();
    app.on_event(AppEvent::Timeline {
        account: "t".into(),
        room_id: room,
        items: vec![Item::Line(said)],
        live: true,
        limited: false,
    });
    let screen = |app: &mut App| {
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 24)).unwrap();
        terminal.draw(|f| ui::draw(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..24).map(|y| (0..140).map(|x| buf[(x, y)].symbol()).collect::<String>()).collect::<Vec<_>>().join("\n")
    };
    let click = |app: &mut App, column: u16, row: u16| {
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        });
    };
    screen(&mut app);

    // A name in the member list: their profile, the list kept beside it.
    let (rect, _) = app.hits.nicklist.iter().find(|(_, n)| *n == 0).copied().unwrap();
    click(&mut app, rect.x + 3, rect.y);
    assert_eq!(app.profile.as_ref().map(|p| (p.user.as_str(), p.member)), Some(("@alice:x.org", Some(0))));
    let shown = screen(&mut app);
    assert!(
        shown.contains("@alice:x.org") && shown.contains("admin here") && shown.contains("asking their server"),
        "{shown}"
    );
    assert!(!app.hits.nicklist.is_empty(), "the list stays to pick from");
    app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.profile.as_ref().unwrap().user, user("bob"), "↓: the next member");

    app.on_event(AppEvent::UserInfo {
        account: "t".into(),
        user: user("bob"),
        info: crate::matrix::UserInfo {
            display: Some("Bob Builder".into()),
            identity: Some(true),
            devices: vec![("ABC".into(), "phone".into(), true), ("XYZ".into(), String::new(), false)],
            fetched: true,
            ..Default::default()
        },
    });
    let shown = screen(&mut app);
    assert!(
        shown.contains("verified by you") && shown.contains("2 devices, 1 verified") && shown.contains("phone (ABC)"),
        "{shown}"
    );
    assert!(shown.contains("(they're Bob Builder)"), "{shown}");

    app.on_key(KeyEvent::new(KeyCode::Char('@'), KeyModifiers::NONE));
    assert!(app.profile.is_none() && app.input.text() == "bob: ", "@ mentions them: {:?}", app.input.text());
    app.input.clear();

    // A nick in the chat.
    screen(&mut app);
    let pane = app.hits.panes.iter().find(|p| p.window == a).unwrap();
    let (y, (from, _, _)) = pane.rows.iter().enumerate().find_map(|(y, r)| r.nick.clone().map(|n| (y, n))).unwrap();
    let (x, top) = (from + 1, pane.area.y + y as u16);
    click(&mut app, x, top);
    assert_eq!(app.profile.as_ref().map(|p| p.user.clone()), Some(user("bob")));
    app.on_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    assert_eq!(app.input.text(), "/nickname @bob:x.org ", "changes wait in the input line");
    app.input.clear();

    // u in selection mode.
    app.enter_select();
    app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE));
    assert_eq!(app.profile.as_ref().map(|p| p.user.clone()), Some(user("bob")));
    app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.profile.is_none());
}

#[tokio::test]
async fn exec_runs_a_command_and_shows_its_output() {
    let (text, code) = run_exec("printf '\\033[31mred\\033[0m\\nsecond\\n'; echo oops >&2").await.unwrap();
    assert_eq!((text.as_str(), code), ("red\nsecond\noops", Some(0)), "colours stripped, stderr after stdout");
    let (_, code) = run_exec("exit 3").await.unwrap();
    assert_eq!(code, Some(3));
    let (many, _) = run_exec("seq 1 600").await.unwrap();
    assert!(many.ends_with("… and 100 more lines") && many.starts_with("1\n2\n"), "cut to size");

    let mut app = test_app();
    let room = room_window(&mut app, "!a:x.org", "Night Owls");
    let key = app.windows[room].room.clone();
    app.exec_done(key.clone(), false, "ls", Ok(("a.txt\nb.txt".into(), Some(0))));
    let shown: Vec<&str> = app.windows[room].lines.iter().map(|l| l.body.as_str()).collect();
    assert_eq!(shown, ["a.txt", "b.txt"], "in the window it ran in");
    app.exec_done(key, false, "false", Ok((String::new(), Some(1))));
    let last: Vec<&str> = app.windows[room].lines.iter().rev().take(2).map(|l| l.body.as_str()).collect();
    assert_eq!(last, ["/exec false: exit 1", "/exec false: no output"]);
}

fn screen(app: &mut App, width: u16, height: u16) -> Vec<String> {
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| ui::draw(f, app)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area.height)
        .map(|y| (0..buffer.area.width).map(|x| buffer[(x, y)].symbol().to_owned()).collect::<String>())
        .collect()
}

fn attachment(id: &str, kind: MediaKind, name: &str) -> Line {
    let mut line = msg(id, &format!("[file] {name}"));
    line.media = Some(Media {
        source: matrix_sdk::ruma::events::room::MediaSource::Plain(format!("mxc://x.org/{name}").into()),
        filename: name.into(),
        kind,
        mimetype: None,
        thumbnail: None,
        size: Some(1_258_291),
        animated: None,
    });
    line
}

#[test]
fn the_links_tab_lists_a_rooms_links_to_open_copy_and_go_to() {
    let mut app = test_app();
    let a = room_window(&mut app, "!a:x.org", "Night Owls");
    app.windows[a].lines.push_back(msg("$1", "docs at https://matrix.org/docs"));
    app.windows[a].lines.push_back(msg("$2", "nice"));
    app.windows[a].lines.push_back(msg("$3", "cats: https://example.org/cats, and again https://matrix.org/docs"));
    app.link_titles.insert("https://example.org/cats".into(), LinkTitle::Ready("Cats — Example".into(), None));
    app.switch_to(a);
    app.command("links");
    let key = |app: &mut App, code| app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    let shown = |app: &mut App| screen(app, 120, 30).join("\n");

    let rows = shown(&mut app);
    assert!(rows.contains("links · Night Owls") && rows.contains("2 links, newest first"), "{rows}");
    let (cats, docs) = (rows.find("Cats — Example").unwrap(), rows.find("https://matrix.org/docs").unwrap());
    assert!(cats < docs, "newest first, each once: {rows}");
    assert!(rows.contains("from alice"), "the chosen one beside the list: {rows}");
    let asked: Vec<&str> = app.link_title_requests.iter().map(|(_, url)| url.as_str()).collect();
    assert_eq!(asked, ["https://matrix.org/docs"], "titles asked for what's untitled");

    key(&mut app, KeyCode::Tab);
    assert!(shown(&mut app).contains("media · Night Owls"));
    key(&mut app, KeyCode::Tab);

    key(&mut app, KeyCode::Char('c'));
    assert_eq!(app.clipboard.as_deref(), Some("https://example.org/cats"));

    // / filters, by address, title or who posted it.
    key(&mut app, KeyCode::Char('/'));
    for c in "MATRIX".chars() {
        key(&mut app, KeyCode::Char(c));
    }
    assert!(shown(&mut app).contains("/MATRIX▏ · 1 of 2 links"), "{}", shown(&mut app));
    key(&mut app, KeyCode::Enter);
    assert!(!app.gallery.as_ref().unwrap().filtering);
    key(&mut app, KeyCode::Esc);
    assert!(app.gallery.as_ref().is_some_and(|g| g.filter.is_empty()), "Esc clears the filter first");

    // m goes to the message, where the link was posted last.
    key(&mut app, KeyCode::Down);
    key(&mut app, KeyCode::Char('m'));
    assert!(app.gallery.is_none());
    assert_eq!((app.active, app.select.as_ref().map(|e| e.as_str())), (a, Some("$3")));
}

#[test]
fn the_gallery_shows_a_rooms_media_as_tiles() {
    let mut app = test_app();
    app.picker = Some(Painter {
        picker: ratatui_image::picker::Picker::halfblocks(),
        text: crate::picture::TextMode::Sextants,
        truecolor: true,
    });
    let a = room_window(&mut app, "!a:x.org", "Night Owls");
    app.windows[a].lines.push_back(attachment("$1", MediaKind::Image, "sunset.png"));
    app.windows[a].lines.push_back(msg("$2", "nice"));
    app.windows[a].lines.push_back(attachment("$3", MediaKind::File, "notes.pdf"));
    app.switch_to(a);
    app.command("media");
    let rows = screen(&mut app, 120, 30).join("\n");
    assert!(rows.contains("media · Night Owls"), "{rows}");
    assert!(rows.contains("1 picture, 1 file, newest first"), "{rows}");
    let (pdf, png) = (rows.find("notes.pdf").unwrap(), rows.find("sunset.png").unwrap());
    assert!(pdf < png, "newest first");
    assert!(rows.contains("PDF") && rows.contains("1.2 MB"), "a file is a labelled box");
    let asked: Vec<&str> = app.tile_requests.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(asked, ["$1"], "only pictures are fetched");
    assert!(app.windows[a].wants_older, "and it looks further back for more");

    app.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    assert_eq!(app.gallery.as_ref().unwrap().selected.as_ref().map(|e| e.as_str()), Some("$1"));
    let rows = screen(&mut app, 120, 30);
    assert!(rows.iter().any(|r| r.contains("sunset.png · 1.2 MB · from alice")), "the chosen one, in full");

    app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.gallery.is_none() && !app.windows[a].wants_older && app.redraw_all);
    assert!(screen(&mut app, 120, 30).join("\n").contains("nice"), "the messages again");

    app.switch_to(0);
    app.command("media");
    assert!(app.gallery.is_none(), "only in a room");
}

#[test]
fn home_shows_whats_waiting_and_goes_there() {
    let mut app = test_app();
    let a = room_window(&mut app, "!a:x.org", "Night Owls");
    let b = room_window(&mut app, "!b:x.org", "Book Club");
    let _c = room_window(&mut app, "!c:x.org", "Garden Club");
    app.windows[a].lines.push_back(msg("$1", "you: look at this"));
    app.record_mention(a, &app.windows[a].lines[0].clone(), true);
    app.windows[a].activity = Activity::Highlight;
    let mut said = msg("$2", "chapter 3 spoilers incoming");
    said.nick = "bob".into();
    app.windows[b].lines.push_back(said);
    app.windows[b].activity = Activity::Message;
    app.on_event(AppEvent::Invites {
        account: "t".into(),
        invites: vec![Invitation {
            account: "t".into(),
            room_id: "!d:x.org".try_into().unwrap(),
            name: "Synth Nerds".into(),
            inviter: Some("sam".into()),
            inviter_id: Some("@sam:x.org".into()),
        }],
    });
    app.on_key(KeyEvent::new(KeyCode::F(4), KeyModifiers::NONE));
    let rows = screen(&mut app, 120, 30).join("\n");
    for want in
        ["Mentions", "alice: you: look at this", "Unread", "bob: chapter 3 spoilers incoming", "Invites", "from sam"]
    {
        assert!(rows.contains(want), "{want} in {rows}");
    }
    let order: Vec<&str> = app.home_rows().iter().map(|r| r.section()).collect();
    assert_eq!(order, ["Mentions", "Unread", "Unread", "Invites"], "not Garden Club: nothing new there");

    // d, then anything else, keeps the invitation.
    app.on_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
    app.on_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
    assert!(screen(&mut app, 120, 30).join("\n").contains("d again declines Synth Nerds"));
    app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert!(app.home.as_ref().unwrap().declining.is_none());

    app.on_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
    assert!(app.home.is_none());
    assert_eq!(app.active, b, "3 went to the third row");
}

#[test]
fn the_changelog_reads_like_the_release_notes() {
    let mut app = test_app();
    let a = room_window(&mut app, "!a:x.org", "Team Tests");
    app.switch_to(a);
    let current = env!("CARGO_PKG_VERSION");
    let release = |version: &str, notes: &str| update::Release {
        version: version.into(),
        date: "2026-10-20".into(),
        notes: notes.into(),
    };
    let releases = vec![
        release("99.0.0", "### Added\n- Home (F4, or `/home`): what's waiting\n  for you in one place."),
        release(current, "### Fixed\n- `u` opens the profile."),
    ];
    app.changelog_done(app.windows[a].room.clone(), Ok(releases));
    let rows = screen(&mut app, 200, 30);
    let find = |text: &str| rows.iter().position(|r| r.contains(text)).unwrap_or_else(|| panic!("{text}: {rows:#?}"));
    assert!(find(&format!("Textrix {current} · 2026-10-20 · the one you're running")) < find("Textrix 99.0.0"));
    assert!(find("newer than yours: brew update && brew upgrade textrix") == find("Textrix 99.0.0"));
    assert!(rows[find("Added") + 1].contains("• Home (F4, or /home): what's waiting for you in one place."));

    app.changelog_done(None, Err("rate limited".into()));
    assert_eq!(app.windows[0].lines.back().unwrap().body, "couldn't get the changelog: rate limited");
    assert!(app.cmd_changelog("0").is_err() && app.cmd_changelog("many").is_err());
}

#[test]
fn alt_r_goes_to_where_you_read_up_to() {
    let mut app = test_app();
    let a = room_window(&mut app, "!a:x.org", "Night Owls");
    for n in 1..=5 {
        app.windows[a].lines.push_back(msg(&format!("${n}"), "hi"));
    }
    app.switch_to(a);
    let alt_r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT);
    app.on_key(alt_r);
    assert!(app.select.is_none(), "no marker, nowhere to go");

    app.windows[a].marker = Some("$2".try_into().unwrap());
    app.on_key(alt_r);
    assert_eq!(app.select.as_ref().map(|e| e.as_str()), Some("$2"), "the last one read, above the line");

    app.select = None;
    app.windows[a].marker = Some("$0".try_into().unwrap());
    app.command("jump new");
    assert_eq!(app.windows[a].seek.as_ref().map(|e| e.as_str()), Some("$0"), "not loaded: history loads back to it");

    app.windows[a].seek = None;
    app.windows[a].marker = Some("$5".try_into().unwrap());
    app.on_key(alt_r);
    assert!(app.select.is_none() && app.windows[a].seek.is_none());
    assert_eq!(app.windows[a].lines.back().unwrap().body, "nothing unread here");
}

#[tokio::test]
async fn notify_command_hears_mentions_through_its_environment() {
    let mut app = test_app();
    let out = std::env::temp_dir().join(format!("textrix-hook-{}", std::process::id()));
    app.cfg.notify_command = format!(
        "printf '%s|%s|%s|%s' \"$TEXTRIX_KIND\" \"$TEXTRIX_FROM\" \"$TEXTRIX_ROOM\" \"$TEXTRIX_TEXT\" > '{}'",
        out.display()
    );
    room_window(&mut app, "!a:x.org", "Night Owls");
    let mut line = msg("$1", "you: look $(echo injected)");
    (line.highlight, line.nick, line.sender) = (true, "sam".into(), Some("@sam:x.org".try_into().unwrap()));
    let items = vec![Item::Line(line)];
    app.on_event(AppEvent::Timeline {
        account: "t".into(),
        room_id: "!a:x.org".try_into().unwrap(),
        items,
        live: true,
        limited: false,
    });
    let mut heard = String::new();
    for _ in 0..100 {
        heard = std::fs::read_to_string(&out).unwrap_or_default();
        if !heard.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    std::fs::remove_file(&out).ok();
    assert_eq!(heard, "mention|sam|Night Owls|you: look $(echo injected)", "the text is data, never run");

    app.in_script = 1;
    assert!(app.cmd_set("notify_command \"touch x\"").is_err(), "not from scripts");
    assert!(app.cmd_set("editor \"touch x\"").is_err());
}

#[test]
fn verification_is_a_panel_with_one_key_a_step() {
    let mut app = test_app();
    let verify = |stage| AppEvent::Verify { account: "t".into(), stage };
    let who = "your other session (Element on iPhone)".to_owned();
    let waiting_for = |app: &App| match &app.verify_panel.as_ref().unwrap().stage {
        VerifyStage::Waiting { what, .. } => *what,
        other => panic!("{other:?}"),
    };

    app.on_event(verify(VerifyStage::Asked { who: who.clone() }));
    let rows = screen(&mut app, 100, 30).join("\n");
    assert!(rows.contains(&format!("Verification request from {who}")) && rows.contains("Enter accept"), "{rows}");
    app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(waiting_for(&app), "to answer");

    let emoji = [
        ("🐶", "Dog"),
        ("🔑", "Key"),
        ("🎸", "Guitar"),
        ("☂️", "Umbrella"),
        ("📌", "Pin"),
        ("🚀", "Rocket"),
        ("🎧", "Headphones"),
    ]
    .map(|(e, name)| (e.to_owned(), name.to_owned()))
    .to_vec();
    app.on_event(verify(VerifyStage::Compare { who: who.clone(), emoji, numbers: None }));
    for width in [100, 50] {
        let rows = screen(&mut app, width, 30).join("\n");
        assert!(rows.contains("Do these match") && rows.contains("Dog") && rows.contains("Headphones"), "{rows}");
    }
    app.on_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    assert_eq!(waiting_for(&app), "to confirm too");

    app.on_event(verify(VerifyStage::Done { who: who.clone() }));
    assert!(screen(&mut app, 100, 30).join("\n").contains(&format!("✓ Verified {who}")));
    app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    assert!(app.verify_panel.is_none(), "any key closes it");

    // Esc on a request leaves it for later, and its cancelling doesn't
    // pop up again.
    app.on_event(verify(VerifyStage::Asked { who: who.clone() }));
    app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.verify_panel.is_none());
    app.on_event(verify(VerifyStage::Cancelled { reason: "the other side cancelled".into() }));
    assert!(app.verify_panel.is_none());
}

#[test]
fn a_pasted_image_waits_in_the_input_line_to_send() {
    let mut app = test_app();
    let a = room_window(&mut app, "!a:x.org", "Night Owls");
    app.switch_to(a);
    let dir = std::env::temp_dir().join(format!("textrix-pasted-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let paste = |app: &mut App| {
        let file = dir.join("Pasted image.png");
        image::RgbImage::new(4, 3).save(&file).unwrap();
        app.on_event(AppEvent::Pasted { image: Ok(Some(file.clone())), picture: None });
        file
    };
    let bar = |app: &mut App| screen(app, 160, 20).join("\n");

    let file = paste(&mut app);
    let shown = bar(&mut app);
    assert!(shown.contains("pasted image, 4×3 PNG") && shown.contains("Esc drops it"), "{shown}");
    assert!(shown.contains("[image caption]"), "what you type is its caption: {shown}");
    app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.pasted.is_none() && !file.exists(), "Esc drops it, file and all");

    // A command still runs, and the image waits.
    paste(&mut app);
    app.input.set_text("/help paste");
    app.submit();
    assert!(app.pasted.is_some());
    let said = &app.windows[a].lines.back().unwrap().body;
    assert!(said.contains("/paste"), "{said}");

    // Not connected: it can't go, so nothing's lost.
    app.input.set_text("the view from here");
    app.submit();
    assert!(app.pasted.is_some());
    assert_eq!(app.input.text(), "the view from here");

    app.on_event(AppEvent::Pasted { image: Ok(None), picture: None });
    assert!(app.windows[a].lines.back().unwrap().body.contains("no image on the clipboard"));
    app.drop_pasted();
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn clicking_the_input_line_stops_selecting() {
    let mut app = test_app();
    let a = room_window(&mut app, "!a:x.org", "Night Owls");
    app.windows[a].lines.push_back(msg("$1", "pizza tonight?"));
    app.switch_to(a);
    app.input.set_text("half typed");
    app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
    assert!(app.select.is_some());
    screen(&mut app, 100, 20);
    let (area, text_x, _) = app.hits.input.unwrap();
    app.on_click(Position { x: text_x + 4, y: area.y });
    assert!(app.select.is_none());
    assert_eq!(app.input.cursor(), 4, "and the cursor goes where you clicked");
}

#[test]
fn typing_a_command_while_selecting_runs_it() {
    let mut app = test_app();
    let a = room_window(&mut app, "!a:x.org", "Night Owls");
    app.windows[a].lines.push_back(msg("$1", "pizza tonight?"));
    app.switch_to(a);
    let windows = app.windows.len();
    app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
    assert!(app.select.is_some());
    for c in "/theme disco".chars() {
        app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    assert!(app.select.is_none());
    assert_eq!(app.input.text(), "/theme disco", "the t is typed, not a thread");
    assert_eq!(app.windows.len(), windows);
}

#[test]
fn thread_windows_close_back_to_their_room() {
    let mut app = test_app();
    let room = room_window(&mut app, "!r:x.org", "Night Owls");
    room_window(&mut app, "!o:x.org", "Book Club");
    let key = WinKey { thread: Some("$root".try_into().unwrap()), ..app.windows[room].room.clone().unwrap() };
    let open = |app: &mut App| {
        app.windows.push(Window::new(Some(key.clone()), "⤷ thread".into()));
        app.switch_to(app.windows.len() - 1);
        app.windows.len()
    };
    let closed = |app: &App, before: usize| app.windows.len() == before - 1 && app.active == room;

    let before = open(&mut app);
    app.input.set_text("half a reply");
    app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.windows.len(), before, "Esc keeps it while you're typing");
    app.input.clear();
    app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(closed(&app, before), "Esc closes it");

    for command in ["/wc", "/part", "/close"] {
        let before = open(&mut app);
        app.input.set_text(command);
        app.submit();
        assert!(closed(&app, before), "{command} closes it");
        assert_eq!(app.window_idx("t", &"!r:x.org".try_into().unwrap()), Some(room), "the room stays");
    }

    // Esc in a room's own window closes nothing.
    let before = app.windows.len();
    app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.windows.len(), before);
}

#[test]
fn esc_typed_just_before_a_key_does_not_eat_it() {
    let mut app = test_app();
    let a = room_window(&mut app, "!a:x.org", "Night Owls");
    app.windows[a].lines.push_back(msg("$1", "pizza tonight?"));
    app.switch_to(a);
    let alt = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT);
    let type_in = |app: &mut App, text: &str| {
        for c in text.chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
    };
    // Esc and "/" as one Alt+/: the slash stays.
    app.on_key(alt('/'));
    type_in(&mut app, "quit");
    assert_eq!(app.input.text(), "/quit");
    app.input.clear();
    app.on_key(KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::ALT | KeyModifiers::SHIFT));
    assert_eq!(app.input.text(), "Q");
    app.input.clear();

    // Leaving selection mode the same way.
    app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
    assert!(app.select.is_some());
    app.on_key(alt('/'));
    type_in(&mut app, "me waves");
    assert!(app.select.is_none());
    assert_eq!(app.input.text(), "/me waves");
    app.input.clear();

    // Alt keys that mean something still do, even from selecting.
    let b = room_window(&mut app, "!b:x.org", "Book Club");
    app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
    app.on_key(alt(char::from(b'1' + a as u8)));
    assert!(app.select.is_none() && app.active == a && app.input.text().is_empty(), "{b}");
}

#[test]
fn stickers_are_chosen_from_a_grid_or_named() {
    use crate::stickers::Sticker;
    let mut app = test_app();
    let a = room_window(&mut app, "!a:x.org", "Night Owls");
    app.switch_to(a);
    assert!(app.cmd_sticker("").unwrap_err().contains("aren't loaded yet"));
    let sticker = |code: &str, body: &str| Sticker {
        code: code.into(),
        body: body.into(),
        url: OwnedMxcUri::from(format!("mxc://x.org/{code}")),
        info: serde_json::json!({}),
    };
    let room: OwnedRoomId = "!a:x.org".try_into().unwrap();
    app.on_event(AppEvent::Stickers {
        account: "t".into(),
        global: vec![sticker("blobcat", "a blob cat"), sticker("party", "party")],
        rooms: HashMap::from([(room.clone(), vec![sticker("party", "the room's party"), sticker("wave", "hello")])]),
    });
    let mine = app.stickers_for("t", &room);
    assert_eq!(mine.iter().map(|s| s.code.as_str()).collect::<Vec<_>>(), ["party", "wave", "blobcat"]);
    assert_eq!(mine[0].body, "the room's party", "the room's own wins");

    app.cmd_sticker("").unwrap();
    let rows = screen(&mut app, 100, 30).join("\n");
    assert!(rows.contains("Stickers · Night Owls") && rows.contains("blobcat") && rows.contains("wave"), "{rows}");
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    app.on_key(key(KeyCode::Right));
    assert_eq!(app.sticker_picker.as_ref().and_then(|p| p.chosen()).map(|s| s.code.as_str()), Some("wave"));
    for c in "blob".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    let rows = screen(&mut app, 100, 30).join("\n");
    assert!(rows.contains("› blob") && rows.contains(":blobcat: · a blob cat") && !rows.contains("wave"), "{rows}");
    // Enter sends: here, without the server, it says so.
    app.on_key(key(KeyCode::Enter));
    assert!(app.sticker_picker.is_none());
    let said = |app: &App, text: &str| app.windows.iter().any(|w| w.lines.iter().any(|l| l.body.contains(text)));
    assert!(said(&app, "t is not connected"), "the error is shown");
    app.cmd_sticker("").unwrap();
    app.on_key(key(KeyCode::Esc));
    assert!(app.sticker_picker.is_none(), "Esc closes it");

    // By name.
    assert!(app.cmd_sticker("dog").unwrap_err().contains("no sticker called dog"));
    assert!(app.cmd_sticker("a").unwrap_err().starts_with("which?"));
    assert!(app.cmd_sticker(":wave:").unwrap_err().contains("is not connected"));
    // A room on a server with none.
    app.stickers.insert("t".into(), (Vec::new(), HashMap::new()));
    assert!(app.cmd_sticker("").unwrap_err().contains("has stickers"));
}

#[test]
fn plus_opens_the_emoji_picker() {
    let mut app = test_app();
    let a = room_window(&mut app, "!a:x.org", "Night Owls");
    app.windows[a].lines.push_back(msg("$1", "pizza tonight?"));
    app.switch_to(a);
    app.layout.recent_emoji = vec!["🍕".into()];
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
    app.on_key(key(KeyCode::Char('+')));
    let rows = screen(&mut app, 100, 30).join("\n");
    assert!(rows.contains("React to alice") && rows.contains("Recent") && rows.contains("🍕"), "{rows}");
    for c in "fire".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    let rows = screen(&mut app, 100, 30).join("\n");
    assert!(rows.contains("› fire") && rows.contains("fire :fire:"), "{rows}");
    app.on_key(key(KeyCode::Esc));
    assert!(app.emoji_picker.is_none() && app.select.is_some(), "Esc closes it, still selecting");
}

#[test]
fn sent_and_read_are_told_apart() {
    let mut app = test_app();
    let a = room_window(&mut app, "!dm:x.org", "sam");
    let now = Local::now();
    let sam: OwnedUserId = "@sam:x.org".try_into().unwrap();
    let mut theirs = msg("$1", "you around?");
    (theirs.nick, theirs.sender, theirs.ts) = ("sam".into(), Some(sam.clone()), now - chrono::TimeDelta::minutes(10));
    let mut mine = msg("$2", "yes, just back");
    (mine.nick, mine.own, mine.ts) = ("me".into(), true, now - chrono::TimeDelta::minutes(1));
    app.windows[a].lines.extend([theirs, mine]);
    app.switch_to(a);
    let row = |app: &mut App, text: &str| {
        screen(app, 120, 12).into_iter().find(|r| r.contains(text)).unwrap_or_default().trim_end().to_owned()
    };
    assert!(row(&mut app, "just back").ends_with("just back ✓"), "sent, not read: {}", row(&mut app, "just back"));

    // A receipt for something not drawn (their reaction, say), made
    // before your message: it doesn't cover your message.
    let receipt = |event: &str, ts| AppEvent::Receipts {
        account: "t".into(),
        room_id: "!dm:x.org".try_into().unwrap(),
        receipts: vec![(sam.clone(), "sam".into(), event.try_into().unwrap(), Some(ts))],
        live: true,
    };
    app.on_event(receipt("$reaction", now - chrono::TimeDelta::minutes(5)));
    assert!(row(&mut app, "just back").ends_with("just back ✓"), "{}", row(&mut app, "just back"));

    // They read it: two ticks and who.
    app.on_event(receipt("$2", now));
    assert!(row(&mut app, "just back").ends_with("just back ✓✓ sam"), "{}", row(&mut app, "just back"));

    // A reply means they read what came before: no marks left there.
    let mut more = msg("$3", "and staying");
    (more.nick, more.own, more.ts) = ("me".into(), true, now);
    let mut reply = msg("$4", "nice");
    (reply.nick, reply.sender, reply.ts) = ("sam".into(), Some(sam.clone()), now);
    let items = vec![Item::Line(more), Item::Line(reply)];
    app.on_event(AppEvent::Timeline {
        account: "t".into(),
        room_id: "!dm:x.org".try_into().unwrap(),
        items,
        live: true,
        limited: false,
    });
    assert!(row(&mut app, "and staying").ends_with("and staying"), "{}", row(&mut app, "and staying"));
}

#[test]
fn a_voice_message_shows_and_goes() {
    let mut app = test_app();
    let a = room_window(&mut app, "!a:x.org", "Night Owls");
    let key = app.windows[a].room.clone().unwrap();
    let file = std::env::temp_dir().join(format!("textrix-voice-test-{}.ogg", std::process::id()));
    std::fs::write(&file, b"OggS").unwrap();
    let voice = crate::voice::Voice {
        file: file.clone(),
        duration: Duration::from_millis(12_300),
        waveform: vec![0.1, 0.5, 1.0, 0.5, 0.1],
    };
    app.voice_done(key.clone(), Ok(Some(voice)));
    assert!(!file.exists(), "not connected: not kept either");
    assert!(app.windows[0].lines.back().unwrap().body.contains("isn't connected"));
    app.voice_done(key.clone(), Ok(None));
    assert_eq!(app.windows[0].lines.back().unwrap().body, "voice message thrown away");
    app.voice_done(key, Err("that was too short to send".into()));
    assert_eq!(app.windows[0].lines.back().unwrap().body, "that was too short to send");
}

/// A plugin as it runs: hello.py, in Python, talking to the app.
#[tokio::test]
async fn a_plugin_starts_hears_and_answers() {
    if !crate::audio::on_path("python3") {
        return;
    }
    let dir = std::env::temp_dir().join(format!("textrix-plugins-{}", std::process::id()));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let paths = Paths { base: dir.clone(), env_file: dir.join("none") };
    let mut app = App::new(Config::default(), paths.clone(), EnvFile::new(paths.env_file), Servers::default(), tx);
    let a = room_window(&mut app, "!a:x.org", "Night Owls");
    room_window(&mut app, "!b:x.org", "Book Club");
    std::fs::create_dir_all(app.plugins_dir()).unwrap();
    std::fs::write(app.plugins_dir().join("hello.py"), crate::plugin::starter("hello")).unwrap();
    let config = crate::plugin::PluginConfig {
        command: "python3 hello.py".into(),
        rooms: vec!["Night Owls".into()],
        enabled: true,
        settings: toml::Table::new(),
    };
    app.plugins.insert("hello".into(), plugins::PluginState::new(config));
    app.start_plugins();
    // Until the app has heard what it's waiting for.
    async fn until(app: &mut App, rx: &mut UnboundedReceiver<AppEvent>, done: impl Fn(&App) -> bool) {
        while !done(app) {
            let event = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.expect("in time").unwrap();
            app.on_event(event);
        }
    }
    until(&mut app, &mut rx, |app| app.plugin_status_items() == ["0 pings"]).await;
    assert_eq!(app.plugin_commands().iter().map(|(_, c)| c.name.as_str()).collect::<Vec<_>>(), ["hello"]);
    assert!(app.windows[0].lines.iter().any(|l| l.body == "plugin hello started: /hello"));

    // It hears its room, not others, and its reply can't go: no server.
    let key = app.windows[a].room.clone().unwrap();
    let mut ping = msg("$p", "!ping");
    (ping.nick, ping.sender) = ("sam".into(), Some("@sam:x.org".try_into().unwrap()));
    let elsewhere = WinKey { room_id: "!b:x.org".try_into().unwrap(), ..key.clone() };
    app.tell_plugins(&elsewhere, &Heard::Message(ping.clone()));
    app.tell_plugins(&key, &Heard::Message(ping));
    until(&mut app, &mut rx, |app| {
        app.windows[0].lines.iter().any(|l| l.body.starts_with("hello: heard:") && l.body.contains("isn't connected"))
    })
    .await;
    assert_eq!(app.plugin_status_items(), ["1 ping"], "heard once, in its own room");

    app.cmd_plugin("list").unwrap();
    assert!(app.windows[0].lines.iter().any(|l| l.body.contains("hello: running for")));
    app.cmd_plugin("stop hello").unwrap();
    assert!(app.plugin_commands().is_empty() && app.plugin_status_items().is_empty());
    assert!(app.plugin_command("hello", "").is_none(), "its command goes with it");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn scripts_cant_exec() {
    let mut app = test_app();
    let source = r#"command("sneaky", "", |args, ctx| run("/exec echo hi"));"#;
    app.scripts.load("sneaky", None, source).unwrap();
    app.command("sneaky");
    app.on_clock();
    let said = app.windows.iter().flat_map(|w| &w.lines).map(|l| l.body.as_str()).collect::<Vec<_>>();
    assert!(said.iter().any(|b| b.contains("scripts can't run programs")), "{said:?}");
    assert_eq!(app.in_script, 0);
}

#[test]
fn edits_keep_what_the_message_said_before() {
    let mut w = Window::status();
    let mut original = msg("$a", "teh first");
    original.ts = Local::now() - chrono::TimeDelta::minutes(10);
    w.push(original, 100);
    let edit = |mins: i64, body: &str| {
        let mut fallback = msg("$e", body);
        fallback.ts = Local::now() - chrono::TimeDelta::minutes(mins);
        Item::Edit { target: "$a".try_into().unwrap(), body: body.into(), fallback }
    };
    w.apply(edit(5, "the first"), 100, true);
    w.apply(edit(1, "the first!"), 100, true);
    // An older edit turning up late (from history) doesn't win.
    w.apply(edit(7, "the frist"), 100, false);
    let line = w.lines.back().unwrap();
    assert_eq!(line.body, "the first!");
    let said: Vec<&str> = line.versions.iter().map(|(_, b)| b.as_str()).collect();
    assert_eq!(said, ["teh first", "the frist", "the first"], "oldest first");

    let mut app = test_app();
    app.windows[0] = w;
    app.enter_select();
    app.on_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
    let shown: Vec<&str> = app.windows[0].lines.iter().rev().take(5).map(|l| l.body.as_str()).collect();
    assert!(shown[4].contains("edited 3 times") && shown[0].ends_with("the first! (now)"), "{shown:?}");
}

#[test]
fn ctrl_x_hands_the_message_to_the_editor() {
    let mut app = test_app();
    app.input.set_text("draft");
    app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
    assert_eq!(app.editor_request.as_deref(), Some("draft"));
    app.on_event(AppEvent::Edited(Ok("draft\nwith more".into())));
    assert_eq!(app.input.text(), "draft\nwith more");
    assert!(app.redraw_all, "the editor had the screen");
    app.on_event(AppEvent::Edited(Err("vi stopped".into())));
    assert_eq!(app.input.text(), "draft\nwith more", "a failed edit leaves it as it was");
}

#[test]
fn textrix_lines_stand_apart_in_rooms() {
    let mut app = test_app();
    let a = room_window(&mut app, "!a:x.org", "Night Owls");
    app.switch_to(a);
    let room: OwnedRoomId = "!a:x.org".try_into().unwrap();
    let say = |app: &mut App, id: &str, body: &str| {
        let items = vec![Item::Line(msg(id, body))];
        app.on_event(AppEvent::Timeline {
            account: "t".into(),
            room_id: room.clone(),
            items,
            live: true,
            limited: false,
        });
    };
    say(&mut app, "$1", "before");
    app.info("theme: dracula");
    app.error("that went wrong");
    say(&mut app, "$2", "after");
    app.show_sidebar = false;
    app.show_nicklist = false;
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
    terminal.draw(|f| ui::draw(f, &mut app)).unwrap();
    let buf = terminal.backend().buffer().clone();
    let rows: Vec<String> =
        (0..buf.area.height).map(|y| (0..80).map(|x| buf[(x, y)].symbol()).collect::<String>()).collect();
    let at = |text: &str| rows.iter().position(|r| r.contains(text)).unwrap_or_else(|| panic!("{text}: {rows:#?}"));
    let (before, theme, wrong, after) = (at("before"), at("theme: dracula"), at("that went wrong"), at("after"));
    assert_eq!([theme - before, wrong - theme, after - wrong], [2, 1, 2], "a blank row either side of the run");
    assert!(rows[before + 1].trim().is_empty() && rows[wrong + 1].trim().is_empty());
    let x = rows[theme].find("theme").unwrap() as u16;
    let cell = &buf[(x, theme as u16)];
    assert!(
        cell.modifier.contains(ratatui::style::Modifier::ITALIC) && cell.fg == app.cfg.palette.dim.fg.unwrap(),
        "quieter"
    );
    assert!(
        buf[(rows[wrong].find("that").unwrap() as u16, wrong as u16)]
            .modifier
            .contains(ratatui::style::Modifier::ITALIC)
    );
}

#[test]
fn the_theme_picker_shows_each_theme_as_you_move() {
    use crate::theme::ThemeName;
    let dir = std::env::temp_dir().join(format!("textrix-picker-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("themes")).unwrap();
    std::fs::write(dir.join("themes/broken.toml"), "base = \"plaid\"").unwrap();
    let mut app = test_app();
    app.paths.base = dir.clone();
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    let draw = |app: &mut App| {
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 40)).unwrap();
        terminal.draw(|f| ui::draw(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    };

    app.command("theme");
    assert!(app.theme_picker.is_some(), "/theme on its own opens it too");
    let screen = draw(&mut app);
    assert!(screen.contains("Themes") && screen.contains("• default") && screen.contains("Classic"), "{screen}");
    app.on_key(key(KeyCode::Down));
    assert_eq!(app.cfg.theme, ThemeName::Light.into(), "moving to a theme shows it");
    assert_eq!(app.cfg.palette.pane, Theme::build(ThemeName::Light, &BTreeMap::new()).unwrap().pane);
    app.on_key(key(KeyCode::Esc));
    assert!(app.theme_picker.is_none());
    assert_eq!(app.cfg.theme, ThemeName::Default.into(), "Esc puts back the one you had");

    app.command("themes");
    for c in "drac".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    assert_eq!(app.cfg.theme, ThemeName::Dracula.into(), "typing narrows the list to it");
    app.on_key(key(KeyCode::Enter));
    assert!(app.theme_picker.is_none());
    assert_eq!(app.cfg.theme, ThemeName::Dracula.into(), "Enter keeps it");
    assert!(app.windows[0].lines.back().unwrap().body.contains("/save keeps it"));

    // Your files are on the list; a broken one says why and isn't kept.
    app.command("themes");
    app.on_key(key(KeyCode::End));
    assert!(app.theme_picker.as_ref().unwrap().error.as_ref().is_some_and(|e| e.contains("plaid")));
    assert_eq!(app.cfg.theme, ThemeName::Dracula.into());
    app.on_key(key(KeyCode::Enter));
    assert_eq!(app.cfg.theme, ThemeName::Dracula.into());

    // A click shows a theme, a second click keeps it.
    app.command("themes");
    draw(&mut app);
    let (_, rows) = app.hits.theme_picker.clone().unwrap();
    let (rect, n) = rows.iter().find(|(_, n)| *n == 4).copied().unwrap();
    let click = |app: &mut App| app.on_click(Position::new(rect.x + 2, rect.y));
    click(&mut app);
    assert_eq!((app.theme_picker.as_ref().unwrap().selected, &app.cfg.theme), (n, &ThemeName::Green.into()));
    click(&mut app);
    assert!(app.theme_picker.is_none() && app.cfg.theme == ThemeName::Green.into());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn themes_are_saved_and_imported() {
    let dir = std::env::temp_dir().join(format!("textrix-theme-cmd-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut app = test_app();
    app.paths.base = dir.clone();
    app.cmd_theme("dracula").unwrap();
    app.cfg.colors.insert("link".into(), "red".into());
    app.cmd_theme("save vampire").unwrap();
    let saved = std::fs::read_to_string(dir.join("themes/vampire.toml")).unwrap();
    assert!(saved.contains("base = \"dracula\"") && saved.contains("link = \"red\""), "{saved}");
    assert!(app.cmd_theme("save nord").is_err(), "not under a built-in name");
    assert!(app.cmd_theme("save list").is_err(), "nor a word /theme takes");
    app.cmd_theme("list").unwrap();
    assert!(app.windows[0].lines.iter().any(|l| l.body.contains("vampire")), "/theme list prints them");
    app.cmd_theme("vampire").unwrap();
    assert_eq!(app.cfg.theme, ThemeId::File("vampire".into()));
    app.cmd_theme("save vampire").unwrap();
    app.cmd_theme("default").unwrap();
    assert!(app.cmd_theme("save vampire").is_err(), "not over another theme");

    // Sent to a friend, who imports it by its name = line.
    let sent = dir.join("sent.toml");
    std::fs::write(&sent, saved.replace("name = \"vampire\"", "name = \"bat\"")).unwrap();
    app.cmd_theme(&format!("import {}", sent.display())).unwrap();
    app.cmd_theme("bat").unwrap();
    assert!(app.cmd_theme(&format!("import {}", sent.display())).is_err(), "already there");
    std::fs::write(&sent, "base = \"plaid\"").unwrap();
    assert!(app.cmd_theme(&format!("import {}", sent.display())).unwrap_err().contains("plaid"));
    assert!(app.cmd_theme("nosuch").is_err());
    assert!(app.cmd_theme("../../etc/passwd").is_err());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn panes_resize_by_command_and_by_dragging() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let mut app = test_app();
    let a = room_window(&mut app, "!a:x.org", "Night Owls");
    room_window(&mut app, "!b:x.org", "Book Club");
    app.switch_to(a);
    app.cmd_split("Book Club", false).unwrap();
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
    let mut heights = |app: &mut App| -> Vec<u16> {
        terminal.draw(|f| ui::draw(f, app)).unwrap();
        app.hits.splits.iter().map(|(r, _)| r.height).collect()
    };
    let even = heights(&mut app);
    assert!(even.len() == 2 && even[0].abs_diff(even[1]) <= 1, "{even:?}");
    let room = even[0] + even[1];

    app.cmd_resize("+4").unwrap();
    assert_eq!(heights(&mut app), [even[0] + 4, room - even[0] - 4], "the focused pane grows");
    app.cmd_resize("-100").unwrap();
    assert_eq!(heights(&mut app)[0], PANE_MIN.0, "not past the smallest");
    app.cmd_resize("=").unwrap();
    assert_eq!(heights(&mut app), even);
    assert!(app.cmd_resize("lots").is_err());

    // Drag the upper pane's title bar down to row 20.
    let (top, bar) = (app.hits.splits[0].0.y, app.hits.splits[0].0.bottom() - 1);
    let mouse = |kind, row| MouseEvent { kind, column: 30, row, modifiers: KeyModifiers::NONE };
    app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), bar));
    assert_eq!(app.dragging, Some(0));
    app.on_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 20));
    app.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 20));
    assert_eq!(app.dragging, None);
    let dragged = heights(&mut app);
    assert_eq!(dragged[0], 20 - top + 1);

    // Saved layouts keep the sizes.
    let dir = std::env::temp_dir().join(format!("textrix-resize-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    app.paths.base = dir.clone();
    app.cmd_layout("save tall").unwrap();
    app.cmd_resize("=").unwrap();
    app.cmd_layout("tall").unwrap();
    assert_eq!(heights(&mut app), dragged);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn layouts_are_saved_and_come_back() {
    let dir = std::env::temp_dir().join(format!("textrix-layouts-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let paths = Paths { base: dir.clone(), env_file: dir.join("none") };
    let start = || {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app =
            App::new(Config::default(), paths.clone(), EnvFile::new(paths.env_file.clone()), Servers::default(), tx);
        let a = room_window(&mut app, "!a:x.org", "Night Owls");
        let b = room_window(&mut app, "!b:x.org", "Book Club");
        (app, a, b)
    };
    let (mut app, a, b) = start();
    app.switch_to(a);
    app.cmd_split("Book Club", true).unwrap();
    app.cmd_layout("save reading").unwrap();
    app.cmd_unsplit("all").unwrap();
    app.show_sidebar = false;
    app.cmd_layout("load reading").unwrap();
    let split = app.split.as_ref().unwrap();
    assert!(split.side_by_side && split.panes.len() == 2);
    assert!(app.show_sidebar, "the panels come back too");
    assert_eq!(app.active, a, "and the focus");
    app.cmd_unsplit("all").unwrap();
    app.cmd_layout("reading").unwrap();
    assert!(app.split.is_some(), "/layout <name> loads it");
    assert!(app.cmd_layout("load nope").is_err());

    // The split you quit with is back next time, once its rooms are.
    app.switch_to(b);
    app.save_layout();
    let (mut next, _, b) = start();
    assert!(next.split.is_none() && next.pending_split.is_some());
    next.restore_split();
    assert_eq!(next.split.as_ref().map(|s| s.panes.len()), Some(2));
    assert_eq!(next.active, b);
    assert!(next.layout.layouts.contains_key("reading"));
    next.cmd_layout("delete reading").unwrap();
    assert!(next.layout.layouts.is_empty());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn clicks_find_wrapped_links_and_select_messages() {
    let mut app = test_app();
    let a = room_window(&mut app, "!a:x.org", "alpha");
    app.switch_to(a);
    let url = "https://en.wikipedia.org/wiki/Rust_(programming_language)";
    let mut line = msg("$1", &format!("see {url}."));
    line.sender = Some("@alice:x.org".try_into().unwrap());
    app.windows[a].lines.push_back(line);
    let event: OwnedEventId = "$1".try_into().unwrap();
    // The second half of a link wrapped onto the next row still opens it.
    assert_eq!(app.link_in(&event, "wiki/Rust_(programming_language)").as_deref(), Some(url));
    assert_eq!(app.link_in(&event, "(https://en.wikipedia"), Some(url.to_owned()));
    assert_eq!(app.link_in(&event, "alice"), None, "underlined text that isn't a link");

    let row = ui::RowHit { event: Some(event.clone()), links: Vec::new(), image: None, nick: None };
    let area = ratatui::layout::Rect::new(0, 0, 40, 5);
    app.hits.panes.push(ui::PaneHits { area, window: a, rows: vec![ui::RowHit::default(), row] });
    app.on_click(Position::new(10, 1));
    assert_eq!(app.select.as_ref(), Some(&event), "a click selects the message");
    app.on_click(Position::new(10, 1));
    assert_eq!(app.select, None, "clicking it again unselects");
    app.on_click(Position::new(10, 1));
    app.on_click(Position::new(10, 0));
    assert_eq!(app.select, None, "an empty row unselects");

    app.on_wheel(Position::new(10, 2), true);
    assert_eq!(app.windows[a].scroll, WHEEL_ROWS);
    app.on_wheel(Position::new(10, 2), false);
    assert_eq!(app.windows[a].scroll, 0);
    // Every row of a wrapped link becomes a hyperlink to the whole of it.
    let pieces = [(30, 40, "https://en.wikipedia".to_owned()), (2, 34, "wiki/Rust_(programming_language)".to_owned())];
    let rows = vec![
        ui::RowHit { event: Some(event.clone()), links: vec![pieces[0].clone()], image: None, nick: None },
        ui::RowHit { event: Some(event.clone()), links: vec![pieces[1].clone()], image: None, nick: None },
    ];
    app.hits.panes = vec![ui::PaneHits { area: ratatui::layout::Rect::new(0, 3, 40, 2), window: a, rows }];
    let links = app.screen_links();
    assert_eq!(links.iter().map(|l| (l.x, l.y, l.width)).collect::<Vec<_>>(), [(30, 3, 10), (2, 4, 32)]);
    assert!(links.iter().all(|l| l.url == url));
}

#[test]
fn notification_modes_quiet_or_raise_alerts() {
    let mut app = test_app();
    let a = room_window(&mut app, "!a:x.org", "alpha");
    let room: OwnedRoomId = "!a:x.org".try_into().unwrap();
    let incoming = |id: &str| {
        let mut line = msg(id, "hello");
        line.sender = Some("@bob:x.org".try_into().unwrap());
        vec![Item::Line(line)]
    };
    let set_mode = |app: &mut App, mode| {
        app.push_modes.insert("t".into(), HashMap::from([(room.clone(), mode)]));
        (app.windows[a].activity, app.bell) = (Activity::None, false);
    };

    app.on_timeline("t", &room, incoming("$1"), true);
    assert_eq!(app.windows[a].activity, Activity::Message);
    assert!(!app.bell, "ordinary messages don't alert by default");

    set_mode(&mut app, RoomNotificationMode::MentionsAndKeywordsOnly);
    app.on_timeline("t", &room, incoming("$2"), true);
    assert_eq!(app.windows[a].activity, Activity::Event, "shown quietly");

    set_mode(&mut app, RoomNotificationMode::Mute);
    let mut mention = incoming("$3");
    if let Item::Line(line) = &mut mention[0] {
        line.highlight = true;
    }
    app.on_timeline("t", &room, mention, true);
    assert_eq!(app.windows[a].activity, Activity::Event);
    assert!(!app.bell, "muted rooms never alert");

    set_mode(&mut app, RoomNotificationMode::AllMessages);
    app.on_timeline("t", &room, incoming("$4"), true);
    assert_eq!(app.windows[a].activity, Activity::Message);
    assert!(app.bell, "every message alerts in a room set to all");
}

#[test]
fn aliases_expand_like_irssi() {
    assert_eq!(expand_alias("/window goto Team Tests", ""), ["window goto Team Tests"]);
    assert_eq!(expand_alias("/msg @bob:x.org", "hi there"), ["msg @bob:x.org hi there"], "args appended");
    assert_eq!(expand_alias("/msg $0 says $1-", "@a:x b c d"), ["msg @a:x says b c d"]);
    assert_eq!(expand_alias("/me waves at $*, costs $$5", "bob"), ["me waves at bob, costs $5"]);
    assert_eq!(expand_alias("/markread all; /window 1", ""), ["markread all", "window 1"]);
    assert_eq!(expand_alias("/say a\\;b", ""), ["say a;b"]);
    assert_eq!(expand_alias("/say $3", "a b"), ["say"], "missing arguments are empty");
}

#[test]
fn aliases_run_save_and_refuse_loops() {
    let mut app = test_app();
    let dir = std::env::temp_dir().join(format!("textrix-alias-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    app.paths.base = dir.clone();
    std::fs::write(app.paths.config_file(), "# my settings\nbell = false\n").unwrap();
    app.cmd_alias("tt /theme mono").unwrap();
    let saved = std::fs::read_to_string(app.paths.config_file()).unwrap();
    assert!(saved.starts_with("# my settings\nbell = false\n"), "the rest of the file is kept: {saved}");
    assert!(saved.contains("[aliases]") && saved.contains("tt = \"/theme mono\""), "{saved}");
    assert!(app.cmd_alias("join /say no").is_err(), "built-ins can't be replaced");
    for taken in ["read /say no", "fwd /say no", "wc /say no", "shrug /say no"] {
        assert!(app.cmd_alias(taken).is_err(), "{taken}: other names for built-ins neither");
    }

    app.command("tt");
    assert_eq!(app.cfg.theme, crate::theme::ThemeName::Mono.into(), "the alias ran");
    app.cfg.aliases.insert("loop".into(), "/loop".into());
    app.command("loop");
    assert!(app.windows[0].lines.iter().any(|l| l.body.contains("too deeply")));

    app.cmd_unalias("tt").unwrap();
    let saved = std::fs::read_to_string(app.paths.config_file()).unwrap();
    assert_eq!(saved, "# my settings\nbell = false\n", "no empty [aliases] left behind");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn mentions_collect_in_their_own_window_and_jump_back() {
    let mut app = test_app();
    let a = room_window(&mut app, "!a:x.org", "alpha");
    let room: OwnedRoomId = "!a:x.org".try_into().unwrap();
    let mention = |id: &str, mins: i64| {
        let mut line = msg(id, "hey you");
        line.highlight = true;
        line.ts = Local::now() - chrono::Duration::minutes(mins);
        Item::Line(line)
    };
    app.on_timeline("t", &room, vec![Item::Line(msg("$plain", "hi"))], true);
    assert!(!app.windows.iter().any(|w| w.is_mentions), "no window until something mentions you");
    app.on_timeline("t", &room, vec![mention("$new", 0)], true);
    let m = app.windows.iter().position(|w| w.is_mentions).unwrap();
    assert_eq!(m, app.windows.len() - 1, "added at the end, so numbers don't shift");
    assert_eq!(app.windows[m].activity, Activity::Highlight);
    assert_eq!(ui::sidebar_entries(&app)[1], ui::SidebarEntry::Window(m), "listed under status");

    // Older mentions found in history slot in before it; repeats are ignored.
    let history = vec![mention("$old", 60), mention("$new", 0)];
    app.on_event(AppEvent::History {
        account: "t".into(),
        room_id: room.clone(),
        items: history,
        end: None,
        failed: false,
    });
    let ids: Vec<&str> = app.windows[m].lines.iter().filter_map(|l| l.event_id.as_ref().map(|e| e.as_str())).collect();
    assert_eq!(ids, ["$old", "$new"]);
    assert!(app.windows[m].lines.back().unwrap().nick.starts_with("alpha/"));

    app.switch_to(m);
    app.select = Some("$old".try_into().unwrap());
    app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.active, a, "Enter goes to the room");
    assert_eq!(app.select.as_ref().map(|e| e.as_str()), Some("$old"), "with the message selected");
}

#[test]
fn link_titles_show_under_their_message() {
    let mut app = test_app();
    let a = room_window(&mut app, "!a:x.org", "alpha");
    app.windows[a].lines.push_back(msg("$1", "docs at https://matrix.org/docs today"));
    app.switch_to(a);
    let draw = |app: &mut App| {
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 20)).unwrap();
        terminal.draw(|f| ui::draw(f, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| (0..buffer.area.width).map(|x| buffer[(x, y)].symbol().to_owned()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    };
    draw(&mut app);
    assert_eq!(app.link_title_requests, [("t".to_owned(), "https://matrix.org/docs".to_owned())], "asked once");
    app.on_event(AppEvent::LinkTitle {
        url: "https://matrix.org/docs".into(),
        title: Some("Docs — Matrix".into()),
        picture: None,
    });
    assert!(draw(&mut app).contains("↳ Docs — Matrix"));

    app.windows[a].encrypted = true;
    assert!(!draw(&mut app).contains("↳ Docs"), "not in encrypted rooms unless link_titles = \"all\"");
}

#[test]
fn a_links_picture_sits_under_its_title() {
    let mut app = test_app();
    let painter = Painter {
        picker: ratatui_image::picker::Picker::halfblocks(),
        text: crate::picture::TextMode::Sextants,
        truecolor: true,
    };
    app.picker = Some(painter.clone());
    let a = room_window(&mut app, "!a:x.org", "alpha");
    app.windows[a].lines.push_back(msg("$1", "docs at https://matrix.org/docs today"));
    app.windows[a].lines.push_back(msg("$2", "thanks"));
    app.switch_to(a);
    let draw = |app: &mut App| {
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 20)).unwrap();
        terminal.draw(|f| ui::draw(f, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let rows = (0..buffer.area.height)
            .map(|y| (0..buffer.area.width).map(|x| buffer[(x, y)].symbol().to_owned()).collect::<String>())
            .collect::<Vec<_>>();
        (rows, buffer)
    };
    draw(&mut app);
    let red = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(64, 32, image::Rgba([200, 0, 0, 255])));
    let picture = painter.fit(red, ratatui::layout::Size::new(24, 6)).unwrap();
    let tall = picture.size().height as usize;
    app.on_event(AppEvent::LinkTitle {
        url: "https://matrix.org/docs".into(),
        title: Some("Docs — Matrix".into()),
        picture: Some(picture),
    });
    let (rows, buffer) = draw(&mut app);
    let title = rows.iter().position(|r| r.contains("↳ Docs — Matrix")).expect("the title");
    let next = rows.iter().position(|r| r.contains("thanks")).expect("the next message");
    assert_eq!(next, title + 1 + tall, "the picture's rows between the title and the next message");
    // Lined up with the title, under it.
    let x = rows[title].chars().position(|c| c == '↳').unwrap() as u16;
    assert_eq!(buffer[(x, title as u16 + 1)].bg, ratatui::style::Color::Rgb(200, 0, 0));

    app.cfg.link_images = false;
    let (rows, _) = draw(&mut app);
    let title = rows.iter().position(|r| r.contains("↳ Docs — Matrix")).unwrap();
    assert!(rows[title + 1].contains("thanks"), "no room kept without pictures");
}

#[test]
fn only_a_click_on_the_picture_opens_it() {
    let mut app = test_app();
    let painter = Painter {
        picker: ratatui_image::picker::Picker::halfblocks(),
        text: crate::picture::TextMode::Sextants,
        truecolor: true,
    };
    app.picker = Some(painter.clone());
    let a = room_window(&mut app, "!a:x.org", "alpha");
    app.windows[a].lines.push_back(attachment("$img", MediaKind::Image, "sunset.png"));
    app.switch_to(a);
    let red = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(64, 32, image::Rgba([200, 0, 0, 255])));
    let picture = painter.fit(red, ratatui::layout::Size::new(24, 6)).unwrap();
    app.previews.insert("$img".try_into().unwrap(), Preview::Ready(picture));
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 20)).unwrap();
    terminal.draw(|f| ui::draw(f, &mut app)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    let (x, y) = (0..20u16)
        .flat_map(|y| (0..100u16).map(move |x| (x, y)))
        .find(|&(x, y)| buffer[(x, y)].bg == ratatui::style::Color::Rgb(200, 0, 0))
        .expect("the picture");
    // The viewer is asked for (it then needs the server, which a test hasn't).
    let asked =
        |app: &App| app.windows.iter().flat_map(|w| &w.lines).filter(|l| l.body.contains("isn't connected")).count();
    app.on_click(Position::new(x + 1, y));
    assert_eq!(asked(&app), 1, "a click on the picture opens it");
    // Beside it, on the same row: its message, like a click on its text.
    app.on_click(Position::new(x + 40, y));
    assert_eq!(asked(&app), 1, "not the picture");
    assert_eq!(app.select.as_deref().map(|e| e.as_str()), Some("$img"), "the message is selected");
}

#[test]
fn writing_to_someone_away_says_so_when_its_news() {
    let mut app = test_app();
    let sam: OwnedUserId = "@sam:x.org".try_into().unwrap();
    let key = WinKey { account: "t".into(), room_id: "!dm:x.org".try_into().unwrap(), thread: None };
    let mut dm = Window::new(Some(key), "sam".into());
    (dm.is_dm, dm.dm_user) = (true, Some(sam.clone()));
    app.windows.push(dm);
    app.switch_to(1);
    assert!(!app.cfg.away_notes, "off unless you turn it on");
    app.cfg.away_notes = true;
    let away = |status: &str| Presence { kind: PresenceKind::Away, status: Some(status.into()), last_active: None };
    let set = |app: &mut App, p: Presence| {
        app.on_event(AppEvent::Presence { account: "t".into(), updates: vec![(sam.clone(), p)] })
    };
    let notes = |app: &App| app.windows[1].lines.iter().filter(|l| l.body.contains(" is ")).count();
    let ago = |mins| Some(Local::now() - chrono::TimeDelta::minutes(mins));
    let presence =
        |kind, status: Option<&str>, last_active| Presence { kind, status: status.map(Into::into), last_active };

    app.note_away(None);
    assert_eq!(notes(&app), 0, "nothing known, nothing said");
    set(&mut app, presence(PresenceKind::Offline, None, None));
    app.note_away(None);
    set(&mut app, presence(PresenceKind::Offline, None, ago(3)));
    app.note_away(None);
    set(&mut app, presence(PresenceKind::Away, None, ago(5)));
    app.note_away(None);
    assert_eq!(notes(&app), 0, "hidden presence, a missed heartbeat, a few idle minutes: not news");

    set(&mut app, away("lunch"));
    app.note_away(None);
    app.note_away(None);
    assert_eq!(app.windows[1].lines.back().unwrap().body, "sam is away: lunch");
    assert_eq!(notes(&app), 1, "once, not every message");
    set(&mut app, away("in a meeting"));
    app.note_away(None);
    assert_eq!(notes(&app), 2, "a new status is news");
    set(&mut app, presence(PresenceKind::Online, None, None));
    set(&mut app, away("in a meeting"));
    app.note_away(None);
    assert_eq!(notes(&app), 2, "a blip online doesn't make it news again");

    // An hour on, long gone is worth saying again.
    app.away_noted.get_mut(&sam).unwrap().1 = Instant::now().checked_sub(AWAY_REPEAT).unwrap();
    set(&mut app, presence(PresenceKind::Offline, None, ago(180)));
    app.note_away(None);
    assert_eq!(app.windows[1].lines.back().unwrap().body, "sam is offline, last seen 3h ago");

    // Someone who's just been talking is around, whatever it says.
    app.away_noted.clear();
    let mut said = Line::new(LineKind::Message, "back");
    (said.sender, said.ts) = (Some(sam.clone()), Local::now() - chrono::TimeDelta::minutes(2));
    app.windows[1].lines.push_back(said);
    app.note_away(None);
    assert_eq!(notes(&app), 3);

    app.windows[1].lines.pop_back();
    app.cfg.away_notes = false;
    app.note_away(None);
    assert_eq!(notes(&app), 3, "away_notes = false: never");
}

#[test]
fn nicknames_show_only_here() {
    let mut app = test_app();
    let dir = std::env::temp_dir().join(format!("textrix-nicknames-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    app.paths.base = dir.clone();
    let sam: OwnedUserId = "@sam:x.org".try_into().unwrap();
    let key = WinKey { account: "t".into(), room_id: "!dm:x.org".try_into().unwrap(), thread: None };
    let mut dm = Window::new(Some(key), "sam".into());
    (dm.is_dm, dm.dm_user) = (true, Some(sam.clone()));
    let mut line = msg("$1", "hi");
    (line.nick, line.sender) = ("sam".into(), Some(sam.clone()));
    dm.lines.push_back(line);
    app.windows.push(dm);
    app.switch_to(1);

    app.cmd_nickname("@sam:x.org Bear").unwrap();
    assert_eq!(app.windows[1].title(), "Bear", "the DM is named after them");
    assert_eq!(app.display_name(Some(&sam), "sam"), "Bear");
    let drawn = ui::render_line_with_names(&app.windows[1].lines[0], 80, &app.cfg, app.nicknames());
    assert!(drawn.contains("<Bear>"), "{drawn}");

    app.input.set_text("Be");
    let candidates = app.candidates();
    assert_eq!(candidates[0].text, "sam: ", "completion inserts their real name");
    assert!(std::fs::read_to_string(app.paths.layout_file()).unwrap().contains("Bear"));

    app.cmd_nickname("@sam:x.org").unwrap();
    assert_eq!(app.windows[1].title(), "sam");
    assert!(app.cmd_nickname("@sam:x.org").is_err(), "nothing to take off");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn reload_skips_settings_it_does_not_know_and_says_so() {
    let mut app = test_app();
    let dir = std::env::temp_dir().join(format!("textrix-reload-unknown-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    app.paths.base = dir.clone();
    std::fs::write(app.paths.config_file(), "theme = \"disco\"\n\n[future_thing]\non = true\n").unwrap();
    app.cmd_reload();
    assert_eq!(app.cfg.theme.name(), "disco", "the rest is applied");
    let said: Vec<&str> = app.windows[0].lines.iter().map(|l| l.body.as_str()).collect();
    assert!(said.iter().any(|b| b.contains("skipped future_thing, which this Textrix doesn't know")), "{said:?}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn script_settings_go_through_set_and_save() {
    let mut app = test_app();
    let dir = std::env::temp_dir().join(format!("textrix-script-set-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    app.paths.base = dir.clone();
    std::fs::write(app.paths.config_file(), "# mine\n\n[script_settings.watch]\nloud = true\n").unwrap();
    app.cfg = Config::load(&app.paths.config_file()).unwrap();
    app.scripts.configure(&app.cfg.script_settings);
    let source = r#"setting("words", "deploy", "Words to watch for"); setting("loud", false, "Ring too");"#;
    app.scripts.load("watch", None, source).unwrap();

    app.cmd_set("").unwrap();
    let listed: Vec<&str> = app.windows[0].lines.iter().map(|l| l.body.as_str()).collect();
    assert!(listed.contains(&"  watch.words = \"deploy\"  (Words to watch for)"), "{listed:#?}");
    assert!(listed.contains(&"  watch.loud = true  (Ring too)"), "config.toml's value");
    assert!(!listed.iter().any(|l| l.contains("[script_settings]")), "shown as settings, not a table");
    app.cmd_set("watch.words pizza|outage").unwrap();
    assert!(app.cmd_set("watch.loud maybe").unwrap_err().contains("takes boolean"));
    app.cmd_set("image_rows 12").unwrap();
    assert_eq!(app.scripts.settings()[0].1, toml::Value::String("pizza|outage".into()), "other /sets keep it");

    app.cmd_save().unwrap();
    let saved = std::fs::read_to_string(app.paths.config_file()).unwrap();
    assert!(saved.contains("# mine\n") && saved.contains("image_rows = 12"), "{saved}");
    assert!(saved.contains("[script_settings.watch]\nloud = true\nwords = \"pizza|outage\"\n"), "{saved}");
    let back = Config::load(&app.paths.config_file()).unwrap();
    assert_eq!(back.script_settings["watch"]["words"], toml::Value::String("pizza|outage".into()));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn set_and_save_keep_what_you_changed() {
    let mut app = test_app();
    let dir = std::env::temp_dir().join(format!("textrix-set-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    app.paths.base = dir.clone();
    std::fs::write(app.paths.config_file(), "# my config\nmarkdown = false\n").unwrap();
    app.cfg = Config::load(&app.paths.config_file()).unwrap();

    app.cmd_set("image_rows 14").unwrap();
    assert_eq!(app.cfg.image_rows, 14);
    app.cmd_set("bell away").unwrap();
    assert_eq!(app.bell_mode, crate::config::BellMode::Away, "words are taken as text");
    assert!(app.cmd_set("image_rows lots").is_err(), "checked like config.toml");
    assert!(app.cmd_set("belll true").is_err(), "unknown settings are errors");
    assert!(app.cmd_set("aliases x").is_err(), "tables are edited in the file");
    app.cmd_theme("blue-deep").unwrap();
    app.on_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));

    app.cmd_save().unwrap();
    let saved = std::fs::read_to_string(app.paths.config_file()).unwrap();
    assert!(saved.starts_with("# my config\nmarkdown = false\n"), "{saved}");
    for line in ["image_rows = 14", "bell = \"away\"", "theme = \"blue-deep\"", "sidebar = false"] {
        assert!(saved.contains(line), "{line} missing from:\n{saved}");
    }
    let back = Config::load(&app.paths.config_file()).unwrap();
    assert!(!back.markdown && back.image_rows == 14 && !back.sidebar);
    app.cmd_save().unwrap();
    assert!(app.windows[0].lines.back().unwrap().body.contains("already has these settings"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn dropped_connections_are_netsplits() {
    let mut app = test_app();
    for (room, name) in [("!a:x.org", "Night Owls"), ("!b:x.org", "Book Club")] {
        room_window(&mut app, room, name);
    }
    app.switch_to(1);
    let error = || AppEvent::SyncError { account: "t".into(), error: "connection refused".into() };
    app.on_event(error());
    app.on_event(error());
    let status: Vec<&str> = app.windows[0].lines.iter().map(|l| l.body.as_str()).collect();
    let splits: Vec<&&str> = status.iter().filter(|b| b.starts_with("Netsplit t ⇄ you")).collect();
    assert_eq!(splits.len(), 1, "once per split: {status:?}");
    assert!(splits[0].contains("quits: Night Owls, Book Club"), "{}", splits[0]);
    app.on_event(AppEvent::Synced { account: "t".into(), first: false });
    let over = &app.windows[0].lines.back().unwrap().body;
    assert!(over.starts_with("Netsplit over after 0s, joins: Night Owls, Book Club"), "{over}");
    assert!(!app.windows[1].lines.iter().any(|l| l.body.contains("Netsplit")), "not in the room you're in");
    assert_eq!(short_duration(Duration::from_secs(133)), "2m 13s");
    assert_eq!(short_duration(Duration::from_secs(3900)), "1h 5m");
}

/// Account `t`, @me:example.org, that hasn't reached its server yet.
fn offline_account(app: &mut App) {
    let def =
        ServerDef { name: "t".into(), homeserver: Some("https://example.org".into()), user: "@me:example.org".into() };
    let state = AccountState {
        user: def.user.clone(),
        def,
        nick: None,
        status: ConnStatus::Syncing,
        verified: None,
        away: None,
        account: None,
    };
    app.accounts.insert("t".into(), state);
}

#[test]
fn restarting_offline_shows_what_still_waits() {
    let mut app = test_app();
    offline_account(&mut app);
    let a = room_window(&mut app, "!a:x.org", "Night Owls");
    let room_id: OwnedRoomId = "!a:x.org".try_into().unwrap();
    let waiting = |txn: &str, body: &str| {
        let mut line = Line::new(LineKind::Message, body);
        (line.own, line.pending, line.waiting) = (true, true, true);
        (line.txn_id, line.sender) = (Some(txn.into()), Some("@me:example.org".try_into().unwrap()));
        line
    };
    let unsent = |lines: Vec<Line>| AppEvent::Unsent { account: "t".into(), room_id: room_id.clone(), lines };
    let sent = |txn: &str, event_id: &str| AppEvent::Sent {
        account: "t".into(),
        room_id: room_id.clone(),
        txn_id: txn.into(),
        event_id: event_id.try_into().unwrap(),
    };
    let line = |app: &App, body: &str| app.windows[a].lines.iter().find(|l| l.body == body).unwrap().clone();

    // Written offline, then Textrix quit: back as waiting, under your name.
    app.on_event(unsent(vec![waiting("q1", "from the train")]));
    app.switch_to(a);
    let rows = screen(&mut app, 120, 20).join("\n");
    assert!(rows.contains("<me>") && rows.contains("from the train (waiting to send)"), "{rows}");
    app.on_event(unsent(vec![waiting("q1", "from the train")]));
    assert_eq!(app.windows[a].lines.iter().filter(|l| l.body == "from the train").count(), 1, "shown once");

    // It goes when the server can be reached.
    app.on_event(sent("q1", "$one"));
    let gone = line(&app, "from the train");
    assert!(!gone.pending && !gone.waiting && gone.event_id.is_some());

    // Sent while Textrix was starting, before it was shown: shown as sent.
    app.on_event(sent("q2", "$two"));
    app.on_event(unsent(vec![waiting("q2", "and another")]));
    let early = line(&app, "and another");
    assert!(!early.waiting && early.event_id.as_ref().map(|e| e.as_str()) == Some("$two"));
}

#[test]
fn starting_offline_reads_whats_saved_and_queues_what_you_send() {
    let mut app = test_app();
    offline_account(&mut app);
    let a = room_window(&mut app, "!a:x.org", "Night Owls");
    let room_id: OwnedRoomId = "!a:x.org".try_into().unwrap();
    app.on_event(AppEvent::SyncError { account: "t".into(), error: "connection refused".into() });
    let said = &app.windows[0].lines.back().unwrap().body;
    assert!(said.starts_with("can't reach example.org") && said.contains("what you send waits"), "{said}");

    // A message waits in the send queue, under the queue's own name.
    let mut line = msg("$x", "on the train");
    (line.event_id, line.pending, line.txn_id) = (None, true, Some("ours".into()));
    app.windows[a].lines.push_back(line);
    let queued = |t: &str| AppEvent::Queued {
        account: "t".into(),
        room_id: room_id.clone(),
        txn_id: "ours".into(),
        queued: t.into(),
    };
    app.on_event(queued("q1"));
    app.on_event(AppEvent::SendWaiting { account: "t".into(), room_id: room_id.clone(), txn_id: "q1".into() });
    let last = |app: &App| app.windows[a].lines.back().unwrap().clone();
    assert!(last(&app).body.contains("goes as soon as it can"), "said once, in the room");
    app.switch_to(a);
    let rows = screen(&mut app, 120, 20).join("\n");
    assert!(rows.contains("on the train (waiting to send)"), "{rows}");

    // Back online: it goes.
    app.on_event(AppEvent::Synced { account: "t".into(), first: true });
    assert!(app.windows[0].lines.back().unwrap().body.contains("reached after"));
    let sent = AppEvent::Sent {
        account: "t".into(),
        room_id: room_id.clone(),
        txn_id: "q1".into(),
        event_id: "$sent".try_into().unwrap(),
    };
    app.on_event(sent);
    let line = app.windows[a].lines.iter().find(|l| l.body == "on the train").unwrap();
    assert!(!line.pending && !line.waiting && line.event_id.as_ref().map(|e| e.as_str()) == Some("$sent"));

    // Sent before the queue's name for it arrived: put right when it does.
    let mut line = msg("$y", "quick one");
    (line.event_id, line.pending, line.txn_id) = (None, true, Some("ours".into()));
    app.windows[a].lines.push_back(line);
    let early = AppEvent::Sent {
        account: "t".into(),
        room_id: room_id.clone(),
        txn_id: "q2".into(),
        event_id: "$early".try_into().unwrap(),
    };
    app.on_event(early);
    app.on_event(queued("q2"));
    let line = app.windows[a].lines.iter().find(|l| l.body == "quick one").unwrap();
    assert!(!line.pending && line.event_id.as_ref().map(|e| e.as_str()) == Some("$early"));
}

#[test]
fn scripts_add_commands_and_react_to_messages() {
    let mut app = test_app();
    let a = room_window(&mut app, "!a:x.org", "Night Owls");
    app.switch_to(a);
    let source = r#"
        command("shout", "Say it louder", |args, ctx| print(args.to_upper() + " in " + ctx.room));
        command("join", "Can't replace built-ins", |args, ctx| print("hijacked"));
        on_message(|msg| if msg.body.contains("ping") { print("pong for " + msg.nick); bell(); });
    "#;
    app.scripts.load("demo", None, source).unwrap();
    app.command("shout hello");
    assert_eq!(app.windows[a].lines.back().unwrap().body, "HELLO in Night Owls");
    app.command("join #x:y.org");
    assert!(!app.windows[a].lines.iter().any(|l| l.body == "hijacked"), "built-ins win");

    let room: OwnedRoomId = "!a:x.org".try_into().unwrap();
    let mut line = msg("$9", "anyone? ping");
    (line.nick, line.sender) = ("sam".into(), Some("@sam:x.org".try_into().unwrap()));
    app.on_timeline("t", &room, vec![Item::Line(line)], true);
    assert_eq!(app.windows[a].lines.back().unwrap().body, "pong for sam");
    assert!(app.bell);

    let mut own = msg("$10", "ping myself");
    own.own = true;
    app.on_timeline("t", &room, vec![Item::Line(own)], true);
    assert_ne!(app.windows[a].lines.back().unwrap().body, "pong for sam", "scripts don't hear your own messages");

    // Joins and reactions from the timeline reach hooks too (not your own).
    app.scripts
        .load("hooks", None, r#"on_join(|m| print("hi " + m.nick)); on_reaction(|r| print(r.nick + r.key + r.text));"#)
        .unwrap();
    let mut join = Line::new(LineKind::Event, "sam joined");
    (join.event_id, join.sender) = (Some("$j".try_into().unwrap()), Some("@sam:x.org".try_into().unwrap()));
    join.fold = Some(Fold::Joined("sam".into()));
    let react = |own| Item::React { target: "$9".try_into().unwrap(), key: "🍕".into(), nick: "sam".into(), own };
    app.on_timeline("t", &room, vec![Item::Line(join), react(true), react(false)], true);
    let said: Vec<&str> = app.windows[a].lines.iter().rev().take(3).map(|l| l.body.as_str()).collect();
    assert!(said.contains(&"hi sam") && said.contains(&"sam🍕anyone? ping"), "{said:?}");
    assert_eq!(app.windows[a].lines.iter().filter(|l| l.body.starts_with("sam🍕")).count(), 1, "not your own");

    app.input.set_text("/sho");
    assert_eq!(app.candidates()[0].text, "/shout ");
    app.input.set_text("/shout");
    assert!(app.input_hint().unwrap().contains("Say it louder (demo.rhai)"));
    app.cmd_script("exec 6 * 7").unwrap();
    assert_eq!(app.windows[a].lines.back().unwrap().body, "42");
    app.cmd_script(r#"exec send("Night", "hi")"#).unwrap();
    let last = &app.windows[a].lines.back().unwrap().body;
    assert!(last.contains("send to Night, which isn't a room here"), "send() needs a whole name: {last}");
}

#[test]
fn play_finds_the_latest_audio() {
    let mut app = test_app();
    let a = room_window(&mut app, "!a:x.org", "Night Owls");
    app.switch_to(a);
    assert_eq!(app.cmd_play(""), Err("no audio or video here".into()));
    assert_eq!(app.cmd_play("stop"), Err("nothing is playing".into()));
    let mut voice = msg("$v", "[voice] 0:03 ▁▄█");
    voice.nick = "sam".into();
    voice.media = Some(Media {
        source: matrix_sdk::ruma::events::room::MediaSource::Plain("mxc://x/v".into()),
        filename: "Voice message.ogg".into(),
        kind: MediaKind::Audio,
        mimetype: Some("audio/ogg".into()),
        thumbnail: None,
        size: None,
        animated: None,
    });
    app.windows[a].push(voice, 100);
    assert_eq!(app.cmd_play("jo_bloggs"), Err("no audio or video from jo_bloggs here".into()));
    // Found; with no server connected here it stops short of playing.
    assert!(app.cmd_play("sam").unwrap_err().contains("not connected"), "found sam's");
}

#[test]
fn restart_comes_back_where_you_were() {
    let dir = std::env::temp_dir().join(format!("textrix-restart-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let paths = Paths { base: dir.clone(), env_file: dir.join("none") };
    let start = || {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app =
            App::new(Config::default(), paths.clone(), EnvFile::new(paths.env_file.clone()), Servers::default(), tx);
        let a = room_window(&mut app, "!a:x.org", "Night Owls");
        (app, a)
    };
    let (mut app, a) = start();
    assert!(app.cmd_restart().is_err(), "nowhere to restart from");
    app.install = crate::install::Install::at(std::env::current_exe().unwrap());
    app.switch_to(a);
    app.input.set_text("half a thought");
    app.switch_to(0);
    app.input.set_text("status draft");
    app.switch_to(a);
    app.cmd_restart().unwrap();
    assert!(app.restart && app.quit);

    let (mut next, a) = start();
    assert!(next.pending_resume.is_some());
    next.restore_split();
    assert_eq!((next.active, next.input.text()), (a, "half a thought".to_owned()), "back, draft and all");
    next.switch_to(0);
    assert_eq!(next.input.text(), "status draft", "and the other windows' drafts");
    assert!(next.pending_resume.is_none());

    // The status bar's upgrade note.
    assert_eq!(next.upgrade_note(), None);
    next.released = Some("9.9.9".into());
    assert_eq!(next.upgrade_note().as_deref(), Some("9.9.9 is out"));
    next.on_event(AppEvent::Installed { version: Some("9.9.9".into()) });
    assert_eq!(next.upgrade_note().as_deref(), Some("9.9.9 installed: /restart"));
    next.on_event(AppEvent::Installed { version: Some(env!("CARGO_PKG_VERSION").into()) });
    assert_eq!(next.upgrade_note().as_deref(), Some("new build: /restart"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn ctrl_c_twice_quits() {
    let mut app = test_app();
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    app.input.set_text("a draft");
    app.on_key(ctrl_c);
    assert!(app.input.is_empty() && app.quit_prompt().is_none(), "with text, it clears the line");
    app.on_key(ctrl_c);
    assert_eq!(app.quit_prompt(), Some("press Ctrl+C again to quit"));
    app.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    assert!(app.quit_prompt().is_none() && !app.quit, "any other key calls it off");
    app.on_key(ctrl_c);
    app.quit_armed = app.quit_armed.map(|at| at - Duration::from_secs(3));
    app.on_key(ctrl_c);
    assert!(!app.quit, "too slow: that one only asks again");
    app.on_key(ctrl_c);
    assert!(app.quit);
}

#[test]
fn easter_eggs() {
    let mut app = test_app();
    for code in eggs::KONAMI {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }
    assert_eq!(app.cfg.theme, crate::theme::ThemeName::Green.into());
    assert!(app.input.is_empty(), "the b doesn't stay in the input line");
    for code in eggs::KONAMI {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }
    assert_eq!(app.cfg.theme, crate::theme::ThemeName::Default.into(), "again turns it off");

    app.command("8ball will it ship");
    assert!(app.windows[0].lines.back().unwrap().body.contains("The 8-ball says:"));
    app.command("bofh");
    assert!(app.windows[0].lines.back().unwrap().body.starts_with("Today's excuse:"));
    app.command("slap");
    assert!(app.windows[0].lines.back().unwrap().body.contains("slap whom"));

    app.input.set_text("Dear team,");
    assert!(app.input_hint().unwrap().contains("writing a letter"));
    app.input.set_text("/slap bob");
    assert!(app.input_hint().is_none(), "no 'unknown command' spoiler");
    assert!(!COMMANDS.iter().any(|c| EGGS.contains(&c.name)), "not in /help");
}

#[test]
fn windows_can_have_your_own_names() {
    let mut app = test_app();
    let dir = std::env::temp_dir().join(format!("textrix-names-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    app.paths.base = dir.clone();
    let a = room_window(&mut app, "!a:x.org", "Night Owls");
    app.switch_to(a);
    app.cmd_window("name Ops").unwrap();
    assert_eq!(app.windows[a].title(), "Ops");
    assert_eq!(app.find_window("ops"), Ok(a));
    assert_eq!(app.find_window("night"), Ok(a), "the room's own name still finds it");
    assert_eq!(app.terminal_title().unwrap(), format!("Ops — Textrix v{}", env!("CARGO_PKG_VERSION")));
    app.cfg.title = "{number}:{window} ({version})".into();
    assert_eq!(app.terminal_title().unwrap(), format!("{}:Ops ({})", a + 1, env!("CARGO_PKG_VERSION")));

    // Kept in the layout, so it comes back with the window.
    let key = app.windows[a].room.clone().unwrap();
    app.remove_window(a);
    let idx = app.insert_window(Window::new(Some(key), "Night Owls".into()));
    assert_eq!(app.windows[idx].title(), "Ops");
    assert!(std::fs::read_to_string(app.paths.layout_file()).unwrap().contains("Ops"));

    app.switch_to(idx);
    app.cmd_window("name").unwrap();
    assert_eq!(app.windows[idx].title(), "Night Owls", "back to the room's name");
    app.switch_to(0);
    assert!(app.cmd_window("name Home").is_err(), "the status window keeps its name");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn bells_follow_the_settings_and_dont_pile_up() {
    let mut app = test_app();
    app.ring(BellKind::Mentions);
    assert!(app.bell && app.flash_until.is_none());
    app.bell = false;
    app.ring(BellKind::Invites);
    assert!(!app.bell, "a burst rings once");

    app.last_bell = None;
    app.cfg.bell_for = vec![BellKind::Mentions];
    app.ring(BellKind::Transfers);
    assert!(!app.bell, "not in bell_for");

    app.cmd_bell("away").unwrap();
    app.focused = true;
    app.ring(BellKind::Mentions);
    assert!(!app.bell, "away: quiet while you're looking");
    app.focused = false;
    app.ring(BellKind::Mentions);
    assert!(app.bell);

    (app.bell, app.last_bell) = (false, None);
    app.cmd_bell("visual").unwrap();
    app.ring(BellKind::Mentions);
    assert!(!app.bell && app.flash_until.is_some(), "flashes instead of beeping");
    assert!(app.cmd_bell("loud").is_err());
}

#[test]
fn forwarding_offers_the_other_rooms() {
    let mut app = test_app();
    let a = room_window(&mut app, "!a:x.org", "alpha");
    room_window(&mut app, "!b:x.org", "beta");
    app.switch_to(a);
    assert!(app.cmd_forward("").is_err(), "nothing to forward yet");
    app.windows[a].lines.push_back(msg("$1", "share me"));
    app.cmd_forward("").unwrap();
    let names: Vec<String> = app.switcher_entries().into_iter().map(|e| e.name).collect();
    assert_eq!(names, ["beta"], "not status, mentions or the room it came from");
    app.switcher = None;
    assert!(app.cmd_forward("zzz").is_err());
    assert!(app.switcher.is_none(), "a failed /forward leaves nothing open");
}

#[test]
fn markread_clears_activity_and_moves_the_divider() {
    let mut app = test_app();
    let a = room_window(&mut app, "!a:x.org", "alpha");
    let b = room_window(&mut app, "!b:x.org", "beta");
    for idx in [a, b] {
        app.windows[idx].lines.push_back(msg(&format!("$x{idx}"), "hi"));
        (app.windows[idx].activity, app.windows[idx].unread) = (Activity::Message, (3, 0));
    }
    app.switch_to(a);
    app.cmd_markread("").unwrap();
    assert_eq!((app.windows[a].activity, app.windows[a].unread()), (Activity::None, 0));
    assert_eq!(app.windows[a].marker.as_ref().map(|e| e.as_str()), Some("$x1"));
    assert_eq!(app.windows[b].activity, Activity::Message, "only this room");
    app.cmd_markread("all").unwrap();
    assert_eq!((app.windows[b].activity, app.windows[b].unread()), (Activity::None, 0));
    assert!(app.cmd_markread("everything").is_err());
}

#[test]
fn cached_history_is_dropped_when_the_sync_skipped_messages() {
    let mut app = test_app();
    let a = room_window(&mut app, "!a:x.org", "alpha");
    let room: OwnedRoomId = "!a:x.org".try_into().unwrap();
    let cached = |id: &str| {
        let mut line = msg(id, "from last time");
        line.cached = true;
        Item::Line(line)
    };
    let timeline = |id: &str, limited| AppEvent::Timeline {
        account: "t".into(),
        room_id: room.clone(),
        items: vec![Item::Line(msg(id, "new"))],
        live: true,
        limited,
    };
    app.on_event(AppEvent::Cached {
        account: "t".into(),
        room_id: room.clone(),
        items: vec![cached("$1"), cached("$2")],
    });
    app.on_event(timeline("$3", false));
    assert_eq!(ids(&app.windows[a]), ["$1", "$2", "$3"], "a complete sync carries on from the cache");

    app.on_event(timeline("$9", true));
    assert_eq!(ids(&app.windows[a]), ["$3", "$9"], "a gap means the cached lines no longer join up");
}

#[test]
fn long_pastes_ask_before_sending() {
    let mut app = test_app();
    let key = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    app.on_terminal(Event::Paste("1\r\n2\n3\n4\n5\n6\n7".into()));
    app.on_key(enter);
    assert_eq!(app.confirm_paste, Some(7));
    app.on_key(key('n'));
    assert_eq!((app.confirm_paste, app.input.text().lines().count()), (None, 7), "kept to edit");
    app.on_key(enter);
    app.on_key(key('y'));
    assert!(app.confirm_paste.is_none() && app.input.is_empty(), "sent");

    // Lines you type yourself, and short pastes, just go.
    app.input.set_text("a\nb\nc\nd\ne\nf\ng");
    app.on_key(enter);
    assert!(app.confirm_paste.is_none() && app.input.is_empty());
    app.on_terminal(Event::Paste("one\ntwo".into()));
    app.on_key(enter);
    assert!(app.confirm_paste.is_none() && app.input.is_empty());
}

#[test]
fn unsent_text_stays_with_its_window() {
    let mut app = test_app();
    let a = room_window(&mut app, "!a:x.org", "alpha");
    let b = room_window(&mut app, "!b:x.org", "beta");
    app.switch_to(a);
    app.input.set_text("half a thought");
    let key = app.windows[a].room.clone().unwrap();
    let event: OwnedEventId = "$1".try_into().unwrap();
    let sender: OwnedUserId = "@bob:x.org".try_into().unwrap();
    app.compose = Some((key, Compose::Reply { event, sender, nick: "bob".into() }));

    app.switch_to(b);
    assert!(app.input.is_empty() && app.compose.is_none(), "the other window starts clean");
    assert!(app.windows[a].has_draft());
    app.input.set_text("for beta");
    app.switch_to(a);
    assert_eq!(app.input.text(), "half a thought");
    assert!(app.compose.is_some(), "the reply being written comes back too");
    assert!(!app.windows[a].has_draft());
    app.switch_to(b);
    assert_eq!(app.input.text(), "for beta");
    app.input.clear();
    app.switch_to(a);
    assert!(!app.windows[b].has_draft(), "an empty line leaves nothing behind");
}

#[test]
fn split_panes_follow_focus_and_window_changes() {
    let mut app = test_app();
    let a = room_window(&mut app, "!a:x.org", "alpha");
    let b = room_window(&mut app, "!b:x.org", "beta");
    let c = room_window(&mut app, "!c:x.org", "gamma");
    app.switch_to(a);
    assert!(app.cmd_split("", false).is_err(), "nothing to change yet");
    assert!(app.cmd_split("alpha", false).is_err(), "already on screen");
    app.cmd_split("beta", false).unwrap();
    assert_eq!(app.screen_windows(), [a, b]);
    assert_eq!(app.active, a, "focus stays where you're typing");

    app.cycle_pane(1);
    assert_eq!(app.active, b);
    // Switching to a hidden window swaps it into the focused pane...
    app.switch_to(c);
    assert_eq!(app.screen_windows(), [a, c]);
    // ...and to a visible one just moves the focus.
    app.switch_to(a);
    assert_eq!((app.active, app.screen_windows()), (a, vec![a, c]));

    app.cmd_split("3", true).unwrap();
    assert_eq!(app.screen_windows(), [a, b, c]);
    assert!(app.split.as_ref().unwrap().side_by_side);

    // Closing a shown window drops its pane; panes track identity, not
    // position, so the others keep their windows.
    app.remove_window(b);
    let (a, c) = (a, c - 1);
    assert_eq!(app.screen_windows(), [a, c]);
    app.cmd_unsplit("").unwrap();
    assert!(app.split.is_none(), "one pane left means no split");
    assert_eq!(app.active, c);
    assert!(app.cmd_unsplit("").is_err());
}

#[test]
fn input_hints_guide_command_typing() {
    let mut app = test_app();
    app.input.set_text("/re");
    let hint = app.input_hint().unwrap();
    for name in ["reply", "react", "redact", "recover"] {
        assert!(hint.contains(name), "{hint}");
    }
    app.input.set_text("/reply ");
    assert!(app.input_hint().unwrap().starts_with("[nick] <text>"));
    app.input.set_text("/wc");
    assert!(app.input_hint().unwrap().contains("Manage windows"));
    app.input.set_text("/reply hi there");
    assert!(app.input_hint().is_none(), "no hint once arguments are typed");
    app.input.set_text("hello");
    assert!(app.input_hint().is_none());
}

#[test]
fn edit_mode_walks_your_own_messages_and_restores_the_draft() {
    let mut app = test_app();
    let key = WinKey { account: "t".into(), room_id: "!r:x.org".try_into().unwrap(), thread: None };
    app.windows.push(Window::new(Some(key), "room".into()));
    app.active = 1;
    let mine = |id: &str, body: &str| {
        let mut l = msg(id, body);
        l.own = true;
        l
    };
    let (first, second) = (mine("$1", "first of mine"), mine("$3", "second of mine"));
    app.win_mut().push(first, 100);
    app.win_mut().push(msg("$2", "someone else"), 100);
    app.win_mut().push(second.clone(), 100);
    app.input.set_text("half-typed draft");

    app.load_edit(&second, app.input.text());
    assert_eq!(app.input.text(), "second of mine");
    assert_eq!(app.compose_label().unwrap(), format!("edit {} 1/2", second.ts.format("%H:%M")));

    app.step_edit(true);
    assert_eq!(app.input.text(), "first of mine", "skips other people's messages");
    app.step_edit(false);
    assert_eq!(app.input.text(), "second of mine");
    app.step_edit(false);
    assert!(!app.is_editing(), "down past the newest leaves edit mode");
    assert_eq!(app.input.text(), "half-typed draft");
}

#[test]
fn shift_or_alt_enter_starts_a_new_line() {
    let mut app = test_app();
    app.input.set_text("first");
    app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
    app.input.insert_str("second");
    app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
    assert_eq!(app.input.text(), "first\nsecond\n");
}

#[test]
fn search_spans_rooms_newest_first_and_jumps() {
    let mut app = test_app();
    for (room, id, body, mins_ago) in
        [("!a:x.org", "$old", "deploy the thing", 30), ("!b:x.org", "$new", "Deploy finished", 5)]
    {
        let key = WinKey { account: "t".into(), room_id: room.try_into().unwrap(), thread: None };
        app.windows.push(Window::new(Some(key), room.into()));
        let mut line = msg(id, body);
        line.ts = Local::now() - chrono::TimeDelta::minutes(mins_ago);
        app.windows.last_mut().unwrap().push(line, 100);
        app.windows.last_mut().unwrap().push(msg(&format!("{id}x"), "unrelated"), 100);
    }
    app.cmd_search("deploy").unwrap();
    let hits: Vec<&str> = app.search_hits.iter().map(|(_, e)| e.as_str()).collect();
    assert_eq!(hits, vec!["$new", "$old"]);

    app.cmd_jump("2").unwrap();
    assert_eq!(app.active, 1, "switched to the first room");
    assert_eq!(app.select.as_ref().map(|e| e.as_str()), Some("$old"));
    assert!(app.cmd_jump("9").is_err());

    // Narrowed by room, sender and date.
    let found = |app: &mut App, query: &str| -> Vec<String> {
        app.cmd_search(query).unwrap();
        app.search_hits.iter().map(|(_, e)| e.to_string()).collect()
    };
    assert_eq!(found(&mut app, "deploy in:!b"), ["$new"]);
    assert_eq!(found(&mut app, "from:alice in:!a"), ["$oldx", "$old"], "everything alice said there");
    assert!(found(&mut app, "deploy from:sam").is_empty());
    assert!(found(&mut app, "deploy before:2020").is_empty());
    assert_eq!(found(&mut app, "deploy after:yesterday").len(), 2);
    assert!(app.cmd_search("after:soon").unwrap_err().contains("isn't a date"));
    assert!(app.cmd_search("").unwrap_err().starts_with("usage"));
}

#[test]
fn window_list_groups_rooms_by_space_without_renumbering() {
    use crate::ui::{
        SidebarEntry::{Header, Window as Win},
        sidebar_entries,
    };
    let mut app = test_app();
    for (room, name) in [("!b:x.org", "loose room"), ("!a:x.org", "in space")] {
        let key = WinKey { account: "t".into(), room_id: room.try_into().unwrap(), thread: None };
        app.windows.push(Window::new(Some(key), name.into()));
    }
    assert_eq!(sidebar_entries(&app), vec![Win(0), Header("Rooms".into()), Win(2), Win(1)], "by name");

    let space = SpaceInfo {
        room_id: "!s:x.org".try_into().unwrap(),
        name: "Work".into(),
        children: vec!["!a:x.org".try_into().unwrap()],
    };
    app.spaces.insert("t".into(), vec![space]);
    assert_eq!(
        sidebar_entries(&app),
        vec![Win(0), Header("Rooms".into()), Header("◆ Work".into()), Win(2), Header("◇ other".into()), Win(1)]
    );
}

#[test]
fn favourites_come_first_in_the_window_list_unread_or_not() {
    use crate::ui::{
        SidebarEntry::{Header, Window as Win},
        sidebar_entries,
    };
    let mut app = test_app();
    let night = room_window(&mut app, "!n:x.org", "Night Owls");
    let sam = room_window(&mut app, "!s:x.org", "sam");
    let book = room_window(&mut app, "!b:x.org", "Book Club");
    app.windows[sam].is_dm = true;
    app.windows[night].favourite = true;
    app.windows[sam].favourite = true;
    app.windows[sam].activity = Activity::Highlight;
    app.windows[book].activity = Activity::Message;
    assert_eq!(
        sidebar_entries(&app),
        vec![Win(0), Header("★ Favourites".into()), Win(night), Win(sam), Header("Unreads".into()), Win(book),],
        "rooms and DMs together, by name, and they stay put when unread"
    );

    // In the switcher, a star says which.
    app.switcher = Some(Switcher::default());
    let entries = app.switcher_entries();
    let mut starred: Vec<&str> =
        entries.iter().filter(|e| e.detail.starts_with('★')).map(|e| e.name.as_str()).collect();
    starred.sort();
    assert_eq!(starred, ["Night Owls", "sam"]);
    app.switcher = None;

    // Making one takes the server (not connected here, so it says so),
    // and only rooms and DMs can be one.
    app.switch_to(book);
    assert_eq!(app.cmd_favourite(""), Err("t isn't connected".into()));
    assert_eq!(app.cmd_favourite("Night Owls"), Err("t isn't connected".into()));
    app.switch_to(0);
    assert_eq!(app.cmd_favourite(""), Err("only rooms and DMs can be favourites".into()));
    assert!(app.cmd_favourite("nowhere").is_err());
}

#[test]
fn window_list_puts_unreads_first_then_rooms_then_dms() {
    use crate::ui::{
        SidebarEntry::{Header, Window as Win},
        sidebar_entries,
    };
    let mut app = test_app();
    let night = room_window(&mut app, "!n:x.org", "Night Owls");
    let sam = room_window(&mut app, "!s:x.org", "sam");
    let book = room_window(&mut app, "!b:x.org", "Book Club");
    let jo = room_window(&mut app, "!j:x.org", "jo_bloggs");
    app.windows[sam].is_dm = true;
    app.windows[jo].is_dm = true;
    let rooms_then_dms =
        vec![Win(0), Header("Rooms".into()), Win(book), Win(night), Header("DMs".into()), Win(jo), Win(sam)];
    assert_eq!(sidebar_entries(&app), rooms_then_dms);

    // New messages bring a window up to Unreads, mentions first.
    app.windows[night].activity = Activity::Message;
    app.windows[sam].activity = Activity::Highlight;
    app.windows[jo].activity = Activity::Event;
    assert_eq!(
        sidebar_entries(&app),
        vec![
            Win(0),
            Header("Unreads".into()),
            Win(sam),
            Win(night),
            Header("Rooms".into()),
            Win(book),
            Header("DMs".into()),
            Win(jo),
        ],
        "joins and leaves aren't unread messages"
    );

    // Reading them sends them back.
    app.switch_to(sam);
    app.switch_to(night);
    app.windows[jo].activity = Activity::None;
    assert_eq!(sidebar_entries(&app), rooms_then_dms);
    assert_eq!(app.active, night, "the numbers stay what they were");
}

#[test]
fn thread_replies_reach_room_and_thread_windows() {
    let mut app = test_app();
    let room_id: OwnedRoomId = "!r:x.org".try_into().unwrap();
    let root: OwnedEventId = "$root".try_into().unwrap();
    let room_key = WinKey { account: "t".into(), room_id: room_id.clone(), thread: None };
    let thread_key = WinKey { thread: Some(root.clone()), ..room_key.clone() };
    app.windows.push(Window::new(Some(room_key), "room".into()));
    app.windows.push(Window::new(Some(thread_key), "⤷ thread".into()));
    app.windows[1].push(msg("$root", "shall we?"), 100);
    app.windows[2].push(msg("$root", "shall we?"), 100);

    let mut reply = msg("$r1", "yes");
    (reply.thread, reply.thread_root) = (true, Some(root.clone()));
    let other = msg("$x", "unrelated chatter");
    let react = Item::React { target: "$r1".try_into().unwrap(), key: "👍".into(), nick: "bob".into(), own: false };
    app.on_timeline("t", &room_id, vec![Item::Line(reply), Item::Line(other), react], true);

    let bodies = |w: &Window| w.lines.iter().map(|l| l.body.clone()).collect::<Vec<_>>();
    assert_eq!(bodies(&app.windows[1]), vec!["shall we?", "yes", "unrelated chatter"]);
    assert_eq!(bodies(&app.windows[2]), vec!["shall we?", "yes"], "only the thread's messages");
    assert_eq!(app.windows[1].lines[0].thread_count, Some(1), "root shows the new reply");
    assert_eq!(app.windows[2].lines[1].reactions.len(), 1, "reactions reach the thread window");
    assert_eq!(app.window_idx("t", &room_id), Some(1), "room lookups skip thread windows");
}

#[tokio::test]
async fn reload_applies_config_and_dotfile_changes() {
    let dir = std::env::temp_dir().join(format!("textrix-reload-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let paths = Paths { base: dir.clone(), env_file: dir.join(".env") };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app =
        App::new(Config::default(), paths.clone(), EnvFile::new(paths.env_file.clone()), Servers::default(), tx);
    assert!(app.show_sidebar);

    crate::servers::write_private(&paths.env_file, b"TEXTRIX_NEW_USER=@me:localhost\n").unwrap();
    std::fs::write(paths.config_file(), "sidebar = false\n").unwrap();
    app.cmd_reload();
    assert!(!app.show_sidebar, "config applied");
    assert!(app.accounts.contains_key("new"), "new dotfile server picked up");

    std::fs::write(paths.config_file(), "sidebar = maybe\n").unwrap();
    crate::servers::write_private(&paths.env_file, b"TEXTRIX_OTHER_USER=@me:localhost\n").unwrap();
    app.cmd_reload();
    assert!(!app.show_sidebar, "a broken config keeps the current settings");
    assert!(!app.accounts.contains_key("new") && app.accounts.contains_key("other"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn help_filter_keeps_matching_entries_and_their_sections() {
    let titles = |rows: &[HelpRow]| -> Vec<&str> {
        rows.iter().filter_map(|r| if let HelpRow::Section(t) = r { Some(*t) } else { None }).collect()
    };
    let all = help_rows("");
    assert_eq!(
        titles(&all),
        vec![
            "Keys",
            "Selecting messages (Alt+Up)",
            "Editing (Shift+Up)",
            "The theme list (/theme)",
            "The media gallery (/media)",
            "Home (/home, F4)",
            "Voting (Enter on a poll)",
            "A profile (click a name, or u)",
            "Commands"
        ]
    );
    let upgrade = help_rows("upgrade");
    assert_eq!(titles(&upgrade), vec!["Commands"]);
    assert!(upgrade.iter().any(|r| matches!(r, HelpRow::Entry(k, _) if k.starts_with("/upgrade"))));
    let leave = help_rows("/leave");
    assert!(leave.iter().any(|r| matches!(r, HelpRow::Entry(k, d) if k.starts_with("/part") && d.contains("/leave"))));
    assert!(help_rows("zzzz-nothing").is_empty());
}

#[test]
fn typing_clears_when_that_person_speaks() {
    let mut app = test_app();
    let room_id: OwnedRoomId = "!r:x.org".try_into().unwrap();
    app.windows
        .push(Window::new(Some(WinKey { account: "t".into(), room_id: room_id.clone(), thread: None }), "r".into()));
    let (alice, bob): (OwnedUserId, OwnedUserId) =
        ("@alice:x.org".try_into().unwrap(), "@bob:x.org".try_into().unwrap());
    app.on_event(AppEvent::Typing {
        account: "t".into(),
        room_id: room_id.clone(),
        nicks: vec![(alice.clone(), "alice".into()), (bob.clone(), "bob".into())],
    });
    assert_eq!(app.windows[1].typing.len(), 2);
    let mut spoke = msg("$1", "done typing");
    spoke.sender = Some(alice);
    app.on_timeline("t", &room_id, vec![Item::Line(spoke)], true);
    assert_eq!(app.windows[1].typing, vec![(bob, "bob".to_owned())], "alice spoke, so they aren't typing");
    assert!(on_off("on", "").unwrap() && !on_off("off", "").unwrap() && on_off("maybe", "usage").is_err());
}

#[test]
fn split_first_word() {
    assert_eq!(split_first("  join  #a:b  "), ("join", "#a:b  "));
    assert_eq!(split_first("quit"), ("quit", ""));
}
