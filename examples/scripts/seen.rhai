// /seen <name>: when someone last said something, and where.
//
// A script keeps state in variables its handlers capture: `last` below
// lives as long as the script is loaded (reloading starts it afresh).

let last = #{};

on_message(|msg| {
    last[msg.nick.to_lower()] = #{ nick: msg.nick, room: msg.room, at: timestamp(), said: msg.body };
});

command("seen", "When someone last spoke: /seen <name>", |args, ctx| {
    args.trim();    // trim changes the string in place
    let who = args.to_lower();
    if who == "" {
        print("usage: /seen <name>");
        return;
    }
    if who !in last {
        print(`haven't seen ${args} say anything since this script loaded`);
        return;
    }
    let seen = last[who];
    let secs = seen.at.elapsed.to_int();
    let ago = if secs < 60 { `${secs}s` } else if secs < 3600 { `${secs / 60}m` } else { `${secs / 3600}h ${secs % 3600 / 60}m` };
    print(`${seen.nick} was in ${seen.room} ${ago} ago: ${seen.said}`);
});
