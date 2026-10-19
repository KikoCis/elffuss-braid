// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 UtopiaIA

//! Credentials for hubs.
//!
//! `braid login` saves a token in `hub.json`, in braid's configuration
//! directory: `$XDG_CONFIG_HOME/braid`, or `$HOME/.config/braid` when
//! `XDG_CONFIG_HOME` is unset or not absolute. The directory has mode 0700
//! and the file 0600, and the file holds:
//!
//! ```json
//! {"default": "<url>", "hubs": {"<url>": {"token": "brd_…", "login": "…", "expires": "…"}},
//!  "autopublish": true}
//! ```
//!
//! `default` is the hub signed in to last, and `autopublish` the answer of
//! `braid hub autopublish on|off`, for every hub (absent until answered).
//! The environment takes precedence over the file: `BRAID_HUB_URL` names
//! the hub, and `BRAID_TOKEN` is the token for it; when `BRAID_HUB_URL` is
//! unset, for the hub used by default (the one signed in to last, or the
//! public hub), and for no other: a `.braid/hub` that came with a checkout
//! or an archive never gets it.
//!
//! A hub's URL is `https://`, or `http://` to this machine only, so that a
//! token never crosses a network in clear. The token itself only ever goes
//! in an `Authorization` header: never in a URL, an argument, a log or an
//! error message. A file that other users can open is used, with a warning.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use braid_hubproto::{DEFAULT_HUB_URL, Token, limits};
use serde::{Deserialize, Serialize};

/// The file, in braid's configuration directory.
pub(crate) const FILE: &str = "hub.json";

/// The smallest request body `BRAID_HUB_MAX_BODY` may ask for.
pub(crate) const MIN_BODY: u64 = 1024;

/// What the environment says about hubs, read once, so that the rules below
/// can be tested without touching the process's environment.
#[derive(Clone, Default)]
pub(crate) struct HubEnv {
    /// `BRAID_TOKEN`.
    pub(crate) token: Option<String>,
    /// `BRAID_HUB_URL`.
    pub(crate) url: Option<String>,
    /// `BRAID_HUB`: `off` keeps landing and updating local.
    pub(crate) mode: Option<String>,
    /// `BRAID_HUB_MAX_BODY`: the largest upload request, in bytes.
    pub(crate) max_body: Option<String>,
    /// `XDG_CONFIG_HOME`.
    pub(crate) config_home: Option<PathBuf>,
    /// `HOME`.
    pub(crate) home: Option<PathBuf>,
}

impl HubEnv {
    /// The process's environment. An empty variable counts as unset.
    pub(crate) fn from_process() -> HubEnv {
        let text = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
        let path = |name: &str| {
            std::env::var_os(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        HubEnv {
            token: text("BRAID_TOKEN"),
            url: text("BRAID_HUB_URL"),
            mode: text("BRAID_HUB"),
            max_body: text("BRAID_HUB_MAX_BODY"),
            config_home: path("XDG_CONFIG_HOME"),
            home: path("HOME").or_else(|| path("USERPROFILE")),
        }
    }

    /// The largest body an upload request may have: the protocol's 256 MiB,
    /// or less when `BRAID_HUB_MAX_BODY` says so, for a hub behind a proxy
    /// that takes less.
    pub(crate) fn max_body(&self) -> Result<u64> {
        let Some(text) = &self.max_body else {
            return Ok(limits::MAX_BODY);
        };
        match text.trim().parse::<u64>() {
            Ok(bytes) if (MIN_BODY..=limits::MAX_BODY).contains(&bytes) => Ok(bytes),
            _ => bail!(
                "BRAID_HUB_MAX_BODY takes a number of bytes from {MIN_BODY} to {}, not {text:?}",
                limits::MAX_BODY
            ),
        }
    }

    /// Where the credentials file is.
    pub(crate) fn credentials_path(&self) -> Result<PathBuf> {
        let base = match (&self.config_home, &self.home) {
            (Some(dir), _) if dir.is_absolute() => dir.clone(),
            (_, Some(home)) => home.join(".config"),
            _ => bail!(
                "cannot find braid's configuration directory for hub credentials: set HOME or XDG_CONFIG_HOME"
            ),
        };
        Ok(base.join("braid").join(FILE))
    }

    /// The saved credentials, or none when there is no file or nowhere to
    /// look for one.
    pub(crate) fn credentials(&self) -> Result<Credentials> {
        match self.credentials_path() {
            Ok(path) => Credentials::load(&path),
            Err(_) => Ok(Credentials::default()),
        }
    }

    /// Whether `BRAID_HUB` turns the hub off: `off` keeps `land`, `update`
    /// and `change create --land` local in a linked repository; `on`, or
    /// nothing, lets the link decide.
    pub(crate) fn hub_off(&self) -> Result<bool> {
        match self.mode.as_deref().map(str::to_ascii_lowercase).as_deref() {
            None | Some("on") => Ok(false),
            Some("off") => Ok(true),
            Some(other) => bail!("BRAID_HUB takes `off` or `on`, not {other:?}"),
        }
    }
}

/// The credentials file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Credentials {
    /// The hub signed in to last.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) default: Option<String>,
    /// A token for each hub, by URL.
    #[serde(default)]
    pub(crate) hubs: BTreeMap<String, Saved>,
    /// Whether coding agents publish the new projects they start to the
    /// hub: `braid hub autopublish`. `None` until answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) autopublish: Option<bool>,
}

/// A token saved for one hub.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Saved {
    /// The token: `brd_<id>_<secret>`.
    pub(crate) token: String,
    /// The account's GitHub login.
    #[serde(default)]
    pub(crate) login: String,
    /// When the token expires, as the hub said it.
    #[serde(default)]
    pub(crate) expires: String,
}

