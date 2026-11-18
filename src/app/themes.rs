//! Themes: the picker (/theme), the editor (/theme edit), rooms' own themes, and the konami code.

use super::*;

impl App {
    /// ↑↑↓↓←→←→BA: the green theme, or back again.
    pub(super) fn konami_unlocked(&mut self) {
        if self.input.text() == "b" {
            self.input.clear();
        }
        let green = ThemeId::Builtin(ThemeName::Green);
        let back = self.cfg.theme == green;
        let id = if back { ThemeId::default() } else { green };
        if let Ok(palette) = Theme::load(&id, &self.cfg.colors, &self.paths.themes_dir()) {
            (self.cfg.palette, self.cfg.theme, self.redraw_all) = (palette, id.clone(), true);
            self.palette_for = id;
        }
        self.info(if back {
            "Phosphor off. The code still works."
        } else {
            "+30 lives. Phosphor on: the screen glows green until you type it again."
        });
    }

    /// `/themes`: the list, at the theme you have.
    pub(super) fn open_theme_picker(&mut self) {
        let dir = self.paths.themes_dir();
        let auto = ThemeChoice {
            id: ThemeId::Auto,
            shelf: "plain",
            about: format!("matches your terminal: {} now", crate::theme::auto_choice()),
        };
        let builtin = ThemeName::ALL.iter().map(|&(_, name)| ThemeChoice {
            id: name.into(),
            shelf: name.group(),
            about: name.about().to_owned(),
        });
        let mut entries: Vec<ThemeChoice> = std::iter::once(auto).chain(builtin).collect();
        for name in crate::theme::files(&dir) {
            let about = match ThemeFile::read(&ThemeFile::path(&dir, &name)) {
                Ok(file) => file.description.unwrap_or_else(|| "one of yours".into()),
                Err(_) => "can't be used (/theme list says why)".into(),
            };
            entries.push(ThemeChoice { id: ThemeId::File(name), shelf: "yours", about });
        }
        // At the theme on screen; for just this room if it has its own.
        let selected = entries.iter().position(|e| e.id == self.palette_for).unwrap_or(0);
        let room = self.win().room.as_ref().map(|_| self.win().title().to_owned());
        let just_here = self.own_theme(self.active).is_some();
        let before = (self.cfg.theme.clone(), self.cfg.palette.clone());
        self.theme_picker =
            Some(ThemePicker { entries, query: String::new(), selected, error: None, room, just_here, before });
    }

    pub(super) fn on_theme_picker_key(&mut self, key: KeyEvent) {
        use KeyCode::*;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let Some(p) = self.theme_picker.as_mut() else { return };
        let last = p.shown().len().saturating_sub(1);
        let was = p.shown().get(p.selected).map(|e| e.id.clone());
        let typed = |p: &mut ThemePicker, edit: &dyn Fn(&mut String)| {
            edit(&mut p.query);
            // Stay on the same theme if it still matches.
            p.selected = p.shown().iter().position(|e| Some(&e.id) == was.as_ref()).unwrap_or(0);
        };
        match key.code {
            Esc => return self.close_theme_picker(false),
            Char('c') if ctrl => return self.close_theme_picker(false),
            Char('t') if ctrl => {
                p.just_here = p.room.is_some() && !p.just_here;
                return;
            }
            Enter => return self.close_theme_picker(true),
            Up | BackTab => p.selected = p.selected.saturating_sub(1),
            Char('p') if ctrl => p.selected = p.selected.saturating_sub(1),
            Down | Tab => p.selected = (p.selected + 1).min(last),
            Char('n') if ctrl => p.selected = (p.selected + 1).min(last),
            PageUp => p.selected = p.selected.saturating_sub(10),
            PageDown => p.selected = (p.selected + 10).min(last),
            Home => p.selected = 0,
            End => p.selected = last,
            Backspace => typed(p, &|q| {
                q.pop();
            }),
            Char('u') if ctrl => typed(p, &|q| q.clear()),
            Char(c) if !ctrl && !alt => typed(p, &|q| q.push(c)),
            _ => return,
        }
        self.try_picked_theme();
    }

