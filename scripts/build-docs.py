#!/usr/bin/env python3
"""Builds the Pages site: site/ as it is, plus docs/ rendered with mdBook.

    scripts/build-docs.py OUT

OUT gets site/'s files, the docs under OUT/docs/ (HTML, and each page's
Markdown at the same path with .md), and OUT/llms.txt and OUT/llms-full.txt
for agents. Links that leave the docs (the changelog, examples/, the plan)
become links to the file on GitHub. Fails on a link to a file or heading
that doesn't exist. Needs mdbook on PATH. See docs/spec/setup.md.
"""

import pathlib
import re
import shutil
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
DOCS = ROOT / "docs"
SITE = ROOT / "site"
BASE = "https://ermag.github.io/rowbit"
GITHUB = "https://github.com/Ermag/rowbit"

LINK = re.compile(r"(\]\()([^)\s]+)(\))")
HEADING = re.compile(r"^(#{1,6})\s+(.*?)\s*#*\s*$")
FENCE = re.compile(r"^\s*(```|~~~)")

# One line per page for llms.txt; a page not here gets its first sentence.
DESCRIPTIONS = {
    "quickstart.md": "Install Rowbit, start the demo shop database, and watch it let a small fix through, hold a risky write, refuse a delete and undo a change.",
    "connect.md": "Set up Claude Code, Cursor, VS Code, Claude Desktop, Codex, Gemini CLI or Windsurf with `rowbit connect`, the Rowbit skill, and stdio versus HTTP.",
    "spec/policy.md": "The rowbit.yml schema: agents, rules (when/then), scopes the database enforces, and shadow mode.",
    "spec/change-lifecycle.md": "How a change moves from propose through preview, decide, apply and undo; the agent tools; reviewing in the CLI and web UI; the audit log.",
    "spec/server.md": "rowbit-server.yml: sign-in through an identity provider, notifications, MCP over HTTP with per-agent tokens, and agents' database users.",
    "spec/setup.md": "The releases and install script, `rowbit demo`, `rowbit connect`, review links, the skill, the home page and this site.",
    "privileges.md": "The exact grants Rowbit's database user needs, agents' own users, and protecting the SQLite state file.",
    "operating.md": "What happens when Rowbit is down, upgrading, how long data is kept, and rotating secrets.",
}


def pages():
    """The pages in SUMMARY.md, in sidebar order, with their titles."""
    out = []
    for line in (DOCS / "SUMMARY.md").read_text().splitlines():
        m = re.search(r"\[([^\]]+)\]\(([^)]+\.md)\)", line)
        if m:
            out.append((m.group(1), m.group(2)))
    return out


def slug(text):
    text = re.sub(r"`|\*|_(?=\w)|(?<=\w)_|\[|\]\([^)]*\)", "", text)
    text = text.strip().lower()
    text = re.sub(r"[^\w\- ]", "", text)
    return text.replace(" ", "-")


def anchors(path):
    seen = {}
    out = set()
    in_code = False
    for line in path.read_text().splitlines():
        if FENCE.match(line):
            in_code = not in_code
            continue
        m = None if in_code else HEADING.match(line)
        if m:
            s = slug(m.group(2))
            n = seen.get(s, 0)
            seen[s] = n + 1
            out.add(s if n == 0 else f"{s}-{n}")
    return out


def rewrite(page, text, on_site, errors):
    """The page's Markdown with links that leave the site pointing at GitHub."""
    src = DOCS / page
    lines = text.splitlines(keepends=True)
    in_code = False
    for i, line in enumerate(lines):
        if FENCE.match(line):
            in_code = not in_code
            continue
        if in_code:
            continue

        def fix(m):
            target = m.group(2)
            if re.match(r"^[a-z][a-z0-9+.-]*:", target) or target.startswith("<"):
                return m.group(0)
            path, _, anchor = target.partition("#")
            dest = (src.parent / path).resolve() if path else src.resolve()
            try:
                rel = dest.relative_to(ROOT)
            except ValueError:
                errors.append(f"{page}: {target} leaves the repository")
                return m.group(0)
            if not dest.exists():
                errors.append(f"{page}: {target} doesn't exist")
                return m.group(0)
            if anchor and dest.suffix == ".md" and anchor not in anchors(dest):
                errors.append(f"{page}: no heading #{anchor} in {rel}")
            if dest.is_file() and dest.suffix == ".md":
                try:
                    in_docs = dest.relative_to(DOCS).as_posix()
                except ValueError:
                    in_docs = None
                if in_docs in on_site:
                    return m.group(0)
            kind = "tree" if dest.is_dir() else "blob"
            url = f"{GITHUB}/{kind}/main/{rel.as_posix()}"
            if anchor:
                url += f"#{anchor}"
            return f"{m.group(1)}{url}{m.group(3)}"

        lines[i] = LINK.sub(fix, line)
    return "".join(lines)


