#!/usr/bin/env python3
"""codebot: coding requests from a Matrix room, done by a coding agent on a
branch of their own, back as pull requests. CODEBOT.md has the whole story.

Set it up from your own Textrix with `/codebot start ~/code/project` in the
room it's for: that puts it in a bot of its own (so it talks as itself)
with settings like these, in the bot's config.toml:

    [plugins.codebot.settings]
    repo = "~/code/project"          # a git checkout with an `origin` on GitHub
    base = "main"                    # branches start here; PRs go here
    test = "cargo test"              # optional: run after the agent
    agent = "claude"                 # "claude", "codex", "aider", or a command:
                                     # ["my-agent", "--task", "{prompt}"]
    allow = ["@you:example.org"]     # who may give it work (Matrix IDs)
    approve = false                  # true: wait for "go" in the thread first
    fix_rounds = 2                   # more goes at failing tests
    max_minutes = 30                 # per task
    max_per_day = 20

    [plugins.codebot.settings.repos.other]   # more repositories
    path = "~/code/other"
    base = "main"
    test = "npm test"
    rooms = ["!room:example.org"]            # where it's the one meant

In the room: "codebot: make the login form remember the user name" (or
"!code …", or "codebot in other: …" for another repository). It answers in
a thread, works on `codebot/<…>`, runs the tests (handing failures back to
the agent), and opens the pull request. More in the thread ("also keep it
for a week") carries on with the same branch and pull request. "stop" in
the thread ends a task; "codebot: status" says what it's doing.

What keeps it safe: only people in `allow` can give it work; the request
reaches the agent as one argument, never through a shell; the agent only
edits files (codebot commits, and pushes only its own branch); it never
merges anything, and never pushes to `base`. A pull request is a proposal:
read it before merging. Run the bot as a user (or on a machine) with
nothing else worth taking, and give `gh` a token for these repositories only.
"""

import collections
import datetime
import json
import os
import queue
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import uuid

import textrix

bot = textrix.Plugin("takes coding requests to a coding agent and opens pull requests")

# The agents it knows. Claude Code reports as it goes (stream-json: what
# it's doing, its answer, the cost), and can pick up its own session again
# for a follow-up; the others are run plainly, their last lines shown.
AGENTS = {
    "claude": {
        # Edits files without asking; codebot commits.
        "command": [
            "claude", "-p", "{prompt}", "--permission-mode", "acceptEdits",
            "--output-format", "stream-json", "--verbose",
        ],
        "new": ["--session-id", "{session}"],
        "resume": ["--resume", "{session}"],
    },
    "codex": {"command": ["codex", "exec", "--full-auto", "{prompt}"]},
    "aider": {"command": ["aider", "--yes-always", "--no-auto-commits", "--message", "{prompt}"]},
}

PROMPT = """You're working in a git checkout of {repo}, on a branch of your own.

{nick} asked, in the chat room {room}:

{request}

Make that change in this checkout, keeping to what was asked. Don't commit, \
push or switch branches: that's done for you when you're finished. End with \
a few lines saying what you changed."""

FOLLOW_UP = """You're working in a git checkout of {repo}, on the branch \
{branch}, which has an open pull request ({pr}). It was made for this request:

{first}

{nick} now asks, in the chat thread about it:

{request}

Make that change too, keeping to what was asked. Don't commit, push or \
switch branches: that's done for you. End with a few lines saying what you \
changed."""

FIX = """The tests (`{test}`) fail after that change. The end of what they \
printed:

{output}

Make them pass, keeping to what was asked: fix the code, or the tests if \
they're what's wrong. Don't commit, push or switch branches. End with a few \
lines saying what you changed."""

HELP = (
    "Tell me what to change, e.g. “codebot: make the login form remember the user name”"
    "{repos}. I work on a branch of my own, run the tests, and open a pull request; "
    "more in its thread carries on with the same one. “stop” in a thread ends a task, "
    "“codebot: status” says what I'm doing."
)

