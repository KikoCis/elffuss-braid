#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright (c) 2026 UtopiaIA
"""Records the README's terminal demos as asciicast v2 files.

Every command runs for real, in a throwaway directory, and its output is kept
as braid printed it; only that directory's prefix is shortened to ~/demo. The
typing and the pauses are paced for a reader, so the recordings say nothing
about how long anything takes. render.sh turns them into GIFs.

usage: record.py <braid binary> <empty work dir> <out dir>
"""
import json
import os
import random
import subprocess
import sys

BRAID, WORK, OUT = (os.path.realpath(p) for p in sys.argv[1:4])

AGENT = {
    "agent-a": (200, 160, 106),
    "agent-b": (84, 220, 198),
    "agent-c": (130, 114, 255),
}
MUTED = (127, 124, 160)
PINK = (231, 121, 207)
RESET = "\x1b[0m"
WIDTH, HEIGHT = 100, 26
GIT = "git -c user.name='Ada Lovelace' -c user.email=ada@example.org -c init.defaultBranch=main"


def rgb(color):
    return "\x1b[38;2;%d;%d;%dm" % color


class Scene:
    """One recording: a directory, an environment and a clock."""

    def __init__(self, name, root, env, cwd="."):
        self.name = name
        self.root = root
        self.env = env
        self.cwd = cwd
        self.t = 0.0
        self.events = []
        self.rng = random.Random(name)

    def emit(self, text, dt=0.0):
        self.t += dt
        self.events.append([round(self.t, 3), "o", text])

    def prompt(self, who):
        if who:
            return f"{rgb(AGENT[who])}[{who}]{RESET} {rgb(MUTED)}${RESET} "
        return f"{rgb(MUTED)}${RESET} "

    def note(self, text):
        self.emit(rgb(MUTED) + "# ", 0.5)
        for ch in text:
            self.emit(ch, 0.014 + self.rng.random() * 0.012)
        self.emit(RESET + "\r\n", 0.25)

    def quiet(self, cmd, who=None, cwd=None):
        """Runs a command off camera."""
        env = dict(self.env)
        if who:
            env["BRAID_AGENT"] = who
        res = subprocess.run(
            ["bash", "-c", cmd],
            cwd=os.path.join(self.root, cwd or self.cwd),
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        return res.returncode, res.stdout.decode()

    def run(self, cmd, who=None, cwd=None, hold=None):
        """Types a command, runs it and shows what it printed."""
        self.emit(self.prompt(who), 0.45)
        for ch in cmd:
            self.emit(ch, 0.02 + self.rng.random() * 0.028)
        self.emit("\r\n", 0.35)
        code, out = self.quiet(cmd, who, cwd)
        lines = out.replace(self.root, "~/demo").splitlines()
        for line in lines:
            self.emit(line + "\r\n", 0.035)
        if code != 0:
            self.emit(f"{rgb(PINK)}exit status {code}{RESET}\r\n", 0.1)
        self.emit("", hold if hold is not None else 0.9 + 0.12 * len(lines))
        return code, out

    def save(self):
        self.emit("", 2.5)
        text = "".join(event[2] for event in self.events)
        user = os.environ.get("USER")
        for leak in (os.path.expanduser("~"), WORK, f"/{user}/" if user else None):
            if leak and leak in text:
                sys.exit(f"{self.name}: the recording contains {leak!r}")
        header = {"version": 2, "width": WIDTH, "height": HEIGHT,
                  "env": {"TERM": "xterm-256color", "SHELL": "/bin/bash"}}
        path = os.path.join(OUT, self.name + ".cast")
        with open(path, "w") as f:
            f.write(json.dumps(header) + "\n")
            for event in self.events:
                f.write(json.dumps(event) + "\n")
        print("recorded", self.name)


def check(result, want, what):
    code, out = result
    if code != want:
        sys.exit(f"{what}: exit status {code}, expected {want}\n{out}")


def directory(name):
    """A fresh directory with its own HOME and no git or braid configuration."""
    root = os.path.join(WORK, name)
    home = os.path.join(root, "home")
    bin_dir = os.path.join(root, "bin")
    os.makedirs(home)
    os.makedirs(bin_dir)
    os.symlink(BRAID, os.path.join(bin_dir, "braid"))
    env = {
        "PATH": bin_dir + os.pathsep + os.environ["PATH"],
        "HOME": home,
        "TZ": "UTC",
        "GIT_CONFIG_GLOBAL": os.devnull,
        "GIT_CONFIG_NOSYSTEM": "1",
        "BRAID_AUTHOR": "Ada Lovelace <ada@example.org>",
    }
    return root, env


def seed(scene):
    """A small git project in a bare repository that plays GitHub."""
    check(scene.quiet(
        f"""set -e
        mkdir seed && cd seed && {GIT} init -q
        printf '# shop\\n\\nA tiny shop.\\n' > README.md
        printf 'free_over = 60\\n' > shipping.toml
        {GIT} add README.md shipping.toml
        GIT_AUTHOR_DATE=2026-10-14T10:00:00Z GIT_COMMITTER_DATE=2026-10-14T10:00:00Z {GIT} commit -q -m 'Start the shop'
        printf '# shop\\n\\nA tiny shop.\\n\\nShipping rules live in shipping.toml.\\n' > README.md
        GIT_AUTHOR_DATE=2026-10-15T09:00:00Z GIT_COMMITTER_DATE=2026-10-15T09:00:00Z {GIT} commit -q -am 'Say where shipping rules live'
        cd .. && {GIT} clone -q --bare seed github.git && rm -rf seed"""), 0, "seed")


def agents_and_conflict():
    root, env = directory("agents")
    a = Scene("agents", root, env)
    seed(a)
    check(a.quiet("braid init shop && cd shop && braid git sync ../github.git"), 0, "setup")
    a.cwd = "shop"
    a.note("one repository, three agents: each gets a workspace of its own")
    check(a.run("for a in a b c; do braid workspace add agent-$a; done"), 0, "workspaces")
    a.note("they edit and commit at the same time: no shared index, no lock")
    check(a.run("echo 'free_over = 50' > shipping.toml && braid commit -m 'Free shipping from 50'",
                who="agent-a", cwd="shop/.braid/work/agent-a"), 0, "commit a")
    check(a.run("echo 'Ships in a day.' >> README.md && braid commit -m 'Next-day shipping'",
                who="agent-b", cwd="shop/.braid/work/agent-b"), 0, "commit b")
    check(a.run("echo 'free_over = 40' > shipping.toml && braid commit -m 'Free shipping from 40'",
                who="agent-c", cwd="shop/.braid/work/agent-c"), 0, "commit c")
    a.note("landing on main is one short step that they take in turn")
    check(a.run("braid land", who="agent-a", cwd="shop/.braid/work/agent-a"), 0, "land a")
    check(a.run("braid land", who="agent-b", cwd="shop/.braid/work/agent-b"), 0, "land b")
    check(a.run("braid land", who="agent-c", cwd="shop/.braid/work/agent-c", hold=3.0), 2, "land c")
    a.save()

    # The same repository, picked up where the first recording stops.
    b = Scene("conflict", root, env, cwd="shop/.braid/work/agent-c")
    b.note("agent-c changed the line agent-a landed first: main did not move")
    b.note("the conflict comes back as data, with all three sides")
    check(b.run("braid land --json | jq '.outcome, .next, (.conflicts[0].hunks[0] | map_values(.text))'",
                who="agent-c"), 0, "land json")
    b.note("update brings it into the working copy, with conflict markers")
    check(b.run("braid update", who="agent-c"), 2, "update")
    check(b.run("cat shipping.toml", who="agent-c"), 0, "cat")
    b.note("settle it, commit, land")
    check(b.run("echo 'free_over = 45' > shipping.toml", who="agent-c", hold=0.4), 0, "fix")
    check(b.run("braid commit -m 'Free shipping from 45' && braid land", who="agent-c"), 0, "resolve")
    check(b.run("braid log main -n 5", cwd="shop", hold=3.5), 0, "log")
    b.save()


def git_sync():
    root, env = directory("sync")
    s = Scene("git-sync", root, env)
    seed(s)
    s.note("github.git plays GitHub: a git repository with some history")
    check(s.run("git -C github.git log --oneline"), 0, "git log")
    s.note("braid takes a branch of it, history and all")
    check(s.run("braid init shop"), 0, "init")
    s.cwd = "shop"
    check(s.run("braid git sync ../github.git"), 0, "first sync")
    # Off camera: two agents land work in braid, and a person pushes to GitHub.
    for who, edit, message in (
        ("agent-a", "echo 'free_over = 50' > shipping.toml", "Free shipping from 50"),
        ("agent-b", "echo 'Ships in a day.' >> README.md", "Next-day shipping"),
    ):
        check(s.quiet(f"braid workspace add {who}"), 0, "workspace")
        check(s.quiet(f"{edit} && braid commit -m '{message}' && braid land",
                      who=who, cwd=f"shop/.braid/work/{who}"), 0, message)
    check(s.quiet(
        f"""set -e
        {GIT} clone -q github.git laptop && cd laptop
        printf 'vat = 21\\n' > prices.toml && {GIT} add prices.toml
        {GIT} -c user.name='Grace Hopper' -c user.email=grace@example.org commit -q -m 'Add VAT'
        {GIT} push -q origin main && cd .. && rm -rf laptop""", cwd="."), 0, "outside push")
    s.note("meanwhile agents landed two changes, and a person pushed to GitHub")
    check(s.run("braid log main -n 4"), 0, "log")
    check(s.run("braid git sync ../github.git"), 0, "sync")
    s.note("a fast-forward push with a lease: the old commits keep their ids")
    check(s.run("git -C ../github.git log --oneline --graph", hold=4.0), 0, "graph")
    s.save()


def agents_init():
    root, env = directory("setup")
    s = Scene("agents-init", root, env)
    seed(s)
    check(s.quiet(f"{GIT} clone -q github.git shop"), 0, "clone")
    s.cwd = "shop"
    s.note("what setting braid up comes down to, in a project's git checkout")
    s.note("(braid.elffuss.com/agents.md gives an agent the full steps)")
    check(s.run("braid init"), 0, "init")
    check(s.run("echo .braid >> .git/info/exclude", hold=0.4), 0, "exclude")
    check(s.run("braid git import ."), 0, "import")
    check(s.run("braid agents init"), 0, "agents init")
    check(s.run("git status --short"), 0, "status")
    s.note("the rules every later agent session reads, in the project's AGENTS.md")
    check(s.run("grep '^- [*][*]' AGENTS.md | cut -c1-76", hold=4.0), 0, "rules")
    s.note("a new session starts in a workspace of its own")
    check(s.run("braid workspace add agent-1 --track main", hold=3.5), 0, "workspace")
    s.save()


def main():
    os.makedirs(WORK)
    os.makedirs(OUT, exist_ok=True)
    agents_and_conflict()
    git_sync()
    agents_init()


main()