    /// Show the chosen theme on the whole screen.
    pub(super) fn try_picked_theme(&mut self) {
        let Some(p) = self.theme_picker.as_mut() else { return };
        p.selected = p.selected.min(p.shown().len().saturating_sub(1));
        let Some(id) = p.shown().get(p.selected).map(|e| e.id.clone()) else { return };
        match Theme::load(&id, &self.cfg.colors, &self.paths.themes_dir()) {
            Ok(palette) => {
                (self.cfg.palette, self.cfg.theme, p.error) = (palette, id, None);
                self.redraw_all = true;
            }
            Err(e) => p.error = Some(format!("{e:#}")),
        }
    }

    /// Enter keeps the theme on screen for the session; Esc puts back the
    /// one you had.
    pub(super) fn close_theme_picker(&mut self, keep: bool) {
        let Some(p) = self.theme_picker.take() else { return };
        if keep && p.error.is_none() && p.just_here {
            // The room keeps it; your own theme stays as it was.
            let chosen = std::mem::replace(&mut self.cfg.theme, p.before.0);
            self.palette_for = chosen.clone();
            let _ = self.set_own_theme(self.active, Some(chosen));
        } else if keep && p.error.is_none() {
            let changed = if *p.had() == self.cfg.theme { "" } else { " for this session; /save keeps it" };
            self.info(format!("theme: {}{changed}", self.cfg.theme));
            self.palette_for = self.cfg.theme.clone();
            self.note_room_theme();
        } else {
            if let Some(error) = &p.error {
                self.error(error.clone());
            }
            (self.cfg.theme, self.cfg.palette) = p.before;
        }
        self.redraw_all = true;
    }

    /// The theme window `idx`'s room has of its own, or a space it's in.
    pub(super) fn own_theme(&self, idx: usize) -> Option<ThemeId> {
        let key = self.windows.get(idx)?.room.clone()?;
        let key = WinKey { thread: None, ..key };
        let find = |key: &WinKey| self.layout.themes.iter().find(|(k, _)| k == key).map(|(_, t)| t.clone());
        find(&key).or_else(|| {
            let spaces = self.spaces.get(key.account.as_str())?;
            spaces
                .iter()
                .filter(|s| s.children.contains(&key.room_id))
                .find_map(|s| find(&WinKey { account: key.account.clone(), room_id: s.room_id.clone(), thread: None }))
        })
    }

    /// Give window `idx`'s room a theme of its own (`None` takes it away),
    /// kept in layout.json.
    pub(super) fn set_own_theme(&mut self, idx: usize, theme: Option<ThemeId>) -> Result<(), String> {
        let key = self.windows[idx].room.clone().ok_or("that's for a room: go to one first")?;
        let key = WinKey { thread: None, ..key };
        self.set_theme_for(key, theme.clone());
        let name = self.windows[idx].title().to_owned();
        match theme {
            Some(theme) => self.info(format!("{name}: theme {theme}, here only (/theme here off puts yours back)")),
            None => self.info(format!("{name}: your own theme again")),
        }
        Ok(())
    }

    pub(super) fn set_theme_for(&mut self, key: WinKey, theme: Option<ThemeId>) {
        self.layout.themes.retain(|(k, _)| *k != key);
        if let Some(theme) = theme {
            self.layout.themes.push((key, theme));
        }
        self.save_layout();
        self.follow_room_theme();
    }

    /// The colours for the window in focus: its room's own theme, or yours.
    pub fn follow_room_theme(&mut self) {
        if self.theme_picker.is_some() || self.theme_editor.is_some() {
            return;
        }
        let want = self.own_theme(self.active).unwrap_or_else(|| self.cfg.theme.clone());
        if want == self.palette_for {
            return;
        }
        let palette = Theme::load(&want, &self.cfg.colors, &self.paths.themes_dir());
        let palette = palette.or_else(|e| {
            // A room's theme file that's gone or broken: yours instead.
            self.error(format!("this room's theme {want}: {e:#}"));
            Theme::load(&self.cfg.theme, &self.cfg.colors, &self.paths.themes_dir())
        });
        if let Ok(palette) = palette {
            (self.cfg.palette, self.palette_for, self.redraw_all) = (palette, want, true);
        }
    }

