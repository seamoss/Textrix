//! The opt-in update notice (`check_updates = true`): at most once an hour,
//! ask GitHub for the latest release and say if it's newer. Nothing is sent
//! but the request itself (with a "textrix/<version>" user agent). Also
//! `/changelog`, the latest releases' notes.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

const RELEASES: &str = "https://api.github.com/repos/seamoss/Textrix/releases";
pub const EVERY: Duration = Duration::from_secs(60 * 60);

/// A release, for `/changelog`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    /// Without the "v".
    pub version: String,
    /// When it came out, as `2026-10-20`.
    pub date: String,
    /// Its notes: its CHANGELOG.md section, in Markdown.
    pub notes: String,
}

async fn get(url: &str) -> Result<String> {
    let client = matrix_sdk::reqwest::Client::builder()
        .user_agent(concat!("textrix/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(20))
        .build()?;
    let response = client.get(url).header("Accept", "application/vnd.github+json").send().await?;
    Ok(response.error_for_status()?.text().await?)
}

/// The latest `count` releases, newest first.
pub async fn releases(count: usize) -> Result<Vec<Release>> {
    parse_releases(&get(&format!("{RELEASES}?per_page={count}")).await?)
}

fn parse_releases(json: &str) -> Result<Vec<Release>> {
    let list: Vec<serde_json::Value> =
        serde_json::from_str(json).context("GitHub's answer isn't a list of releases")?;
    Ok(list
        .iter()
        .filter(|r| !r["draft"].as_bool().unwrap_or(false) && !r["prerelease"].as_bool().unwrap_or(false))
        .filter_map(|r| {
            Some(Release {
                version: r["tag_name"].as_str()?.trim_start_matches('v').to_owned(),
                date: r["published_at"].as_str().unwrap_or_default().chars().take(10).collect(),
                notes: r["body"].as_str().unwrap_or_default().trim().to_owned(),
            })
        })
        .collect())
}

/// Notes as written are wrapped at 72 columns; joined back up, they wrap
/// at the window's width instead. Headings, list items, blank lines and
/// code stay where they are.
pub fn unwrapped(notes: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut code = false;
    for line in notes.lines() {
        let text = line.trim_start();
        let fence = text.starts_with("```");
        code ^= fence;
        let starts_block = text.is_empty()
            || code
            || fence
            || text.starts_with(['#', '>', '|'])
            || text.starts_with("- ")
            || text.starts_with("* ")
            || text.split_once(". ").is_some_and(|(n, _)| n.chars().all(|c| c.is_ascii_digit()) && !n.is_empty());
        match out.last_mut() {
            Some(last) if !starts_block && !last.trim().is_empty() && !last.trim_start().starts_with('#') => {
                last.push(' ');
                last.push_str(text);
            }
            _ => out.push(line.to_owned()),
        }
    }
    out.join("\n")
}

/// The latest release: (version without the "v", its page).
pub async fn latest() -> Result<(String, String)> {
    let text = get(&format!("{RELEASES}/latest")).await?;
    let release: serde_json::Value = serde_json::from_str(&text)?;
    let tag = release["tag_name"].as_str().context("no tag in GitHub's answer")?;
    let url = release["html_url"].as_str().unwrap_or("https://github.com/seamoss/Textrix/releases");
    Ok((tag.trim_start_matches('v').to_owned(), url.to_owned()))
}

/// Whether version `a` comes after `b` ("0.1.10" > "0.1.9").
pub fn newer(a: &str, b: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> { v.split('.').map(|p| p.parse().unwrap_or(0)).collect() };
    parts(a) > parts(b)
}

/// When the last check happened, from its stamp file.
pub fn last_checked(stamp: &std::path::Path) -> Option<SystemTime> {
    let secs: u64 = std::fs::read_to_string(stamp).ok()?.trim().parse().ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(secs))
}

pub fn mark_checked(stamp: &std::path::Path) {
    if let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) {
        let _ = std::fs::write(stamp, now.as_secs().to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number() {
        assert!(newer("0.1.10", "0.1.9"));
        assert!(newer("0.2.0", "0.1.99"));
        assert!(!newer("0.1.3", "0.1.3"));
        assert!(!newer("0.1.2", "0.1.3"));
    }

    #[test]
    fn releases_come_with_their_notes() {
        let json = r####"[
            {"tag_name": "v0.14.0", "published_at": "2026-10-20T22:53:41Z", "draft": false, "prerelease": false,
             "body": "### Added\n- Home\n\n"},
            {"tag_name": "v0.15.0-rc1", "published_at": "2026-10-21T00:00:00Z", "prerelease": true, "body": "x"},
            {"tag_name": "v0.13.0", "published_at": "2026-10-19T08:00:00Z", "body": null}
        ]"####;
        let releases = parse_releases(json).unwrap();
        assert_eq!(
            releases,
            [
                Release { version: "0.14.0".into(), date: "2026-10-20".into(), notes: "### Added\n- Home".into() },
                Release { version: "0.13.0".into(), date: "2026-10-19".into(), notes: String::new() },
            ]
        );
        assert!(parse_releases(r#"{"message": "API rate limit exceeded"}"#).is_err());
    }

    #[test]
    fn wrapped_notes_join_back_up() {
        let notes = "### Added\n- Home (F4): what's\n  waiting for you.\n- The gallery\n\nA paragraph\n`code` wrapped.\n\n```\nkeep\nthis\n```";
        assert_eq!(
            unwrapped(notes),
            "### Added\n- Home (F4): what's waiting for you.\n- The gallery\n\nA paragraph `code` wrapped.\n\n```\nkeep\nthis\n```"
        );
    }

    /// Talks to GitHub: `cargo test -- --ignored latest_release`.
    #[tokio::test]
    #[ignore]
    async fn latest_release_from_github() {
        let (version, url) = latest().await.unwrap();
        assert!(version.split('.').count() == 3, "{version}");
        assert!(url.contains("/releases/tag/v"), "{url}");
        let releases = releases(3).await.unwrap();
        assert_eq!(releases.len(), 3);
        assert_eq!(releases[0].version, version, "newest first");
        assert!(releases[0].notes.starts_with("### "), "{}", releases[0].notes);
    }

    #[test]
    fn the_stamp_remembers_the_last_check() {
        let stamp = std::env::temp_dir().join(format!("textrix-update-{}", std::process::id()));
        assert!(last_checked(&stamp).is_none());
        mark_checked(&stamp);
        assert!(last_checked(&stamp).unwrap().elapsed().unwrap() < Duration::from_secs(5));
        std::fs::remove_file(stamp).unwrap();
    }
}
