# Benchmarks against git

`run.py` does the same work with braid and with git on one machine and says
how long braid took as a fraction of git's time, or how large its store is
as a fraction of git's: **braid / git**. Below 1 means braid is faster or
smaller, above 1 that it is slower or larger.

Braid publishes only these ratios, together with this script. Absolute
times, sizes and the description of the machine stay on the machine that
measured them.

## Running it

```bash
python3 bench/run.py                    # every scenario, full size
python3 bench/run.py --quick            # small inputs: a smoke run, as CI does
python3 bench/run.py --only status,commit
python3 bench/run.py --list             # the scenarios
python3 bench/run.py --markdown         # also print the table as Markdown
```

It needs a Unix-like system, Python 3.9 or newer (standard library only),
git, cargo and `du`. It builds braid with `cargo build --release` first;
`--braid PATH` uses a binary you already have. The full run fetches the
repository of the import scenario once from GitHub.

Everything happens in a new temporary directory, removed at the end:
`--workdir DIR` puts it elsewhere and `--keep` keeps it. The script checks
that there is room for its inputs there before it starts. Your git
configuration is never read or written: every git command runs with
`GIT_CONFIG_GLOBAL=/dev/null`, `GIT_CONFIG_NOSYSTEM=1` and a fixed identity.

The table goes to standard output. The raw measurements, every time of
every round, go to `bench/results/`, which git ignores.

## How it measures

- **Alternating order.** Every case runs braid and git in turn: braid, git,
  git, braid, braid, git, and so on; with three contestants, each takes
  every place once in every three rounds. A machine that grows warmer, or
  caches that fill, favour none of them.
- **Warm.** Each case first runs once or twice untimed, so that both tools
  start with the file system cache holding their files.
- **What an agent runs.** A time is the wall-clock time of the commands, from
  process start to exit, as an agent calling the command line would wait
  for them. When a case takes two commands, such as `git add` and
  `git commit`, their times add up.
- **Medians.** The ratio is braid's median over git's median. `spread` is the
  smallest and the largest ratio of a single round, which shows how noisy
  the machine was; `cpu` is the ratio of CPU time (user and system) instead
  of wall time, which differs when one tool uses more cores than the other.
- **Same work.** Before timing, each scenario checks that both tools do the
  same thing: the same files modified, the same number of commits made, the
  same changesets listed. A failed check is reported as an error, never as
  a number.
- **Git as installed.** Git runs with its defaults, except that automatic
  garbage collection and maintenance are off in the repositories the
  benchmark makes, so that they never land in a measurement, and that the
  file system monitor and the untracked cache are off unless a case turns
  them on.

## Scenarios

### import

The source is [tmux](https://github.com/tmux/tmux) at its 3.5a release,
10,287 commits, fetched once with git into a local bare repository whose
only branch is `main`. Three rounds of:

- **history only:** `braid init` and `braid git import <source>` against
  `git clone --no-local --bare <source>`;
- **history and a working copy:** the same followed by
  `braid workspace add`, against `git clone --no-local <source>`.

`--no-local` makes git go through its transport, building and indexing a
pack, instead of copying or hard-linking the source's files. Braid's import
also exports every commit again and compares the ids, so that the round
trip is bit for bit.

`--source URL_OR_PATH` (with `--source-ref REF`) imports another
repository; `--quick` imports this repository's own history.

### status

A generated tree of 100,000 files: three levels of directories, 25 files in
each directory, text of 100 to 3,000 bytes, the same bytes on every run.
Git checks it out from one commit, and braid imports that commit and checks
it out in a workspace. Nine rounds of `braid status` against `git status`,
twice: with git's untracked cache off, then with `core.untrackedCache=true`;
the file system monitor is off in both (no daemon). Two cases: a clean tree,
and 10 files spread over the tree with a line appended.

### commit

In the same tree, one file gets a line appended and is committed: nine
rounds of `braid commit` against `git add <file>` and `git commit`, a
different file each round. Braid has no staging area: its commit scans the
working copy, as a status does, and records what changed.

### wide

A single directory of 20,000 small files. Each round adds one new file at
the end of it and commits it, 200 rounds, three ways:

- git: `git add <file>` and `git commit`;
- braid in a workspace: `braid commit`;
- braid without a working copy: `braid change create --set <file>=@<copy>
  --land main`.

The table gives the ratio of each braid way to git, and then whether the
cost of one commit grows with the directory: the same series, 30 rounds,
in directories of 2,500, 5,000 and 10,000 files, and for each tool the cost
of one commit with 20,000 files divided by its cost with 2,500. 1 means the
cost does not grow with the directory; a cost proportional to it would give
8. The raw results also have the exponent of a fit of cost against size, and
how much the last quarter of the 200 commits differs from the first.

### agents

N = 4, 8 and 16 agents start at once. Each makes K = 10 commits, appending a
line to its own file, and lands each one before the next:

- braid: `braid commit` and `braid land`, each agent in its own workspace of
  one repository;
- git: `git commit -a` and `git push`, each agent in its own clone of one
  bare repository; a rejected push runs `git pull --rebase` and pushes
  again.

The time is the wall-clock time from the start until the last agent has
landed its last commit. Three rounds for each N, with new repositories each
time. The table also says how many times git retried a push, on average.

### store

After the import, `du -sk` of `.braid/store` against `du -sk` of the bare
clone after `git gc --aggressive --prune=now`. The raw results also have the
sum of the file sizes alone, and the size of the clone before the gc.

### history

On the imported repository, and on the bare clone after its gc:

- `braid log -n 1000` against `git log --first-parent -n 1000 --format=%H`;
- `braid log -- <path>` against `git log --first-parent -- <path>`, for the
  file that the most commits changed and for a file that one commit touched,
  both listing every commit that touched it;
- `braid blame <path>` against `git blame --first-parent <path>`.

Braid follows first parents, so git does too, and both list the same
changesets; the scenario checks it. A query that braid does not have yet is
skipped, and the table says so.

## Reading the ratios

- 0.5 means braid took half of git's time; 2 means twice as long. For the
  store, 3 means three times as many bytes on disk.
- A ratio holds for the machine it was measured on. Another file system,
  another number of cores, or another git version moves it, sometimes a lot:
  run the script on your machine before relying on a number.
- Look at `spread` before trusting a ratio. A wide spread means the machine
  was busy; run again with more rounds (`--rounds N`), or, on a machine
  shared with other work, with `--max-load N`, which waits before each timed
  round until the load average of the last minute is below N. It changes
  when rounds run, never what they measure, and the raw results say how
  long it waited.
- `--quick` inputs are far too small to compare anything. It only checks
  that every scenario still runs.

## What it does not measure

- Cold caches: every case runs with warm caches.
- Git with the file system monitor daemon, or with `feature.manyFiles`.
- Transfers between machines: braid has no server yet.