    /// Whether a theme in use is `auto`, which needs to know the terminal's
    /// background.
    pub fn wants_background(&self) -> bool {
        self.cfg.theme == ThemeId::Auto || self.layout.themes.iter().any(|(_, t)| *t == ThemeId::Auto)
    }

    /// Now the terminal's background is known: `auto` follows it.
    pub fn refresh_auto_theme(&mut self) {
        if self.palette_for == ThemeId::Auto
            && let Ok(palette) = Theme::load(&ThemeId::Auto, &self.cfg.colors, &self.paths.themes_dir())
        {
            (self.cfg.palette, self.redraw_all) = (palette, true);
        }
    }

    /// After your own theme changed: say so if this room keeps its own.
    pub(super) fn note_room_theme(&mut self) {
        if let Some(own) = self.own_theme(self.active) {
            let name = self.win().title().to_owned();
            self.info(format!("{name} has a theme of its own, {own}: /theme here off gives it yours"));
        }
    }

    pub(super) fn cmd_theme(&mut self, args: &str) -> Result<(), String> {
        let args = args.trim();
        let (sub, rest) = args.split_once(' ').map_or((args, ""), |(a, b)| (a, b.trim()));
        match sub {
            "" => {
                self.open_theme_picker();
                return Ok(());
            }
            "list" => {
                self.list_themes();
                return Ok(());
            }
            "here" => return self.cmd_theme_here(rest),
            "space" => return self.cmd_theme_space(rest),
            "save" => return self.save_theme(rest),
            "edit" => return self.open_theme_editor(),
            "import" => return self.import_theme(rest),
            _ => {}
        }
        let id = ThemeId::parse(args).map_err(|e| format!("{e:#}"))?;
        self.cfg.palette =
            Theme::load(&id, &self.cfg.colors, &self.paths.themes_dir()).map_err(|e| format!("{e:#}"))?;
        self.cfg.theme = id.clone();
        self.palette_for = id;
        self.redraw_all = true;
        self.note_room_theme();
        Ok(())
    }

    /// `/theme here [name|off]`: this room's own theme.
    pub(super) fn cmd_theme_here(&mut self, name: &str) -> Result<(), String> {
        if self.win().room.is_none() {
            return Err("/theme here is for a room: go to one first".into());
        }
        match name {
            "" => {
                let text = match self.own_theme(self.active) {
                    Some(own) => format!("{}: theme {own}, its own", self.win().title()),
                    None => format!(
                        "{}: your theme ({}); /theme here <name> gives it its own",
                        self.win().title(),
                        self.cfg.theme
                    ),
                };
                self.info(text);
                Ok(())
            }
            "off" => self.set_own_theme(self.active, None),
            name => {
                let id = ThemeId::parse(name).map_err(|e| format!("{e:#}"))?;
                Theme::load(&id, &self.cfg.colors, &self.paths.themes_dir()).map_err(|e| format!("{e:#}"))?;
                self.set_own_theme(self.active, Some(id))
            }
        }
    }

    /// `/theme space <space> <name|off>`: a theme for every room in a
    /// space (each room's own still wins).
    pub(super) fn cmd_theme_space(&mut self, args: &str) -> Result<(), String> {
        let usage = "usage: /theme space <space> <theme|off>";
        let (space, theme) = args.rsplit_once(' ').ok_or(usage)?;
        let (space, theme) = (space.trim().trim_matches('"'), theme.trim());
        let account =
            self.win().account().map(ToOwned::to_owned).or_else(|| self.accounts.keys().next().map(|a| a.to_string()));
        let account = account.ok_or("no server")?;
        let found = self.spaces.get(account.as_str()).and_then(|spaces| {
            let lower = space.to_lowercase();
            spaces.iter().find(|s| s.name.to_lowercase() == lower).map(|s| (s.room_id.clone(), s.name.clone()))
        });
        let (room_id, name) = found.ok_or_else(|| format!("no space {space}; /spaces lists them"))?;
        let id = match theme {
            "off" => None,
            theme => {
                let id = ThemeId::parse(theme).map_err(|e| format!("{e:#}"))?;
                Theme::load(&id, &self.cfg.colors, &self.paths.themes_dir()).map_err(|e| format!("{e:#}"))?;
                Some(id)
            }
        };
        let text = match &id {
            Some(id) => format!("{name}: its rooms in {id}, unless they have their own"),
            None => format!("{name}: its rooms back to your theme"),
        };
        self.set_theme_for(WinKey { account, room_id, thread: None }, id);
        self.info(text);
        Ok(())
    }

