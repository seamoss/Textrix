// /remind <minutes> <text>: a reminder, with the bell and a notification,
// after that long. /remind on its own lists them; /remind cancel stops them.
//
// after(seconds, fn) calls fn once that time has passed (every(seconds, fn)
// keeps calling it), and both return an ID that cancel(id) takes.

// The reminders still to come, each #{ key, id }; a fired one takes
// itself off.
let pending = [];
let made = 0;

command("remind", "Remind you: /remind 10 stretch", |args, ctx| {
    args.trim();
    if args == "" {
        print(if pending.is_empty() { "no reminders" } else { `${pending.len()} reminder(s) waiting` });
        return;
    }
    if args == "cancel" {
        for r in pending {
            cancel(r.id);
        }
        pending.clear();
        print("reminders cancelled");
        return;
    }
    let words = args.split(" ");
    let minutes = parse_int(words[0]);
    words.remove(0);
    let text = "";
    for word in words {
        text += if text == "" { word } else { " " + word };
    }
    if minutes < 1 || text == "" {
        print("usage: /remind <minutes> <text>");
        return;
    }
    made += 1;
    let key = made;
    let id = after(minutes * 60, || {
        pending.retain(|r| r.key != key);
        bell();
        notify("Reminder", text);
        print(`⏰ ${text}`);
    });
    pending.push(#{ key: key, id: id });
    print(`I'll remind you in ${minutes} min: ${text}`);
});
