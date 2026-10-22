#!/usr/bin/env python3
"""A Textrix plugin to start from: /hello, and "pong" to !ping.

In config.toml (`/plugin new` writes this, for the room you're in):

    [plugins.hello]
    command = "python3 hello.py"     # run in ~/.textrix/plugins
    rooms = ["Team Tests"]           # where it hears and may post

Change it, then `/plugin restart hello`. What a plugin can do is in
PLUGINS.md (github.com/seamoss/Textrix); textrix.py, next to this file, does the talking.
"""

import textrix

bot = textrix.Plugin("says hello, and pong to !ping")
pings = 0


@bot.on_start
def started():
    bot.status("0 pings")


@bot.command("hello", args="[name]", help="Say hello in this room")
def hello(args, room):
    if room is None:
        if bot.rooms:
            bot.log("/hello works in the rooms this plugin may use: " + ", ".join(r["name"] for r in bot.rooms))
        else:
            bot.log(f"{bot.name} has no rooms yet: list some under rooms in its [plugins.{bot.name}], then /reload")
        return
    bot.send(room["id"], f"hello, {args.strip() or 'everyone'} 👋")


@bot.on_message
def heard(msg):
    global pings
    if msg["body"].strip() == "!ping":
        pings += 1
        bot.status(f"{pings} ping{'' if pings == 1 else 's'}")
        bot.send(msg["room"]["id"], "pong", reply_to=msg["event_id"])


bot.run()
