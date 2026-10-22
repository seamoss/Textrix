#!/usr/bin/env python3
"""codebot: coding requests from a Matrix room, done by a coding agent on a
branch of their own, back as pull requests.

Run it in a bot (`textrix bot new codebot`), so it talks as itself. Then,
attached to the bot, in the room: `/plugin new codebot codebot`, and fill
in its settings in the bot's config.toml:

    [plugins.codebot.settings]
    repo = "~/code/project"          # a git checkout with an `origin` on GitHub
    base = "main"                    # branches start here; PRs go here
    agent = "claude"                 # "claude", "codex", "aider", or a command:
                                     # ["my-agent", "--task", "{prompt}"]
    allow = ["@you:example.org"]     # who may give it work (Matrix IDs)
    test = "cargo test"              # optional: run after the agent
    approve = false                  # true: wait for "go" in the thread first
    max_minutes = 30                 # per task
    max_per_day = 20

In the room: "codebot: make the login form remember the user name" (or
"!code …"). It answers in a thread, works on `codebot/<…>`, and posts the
pull request. "stop" in the thread (or "codebot: stop") ends a task;
"codebot: status" says what it's doing.

What keeps it safe: only people in `allow` can give it work; the request
reaches the agent as one argument, never through a shell; the agent only
edits files (codebot commits, and pushes only its own branch); it never
merges anything, and never pushes to `base`. A pull request is a proposal:
read it before merging. Run the bot as a user (or on a machine) with
nothing else worth taking, and give `gh` a token for this repository only.
"""

import collections
import datetime
import os
import queue
import re
import shlex
import shutil
import signal
import subprocess
import tempfile
import threading
import time

import textrix

bot = textrix.Plugin("takes coding requests to a coding agent and opens pull requests")

AGENTS = {
    # Edits files without asking; codebot commits.
    "claude": ["claude", "-p", "{prompt}", "--permission-mode", "acceptEdits"],
    "codex": ["codex", "exec", "--full-auto", "{prompt}"],
    "aider": ["aider", "--yes-always", "--no-auto-commits", "--message", "{prompt}"],
}

PROMPT = """You're working in a git checkout of {repo}, on a branch of your own.

{nick} asked, in the chat room {room}:

{request}

Make that change in this checkout, keeping to what was asked. Don't commit, \
push or switch branches: that's done for you when you're finished. End with \
a few lines saying what you changed."""

PREFIX = "codebot/"
QUEUE_MAX = 5
PROGRESS_EVERY = 20  # seconds between progress edits
TAIL = 40  # lines of the agent's output kept

# ---- settings ------------------------------------------------------------


class Config:
    def __init__(self, s):
        repo = s.get("repo") or ""
        self.repo = os.path.abspath(os.path.expanduser(repo)) if repo else ""
        self.base = s.get("base", "main")
        agent = s.get("agent", "claude")
        self.agent = AGENTS.get(agent, agent) if isinstance(agent, str) else list(agent)
        if isinstance(self.agent, str):
            self.agent = shlex.split(self.agent)
        self.allow = set(s.get("allow", []))
        test = s.get("test")
        self.test = shlex.split(test) if isinstance(test, str) else (list(test) if test else None)
        self.approve = bool(s.get("approve", False))
        self.max_seconds = int(s.get("max_minutes", 30)) * 60
        self.max_per_day = int(s.get("max_per_day", 20))
        self.git_name = s.get("git_name", "codebot")
        self.git_email = s.get("git_email", "codebot@users.noreply.github.com")
        self.work = os.path.abspath(f"{bot.name or 'codebot'}-work")

    def problems(self):
        """What's wrong with the settings, if anything."""
        if not self.repo:
            return "set repo (a git checkout) in its settings in config.toml, then /reload"
        if not os.path.isdir(os.path.join(self.repo, ".git")):
            return f"repo isn't a git checkout: {self.repo}"
        if not self.agent or "{prompt}" not in self.agent:
            return "agent needs a {prompt} argument"
        if not shutil.which(self.agent[0]):
            return f"{self.agent[0]} isn't installed (or isn't on PATH)"
        if not self.allow:
            return "allow is empty: nobody may give it work"
        return None


cfg = None
problem = None

# ---- tasks ---------------------------------------------------------------


