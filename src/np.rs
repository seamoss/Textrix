//! `/np`: what's playing, said in the room the way mIRC scripts did. Music
//! or Spotify on macOS (only if they're open: asking never starts them),
//! any MPRIS player on Linux through playerctl, or your own `np_command`.

use std::time::Duration;

use tokio::process::Command;

/// How long the player has to answer.
const WAIT: Duration = Duration::from_secs(5);

/// The players macOS is asked about, in this order.
const MAC_PLAYERS: &[&str] = &["Music", "Spotify"];

/// A track, as the player tells it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Track {
    pub title: String,
    pub artist: String,
    pub album: String,
    /// Music, Spotify, or playerctl's name for it.
    pub player: String,
}

impl Track {
    /// "Artist – Title", or the title alone.
    pub fn line(&self) -> String {
        match (self.artist.trim(), self.title.trim()) {
            ("", title) => title.to_owned(),
            (artist, "") => artist.to_owned(),
            (artist, title) => format!("{artist} – {title}"),
        }
    }

    /// `format` with {track}, {title}, {artist}, {album} and {player}
    /// filled in, and whether it's an action (it starts with `/me `).
    pub fn say(&self, format: &str) -> (String, bool) {
        let (format, action) = match format.strip_prefix("/me ") {
            Some(rest) => (rest, true),
            None => (format, false),
        };
        // One pass, so a title with "{album}" in it stays as it is.
        let mut out = String::new();
        let mut rest = format;
        while let Some(open) = rest.find('{') {
            out.push_str(&rest[..open]);
            let after = &rest[open..];
            let field = after.find('}').map(|close| (&after[1..close], close));
            let value = field.and_then(|(name, _)| match name {
                "track" => Some(self.line()),
                "title" => Some(self.title.clone()),
                "artist" => Some(self.artist.clone()),
                "album" => Some(self.album.clone()),
                "player" => Some(self.player.clone()),
                _ => None,
            });
            match (value, field) {
                (Some(value), Some((_, close))) => {
                    out.push_str(&value);
                    rest = &after[close + 1..];
                }
                _ => {
                    out.push('{');
                    rest = &after[1..];
                }
            }
        }
        out.push_str(rest);
        (out.trim().to_owned(), action)
    }
}

/// What's playing now: from `command` when it's set (its first line is
/// the track), otherwise from the system's players.
pub async fn now_playing(command: &str) -> Result<Track, String> {
    if !command.trim().is_empty() {
        let out = run(Command::new("sh").args(["-c", command])).await.map_err(|e| format!("np_command: {e}"))?;
        let line = out.lines().map(str::trim).find(|l| !l.is_empty()).ok_or("np_command printed nothing")?;
        return Ok(Track { title: line.to_owned(), ..Track::default() });
    }
    if cfg!(target_os = "macos") {
        let out = run(Command::new("osascript").args(["-l", "JavaScript", "-e", &mac_script()])).await?;
        return pick(&out, "Music or Spotify");
    }
    if cfg!(target_os = "linux") {
        let format = "{{status}}\t{{playerName}}\t{{title}}\t{{artist}}\t{{album}}";
        let mut playerctl = Command::new("playerctl");
        playerctl.args(["--all-players", "metadata", "--format", format]);
        let out = match run(&mut playerctl).await {
            Ok(out) => out,
            Err(e) if e.contains("No players found") => String::new(),
            Err(e) if e.starts_with("not found") => {
                return Err("nothing to ask: install playerctl, or set np_command".into());
            }
            Err(e) => return Err(format!("playerctl: {e}")),
        };
        return pick(&out, "any player");
    }
    Err("set np_command to a command that prints what's playing".into())
}

/// Asks each of `MAC_PLAYERS` that's already open what it's playing: a
/// line each of state, player, title, artist and album, tab-separated.
/// "denied" when macOS doesn't let the terminal ask it.
fn mac_script() -> String {
    let players = MAC_PLAYERS.iter().map(|p| format!("{p:?}")).collect::<Vec<_>>().join(", ");
    format!(
        r#"function run() {{
  var found = [];
  [{players}].forEach(function (name) {{
    var app;
    try {{ app = Application(name); }} catch (e) {{ return; }}
    try {{
      if (!app.running()) return;
      var state = String(app.playerState());
      if (state !== "playing" && state !== "paused") return;
      var t = app.currentTrack;
      found.push([state, name, t.name(), t.artist(), t.album()].join("\t"));
    }} catch (e) {{
      if (String(e).indexOf("1743") >= 0) found.push(["denied", name].join("\t"));
    }}
  }});
  return found.join("\n");
}}"#
    )
}

