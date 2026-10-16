#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright (c) 2026 UtopiaIA
"""Benchmarks braid against git on the same machine.

    python3 bench/run.py                       # every scenario, full size
    python3 bench/run.py --quick               # small inputs, a smoke run
    python3 bench/run.py --only status,commit  # some scenarios

Every scenario does the same work with braid and with git several times,
alternating which goes first, and compares the medians as a ratio, braid
divided by git: below 1 means braid is faster, or smaller. The table goes to
standard output. The raw measurements go to bench/results/, which is not
tracked: they describe this machine and stay on it. bench/README.md says
what each scenario measures.

It needs Python 3.9 or newer (standard library only), git, and cargo, which
builds braid in release mode first.
"""

from __future__ import annotations

import argparse
import datetime
import json
import math
import os
import platform
import resource
import shutil
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import traceback
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RESULTS = ROOT / "bench" / "results"

NAME = "Bench"
EMAIL = "bench@example.org"

# The medium public repository of the import, store and history scenarios,
# pinned to one release so that every run imports the same history.
PINNED = {
    "url": "https://github.com/tmux/tmux",
    "ref": "refs/tags/3.5a",
    "commit": "549c35b06165f6ae023115eb76f83f2cbf945395",
    "name": "tmux 3.5a",
}

SCENARIOS = {
    "import": "import a repository: braid git import against git clone --no-local",
    "status": "status on a large tree, clean and with files modified",
    "commit": "commit one changed file in the large tree",
    "wide": "one new file per commit in a single wide directory",
    "agents": "N agents landing K commits each, all at once",
    "store": "store size after the import",
    "history": "history queries on the imported repository",
}

FULL = {
    "import_rounds": 3,
    "tree_files": 100_000,
    "modified": 10,
    "status_rounds": 9,
    "commit_rounds": 9,
    "wide_files": 20_000,
    "wide_commits": 200,
    "wide_sweep": [2_500, 5_000, 10_000],
    "wide_sweep_commits": 30,
    "agents": [4, 8, 16],
    "agent_commits": 10,
    "agent_rounds": 3,
    "history_rounds": 9,
    "warmup": 2,
}

QUICK = {
    "import_rounds": 1,
    "tree_files": 2_000,
    "modified": 10,
    "status_rounds": 2,
    "commit_rounds": 2,
    "wide_files": 500,
    "wide_commits": 6,
    "wide_sweep": [125, 250],
    "wide_sweep_commits": 3,
    "agents": [2, 4],
    "agent_commits": 3,
    "agent_rounds": 1,
    "history_rounds": 2,
    "warmup": 1,
}

# Free space the work directory needs, in bytes: the large tree is checked
# out twice, and every small file takes a whole block.
NEEDED_SPACE = {"full": 6 << 30, "quick": 1 << 30}

# Files per directory of the large tree.
FILES_PER_DIR = 25

# A fixed date for the commits the generators make, so that their ids are
# the same on every run.
SEED_DATE = "1767225600 +0000"

# Enough for "every changeset" in log queries.
ALL = "1000000"


class Failure(Exception):
    """A command failed, or a scenario's premise did not hold."""


# ---------------------------------------------------------------------------
# Running and timing commands


class Run:
    """How long a command took, in wall-clock seconds and in CPU seconds
    (user and system, of the command and its children)."""

    __slots__ = ("wall", "cpu", "code", "out", "err")

    def __init__(self, wall, cpu, code=0, out=b"", err=b""):
        self.wall = wall
        self.cpu = cpu
        self.code = code
        self.out = out
        self.err = err

    def __add__(self, other):
        cpu = None if self.cpu is None or other.cpu is None else self.cpu + other.cpu
        return Run(self.wall + other.wall, cpu)

    def text(self):
        return self.out.decode("utf-8", "replace")

    def lines(self):
        return [line for line in self.text().splitlines() if line.strip()]

    def json(self):
        return json.loads(self.out)


def clean_environment():
    """The environment every command runs in: no configuration of the
    user's or the system's git, a fixed identity, and nothing that points
    git or braid somewhere else."""
    env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith(("GIT_", "BRAID_"))
    }
    env.update(
        {
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_AUTHOR_NAME": NAME,
            "GIT_AUTHOR_EMAIL": EMAIL,
            "GIT_COMMITTER_NAME": NAME,
            "GIT_COMMITTER_EMAIL": EMAIL,
            "GIT_TERMINAL_PROMPT": "0",
            "GIT_PAGER": "cat",
            "PAGER": "cat",
            "LC_ALL": "C",
            "BRAID_AUTHOR": f"{NAME} <{EMAIL}>",
            "BRAID_AGENT": "bench",
        }
    )
    return env


def children_cpu():
    """CPU seconds of every child process that has exited so far."""
    usage = resource.getrusage(resource.RUSAGE_CHILDREN)
    return usage.ru_utime + usage.ru_stime


class Tools:
    """Runs braid and git, and times them.

    The CPU time of a run is what the children that exited during it used,
    so it is only right while one command runs at a time; the agents
    scenario, which runs many at once, only uses wall time."""

    def __init__(self, braid, git):
        self.braid = str(braid)
        self.git = git
        self.env = clean_environment()

    def run(self, argv, cwd, check=True, capture=False, env=None):
        argv = [str(arg) for arg in argv]
        cpu_before = children_cpu()
        start = time.perf_counter()
        proc = subprocess.run(
            argv,
            cwd=cwd,
            env=env or self.env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE if capture else subprocess.DEVNULL,
            stderr=subprocess.PIPE,
        )
        wall = time.perf_counter() - start
        cpu = children_cpu() - cpu_before
        if check and proc.returncode != 0:
            raise Failure(
                f"`{' '.join(argv[:6])}` exited with {proc.returncode}: "
                + proc.stderr.decode("utf-8", "replace").strip()[-800:]
            )
        return Run(wall, cpu, proc.returncode, proc.stdout or b"", proc.stderr)

    def braid_run(self, cwd, *args, **kwargs):
        return self.run([self.braid, *args], cwd, **kwargs)

    def git_run(self, cwd, *args, **kwargs):
        return self.run([self.git, *args], cwd, **kwargs)


def round_order(names, round_index):
    """The order of the contestants in one round, so that a machine growing
    warmer or its caches filling favour none: A B, then B A, for two
    (braid, git, git, braid, ...); for more, every rotation of the list and
    then every rotation of it reversed, so that in each run of len(names)
    rounds every contestant takes every place once."""
    names = list(names)
    if len(names) == 2:
        return names if round_index % 2 == 0 else names[::-1]
    cycle = round_index % (2 * len(names))
    shift = cycle % len(names)
    order = names[shift:] + names[:shift]
    return order if cycle < len(names) else order[::-1]


