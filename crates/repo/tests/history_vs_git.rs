// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Differential tests of the history queries, with the git command line as
//! the oracle: on fixture repositories built here, on random histories, and
//! on the history of this repository, imported into braid and mapped back
//! through the git map.
//!
//! - `log -- <path>` against `git log --first-parent --format=%H -- <path>`,
//!   for every path a history ever had, its directories, and paths it never
//!   had;
//! - merge bases against `git merge-base --all`;
//! - blame against `git blame --first-parent --line-porcelain` (no `-w`,
//!   `-M` or `-C`), line by line, for every file at several revisions.
//!
//! Blame differs from git's in two documented ways, and the tests check that
//! it differs in no other:
//!
//! - Git's blame follows a file across whole-file renames (it has no switch
//!   to turn that off), and braid's does not. A line that git traces to a
//!   changeset older than the one where the path last appeared on the
//!   first-parent chain is, for braid, that changeset's, and git must have
//!   reached it under another file name.
//! - Git's diff and braid's can differ: when a change has several shortest
//!   line diffs, they may keep different copies of lines that repeat, such
//!   as closing braces; and neither always finds a shortest diff, so on some
//!   changes one inserts a few more lines than the other. Where the two
//!   blames differ, the tests walk the file's versions along the first-parent
//!   chain: replaying git's own diffs (`git diff -U0`) must give git's blame
//!   line for line; braid's blame at each version must be braid's own diff
//!   applied to its blame at the version before, a diff that keeps only
//!   equal lines; and that diff must never insert fewer lines than the
//!   shortest one (`git diff --minimal -U0`), which would be a bug. Files
//!   where the two diffs insert as many lines at every version count as
//!   realigned, the others as heuristic divergences.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use braid_engine::{LineChange, read_changes, read_changeset};
use braid_format::{EntryKind, ObjectId};
use braid_git::{GitMap, GitOid, ImportOptions};
use braid_repo::{Repo, RepoHistory};
use tempfile::TempDir;

/// Variables that would point git at another repository or change what it
/// reads.
const LEAKY_VARIABLES: [&str; 12] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_REPLACE_REF_BASE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_GRAFT_FILE",
    "GIT_SHALLOW_FILE",
];

/// A git command in `dir`, without the user's or the system's
/// configuration, with fixed identities, the clock at `seconds`, and
/// literal pathspecs.
fn git_command(dir: &Path, seconds: i64) -> Command {
    let mut command = Command::new("git");
    command.current_dir(dir);
    for variable in LEAKY_VARIABLES {
        command.env_remove(variable);
    }
    let date = format!("{seconds} +0200");
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env("GIT_AUTHOR_NAME", "Ada Lovelace")
        .env("GIT_AUTHOR_EMAIL", "ada@example.org")
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_NAME", "Charles Babbage")
        .env("GIT_COMMITTER_EMAIL", "charles@example.org")
        .env("GIT_COMMITTER_DATE", &date)
        .args([
            "-c",
            "init.defaultBranch=main",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "core.autocrlf=false",
            "-c",
            "core.quotePath=false",
            "-c",
            "gc.auto=0",
        ]);
    command
}

/// Runs git and returns its standard output; `None` when it exits with 1,
/// as `git merge-base` does when there is no answer.
fn run(mut command: Command) -> Option<Vec<u8>> {
    let output = command
        .stdin(Stdio::null())
        .output()
        .expect("git must be installed to run these tests");
    match output.status.code() {
        Some(0) => Some(output.stdout),
        Some(1) if output.stderr.is_empty() => None,
        _ => panic!(
            "{command:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ),
    }
}

fn text(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap()
}

/// A git repository with a work tree, in a temporary directory.
struct Fixture {
    _dir: TempDir,
    work: PathBuf,
    clock: i64,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        std::fs::create_dir(&work).unwrap();
        let mut fixture = Fixture {
            _dir: dir,
            work,
            clock: 1_700_000_000,
        };
        fixture.git(&["init", "-q"]);
        fixture
    }

    fn git(&mut self, args: &[&str]) -> String {
        let mut command = git_command(&self.work, self.clock);
        command.args(args);
        let out = run(command).unwrap_or_else(|| panic!("git {args:?} exited with 1"));
        text(out).trim().to_owned()
    }

    fn write(&self, path: &str, content: &str) {
        let path = self.work.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn remove(&self, path: &str) {
        std::fs::remove_file(self.work.join(path)).unwrap();
    }

    /// Makes a file executable, in the work tree where the file system can
    /// say so, and in the index.
    fn make_executable(&mut self, path: &str) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file = self.work.join(path);
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        self.git(&["update-index", "--chmod=+x", path]);
    }

    /// Commits everything in the work tree, even nothing.
    fn commit(&mut self, message: &str) -> String {
        self.clock += 60;
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--allow-empty", "-m", message]);
        self.git(&["rev-parse", "HEAD"])
    }

    /// Merges `others` into the current branch with a merge commit.
    fn merge(&mut self, others: &[&str], message: &str) -> String {
        self.clock += 60;
        let mut args = vec!["merge", "-q", "--no-ff", "--no-edit", "-m", message];
        args.extend(others);
        self.git(&args);
        self.git(&["rev-parse", "HEAD"])
    }

    fn checkout(&mut self, branch: &str) {
        self.git(&["checkout", "-q", branch]);
    }

    fn branch(&mut self, branch: &str) {
        self.git(&["checkout", "-q", "-b", branch]);
    }
}