/// The playing track among a player's answers (state, player, title,
/// artist, album a line); otherwise why there's none.
fn pick(out: &str, asked: &str) -> Result<Track, String> {
    let mut paused = None;
    let mut denied = None;
    for line in out.lines() {
        let mut fields = line.split('\t').map(str::trim);
        let state = fields.next().unwrap_or("").to_lowercase();
        let player = fields.next().unwrap_or("").to_owned();
        let mut field = || fields.next().unwrap_or("").to_owned();
        let track = Track { title: field(), artist: field(), album: field(), player: player.clone() };
        match state.as_str() {
            "playing" if !track.line().is_empty() => return Ok(track),
            "paused" => paused = paused.or(Some(track)),
            "denied" => denied = denied.or(Some(player)),
            _ => {}
        }
    }
    if let Some(player) = denied {
        return Err(format!(
            "macOS didn't let your terminal ask {player}: allow it in System Settings → Privacy & Security → Automation"
        ));
    }
    match paused {
        Some(track) => Err(format!("nothing is playing ({} is paused on {})", track.player, track.line())),
        None => Err(format!("nothing is playing in {asked}")),
    }
}

/// A command's output, given a few seconds; its first error line if it
/// fails.
async fn run(command: &mut Command) -> Result<String, String> {
    command.kill_on_drop(true).stdin(std::process::Stdio::null());
    let out = match tokio::time::timeout(WAIT, command.output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => return Err(format!("not found: {e}")),
        Ok(Err(e)) => return Err(e.to_string()),
        Err(_) => return Err(format!("no answer in {}s", WAIT.as_secs())),
    };
    if !out.status.success() {
        let said = String::from_utf8_lossy(&out.stderr);
        let said = said.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").to_owned();
        return Err(if said.is_empty() { out.status.to_string() } else { said });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nightcall() -> Track {
        Track { title: "Nightcall".into(), artist: "Kavinsky".into(), album: "OutRun".into(), player: "Spotify".into() }
    }

    #[test]
    fn tracks_read_like_mirc_said_them() {
        let t = nightcall();
        assert_eq!(t.line(), "Kavinsky – Nightcall");
        assert_eq!(t.say("/me is listening to ♫ {track}"), ("is listening to ♫ Kavinsky – Nightcall".into(), true));
        assert_eq!(t.say("np: {title} ({album}, on {player})"), ("np: Nightcall (OutRun, on Spotify)".into(), false));
        assert_eq!(t.say("{nope} {track"), ("{nope} {track".into(), false), "unknown fields stay");
        let odd = Track { title: "{album}".into(), album: "x".into(), ..Track::default() };
        assert_eq!(odd.say("{title}").0, "{album}", "a title is never filled in again");
        let radio = Track { title: "Night Drive FM".into(), ..Track::default() };
        assert_eq!(radio.line(), "Night Drive FM", "no artist: the title alone");
    }

    #[test]
    fn the_playing_one_wins_and_otherwise_says_why() {
        let both = "paused\tMusic\tSome Song\tSomeone\tAn Album\nplaying\tSpotify\tNightcall\tKavinsky\tOutRun\n";
        assert_eq!(pick(both, "Music or Spotify"), Ok(nightcall()));
        let paused = "paused\tMusic\tSome Song\tSomeone\tAn Album\n";
        assert_eq!(pick(paused, "x").unwrap_err(), "nothing is playing (Music is paused on Someone – Some Song)");
        assert_eq!(pick("", "Music or Spotify").unwrap_err(), "nothing is playing in Music or Spotify");
        // playerctl says "Playing".
        assert_eq!(pick("Playing\tspotify\tNightcall\tKavinsky\tOutRun", "any player").unwrap().player, "spotify");
        assert!(pick("denied\tMusic", "x").unwrap_err().contains("Automation"));
    }

    #[test]
    fn the_mac_script_asks_only_open_players() {
        let script = mac_script();
        assert!(script.contains(r#"["Music", "Spotify"]"#), "{script}");
        // Checked before anything that would start the app.
        assert!(script.find("running()") < script.find("playerState()"));
    }

    #[tokio::test]
    async fn np_command_is_asked_first() {
        let track = now_playing("printf '\\n  Kavinsky – Nightcall  \\nmore\\n'").await.unwrap();
        assert_eq!(track.line(), "Kavinsky – Nightcall");
        assert_eq!(now_playing("true").await.unwrap_err(), "np_command printed nothing");
        assert!(now_playing("echo oops >&2; exit 3").await.unwrap_err().contains("oops"));
    }
}
