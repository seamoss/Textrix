//! The opt-in update notice (`check_updates = true`): at most once a day,
//! ask GitHub for the latest release and say if it's newer. Nothing is sent
//! but the request itself (with a "textrix/<version>" user agent).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

const LATEST: &str = "https://api.github.com/repos/seamoss/Textrix/releases/latest";
pub const EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// The latest release: (version without the "v", its page).
pub async fn latest() -> Result<(String, String)> {
    let client = matrix_sdk::reqwest::Client::builder()
        .user_agent(concat!("textrix/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(20))
        .build()?;
    let text = client
        .get(LATEST)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
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

    /// Talks to GitHub: `cargo test -- --ignored latest_release`.
    #[tokio::test]
    #[ignore]
    async fn latest_release_from_github() {
        let (version, url) = latest().await.unwrap();
        assert!(version.split('.').count() == 3, "{version}");
        assert!(url.contains("/releases/tag/v"), "{url}");
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