/// Lines `1..=n` of a file, each naming `tag`.
fn lines(tag: &str, n: usize) -> String {
    (1..=n).map(|i| format!("{tag} {i}\n")).collect()
}

/// Builds a history with everything blame and path history meet: edits,
/// renames, deletions and re-additions, a file that becomes a directory,
/// mode changes, an empty commit, merges whose side branches change files,
/// an octopus merge, criss-cross merges, and blocks of lines that move and
/// repeat.
fn build_fixture() -> Fixture {
    let mut f = Fixture::new();
    f.write("README.md", &lines("readme", 6));
    f.write("src/lib.rs", &lines("lib", 12));
    f.write("src/util.rs", &lines("util", 10));
    f.write("docs/a.txt", &lines("a", 5));
    f.write("docs/read me.txt", "spaces\n");
    f.write("notes", "a file for now\n");
    f.write("caf\u{e9}.txt", &lines("caf\u{e9}", 3));
    f.commit("First");

    let lib = lines("lib", 12).replace("lib 3\n", "LIB 3\n") + "lib 13\n";
    f.write("src/lib.rs", &lib);
    f.commit("Edit lib");

    f.remove("docs/a.txt");
    f.commit("Remove a");

    // A side branch edits util and adds a file; main edits the README.
    f.branch("side");
    f.write(
        "src/util.rs",
        &lines("util", 10).replace("util 5\n", "side 5\n"),
    );
    f.write("side.txt", &lines("side", 3));
    f.commit("Side work");
    f.checkout("main");
    f.write("README.md", &(lines("readme", 6) + "readme 7\n"));
    f.commit("Readme grows");
    f.merge(&["side"], "Merge side");

    // A rename with an edit, and a rename of a whole directory.
    f.git(&["mv", "src/util.rs", "src/helpers.rs"]);
    let helpers = lines("util", 10).replace("util 5\n", "side 5\n") + "helpers 11\n";
    f.write("src/helpers.rs", &helpers);
    f.commit("Rename util to helpers");
    f.git(&["mv", "docs", "manual"]);
    f.commit("Rename docs to manual");

    // Deleted earlier, added again with some of the old lines.
    f.write("docs/a.txt", &(lines("a", 3) + "new a 4\n"));
    f.commit("Add a again");

    // A file that becomes a directory.
    f.remove("notes");
    f.write("notes/today.txt", &lines("today", 2));
    f.commit("Notes become a directory");

    // A mode change only, then an empty commit.
    f.make_executable("src/lib.rs");
    f.commit("Make lib executable");
    f.commit("Nothing");

    // An octopus merge of three branches.
    for (branch, file) in [
        ("o1", "o1.txt"),
        ("o2", "src/o2.rs"),
        ("o3", "manual/o3.md"),
    ] {
        f.branch(branch);
        f.write(file, &lines(branch, 4));
        f.commit(&format!("Work on {branch}"));
        f.checkout("main");
    }
    f.write("src/lib.rs", &(lib.clone() + "lib 14\n"));
    f.commit("Lib grows");
    f.merge(&["o1", "o2", "o3"], "Octopus");

    // Criss-cross: x and y merge each other.
    f.branch("x");
    f.write("x.txt", &lines("x", 3));
    let x1 = f.commit("x 1");
    f.checkout("main");
    f.branch("y");
    f.write("y.txt", &lines("y", 3));
    let y1 = f.commit("y 1");
    f.merge(&[x1.as_str()], "y merges x");
    f.checkout("x");
    f.merge(&[y1.as_str()], "x merges y");
    f.write("x.txt", &(lines("x", 3) + "x 4\n"));
    f.commit("x 2");
    f.checkout("y");
    f.write("y.txt", &(lines("y", 3) + "y 4\n"));
    f.commit("y 2");
    f.checkout("main");
    f.merge(&["x"], "Merge x");
    f.merge(&["y"], "Merge y");

    // Blocks that move, lines that repeat, a line that loses its newline.
    let blocks = "fn one() {\n    1\n}\n\nfn two() {\n    2\n}\n\nfn three() {\n    3\n}\n";
    f.write("src/blocks.rs", blocks);
    f.commit("Blocks");
    let swapped = "fn three() {\n    3\n}\n\nfn one() {\n    1\n}\n\nfn two() {\n    2\n}\n";
    f.write("src/blocks.rs", swapped);
    f.commit("Move three up");
    f.write(
        "src/blocks.rs",
        &(swapped.to_owned() + "\nfn one() {\n    1\n}\n"),
    );
    f.commit("Repeat one");
    f.write("README.md", &(lines("readme", 6) + "readme 7"));
    f.commit("README without a final newline");
    f.write(
        "src/lib.rs",
        &lib.replace("lib 7\n", "").replace("lib 9\n", "LIB 9\n"),
    );
    f.commit("Lib shrinks");
    f
}

