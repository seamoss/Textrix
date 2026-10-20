#!/usr/bin/env python3
"""Build the website: site/ plus a changelog page made from CHANGELOG.md,
with the current version stamped in. Standard library only.

    python3 packaging/site/build.py _site
"""

import html
import re
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def inline(text):
    """`code`, **bold** and [links](url) in already-plain text."""
    out = []
    for n, part in enumerate(text.split("`")):
        if n % 2:
            out.append(f"<code>{html.escape(part)}</code>")
            continue
        part = html.escape(part)
        part = re.sub(r"\*\*(.+?)\*\*", r"<b>\1</b>", part)
        part = re.sub(r"\[([^\]]+)\]\((https?://[^)\s]+)\)", r'<a href="\2">\1</a>', part)
        out.append(part)
    return "".join(out)


def indent_of(line):
    return len(line) - len(line.lstrip(" "))


def bullet_list(lines, i, indent):
    """A list starting at line i whose bullets sit at `indent` spaces."""
    items = []
    while i < len(lines) and lines[i].strip() and indent_of(lines[i]) == indent and lines[i].lstrip().startswith("- "):
        text, parts = [lines[i].strip()[2:]], []
        i += 1
        while i < len(lines) and lines[i].strip() and indent_of(lines[i]) > indent:
            line = lines[i]
            if line.lstrip().startswith("- "):
                parts.append(inline(" ".join(text)))
                text = []
                nested, i = bullet_list(lines, i, indent_of(line))
                parts.append(nested)
            else:
                text.append(line.strip())
                i += 1
        if text:
            parts.append(inline(" ".join(text)))
        items.append("<li>" + " ".join(parts) + "</li>")
    return "<ul>\n" + "\n".join(items) + "\n</ul>", i


def newer(a, b):
    """Whether version a comes after b (1.2.10 after 1.2.9)."""
    return [int(n) for n in a.split(".")] > [int(n) for n in b.split(".")]


def changelog(markdown, released):
    lines, out, i = markdown.splitlines(), [], 0
    while i < len(lines):
        line = lines[i]
        if not line.strip() or line.startswith("# "):
            i += 1
        elif m := re.match(r"## \[([^\]]+)\](?: - (\S+))?", line):
            version, date = m.group(1), m.group(2) or ""
            anchor = "v" + version.replace(".", "-")
            # Pushed, but its builds aren't out yet.
            if newer(version, released):
                date = "on its way"
            out.append(f'<h2 id="{anchor}"><a href="#{anchor}">v{html.escape(version)}</a> <span class="date">{html.escape(date)}</span></h2>')
            i += 1
        elif line.startswith("### "):
            out.append(f"<h3>{inline(line[4:])}</h3>")
            i += 1
        elif line.lstrip().startswith("- "):
            block, i = bullet_list(lines, i, indent_of(line))
            out.append(block)
        else:
            para = []
            while i < len(lines) and lines[i].strip() and not lines[i].startswith("#") and not lines[i].lstrip().startswith("- "):
                para.append(lines[i].strip())
                i += 1
            out.append(f"<p>{inline(' '.join(para))}</p>")
    return "\n".join(out)


def version():
    """The latest release out (its tag), not the version in Cargo.toml,
    which is ahead of it while a release builds. Cargo.toml's where there
    are no tags (a copy without its history)."""
    try:
        tag = subprocess.run(
            ["git", "describe", "--tags", "--abbrev=0", "--match", "v[0-9]*"],
            cwd=ROOT, capture_output=True, text=True, check=True,
        ).stdout.strip()
        return tag.removeprefix("v")
    except (OSError, subprocess.CalledProcessError):
        pass
    for line in (ROOT / "Cargo.toml").read_text().splitlines():
        if m := re.match(r'version = "([^"]+)"', line):
            return m.group(1)
    raise SystemExit("no version in Cargo.toml")


def main(out_dir):
    out = Path(out_dir)
    shutil.rmtree(out, ignore_errors=True)
    shutil.copytree(ROOT / "site", out)
    v = version()
    for page in out.glob("*.html"):
        page.write_text(page.read_text().replace("{{VERSION}}", v))
    template = (Path(__file__).parent / "changelog.html").read_text()
    body = changelog((ROOT / "CHANGELOG.md").read_text(), v)
    (out / "changelog.html").write_text(template.replace("{{VERSION}}", v).replace("{{CHANGELOG}}", body))
    print(f"built {out} for v{v}")


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else "_site")