class LoadGate:
    """With --max-load, waits before each timed round until the load average
    of the last minute drops below the limit, so that on a machine shared
    with other work the rounds run in its quieter moments. It changes when
    a round runs, never what it measures."""

    # The longest wait before one round runs anyway, and the longest wait of
    # a whole run, after which rounds no longer wait, in seconds.
    PATIENCE = 300
    BUDGET = 3600

    def __init__(self):
        self.limit = None
        self.waited = 0.0
        self.gave_up = 0

    def wait(self):
        if self.limit is None or not hasattr(os, "getloadavg"):
            return
        start = time.monotonic()
        while os.getloadavg()[0] >= self.limit:
            waited = time.monotonic() - start
            if waited > self.PATIENCE or self.waited + waited > self.BUDGET:
                self.gave_up += 1
                break
            time.sleep(2)
        self.waited += time.monotonic() - start


GATE = LoadGate()


def alternate(rounds, contestants, warmup=0):
    """Runs each contestant `warmup` times untimed, then `rounds` times in
    alternating order. `contestants` maps a name to a function of the round
    number that returns a Run. Returns the Runs of each name, by round."""
    names = list(contestants)
    for index in range(warmup):
        for name in round_order(names, index):
            contestants[name](-1 - index)
    out = {name: [] for name in names}
    for index in range(rounds):
        GATE.wait()
        for name in round_order(names, index):
            out[name].append(contestants[name](index))
    return out


# ---------------------------------------------------------------------------
# Results


class Row:
    """One line of the table: braid against git in one case."""

    def __init__(
        self,
        scenario,
        case,
        compared,
        braid=(),
        git=(),
        kind="time",
        note="",
        skipped="",
        extra=None,
    ):
        self.scenario = scenario
        self.case = case
        self.compared = compared
        self.braid = list(braid)
        self.git = list(git)
        self.kind = kind  # "time" (Runs), "size" (bytes) or "info"
        self.note = note
        self.skipped = skipped
        self.extra = extra or {}

    def _values(self, side):
        return [run.wall for run in side] if self.kind == "time" else list(side)

    def ratio(self):
        if self.skipped or not self.braid or not self.git:
            return None
        braid = statistics.median(self._values(self.braid))
        git = statistics.median(self._values(self.git))
        return braid / git if git else None

    def spread(self):
        """The smallest and largest ratio of a single round."""
        if self.kind != "time" or len(self.braid) != len(self.git) or len(self.braid) < 2:
            return None
        ratios = [b.wall / g.wall for b, g in zip(self.braid, self.git) if g.wall]
        return (min(ratios), max(ratios)) if ratios else None

    def cpu_ratio(self):
        if self.kind != "time" or self.skipped or not self.braid or not self.git:
            return None
        if any(run.cpu is None for run in self.braid + self.git):
            return None
        braid = statistics.median(run.cpu for run in self.braid)
        git = statistics.median(run.cpu for run in self.git)
        return braid / git if git else None

    def to_json(self):
        out = {
            "scenario": self.scenario,
            "case": self.case,
            "compared": self.compared,
            "kind": self.kind,
            "ratio": self.ratio(),
            "spread": self.spread(),
            "cpu_ratio": self.cpu_ratio(),
            "note": self.note,
            "skipped": self.skipped or None,
            "extra": self.extra,
        }
        for side, runs in (("braid", self.braid), ("git", self.git)):
            if self.kind == "time":
                out[side] = {
                    "wall_seconds": [run.wall for run in runs],
                    "cpu_seconds": [run.cpu for run in runs],
                    "median_wall_seconds": (
                        statistics.median(run.wall for run in runs) if runs else None
                    ),
                }
            else:
                out[side] = runs
        return out


def fmt(value, digits=2):
    return "-" if value is None else f"{value:.{digits}f}"


def print_table(rows, out=sys.stdout):
    print("", file=out)
    print("braid / git on this machine: below 1 means braid is faster or smaller.", file=out)
    print("spread: smallest and largest ratio of a single round; cpu: the same ratio", file=out)
    print("for CPU time (user and system) instead of wall time.", file=out)
    print("", file=out)
    header = f"{'scenario':<8} {'case':<44} {'braid/git':>9} {'spread':>13} {'cpu':>6}  note"
    print(header, file=out)
    print("-" * len(header), file=out)
    for row in rows:
        spread = row.spread()
        spread_text = f"{spread[0]:.2f}-{spread[1]:.2f}" if spread else "-"
        if row.skipped:
            line = (
                f"{row.scenario:<8} {row.case:<44} {'skipped':>9} {'':>13} {'':>6}  "
                f"{row.skipped}"
            )
        else:
            line = (
                f"{row.scenario:<8} {row.case:<44} {fmt(row.ratio()):>9} "
                f"{spread_text:>13} {fmt(row.cpu_ratio()):>6}  {row.note}"
            )
        print(line.rstrip(), file=out)
    print("", file=out)


def print_markdown(rows, out=sys.stdout):
    """The table for the README: ratios rounded to one decimal."""
    print("| What was compared | braid / git | Reproduce |", file=out)
    print("|---|---|---|", file=out)
    for row in rows:
        if row.skipped or row.kind == "info":
            continue
        reproduce = f"`python3 bench/run.py --only {row.scenario}`"
        print(f"| {row.compared} | {row.ratio():.1f} | {reproduce} |", file=out)
    for row in rows:
        if row.kind == "info" and row.note:
            print(f"\n{row.case}: {row.note}", file=out)
        if row.skipped:
            print(f"\nNot measured, {row.case}: {row.skipped}", file=out)


# ---------------------------------------------------------------------------
# Generated inputs


class SplitMix64:
    """A small deterministic generator: the same numbers on every platform
    and Python version."""

    MASK = (1 << 64) - 1

    def __init__(self, seed):
        self.state = seed & self.MASK

    def next(self):
        self.state = (self.state + 0x9E3779B97F4A7C15) & self.MASK
        z = self.state
        z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & self.MASK
        z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & self.MASK
        return z ^ (z >> 31)


def text_pool(size, seed):
    """`size` bytes of text lines that look like data, always the same."""
    rng = SplitMix64(seed)
    lines = []
    total = 0
    while total < size:
        line = f"{rng.next():016x} {rng.next() % 100_000:05d} {rng.next():016x}\n"
        lines.append(line)
        total += len(line)
    return "".join(lines).encode()[:size]


def tree_path(index):
    """Where file `index` of the large tree lives: 25 files per directory,
    three levels of directories under src/."""
    leaf, number = divmod(index, FILES_PER_DIR)
    return (
        f"src/m{leaf // 400:02d}/p{leaf // 20 % 20:02d}/"
        f"l{leaf % 20:02d}/f{number:02d}.txt"
    )


