# codebot

codebot takes coding requests in a Matrix room to a coding agent, and
brings back pull requests. You ask in plain words. The agent (Claude Code,
Codex, aider, or your own) makes the change on a branch of its own. codebot
then runs the tests, hands any failures back to the agent, and opens the
pull request. Anything more you say in its thread carries on with the same
pull request.

```
<sam>      codebot: make the to-do list say how many items are left
           👀
           └ codebot: ⏳ Working (1m) on codebot/make-the-to-do-list-say-how-48213 in todo.
                       ✏️ editing todo.py · 2 files changed
           └ codebot: ✅ Pull request: https://github.com/sam/todo/pull/12
                       2 files changed, 23 insertions(+); tests passed · 1m24s · $0.09
                       More in this thread carries on with it.
<sam>      └ also say "all done!" when there's nothing left
           └ codebot: ✅ Updated the pull request: https://github.com/sam/todo/pull/12
```

It's a plugin ([PLUGINS.md](PLUGINS.md)) that runs in a bot: a Textrix in
the background, logged in as its own Matrix account. So it talks as itself
and never as you. Its work only ever reaches you as pull requests. It never
merges anything, and never pushes anywhere but its own `codebot/…`
branches.

*Experimental:* it has made real pull requests with Claude Code, but it's
young. Try it on a repository you don't mind first.

