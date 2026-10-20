//! Ctrl+X: the message being typed, in your own editor (`editor` in
//! config.toml, else $VISUAL, else $EDITOR, else vi), back in the input
//! line when you quit it.
//!
//! The editor gets the terminal to itself: Textrix leaves its screen and
//! raw mode for it and comes back after. In the background, the attached
//! terminal runs it.

use std::{
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use crossterm::{
    event::{
        DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste, EnableFocusChange,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};

/// The editor to run: `configured`, else $VISUAL, else $EDITOR, else vi.
pub fn command(configured: &str) -> String {
    [Some(configured.to_owned()), std::env::var("VISUAL").ok(), std::env::var("EDITOR").ok()]
        .into_iter()
        .flatten()
        .map(|e| e.trim().to_owned())
        .find(|e| !e.is_empty())
        .unwrap_or_else(|| "vi".to_owned())
}

/// Hand the terminal to `editor` on `text`, and return what was saved.
/// `enhanced_keys`: the terminal reports keys the enhanced way, which the
/// editor shouldn't inherit.
pub fn edit(editor: &str, text: &str, enhanced_keys: bool) -> Result<String> {
    let file = TempFile::new(text)?;
    let mut out = std::io::stdout();
    let _ = crossterm::execute!(out, DisableMouseCapture, DisableBracketedPaste, DisableFocusChange);
    if enhanced_keys {
        let _ = crossterm::execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = crossterm::execute!(out, LeaveAlternateScreen, crossterm::cursor::Show);
    let _ = disable_raw_mode();

    let status = run(editor, &file.0);

    let _ = enable_raw_mode();
    let _ = crossterm::execute!(out, EnterAlternateScreen, EnableBracketedPaste, EnableFocusChange);
    if enhanced_keys {
        let _ = crossterm::execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    let _ = out.flush();

    let status = status?;
    if !status.success() {
        bail!("{editor} stopped with {status}; your message is as it was");
    }
    let saved = std::fs::read_to_string(&file.0).context("reading what the editor saved")?;
    Ok(saved.trim_end_matches(['\n', '\r']).to_owned())
}

/// `editor` with the file as its last argument, through the shell so an
/// editor with options (`code --wait`) works.
fn run(editor: &str, path: &Path) -> Result<std::process::ExitStatus> {
    Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("textrix")
        .arg(path)
        .status()
        .with_context(|| format!("starting {editor}"))
}

/// The message on disk while the editor has it: only you can read it, and
/// it's gone when this is.
struct TempFile(PathBuf);

impl TempFile {
    fn new(text: &str) -> Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let path = std::env::temp_dir().join(format!("textrix-message-{}-{nanos}.md", std::process::id()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        file.write_all(text.as_bytes())?;
        Ok(Self(path))
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_editor_edits_a_private_copy() {
        // A stand-in editor: uppercases the file in place.
        let file = TempFile::new("hello\nworld").unwrap();
        let path = file.0.clone();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let status = run("f() { tr a-z A-Z < \"$1\" > \"$1.up\" && mv \"$1.up\" \"$1\"; }; f", &path).unwrap();
        assert!(status.success());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "HELLO\nWORLD");
        drop(file);
        assert!(!path.exists(), "gone afterwards");
        assert_eq!(command("hx"), "hx");
    }
}
