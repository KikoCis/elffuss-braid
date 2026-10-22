// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! The hub commands of the `braid` binary where no hub answers: what is
//! refused before anything goes over the network, what a linked repository
//! does when its hub cannot be reached, and how `--local` and
//! `BRAID_HUB=off` keep landing and updating local; and `braid hub verify`
//! against a stand-in that gives the answers a hub would. Every test gets a
//! configuration directory of its own, and a token is never printed.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread::JoinHandle;

use serde_json::Value;

/// A home, a configuration directory and a repository, in a temporary
/// directory.
struct Place {
    _temp: tempfile::TempDir,
    base: PathBuf,
    root: PathBuf,
}

impl Place {
    fn new() -> Place {
        let temp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(temp.path()).unwrap();
        let root = base.join("repo");
        let place = Place {
            _temp: temp,
            base,
            root,
        };
        let out = place.braid(&place.base, &["init", "repo"], &[]);
        assert!(out.status.success(), "{}", text(&out.stderr));
        place
    }

    /// Runs braid in `dir`, with `env` on top of a clean environment.
    fn braid(&self, dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_braid"));
        command
            .current_dir(dir)
            .args(args)
            .env("HOME", self.base.join("home"))
            .env("XDG_CONFIG_HOME", self.base.join("config"))
            .env("BRAID_AUTHOR", "Ada <ada@example.org>")
            .env("BRAID_AGENT", "tester")
            .env_remove("BRAID_TOKEN")
            .env_remove("BRAID_HUB_URL")
            .env_remove("BRAID_HUB");
        for (key, value) in env {
            command.env(key, value);
        }
        command.output().unwrap()
    }

    /// Links the repository to a hub on this machine where nothing
    /// listens, and returns its URL.
    fn link_to_nowhere(&self) -> String {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let url = format!("http://127.0.0.1:{port}");
        fs::write(
            self.root.join(".braid/hub"),
            format!("{{\"url\": \"{url}\", \"repo\": \"ada/demo\"}}\n"),
        )
        .unwrap();
        url
    }

    /// Makes a workspace and commits a file in it.
    fn workspace_with_a_commit(&self, name: &str) -> PathBuf {
        let out = self.braid(&self.root, &["workspace", "add", name], &[]);
        assert!(out.status.success(), "{}", text(&out.stderr));
        let dir = self.root.join(".braid/work").join(name);
        self.commit(&dir, "a.txt", "one\n");
        dir
    }

