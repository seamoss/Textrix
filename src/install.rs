//! Noticing that the textrix on disk has changed under a running one (`brew
//! upgrade`, `cargo build`), and `/restart`: becoming the new one in place.
//!
//! Textrix remembers the path it was started as (on Homebrew, the symlink
//! that the next version takes over) and what that pointed at. When either
//! changes, it asks the new binary its version. Nothing goes over the
//! network.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use serde::{Deserialize, Serialize};

/// The textrix this process was started as.
#[derive(Clone, Debug)]
pub struct Install {
    /// As started: `/opt/homebrew/bin/textrix`, `./target/debug/textrix`
    /// made absolute.
    pub path: PathBuf,
    /// What it was then (symlinks followed), and when that was written.
    target: PathBuf,
    modified: Option<SystemTime>,
}

impl Install {
    /// This process's own: the path it was started as, found on `PATH` if
    /// it was started by name.
    pub fn current() -> Option<Self> {
        let started = PathBuf::from(std::env::args_os().next()?);
        let path = if started.components().count() > 1 {
            std::env::current_dir().ok()?.join(started)
        } else {
            std::env::split_paths(&std::env::var_os("PATH")?).map(|dir| dir.join(&started)).find(|p| p.is_file())?
        };
        Self::at(path)
    }

    pub fn at(path: PathBuf) -> Option<Self> {
        let target = std::fs::canonicalize(&path).ok()?;
        let modified = std::fs::metadata(&target).and_then(|m| m.modified()).ok();
        Some(Self { path, target, modified })
    }

    /// Whether what `path` names now isn't what this process started as.
    pub fn replaced(&self) -> bool {
        match std::fs::canonicalize(&self.path) {
            Ok(target) => {
                target != self.target || std::fs::metadata(&target).and_then(|m| m.modified()).ok() != self.modified
            }
            // Mid-upgrade: nothing there right now.
            Err(_) => false,
        }
    }

    /// The version of what's at `path` now ("0.8.0"), asking it.
    pub async fn version(&self) -> Option<String> {
        let output = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::process::Command::new(&self.path).arg("--version").kill_on_drop(true).output(),
        )
        .await
        .ok()?
        .ok()?;
        let text = String::from_utf8(output.stdout).ok()?;
        let version = text.trim().strip_prefix("textrix ")?;
        version.chars().all(|c| c.is_ascii_digit() || c == '.').then(|| version.to_owned())
    }
}

/// Where you were when `/restart` happened, picked up by the new process.
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Resume {
    pub window: Option<crate::app::WinRef>,
    /// Unsent text in each window.
    pub drafts: Vec<(crate::app::WinRef, String)>,
}

pub fn resume_file(base: &Path) -> PathBuf {
    base.join("restart.json")
}

/// Become the textrix at `path` (the same arguments, the same process and
/// terminal). Only returns if that fails.
pub fn exec(path: &Path, args: Vec<OsString>) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    std::process::Command::new(path).args(args).exec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rebuilt_or_relinked_binary_is_noticed() {
        let dir = std::env::temp_dir().join(format!("textrix-install-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("0.7.0")).unwrap();
        std::fs::create_dir_all(dir.join("0.8.0")).unwrap();
        let (old, new, link) = (dir.join("0.7.0/textrix"), dir.join("0.8.0/textrix"), dir.join("textrix"));
        std::fs::write(&old, "old").unwrap();
        std::fs::write(&new, "new").unwrap();
        std::os::unix::fs::symlink(&old, &link).unwrap();

        let install = Install::at(link.clone()).unwrap();
        assert!(!install.replaced());
        // brew upgrade: the link now points at the new version.
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&new, &link).unwrap();
        assert!(install.replaced());

        // cargo build: the same path, written again.
        let built = Install::at(new.clone()).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&new, "rebuilt").unwrap();
        assert!(built.replaced());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn versions_come_from_the_binary() {
        let dir = std::env::temp_dir().join(format!("textrix-version-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("textrix");
        std::fs::write(&script, "#!/bin/sh\necho 'textrix 0.8.1'\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(Install::at(script.clone()).unwrap().version().await.as_deref(), Some("0.8.1"));
        std::fs::write(&script, "#!/bin/sh\necho 'something else'\n").unwrap();
        assert_eq!(Install::at(script).unwrap().version().await, None);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
