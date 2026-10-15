//! Secrets Textrix makes for itself (each account's store key and saved
//! session), kept in the OS keychain: the macOS Keychain through
//! `/usr/bin/security`, or the Secret Service through `secret-tool` on
//! Linux. Values only ever travel over pipes, never on a command line, and
//! are hex-encoded so no quoting can go wrong.

use std::{
    io::Write,
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail};
use zeroize::Zeroizing;

const SERVICE: &str = "Textrix";

/// Whether this system has a keychain Textrix can use.
pub fn available() -> bool {
    tool().is_some_and(|t| std::path::Path::new(t).exists() || which(t))
}

/// The value stored for `account`, if there is one.
pub fn get(account: &str) -> Result<Option<Zeroizing<String>>> {
    let output = match tool().context("no keychain on this system")? {
        MACOS => Command::new(MACOS).args(["find-generic-password", "-s", SERVICE, "-a", account, "-w"]).output(),
        tool => Command::new(tool).args(["lookup", "service", SERVICE, "account", account]).output(),
    }
    .context("running the keychain tool")?;
    let stdout = Zeroizing::new(output.stdout);
    if !output.status.success() {
        // 44: the macOS "item not found"; secret-tool just exits 1 with no output.
        if output.status.code() == Some(44) || (cfg!(not(target_os = "macos")) && stdout.is_empty()) {
            return Ok(None);
        }
        bail!("keychain lookup failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    let hex = String::from_utf8_lossy(&stdout);
    let bytes = Zeroizing::new(unhex(hex.trim()).context("the keychain item isn't one of Textrix's")?);
    Ok(Some(Zeroizing::new(String::from_utf8(bytes.to_vec()).context("the keychain item isn't text")?)))
}

/// Store `value` for `account`, replacing what was there, and check it
/// reads back.
pub fn set(account: &str, value: &str) -> Result<()> {
    let hex = Zeroizing::new(to_hex(value.as_bytes()));
    let tool = tool().context("no keychain on this system")?;
    let (mut child, input) = match tool {
        MACOS => {
            // Interactive mode reads the command from stdin, keeping the
            // value out of the process list.
            let child = Command::new(MACOS).arg("-i").stdin(Stdio::piped()).stdout(Stdio::null()).spawn();
            let line = format!("add-generic-password -U -s {SERVICE} -a {} -w {}\n", quote(account), hex.as_str());
            (child, Zeroizing::new(line))
        }
        tool => {
            let label = format!("{SERVICE} {account}");
            let child = Command::new(tool)
                .args(["store", "--label", &label, "service", SERVICE, "account", account])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn();
            (child, Zeroizing::new(hex.to_string()))
        }
    };
    let child = child.as_mut().map_err(|e| anyhow::anyhow!("running the keychain tool: {e}"))?;
    child.stdin.take().context("keychain tool stdin")?.write_all(input.as_bytes())?;
    child.wait()?;
    match get(account)? {
        Some(stored) if *stored == value => Ok(()),
        _ => bail!("the keychain didn't keep the item"),
    }
}

pub fn delete(account: &str) -> Result<()> {
    let Some(tool) = tool() else { return Ok(()) };
    let status = match tool {
        MACOS => Command::new(MACOS).args(["delete-generic-password", "-s", SERVICE, "-a", account]).output(),
        tool => Command::new(tool).args(["clear", "service", SERVICE, "account", account]).output(),
    }
    .context("running the keychain tool")?
    .status;
    if status.success() || status.code() == Some(44) { Ok(()) } else { bail!("couldn't remove the keychain item") }
}

/// 32 random bytes as hex, for a store passphrase.
pub fn random_key() -> Result<Zeroizing<String>> {
    use std::io::Read;
    let mut bytes = Zeroizing::new([0u8; 32]);
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(bytes.as_mut()))
        .context("reading /dev/urandom")?;
    Ok(Zeroizing::new(to_hex(bytes.as_ref())))
}

const MACOS: &str = "/usr/bin/security";

fn tool() -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        Some(MACOS)
    } else if cfg!(unix) {
        Some("secret-tool")
    } else {
        None
    }
}

fn which(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

/// Quote an argument for `security -i`, which splits on spaces.
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() {
        let value = "{\"passphrase\":\"ab\",\"session\":null} ✓";
        assert_eq!(unhex(&to_hex(value.as_bytes())).unwrap(), value.as_bytes());
        assert!(unhex("abc").is_none());
        assert!(unhex("zz").is_none());
    }

    #[test]
    fn random_keys_are_long_and_differ() {
        let (a, b) = (random_key().unwrap(), random_key().unwrap());
        assert_eq!(a.len(), 64);
        assert_ne!(*a, *b);
    }

    #[test]
    fn quoting_for_security_interactive() {
        assert_eq!(quote(r#"0x4f02 /Users/a "b"/x"#), r#""0x4f02 /Users/a \"b\"/x""#);
    }
}