    fn commit(&self, dir: &Path, path: &str, content: &str) {
        fs::write(dir.join(path), content).unwrap();
        let out = self.braid(dir, &["commit", "-m", path], &[]);
        assert!(out.status.success(), "{}", text(&out.stderr));
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Asserts that `out` failed with status 1, nothing on standard output,
/// and an error that says `expected`.
fn fails_with(out: &Output, expected: &str) {
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(out.stdout.is_empty(), "{}", text(&out.stdout));
    assert!(err.starts_with("braid: error: "), "{err}");
    assert!(err.contains(expected), "expected {expected:?} in {err}");
}

fn json(out: &Output) -> Value {
    assert!(out.status.success(), "{}", text(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn refused_before_any_request() {
    let p = Place::new();
    fails_with(
        &p.braid(&p.root, &["hub", "status"], &[]),
        "not linked to a hub",
    );
    fails_with(
        &p.braid(&p.root, &["hub", "fetch"], &[]),
        "not linked to a hub",
    );
    // Publishing needs a token, and nothing is linked without one.
    fails_with(
        &p.braid(&p.root, &["hub", "publish"], &[]),
        "publishing needs signing in",
    );
    assert!(!p.root.join(".braid/hub").exists());
    // A token is never sent in clear to another machine.
    fails_with(
        &p.braid(&p.base, &["login", "--hub", "http://hub.example.org"], &[]),
        "would send the token in clear",
    );
    fails_with(
        &p.braid(
            &p.root,
            &["hub", "publish"],
            &[("BRAID_HUB_URL", "http://hub.example.org")],
        ),
        "would send the token in clear",
    );
    fails_with(
        &p.braid(&p.base, &["hub", "clone", "nobody"], &[]),
        "is not a repository",
    );
    fails_with(
        &p.braid(
            &p.base,
            &["hub", "clone", "ada/demo", "--paths", "../etc"],
            &[],
        ),
        "is not a path prefix",
    );
    // A malformed token is refused without being repeated.
    let secret = "brd_aaaaaaaa_this-is-not-base32-and-must-not-show";
    fails_with(
        &p.braid(&p.root, &["hub", "publish"], &[("BRAID_TOKEN", secret)]),
        "BRAID_TOKEN is not a braid hub token",
    );
    let out = p.braid(&p.root, &["hub", "publish"], &[("BRAID_TOKEN", secret)]);
    assert!(!text(&out.stderr).contains("must-not-show"));
    // Logging out of a hub never signed in to says so.
    let out = p.braid(&p.base, &["logout", "--json"], &[]);
    let value = json(&out);
    assert_eq!(value["signed_in"], false);
    assert_eq!(value["hub"], "https://braidhub.elffuss.com");
    fails_with(
        &p.braid(&p.base, &["hub", "whoami"], &[]),
        "no one is signed in to https://braidhub.elffuss.com: run `braid login`, or set BRAID_TOKEN",
    );
    fails_with(
        &p.braid(
            &p.base,
            &["hub", "whoami", "--hub", "http://localhost:1"],
            &[],
        ),
        "run `braid login --hub http://localhost:1`",
    );
    fails_with(
        &p.braid(
            &p.root,
            &["hub", "publish"],
            &[
                (
                    "BRAID_TOKEN",
                    "brd_aaaaaaaa_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                ),
                ("BRAID_HUB_MAX_BODY", "lots"),
            ],
        ),
        "BRAID_HUB_MAX_BODY takes a number of bytes",
    );
}

#[test]
fn the_autopublish_answer_is_kept_for_every_project() {
    let p = Place::new();
    let ask = |args: &[&str]| -> String {
        let out = p.braid(&p.base, args, &[]);
        assert!(out.status.success(), "{}", text(&out.stderr));
        text(&out.stdout)
    };
    assert_eq!(ask(&["hub", "autopublish"]), "unset\n");
    let file = p.base.join("config/braid/hub.json");
    assert!(!file.exists());
    assert_eq!(ask(&["hub", "autopublish", "on"]), "on\n");
    assert_eq!(ask(&["hub", "autopublish"]), "on\n");
    let value = json(&p.braid(&p.root, &["hub", "autopublish", "--json"], &[]));
    assert_eq!(value, serde_json::json!({ "autopublish": "on" }));
    let value = json(&p.braid(&p.root, &["hub", "autopublish", "off", "--json"], &[]));
    assert_eq!(value["autopublish"], "off");
    let saved: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    assert_eq!(saved["autopublish"], false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
    let out = p.braid(&p.base, &["hub", "autopublish", "maybe"], &[]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    assert_eq!(ask(&["hub", "autopublish"]), "off\n");
}

/// The ```bash blocks and the code spans of a Markdown text: its lines that
/// may hold commands.
fn command_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut fenced = None;
    let mut prose = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            fenced = match fenced {
                None => Some(trimmed == "```bash"),
                Some(_) => None,
            };
            continue;
        }
        match fenced {
            Some(true) => lines.push(trimmed.to_owned()),
            Some(false) => {}
            None => prose.push(trimmed.to_owned()),
        }
    }
    let prose = prose.join(" ");
    lines.extend(prose.split('`').skip(1).step_by(2).map(str::to_owned));
    lines
}

/// Whether the help `shown` lists `word` among its commands.
fn lists_command(shown: &str, word: &str) -> bool {
    shown
        .lines()
        .skip_while(|line| line.trim() != "Commands:")
        .skip(1)
        .take_while(|line| line.starts_with("  "))
        .any(|line| line.split_whitespace().next() == Some(word))
}

/// Every braid command docs/hub-agents.md, the guide the hub serves at
/// /agents.md, gives exists, with every flag it is given.
#[test]
fn every_braid_command_in_the_hub_guide_exists_with_its_flags() {
    let guide =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/hub-agents.md"))
            .unwrap();
    let mut commands = Vec::new();
    let mut flags = Vec::new();
    for line in command_lines(&guide) {
        let line = line.split(" #").next().unwrap();
        for command in line.split(['|', '&', ';']).map(str::trim) {
            if command.starts_with("braid ") {
                commands.push(command.to_owned());
            } else if command.starts_with("--") {
                flags.push(command.split_whitespace().next().unwrap().to_owned());
            }
        }
    }
    assert!(commands.len() >= 12, "{commands:?}");
    let help = |path: &[&str]| -> String {
        let mut args = path.to_vec();
        args.push("--help");
        let out = Command::new(env!("CARGO_BIN_EXE_braid"))
            .args(&args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "braid {args:?}: {}",
            text(&out.stderr)
        );
        text(&out.stdout)
    };
    let mut helps = String::new();
    for command in &commands {
        let words: Vec<&str> = command.split_whitespace().skip(1).collect();
        if words == ["--version"] {
            continue;
        }
        let mut path = vec![words[0]];
        // Commands with subcommands of their own: the flags are those of
        // the subcommand.
        if ["hub", "change", "workspace", "git", "op", "agents"].contains(&words[0])
            && words.get(1).is_some_and(|word| !word.starts_with('-'))
        {
            path.push(words[1]);
        }
        let mut shown = help(&path);
        // A subcommand with commands of its own (`braid hub request
        // create`): the flags are those of its command.
        if path.len() == 2
            && let Some(&word) = words.get(2)
            && lists_command(&shown, word)
        {
            path.push(word);
            shown = help(&path);
        }
        for word in &words[path.len()..] {
            if word.starts_with('-') {
                let flag = word.split('=').next().unwrap();
                assert!(
                    shown.contains(flag),
                    "{command}: no {flag} in braid {path:?}"
                );
            }
        }
        helps.push_str(&shown);
    }
    // Flags the guide names on their own belong to the commands it gives.
    assert!(flags.len() >= 3, "{flags:?}");
    for flag in &flags {
        assert!(
            helps.contains(flag.as_str()),
            "{flag} is no flag of the guide's commands"
        );
    }
}

#[test]
fn a_linked_repository_goes_to_its_hub_unless_kept_local() {
    let p = Place::new();
    let url = p.link_to_nowhere();
    let ws = p.workspace_with_a_commit("w1");

    // Landing goes to the hub, which cannot be reached: nothing moves.
    let out = p.braid(&ws, &["land", "--json"], &[]);
    fails_with(&out, &format!("cannot reach the hub at {url}"));
    let refs = json(&p.braid(&p.root, &["refs", "--json"], &[]));
    assert_eq!(refs["refs"]["main"], Value::Null);

    // BRAID_HUB=off and --local land here, as before.
    let landed = json(&p.braid(&ws, &["land", "--json"], &[("BRAID_HUB", "off")]));
    assert_eq!(landed["outcome"], "fast-forward");
    for key in [
        "branch",
        "outcome",
        "head_before",
        "head",
        "landed",
        "merged_paths",
        "operation",
        "change",
        "conflicts",
        "next",
        "base",
        "updated_paths",
    ] {
        assert!(landed.get(key).is_some(), "{key}: {landed}");
    }
    p.commit(&ws, "b.txt", "two\n");
    let landed = json(&p.braid(&ws, &["land", "--local", "--json"], &[]));
    assert_eq!(landed["outcome"], "fast-forward");

    // Updating fetches first, and says how to do without.
    fails_with(&p.braid(&ws, &["update"], &[]), "--local");
    let updated = json(&p.braid(&ws, &["update", "--local", "--json"], &[]));
    assert_eq!(updated["outcome"], "up-to-date");
    let out = p.braid(&ws, &["update"], &[("BRAID_HUB", "off")]);
    assert!(out.status.success(), "{}", text(&out.stderr));

    // change create --land too.
    let file = p.base.join("c.txt");
    fs::write(&file, "c\n").unwrap();
    let set = format!("c.txt=@{}", file.display());
    let create = [
        "change", "create", "--base", "main", "--set", &set, "-m", "c", "--land", "main",
    ];
    fails_with(&p.braid(&p.root, &create, &[]), "cannot reach the hub");
    let mut local = create.to_vec();
    local.push("--local");
    let made = json(&p.braid(&p.root, &local, &[]));
    assert_eq!(made["land"]["outcome"], "fast-forward");

    // A value of BRAID_HUB that means nothing is refused.
    fails_with(
        &p.braid(&ws, &["land"], &[("BRAID_HUB", "maybe")]),
        "BRAID_HUB takes",
    );

    // The hub commands name the hub they cannot reach, and never the
    // token they would have sent.
    let token = "brd_aaaaaaaa_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    for args in [&["hub", "fetch"][..], &["hub", "status"]] {
        let out = p.braid(&p.root, args, &[("BRAID_TOKEN", token)]);
        fails_with(&out, &format!("cannot reach the hub at {url}"));
        assert!(!text(&out.stderr).contains("bbbbbbbbbbbbbbbb"));
    }
}

#[test]
fn a_damaged_link_is_an_error_that_names_it() {
    let p = Place::new();
    fs::write(p.root.join(".braid/hub"), b"{\"url\": \"https://x\"}").unwrap();
    let ws = p.workspace_with_a_commit("w1");
    fails_with(&p.braid(&ws, &["land"], &[]), "is not a valid hub link");
    // --local does not read it.
    let out = p.braid(&ws, &["land", "--local"], &[]);
    assert!(out.status.success(), "{}", text(&out.stderr));
}

/// `braid status --json` gives the hub's green and red of the branch as a
/// fetch last recorded them, and null where none was.
#[test]
fn status_gives_the_hubs_green_and_red_as_last_seen() {
    let p = Place::new();
    let ws = p.workspace_with_a_commit("w1");
    let landed = json(&p.braid(&ws, &["land", "--local", "--json"], &[]));
    let head = landed["head"].as_str().unwrap().to_owned();
    let status = json(&p.braid(&ws, &["status", "--json"], &[]));
    assert_eq!(
        (&status["hub_green"], &status["hub_red"]),
        (&Value::Null, &Value::Null)
    );
    for name in ["remote/hub/braid/green/main", "remote/hub/braid/red/main"] {
        let out = p.braid(&p.root, &["ref", "set", name, &head], &[]);
        assert!(out.status.success(), "{}", text(&out.stderr));
    }
    let status = json(&p.braid(&ws, &["status", "--json"], &[]));
    assert_eq!(status["hub_green"], head.as_str());
    assert_eq!(status["hub_red"], head.as_str());
    // The local green of `braid verify` is another ref.
    assert_eq!(status["green"], Value::Null);
}

/// A request a stand-in hub read: its request line, its `Idempotency-Key`
/// and its body.
#[derive(Debug, Default, PartialEq, Eq)]
struct Seen {
    line: String,
    key: Option<String>,
    body: String,
}

/// What `GET /v1` answers on a hub that verifies branches.
const VERIFYING: &str = r#"{"version":1,"limits":{"max_body":1,"max_put":1,"max_json":1,"max_ids":1,"max_paths":1,"missing_page":1,"missing_page_max":1,"missing_objects":1,"tree_entries":1,"tree_depth":1,"max_list":1},"features":["verify"]}"#;

/// A stand-in for a hub on this machine: it reads one request per
/// connection and gives each of `answers` in turn, a status and a JSON
/// body, or closes the connection without a word for an empty status, as
/// when an answer is lost. It hands back what it was sent.
fn stand_in(answers: Vec<(&'static str, &'static str)>) -> (String, JoinHandle<Vec<Seen>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let mut seen = Vec::new();
        for (status, body) in answers {
            let (mut stream, _) = listener.accept().unwrap();
            seen.push(read_request(&stream));
            if !status.is_empty() {
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        }
        seen
    });
    (url, server)
}

fn read_request(stream: &TcpStream) -> Seen {
    let mut reader = BufReader::new(stream);
    let mut seen = Seen::default();
    reader.read_line(&mut seen.line).unwrap();
    seen.line.truncate(seen.line.trim_end().len());
    let mut length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let Some((name, value)) = line.trim_end().split_once(':') else {
            break;
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "idempotency-key" => seen.key = Some(value.trim().to_owned()),
            "content-length" => length = value.trim().parse().unwrap(),
            _ => {}
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    seen.body = String::from_utf8(body).unwrap();
    seen
}

/// `braid hub verify` and its exit statuses, against a stand-in hub.
#[test]
fn verify_commands_and_their_exit_statuses() {
    let p = Place::new();
    let token = "brd_aaaaaaaa_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let verify = |url: &str, args: &[&str]| -> Output {
        let mut line = vec!["hub", "verify"];
        line.extend(args);
        p.braid(
            &p.base,
            &line,
            &[("BRAID_HUB_URL", url), ("BRAID_TOKEN", token)],
        )
    };
    let red = r#"{"mode":"report","branch":"main","state":"red","red":null,"later":[1]}"#;

    // Red: 4 with --fail-on-red, and the answer printed whole.
    let (url, server) = stand_in(vec![("200 OK", VERIFYING), ("200 OK", red)]);
    let out = verify(
        &url,
        &["status", "-R", "ada/demo", "--fail-on-red", "--json"],
    );
    assert_eq!(out.status.code(), Some(4), "{}", text(&out.stderr));
    let printed: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(printed, serde_json::from_str::<Value>(red).unwrap());
    let lines: Vec<String> = server
        .join()
        .unwrap()
        .into_iter()
        .map(|seen| seen.line)
        .collect();
    assert_eq!(
        lines,
        ["GET /v1 HTTP/1.1", "GET /v1/r/ada/demo/verify HTTP/1.1"]
    );
    // Without the flag, 0, and words.
    let (url, _server) = stand_in(vec![("200 OK", VERIFYING), ("200 OK", red)]);
    let out = verify(&url, &["status", "-R", "ada/demo"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(text(&out.stdout).starts_with("ada/demo main: red"));

    // A hub that does not verify is asked nothing more.
    let (url, server) = stand_in(vec![(
        "200 OK",
        r#"{"version":1,"limits":{"max_body":1,"max_put":1,"max_json":1,"max_ids":1,"max_paths":1,"missing_page":1,"missing_page_max":1,"missing_objects":1,"tree_entries":1,"tree_depth":1,"max_list":1}}"#,
    )]);
    fails_with(
        &verify(&url, &["status", "-R", "ada/demo"]),
        "does not verify branches",
    );
    assert_eq!(server.join().unwrap().len(), 1);

    // A refusal is 1, with the hub's reason.
    let (url, _server) = stand_in(vec![
        ("200 OK", VERIFYING),
        (
            "403 Forbidden",
            r#"{"error":"forbidden","detail":"use a token with read and verify:<glob> only"}"#,
        ),
    ]);
    let out = verify(
        &url,
        &[
            "report",
            "-R",
            "ada/demo",
            "--outcome",
            "pass",
            "--git-commit",
            &"0a".repeat(20),
            "--suite",
            "ci",
        ],
    );
    fails_with(&out, "use a token with read and verify:<glob> only");
    assert!(!text(&out.stderr).contains("bbbbbbbbbbbbbbbb"));

    // A hub out of reach is 3.
    let nowhere = {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    let out = verify(&nowhere, &["status", "-R", "ada/demo"]);
    assert_eq!(out.status.code(), Some(3), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("cannot reach the hub"));

    // A report whose answer is lost goes again with the same key, and the
    // hub's first answer is printed.
    let answer =
        r#"{"effect":"green_moved","status":{"mode":"report","branch":"main","state":"green"}}"#;
    let (url, server) = stand_in(vec![
        ("200 OK", VERIFYING),
        ("", ""),
        ("201 Created", answer),
    ]);
    let commit = "0a".repeat(20);
    let out = verify(
        &url,
        &[
            "report",
            "-R",
            "ada/demo",
            "--outcome",
            "pass",
            "--git-commit",
            &commit,
            "--suite",
            "ci",
            "--json",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap(),
        serde_json::from_str::<Value>(answer).unwrap()
    );
    let seen = server.join().unwrap();
    assert_eq!(seen[1], seen[2], "the same request, with the same key");
    assert_eq!(seen[1].line, "POST /v1/r/ada/demo/verify/report HTTP/1.1");
    assert_eq!(seen[1].key.as_ref().map(String::len), Some(32));
    assert_eq!(
        serde_json::from_str::<Value>(&seen[1].body).unwrap(),
        serde_json::json!({"git_commit": commit, "outcome": "pass", "suite": "ci"})
    );

    // What the hub would refuse is refused before anything is sent.
    fails_with(
        &verify(
            &nowhere,
            &[
                "report",
                "-R",
                "ada/demo",
                "--outcome",
                "pass",
                "--changeset",
                "abc",
                "--suite",
                "ci",
            ],
        ),
        "64 hex digits",
    );
}
