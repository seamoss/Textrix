// Counts direct messages in the status bar, [DMs: 3], until you look:
// /dms says who wrote and clears the count.

let from = [];

on_message(|msg| {
    if msg.dm {
        if !from.contains(msg.nick) {
            from.push(msg.nick);
        }
        status(`DMs: ${from.len()}`);
    }
});

command("dms", "Who sent direct messages; clears the count", |args, ctx| {
    if from.is_empty() {
        print("no new direct messages");
        return;
    }
    let names = "";
    for nick in from {
        names += if names == "" { nick } else { ", " + nick };
    }
    print("direct messages from " + names);
    from.clear();
    status("");
});