PREFIX = "codebot/"
QUEUE_MAX = 5
PROGRESS_EVERY = 15  # seconds between progress edits
TAIL = 40  # lines of the agent's output kept
MAX_JOBS = 30  # pull requests whose threads still take follow-ups
# Things said in a thread that aren't more work.
THANKS = re.compile(
    r"^(thanks?( you)?|thx|ty|cheers|nice( one)?|great|cool|perfect|awesome|lgtm|ok(ay)?|done|merged|👍|🙏|❤️|🎉|✅)"
    r"[\s!.]*$",
    re.IGNORECASE,
)

# ---- settings ------------------------------------------------------------


class Repo:
    """A checkout it works on."""

    def __init__(self, name, s):
        path = s.get("path") or s.get("repo") or ""
        self.name = name
        self.path = os.path.abspath(os.path.expanduser(path)) if path else ""
        self.base = s.get("base", "main")
        test = s.get("test")
        self.test = shlex.split(test) if isinstance(test, str) else (list(test) if test else None)
        self.rooms = list(s.get("rooms", []))

    def problem(self):
        if not os.path.isdir(os.path.join(self.path, ".git")) and not os.path.isfile(os.path.join(self.path, ".git")):
            return f"{self.name} isn't a git checkout: {self.path}"
        return None


class Config:
    def __init__(self, s):
        self.repos = {}
        # The main one (`repo`), named after its folder; then `repos`.
        if s.get("repo"):
            main = Repo(os.path.basename(os.path.normpath(os.path.expanduser(s["repo"]))).lower(), s)
            self.repos[main.name] = main
        self.main = next(iter(self.repos.values()), None)
        for name, table in (s.get("repos") or {}).items():
            if isinstance(table, dict):
                self.repos[name] = Repo(name, table)
        agent = s.get("agent", "claude")
        if isinstance(agent, str) and agent in AGENTS:
            self.agent_name = agent
            self.agent = AGENTS[agent]
        else:
            command = shlex.split(agent) if isinstance(agent, str) else list(agent)
            self.agent_name = os.path.basename(command[0]) if command else "agent"
            self.agent = {"command": command}
        self.allow = set(s.get("allow", []))
        self.approve = bool(s.get("approve", False))
        self.fix_rounds = max(0, int(s.get("fix_rounds", 2)))
        self.max_seconds = int(s.get("max_minutes", 30)) * 60
        self.max_per_day = int(s.get("max_per_day", 20))
        self.git_name = s.get("git_name", "codebot")
        self.git_email = s.get("git_email", "codebot@users.noreply.github.com")
        self.work = os.path.abspath(f"{bot.name or 'codebot'}-work")

    def problems(self):
        """What's wrong with the settings, if anything."""
        if not self.repos:
            return "set repo (a git checkout) in its settings: /codebot start <repo> in your own Textrix"
        for repo in self.repos.values():
            if repo.problem():
                return repo.problem()
        command = self.agent["command"]
        if not command or "{prompt}" not in command:
            return "agent needs a {prompt} argument"
        if not shutil.which(command[0]):
            return f"{command[0]} isn't installed (or isn't on PATH)"
        if not self.allow:
            return "allow is empty: nobody may give it work"
        return None

    def repo_for(self, room):
        """The repository a room means: one that lists it, else the main one,
        else the only one."""
        for repo in self.repos.values():
            if room["id"] in repo.rooms or room.get("name") in repo.rooms:
                return repo
        if self.main:
            return self.main
        return next(iter(self.repos.values())) if len(self.repos) == 1 else None


cfg = None
problem = None

# ---- jobs: pull requests, and the threads that carry on with them ---------


class Job:
    """A pull request codebot opened: more in its thread works on it."""

    FIELDS = ("root", "room", "repo", "branch", "path", "pr", "session", "first", "updated")

    def __init__(self, **kw):
        for f in self.FIELDS:
            setattr(self, f, kw.get(f))

    def to_json(self):
        return {f: getattr(self, f) for f in self.FIELDS}


jobs = {}  # thread root -> Job


def jobs_file():
    return os.path.join(cfg.work, "jobs.json")


def load_jobs():
    jobs.clear()
    try:
        with open(jobs_file()) as f:
            for item in json.load(f):
                job = Job(**item)
                jobs[job.root] = job
    except (OSError, ValueError, TypeError):
        pass


