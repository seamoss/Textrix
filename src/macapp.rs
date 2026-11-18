//! `textrix app install` (macOS only): Textrix.app in ~/Applications, so
//! Alfred, Spotlight, Launchpad and the Dock can start Textrix. Opening it
//! opens a terminal window that runs textrix: in the Ghostty you have open
//! (or Ghostty started with just that), or in Terminal without Ghostty.
//!
//! The app is an AppleScript applet (`osacompile`, which every Mac has) with
//! a small sh script beside it that runs textrix with your login shell's
//! PATH. It runs the textrix that installed it, by the path it was started
//! as: on Homebrew the symlink every upgrade takes over, so it keeps working.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};

/// How the app knows it's this launcher, and not some other Textrix.app.
pub const BUNDLE_ID: &str = "io.github.seamoss.textrix.launcher";
const APP: &str = "Textrix.app";
const ICON: &[u8] = include_bytes!("../packaging/macos/Textrix.icns");
const GHOSTTY: &str = "/Applications/Ghostty.app";

/// Which terminal the app opens Textrix in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Terminal {
    Ghostty,
    Terminal,
}

impl Terminal {
    /// Ghostty when it's installed, otherwise the Mac's own Terminal.
    pub fn found() -> Self {
        if ghostty_app().is_some() { Terminal::Ghostty } else { Terminal::Terminal }
    }
}

fn ghostty_app() -> Option<PathBuf> {
    let home = dirs::home_dir().map(|h| h.join("Applications/Ghostty.app"));
    [Some(PathBuf::from(GHOSTTY)), home].into_iter().flatten().find(|p| p.exists())
}

/// Whether this Ghostty can be told to open a window with a command
/// (AppleScript, Ghostty 1.3 on), rather than started again for it.
fn ghostty_scriptable() -> bool {
    let Some(app) = ghostty_app() else { return false };
    Command::new("sdef")
        .arg(app)
        .output()
        .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains("new surface configuration"))
}

/// What the app does when it's opened.
pub fn applescript(terminal: Terminal, scriptable: bool) -> String {
    let head = "-- Textrix.app (textrix app install): Textrix in a terminal window.\n\
                set session to POSIX path of (path to me) & \"Contents/Resources/textrix-session\"\n";
    let body = match (terminal, scriptable) {
        // A window in the Ghostty you have open; Ghostty started with just
        // Textrix in it when it isn't.
        (Terminal::Ghostty, true) => {
            "if application \"Ghostty\" is running then\n\
             \ttell application \"Ghostty\"\n\
             \t\tactivate\n\
             \t\tset cfg to new surface configuration\n\
             \t\tset command of cfg to session\n\
             \t\tset initial working directory of cfg to POSIX path of (path to home folder)\n\
             \t\tnew window with configuration cfg\n\
             \tend tell\n\
             else\n\
             \tdo shell script \"open -a Ghostty --args -e \" & quoted form of session\n\
             end if\n"
        }
        // An older Ghostty: a Ghostty of its own for Textrix.
        (Terminal::Ghostty, false) => {
            "do shell script \"open -na Ghostty --args --quit-after-last-window-closed=true -e \" & quoted form of session\n"
        }
        (Terminal::Terminal, _) => {
            "tell application \"Terminal\"\n\
             \tactivate\n\
             \tdo script quoted form of session\n\
             end tell\n"
        }
    };
    format!("{head}{body}")
}

/// The script the app's window runs: textrix (at `textrix`, or by name),
/// with your login shell's PATH (mpv, gh…). A start that fails (it's
/// already open somewhere, say) leaves the window up to say why.
pub fn session_script(textrix: Option<&Path>) -> String {
    let textrix = textrix.map_or_else(|| "textrix".to_owned(), |p| sh_quote(&p.to_string_lossy()));
    format!(
        "#!/bin/sh\n\
         # Textrix.app's window (textrix app install): Textrix, with your login\n\
         # shell's PATH. If it stops with an error, the window stays to say why.\n\
         login_path=$(\"${{SHELL:-/bin/zsh}}\" -l -c 'printf \"\\n%s\" \"$PATH\"' 2>/dev/null | tail -n 1)\n\
         [ -n \"$login_path\" ] && PATH=$login_path\n\
         PATH=\"$PATH:/opt/homebrew/bin:/usr/local/bin\"\n\
         export PATH\n\
         textrix={textrix}\n\
         [ -x \"$textrix\" ] || textrix=textrix\n\
         \"$textrix\" && exit 0\n\
         code=$?\n\
         printf '\\nTextrix stopped (exit %s). Press Enter to close. ' \"$code\"\n\
         read -r _\n"
    )
}