def summary_line(text):
    """The page's first sentence, for llms.txt."""
    in_code = False
    for para in re.split(r"\n\s*\n", text):
        p = para.strip()
        if not p or p.startswith("#") or p.startswith("Status:") or p.startswith("|"):
            continue
        if FENCE.match(p):
            continue
        p = re.sub(r"\[([^\]]+)\]\([^)]*\)", r"\1", " ".join(p.split()))
        m = re.match(r"(.+?[.!?])(\s|$)", p)
        return m.group(1) if m else p
    return ""


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    out = pathlib.Path(sys.argv[1]).resolve()
    if out.exists():
        shutil.rmtree(out)
    shutil.copytree(SITE, out)
    build = out.parent / (out.name + ".docs-src")
    if build.exists():
        shutil.rmtree(build)
    build.mkdir(parents=True)

    listed = pages()
    on_site = {p for _, p in listed}
    errors = []
    markdown = {}
    for _, page in listed:
        src = DOCS / page
        if not src.exists():
            errors.append(f"SUMMARY.md: {page} doesn't exist")
            continue
        md = rewrite(page, src.read_text(), on_site, errors)
        markdown[page] = md
        dest = build / "src" / page
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_text(
            md.rstrip("\n")
            # Absolute, or mdBook would turn it into a link to the HTML.
            + f'\n\n---\n\n<p class="rowbit-md"><a href="{BASE}/docs/{page}">This page as Markdown</a>'
            + f' · <a href="{BASE}/llms.txt">llms.txt</a></p>\n'
        )
    if errors:
        sys.exit("broken links:\n  " + "\n  ".join(errors))
    shutil.copy(DOCS / "SUMMARY.md", build / "src" / "SUMMARY.md")
    shutil.copy(DOCS / "book.toml", build / "book.toml")

    subprocess.run(["mdbook", "build", str(build), "-d", str(out / "docs")], check=True)
    for page, md in markdown.items():
        (out / "docs" / page).write_text(md)
    shutil.rmtree(build)

    index = [
        "# Rowbit",
        "",
        "> A safety net for AI agents that write to your database. Agents propose writes "
        "over MCP; Rowbit previews each one on a copy, applies it, holds it for a person, "
        "or refuses it by the rules in rowbit.yml, and can undo any single change. "
        "MariaDB and MySQL. Open source (AGPL-3.0).",
        "",
        "Try it with Docker: `curl -fsSL https://ermag.github.io/rowbit/install.sh | sh`, "
        "then `rowbit demo`. For a real database, a person sets Rowbit up: an agent must "
        "not hold the database's credentials or edit its own rules.",
        "",
        "## Docs",
        "",
    ]
    for title, page in listed:
        line = DESCRIPTIONS.get(page) or summary_line(markdown[page])
        index.append(f"- [{title}]({BASE}/docs/{page}): {line}")
    index += [
        "",
        "## Optional",
        "",
        f"- [Changelog]({GITHUB}/blob/main/CHANGELOG.md)",
        f"- [Security]({GITHUB}/blob/main/SECURITY.md)",
        f"- [Source]({GITHUB})",
        "",
    ]
    (out / "llms.txt").write_text("\n".join(index))
    full = [f"<!-- {BASE}/docs/{page} -->\n\n{markdown[page].strip()}\n" for _, page in listed]
    (out / "llms-full.txt").write_text("\n\n".join(full))
    print(f"built {len(listed)} pages into {out}")


if __name__ == "__main__":
    main()
