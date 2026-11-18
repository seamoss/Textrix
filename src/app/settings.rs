//! Settings and servers: config.toml and /set, /save, /reload, /restart, aliases, /help, servers, devices and keys.

use super::*;

impl App {
    /// In the status window, and the window you're in if it's that server's.
    /// `/bell [on|off|away|sound|visual|both|test]` for this session.
    pub(super) fn cmd_bell(&mut self, args: &str) -> Result<(), String> {
        match args.trim() {
            "" => {}
            "on" => self.bell_mode = BellMode::On,
            "off" => self.bell_mode = BellMode::Off,
            "away" => self.bell_mode = BellMode::Away,
            "sound" => self.bell_style = BellStyle::Sound,
            "visual" => self.bell_style = BellStyle::Visual,
            "both" => self.bell_style = BellStyle::Both,
            "test" => {
                self.last_bell = None;
                let mode = std::mem::replace(&mut self.bell_mode, BellMode::On);
                let kinds = std::mem::replace(&mut self.cfg.bell_for, vec![BellKind::Mentions]);
                self.ring(BellKind::Mentions);
                (self.bell_mode, self.cfg.bell_for) = (mode, kinds);
                return Ok(());
            }
            _ => return Err("usage: /bell [on|off|away|sound|visual|both|test]".into()),
        }
        let when = match self.bell_mode {
            BellMode::On => "on",
            BellMode::Off => "off",
            BellMode::Away => "only while the terminal is in the background",
        };
        let how = match self.bell_style {
            BellStyle::Sound => "sound",
            BellStyle::Visual => "the status bar flashes",
            BellStyle::Both => "sound and a flash",
        };
        self.info(format!("bell: {when}; {how} (bell = … in config.toml makes it stick)"));
        Ok(())
    }

    /// Run an alias's commands; aliases may use other aliases, a few deep.
    pub(super) fn run_alias(&mut self, template: &str, args: &str) -> Result<(), String> {
        if self.alias_depth >= 8 {
            return Err("aliases call each other too deeply (a loop?)".into());
        }
        self.alias_depth += 1;
        for cmd in expand_alias(template, args) {
            self.command(&cmd);
        }
        self.alias_depth -= 1;
        Ok(())
    }