/// A braid repository with a git repository imported, and the map back.
struct Imported {
    _dir: TempDir,
    repo: Repo,
    map: GitMap,
}

impl Imported {
    /// Imports every branch and tag.
    fn all(git: &Path) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path()).unwrap();
        repo.git_import(git, "test").unwrap();
        let map = repo.git_map().unwrap();
        Imported {
            _dir: dir,
            repo,
            map,
        }
    }

    /// Imports what `HEAD` reaches.
    fn head(git: &Path) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path()).unwrap();
        let mut map = GitMap::new();
        braid_git::import(
            git,
            repo.store(),
            &mut map,
            &ImportOptions {
                refs: vec![b"HEAD".to_vec()],
            },
        )
        .unwrap();
        Imported {
            _dir: dir,
            repo,
            map,
        }
    }

    fn changeset(&self, commit: &str) -> ObjectId {
        let oid: GitOid = commit
            .parse()
            .unwrap_or_else(|_| panic!("{commit:?} is not a commit id"));
        self.map
            .changeset(&oid)
            .unwrap_or_else(|| panic!("commit {commit} was not imported"))
    }
}

/// What the comparisons found.
#[derive(Debug, Default)]
struct Tally {
    paths: usize,
    merge_bases: usize,
    blamed_files: usize,
    blamed_lines: usize,
    /// Lines git traced across a rename, which braid gives to the changeset
    /// where the path appeared.
    renamed_lines: usize,
    /// Files and lines where braid and git differ while their diffs insert
    /// as many lines at every version: they kept different copies of equal
    /// lines.
    realigned_files: usize,
    realigned_lines: usize,
    /// Files and lines where braid and git differ and, at some version,
    /// their diffs insert different numbers of lines, none fewer than the
    /// shortest diff.
    heuristic_files: usize,
    heuristic_lines: usize,
    /// Versions whose blame was checked against braid's own diff.
    versions_checked: usize,
    /// The files that differ, as `path at revision`, and why.
    differences: Vec<String>,
}

/// `log -- <path>` for each path, against git.
fn compare_path_logs(
    git: &Path,
    imported: &Imported,
    history: &mut RepoHistory<'_>,
    revision: &str,
    paths: &BTreeSet<String>,
    tally: &mut Tally,
) {
    let head = imported.changeset(&rev_parse(git, revision));
    for path in paths {
        let mut command = git_command(git, 0);
        command.args(["log", "--first-parent", "--format=%H", revision, "--", path]);
        let expected: Vec<ObjectId> = text(run(command).unwrap())
            .lines()
            .map(|commit| imported.changeset(commit))
            .collect();
        let braid_path = if path == "." { "" } else { path.as_str() };
        let found = history
            .path_log(head, &[braid_path.as_bytes()], usize::MAX)
            .unwrap();
        assert_eq!(found, expected, "log {revision} -- {path}");
        tally.paths += 1;
    }
}