impl fmt::Debug for Saved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The secret never reaches a debug print.
        let shown = Token::parse(&self.token)
            .map_or_else(|_| "(not a token)".to_owned(), |t| format!("{t:?}"));
        f.debug_struct("Saved")
            .field("token", &shown)
            .field("login", &self.login)
            .field("expires", &self.expires)
            .finish()
    }
}

impl Credentials {
    /// Reads the file at `path`; no file holds no credentials. A damaged
    /// file is an error that says where it is damaged, but never quotes
    /// what it found there, which may be a token.
    pub(crate) fn load(path: &Path) -> Result<Credentials> {
        match fs::read(path) {
            Ok(bytes) => {
                warn_if_open(path);
                Credentials::decode(path, &bytes)
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Credentials::default()),
            Err(err) => Err(err).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    /// The credentials in `bytes`, read from `path`.
    fn decode(path: &Path, bytes: &[u8]) -> Result<Credentials> {
        let creds: Credentials = serde_json::from_slice(bytes).map_err(|err| {
            let what = match err.classify() {
                serde_json::error::Category::Eof => "it is cut short",
                serde_json::error::Category::Syntax => "it is not JSON",
                serde_json::error::Category::Data => "a value has the wrong type",
                serde_json::error::Category::Io => "it cannot be read",
            };
            anyhow!(
                "{} is not a valid credentials file ({what}, at line {} column {}): fix it, or remove it and sign in again",
                path.display(),
                err.line(),
                err.column()
            )
        })?;
        if let Some(default) = &creds.default
            && normalize_url(default).is_err()
        {
            bail!(
                "{} is not a valid credentials file (its default hub is not a hub URL): fix it, or remove it and sign in again",
                path.display()
            );
        }
        Ok(creds)
    }

    /// Writes the file at `path`, mode 0600, through a scratch file and a
    /// rename, in a directory of mode 0700.
    pub(crate) fn save(&self, path: &Path) -> Result<()> {
        let dir = path
            .parent()
            .with_context(|| format!("{} has no directory", path.display()))?;
        private_dir(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let mut text = serde_json::to_vec_pretty(self)?;
        text.push(b'\n');
        let scratch = dir.join(format!(".{FILE}.{}.tmp", std::process::id()));
        let _ = fs::remove_file(&scratch);
        let written = private_file(&scratch).and_then(|mut file| {
            file.write_all(&text)?;
            file.sync_all()
        });
        if let Err(err) = written.and_then(|()| fs::rename(&scratch, path)) {
            let _ = fs::remove_file(&scratch);
            return Err(err).with_context(|| format!("cannot write {}", path.display()));
        }
        Ok(())
    }
}

/// Warns, on standard error, when the credentials file is open to other
/// users, or its directory can be written by them: the token in it is
/// theirs to read, or to replace. The command goes on; saving the file
/// closes both again.
#[cfg(unix)]
fn warn_if_open(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mode = |path: &Path| {
        fs::metadata(path)
            .ok()
            .map(|meta| meta.permissions().mode() & 0o777)
    };
    if let Some(mode) = mode(path)
        && mode & 0o077 != 0
    {
        eprintln!(
            "braid: warning: {} is open to other users (mode {mode:03o}), and it holds tokens: run `chmod 600 {}`",
            path.display(),
            path.display()
        );
    }
    if let Some(dir) = path.parent()
        && let Some(mode) = mode(dir)
        && mode & 0o022 != 0
    {
        eprintln!(
            "braid: warning: other users can write in {} (mode {mode:03o}), which holds the hub tokens: run `chmod 700 {}`",
            dir.display(),
            dir.display()
        );
    }
}

#[cfg(not(unix))]
fn warn_if_open(_path: &Path) {}

/// Creates `dir` with mode 0700, and gives it that mode if it exists: it
/// holds tokens.
#[cfg(unix)]
fn private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        }
        Err(err) => Err(err),
    }
}