def save_job(job):
    """Keep it, and forget (and tidy away) the oldest past MAX_JOBS."""
    jobs[job.root] = job
    for old in sorted(jobs.values(), key=lambda j: j.updated or 0)[:-MAX_JOBS]:
        del jobs[old.root]
        repo = cfg.repos.get(old.repo)
        if repo and old.path and os.path.isdir(old.path):
            subprocess.run(["git", "worktree", "remove", "--force", old.path], cwd=repo.path, capture_output=True)
    os.makedirs(cfg.work, exist_ok=True)
    tmp = jobs_file() + ".tmp"
    with open(tmp, "w") as f:
        json.dump([j.to_json() for j in jobs.values()], f, indent=1)
    os.replace(tmp, jobs_file())


# ---- tasks ---------------------------------------------------------------


class Task:
    count = 0

    def __init__(self, msg, request, repo, root=None):
        Task.count += 1
        self.id = Task.count
        self.room = msg["room"]["id"]
        self.room_name = msg["room"]["name"]
        # A follow-up stays in its job's thread.
        self.root = root or msg.get("thread_root") or msg["event_id"]
        self.follow_up = root is not None
        self.request_event = msg["event_id"]
        self.sender = msg["sender"]
        self.nick = msg["nick"]
        self.request = request
        self.repo = repo
        self.job = None  # found when it starts, for a follow-up
        self.path = None  # its worktree
        words = re.sub(r"[^a-z0-9]+", "-", request.lower()).strip("-")
        if len(words) > 30:
            words = words[:31].rsplit("-", 1)[0] if "-" in words[:31] else words[:30]
        self.branch = f"{PREFIX}{words.strip('-') or 'task'}-{int(time.time()) % 100000}"
        self.session = None
        self.resume = False
        self.status_event = None
        self.state = "waiting" if cfg.approve and not self.follow_up else "queued"
        self.started = None
        self.deadline = None
        self.cancelled = False
        self.proc = None
        self.tail = collections.deque(maxlen=TAIL)
        self.last_edit = 0.0
        # What the agent's doing, from its stream: the latest step, the
        # files it's changed, its answer, and what it cost.
        self.activity = ""
        self.edited = set()
        self.answer = None
        self.cost = 0.0
        self.round = 1


waiting = []  # tasks queued (or awaiting "go")
current = None
updates = queue.Queue()  # (task, kind, payload) from the worker
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
        load_jobs()
        bot.status("codebot: ready")


def addressed(msg):
    """The request, if the message is for codebot: it starts with its name
    ("codebot: …", "@codebot:server …") or !code, or mentions it."""
    body = msg["body"].strip()
    if body.lower().startswith("!code"):
        return body[5:].strip()
    # Its name at the front goes. A name typed out rather than completed
    # isn't a mention to the server (whose old "contains my name" rules
    # don't apply once a message says whom it mentions), so the name
    # counts on its own.
    names = set()
    for me in bot.me:
        user = me.get("user") or ""
        names.update({user, user.lstrip("@").split(":")[0], me.get("nick") or ""})
    for name in sorted(filter(None, names), key=len, reverse=True):
        rest = body[len(name):]
        if body.lower().startswith(name.lower()) and (not rest or rest[0] in " :,"):
            return rest.lstrip(" :,").strip()
    return body if msg.get("highlight") else None


def which_repo(request):
    """ "in <repo>: …" at the front picks a repository: (it, the rest)."""
    m = re.match(r"in\s+([\w.-]+)\s*[:,]?\s+(.+)", request, re.S | re.I)
    if m and m.group(1).lower() in {n.lower() for n in cfg.repos}:
        name = next(n for n in cfg.repos if n.lower() == m.group(1).lower())
        return cfg.repos[name], m.group(2).strip()
    return None, request