/// Merge bases of each pair, against git.
fn compare_merge_bases(
    git: &Path,
    imported: &Imported,
    history: &mut RepoHistory<'_>,
    pairs: &[(String, String)],
    tally: &mut Tally,
) {
    for (a, b) in pairs {
        let mut command = git_command(git, 0);
        command.args(["merge-base", "--all", a, b]);
        let mut expected: Vec<ObjectId> = run(command)
            .map(text)
            .unwrap_or_default()
            .lines()
            .map(|commit| imported.changeset(commit))
            .collect();
        expected.sort();
        let (x, y) = (
            imported.changeset(&rev_parse(git, a)),
            imported.changeset(&rev_parse(git, b)),
        );
        assert_eq!(
            history.merge_bases(x, y).unwrap(),
            expected,
            "merge bases of {a} and {b}"
        );
        // The walking version agrees too.
        assert_eq!(
            braid_engine::merge_bases(imported.repo.store(), x, y).unwrap(),
            expected
        );
        tally.merge_bases += 1;
    }
}

/// One line of `git blame --line-porcelain`: the commit and the file name
/// the line has there.
struct GitLine {
    commit: String,
    filename: String,
}

fn git_blame(git: &Path, revision: &str, path: &str) -> Vec<GitLine> {
    let mut command = git_command(git, 0);
    command.args([
        "blame",
        "--first-parent",
        "--line-porcelain",
        revision,
        "--",
        path,
    ]);
    let output = run(command).unwrap();
    let mut lines = Vec::new();
    let mut commit = None;
    let mut filename = None;
    for line in output.split(|byte| *byte == b'\n') {
        if line.first() == Some(&b'\t') {
            lines.push(GitLine {
                commit: commit.take().unwrap(),
                filename: filename.take().unwrap(),
            });
        } else if commit.is_none()
            && line.len() > 41
            && line[40] == b' '
            && line[..40].iter().all(u8::is_ascii_hexdigit)
        {
            commit = Some(text(line[..40].to_vec()));
        } else if let Some(name) = line.strip_prefix(b"filename ") {
            filename = Some(text(name.to_vec()));
        }
    }
    lines
}

/// The versions of a file along the first-parent chain of `at`, oldest
/// first: each changeset that set it since it last appeared there, where
/// its first parent had no file, with the blob it set. Found by reading
/// changesets, without derived data.
fn versions(repo: &Repo, at: ObjectId, path: &str) -> Vec<(ObjectId, ObjectId)> {
    let objects = repo.store();
    let file = |kind: &EntryKind| {
        matches!(
            kind,
            EntryKind::File { .. } | EntryKind::Executable { .. } | EntryKind::Symlink { .. }
        )
    };
    let mut found = Vec::new();
    let mut current = at;
    loop {
        let changeset = read_changeset(objects, current).unwrap();
        let changes = read_changes(objects, current, &changeset).unwrap();
        let parent = changeset.parents.first().copied();
        let set = changes
            .iter()
            .find(|change| change.path == path.as_bytes())
            .and_then(|change| change.op.to_entry())
            .filter(file);
        if let Some(kind) = set {
            let (EntryKind::File { blob, .. }
            | EntryKind::Executable { blob, .. }
            | EntryKind::Symlink { blob, .. }) = kind
            else {
                unreachable!("only files are kept")
            };
            found.push((current, blob));
            let before = parent.and_then(|parent| {
                let root = read_changeset(objects, parent).unwrap().root;
                braid_engine::entry_at(objects, root, path.as_bytes()).unwrap()
            });
            if !before.as_ref().is_some_and(file) {
                found.reverse();
                return found;
            }
        }
        current = parent.unwrap_or_else(|| panic!("{path} never appeared"));
    }
}