def tree_files(count):
    """The large tree: `count` text files of 100 to 3,000 bytes, the same
    bytes on every run."""
    pool = text_pool(1 << 20, seed=1)
    rng = SplitMix64(seed=2)
    for index in range(count):
        size = 100 + rng.next() % 2_900
        offset = rng.next() % (len(pool) - size)
        header = f"file {index}\n".encode()
        yield tree_path(index), header + pool[offset : offset + size]


def wide_path(index):
    return f"wide/f{index:05d}.txt"


def wide_content(index):
    return f"wide file {index}\n".encode()


def wide_files(count):
    yield "README.md", b"A single wide directory.\n"
    for index in range(count):
        yield wide_path(index), wide_content(index)


def agent_file(index):
    return f"agents/agent-{index:02d}.txt"


def agents_files(count):
    yield "README.md", b"Agents landing at once.\n"
    for index in range(1, count + 1):
        yield agent_file(index), f"agent {index}\n".encode()


def append(path, data):
    with open(path, "ab") as file:
        file.write(data)


# ---------------------------------------------------------------------------
# Repositories


def configure_git(tools, repo):
    """Settings of every git repository the benchmark makes: no automatic
    maintenance or garbage collection that could land in a measurement,
    and no file system monitor or untracked cache unless a case asks."""
    for key, value in (
        ("gc.auto", "0"),
        ("maintenance.auto", "false"),
        ("receive.autogc", "false"),
        ("core.fsmonitor", "false"),
        ("core.untrackedCache", "false"),
    ):
        tools.git_run(repo, "config", key, value)


def git_seed(tools, repo, files, bare=False):
    """Makes a git repository whose main branch has one commit with these
    files, written with git fast-import, and checks it out unless bare."""
    repo.parent.mkdir(parents=True, exist_ok=True)
    tools.git_run(
        repo.parent, "init", "-q", "-b", "main", *(["--bare"] if bare else []), repo
    )
    configure_git(tools, repo)
    with tempfile.TemporaryFile() as errors:
        proc = subprocess.Popen(
            [tools.git, "fast-import", "--quiet", "--done"],
            cwd=repo,
            env=tools.env,
            stdin=subprocess.PIPE,
            stdout=subprocess.DEVNULL,
            stderr=errors,
        )
        try:
            stream = proc.stdin
            marks = []
            for mark, (path, data) in enumerate(files, start=1):
                stream.write(b"blob\nmark :%d\ndata %d\n" % (mark, len(data)))
                stream.write(data)
                stream.write(b"\n")
                marks.append((path, mark))
            message = b"Seed"
            stream.write(b"commit refs/heads/main\n")
            for who in (b"author", b"committer"):
                stream.write(
                    b"%s %s <%s> %s\n"
                    % (who, NAME.encode(), EMAIL.encode(), SEED_DATE.encode())
                )
            stream.write(b"data %d\n%s\n" % (len(message), message))
            for path, mark in marks:
                stream.write(b"M 100644 :%d %s\n" % (mark, path.encode()))
            stream.write(b"done\n")
            stream.close()
        except BrokenPipeError:
            pass
        if proc.wait() != 0:
            errors.seek(0)
            raise Failure("git fast-import failed: " + errors.read().decode()[-800:])
    if not bare:
        tools.git_run(repo, "reset", "-q", "--hard")


def braid_from_git(tools, repo, git_repo, workspace=None, name="ws"):
    """A braid repository that imported `git_repo`, with a workspace on main
    when one is asked for."""
    tools.braid_run(repo.parent, "init", repo)
    tools.braid_run(repo, "git", "import", git_repo)
    if workspace is not None:
        tools.braid_run(repo, "workspace", "add", name, workspace)


def disk_usage(path):
    """Bytes `du` counts for `path`, and the sum of the sizes of its files."""
    out = subprocess.run(
        ["du", "-sk", str(path)], capture_output=True, check=True, text=True
    ).stdout
    blocks = int(out.split()[0]) * 1024
    apparent = 0
    for parent, _dirs, names in os.walk(path):
        for name in names:
            try:
                apparent += os.lstat(os.path.join(parent, name)).st_size
            except FileNotFoundError:
                pass
    return blocks, apparent


def remove(*paths):
    for path in paths:
        shutil.rmtree(path, ignore_errors=True)


def count_files(root):
    """Files and symlinks under `root`, never following a symlink."""
    count = 0
    pending = [root]
    while pending:
        with os.scandir(pending.pop()) as entries:
            for entry in entries:
                if entry.is_dir(follow_symlinks=False):
                    pending.append(entry.path)
                else:
                    count += 1
    return count


# ---------------------------------------------------------------------------
# What the scenarios share


class LargeTree:
    """The large tree, checked out by git and by braid: a git repository
    with its working tree, and a braid repository with one workspace."""

    def __init__(self, ctx):
        count = ctx.settings["tree_files"]
        base = ctx.work / "tree"
        self.git = base / "git"
        self.repo = base / "braid"
        self.ws = base / "braid-ws"
        self.count = count
        ctx.log(f"writing a tree of {count} files with git, then importing it into braid")
        git_seed(ctx.tools, self.git, tree_files(count))
        braid_from_git(ctx.tools, self.repo, self.git, self.ws)
        self.base = base

    def both(self, path):
        return self.git / path, self.ws / path