@bot.on_message
def heard(msg):
    global current
    body = msg["body"].strip()
    root = msg.get("thread_root")
    allowed = cfg is not None and msg["sender"] in cfg.allow
    # In a task's thread: go, or stop.
    task = next((t for t in [current, *waiting] if t and root and t.root == root), None)
    if task and allowed:
        word = body.lower().rstrip(".!")
        if word in ("go", "yes", "ok") and task.state == "waiting":
            task.state = "queued"
            say(task, "Queued." if current else "Starting.")
            return
        if word in ("stop", "cancel"):
            return stop(task)
    request = addressed(msg)
    # In the thread of its work, more needn't name it (a thank-you isn't more).
    in_its_thread = bool(root) and (root in jobs or task is not None)
    if request is None and in_its_thread and allowed and not THANKS.match(body):
        request = body
    if request is None:
        return
    if in_its_thread and task is None and request.lower().rstrip(".!") in ("stop", "cancel", "go", "yes"):
        return  # nothing running in this thread
    if problem:
        bot.send(msg["room"]["id"], f"I'm not set up yet: {problem}", reply_to=msg["event_id"])
        return
    if not allowed:
        last = told_no.get(msg["sender"], 0)
        if time.time() - last > 3600:
            told_no[msg["sender"]] = time.time()
            bot.send(msg["room"]["id"], "Sorry, I only take work from the people I've been told to.", reply_to=msg["event_id"])
        return
    word = request.lower().rstrip(".!?")
    if word in ("stop", "cancel"):
        if current:
            stop(current)
        return
    if word in ("status", ""):
        return status(msg) if word else help_text(msg)
    if word == "help":
        return help_text(msg)
    if word in ("repos", "repositories"):
        return list_repos(msg)
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
    if in_its_thread:
        # More for the pull request this thread is about.
        job = jobs.get(root)
        repo = cfg.repos.get(job.repo) if job else (task.repo if task else None)
        if repo is None:
            bot.send(msg["room"]["id"], "That repository isn't in my settings any more.", reply_to=msg["event_id"])
            return
        task = Task(msg, request, repo, root=root)
    else:
        repo, request = which_repo(request)
        repo = repo or cfg.repo_for(msg["room"])
        if repo is None:
            names = ", ".join(cfg.repos)
            bot.send(msg["room"]["id"], f"Which repository? “codebot in <name>: …”, with one of {names}.", reply_to=msg["event_id"])
            return
        task = Task(msg, request, repo)
    waiting.append(task)
    react(task, "👀")
    if task.state == "waiting":
        say(task, f"I'd work on `{task.branch}` in {repo.name} with {cfg.agent_name}. Reply “go” here to start, or “stop”.")
    elif current:
        say(task, "Queued: I'll start once the current task is done.")


def help_text(msg):
    repos = ""
    if len(cfg.repos) > 1:
        repos = f", or “codebot in {next(iter(cfg.repos))}: …” for one of {', '.join(cfg.repos)}"
    bot.send(msg["room"]["id"], HELP.format(repos=repos), reply_to=msg["event_id"])


def list_repos(msg):
    lines = []
    for repo in cfg.repos.values():
        where = ""
        if repo.rooms:
            where = " (this room's)" if msg["room"]["id"] in repo.rooms or msg["room"]["name"] in repo.rooms else ""
        elif repo is cfg.main:
            where = " (the main one)"
        tests = f", tests `{' '.join(repo.test)}`" if repo.test else ""
        lines.append(f"{repo.name}{where}: {repo.path}, pull requests to {repo.base}{tests}")
    bot.send(msg["room"]["id"], "\n".join(lines), reply_to=msg["event_id"])


