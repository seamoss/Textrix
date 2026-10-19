// /shout <text>: say it in capitals, with feeling.
//
// The smallest useful script: one command. `args` is everything typed
// after the command; `ctx` says where you are (ctx.room, ctx.server,
// ctx.room_id).

command("shout", "Say it in capitals", |args, ctx| {
    if args == "" {
        print("usage: /shout <text>");
        return;
    }
    say(args.to_upper() + "!");
});