    pub(super) fn cmd_alias(&mut self, args: &str) -> Result<(), String> {
        let (name, expansion) = split_first(args.trim());
        let name = name.trim_start_matches('/').to_lowercase();
        if name.is_empty() {
            if self.cfg.aliases.is_empty() {
                self.info("no aliases yet; e.g. /alias tt /window goto Team Tests");
            }
            let rows: Vec<String> = self.cfg.aliases.iter().map(|(n, e)| format!("  /{n} → {e}")).collect();
            rows.into_iter().for_each(|row| self.info(row));
            return Ok(());
        }
        if expansion.trim().is_empty() {
            let text = match self.cfg.aliases.get(&name) {
                Some(e) => format!("/{name} → {e}"),
                None => format!("no alias /{name}"),
            };
            self.info(text);
            return Ok(());
        }
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)) {
            return Err("alias names are letters, digits, - and _".into());
        }
        let builtin = COMMANDS.iter().any(|c| c.name == name)
            || ALIASES.iter().any(|(a, _)| *a == name)
            || ["wc", "wl"].contains(&name.as_str())
            || EGGS.contains(&name.as_str());
        if builtin {
            return Err(format!("/{name} is a built-in command"));
        }
        let expansion = expansion.trim().to_owned();
        crate::config::save_alias(&self.paths.config_file(), &name, Some(&expansion)).map_err(|e| format!("{e:#}"))?;
        self.info(format!("/{name} → {expansion} (saved in {})", self.paths.config_file().display()));
        self.cfg.aliases.insert(name, expansion);
        Ok(())
    }

    pub(super) fn cmd_unalias(&mut self, args: &str) -> Result<(), String> {
        let name = args.trim().trim_start_matches('/').to_lowercase();
        if self.cfg.aliases.remove(&name).is_none() {
            return Err(format!("no alias /{name}; /alias lists them"));
        }
        crate::config::save_alias(&self.paths.config_file(), &name, None).map_err(|e| format!("{e:#}"))?;
        self.info(format!("removed /{name}"));
        Ok(())
    }

    /// `/split` and `/vsplit`: show another window in a new pane. Focus
    /// stays where you are typing; F6 moves it. With no window, an existing
    /// split just changes direction.
    pub(super) fn cmd_devices(&mut self, args: &str) -> Result<(), String> {
        let (acct, args) = self.pick_account(args)?;
        let (sub, rest) = split_first(args.trim());
        match sub {
            "" | "list" => acct.list_devices(self.here()),
            "rename" => {
                let (id, name) = split_first(rest);
                if id.is_empty() || name.trim().is_empty() {
                    return Err("usage: /devices rename <id> <name>".into());
                }
                acct.rename_device(self.here(), id.into(), name.trim().to_owned());
            }
            "delete" | "remove" | "signout" => {
                let (id, confirm) = split_first(rest);
                if id.is_empty() {
                    return Err("usage: /devices delete <id> (IDs from /devices)".into());
                }
                acct.delete_device(self.here(), id.into(), confirm.trim() == "confirm");
            }
            _ => return Err("usage: /devices [rename <id> <name> | delete <id> [confirm]]".into()),
        }
        Ok(())
    }

    pub(super) fn cmd_keys(&mut self, args: &str) -> Result<(), String> {
        let (acct, args) = self.pick_account(args)?;
        let (sub, file) = split_first(args.trim());
        match (sub, file.trim()) {
            ("export", "") => {
                let dir = dirs::download_dir().or_else(dirs::home_dir).ok_or("no Downloads folder; give a file")?;
                let name = format!("textrix-keys-{}-{}.txt", acct.name, Local::now().format("%Y-%m-%d"));
                acct.export_keys(self.here(), dir.join(name));
            }
            ("export", file) => acct.export_keys(self.here(), parse_path(file)),
            ("import", "") => return Err("usage: /keys import <file>".into()),
            ("import", file) => acct.import_keys(self.here(), parse_path(file)),
            _ => return Err("usage: /keys export [file] | /keys import <file>".into()),
        }
        Ok(())
    }

    pub(super) fn cmd_notify(&mut self, args: &str) -> Result<(), String> {
        let (acct, room_id) = self.room_ctx()?;
        if self.win().thread_root().is_some() {
            return Err("notifications are set for the whole room; use /notify in its window".into());
        }
        let mode = match args.trim() {
            "" => {
                let what = matrix::describe_notify(self.room_mode(self.active), self.win().is_dm);
                self.info(format!("notifications in {}: {what}", self.win().title()));
                return Ok(());
            }
            "all" => Some(RoomNotificationMode::AllMessages),
            "mentions" => Some(RoomNotificationMode::MentionsAndKeywordsOnly),
            "mute" | "off" => Some(RoomNotificationMode::Mute),
            "default" => None,
            _ => return Err("usage: /notify [all|mentions|mute|default]".into()),
        };
        acct.set_notify_mode(self.here(), room_id, mode);
        Ok(())
    }

    /// `/restart`: save everything as on quitting, and become the textrix
    /// on disk (a newer one, if it's been upgraded), back where you were.
    pub(super) fn cmd_restart(&mut self) -> Result<(), String> {
        if self.install.is_none() {
            return Err("can't find the textrix this was started as, to start it again".into());
        }
        let mut drafts: Vec<(WinRef, String)> =
            self.windows.iter().filter_map(|w| Some((w.id(), w.unsent.as_ref()?.text.as_ref()?.text()))).collect();
        if !self.input.is_empty() {
            drafts.push((self.windows[self.active].id(), self.input.text()));
        }
        let resume = crate::install::Resume { window: Some(self.windows[self.active].id()), drafts };
        let json = serde_json::to_vec(&resume).map_err(|e| e.to_string())?;
        crate::config::write_private(&crate::install::resume_file(&self.paths.base), &json)
            .map_err(|e| format!("{e:#}"))?;
        (self.restart, self.quit) = (true, true);
        Ok(())
    }

    pub(super) fn cmd_servers(&mut self) {
        let rows: Vec<String> = self
            .accounts
            .iter()
            .map(|(name, s)| {
                let hs = s.def.homeserver_or_server_name().unwrap_or("?");
                format!("{name}: {} on {hs} — {:?}", s.def.user, s.status)
            })
            .collect();
        for row in rows {
            self.info(row);
        }
    }

    /// Start or stop chat logging to match the config.
    pub(super) fn apply_logging(&mut self) {
        let dir = self.paths.log_dir(self.cfg.log_dir.as_deref());
        match (self.cfg.log, &self.log) {
            (true, Some(log)) if log.dir() == dir => {}
            (true, _) => {
                self.status(Line::info(format!("logging messages to {} (decrypted, plaintext)", dir.display())));
                self.log = Some(ChatLog::new(dir));
            }
            (false, Some(_)) => {
                self.log = None;
                self.status(Line::info("chat logging off"));
            }
            (false, None) => {}
        }
    }

    /// Switch to new settings, from config.toml or /set.
    pub(super) fn apply_config(&mut self, cfg: Config) {
        if cfg.image_previews && self.picker.is_none() {
            self.info("image previews start after a restart (graphics support is checked at startup)");
        }
        if !cfg.image_previews {
            self.picker = None;
            self.previews.clear();
        }
        self.notify_mode = notify::resolve(cfg.notifications);
        (self.show_sidebar, self.show_nicklist) = (cfg.sidebar, cfg.nicklist);
        (self.bell_mode, self.bell_style) = (cfg.bell, cfg.bell_style);
        if cfg.mouse != self.cfg.mouse {
            set_mouse_capture(cfg.mouse);
        }
        if cfg.image_animation != self.cfg.image_animation {
            self.previews.clear();
        }
        if cfg.image_text != self.cfg.image_text
            && let Some(painter) = self.picker.as_mut()
        {
            *painter = Painter::new(painter.picker.clone(), cfg.image_text);
            self.previews.clear();
            self.viewer = None;
        }
        if cfg.theme != self.cfg.theme || cfg.colors != self.cfg.colors {
            self.redraw_all = true;
        }
        self.scripts.configure(&cfg.script_settings);
        let plugins_changed = cfg.plugins != self.cfg.plugins;
        self.cfg = cfg;
        if plugins_changed {
            self.sync_plugins();
        }
        self.palette_for = self.cfg.theme.clone();
        self.apply_logging();
    }

    /// The settings as they stand in this session, after /theme, /bell,
    /// F2, F3 and the like.
    pub(super) fn effective_config(&self) -> Config {
        let mut cfg = self.cfg.clone();
        (cfg.bell, cfg.bell_style) = (self.bell_mode, self.bell_style);
        (cfg.sidebar, cfg.nicklist) = (self.show_sidebar, self.show_nicklist);
        cfg.script_settings = self.scripts.settings_table();
        cfg
    }

    /// `/set`: list the settings, show one, or change one for the session
    /// (checked like config.toml). Values are TOML (`12`, `true`,
    /// `["mentions"]`); anything else is taken as text.
    pub(super) fn cmd_set(&mut self, args: &str) -> Result<(), String> {
        let current = self.effective_config().to_table().map_err(|e| format!("{e:#}"))?;
        let (key, value) = split_first(args.trim());
        let value = value.trim();
        let script_settings = self.scripts.settings();
        if key.is_empty() {
            self.info("Settings (/set <name> <value> changes one for this session, /save keeps them):");
            let mut rows: Vec<String> = current
                .iter()
                .filter(|(k, _)| *k != "script_settings")
                .map(|(k, v)| match v.as_table() {
                    Some(t) => {
                        format!("  [{k}] {} entr{} (edit config.toml)", t.len(), if t.len() == 1 { "y" } else { "ies" })
                    }
                    None => format!("  {k} = {v}"),
                })
                .collect();
            if !script_settings.is_empty() {
                rows.push("Script settings:".into());
                rows.extend(script_settings.iter().map(|(k, v, help)| format!("  {k} = {v}  ({help})")));
            }
            rows.into_iter().for_each(|row| self.info(row));
            return Ok(());
        }
        let parse = |value: &str| {
            toml::from_str::<toml::Table>(&format!("v = {value}"))
                .ok()
                .and_then(|mut t| t.remove("v"))
                .unwrap_or_else(|| toml::Value::String(value.to_owned()))
        };
        // Settings that run programs aren't for scripts, which don't.
        if self.in_script > 0 && RUNS_PROGRAMS.contains(&key) {
            return Err(format!("scripts can't change {key}, which runs a program"));
        }
        if let Some((_, now, help)) = script_settings.iter().find(|(k, _, _)| k == key) {
            if value.is_empty() {
                self.info(format!("{key} = {now}  ({help})"));
            } else {
                let set = self.scripts.set_setting(key, parse(value))?;
                self.info(format!("{key} = {set} for this session; /save keeps it"));
            }
            return Ok(());
        }
        if value.is_empty() {
            let text = match current.get(key) {
                Some(v) => format!("{key} = {v}"),
                None => format!("no setting {key}; /set lists them"),
            };
            self.info(text);
            return Ok(());
        }
        if current.get(key).is_some_and(toml::Value::is_table) {
            return Err(format!("[{key}] is a table: edit it in config.toml, then /reload"));
        }
        let parsed = parse(value);
        let mut table = current;
        table.insert(key.to_owned(), parsed.clone());
        let cfg = Config::from_table(table, &self.paths.themes_dir()).map_err(|e| format!("{key}: {e:#}"))?;
        self.apply_config(cfg);
        self.info(format!("{key} = {parsed} for this session; /save keeps it"));
        Ok(())
    }

    /// `/save`: write this session's settings into config.toml.
    pub(super) fn cmd_save(&mut self) -> Result<(), String> {
        let path = self.paths.config_file();
        let written = crate::config::save_settings(&path, &self.effective_config()).map_err(|e| format!("{e:#}"))?;
        let text = match written.len() {
            0 => format!("{} already has these settings", path.display()),
            _ => format!("saved {} to {}", written.join(", "), path.display()),
        };
        self.info(text);
        Ok(())
    }

    /// Settings in config.toml this Textrix doesn't know, which it skipped.
    pub(super) fn note_ignored_settings(&mut self) {
        if self.cfg.ignored.is_empty() {
            return;
        }
        self.status(Line::error(format!(
            "{}: skipped {}, which this Textrix doesn't know (a newer one's setting, or a typo?)",
            self.paths.config_file().display(),
            self.cfg.ignored.join(", ")
        )));
    }

    /// `/reload`: apply config.toml and the dotfile without restarting. A
    /// broken file leaves the current settings in place.
    pub(super) fn cmd_reload(&mut self) {
        match Config::load(&self.paths.config_file()) {
            Ok(cfg) => {
                self.apply_config(cfg);
                self.info(format!("reloaded {}", self.paths.config_file().display()));
                self.note_ignored_settings();
            }
            Err(e) => self.error(format!("{e:#}; keeping the current settings")),
        }

        let servers = match self.env.servers() {
            Ok(servers) => servers,
            Err(e) => return self.error(format!("{e:#}; keeping the current servers")),
        };
        for warning in servers.warnings {
            self.status(Line::error(warning));
        }
        let names: HashSet<&str> = servers.defs.iter().map(|d| d.name.as_str()).collect();
        let removed: Vec<AccountName> = self.accounts.keys().filter(|n| !names.contains(&***n)).cloned().collect();
        for name in removed {
            if let Some(acct) = self.accounts.remove(&name).and_then(|s| s.account) {
                acct.stop();
            }
            while let Some(idx) = self.windows.iter().position(|w| w.account() == Some(&*name)) {
                self.remove_window(idx);
            }
            self.status(Line::info(format!("{name}: no longer in the dotfile; disconnected")));
        }
        for def in servers.defs {
            match self.accounts.get_mut(def.name.as_str()) {
                Some(state) => {
                    if state.def.user != def.user || state.def.homeserver != def.homeserver {
                        let name = def.name.clone();
                        (state.user, state.def) = (def.user.clone(), def);
                        self.status(Line::info(format!(
                            "{name}: account details changed; /disconnect {name} and /connect {name} to use them"
                        )));
                    }
                }
                None => {
                    let name = AccountName::from(def.name.as_str());
                    let state = AccountState {
                        user: def.user.clone(),
                        def,
                        nick: None,
                        status: ConnStatus::Offline,
                        verified: None,
                        away: None,
                        account: None,
                    };
                    self.accounts.insert(name.clone(), state);
                    self.status(Line::info(format!("{name}: new in the dotfile")));
                    self.connect(&name);
                }
            }
        }
    }

    pub(super) fn cmd_connect(&mut self, args: &str) -> Result<(), String> {
        let name = args.trim().to_lowercase();
        let state = self.accounts.get(name.as_str()).ok_or_else(|| {
            if name.is_empty() {
                "usage: /connect <server>".into()
            } else {
                format!("no server named {name} in the dotfile")
            }
        })?;
        if state.account.is_some() || state.status == ConnStatus::Connecting {
            return Err(format!("{name} is already connected"));
        }
        self.connect(&name);
        Ok(())
    }

    pub(super) fn cmd_disconnect(&mut self, args: &str) -> Result<(), String> {
        let name = args.trim().to_lowercase();
        let state = self.accounts.get_mut(name.as_str()).ok_or("usage: /disconnect <server>")?;
        let account = state.account.take().ok_or_else(|| format!("{name} is not connected"))?;
        account.stop();
        state.status = ConnStatus::Offline;
        self.status(Line::info(format!("{name}: disconnected")));
        Ok(())
    }

    /// `/logout <server>` and `/recover [server]`.
    pub(super) fn server_arg(&self, args: &str) -> Result<Account, String> {
        let name = args.trim().to_lowercase();
        if name.is_empty() {
            return self.pick_account("").map(|(a, _)| a);
        }
        self.connected(&name).ok_or_else(|| format!("{name} is not connected"))
    }

    pub(super) fn cmd_verify(&mut self, args: &str) -> Result<(), String> {
        let (acct, args) = self.pick_account(args)?;
        match args.trim() {
            "" => acct.verify_request(None),
            "accept" => acct.verify_accept(),
            "confirm" | "yes" => acct.verify_answer(VerifyAnswer::Confirm),
            "mismatch" | "no" => acct.verify_answer(VerifyAnswer::Mismatch),
            "cancel" => acct.verify_answer(VerifyAnswer::Cancel),
            who => acct.verify_request(Some(self.resolve_user(who)?)),
        }
        Ok(())
    }

    pub(super) fn cmd_help(&mut self, args: &str) {
        let wanted = args.trim().trim_start_matches('/').to_lowercase();
        if wanted.is_empty() {
            self.info("Commands (Tab completes; /help <command> for details; F1 for a searchable list):");
            let rows: Vec<String> = COMMANDS.iter().map(|c| format!("  /{:<10} {}", c.name, c.help)).collect();
            for row in rows {
                self.info(row);
            }
            for (title, keys) in
                [("Keys", KEYS), ("Selecting messages (Alt+Up)", SELECT_KEYS), ("Editing (Shift+Up)", EDIT_KEYS)]
            {
                self.info(format!("{title}:"));
                for (key, what) in keys {
                    self.info(format!("  {key:<34} {what}"));
                }
            }
            let scripted = self.scripts.commands();
            if !scripted.is_empty() {
                self.info("Script commands:");
                for (name, help, script) in scripted {
                    self.info(format!("  /{name:<10} {help} ({script}.rhai)"));
                }
            }
            let plugged = self.plugin_commands();
            if !plugged.is_empty() {
                self.info("Plugin commands:");
                for (plugin, c) in plugged {
                    self.info(format!("  /{:<10} {} ({plugin} plugin)", c.name, c.help));
                }
            }
            self.info("F1 shows all of this full-screen, with a filter");
            return;
        }
        let real = ALIASES.iter().find(|(a, _)| *a == wanted).map_or(wanted.as_str(), |(_, r)| *r);
        match COMMANDS.iter().find(|c| c.name == real) {
            Some(c) => {
                let text = format!("/{} {} — {}", c.name, c.args, c.help);
                self.info(text);
            }
            None => self.error(format!("no command /{wanted}")),
        }
    }
}