def status(msg):
    if not current and not waiting:
        text = "Nothing on: ready for work."
    else:
        lines = []
        if current:
            mins = int((time.time() - current.started) // 60)
            doing = f" ({current.activity})" if current.activity else ""
            lines.append(
                f"Working on “{current.request[:80]}” for {current.nick} in {current.repo.name}, {mins}m in{doing}."
            )
        for t in waiting:
            lines.append(f"{'Waiting for go' if t.state == 'waiting' else 'Queued'}: “{t.request[:80]}” for {t.nick}.")
        text = "\n".join(lines)
    bot.send(msg["room"]["id"], text, reply_to=msg["event_id"])


def stop(task):
    task.cancelled = True
    if task in waiting:
        waiting.remove(task)
        say(task, "Dropped.")
        react(task, "🛑")
    elif task is current and task.proc:
        end(task.proc)


def progress(task):
    """The status message while it works: how long, which round, and what
    the agent's doing (or last said)."""
    mins = int((time.time() - task.started) // 60)
    where = f"`{task.branch}` in {task.repo.name}"
    head = f"⏳ Working ({mins}m) on {where}"
    if task.round > 1:
        head += f", round {task.round}: fixing the tests"
    lines = [head + ". “stop” ends it."]
    if task.activity:
        changed = f" · {len(task.edited)} file{'s' if len(task.edited) != 1 else ''} changed" if task.edited else ""
        lines.append(f"{task.activity}{changed}")
    else:
        last = next((line for line in reversed(task.tail) if line.strip()), "")
        if last:
            lines.append(f"> {last[:200]}")
    return "\n".join(lines)


@bot.every(1)
def tick():
    global current, done_today
    # What the worker has to say.
    while True:
        try:
            task, kind, payload = updates.get_nowait()
        except queue.Empty:
            break
        if kind == "status":
            if task.status_event:
                try:
                    bot.edit(task.room, task.status_event, payload)
                except textrix.TextrixError:
                    pass
            else:
                task.status_event = say(task, payload)
            task.last_edit = time.time()
        elif kind == "job":
            save_job(payload)
        elif kind == "done":
            say(task, payload)
            current = None
            bot.status("codebot: ready")
        elif kind == "react":
            react(task, payload)
    # Progress, every so often, in place.
    if current and current.status_event and time.time() - current.last_edit > PROGRESS_EVERY:
        current.last_edit = time.time()
        try:
            bot.edit(current.room, current.status_event, progress(current))
        except textrix.TextrixError:
            pass
    # Next.
    if not current:
        task = next((t for t in waiting if t.state == "queued"), None)
        if task:
            waiting.remove(task)
            current = task
            task.started = time.time()
            task.deadline = task.started + cfg.max_seconds
            task.job = jobs.get(task.root) if task.follow_up else None
            if task.job:
                task.branch = task.job.branch
            day, count = done_today
            done_today = (day, count + 1)
            bot.status(f"codebot: working for {task.nick}")
            threading.Thread(target=work, args=(task,), daemon=True).start()


# ---- the work (in a thread of its own: no bot.* here) ---------------------


def fenced(text):
    """`text` as a code block whose fence no run of backticks in it closes
    (an agent's answer often has code blocks of its own)."""
    longest = max((len(run) for run in re.findall(r"`+", text)), default=0)
    fence = "`" * max(3, longest + 1)
    return f"{fence}\n{text}\n{fence}"


def headline(request, most=70):
    """The request's first line, cut at a word if it's long."""
    line = request.strip().splitlines()[0].strip() if request.strip() else "codebot"
    if len(line) <= most:
        return line
    cut = line[: most - 1]
    cut = cut.rsplit(" ", 1)[0] if " " in cut else cut
    return cut.rstrip(" ,.;:-") + "…"


def took(seconds):
    m, s = divmod(int(seconds), 60)
    return f"{m}m{s:02d}s" if m else f"{s}s"


def end(proc):
    """The agent and everything it started."""
    if proc.poll() is None:
        try:
            os.killpg(proc.pid, signal.SIGTERM)
        except OSError:
            pass


def git(repo, *args, cwd=None, check=True):
    out = subprocess.run(["git", *args], cwd=cwd or repo.path, capture_output=True, text=True, timeout=600)
    if check and out.returncode != 0:
        raise RuntimeError(f"git {args[0]} failed: {(out.stderr or out.stdout).strip()[-400:]}")
    return out.stdout.strip()


def describe(name, args, path):
    """One of Claude Code's steps, for the status message."""
    args = args if isinstance(args, dict) else {}
    target = args.get("file_path") or args.get("notebook_path") or args.get("path") or ""
    if target.startswith(path):
        target = os.path.relpath(target, path)
    if name in ("Edit", "MultiEdit", "NotebookEdit"):
        return f"✏️ editing {target}"
    if name == "Write":
        return f"📝 writing {target}"
    if name == "Read":
        return f"📖 reading {target}"
    if name in ("Grep", "Glob", "LS"):
        what = args.get("pattern") or target
        return f"🔎 looking for {what}"[:120] if what else "🔎 looking around"
    if name == "Bash":
        return f"▶️ running `{(args.get('command') or '')[:60]}`"
    if name == "TodoWrite":
        return "🗒️ planning"
    if name in ("WebFetch", "WebSearch"):
        return "🌐 looking something up"
    if name in ("Task", "Agent"):
        return "🤝 handing part of it off"
    return f"🔧 {name}"


def heard_agent(task, line, path):
    """A line of the agent's output: Claude Code's stream (a JSON object a
    line), or plain text."""
    line = line.rstrip("\n")
    event = None
    if line.startswith("{"):
        try:
            event = json.loads(line)
        except ValueError:
            event = None
    if not isinstance(event, dict) or "type" not in event:
        task.tail.append(line)
        return
    kind = event.get("type")
    if kind == "system" and event.get("session_id"):
        task.session = event["session_id"]
    elif kind == "assistant":
        for block in (event.get("message") or {}).get("content") or []:
            if block.get("type") == "text" and block.get("text", "").strip():
                task.tail.extend(block["text"].strip().splitlines())
            elif block.get("type") == "tool_use":
                task.activity = describe(block.get("name", ""), block.get("input"), path)
                if block.get("name") in ("Edit", "MultiEdit", "Write", "NotebookEdit"):
                    target = (block.get("input") or {}).get("file_path") or ""
                    if target:
                        task.edited.add(os.path.relpath(target, path) if target.startswith(path) else target)
    elif kind == "result":
        if isinstance(event.get("result"), str):
            task.answer = event["result"].strip()
        task.cost += float(event.get("total_cost_usd") or 0)
        if event.get("session_id"):
            task.session = event["session_id"]


def run_agent(task, path, prompt):
    """Run the agent once in `path`: (exit code, what it said, its stderr)."""
    agent = cfg.agent
    command = list(agent["command"])
    if "new" in agent:
        if task.resume and task.session:
            command += agent["resume"]
        else:
            task.session = task.session or str(uuid.uuid4())
            command += agent["new"]
    command = [prompt if a == "{prompt}" else a.replace("{session}", task.session or "") for a in command]
    task.answer = None
    task.activity = ""
    # What it says (stdout) goes in the thread and the pull request; its
    # warnings and the like (stderr) only to the log, unless it fails.
    errors = tempfile.TemporaryFile(mode="w+")
    # In a process group of its own, so stopping it stops what it started.
    task.proc = subprocess.Popen(
        command,
        cwd=path,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=errors,
        text=True,
        start_new_session=True,
    )
    timer = threading.Timer(max(1, task.deadline - time.time()), end, args=(task.proc,))
    timer.start()
    for line in task.proc.stdout:
        heard_agent(task, line, path)
    code = task.proc.wait()
    timer.cancel()
    errors.seek(0)
    warned = errors.read().strip()
    errors.close()
    if warned:
        # Its log (`/codebot log`), not the room.
        print(f"{cfg.agent_name} (task {task.id}) said on stderr:\n{warned[-2000:]}", file=sys.stderr, flush=True)
    said = task.answer or "\n".join(line for line in list(task.tail)[-12:] if line.strip())
    # A session that can't be found again: start afresh, once.
    if code != 0 and task.resume and "conversation" in (warned + said).lower() and "found" in (warned + said).lower():
        task.resume = False
        task.session = None
        return run_agent(task, path, prompt)
    return code, said, warned


def run_tests(task, path):
    """(passed, the end of what they printed)."""
    run = subprocess.run(task.repo.test, cwd=path, capture_output=True, text=True, timeout=1200)
    output = "\n".join((run.stdout + run.stderr).strip().splitlines()[-30:])
    return run.returncode == 0, output


def work(task):
    try:
        result = do_work(task)
    except Exception as e:  # anything: report it, don't die
        result = f"❌ That didn't work: {e}"
        updates.put((task, "react", "❌"))
    # A worktree with nothing pushed from it goes; one behind a pull
    # request stays, for follow-ups.
    if task.path and os.path.isdir(task.path) and not task.job:
        git(task.repo, "worktree", "remove", "--force", task.path, check=False)
        git(task.repo, "branch", "-D", task.branch, check=False)
    updates.put((task, "done", result))


def worktree(task):
    """Where the task works: a new branch from the base, or the pull
    request's branch as it is on GitHub (with anything pushed to it since)."""
    repo, job = task.repo, task.job
    os.makedirs(os.path.join(cfg.work, repo.name), exist_ok=True)
    if job:
        git(repo, "fetch", "-q", "origin", job.branch)
        path = job.path if job.path and os.path.isdir(job.path) else os.path.join(cfg.work, repo.name, job.branch.replace("/", "-"))
        if not os.path.isdir(path):
            git(repo, "worktree", "prune", check=False)
            if git(repo, "branch", "--list", job.branch):
                git(repo, "worktree", "add", "-q", path, job.branch)
            else:
                git(repo, "worktree", "add", "-q", "-b", job.branch, path, f"origin/{job.branch}")
        git(repo, "reset", "-q", "--hard", f"origin/{job.branch}", cwd=path)
        git(repo, "clean", "-q", "-fd", cwd=path)
        return path
    git(repo, "fetch", "-q", "origin", repo.base)
    path = os.path.join(cfg.work, repo.name, task.branch.replace("/", "-"))
    git(repo, "worktree", "add", "-q", "-b", task.branch, path, f"origin/{repo.base}")
    return path


def do_work(task):
    repo, job = task.repo, task.job
    verb = "Back on" if job else "Starting on"
    updates.put((task, "status", f"⏳ {verb} `{task.branch}` in {repo.name} with {cfg.agent_name}. “stop” ends it."))
    path = task.path = worktree(task)
    if task.cancelled:
        updates.put((task, "react", "🛑"))
        return "🛑 Stopped before starting. Nothing was pushed."
    before = git(repo, "rev-parse", "HEAD", cwd=path)
    if job:
        task.session, task.resume = job.session, bool(job.session)
        prompt = FOLLOW_UP.format(
            repo=os.path.basename(repo.path), branch=job.branch, pr=job.pr, first=job.first,
            nick=task.nick, request=task.request,
        )
    else:
        prompt = PROMPT.format(repo=os.path.basename(repo.path), nick=task.nick, room=task.room_name, request=task.request)

    said, tests, test_output = "", None, ""
    while True:
        code, said, warned = run_agent(task, path, prompt)
        if task.cancelled:
            updates.put((task, "react", "🛑"))
            return "🛑 Stopped. Nothing was pushed." if not job else "🛑 Stopped. The pull request is as it was."
        if time.time() >= task.deadline:
            updates.put((task, "react", "⌛"))
            return f"⌛ {cfg.agent_name} took more than {cfg.max_seconds // 60} minutes; stopped. Nothing was pushed."
        if code != 0:
            updates.put((task, "react", "❌"))
            why = "\n".join(filter(None, [said, warned[-800:]]))
            return f"❌ {cfg.agent_name} stopped with exit code {code}. Nothing was pushed.\n\n" + fenced(why[-1500:])
        # Its changes, committed by codebot: a commit a round.
        changed = bool(git(repo, "status", "--porcelain", cwd=path))
        if changed:
            git(repo, "add", "-A", cwd=path)
            title = headline(task.request) if task.round == 1 else f"Make the tests pass (round {task.round})"
            message = f"{title}\n\nAsked for by {task.nick} ({task.sender}) in {task.room_name}; made by {cfg.agent_name} via codebot."
            git(repo, "-c", f"user.name={cfg.git_name}", "-c", f"user.email={cfg.git_email}", "commit", "-q", "-m", message, cwd=path)
        if not repo.test or (task.round > 1 and not changed):
            break
        updates.put((task, "status", f"🧪 Running the tests on `{task.branch}`."))
        passed, test_output = run_tests(task, path)
        tests = passed
        if passed or task.round > cfg.fix_rounds:
            break
        task.round += 1
        updates.put((task, "status", f"🧪 The tests failed; round {task.round} of {cfg.fix_rounds + 1}: {cfg.agent_name} is fixing them."))
        prompt = FIX.format(test=" ".join(repo.test), output=test_output[-3000:])
        task.resume = bool(task.session and "new" in cfg.agent)
        if not task.resume:
            prompt = f"{task.request}\n\n{prompt}"

    if git(repo, "rev-parse", "HEAD", cwd=path) == before:
        updates.put((task, "react", "🤷"))
        return f"🤷 {cfg.agent_name} didn't change anything.\n\n" + fenced(said[-1500:])
    stat = git(repo, "diff", "--shortstat", f"{before}..HEAD", cwd=path)
    if task.cancelled:
        updates.put((task, "react", "🛑"))
        return "🛑 Stopped. Nothing was pushed."
    # Only its own branch, never the base.
    if not task.branch.startswith(PREFIX) or task.branch == repo.base:
        raise RuntimeError(f"won't push {task.branch}")
    git(repo, "push", "-q", "-u", "origin", task.branch, cwd=path)

    spent = took(time.time() - task.started)
    cost = f" · ${task.cost:.2f}" if task.cost else ""
    rounds = f" after {task.round} rounds" if task.round > 1 else ""
    tested = ""
    if tests is not None:
        tested = f"; tests passed{rounds}" if tests else f"; tests still fail after {task.round} rounds"
    summary = f"{stat}{tested} · {spent}{cost}"

    if not shutil.which("gh"):
        updates.put((task, "react", "✅"))
        return f"✅ Pushed `{task.branch}` ({summary}). gh isn't installed, so no pull request: open one from the branch."
    details = f"<details><summary>What {cfg.agent_name} said</summary>\n\n{fenced(said[-3000:])}\n</details>\n"
    if tests is not None:
        verdict = f"passed{rounds}" if tests else f"failed, after {task.round} rounds"
        details += f"\nTests (`{' '.join(repo.test)}`): {verdict}\n\n{fenced(test_output[-2000:])}\n"
    details += f"\n{spent}{cost} with `{cfg.agent_name}`.\n"
    quote = "\n".join("> " + line for line in task.request.splitlines())

    if job:
        body = f"**{task.nick}** asked for more in {task.room_name}:\n\n{quote}\n\n{details}"
        out = gh(path, "pr", "comment", job.pr, "--body-file", body=body)
        job.session, job.updated, job.path = task.session, time.time(), path
        updates.put((task, "job", job))
        updates.put((task, "react", "✅" if tests is not False else "⚠️"))
        note = "" if out.returncode == 0 else f" (its comment on the pull request failed: {out.stderr.strip()[-200:]})"
        return f"✅ Updated the pull request: {job.pr}{note}\n{summary}"

    body = (
        f"Asked for by **{task.nick}** (`{task.sender}`) in {task.room_name}:\n\n{quote}\n\n"
        f"Made by `{cfg.agent_name}` via codebot (Textrix). Review before merging; more in the chat thread "
        f"carries on with this pull request.\n\n{details}"
    )
    out = gh(path, "pr", "create", "--base", repo.base, "--head", task.branch, "--title", headline(task.request), "--body-file", body=body)
    if out.returncode != 0:
        updates.put((task, "react", "⚠️"))
        return f"⚠️ Pushed `{task.branch}` ({summary}), but the pull request failed: {out.stderr.strip()[-300:]}"
    url = out.stdout.strip().splitlines()[-1] if out.stdout.strip() else "(no link)"
    task.job = Job(
        root=task.root, room=task.room, repo=repo.name, branch=task.branch, path=path, pr=url,
        session=task.session, first=task.request, updated=time.time(),
    )
    updates.put((task, "job", task.job))
    updates.put((task, "react", "✅" if tests is not False else "⚠️"))
    return f"✅ Pull request: {url}\n{summary}\nMore in this thread carries on with it."


def gh(path, *args, body):
    """`gh …` with `body` in a file for --body-file (its last argument)."""
    with tempfile.NamedTemporaryFile("w", suffix=".md", delete=False) as f:
        f.write(body)
    try:
        return subprocess.run(["gh", *args, f.name], cwd=path, capture_output=True, text=True, timeout=120)
    finally:
        os.unlink(f.name)


bot.run()
