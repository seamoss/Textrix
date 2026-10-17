//! Throwbacks for those who look: the Konami code, mIRC's trout, a magic
//! 8-ball, sysadmin excuses, Clippy, and a leet clock. Nothing here sends a
//! message you didn't type a command for.

use crossterm::event::KeyCode;

/// ↑ ↑ ↓ ↓ ← → ← → B A.
pub const KONAMI: [KeyCode; 10] = [
    KeyCode::Up,
    KeyCode::Up,
    KeyCode::Down,
    KeyCode::Down,
    KeyCode::Left,
    KeyCode::Right,
    KeyCode::Left,
    KeyCode::Right,
    KeyCode::Char('b'),
    KeyCode::Char('a'),
];

/// How far into the Konami code `progress` gets with `key`; reaching
/// `KONAMI.len()` means it's complete.
pub fn konami_step(progress: usize, key: KeyCode) -> usize {
    let key = match key {
        KeyCode::Char(c) => KeyCode::Char(c.to_ascii_lowercase()),
        other => other,
    };
    if KONAMI.get(progress) == Some(&key) {
        progress + 1
    } else if key == KONAMI[0] {
        // ↑ ↑ ↑ still leaves you two ups in.
        if progress >= 2 && KONAMI[..progress].iter().all(|k| *k == KONAMI[0]) { progress } else { 1 }
    } else {
        0
    }
}

pub const EIGHT_BALL: [&str; 20] = [
    "It is certain.",
    "It is decidedly so.",
    "Without a doubt.",
    "Yes, definitely.",
    "You may rely on it.",
    "As I see it, yes.",
    "Most likely.",
    "Outlook good.",
    "Yes.",
    "Signs point to yes.",
    "Reply hazy, try again.",
    "Ask again later.",
    "Better not tell you now.",
    "Cannot predict now.",
    "Concentrate and ask again.",
    "Don't count on it.",
    "My reply is no.",
    "My sources say no.",
    "Outlook not so good.",
    "Very doubtful.",
];

/// Why it's broken today.
pub const EXCUSES: [&str; 16] = [
    "a cosmic ray flipped a bit in the federation queue",
    "the homeserver is defragmenting its feelings",
    "someone tripped over the token ring",
    "the megolm session went for coffee and never came back",
    "your packets are taking the scenic route via a 56k modem",
    "the sync token expired of natural causes",
    "a moth got into the relay (we have it taped in the logbook)",
    "the cloud is experiencing light drizzle",
    "the room state resolved itself into a paradox",
    "a backhoe found the fibre",
    "the server's uptime was getting embarrassing",
    "the Y2K bug finally made it to your timezone",
    "the hamster powering the database needs a nap",
    "solar flares, probably",
    "the firewall took that personally",
    "it worked on the staging server",
];

/// A line for the status window at startup.
pub const TAGLINES: [&str; 10] = [
    "Hack the planet!",
    "Now with 100% fewer netsplits.",
    "Best viewed in 80×24.",
    "No Winamp skins were harmed.",
    "All your rooms are belong to you.",
    "Press any key to continue. Where's the any key?",
    "Your call is important to us. Please hold.",
    "Ready.",
    "Brought to you by the letter /.",
    "Please do not unplug the modem.",
];

/// mIRC's finest.
pub fn slap(target: &str) -> String {
    format!("slaps {target} around a bit with a large trout")
}

/// When the message starts like a letter.
pub fn clippy(text: &str) -> Option<&'static str> {
    let lower = text.trim_start().to_lowercase();
    ["dear ", "to whom it may concern"]
        .iter()
        .any(|start| lower.starts_with(start))
        .then_some("📎 It looks like you're writing a letter. Would you like help?")
}

/// Pick one, differently most times.
pub fn pick<T: Copy>(items: &[T]) -> T {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    items[nanos as usize % items.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn konami_code_counts_and_forgives_extra_ups() {
        let mut progress = 0;
        for key in KONAMI {
            progress = konami_step(progress, key);
        }
        assert_eq!(progress, KONAMI.len());
        let mut progress = 0;
        for key in [KeyCode::Up, KeyCode::Up, KeyCode::Up] {
            progress = konami_step(progress, key);
        }
        assert_eq!(progress, 2, "a third up still counts as the start");
        assert_eq!(konami_step(5, KeyCode::Char('x')), 0);
        assert_eq!(konami_step(8, KeyCode::Char('B')), 9, "shift doesn't matter");
    }

    #[test]
    fn clippy_spots_letters() {
        assert!(clippy("Dear team,").is_some());
        assert!(clippy("  to whom it may concern").is_some());
        assert!(clippy("deer are nice").is_none());
    }
}
