// Ring the bell and pop up a notification when a message mentions one of
// your watch words, in any room, even without a highlight.
//
// The words are a setting: `/set watchwords.words deploy|outage|lunch`
// changes them for the session, and /save keeps them in config.toml.
//
// `matches(text, pattern)` is a regular expression test: (?i) ignores
// case, and \b keeps "deploy" from matching "redeployment". Rhai strings
// need the backslash doubled.

setting("words", "deploy|outage|pizza", "Words to watch for, separated by |");

on_message(|msg| {
    let watch = "(?i)\\b(" + setting("words") + ")\\b";
    if !msg.highlight && matches(msg.body, watch) {
        bell();
        notify(msg.nick + " in " + msg.room, msg.body);
    }
});