    pub(super) fn list_themes(&mut self) {
        let custom = if self.cfg.colors.is_empty() { "" } else { ", with your [colors] changes" };
        let picked = match self.cfg.theme {
            ThemeId::Auto => format!(" (now {})", crate::theme::auto_choice()),
            _ => String::new(),
        };
        self.info(format!("theme: {}{picked}{custom}", self.cfg.theme));
        let mut shelves: Vec<(&str, Vec<&str>)> = vec![("plain", vec!["auto"])];
        for (name, theme) in ThemeName::ALL {
            match shelves.last_mut() {
                Some((group, names)) if *group == theme.group() => names.push(name),
                _ => shelves.push((theme.group(), vec![name])),
            }
        }
        for (group, names) in shelves {
            self.info(format!("  {group}: {}", names.join(", ")));
        }
        let dir = self.paths.themes_dir();
        let yours = crate::theme::files(&dir);
        if !yours.is_empty() {
            self.info(format!("  yours, in {}:", dir.display()));
        }
        for name in yours {
            let about = match ThemeFile::read(&ThemeFile::path(&dir, &name)) {
                Ok(file) => file.description.map(|d| format!(": {d}")).unwrap_or_default(),
                Err(e) => format!(" (can't be used: {})", e.root_cause()),
            };
            self.info(format!("    {name}{about}"));
        }
        self.info(
            "/theme on its own picks one with a preview (Ctrl+T there: just this room); /theme <name> tries one \
             (theme = \"<name>\" in config.toml keeps it); /theme here <name> and /theme space <space> <name> give \
             a room or a space's rooms their own; /theme save <name> writes this look to a theme file to share, \
             /theme import <file> adds one",
        );
    }

    /// `/theme save <name>`: the theme in use, with your `[colors]`, as a
    /// theme file.
    pub(super) fn save_theme(&mut self, name: &str) -> Result<(), String> {
        let (theme, colors) = (self.cfg.theme.clone(), self.cfg.colors.clone());
        let path = self.write_theme_file(name, &theme, &colors)?;
        self.info(format!("saved this look as {}: /theme {name} uses it, and the file shares it", path.display()));
        Ok(())
    }

    /// `theme` with `colors` over it, as a theme file called `name`: a new
    /// one, or the one being changed.
    pub(super) fn write_theme_file(
        &self,
        name: &str,
        theme: &ThemeId,
        colors: &BTreeMap<String, String>,
    ) -> Result<PathBuf, String> {
        if name.is_empty() {
            return Err("usage: /theme save <name>".into());
        }
        if !matches!(ThemeId::parse(name).map_err(|e| format!("{e:#}"))?, ThemeId::File(_)) {
            return Err(format!("{name} is a built-in theme; pick another name"));
        }
        if THEME_WORDS.contains(&name) {
            return Err(format!("/theme {name} means something else; pick another name"));
        }
        let dir = self.paths.themes_dir();
        let path = ThemeFile::path(&dir, name);
        if path.exists() && *theme != ThemeId::File(name.to_owned()) {
            return Err(format!("there's already a theme {name} ({}); pick another name", path.display()));
        }
        let mut file = ThemeFile::of(theme, colors, &dir).map_err(|e| format!("{e:#}"))?;
        file.name = Some(name.to_owned());
        let text = file.to_toml(name).map_err(|e| format!("{e:#}"))?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::create_dir_all(&dir)
            .and_then(|()| std::fs::write(&tmp, text))
            .and_then(|()| std::fs::rename(&tmp, &path))
            .map_err(|e| format!("writing {}: {e}", path.display()))?;
        Ok(path)
    }