class Context:
    """What the scenarios share: the tools, the settings, the work directory,
    and inputs made once."""

    def __init__(self, tools, settings, work, source, keep):
        self.tools = tools
        self.settings = settings
        self.work = work
        self.source_spec = source
        self.keep = keep
        self.extra = {}
        self._source = None
        self._imported = None
        self._gc = False
        self._tree = None

    def log(self, message):
        print(f"  {message}", file=sys.stderr, flush=True)

    def source(self):
        """The git repository to import, as a bare repository with one
        branch, main, at the pinned commit: fetched once."""
        if self._source is None:
            spec = self.source_spec
            path = self.work / "source.git"
            self.log(f"fetching {spec['name']} once, as the local source")
            self.tools.git_run(self.work, "init", "-q", "--bare", "-b", "main", path)
            configure_git(self.tools, path)
            self.tools.git_run(
                path, "fetch", "-q", "--no-tags", spec["url"], f"+{spec['ref']}:refs/bench/pinned"
            )
            commit = self.tools.git_run(
                path, "rev-parse", "refs/bench/pinned^{commit}", capture=True
            ).text().strip()
            if spec.get("commit") and commit != spec["commit"]:
                raise Failure(f"{spec['name']} is at {commit}, not the pinned {spec['commit']}")
            self.tools.git_run(path, "update-ref", "refs/heads/main", commit)
            self.tools.git_run(path, "update-ref", "-d", "refs/bench/pinned")
            self.tools.git_run(path, "symbolic-ref", "HEAD", "refs/heads/main")
            commits = int(
                self.tools.git_run(path, "rev-list", "--count", "main", capture=True).text()
            )
            # Files and symlinks: a checkout writes a gitlink as a directory.
            entries = self.tools.git_run(path, "ls-tree", "-r", "main", capture=True).lines()
            files = sum(1 for entry in entries if entry.split()[1] == "blob")
            self.extra["source"] = {
                "name": spec["name"],
                "commit": commit,
                "commits": commits,
                "files": files,
            }
            self._source = path
        return self._source

    def imported(self):
        """A braid repository that imported the source, and a bare git clone
        of it: the ones the import scenario made, or new ones."""
        if self._imported is None:
            source = self.source()
            braid_repo = self.work / "imported-braid"
            git_repo = self.work / "imported-git.git"
            self.log("importing the source once with braid, cloning it once with git")
            braid_from_git(self.tools, braid_repo, source)
            self.tools.git_run(
                self.work, "clone", "-q", "--no-local", "--bare", source, git_repo
            )
            configure_git(self.tools, git_repo)
            self._imported = (braid_repo, git_repo)
        return self._imported

    def gc_git(self):
        """`git gc --aggressive` on the bare clone, once."""
        _braid_repo, git_repo = self.imported()
        if not self._gc:
            self.extra.setdefault("store", {})["git_as_cloned"] = disk_usage(git_repo)
            self.log("git gc --aggressive on the clone")
            self.tools.git_run(git_repo, "gc", "-q", "--aggressive", "--prune=now")
            self._gc = True
        return git_repo

    def tree(self):
        if self._tree is None:
            self._tree = LargeTree(self)
        return self._tree

    def drop(self, remaining):
        """Removes inputs that no scenario still to run needs."""
        if self.keep:
            return
        if self._tree is not None and not {"status", "commit"} & remaining:
            remove(self._tree.base)
            self._tree = None
        if self._imported is not None and not {"store", "history"} & remaining:
            remove(*self._imported)
            self._imported = None
            self._gc = False
        if self._source is not None and not {"import", "store", "history"} & remaining:
            remove(self._source)
            self._source = None


# ---------------------------------------------------------------------------
# 1. Import


def scenario_import(ctx):
    tools = ctx.tools
    source = ctx.source()
    info = ctx.extra["source"]
    rounds = ctx.settings["import_rounds"]
    work = ctx.work
    out = {"braid": [], "braid-checkout": [], "git-bare": [], "git-full": []}
    kept = None
    for index in range(rounds):
        made = {}
        GATE.wait()
        for who in round_order(["braid", "git-bare", "git-full"], index):
            ctx.log(f"import round {index + 1}/{rounds}: {who}")
            if who == "braid":
                repo = work / f"import-{index}-braid"
                ws = work / f"import-{index}-braid-ws"
                init = tools.braid_run(work, "init", repo)
                imported = tools.braid_run(repo, "git", "import", source, "--json", capture=True)
                checkout = tools.braid_run(repo, "workspace", "add", "ws", ws)
                if imported.json()["commits"] != info["commits"]:
                    raise Failure(
                        f"braid imported {imported.json()['commits']} commits, "
                        f"git has {info['commits']}"
                    )
                out["braid"].append(init + imported)
                out["braid-checkout"].append(init + imported + checkout)
                made["braid"] = (repo, ws)
            elif who == "git-bare":
                target = work / f"import-{index}-git.git"
                out["git-bare"].append(
                    tools.git_run(work, "clone", "-q", "--no-local", "--bare", source, target)
                )
                made["git-bare"] = target
            else:
                target = work / f"import-{index}-git"
                out["git-full"].append(
                    tools.git_run(work, "clone", "-q", "--no-local", source, target)
                )
                made["git-full"] = target
        repo, ws = made["braid"]
        if index == 0:
            fsck = tools.braid_run(repo, "fsck", "--json", capture=True).json()
            if not fsck["clean"]:
                raise Failure(f"braid fsck after the import: {fsck['problems'][:3]}")
            files = count_files(ws) - 1  # less the workspace's .braid file
            if files != info["files"]:
                raise Failure(f"the workspace has {files} files, git's tree {info['files']}")
            configure_git(tools, made["git-bare"])
            kept = (repo, made["git-bare"])
            remove(ws, made["git-full"])
        else:
            remove(repo, ws, made["git-bare"], made["git-full"])
    # The first round's repositories serve the store and history scenarios.
    if ctx._imported is None:
        ctx._imported = kept
    elif kept is not None:
        remove(*kept)
    name = f"{info['name']}, {info['commits']:,} commits"
    return [
        Row(
            "import",
            "history only",
            f"Import {name}: `braid init` + `braid git import` vs "
            "`git clone --no-local --bare`",
            out["braid"],
            out["git-bare"],
        ),
        Row(
            "import",
            "history and a working copy",
            f"Import {name} and check it out: `braid init` + `braid git import` + "
            "`braid workspace add` vs `git clone --no-local`",
            out["braid-checkout"],
            out["git-full"],
        ),
    ]


# ---------------------------------------------------------------------------
# 2. Status


def check_status(tools, tree, modified):
    """Both tools must see the same thing: a clean tree, or exactly the
    modified files."""
    braid = tools.braid_run(tree.ws, "status", "--json", capture=True).json()
    git = tools.git_run(tree.git, "status", "--porcelain", capture=True).lines()
    want = sorted(modified)
    if sorted(braid["modified"]) != want or braid["added"] or braid["deleted"]:
        raise Failure(f"braid status does not list exactly the {len(want)} modified files")
    if sorted(line[3:] for line in git) != want or any(line[:2] != " M" for line in git):
        raise Failure(f"git status does not list exactly the {len(want)} modified files")


