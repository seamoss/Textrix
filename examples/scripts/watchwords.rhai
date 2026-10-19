// Ring the bell and pop up a notification when a message mentions one of
// your watch words, in any room, even without a highlight.
//
// `matches(text, pattern)` is a regular expression test: (?i) ignores
// case, and \b keeps "deploy" from matching "redeployment". Rhai strings
// need the backslash doubled.

let watch = "(?i)\\b(deploy|outage|pizza)\\b";

on_message(|msg| {
    if !msg.highlight && matches(msg.body, watch) {
        bell();
        notify(msg.nick + " in " + msg.room, msg.body);
    }
});