    /// `/theme edit`: the theme on screen (this room's own, if it has one),
    /// from your `[colors]`.
    pub(super) fn open_theme_editor(&mut self) -> Result<(), String> {
        let base = self.palette_for.clone();
        let full = Theme::resolve(&base, &self.cfg.colors, &self.paths.themes_dir()).map_err(|e| format!("{e:#}"))?;
        let room = self.own_theme(self.active).is_some().then(|| self.win().title().to_owned());
        self.theme_picker = None;
        self.theme_editor = Some(crate::theme_editor::ThemeEditor {
            base,
            colors: self.cfg.colors.clone(),
            full,
            before: (self.cfg.colors.clone(), self.cfg.palette.clone()),
            room,
            selected: 0,
            typing: None,
            naming: None,
            error: None,
        });
        Ok(())
    }

    /// One colour changed (`None`: back to the theme's own), shown at once,
    /// or why it can't be.
    pub(super) fn edit_theme(&mut self, key: &str, value: Option<String>) {
        let dir = self.paths.themes_dir();
        let Some(e) = self.theme_editor.as_mut() else { return };
        match e.with(key, value, &dir) {
            Ok((colors, full)) => {
                (e.colors, e.full, e.error) = (colors, full, None);
                // On screen as this terminal draws it (256 colours, perhaps).
                if let Ok(palette) = Theme::load(&e.base, &e.colors, &dir) {
                    (self.cfg.palette, self.redraw_all) = (palette, true);
                }
            }
            Err(error) => e.error = Some(error),
        }
    }

    pub(super) fn on_theme_editor_key(&mut self, key: KeyEvent) {
        use crate::theme_editor::{FINE_HUE_STEP, HUE_STEP, LIGHT_STEP, ROLES, VIVID_STEP};
        use KeyCode::*;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(e) = self.theme_editor.as_mut() else { return };
        // The name to save it as.
        if let Some(name) = e.naming.as_mut() {
            match key.code {
                Esc => (e.naming, e.error) = (None, None),
                Enter => {
                    let name = name.trim().to_owned();
                    self.save_theme_editor(&name);
                }
                Backspace => drop(name.pop()),
                Char(c) if !ctrl => name.push(c),
                _ => {}
            }
            return;
        }
        // A colour typed in.
        if let Some(text) = e.typing.as_mut() {
            match key.code {
                Esc => (e.typing, e.error) = (None, None),
                Enter => {
                    let value = text.trim().to_owned();
                    let role = e.key();
                    e.typing = None;
                    // Nothing typed: the theme's own.
                    self.edit_theme(role, (!value.is_empty()).then_some(value));
                }
                Backspace => drop(text.pop()),
                Char(c) if !ctrl => text.push(c),
                _ => {}
            }
            return;
        }
        let last = ROLES.len() - 1;
        let hue = if key.modifiers.contains(KeyModifiers::SHIFT) { FINE_HUE_STEP } else { HUE_STEP };
        let moved = |e: &crate::theme_editor::ThemeEditor, h, s, l| (e.key(), Some(e.moved(h, s, l)));
        let change = match key.code {
            Esc => return self.close_theme_editor(),
            Char('c') if ctrl => return self.close_theme_editor(),
            Up | Char('k') => {
                e.selected = e.selected.saturating_sub(1);
                None
            }
            Down | Char('j') => {
                e.selected = (e.selected + 1).min(last);
                None
            }
            PageUp => {
                e.selected = e.selected.saturating_sub(10);
                None
            }
            PageDown => {
                e.selected = (e.selected + 10).min(last);
                None
            }
            Home | Char('g') => {
                e.selected = 0;
                None
            }
            End | Char('G') => {
                e.selected = last;
                None
            }
            Left | Char('h') => Some(moved(e, -hue, 0.0, 0.0)),
            Right | Char('l') => Some(moved(e, hue, 0.0, 0.0)),
            Char('-' | '_') => Some(moved(e, 0.0, 0.0, -LIGHT_STEP)),
            Char('+' | '=') => Some(moved(e, 0.0, 0.0, LIGHT_STEP)),
            Char(',' | '<') => Some(moved(e, 0.0, -VIVID_STEP, 0.0)),
            Char('.' | '>') => Some(moved(e, 0.0, VIVID_STEP, 0.0)),
            Char('r') => Some((e.key(), None)),
            Enter => {
                e.typing = Some(e.value());
                None
            }
            Char('s') => {
                e.naming = Some(e.suggested_name());
                None
            }
            _ => None,
        };
        if let Some((role, value)) = change {
            self.edit_theme(role, value);
        }
    }

