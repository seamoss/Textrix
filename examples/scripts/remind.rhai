// /remind <minutes> <text>: a reminder, with the bell and a notification,
// after that long. /remind on its own lists them; /remind cancel stops them.
//
// after(seconds, fn) calls fn once that time has passed (every(seconds, fn)
// keeps calling it), and both return an ID that cancel(id) takes.

let pending = [];

command("remind", "Remind you: /remind 10 stretch", |args, ctx| {
    args.trim();
    if args == "" {
        print(if pending.is_empty() { "no reminders" } else { `${pending.len()} reminder(s) waiting` });
        return;
    }
    if args == "cancel" {
        for id in pending {
            cancel(id);
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
    let id = after(minutes * 60, || {
        bell();
        notify("Reminder", text);
        print(`⏰ ${text}`);
    });
    pending.push(id);
    print(`I'll remind you in ${minutes} min: ${text}`);
});