/// The hunks of `git diff -U0` between two blobs, or of `git diff --minimal
/// -U0`: for each, the lines it replaces and the lines it puts there,
/// counted from 0.
fn git_diff_hunks(
    git: &Path,
    old: &GitOid,
    new: &GitOid,
    minimal: bool,
) -> Vec<(Range<usize>, Range<usize>)> {
    if old == new {
        return Vec::new();
    }
    let mut command = git_command(git, 0);
    command.args([
        "diff",
        "-U0",
        "--text",
        "--no-color",
        "--no-ext-diff",
        "--indent-heuristic",
    ]);
    if minimal {
        command.arg("--minimal");
    }
    command.args([old.to_hex(), new.to_hex()]);
    let range = |spec: &str| -> Range<usize> {
        let (start, len) = match spec.split_once(',') {
            Some((start, len)) => (start.parse::<usize>().unwrap(), len.parse().unwrap()),
            None => (spec.parse().unwrap(), 1),
        };
        if len == 0 {
            start..start
        } else {
            start - 1..start - 1 + len
        }
    };
    text(run(command).unwrap())
        .lines()
        .filter_map(|line| line.strip_prefix("@@ -"))
        .map(|header| {
            let (before, rest) = header.split_once(" +").unwrap();
            let after = rest.split_once(" @@").unwrap().0;
            (range(before), range(after))
        })
        .collect()
}

/// The origins of the lines of a new version, from those of the old one
/// and the hunks that make it, for the changeset `id` that made it.
fn replay(
    old: &[ObjectId],
    new_lines: usize,
    hunks: &[(Range<usize>, Range<usize>)],
    id: ObjectId,
) -> Vec<ObjectId> {
    let mut origins = Vec::with_capacity(new_lines);
    let mut kept = 0;
    for (before, after) in hunks {
        origins.extend_from_slice(&old[kept..before.start]);
        origins.extend(std::iter::repeat_n(id, after.len()));
        kept = before.end;
    }
    origins.extend_from_slice(&old[kept..]);
    assert_eq!(origins.len(), new_lines);
    origins
}

/// The lines of a blob, as blame counts them: each ends after a `\n`, and
/// a last line without one is a line too.
fn split_lines(content: &[u8]) -> Vec<&[u8]> {
    let mut lines: Vec<&[u8]> = content.split_inclusive(|byte| *byte == b'\n').collect();
    if content.is_empty() {
        lines.clear();
    }
    lines
}

/// The lines of the new version that braid's line diff keeps, each paired
/// with a line of the old version, must be equal to it.
fn check_kept_lines(old: &[u8], new: &[u8], changes: &[LineChange], what: &str) {
    let (old, new) = (split_lines(old), split_lines(new));
    let (mut at_old, mut at_new) = (0, 0);
    for change in changes {
        assert_eq!(
            change.old.start - at_old,
            change.new.start - at_new,
            "{what}: as many kept lines on both sides"
        );
        while at_old < change.old.start {
            assert_eq!(old[at_old], new[at_new], "{what}: a kept line is equal");
            at_old += 1;
            at_new += 1;
        }
        (at_old, at_new) = (change.old.end, change.new.end);
    }
    assert_eq!(
        &old[at_old..],
        &new[at_new..],
        "{what}: the kept end is equal"
    );
}

/// Braid's blame of every version of a file, oldest first, checked against
/// braid's own line diff: the first version is all its own, and each other
/// one is the diff applied to the blame of the version before, a diff that
/// keeps only equal lines. Returns each version's content and blame.
fn check_versions(
    imported: &Imported,
    history: &mut RepoHistory<'_>,
    path: &str,
    versions: &[(ObjectId, ObjectId)],
    tally: &mut Tally,
) -> Vec<(Vec<u8>, Vec<ObjectId>)> {
    let mut checked: Vec<(Vec<u8>, Vec<ObjectId>)> = Vec::new();
    for (changeset, blob) in versions {
        let what = format!("{path} at {changeset}");
        let content = braid_engine::read_blob(imported.repo.store(), *blob).unwrap();
        let lines = split_lines(&content).len();
        let own = history
            .blame(*changeset, path.as_bytes())
            .unwrap()
            .unwrap()
            .origins;
        assert_eq!(own.len(), lines, "{what}: one origin per line");
        match checked.last() {
            None => assert!(
                own.iter().all(|origin| origin == changeset),
                "{what}: a new file is all its own"
            ),
            Some((old_content, old_own)) => {
                let changes = braid_engine::line_diff(old_content, &content);
                check_kept_lines(old_content, &content, &changes, &what);
                let hunks: Vec<(Range<usize>, Range<usize>)> = changes
                    .iter()
                    .map(|change| (change.old.clone(), change.new.clone()))
                    .collect();
                assert_eq!(
                    own,
                    replay(old_own, lines, &hunks, *changeset),
                    "{what}: braid's blame follows braid's diff"
                );
            }
        }
        tally.versions_checked += 1;
        checked.push((content, own));
    }
    checked
}