#[cfg(not(unix))]
fn private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)
}

/// Creates a new file that only its owner can read and write.
#[cfg(unix)]
fn private_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn private_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

/// Where a command's token came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TokenSource {
    /// `BRAID_TOKEN`.
    Environment,
    /// The credentials file.
    File,
    /// Nowhere: the command goes without one.
    None,
}

impl TokenSource {
    /// How JSON output names it.
    pub(crate) fn code(self) -> Option<&'static str> {
        match self {
            TokenSource::Environment => Some("environment"),
            TokenSource::File => Some("file"),
            TokenSource::None => None,
        }
    }
}

/// The hub a command talks to when no linked repository names one: `given`
/// (`--hub`), then `BRAID_HUB_URL`, then the hub signed in to last, then the
/// public hub.
pub(crate) fn hub_url(env: &HubEnv, creds: &Credentials, given: Option<&str>) -> Result<String> {
    let url = given
        .map(str::to_owned)
        .or_else(|| env.url.clone())
        .or_else(|| creds.default.clone())
        .unwrap_or_else(|| DEFAULT_HUB_URL.to_owned());
    normalize_url(&url)
}

/// The token for the hub at `url` (normalized): `BRAID_TOKEN` for the hub
/// it is meant for, the one [`hub_url`] gives without `--hub`; else the one
/// saved for `url`; else none. A linked repository's `.braid/hub` may come
/// from anywhere, so BRAID_TOKEN never goes to a hub only it names.
pub(crate) fn token_for(
    env: &HubEnv,
    creds: &Credentials,
    url: &str,
) -> Result<(Option<Token>, TokenSource)> {
    if let Some(text) = &env.token
        && hub_url(env, creds, None)? == url
    {
        // The message never repeats the variable's value.
        let token = Token::parse(text.trim()).map_err(|_| {
            anyhow!(
                "BRAID_TOKEN is not a braid hub token: expected brd_ then an 8-character id, _ and a 52-character secret"
            )
        })?;
        return Ok((Some(token), TokenSource::Environment));
    }
    match creds.hubs.get(url) {
        Some(saved) => {
            let token = Token::parse(&saved.token).map_err(|_| {
                anyhow!(
                    "the token saved for {url} is not a braid hub token: sign in again with `braid login --hub {url}`"
                )
            })?;
            Ok((Some(token), TokenSource::File))
        }
        None => Ok((None, TokenSource::None)),
    }
}

/// The hub `BRAID_TOKEN` is meant for, when it is set and that is not
/// `url`: what a command that sends no token to `url` says.
pub(crate) fn withheld_for(env: &HubEnv, creds: &Credentials, url: &str) -> Option<String> {
    env.token.as_ref()?;
    hub_url(env, creds, None).ok().filter(|meant| meant != url)
}

