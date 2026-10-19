// /roll [NdM]: roll dice for the room, e.g. /roll 2d6 (default 1d6).

command("roll", "Roll dice: /roll 2d6", |args, ctx| {
    args.trim();    // trim and make_lower change the string in place
    args.make_lower();
    let spec = if args == "" { "1d6" } else { args };
    if !matches(spec, "^[0-9]{0,2}d[0-9]{1,4}$") {
        print("usage: /roll NdM, e.g. /roll 2d6");
        return;
    }
    let parts = spec.split("d");
    let count = if parts[0] == "" { 1 } else { parse_int(parts[0]) };
    let sides = parse_int(parts[1]);
    if count < 1 || count > 20 || sides < 2 {
        print("usage: /roll NdM, with 1-20 dice of 2 or more sides");
        return;
    }
    let shown = "";
    let total = 0;
    for i in 0..count {
        let roll = random(sides) + 1;
        if shown != "" {
            shown += " + ";
        }
        shown += roll;
        total += roll;
    }
    if count > 1 {
        shown += ` = ${total}`;
    }
    say(`🎲 ${spec}: ${shown}`);
});