/// Lines a diff's hunks insert.
fn inserts(hunks: &[(Range<usize>, Range<usize>)]) -> usize {
    hunks.iter().map(|(_, after)| after.len()).sum()
}

/// Blame of each file at `revision`, against git, line by line.
///
/// Braid's blame of every version of the file is checked against braid's
/// own diff. Where braid's blame and git's differ, the documented rules must
/// explain it (see the module documentation): git's own diffs, replayed,
/// give git's blame, and braid's diff never inserts fewer lines than the
/// shortest one.
fn compare_blames(
    git: &Path,
    imported: &Imported,
    history: &mut RepoHistory<'_>,
    revision: &str,
    files: &[String],
    tally: &mut Tally,
) {
    let at = imported.changeset(revision);
    let mut heights: HashMap<ObjectId, u64> = HashMap::new();
    for path in files {
        let expected = git_blame(git, revision, path);
        let blame = history
            .blame(at, path.as_bytes())
            .unwrap()
            .unwrap_or_else(|| panic!("no blame for {path} at {revision}"));
        assert_eq!(
            blame.origins.len(),
            expected.len(),
            "lines of {path} at {revision}"
        );
        let versions = versions(&imported.repo, at, path);
        let checked = check_versions(imported, history, path, &versions, tally);
        assert_eq!(
            checked.last().map(|(_, own)| own),
            Some(&blame.origins),
            "{path} at {revision}: the blame of its last version"
        );
        let appeared = versions[0].0;
        let mut height = |id: ObjectId| -> u64 {
            *heights
                .entry(id)
                .or_insert_with(|| history.graph(id).unwrap().height)
        };
        let floor = height(appeared);
        let mut wanted = Vec::with_capacity(expected.len());
        for (n, line) in expected.iter().enumerate() {
            let git_origin = imported.changeset(&line.commit);
            wanted.push(if height(git_origin) >= floor {
                git_origin
            } else {
                // Git followed a rename; braid starts where the path appeared.
                assert_ne!(
                    &line.filename,
                    path,
                    "line {} of {path} at {revision}: git found it before the path appeared, under the same name",
                    n + 1
                );
                tally.renamed_lines += 1;
                appeared
            });
        }
        tally.blamed_files += 1;
        tally.blamed_lines += expected.len();
        let differing = blame
            .origins
            .iter()
            .zip(&wanted)
            .filter(|(origin, wanted)| origin != wanted)
            .count();
        if differing == 0 {
            continue;
        }

        // Git's blame, replayed from git's own diffs, and the lengths of the
        // two diffs at every version.
        let mut replayed: Vec<ObjectId> = Vec::new();
        let mut same_lengths = true;
        for (i, (changeset, blob)) in versions.iter().enumerate() {
            let lines = split_lines(&checked[i].0).len();
            if i == 0 {
                replayed = vec![*changeset; lines];
                continue;
            }
            let what = format!("{path} at {changeset}");
            let old = imported.map.git_blob(&versions[i - 1].1).unwrap();
            let new = imported.map.git_blob(blob).unwrap();
            let inserted = braid_engine::line_diff(&checked[i - 1].0, &checked[i].0)
                .iter()
                .map(|change| change.new.len())
                .sum::<usize>();
            let fewest = inserts(&git_diff_hunks(git, &old, &new, true));
            assert!(
                inserted >= fewest,
                "{what}: braid's diff inserts {inserted} lines, the shortest diff {fewest}"
            );
            let git_hunks = git_diff_hunks(git, &old, &new, false);
            same_lengths &= inserted == inserts(&git_hunks);
            replayed = replay(&replayed, lines, &git_hunks, *changeset);
        }
        assert_eq!(
            replayed, wanted,
            "{path} at {revision}: git's own diffs, replayed, give git's blame"
        );
        let why = if same_lengths {
            tally.realigned_files += 1;
            tally.realigned_lines += differing;
            "realigned"
        } else {
            tally.heuristic_files += 1;
            tally.heuristic_lines += differing;
            "heuristic"
        };
        let short = &revision[..revision.len().min(12)];
        tally
            .differences
            .push(format!("{path} at {short}: {differing} lines, {why}"));
    }
}

/// The commit a revision names.
fn rev_parse(git: &Path, revision: &str) -> String {
    let mut command = git_command(git, 0);
    command.args(["rev-parse", "--verify", "--end-of-options"]);
    command.arg(format!("{revision}^{{commit}}"));
    text(run(command).unwrap()).trim().to_owned()
}