class Task:
    count = 0

    def __init__(self, msg, request):
        Task.count += 1
        self.id = Task.count
        self.room = msg["room"]["id"]
        self.room_name = msg["room"]["name"]
        self.root = msg.get("thread_root") or msg["event_id"]
        self.request_event = msg["event_id"]
        self.sender = msg["sender"]
        self.nick = msg["nick"]
        self.request = request
        words = re.sub(r"[^a-z0-9]+", "-", request.lower()).strip("-")
        self.branch = f"{PREFIX}{words[:30].strip('-') or 'task'}-{int(time.time()) % 100000}"
        self.status_event = None
        self.state = "waiting" if cfg.approve else "queued"
        self.started = None
        self.cancelled = False
        self.proc = None
        self.tail = collections.deque(maxlen=TAIL)
        self.last_edit = 0.0


waiting = []  # tasks queued (or awaiting "go")
current = None
updates = queue.Queue()  # (task, kind, text) from the worker
done_today = (datetime.date.today(), 0)
told_no = {}  # sender -> when they were last told they can't


def say(task, text):
    """In the task's thread. A failure here doesn't stop the work."""
    try:
        return bot.send(task.room, text, thread=task.root)
    except textrix.TextrixError as e:
        bot.log(f"couldn't post in {task.room_name}: {e}", error=True)


def react(task, key):
    try:
        bot.react(task.room, task.request_event, key)
    except textrix.TextrixError:
        pass


@bot.on_start
def started():
    global cfg, problem
    cfg = Config(bot.settings)
    problem = cfg.problems()
    if problem:
        bot.log(f"not taking work: {problem}", error=True)
        bot.status("codebot: needs setting up")
    else:
        bot.status("codebot: ready")


def addressed(msg):
    """The request, if the message is for codebot: it mentions it, or
    starts with !code."""
    body = msg["body"].strip()
    if body.lower().startswith("!code"):
        return body[5:].strip()
    if not msg.get("highlight"):
        return None
    # Its name at the front ("codebot: …", "@codebot:server …") goes.
    names = set()
    for me in bot.me:
        user = me.get("user") or ""
        names.update({user, user.lstrip("@").split(":")[0], me.get("nick") or ""})
    for name in sorted(filter(None, names), key=len, reverse=True):
        if body.lower().startswith(name.lower()):
            return body[len(name):].lstrip(" :,").strip()
    return body


@bot.on_message
def heard(msg):
    global current
    body = msg["body"].strip()
    # In a task's thread: go, or stop.
    root = msg.get("thread_root")
    task = next((t for t in [current, *waiting] if t and root and t.root == root), None)
    if task and msg["sender"] in (cfg.allow if cfg else ()):
        word = body.lower().rstrip(".!")
        if word in ("go", "yes", "ok") and task.state == "waiting":
            task.state = "queued"
            say(task, "Queued." if current else "Starting.")
            return
        if word in ("stop", "cancel"):
            return stop(task)
    request = addressed(msg)
    if request is None:
        return
    if problem:
        bot.send(msg["room"]["id"], f"I'm not set up yet: {problem}", reply_to=msg["event_id"])
        return
    if msg["sender"] not in cfg.allow:
        last = told_no.get(msg["sender"], 0)
        if time.time() - last > 3600:
            told_no[msg["sender"]] = time.time()
            bot.send(msg["room"]["id"], "Sorry, I only take work from the people I've been told to.", reply_to=msg["event_id"])
        return
    word = request.lower().rstrip(".!")
    if word in ("stop", "cancel"):
        if current:
            stop(current)
        return
    if word in ("status", "?"):
        return status(msg)
    if not request or word == "help":
        bot.send(
            msg["room"]["id"],
            "Tell me what to change, e.g. “codebot: make the login form remember the user name”. I work on a "
            "branch of my own and open a pull request. “stop” ends a task; “status” says what I'm doing.",
            reply_to=msg["event_id"],
        )
        return
    global done_today
    day, count = done_today
    if day != datetime.date.today():
        done_today = (datetime.date.today(), 0)
        count = 0
    if count + len(waiting) + (1 if current else 0) >= cfg.max_per_day:
        bot.send(msg["room"]["id"], f"That's {cfg.max_per_day} today, my limit. Tomorrow!", reply_to=msg["event_id"])
        return
    if len(waiting) >= QUEUE_MAX:
        bot.send(msg["room"]["id"], "I've a queue of work already; ask again in a bit.", reply_to=msg["event_id"])
        return
    task = Task(msg, request)
    waiting.append(task)
    react(task, "👀")
    if task.state == "waiting":
        say(task, f"I'd work on `{task.branch}` with {cfg.agent[0]}. Reply “go” here to start, or “stop”.")
    elif current:
        say(task, "Queued: I'll start once the current task is done.")