/// `text` as one word for sh.
fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// Make `dir`/Textrix.app (replacing one this made before) that opens
/// Textrix in `terminal`, running `textrix`. Where it went.
pub fn install(dir: &Path, terminal: Terminal, textrix: Option<&Path>) -> Result<PathBuf> {
    let app = dir.join(APP);
    if app.exists() {
        if !ours(&app) {
            bail!("{} is there already and isn't this launcher: move it first", app.display());
        }
        std::fs::remove_dir_all(&app).with_context(|| format!("replacing {}", app.display()))?;
    }
    std::fs::create_dir_all(dir).with_context(|| format!("making {}", dir.display()))?;

    let source = std::env::temp_dir().join(format!("textrix-app-{}.applescript", std::process::id()));
    std::fs::write(&source, applescript(terminal, terminal == Terminal::Ghostty && ghostty_scriptable()))?;
    let compiled = Command::new("osacompile").arg("-o").arg(&app).arg(&source).output();
    let _ = std::fs::remove_file(&source);
    let compiled = compiled.context("running osacompile")?;
    if !compiled.status.success() {
        bail!("osacompile: {}", String::from_utf8_lossy(&compiled.stderr).trim());
    }

    let resources = app.join("Contents/Resources");
    let session = resources.join("textrix-session");
    std::fs::write(&session, session_script(textrix))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&session, std::fs::Permissions::from_mode(0o755))?;
    std::fs::write(resources.join("applet.icns"), ICON)?;
    // The applet's own icon, which would win over ours.
    let _ = std::fs::remove_file(resources.join("Assets.car"));
    let plist = app.join("Contents/Info.plist");
    for (key, kind, value) in [
        ("CFBundleName", "-string", "Textrix"),
        ("CFBundleIdentifier", "-string", BUNDLE_ID),
        // It only opens a window and quits: no Dock icon of its own.
        ("LSUIElement", "-bool", "true"),
    ] {
        let set = Command::new("plutil").args(["-replace", key, kind, value]).arg(&plist).status()?;
        if !set.success() {
            bail!("plutil couldn't set {key} in {}", plist.display());
        }
    }
    let _ = Command::new("plutil").args(["-remove", "CFBundleIconName"]).arg(&plist).output();
    // Changed after osacompile signed it: signed again, for this Mac.
    let _ = Command::new("codesign").args(["--force", "--deep", "--sign", "-"]).arg(&app).output();
    // Seen at once by Spotlight and Alfred, icon and all.
    let _ = Command::new("touch").arg(&app).output();
    let _ = Command::new("mdimport").arg(&app).output();
    Ok(app)
}

/// Take away `dir`/Textrix.app if this made it. Where it was, or `None`
/// when there's none.
pub fn uninstall(dir: &Path) -> Result<Option<PathBuf>> {
    let app = dir.join(APP);
    if !app.exists() {
        return Ok(None);
    }
    if !ours(&app) {
        bail!("{} isn't this launcher: left as it is", app.display());
    }
    std::fs::remove_dir_all(&app).with_context(|| format!("removing {}", app.display()))?;
    Ok(Some(app))
}

/// Whether the app at `app` is this launcher.
fn ours(app: &Path) -> bool {
    Command::new("plutil")
        .args(["-extract", "CFBundleIdentifier", "raw", "-o", "-"])
        .arg(app.join("Contents/Info.plist"))
        .output()
        .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).trim() == BUNDLE_ID)
}

/// Where the app goes unless you say: ~/Applications.
pub fn default_dir() -> Result<PathBuf> {
    Ok(dirs::home_dir().context("no home folder")?.join("Applications"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_app_opens_the_terminal_you_have() {
        let ghostty = applescript(Terminal::Ghostty, true);
        assert!(ghostty.contains("new window with configuration cfg"), "{ghostty}");
        assert!(ghostty.contains("open -a Ghostty --args -e"), "Ghostty started for it when it isn't running");
        assert!(applescript(Terminal::Ghostty, false).contains("open -na Ghostty"), "an older Ghostty");
        assert!(applescript(Terminal::Terminal, false).contains("tell application \"Terminal\""));
    }

    #[test]
    fn its_window_runs_the_textrix_that_made_it() {
        let script = session_script(Some(Path::new("/opt/home brew/bin/text'rix")));
        assert!(script.contains(r"textrix='/opt/home brew/bin/text'\''rix'"), "{script}");
        let checked = Command::new("sh").arg("-n").arg("-c").arg(&script).status().unwrap();
        assert!(checked.success(), "sh reads it: {script}");
        assert!(session_script(None).contains("textrix=textrix"));
    }

    #[test]
    fn install_makes_the_app_and_only_takes_away_its_own() {
        let dir = std::env::temp_dir().join(format!("textrix-macapp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let app = install(&dir, Terminal::Terminal, Some(Path::new("/opt/homebrew/bin/textrix"))).unwrap();
        assert_eq!(app, dir.join("Textrix.app"));
        assert!(ours(&app), "its bundle ID");
        let resources = app.join("Contents/Resources");
        assert_eq!(std::fs::read(resources.join("applet.icns")).unwrap(), ICON);
        let session = std::fs::read_to_string(resources.join("textrix-session")).unwrap();
        assert!(session.contains("/opt/homebrew/bin/textrix"));
        assert!(app.join("Contents/Resources/Scripts/main.scpt").exists(), "compiled");
        // Again: replaced, not refused.
        install(&dir, Terminal::Terminal, None).unwrap();
        assert_eq!(uninstall(&dir).unwrap(), Some(app.clone()));
        assert!(!app.exists());
        assert_eq!(uninstall(&dir).unwrap(), None, "nothing to take away");

        // Someone else's Textrix.app: left alone either way.
        std::fs::create_dir_all(app.join("Contents")).unwrap();
        std::fs::write(app.join("Contents/Info.plist"), "<plist><dict></dict></plist>").unwrap();
        assert!(install(&dir, Terminal::Terminal, None).unwrap_err().to_string().contains("isn't this launcher"));
        assert!(uninstall(&dir).unwrap_err().to_string().contains("isn't this launcher"));
        assert!(app.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