/// Every path a history ever had, from `git log`, with their directories.
fn every_path(git: &Path, revision: &str) -> BTreeSet<String> {
    let mut command = git_command(git, 0);
    command.args(["log", "--format=", "--name-only", "--no-renames", revision]);
    let mut paths = BTreeSet::new();
    for path in text(run(command).unwrap())
        .lines()
        .filter(|l| !l.is_empty())
    {
        let mut dir = path;
        paths.insert(path.to_owned());
        while let Some(slash) = dir.rfind('/') {
            dir = &dir[..slash];
            paths.insert(dir.to_owned());
        }
    }
    paths
}

/// The files at a revision.
fn files_at(git: &Path, revision: &str) -> Vec<String> {
    let mut command = git_command(git, 0);
    command.args(["ls-tree", "-r", "--name-only", "-z", revision]);
    text(run(command).unwrap())
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The commits of the first-parent chain of a revision, newest first.
fn first_parents(git: &Path, revision: &str) -> Vec<String> {
    let mut command = git_command(git, 0);
    command.args(["rev-list", "--first-parent", revision]);
    text(run(command).unwrap())
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Every commit, newest first.
fn all_commits(git: &Path, revisions: &[&str]) -> Vec<String> {
    let mut command = git_command(git, 0);
    command.arg("rev-list").args(revisions);
    text(run(command).unwrap())
        .lines()
        .map(str::to_owned)
        .collect()
}

/// About `target` pairs of commits, spread over all of them, each once.
fn spread_pairs(commits: &[String], target: usize) -> Vec<(String, String)> {
    let n = commits.len();
    let total = n * (n + 1) / 2;
    let every = total.div_ceil(target).max(1);
    let mut pairs = Vec::new();
    let mut k = 0;
    for i in 0..n {
        for j in i..n {
            if k % every == 0 {
                pairs.push((commits[i].clone(), commits[j].clone()));
            }
            k += 1;
        }
    }
    pairs
}

#[test]
fn a_fixture_history_matches_git() {
    let fixture = build_fixture();
    let git = fixture.work.as_path();
    let imported = Imported::all(git);
    let mut history = imported.repo.history();
    let mut tally = Tally::default();

    // Path history, for every path and directory, and paths never there.
    let mut paths = every_path(git, "main");
    paths.extend([
        "nothing".to_owned(),
        "src/nothing.rs".to_owned(),
        ".".to_owned(),
    ]);
    for revision in ["main", "main~3", "x", "y"] {
        compare_path_logs(git, &imported, &mut history, revision, &paths, &mut tally);
    }

    // Merge bases of pairs of every commit, criss-cross ones included.
    let commits = all_commits(git, &["--all"]);
    let mut pairs = spread_pairs(&commits, 150);
    let x = imported.repo.resolve("x").unwrap();
    let y = imported.repo.resolve("y").unwrap();
    assert_eq!(history.merge_bases(x, y).unwrap().len(), 2, "criss-cross");
    pairs.push(("x".to_owned(), "y".to_owned()));
    compare_merge_bases(git, &imported, &mut history, &pairs, &mut tally);

    // Blame of every file at every revision of main's first-parent chain.
    for revision in first_parents(git, "main") {
        let files = files_at(git, &revision);
        compare_blames(git, &imported, &mut history, &revision, &files, &mut tally);
    }
    history.flush().unwrap();
    eprintln!("{tally:?}");
    assert!(tally.renamed_lines > 0, "the fixture has renames");
}

/// Commits of a random history built with plumbing: `count` commits whose
/// parents are picked by a fixed generator, many of them merges, some of
/// them octopus merges, some criss-cross.
fn random_history(fixture: &mut Fixture, seed: u64, count: usize) -> Vec<String> {
    let mut state = seed | 1;
    let mut next = |below: usize| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % below as u64) as usize
    };
    let tree = fixture.git(&["mktree"]);
    let mut commits: Vec<String> = Vec::new();
    let mut has_child = BTreeSet::new();
    for n in 0..count {
        let parents: Vec<usize> = match (commits.len(), next(10)) {
            (0, _) | (_, 0) => Vec::new(),
            (len, 1..=4) => vec![next(len)],
            (len, 5..=7) => vec![next(len), next(len)],
            (len, 8) => (0..2 + next(3)).map(|_| next(len)).collect(),
            (len, _) if len >= 2 => {
                // Two merges of the same pair, in both orders.
                let (a, b) = (next(len), next(len));
                fixture.clock += 1;
                let message = format!("{n} criss");
                let first = fixture.git(&[
                    "commit-tree",
                    &tree,
                    "-p",
                    &commits[a],
                    "-p",
                    &commits[b],
                    "-m",
                    &message,
                ]);
                has_child.extend([a, b]);
                commits.push(first);
                vec![b, a]
            }
            _ => Vec::new(),
        };
        fixture.clock += 1;
        let mut args = vec!["commit-tree".to_owned(), tree.clone()];
        for parent in &parents {
            args.extend(["-p".to_owned(), commits[*parent].clone()]);
            has_child.insert(*parent);
        }
        args.extend(["-m".to_owned(), format!("{n}")]);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let commit = fixture.git(&args);
        commits.push(commit);
    }
    // A branch for every commit nothing descends from, so that the import
    // brings them all.
    for (n, commit) in commits.iter().enumerate() {
        if !has_child.contains(&n) {
            fixture.git(&["update-ref", &format!("refs/heads/tip{n}"), commit]);
        }
    }
    commits
}