def scenario_status(ctx):
    tools = ctx.tools
    tree = ctx.tree()
    count = tree.count
    rounds = ctx.settings["status_rounds"]
    warmup = ctx.settings["warmup"]
    rows = []
    # Modified files, spread over the tree.
    many = ctx.settings["modified"]
    changed = [tree_path((k * count // many + 7) % count) for k in range(many)]
    originals = {}
    for case in ("clean", "modified"):
        if case == "modified":
            for path in changed:
                for full in tree.both(path):
                    originals[full] = full.read_bytes()
                    append(full, b"modified\n")
        modified = changed if case == "modified" else []
        check_status(tools, tree, modified)
        label = "clean" if case == "clean" else f"{len(changed)} files modified"
        for untracked_cache in (False, True):
            tools.git_run(tree.git, "config", "core.untrackedCache", str(untracked_cache).lower())
            setting = "untracked cache on" if untracked_cache else "no untracked cache"
            ctx.log(f"status, {label}, git with {setting}")
            runs = alternate(
                rounds,
                {
                    "braid": lambda _r: tools.braid_run(tree.ws, "status"),
                    "git": lambda _r: tools.git_run(tree.git, "status"),
                },
                warmup=warmup,
            )
            git_text = (
                "`git status` with `core.untrackedCache=true`"
                if untracked_cache
                else "`git status` with no untracked cache"
            )
            rows.append(
                Row(
                    "status",
                    f"{label}, git {setting}",
                    f"Status of {count:,} files, {label}: `braid status` vs {git_text} "
                    "and no fsmonitor",
                    runs["braid"],
                    runs["git"],
                )
            )
        if case == "modified":
            for full, data in originals.items():
                full.write_bytes(data)
            check_status(tools, tree, [])
    tools.git_run(tree.git, "config", "core.untrackedCache", "false")
    return rows


# ---------------------------------------------------------------------------
# 3. Commit


def scenario_commit(ctx):
    tools = ctx.tools
    tree = ctx.tree()
    count = tree.count
    rounds = ctx.settings["commit_rounds"]
    warmup = ctx.settings["warmup"]
    check_status(tools, tree, [])
    before = int(tools.git_run(tree.git, "rev-list", "--count", "main", capture=True).text())
    ctx.log(f"commit one changed file of {count}, {rounds} rounds")
    picked = {}

    def pick(index):
        if index not in picked:
            path = tree_path((len(picked) * 7_919 + 4_242) % count)
            picked[index] = path
            for full in tree.both(path):
                append(full, f"edit {index}\n".encode())
        return picked[index]

    def braid(index):
        pick(index)
        run = tools.braid_run(tree.ws, "commit", "-m", f"Edit {index}", "--json", capture=True)
        if run.json()["changes"] != 1:
            raise Failure("braid commit did not record exactly one path")
        return run

    def git(index):
        path = pick(index)
        return tools.git_run(tree.git, "add", path) + tools.git_run(
            tree.git, "commit", "-q", "-m", f"Edit {index}"
        )

    runs = alternate(rounds, {"braid": braid, "git": git}, warmup=warmup)
    after = int(tools.git_run(tree.git, "rev-list", "--count", "main", capture=True).text())
    if after - before != rounds + warmup:
        raise Failure(f"git made {after - before} commits, not {rounds + warmup}")
    check_status(tools, tree, [])
    return [
        Row(
            "commit",
            f"one changed file of {count:,}",
            f"Commit one changed file in a tree of {count:,} files: `braid commit` vs "
            "`git add` + `git commit`",
            runs["braid"],
            runs["git"],
        )
    ]


# ---------------------------------------------------------------------------
# 4. Wide directory


def wide_series(ctx, size, commits, warmup):
    """Appends one new file to a directory of `size` files and commits it,
    `commits` times, with git (add and commit), with a braid workspace
    (commit), and without a working copy (change create --land). Returns
    the timed Runs of each, in order."""
    tools = ctx.tools
    base = ctx.work / f"wide-{size}"
    git_repo = base / "git"
    ws_repo = base / "braid"
    ws = base / "braid-ws"
    change_repo = base / "braid-change"
    scratch = base / "new-file"
    ctx.log(f"wide directory of {size} files: {commits} commits each")
    git_seed(tools, git_repo, wide_files(size))
    braid_from_git(tools, ws_repo, git_repo, ws)
    braid_from_git(tools, change_repo, git_repo)
    # One new file per round, written when the round's first contestant
    # starts: every contestant commits the same file in that round.
    written = {}

    def write(index):
        if index not in written:
            number = size + len(written)
            path = wide_path(number)
            data = wide_content(number)
            (git_repo / path).write_bytes(data)
            (ws / path).write_bytes(data)
            scratch.write_bytes(data)
            written[index] = path
        return written[index]

    def braid_commit(index):
        write(index)
        run = tools.braid_run(ws, "commit", "-m", f"Add file {index}", "--json", capture=True)
        if run.json()["changes"] != 1:
            raise Failure("braid commit did not record exactly one path")
        return run

    def braid_change(index):
        path = write(index)
        run = tools.braid_run(
            change_repo,
            "change",
            "create",
            "--base",
            "main",
            "--set",
            f"{path}=@{scratch}",
            "-m",
            f"Add file {index}",
            "--land",
            "main",
            capture=True,
        )
        if run.json()["land"]["outcome"] != "fast-forward":
            raise Failure("braid change create --land did not fast-forward")
        return run

    def git(index):
        path = write(index)
        return tools.git_run(git_repo, "add", path) + tools.git_run(
            git_repo, "commit", "-q", "-m", f"Add file {index}"
        )

    contestants = {"braid-commit": braid_commit, "git": git, "braid-change": braid_change}
    runs = alternate(commits, contestants, warmup=warmup)
    total = commits + warmup
    made = int(tools.git_run(git_repo, "rev-list", "--count", "main", capture=True).text()) - 1
    landed = len(tools.braid_run(change_repo, "log", "main", "-n", ALL, capture=True).lines()) - 1
    unlanded = len(tools.braid_run(ws, "status", "--json", capture=True).json()["unlanded"])
    if (made, landed, unlanded) != (total, total, total):
        raise Failure(
            f"expected {total} commits each; git made {made}, change create landed "
            f"{landed}, the workspace has {unlanded}"
        )
    if not ctx.keep:
        remove(base)
    return runs


def median_of(runs):
    return statistics.median(run.wall for run in runs)


def trend(runs):
    """How much slower the last quarter of a series is than the first, from
    the medians of each: 0.1 means 10% slower."""
    quarter = max(1, len(runs) // 4)
    first = median_of(runs[:quarter])
    last = median_of(runs[-quarter:])
    return last / first - 1 if first else None


def exponent(sizes, costs):
    """The slope of log(cost) against log(size), by least squares: 0 when the
    cost does not grow with the directory, 1 when it grows in proportion."""
    xs = [math.log(size) for size in sizes]
    ys = [math.log(cost) for cost in costs]
    mean_x = sum(xs) / len(xs)
    mean_y = sum(ys) / len(ys)
    den = sum((x - mean_x) ** 2 for x in xs)
    return sum((x - mean_x) * (y - mean_y) for x, y in zip(xs, ys)) / den if den else None


def scenario_wide(ctx):
    settings = ctx.settings
    size = settings["wide_files"]
    commits = settings["wide_commits"]
    warmup = settings["warmup"]
    main = wide_series(ctx, size, commits, warmup)
    rows = [
        Row(
            "wide",
            f"new file in {size:,}: braid commit",
            f"One new file per commit in a directory of {size:,} files, {commits} times: "
            "`braid commit` vs `git add` + `git commit`",
            main["braid-commit"],
            main["git"],
        ),
        Row(
            "wide",
            f"new file in {size:,}: change create --land",
            f"The same, without a working copy: `braid change create --land` vs "
            "`git add` + `git commit`",
            main["braid-change"],
            main["git"],
        ),
    ]

    # Does the cost of one commit grow with the directory? The same series,
    # shorter, on smaller directories, against the start of the main one.
    sweep_commits = settings["wide_sweep_commits"]
    sizes = list(settings["wide_sweep"]) + [size]
    medians = {name: [] for name in main}
    for sweep_size in settings["wide_sweep"]:
        series = wide_series(ctx, sweep_size, sweep_commits, warmup)
        for name in medians:
            medians[name].append(median_of(series[name]))
    for name in medians:
        medians[name].append(median_of(main[name][:sweep_commits]))
    growth = {name: medians[name][-1] / medians[name][0] for name in medians}
    slopes = {name: exponent(sizes, medians[name]) for name in medians}
    drift = {name: trend(main[name]) for name in main}
    ctx.extra["wide"] = {
        "sizes": sizes,
        "median_seconds_by_size": medians,
        "growth": growth,
        "exponent": slopes,
        "trend_over_main_series": drift,
    }
    labels = {"git": "git", "braid-commit": "braid commit", "braid-change": "change create"}
    note = ", ".join(f"{labels[name]} x{growth[name]:.2f}" for name in labels)
    rows.append(
        Row(
            "wide",
            f"growth from {sizes[0]:,} to {size:,} files",
            f"Cost of one commit in a directory of {size:,} files divided by its cost "
            f"in one of {sizes[0]:,}",
            kind="info",
            note=note,
            extra={"growth": growth, "exponent": slopes},
        )
    )
    note = ", ".join(f"{labels[name]} {drift[name]:+.0%}" for name in labels)
    rows.append(
        Row(
            "wide",
            f"drift over the {commits} commits",
            f"Last quarter of the {commits} commits against the first",
            kind="info",
            note=note,
            extra={"trend": drift},
        )
    )
    return rows


# ---------------------------------------------------------------------------
# 5. Agents


def run_agents(count, agent):
    """Runs `agent(i)` for i in 1..count on as many threads, released at
    once. Returns the wall time until the last one finished."""
    barrier = threading.Barrier(count + 1)
    errors = []

    def body(index):
        try:
            barrier.wait(timeout=600)
            agent(index)
        except BaseException as err:  # reported below, from the main thread
            errors.append(err)

    threads = [threading.Thread(target=body, args=(i,)) for i in range(1, count + 1)]
    GATE.wait()
    for thread in threads:
        thread.start()
    barrier.wait(timeout=600)
    start = time.perf_counter()
    for thread in threads:
        thread.join()
    wall = time.perf_counter() - start
    if errors:
        raise errors[0]
    return wall


def braid_agents(ctx, base, count, commits):
    tools = ctx.tools
    seed = base / "seed"
    repo = base / "braid"
    git_seed(tools, seed, agents_files(count))
    braid_from_git(tools, repo, seed)
    for index in range(1, count + 1):
        tools.braid_run(repo, "workspace", "add", f"agent-{index:02d}", base / f"ws-{index:02d}")
    outcomes = {}
    lock = threading.Lock()

    def agent(index):
        ws = base / f"ws-{index:02d}"
        env = dict(
            tools.env,
            BRAID_AGENT=f"agent-{index:02d}",
            BRAID_AUTHOR=f"Agent {index} <agent-{index:02d}@example.org>",
        )
        for change in range(commits):
            append(ws / agent_file(index), f"agent {index} change {change}\n".encode())
            tools.braid_run(ws, "commit", "-m", f"Agent {index}, change {change}", env=env)
            landed = tools.braid_run(ws, "land", "--json", env=env, capture=True)
            outcome = landed.json()["outcome"]
            with lock:
                outcomes[outcome] = outcomes.get(outcome, 0) + 1

    wall = run_agents(count, agent)
    total = 1 + count * commits
    landed = len(tools.braid_run(repo, "log", "main", "-n", ALL, capture=True).lines())
    if landed != total:
        raise Failure(f"main has {landed} changesets after the agents, not {total}")
    check = base / "check"
    tools.braid_run(repo, "workspace", "add", "check", check)
    for index in range(1, count + 1):
        lines = (check / agent_file(index)).read_text().splitlines()
        if len(lines) != 1 + commits:
            raise Failure(f"agent {index}'s file has {len(lines)} lines on main")
    return Run(wall, None), outcomes


def git_agents(ctx, base, count, commits):
    tools = ctx.tools
    seed = base / "seed"
    hub = base / "hub.git"
    git_seed(tools, seed, agents_files(count))
    tools.git_run(base, "clone", "-q", "--bare", seed, hub)
    configure_git(tools, hub)
    for index in range(1, count + 1):
        clone = base / f"clone-{index:02d}"
        tools.git_run(base, "clone", "-q", hub, clone)
        configure_git(tools, clone)
    retries = []
    lock = threading.Lock()

    def agent(index):
        clone = base / f"clone-{index:02d}"
        mine = 0
        for change in range(commits):
            append(clone / agent_file(index), f"agent {index} change {change}\n".encode())
            tools.git_run(clone, "commit", "-q", "-a", "-m", f"Agent {index}, change {change}")
            while tools.git_run(clone, "push", "-q", "origin", "main", check=False).code != 0:
                mine += 1
                if mine > 1_000 * commits:
                    raise Failure(f"agent {index} could not push")
                tools.git_run(clone, "pull", "-q", "--rebase", "origin", "main")
        with lock:
            retries.append(mine)

    wall = run_agents(count, agent)
    total = 1 + count * commits
    landed = int(tools.git_run(hub, "rev-list", "--count", "main", capture=True).text())
    if landed != total:
        raise Failure(f"the hub's main has {landed} commits after the agents, not {total}")
    for index in range(1, count + 1):
        text = tools.git_run(hub, "show", f"main:{agent_file(index)}", capture=True).text()
        if len(text.splitlines()) != 1 + commits:
            raise Failure(f"agent {index}'s file is wrong on the hub's main")
    return Run(wall, None), sum(retries)


def scenario_agents(ctx):
    settings = ctx.settings
    commits = settings["agent_commits"]
    rounds = settings["agent_rounds"]
    rows = []
    for count in settings["agents"]:
        walls = {"braid": [], "git": []}
        retries = []
        outcomes = {}
        for index in range(rounds):
            for who in round_order(["braid", "git"], index):
                ctx.log(f"{count} agents, round {index + 1}/{rounds}: {who}")
                base = ctx.work / f"agents-{count}-{index}-{who}"
                if who == "braid":
                    run, landed = braid_agents(ctx, base, count, commits)
                    for outcome, number in landed.items():
                        outcomes[outcome] = outcomes.get(outcome, 0) + number
                else:
                    run, retried = git_agents(ctx, base, count, commits)
                    retries.append(retried)
                walls[who].append(run)
                if not ctx.keep:
                    remove(base)
        pushes = count * commits
        per_push = statistics.median(retries) / pushes
        rebased = outcomes.get("rebased", 0) / max(1, sum(outcomes.values()))
        rows.append(
            Row(
                "agents",
                f"{count} agents x {commits} commits",
                f"{count} agents land {commits} commits each, at once: `braid commit` + "
                "`braid land` in a workspace each, vs `git commit` + `git push` from a "
                "clone each, with `git pull --rebase` and a new push when rejected",
                walls["braid"],
                walls["git"],
                note=f"git retried {per_push:.2f} times per push; "
                f"braid rebased {rebased:.0%} of its landings, retrying none",
                extra={
                    "git_retries_per_round": retries,
                    "git_retries_per_push": per_push,
                    "braid_land_outcomes": outcomes,
                },
            )
        )
    return rows


# ---------------------------------------------------------------------------
# 6. Store size


def scenario_store(ctx):
    braid_repo, _ = ctx.imported()
    git_repo = ctx.gc_git()
    info = ctx.extra["source"]
    braid_blocks, braid_bytes = disk_usage(braid_repo / ".braid" / "store")
    git_blocks, git_bytes = disk_usage(git_repo)
    store = ctx.extra.setdefault("store", {})
    store.update(
        {
            "braid_store": [braid_blocks, braid_bytes],
            "git_after_gc_aggressive": [git_blocks, git_bytes],
        }
    )
    return [
        Row(
            "store",
            "after the import",
            f"Size of {info['name']}: `du` of `.braid/store` after `braid git import` vs "
            "the bare clone after `git gc --aggressive`",
            [braid_blocks],
            [git_blocks],
            kind="size",
            note=f"file sizes alone: {braid_bytes / git_bytes:.2f}",
        )
    ]


# ---------------------------------------------------------------------------
# 7. History


def unsupported(run):
    """Whether braid refused the command line itself: a subcommand or an
    argument it does not have yet."""
    err = run.err.decode("utf-8", "replace")
    return run.code == 2 and (
        "unrecognized subcommand" in err or "unexpected argument" in err
    )


def pick_paths(tools, git_repo):
    """The file of main that most first-parent commits changed, and one that
    only one of them touched, with how many changed each file."""
    present = set(
        tools.git_run(git_repo, "ls-tree", "-r", "--name-only", "main", capture=True).lines()
    )
    counts = {}
    names = tools.git_run(
        git_repo, "log", "--first-parent", "--format=", "--name-only", "main", capture=True
    ).lines()
    for name in names:
        if name in present:
            counts[name] = counts.get(name, 0) + 1
    hot = min(counts, key=lambda name: (-counts[name], name))
    cold = min(counts, key=lambda name: (counts[name], name))
    return hot, cold, counts


def with_json(args):
    """Braid arguments with --json, before any `--`."""
    if "--" in args:
        at = args.index("--")
        return [*args[:at], "--json", *args[at:]]
    return [*args, "--json"]


def scenario_history(ctx):
    tools = ctx.tools
    braid_repo, _ = ctx.imported()
    git_repo = ctx.gc_git()
    info = ctx.extra["source"]
    rounds = ctx.settings["history_rounds"]
    warmup = ctx.settings["warmup"]
    hot, cold, counts = pick_paths(tools, git_repo)
    ctx.extra["history"] = {"hot": [hot, counts[hot]], "cold": [cold, counts[cold]]}
    name = info["name"]
    # Braid follows first parents, so git does too: both list the same
    # changesets. The last item is what git lists, to check braid against.
    queries = [
        (
            "log -n 1000",
            f"Newest 1000 changesets of {name}: `braid log -n 1000` vs "
            "`git log --first-parent -n 1000 --format=%H`",
            ["log", "main", "-n", "1000"],
            ["log", "--first-parent", "-n", "1000", "--format=%H", "main"],
            ["log", "--first-parent", "-n", "1000", "--format=%H", "main"],
        ),
        (
            "log -- the most changed file",
            f"History of the file most changed in {name}: "
            "`braid log -- <path>` vs `git log --first-parent -- <path>`",
            ["log", "main", "-n", ALL, "--", hot],
            ["log", "--first-parent", "-n", ALL, "main", "--", hot],
            ["log", "--first-parent", "--format=%H", "main", "--", hot],
        ),
        (
            "log -- a file one commit touched",
            f"History of a file one commit touched in {name}: "
            "`braid log -- <path>` vs `git log --first-parent -- <path>`",
            ["log", "main", "-n", ALL, "--", cold],
            ["log", "--first-parent", "-n", ALL, "main", "--", cold],
            ["log", "--first-parent", "--format=%H", "main", "--", cold],
        ),
        (
            "blame the most changed file",
            f"Blame of the file most changed in {name}: `braid blame <path>` vs "
            "`git blame --first-parent <path>`",
            ["blame", hot],
            ["blame", "--first-parent", "main", "--", hot],
            None,
        ),
    ]
    rows = []
    for case, compared, braid_args, git_args, listing in queries:
        # A log query is checked through --json, one entry per changeset,
        # against git's list; blame only has to succeed, since its output
        # is braid's own.
        probe_args = with_json(braid_args) if listing else braid_args
        probe = tools.braid_run(braid_repo, *probe_args, check=False, capture=True)
        if unsupported(probe):
            reason = probe.err.decode("utf-8", "replace").strip().splitlines()[0]
            rows.append(Row("history", case, compared, skipped=f"not in braid yet ({reason})"))
            continue
        if probe.code != 0:
            raise Failure(f"braid {case}: {probe.err.decode('utf-8', 'replace')[-400:]}")
        if listing:
            expected = len(tools.git_run(git_repo, *listing, capture=True).lines())
            got = len(probe.json())
            if got != expected:
                raise Failure(f"braid {case}: {got} changesets, git lists {expected} commits")
        elif not probe.out.strip():
            raise Failure(f"braid {case} printed nothing")
        ctx.log(f"history: {case}")
        runs = alternate(
            rounds,
            {
                "braid": lambda _r, a=braid_args: tools.braid_run(braid_repo, *a),
                "git": lambda _r, a=git_args: tools.git_run(git_repo, *a),
            },
            warmup=warmup,
        )
        rows.append(Row("history", case, compared, runs["braid"], runs["git"]))
    return rows


RUNNERS = {
    "import": scenario_import,
    "status": scenario_status,
    "commit": scenario_commit,
    "wide": scenario_wide,
    "agents": scenario_agents,
    "store": scenario_store,
    "history": scenario_history,
}


# ---------------------------------------------------------------------------
# Main


def build_braid():
    """Builds braid in release mode and returns the binary's path."""
    print("building braid in release mode", file=sys.stderr, flush=True)
    subprocess.run(
        ["cargo", "build", "--release", "--locked", "-p", "elffuss-braid-cli"],
        cwd=ROOT,
        check=True,
    )
    meta = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"],
        cwd=ROOT,
        check=True,
        capture_output=True,
    )
    target = Path(json.loads(meta.stdout)["target_directory"])
    return target / "release" / ("braid.exe" if os.name == "nt" else "braid")


def describe_build(git):
    """Which braid was measured: the commit, and whether the tree differed."""
    def ask(*args):
        out = subprocess.run(
            [git, "-C", str(ROOT), *args],
            capture_output=True,
            text=True,
            env=clean_environment(),
        )
        return out.stdout.strip() if out.returncode == 0 else None

    status = ask("status", "--porcelain", "--untracked-files=no")
    return {
        "commit": ask("rev-parse", "HEAD"),
        "modified": None if status is None else bool(status),
    }


def parse_args(argv):
    parser = argparse.ArgumentParser(
        description="Benchmarks braid against git on this machine and prints braid/git ratios.",
    )
    parser.add_argument("--quick", action="store_true", help="small inputs, for a smoke run")
    parser.add_argument(
        "--only",
        metavar="LIST",
        help="comma-separated scenarios to run (default: all): " + ", ".join(SCENARIOS),
    )
    parser.add_argument("--list", action="store_true", help="list the scenarios and exit")
    parser.add_argument("--rounds", type=int, metavar="N", help="rounds of every scenario")
    parser.add_argument(
        "--braid", type=Path, metavar="PATH", help="a braid binary to use instead of building one"
    )
    parser.add_argument(
        "--git", default="git", metavar="PATH", help="the git binary (default: git)"
    )
    parser.add_argument(
        "--source",
        metavar="URL_OR_PATH",
        help="the git repository to import, instead of the pinned one (or, with --quick, "
        "this repository's own history)",
    )
    parser.add_argument(
        "--source-ref", default="HEAD", metavar="REF", help="what to import from --source"
    )
    parser.add_argument(
        "--workdir",
        type=Path,
        metavar="DIR",
        help="where to make the inputs (default: a new temporary directory)",
    )
    parser.add_argument(
        "--keep", action="store_true", help="keep the inputs instead of removing them"
    )
    parser.add_argument(
        "--markdown", action="store_true", help="also print the table as Markdown, rounded"
    )
    parser.add_argument(
        "--max-load",
        type=float,
        metavar="N",
        help="before each timed round, wait until the load average of the last minute "
        "is below N (on a machine shared with other work)",
    )
    args = parser.parse_args(argv)
    if args.only:
        chosen = {name.strip() for name in args.only.split(",") if name.strip()}
        unknown = sorted(chosen - set(SCENARIOS))
        if unknown:
            parser.error(f"unknown scenarios: {', '.join(unknown)}")
        # Always in the same order, which is also the order inputs are
        # shared in.
        args.only = [name for name in SCENARIOS if name in chosen]
    else:
        args.only = list(SCENARIOS)
    return args


def main(argv=None):
    args = parse_args(argv)
    if args.list:
        for name, text in SCENARIOS.items():
            print(f"{name:<8} {text}")
        return 0
    mode = "quick" if args.quick else "full"
    settings = dict(QUICK if args.quick else FULL)
    if args.rounds:
        for key in settings:
            if key.endswith("_rounds"):
                settings[key] = args.rounds
    if args.source:
        source = {"url": args.source, "ref": args.source_ref, "commit": None, "name": "the source"}
    elif args.quick:
        source = {"url": str(ROOT), "ref": "HEAD", "commit": None, "name": "this repository"}
    else:
        source = dict(PINNED)

    git = shutil.which(args.git)
    if git is None:
        print(f"bench: {args.git} not found", file=sys.stderr)
        return 1
    braid = args.braid.resolve() if args.braid else build_braid()

    if args.workdir:
        args.workdir.mkdir(parents=True, exist_ok=True)
        work = Path(tempfile.mkdtemp(prefix="braid-bench-", dir=args.workdir))
    else:
        work = Path(tempfile.mkdtemp(prefix="braid-bench-"))
    work = work.resolve()
    free = shutil.disk_usage(work).free
    if free < NEEDED_SPACE[mode]:
        remove(work)
        print(
            f"bench: the work directory has {free >> 20} MiB free, and a {mode} run needs "
            f"{NEEDED_SPACE[mode] >> 20} MiB; pass --workdir elsewhere",
            file=sys.stderr,
        )
        return 1

    GATE.limit = args.max_load
    tools = Tools(braid, git)
    ctx = Context(tools, settings, work, source, args.keep)
    started = datetime.datetime.now(datetime.timezone.utc)
    load_before = os.getloadavg() if hasattr(os, "getloadavg") else None
    rows = []
    errors = {}
    try:
        for position, name in enumerate(args.only):
            print(f"{name}: {SCENARIOS[name]}", file=sys.stderr, flush=True)
            try:
                rows.extend(RUNNERS[name](ctx))
            except Failure as err:
                print(f"bench: {name} failed: {err}", file=sys.stderr, flush=True)
                errors[name] = str(err)
            except Exception as err:  # a bug here, or in braid's output
                traceback.print_exc()
                errors[name] = f"{type(err).__name__}: {err}"
            ctx.drop(set(args.only[position + 1 :]))
    finally:
        if not args.keep:
            remove(work)
        else:
            print(f"inputs kept in {work}", file=sys.stderr)

    print_table(rows)
    if args.markdown:
        print_markdown(rows)
    for name, err in errors.items():
        print(f"FAILED {name}: {err}")

    results = {
        "format": 1,
        "mode": mode,
        "started": started.isoformat(timespec="seconds"),
        "finished": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
        "scenarios": args.only,
        "settings": settings,
        "braid": describe_build(git),
        "git": subprocess.run(
            [git, "--version"], capture_output=True, text=True, env=tools.env
        ).stdout.strip(),
        "system": platform.system(),
        "cpus": os.cpu_count(),
        "load_before": load_before,
        "load_after": os.getloadavg() if hasattr(os, "getloadavg") else None,
        "max_load": GATE.limit,
        "seconds_waited_for_load": round(GATE.waited, 1),
        "rounds_run_above_max_load": GATE.gave_up,
        "rows": [row.to_json() for row in rows],
        "extra": ctx.extra,
        "errors": errors,
    }
    RESULTS.mkdir(parents=True, exist_ok=True)
    stamp = started.strftime("%Y%m%d-%H%M%S")
    path = RESULTS / f"{stamp}-{mode}.json"
    path.write_text(json.dumps(results, indent=2) + "\n")
    print(f"raw measurements: {path.relative_to(ROOT)}")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
