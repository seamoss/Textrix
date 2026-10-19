// /seen <name>: when someone last said something, and where.
//
// It keeps who, where and when in the script's store, so it remembers
// across restarts. (Not what they said: a store is a plain file.)

on_message(|msg| {
    store_set(msg.nick.to_lower(), #{ nick: msg.nick, room: msg.room, at: now() });
});

command("seen", "When someone last spoke: /seen <name>", |args, ctx| {
    args.trim();    // trim changes the string in place
    let who = args.to_lower();
    if who == "" {
        print("usage: /seen <name>");
        return;
    }
    let seen = store_get(who);
    if type_of(seen) != "map" {
        print(`haven't seen ${args} say anything`);
        return;
    }
    let secs = now() - seen.at;
    let ago = if secs < 60 {
        `${secs}s`
    } else if secs < 3600 {
        `${secs / 60}m`
    } else if secs < 86400 {
        `${secs / 3600}h ${secs % 3600 / 60}m`
    } else {
        `${secs / 86400} days`
    };
    print(`${seen.nick} was in ${seen.room} ${ago} ago`);
});