#[test]
fn merge_bases_of_random_histories_match_git() {
    let mut tally = Tally::default();
    for seed in [7, 1234, 98765] {
        let mut fixture = Fixture::new();
        let commits = random_history(&mut fixture, seed, 40);
        let git = fixture.work.clone();
        let imported = Imported::all(&git);
        let mut history = imported.repo.history();
        let pairs = spread_pairs(&commits, 120);
        compare_merge_bases(&git, &imported, &mut history, &pairs, &mut tally);
    }
    eprintln!("{tally:?}");
}

/// The repository these tests live in, when it is a full git checkout.
fn this_repository() -> Option<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    if !root.join(".git").exists() {
        eprintln!("skipped: the sources are not in a git checkout");
        return None;
    }
    let mut command = git_command(&root, 0);
    command.args(["rev-parse", "--is-shallow-repository"]);
    if text(run(command).unwrap()).trim() != "false" {
        eprintln!("skipped: this checkout is shallow");
        return None;
    }
    Some(root)
}

#[test]
fn the_history_of_this_repository_matches_git() {
    let Some(root) = this_repository() else {
        return;
    };
    let head = text(
        run({
            let mut command = git_command(&root, 0);
            command.args(["rev-parse", "HEAD"]);
            command
        })
        .unwrap(),
    )
    .trim()
    .to_owned();
    let imported = Imported::head(&root);
    let mut history = imported.repo.history();
    let mut tally = Tally::default();

    let mut paths = every_path(&root, &head);
    paths.extend(["nothing".to_owned(), ".".to_owned()]);
    compare_path_logs(&root, &imported, &mut history, &head, &paths, &mut tally);

    let commits = all_commits(&root, &[&head]);
    let pairs = spread_pairs(&commits, 150);
    compare_merge_bases(&root, &imported, &mut history, &pairs, &mut tally);

    // Every file at HEAD, and some of the files at older revisions along the
    // first-parent chain.
    let chain = first_parents(&root, &head);
    compare_blames(
        &root,
        &imported,
        &mut history,
        &head,
        &files_at(&root, &head),
        &mut tally,
    );
    for revision in chain.iter().skip(1).step_by(6) {
        let files: Vec<String> = files_at(&root, revision).into_iter().step_by(4).collect();
        compare_blames(&root, &imported, &mut history, revision, &files, &mut tally);
    }
    history.flush().unwrap();
    eprintln!("{tally:?}");
    assert!(tally.blamed_lines > 0);
}

/// The paths of a git history and their map to braid, for debugging a
/// failure by hand: `cargo test -p elffuss-braid-repo --test
/// history_vs_git -- --ignored --nocapture`.
#[test]
#[ignore = "prints the fixture's history"]
fn print_the_fixture() {
    let fixture = build_fixture();
    let mut command = git_command(&fixture.work, 0);
    command.args(["log", "--graph", "--all", "--format=%h %s", "--name-status"]);
    eprintln!("{}", text(run(command).unwrap()));
    let imported = Imported::all(&fixture.work);
    let mut by_commit = BTreeMap::new();
    for (commit, changeset) in imported.map.commits() {
        by_commit.insert(commit.to_hex(), changeset.short());
    }
    eprintln!("{by_commit:#?}");
}
