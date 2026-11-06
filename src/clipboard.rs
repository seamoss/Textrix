//! The image on the system clipboard, for pasting into a message: terminals
//! only paste text, so Textrix asks the system (`osascript` on macOS,
//! `wl-paste` or `xclip` elsewhere). Over SSH that's the clipboard of the
//! machine Textrix runs on.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::process::Command;

/// The image on the clipboard, saved in `dir` and named like a macOS
/// screenshot, or `None` when the clipboard holds no image.
pub async fn image(dir: &Path) -> Result<Option<PathBuf>> {
    crate::config::create_private_dir(dir)?;
    let stem = chrono::Local::now().format("Pasted image %Y-%m-%d at %H.%M.%S").to_string();
    #[cfg(target_os = "macos")]
    return mac(dir, &stem).await;
    #[cfg(not(target_os = "macos"))]
    return unix(dir, &stem).await;
}

/// PNG if the clipboard has it (screenshots, most browsers), else TIFF
/// (Preview, Safari) turned into PNG by `sips`.
#[cfg(target_os = "macos")]
async fn mac(dir: &Path, stem: &str) -> Result<Option<PathBuf>> {
    let png = dir.join(format!("{stem}.png"));
    if write_clipboard_as(&png, "PNGf").await? {
        return Ok(Some(png));
    }
    let tiff = dir.join(format!("{stem}.tiff"));
    if !write_clipboard_as(&tiff, "TIFF").await? {
        return Ok(None);
    }
    let converted =
        Command::new("/usr/bin/sips").args(["-s", "format", "png"]).arg(&tiff).arg("--out").arg(&png).output().await;
    let _ = tokio::fs::remove_file(&tiff).await;
    match converted {
        Ok(out) if out.status.success() => Ok(Some(png)),
        _ => anyhow::bail!("the clipboard's image couldn't be turned into a PNG"),
    }
}

/// Write the clipboard, as AppleScript's `class` of data, to `file`.
/// Whether there was such data. The path goes as an argument, never into
/// the script.
#[cfg(target_os = "macos")]
async fn write_clipboard_as(file: &Path, class: &str) -> Result<bool> {
    let script = format!(
        "on run argv
            set f to open for access (POSIX file (item 1 of argv)) with write permission
            try
                set eof f to 0
                write (the clipboard as «class {class}») to f
                close access f
            on error e
                close access f
                error e
            end try
        end run"
    );
    let out = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .arg(file)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .context("running osascript")?;
    let written = out.status.success() && tokio::fs::metadata(file).await.is_ok_and(|m| m.len() > 0);
    if !written {
        let _ = tokio::fs::remove_file(file).await;
    }
    Ok(written)
}

/// Image types to ask for, best first, with the extension each is saved as.
#[cfg(any(not(target_os = "macos"), test))]
const TYPES: [(&str, &str); 5] =
    [("image/png", "png"), ("image/jpeg", "jpg"), ("image/webp", "webp"), ("image/gif", "gif"), ("image/bmp", "bmp")];

/// The best image type among those the clipboard offers, one per line.
#[cfg(any(not(target_os = "macos"), test))]
fn pick(offered: &str) -> Option<(&'static str, &'static str)> {
    TYPES.into_iter().find(|(mime, _)| offered.lines().any(|line| line.trim() == *mime))
}

/// Wayland's `wl-paste`, or X11's `xclip`.
#[cfg(not(target_os = "macos"))]
async fn unix(dir: &Path, stem: &str) -> Result<Option<PathBuf>> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let (program, package, list, get): (&str, &str, &[&str], &[&str]) = if wayland {
        ("wl-paste", "wl-clipboard", &["--list-types"], &["--no-newline", "--type"])
    } else {
        (
            "xclip",
            "xclip",
            &["-selection", "clipboard", "-o", "-t", "TARGETS"],
            &["-selection", "clipboard", "-o", "-t"],
        )
    };
    let offered = Command::new(program)
        .args(list)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .with_context(|| format!("running {program} (install {package} to paste images)"))?;
    // An empty clipboard is an error to them.
    let Some((mime, ext)) = offered.status.success().then(|| pick(&String::from_utf8_lossy(&offered.stdout))).flatten()
    else {
        return Ok(None);
    };
    let data = Command::new(program).args(get).arg(mime).stdin(std::process::Stdio::null()).output().await?;
    if !data.status.success() || data.stdout.is_empty() {
        return Ok(None);
    }
    let file = dir.join(format!("{stem}.{ext}"));
    tokio::fs::write(&file, &data.stdout).await.with_context(|| format!("writing {}", file.display()))?;
    Ok(Some(file))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_best_image_type_offered_is_taken() {
        let offered = "TARGETS\ntext/plain\nimage/jpeg\nimage/png\n";
        assert_eq!(pick(offered), Some(("image/png", "png")));
        assert_eq!(pick("text/plain\nimage/gif"), Some(("image/gif", "gif")));
        assert_eq!(pick("text/plain\nUTF8_STRING"), None, "text only");
        assert_eq!(pick("image/svg+xml"), None, "not one Textrix sends as a picture");
    }
}