/// A hub's URL in the one form braid keeps it in: the scheme and the host
/// lowercase, without a trailing `/`. Only `https://` is taken, and
/// `http://` to this machine, which tests use; a URL with a
/// user name or password, a query or a fragment is refused.
pub(crate) fn normalize_url(text: &str) -> Result<String> {
    let text = text.trim();
    let invalid = |why: &str| {
        if redact(text) == text {
            anyhow!("{text:?} is not a hub URL: {why}")
        } else {
            // Never repeated: a token put where the URL goes, as when the
            // two variables are swapped.
            anyhow!(
                "that is not a hub URL: it holds a braid hub token, which goes in BRAID_TOKEN and never in a URL"
            )
        }
    };
    let (scheme, rest) = text
        .split_once("://")
        .ok_or_else(|| invalid("expected https:// and a host"))?;
    if text.contains(['?', '#']) || text.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(invalid("it holds a query, a fragment or a space"));
    }
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    if authority.is_empty() {
        return Err(invalid("it has no host"));
    }
    if authority.contains('@') {
        return Err(invalid(
            "it holds a user name or password: braid sends the token in a header",
        ));
    }
    let scheme = scheme.to_ascii_lowercase();
    let authority = authority.to_ascii_lowercase();
    match scheme.as_str() {
        "https" => {}
        "http" if is_loopback(&authority) => {}
        "http" => {
            return Err(invalid(
                "http:// would send the token in clear: use https:// (http:// is taken only for an address on this machine)",
            ));
        }
        _ => return Err(invalid("expected https://")),
    }
    Ok(format!(
        "{scheme}://{authority}{}",
        path.trim_end_matches('/')
    ))
}

/// `text` with the secret of every braid token in it, whole or cut short,
/// left out: `brd_<id>_...`. For what a hub, or a proxy in front of it,
/// says, which may echo the token a request carried.
pub(crate) fn redact(text: &str) -> std::borrow::Cow<'_, str> {
    const PREFIX: &str = "brd_";
    if !text.contains(PREFIX) {
        return std::borrow::Cow::Borrowed(text);
    }
    let base32 = |byte: u8| byte.is_ascii_lowercase() || (b'2'..=b'7').contains(&byte);
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut at = 0;
    while let Some(found) = text[at..].find(PREFIX) {
        let id = at + found + PREFIX.len();
        let underscore = id + 8;
        if underscore < bytes.len()
            && bytes[id..underscore].iter().all(|&byte| base32(byte))
            && bytes[underscore] == b'_'
        {
            let secret = underscore + 1;
            let mut end = secret;
            while end < bytes.len() && base32(bytes[end]) {
                end += 1;
            }
            if end > secret {
                out.push_str(&text[copied..secret]);
                out.push_str("...");
                copied = end;
            }
            at = end;
        } else {
            at = id;
        }
    }
    out.push_str(&text[copied..]);
    std::borrow::Cow::Owned(out)
}

/// The `host[:port]` of a normalized URL.
pub(crate) fn authority(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    &rest[..rest.find('/').unwrap_or(rest.len())]
}