    /// Esc: everything as it was.
    pub(super) fn close_theme_editor(&mut self) {
        if let Some(e) = self.theme_editor.take() {
            (self.cfg.palette, self.redraw_all) = (e.before.1, true);
        }
    }

    /// `s` in the editor: the look as a theme file called `name`, and that
    /// theme in use: yours (for this session; /save keeps it), or this
    /// room's when it was the room's own.
    pub(super) fn save_theme_editor(&mut self, name: &str) {
        let Some(e) = self.theme_editor.as_ref() else { return };
        let (base, colors, room) = (e.base.clone(), e.colors.clone(), e.room.clone());
        let path = match self.write_theme_file(name, &base, &colors) {
            Ok(path) => path,
            Err(error) => {
                if let Some(e) = self.theme_editor.as_mut() {
                    e.error = Some(error);
                }
                return;
            }
        };
        self.theme_editor = None;
        let id = ThemeId::File(name.to_owned());
        match (&room, self.win().room.clone()) {
            (Some(_), Some(key)) => self.set_theme_for(WinKey { thread: None, ..key }, Some(id.clone())),
            _ => self.cfg.theme = id.clone(),
        }
        match Theme::load(&id, &self.cfg.colors, &self.paths.themes_dir()) {
            Ok(palette) => (self.cfg.palette, self.palette_for) = (palette, id),
            Err(e) => self.error(format!("{e:#}")),
        }
        self.redraw_all = true;
        let whose = match &room {
            Some(room) => format!("{room}'s theme now"),
            None => "your theme now, for this session (/save keeps it)".into(),
        };
        self.info(format!("saved as {}: {whose}", path.display()));
        // [colors] in config.toml go over every theme, this one too.
        let over: Vec<&str> = self
            .cfg
            .colors
            .iter()
            .filter(|(key, value)| colors.get(*key) != Some(*value))
            .map(|(key, _)| key.as_str())
            .collect();
        if !over.is_empty() {
            self.info(format!(
                "[colors] in config.toml still sets {} over it: take {} out to see the theme's",
                over.join(", "),
                if over.len() == 1 { "that" } else { "those" }
            ));
        }
    }

    /// `/theme import <file>`: a theme file someone sent you, checked and
    /// copied into the themes folder.
    pub(super) fn import_theme(&mut self, arg: &str) -> Result<(), String> {
        if arg.is_empty() {
            return Err("usage: /theme import <file>".into());
        }
        let from = parse_path(arg);
        let file = ThemeFile::read(&from).map_err(|e| format!("{e:#}"))?;
        let stem = from.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
        let name = file.name.as_deref().unwrap_or(stem);
        match ThemeId::parse(name).map_err(|e| format!("{e:#}"))? {
            ThemeId::File(_) if THEME_WORDS.contains(&name) => {
                return Err(format!("“{name}” is a word /theme uses; change the file's name = line"));
            }
            ThemeId::File(_) => {}
            ThemeId::Builtin(_) | ThemeId::Auto => {
                return Err(format!("{name} is a built-in theme's name; change the file's name = line"));
            }
        }
        let dir = self.paths.themes_dir();
        let to = ThemeFile::path(&dir, name);
        if to.exists() {
            return Err(format!("there's already a theme {name} ({})", to.display()));
        }
        std::fs::create_dir_all(&dir)
            .and_then(|()| std::fs::copy(&from, &to))
            .map_err(|e| format!("copying it to {}: {e}", to.display()))?;
        self.info(format!("added theme {name}: /theme {name} tries it"));
        Ok(())
    }

    /// What the status bar says about upgrades: one installed under you,
    /// or one released.
    /// The theme on screen: yours, or the focused room's own.
    pub fn theme_on_screen(&self) -> &ThemeId {
        &self.palette_for
    }
}