- [Setting it up](#setting-it-up)
- [Talking to it](#talking-to-it)
- [What happens to a request](#what-happens-to-a-request)
- [Following up](#following-up)
- [Tests, and second goes](#tests-and-second-goes)
- [Several repositories](#several-repositories)
- [Watching it work](#watching-it-work)
- [Agents](#agents)
- [Settings](#settings)
- [Safety](#safety)
- [Troubleshooting](#troubleshooting)
- [Limits](#limits)

## Setting it up

You need:

- **A Matrix account for the bot** on your server, e.g. `@codebot:example.org`.
  Make it the way your server makes accounts; Textrix can't make accounts.
- **A git checkout** of the repository, whose `origin` is on GitHub.
- **[`gh`](https://cli.github.com)**, logged in (`gh auth login`) with
  access to that repository, and git able to push there.
- **The agent**: [Claude Code](https://docs.anthropic.com/en/docs/claude-code)
  (`claude`) unless you pick another, logged in.
- **python3**, to run codebot itself.

Then, in your own Textrix, in the room codebot is for:

```
/codebot start ~/code/project
```

The first time, that makes the bot, with its home in `~/.textrix/bots/codebot`.
It then asks you to put the bot's account in the `.env` there:

```sh
TEXTRIX_BOT_USER=@codebot:example.org
TEXTRIX_BOT_PASSWORD=…
```

Run `/codebot start ~/code/project` again once that's in. It does the rest:

- it writes codebot into the bot, with its settings: the repository, the
  branch pull requests go to (origin's default branch), the tests (guessed
  from the project), and you as the one who gives it work;
- it grants it the room;
- it starts the bot, or restarts it so it reads new settings;
- it invites the bot in. The bot joins rooms it's invited to by you.

`/codebot` then says how it's set up, `/codebot log` shows the end of its
log, `/codebot stop` stops it, and `/codebot help` is a short reminder.
`/codebot start` in another room adds that room too. With another checkout,
it adds that repository as that room's own (see
[Several repositories](#several-repositories)). Running it again is always
safe: it only changes what's new, and it brings codebot up to date with
your Textrix.

The tests it guesses: `cargo test` (Cargo.toml), `npm test` (package.json),
`go test ./...` (go.mod), pytest or unittest (Python), or `make test` (a
Makefile with a `test:` target). Change them in its settings if that's
wrong.

**The bot runs with your Textrix's environment.** It gets the PATH that
finds `claude`, `git` and `gh`, your ssh agent, and any API keys you have
set. One consequence: if `ANTHROPIC_API_KEY` is set, Claude Code uses it
before your claude.ai login, so codebot's work is billed to that key. Start
your Textrix (and so the bot) without it if you'd rather use your plan.

By hand, without `/codebot`: `textrix bot new codebot`, put its account in
`~/.textrix/bots/codebot/.env`, `textrix bot start codebot`, invite it,
then `textrix bot attach codebot` and `/plugin new codebot codebot` in the
room, and fill in [its settings](#settings) in the bot's config.toml.
`/plugin new … codebot` in your own Textrix is refused: there it would post
as you.

## Talking to it

| Where | What you say | What happens |
|---|---|---|
| the room | `codebot: <what to change>` | a new task, on a new branch, for a new pull request |
| | `!code <what to change>` | the same, for short |
| | `codebot in <repo>: <what to change>` | the same, in another of its repositories |
| | `codebot: status` | what it's doing, and what's queued |
| | `codebot: repos` | its repositories, and which is this room's |
| | `codebot: stop` | stops the task it's working on |
| | `codebot: help` | a short reminder |
| its thread | anything that's more to do | carries on with the same branch and pull request ([Following up](#following-up)) |
| | `go` | starts a task that waits for one (`approve = true`) |
| | `stop` | stops that task, or drops it from the queue |

Its name at the front of a message is enough: typed out, Tab-completed, or
its full Matrix ID. In its own threads you needn't name it at all. A
thank-you there ("thanks!", "lgtm", 👍) isn't taken as more work.

Only people in `allow` can give it work. Anyone else is told so, once an
hour at most.

## What happens to a request

1. It reacts 👀 to your message and answers in a thread under it.
2. It makes a branch, `codebot/<words-from-the-request>-<number>`, from
   `origin/<base>`, in a worktree of its own. Your checkout isn't touched.
3. It runs the agent there, with your request as one argument. The agent
   edits files; it doesn't commit or push.
4. codebot commits what changed, as `codebot`, with who asked in the
   commit message.
5. It runs the tests. If they fail, the agent gets the failures and
   another go, up to `fix_rounds` more times
   ([Tests, and second goes](#tests-and-second-goes)).
6. It pushes the branch, its own and no other, and opens the pull request
   with `gh pr create`. The pull request says who asked and what; what the
   agent said; how the tests went; and how long it took, and what it cost
   with Claude Code.
7. It posts the link (✅), with what changed and how the tests went.

If the agent changes nothing, you get 🤷 and its answer. If it fails or
runs out of time, you get ❌ or ⌛ and nothing is pushed. If you stop it, you
get 🛑 and nothing is pushed.

One task runs at a time, and up to five more wait in the queue.

## Following up

Everything said in a pull request's thread is about that pull request.
"Also say 'all done!' when the list's empty" there:

- goes back to the same branch, as it is on GitHub, so commits you or
  others pushed to it are kept;
- runs the agent with the follow-up and the original request;
- commits, tests (with second goes, as before) and pushes to the same
  branch, which updates the pull request;
- adds a comment to the pull request saying who asked for what, what the
  agent said, and how the tests went;
- answers in the thread: ✅ Updated the pull request.

With Claude Code, a follow-up carries on in the same Claude session, so the
agent remembers what it did and why. Other agents start afresh, with the
original request in their prompt.

Follow-ups work across restarts. codebot keeps its 30 most recent pull
requests (in `codebot-work/jobs.json` in the bot's plugins folder) and
keeps a worktree for each. Older ones are tidied away; ask in the room for
more work on those.

A follow-up doesn't wait for `go`, even with `approve = true`: you're
already talking about that pull request.

## Tests, and second goes

With `test` set, codebot runs the tests after the agent. If they fail, the
agent gets what they printed and another go (with Claude Code, in the same
session), up to `fix_rounds` more times (2 unless you say). Each round is a
commit of its own. The pull request then says "tests passed after 2
rounds", or that they still fail. A pull request whose tests still fail is
opened anyway, marked ⚠️, for you to judge. `fix_rounds = 0` turns second
goes off.

The tests run with your environment, in codebot's worktree, for up to 20
minutes.

## Several repositories

One repository is the main one (`repo`), for every room. More go under
`repos`, each with the rooms it's the one for:

```toml
[plugins.codebot.settings]
repo = "~/code/website"
base = "main"
test = "npm test"

[plugins.codebot.settings.repos.api]
path = "~/code/api"
base = "main"
test = "cargo test"
rooms = ["!backend:example.org"]
```

In a room an entry lists, that repository is the one meant. Anywhere else,
it's the main one. `codebot in api: …` picks one anywhere. With several
and no main one, a room that no entry lists has to say which. `/codebot start <checkout>` in
a room adds the checkout there; repositories are named after their folders.

## Watching it work

Its status message in the thread changes as it goes, about every 15
seconds: how long it's been, which round, and what it's doing. With Claude
Code that's each step (✏️ editing todo.py, 📖 reading, 🔎 looking for,
▶️ running, 🗒️ planning) and how many files it has changed so far. With
other agents, it's the last line they printed. `codebot: status` says the
same in the room.

The end says how long it took and, with Claude Code, what it cost (Claude
Code's own figure: on a claude.ai plan it's what the work would have cost,
not what you pay).

## Agents

| `agent` | Runs | Follow-ups remember | Steps and cost |
|---|---|---|---|
| `"claude"` (the default) | `claude -p … --permission-mode acceptEdits --output-format stream-json` | ✓ (its session) | ✓ |
| `"codex"` | `codex exec --full-auto …` | (starts afresh) | (its last lines) |
| `"aider"` | `aider --yes-always --no-auto-commits --message …` | (starts afresh) | (its last lines) |
| `["my-agent", "--task", "{prompt}"]` | your command, `{prompt}` replaced by the request | (starts afresh) | (its last lines) |

The request always reaches the agent as one argument, never through a
shell. A command of your own that writes Claude Code's stream-json (a JSON
object a line) gets steps and cost too.

Claude Code with `acceptEdits` edits files in its worktree without asking,
and asks before running commands. Run headless as codebot runs it, nobody
answers, so it doesn't run them; codebot runs the tests itself. Codex's
`--full-auto` runs commands in its own sandbox.

## Settings

In the bot's config.toml (`~/.textrix/bots/codebot/config.toml`), under
`[plugins.codebot.settings]`. `/codebot start` writes the first ones; change
the rest by hand, then `/codebot start` (or `/codebot stop` and `/codebot
start`) to restart it with them.

| Setting | Default | |
|---|---|---|
| `repo` | (none) | the main repository: a git checkout whose `origin` is on GitHub |
| `base` | `"main"` | its branch that branches start from and pull requests go to |
| `test` | (none) | its tests: a command, e.g. `"cargo test"` (or a list of arguments) |
| `repos.<name>` | | more repositories: `path`, `base`, `test`, and `rooms` they're the one for |
| `agent` | `"claude"` | `"claude"`, `"codex"`, `"aider"`, or a command with `"{prompt}"` in it |
| `allow` | you | the Matrix IDs that may give it work |
| `approve` | `false` | wait for `go` in the thread before starting a new task |
| `fix_rounds` | `2` | more goes the agent gets at failing tests |
| `max_minutes` | `30` | per task, all rounds together |
| `max_per_day` | `20` | tasks a day, follow-ups too |
| `git_name`, `git_email` | `codebot`, `codebot@users.noreply.github.com` | who its commits are by |

And in `[plugins.codebot]` itself: `rooms`, the rooms it hears and posts
in. `/codebot start` adds the room you're in.

## Safety

What codebot does:

- Only people in `allow` can give it work.
- The request reaches the agent as one argument, never through a shell.
- The agent edits files in a worktree of its own. codebot does the
  committing, and pushes only its own `codebot/…` branch. It never pushes
  to `base`, and never merges.
- Time limits per task, a cap per day, a queue of five.
- A pull request is a proposal: read it before merging.

What it can't do: stop an agent from reading what your account can read,
or from being misled by what's in the repository (instructions planted in
a file it reads, say). So:

- Try it on a repository you don't mind first.
- Run the bot as a macOS or Linux user (or on a machine) with nothing else
  worth taking.
- Give it a token for those repositories only: a
  [fine-grained token](https://github.com/settings/personal-access-tokens)
  with contents and pull requests access to them, in the bot's environment
  as `GH_TOKEN`. That's what `gh` uses. For git to push with it too, the
  checkout's `origin` has to be https (not ssh), with `gh auth setup-git`
  run once, so git asks `gh` for the token.
- Keep `allow` to people you'd let push a branch.

## Troubleshooting

**It doesn't answer.**
- Check `/codebot`: is the bot running, is the room among its rooms, are
  you in `allow`?
- Check `/codebot log` for errors. "not taking work: …" says what's
  missing.
- In an encrypted room, the bot needs the room's keys. If attaching to it
  (`textrix bot attach codebot`) shows your message as one it can't
  decrypt, send it again, now that it's in the room.
- Try `!code …`: it needs no name.

**"claude isn't installed (or isn't on PATH)".** The bot has the
environment of what started it. Start it from a Textrix whose shell finds
`claude`: `/codebot stop`, then `/codebot start`.

**The push or the pull request fails.** It says why in the thread. Check
that git can push to `origin` from that checkout, and `gh auth status`, in
the environment the bot runs in.

**A follow-up started a new pull request.** Only replies in the pull
request's thread are follow-ups. Older pull requests (past its 30 most
recent) are forgotten; ask in the room again.

**Claude Code bills an API key.** See the note on the bot's environment in
[Setting it up](#setting-it-up).

## Limits

- One task at a time, on the machine the bot runs on.
- GitHub only, for pull requests (`gh`). Without `gh` it pushes the branch
  and says so.
- Planned: running the agent elsewhere (over ssh, or a cloud agent that
  opens the pull request itself), with codebot handing over the work and
  reporting back.