/// Whether `authority` (`host[:port]`) names this machine: `localhost`, or
/// a loopback address.
pub(crate) fn is_loopback(authority: &str) -> bool {
    let host = match authority.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => authority
            .rsplit_once(':')
            .map_or(authority, |(host, _)| host),
    };
    host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(n: u8) -> Token {
        Token::from_random([n; 5], [n; 32])
    }

    fn env_in(dir: &Path) -> HubEnv {
        HubEnv {
            home: Some(dir.to_path_buf()),
            ..HubEnv::default()
        }
    }

    #[test]
    fn urls() {
        let good = [
            (
                "https://braidhub.elffuss.com",
                "https://braidhub.elffuss.com",
            ),
            (
                "https://BraidHub.Example.ORG/",
                "https://braidhub.example.org",
            ),
            (
                "HTTPS://hub.example.org/base/",
                "https://hub.example.org/base",
            ),
            ("http://127.0.0.1:8080", "http://127.0.0.1:8080"),
            ("http://localhost:3000/", "http://localhost:3000"),
            ("http://[::1]:9000", "http://[::1]:9000"),
        ];
        for (given, normal) in good {
            assert_eq!(normalize_url(given).unwrap(), normal, "{given}");
        }
        for bad in [
            "braidhub.elffuss.com",
            "http://hub.example.org",
            "http://10.0.0.1:8080",
            "ftp://hub.example.org",
            "https://user:secret@hub.example.org",
            "https://hub.example.org/?token=x",
            "https://hub.example.org/#x",
            "https://",
            "https://hub example.org",
        ] {
            let err = normalize_url(bad).unwrap_err().to_string();
            assert!(err.contains("is not a hub URL"), "{bad}: {err}");
        }
        assert_eq!(authority("https://hub.example.org/base"), "hub.example.org");
        assert!(is_loopback("127.0.0.1:1") && is_loopback("[::1]:1"));
        assert!(!is_loopback("hub.example.org:443") && !is_loopback("192.168.1.2"));
    }

    #[test]
    fn where_the_file_is() {
        let base = std::env::temp_dir();
        let xdg = base.join("xdg");
        let home = base.join("home");
        let env = HubEnv {
            config_home: Some(xdg.clone()),
            home: Some(home.clone()),
            ..HubEnv::default()
        };
        assert_eq!(env.credentials_path().unwrap(), xdg.join("braid/hub.json"));
        // A relative XDG_CONFIG_HOME is ignored, as the specification says.
        let env = HubEnv {
            config_home: Some(PathBuf::from("relative")),
            home: Some(home.clone()),
            ..HubEnv::default()
        };
        assert_eq!(
            env.credentials_path().unwrap(),
            home.join(".config/braid/hub.json")
        );
        assert!(HubEnv::default().credentials_path().is_err());
        assert_eq!(
            HubEnv::default().credentials().unwrap(),
            Credentials::default()
        );
    }

    #[test]
    fn saved_privately_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_in(dir.path());
        let path = env.credentials_path().unwrap();
        assert_eq!(Credentials::load(&path).unwrap(), Credentials::default());
        let mut creds = Credentials {
            default: Some("https://hub.example.org".to_owned()),
            ..Credentials::default()
        };
        creds.hubs.insert(
            "https://hub.example.org".to_owned(),
            Saved {
                token: token(1).expose(),
                login: "ada".to_owned(),
                expires: "2027-01-01T00:00:00Z".to_owned(),
            },
        );
        creds.save(&path).unwrap();
        assert_eq!(Credentials::load(&path).unwrap(), creds);
        // Saving again replaces the file.
        creds.hubs.clear();
        creds.save(&path).unwrap();
        assert_eq!(Credentials::load(&path).unwrap(), creds);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(path.parent().unwrap()), 0o700);
            // A directory made by someone else is closed too.
            fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o755)).unwrap();
            creds.save(&path).unwrap();
            assert_eq!(mode(path.parent().unwrap()), 0o700);
        }
        // No scratch file is left behind.
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, [FILE]);
        // A damaged file is an error that names it.
        fs::write(&path, b"{not json").unwrap();
        let err = format!("{:#}", Credentials::load(&path).unwrap_err());
        assert!(err.contains("not a valid credentials file"), "{err}");
    }

    #[test]
    fn the_environment_comes_first() {
        let saved_url = "https://hub.example.org";
        let mut creds = Credentials {
            default: Some(saved_url.to_owned()),
            ..Credentials::default()
        };
        creds.hubs.insert(
            saved_url.to_owned(),
            Saved {
                token: token(1).expose(),
                login: "ada".to_owned(),
                expires: String::new(),
            },
        );
        // The hub: --hub, then BRAID_HUB_URL, then the default, then the
        // public hub.
        let none = HubEnv::default();
        assert_eq!(hub_url(&none, &creds, None).unwrap(), saved_url);
        assert_eq!(
            hub_url(&none, &Credentials::default(), None).unwrap(),
            DEFAULT_HUB_URL
        );
        let env = HubEnv {
            url: Some("https://other.example.org/".to_owned()),
            ..HubEnv::default()
        };
        assert_eq!(
            hub_url(&env, &creds, None).unwrap(),
            "https://other.example.org"
        );
        assert_eq!(
            hub_url(&env, &creds, Some("http://localhost:1")).unwrap(),
            "http://localhost:1"
        );

        // The token: the saved one, unless BRAID_TOKEN is set.
        assert_eq!(
            token_for(&none, &creds, saved_url).unwrap(),
            (Some(token(1)), TokenSource::File)
        );
        assert_eq!(
            token_for(&none, &creds, "https://unknown.example.org").unwrap(),
            (None, TokenSource::None)
        );
        let env = HubEnv {
            token: Some(token(2).expose()),
            ..HubEnv::default()
        };
        assert_eq!(
            token_for(&env, &creds, saved_url).unwrap(),
            (Some(token(2)), TokenSource::Environment)
        );
        // BRAID_TOKEN belongs to the hub BRAID_HUB_URL names, and to no
        // other.
        let env = HubEnv {
            token: Some(token(2).expose()),
            url: Some("https://other.example.org".to_owned()),
            ..HubEnv::default()
        };
        assert_eq!(
            token_for(&env, &creds, "https://other.example.org").unwrap(),
            (Some(token(2)), TokenSource::Environment)
        );
        assert_eq!(
            token_for(&env, &creds, saved_url).unwrap(),
            (Some(token(1)), TokenSource::File)
        );
        // A malformed BRAID_TOKEN is refused without being repeated.
        let secret = "brd_notavalidtokenatall_with-a-secret-inside";
        let env = HubEnv {
            token: Some(secret.to_owned()),
            ..HubEnv::default()
        };
        let err = token_for(&env, &creds, saved_url).unwrap_err().to_string();
        assert!(
            err.contains("BRAID_TOKEN") && !err.contains("secret-inside"),
            "{err}"
        );
    }

    #[test]
    fn the_autopublish_answer_lives_next_to_the_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let path = env_in(dir.path()).credentials_path().unwrap();
        let mut creds = Credentials::default();
        creds.hubs.insert(
            "https://hub.example.org".to_owned(),
            Saved {
                token: token(1).expose(),
                login: "ada".to_owned(),
                expires: String::new(),
            },
        );
        creds.autopublish = Some(false);
        creds.save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"autopublish\": false"), "{text}");
        assert_eq!(Credentials::load(&path).unwrap(), creds);
        // A file from before the question reads as unanswered, and an
        // unanswered question writes nothing.
        fs::write(&path, br#"{"hubs": {}}"#).unwrap();
        let creds = Credentials::load(&path).unwrap();
        assert_eq!(creds.autopublish, None);
        creds.save(&path).unwrap();
        assert!(!fs::read_to_string(&path).unwrap().contains("autopublish"));
    }

    #[test]
    fn the_largest_upload() {
        let env = |value: Option<&str>| HubEnv {
            max_body: value.map(str::to_owned),
            ..HubEnv::default()
        };
        assert_eq!(env(None).max_body().unwrap(), limits::MAX_BODY);
        assert_eq!(env(Some("65536")).max_body().unwrap(), 65536);
        assert_eq!(env(Some(" 1024 ")).max_body().unwrap(), MIN_BODY);
        for bad in ["0", "1023", "268435457", "64k", "-1"] {
            let err = env(Some(bad)).max_body().unwrap_err().to_string();
            assert!(err.contains("BRAID_HUB_MAX_BODY takes"), "{bad}: {err}");
        }
    }

    #[test]
    fn braid_hub_off() {
        let mode = |value: Option<&str>| HubEnv {
            mode: value.map(str::to_owned),
            ..HubEnv::default()
        };
        assert!(!mode(None).hub_off().unwrap());
        assert!(!mode(Some("on")).hub_off().unwrap());
        assert!(mode(Some("off")).hub_off().unwrap());
        assert!(mode(Some("OFF")).hub_off().unwrap());
        assert!(mode(Some("nope")).hub_off().is_err());
    }

    #[test]
    fn debug_never_shows_a_secret() {
        let secret = token(3).expose();
        let saved = Saved {
            token: secret.clone(),
            login: "ada".to_owned(),
            expires: String::new(),
        };
        let shown = format!("{saved:?}");
        assert!(!shown.contains(&secret[13..]), "{shown}");
        let mut creds = Credentials::default();
        creds
            .hubs
            .insert("https://hub.example.org".to_owned(), saved);
        assert!(!format!("{creds:?}").contains(&secret[13..]));
    }
}