def status(msg):
    if not current and not waiting:
        text = "Nothing on: ready for work."
    else:
        lines = []
        if current:
            mins = int((time.time() - current.started) // 60)
            lines.append(f"Working on “{current.request[:80]}” for {current.nick}, {mins}m in, on `{current.branch}`.")
        for t in waiting:
            lines.append(f"{'Waiting for go' if t.state == 'waiting' else 'Queued'}: “{t.request[:80]}” for {t.nick}.")
        text = "\n".join(lines)
    bot.send(msg["room"]["id"], text, reply_to=msg["event_id"])


def stop(task):
    global current
    task.cancelled = True
    if task in waiting:
        waiting.remove(task)
        say(task, "Dropped.")
        react(task, "🛑")
    elif task is current and task.proc:
        end(task.proc)


@bot.every(1)
def tick():
    global current, done_today
    # What the worker has to say.
    while True:
        try:
            task, kind, text = updates.get_nowait()
        except queue.Empty:
            break
        if kind == "status":
            if task.status_event:
                try:
                    bot.edit(task.room, task.status_event, text)
                except textrix.TextrixError:
                    pass
            else:
                task.status_event = say(task, text)
        elif kind == "done":
            say(task, text)
            current = None
            bot.status("codebot: ready")
        elif kind == "react":
            react(task, text)
    # Progress, every so often, in place.
    if current and current.status_event and time.time() - current.last_edit > PROGRESS_EVERY:
        current.last_edit = time.time()
        mins = int((time.time() - current.started) // 60)
        last = next((line for line in reversed(current.tail) if line.strip()), "")
        text = f"⏳ Working ({mins}m) on `{current.branch}`. “stop” ends it."
        if last:
            text += f"\n> {last[:200]}"
        try:
            bot.edit(current.room, current.status_event, text)
        except textrix.TextrixError:
            pass
    # Next.
    if not current:
        task = next((t for t in waiting if t.state == "queued"), None)
        if task:
            waiting.remove(task)
            current = task
            task.started = time.time()
            day, count = done_today
            done_today = (day, count + 1)
            bot.status(f"codebot: working for {task.nick}")
            threading.Thread(target=work, args=(task,), daemon=True).start()


# ---- the work (in a thread of its own: no bot.* here) ---------------------


def end(proc):
    """The agent and everything it started."""
    if proc.poll() is None:
        try:
            os.killpg(proc.pid, signal.SIGTERM)
        except OSError:
            pass


def git(*args, cwd=None, check=True):
    out = subprocess.run(["git", *args], cwd=cwd or cfg.repo, capture_output=True, text=True, timeout=600)
    if check and out.returncode != 0:
        raise RuntimeError(f"git {args[0]} failed: {(out.stderr or out.stdout).strip()[-400:]}")
    return out.stdout.strip()


def work(task):
    path = os.path.join(cfg.work, f"task-{task.id}")
    try:
        result = do_work(task, path)
    except Exception as e:  # anything: report it, don't die
        result = f"❌ That didn't work: {e}"
        updates.put((task, "react", "❌"))
    finally:
        if os.path.isdir(path):
            git("worktree", "remove", "--force", path, check=False)
    updates.put((task, "done", result))


def do_work(task, path):
    updates.put((task, "status", f"⏳ Starting on `{task.branch}` with {cfg.agent[0]}. “stop” ends it."))
    os.makedirs(cfg.work, exist_ok=True)
    git("fetch", "origin", cfg.base)
    git("worktree", "add", "-b", task.branch, path, f"origin/{cfg.base}")
    if task.cancelled:
        updates.put((task, "react", "🛑"))
        return "🛑 Stopped before starting. Nothing was pushed."

    prompt = PROMPT.format(repo=os.path.basename(cfg.repo), nick=task.nick, room=task.room_name, request=task.request)
    command = [prompt if a == "{prompt}" else a for a in cfg.agent]
    # In a process group of its own, so stopping it stops what it started.
    task.proc = subprocess.Popen(
        command,
        cwd=path,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        start_new_session=True,
    )
    deadline = time.time() + cfg.max_seconds
    timer = threading.Timer(cfg.max_seconds, end, args=(task.proc,))
    timer.start()
    for line in task.proc.stdout:
        task.tail.append(line.rstrip())
    code = task.proc.wait()
    timer.cancel()
    if task.cancelled:
        updates.put((task, "react", "🛑"))
        return "🛑 Stopped. Nothing was pushed."
    if time.time() >= deadline:
        updates.put((task, "react", "⌛"))
        return f"⌛ {cfg.agent[0]} took more than {cfg.max_seconds // 60} minutes; stopped. Nothing was pushed."
    said = "\n".join(line for line in list(task.tail)[-12:] if line.strip())
    if code != 0:
        updates.put((task, "react", "❌"))
        return f"❌ {cfg.agent[0]} stopped with exit code {code}. Nothing was pushed.\n\n```\n{said[-1500:]}\n```"

    # Its changes, committed by codebot.
    if git("status", "--porcelain", cwd=path):
        git("add", "-A", cwd=path)
        title = task.request.splitlines()[0][:70]
        message = f"{title}\n\nAsked for by {task.nick} ({task.sender}) in {task.room_name}; made by {cfg.agent[0]} via codebot."
        git("-c", f"user.name={cfg.git_name}", "-c", f"user.email={cfg.git_email}", "commit", "-q", "-m", message, cwd=path)
    if not git("log", "--oneline", f"origin/{cfg.base}..HEAD", cwd=path):
        updates.put((task, "react", "🤷"))
        return f"🤷 {cfg.agent[0]} didn't change anything.\n\n```\n{said[-1500:]}\n```"
    stat = git("diff", "--shortstat", f"origin/{cfg.base}...HEAD", cwd=path)

    tests = ""
    if cfg.test:
        updates.put((task, "status", "🧪 Running the tests."))
        run = subprocess.run(cfg.test, cwd=path, capture_output=True, text=True, timeout=1200)
        output = (run.stdout + run.stderr).strip().splitlines()[-15:]
        tests = "passed" if run.returncode == 0 else f"failed (exit {run.returncode})"
        tests_detail = "\n".join(output)

    if task.cancelled:
        updates.put((task, "react", "🛑"))
        return "🛑 Stopped. Nothing was pushed."
    # Only its own branch, never the base.
    if not task.branch.startswith(PREFIX) or task.branch == cfg.base:
        raise RuntimeError(f"won't push {task.branch}")
    git("push", "-q", "-u", "origin", task.branch, cwd=path)

    if not shutil.which("gh"):
        updates.put((task, "react", "✅"))
        return f"✅ Pushed `{task.branch}` ({stat}). gh isn't installed, so no pull request: open one from the branch."
    body = (
        f"Asked for by **{task.nick}** (`{task.sender}`) in {task.room_name}:\n\n"
        + "\n".join("> " + line for line in task.request.splitlines())
        + f"\n\nMade by `{cfg.agent[0]}` via codebot (Textrix). Review before merging.\n\n"
        + f"<details><summary>What {cfg.agent[0]} said</summary>\n\n```\n{said[-3000:]}\n```\n</details>\n"
    )
    if cfg.test:
        body += f"\nTests (`{' '.join(cfg.test)}`): {tests}\n\n```\n{tests_detail[-2000:]}\n```\n"
    with tempfile.NamedTemporaryFile("w", suffix=".md", delete=False) as f:
        f.write(body)
    try:
        title = task.request.splitlines()[0][:70]
        pr = subprocess.run(
            ["gh", "pr", "create", "--base", cfg.base, "--head", task.branch, "--title", title, "--body-file", f.name],
            cwd=path, capture_output=True, text=True, timeout=120,
        )
    finally:
        os.unlink(f.name)
    if pr.returncode != 0:
        updates.put((task, "react", "⚠️"))
        return f"⚠️ Pushed `{task.branch}` ({stat}), but the pull request failed: {pr.stderr.strip()[-300:]}"
    url = pr.stdout.strip().splitlines()[-1] if pr.stdout.strip() else "(no link)"
    updates.put((task, "react", "✅"))
    tested = f"; tests {tests}" if cfg.test else ""
    return f"✅ Pull request: {url}\n{stat}{tested}"


bot.run()
