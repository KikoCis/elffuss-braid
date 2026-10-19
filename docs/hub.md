# BraidHub: the protocol

BraidHub is a braid server: repositories live on it, people and agents sign
in with GitHub, and agents on any machine land on the same `main`. Each
repository can keep a GitHub repository as its mirror. BraidHub runs at
<https://braidhub.elffuss.com>. This document is the contract between the
hub and the client (`braid login`, `braid hub …`, and `braid land` / `braid
update` in a repository linked to a hub). `crates/hubproto` holds the types
and the object stream codec that both sides use.

Status: protocol version 1. Everything here is new in braid 0.1, and the
hub serves all of it.

Sections 1 to 14 are the core: accounts, repositories, objects, landing,
git, mirrors, limits, the client and the web. Sections 15 to 24 are what
grows around the repositories: browsing, discovery, stars and following,
issues, change requests, forks, the inbox, blocks, syncing with GitHub,
and the terms.

## 1. Conventions

- `GET /v1` → `{"version": 1, "limits": {"max_body", "max_put",
  "max_json", "max_ids", "max_paths", "missing_page", "missing_page_max",
  "missing_objects", "tree_entries", "tree_depth", "max_list",
  "social"}}`: the protocol version, and the sizes of section 12 this hub
  enforces, in bytes or entries, so that a client sizes its requests to
  them. `social` holds those of browsing and of the social features:
  `{"title", "text", "page", "topics", "topic", "labels",
  "thread_labels", "assignees", "comments_per_thread", "review_comments",
  "mentions", "references", "request_changesets", "request_bytes",
  "dir_page", "text_view"}`. Anyone may ask.
- JSON bodies are UTF-8, `Content-Type: application/json`. Ids are 64
  lowercase hex digits (braid) or 40 (git SHA-1). Times are RFC 3339 in UTC.
- Every error is a JSON object `{"error": "<code>", "detail": "<text>"}`
  with a 4xx or 5xx status, plus extra fields where noted. Codes:
  `unauthorized` (401), `forbidden` (403), `not_found` (404),
  `method_not_allowed` (405: the route exists, the method does not),
  `exists` (409), `invalid` (400), `missing` (409, with `ids`: objects the
  server needs), `not_on_branch` (409, with `head`: the stack is not built
  on the ref, which points to `head`; update first), `moved` (409: a ref
  does not point where the request
  expects), `too_large` (413), `rate_limited` (429, with `retry_after`
  seconds), `quota` (403: an account holds as many repositories, or its
  repositories store as many bytes, as an account may; section 12),
  `internal` (500), `unavailable` (503: the hub cannot do this
  now, such as signing in when it has no GitHub app or GitHub does not
  answer). The detail of `internal` only says that the hub failed: why goes
  to the hub's log.
- What the hub's logs keep, and for how long, is in its terms
  (`/terms#logs`, section 24).
- A repository is `{owner}/{name}`. `owner` is a GitHub login, compared
  without regard to case and stored lowercase. `name` is 1 to 100 of
  `A-Z a-z 0-9 . _ -`, does not start with `.` and does not end with `.git`;
  names are compared without regard to case too (`Demo` and `demo` are one
  repository), and a repository keeps the case it was made with.
- A path parameter that breaks these rules, or an id that is not 64
  lowercase hex digits (40 for git), is 400 `invalid`.
- A private repository answers 404 `not_found` to whoever may not read it,
  as if it did not exist, whatever the route, the method, the path or the
  body; and it is in none of their lists, counts, search results,
  notifications or links (section 5). One the caller may read but not
  change answers 403 `forbidden`.
- Branch and tag names are braid ref names: not empty, at most 4,096 bytes,
  no NUL. A ref under `braid/` or `remote/` is internal: it is listed, never
  written by a client.
- **Lists that come a page at a time** take `limit`, 1 to 100 (the `page`
  of `limits.social`; 30 when it is left out), and `after`, the `next` of
  the page before, and answer `next`, `null` on the last page. A cursor is
  opaque: one that does not belong to the list (another route, query or
  order) is 400 `invalid`, as is a `limit` out of range. A page holds only
  what the caller may see, and neither it nor its `next` says anything of
  what was left out.
- **`Idempotency-Key`.** A request that creates something (each route that
  takes the header says so: opening an issue or a change request,
  commenting, reviewing, forking, and a sync of GitHub repositories) may
  send `Idempotency-Key: <key>`, 1 to 64 of `A-Z a-z 0-9 _ -`, a new key
  for each new request; any other value, or the header twice, is 400
  `invalid`. The hub keeps the first answer for a day, for the account
  that sent it: the same request sent again with the same key, by any
  token or the web session of that account, gets that answer again, with
  `Idempotency-Replayed: true`, and is not done twice. The same request
  means the same method, path and body. The same key with another request
  is 400 `invalid`; while the first request is still being answered, 409
  `exists`, and the request can be sent again a moment later for its
  answer. An answer that says the hub failed (5xx) or was busy (429), and
  one over 64 KiB, is not kept: sent again, the request runs again. A day
  after its request, a key is forgotten. Without the header, every request
  runs.
- **Texts.** The titles of issues and change requests are 1 to 256
  characters on one line, without control characters (400 `invalid`
  otherwise). Their texts, and those of comments and reviews, are Markdown
  of at most 64 KiB (413 `too_large` past it), and a comment says
  something (400 for an empty one). `title` and `text` of `limits.social`
  give both sizes. A body that writes a text may carry `agent`, a label of
  1 to 100 characters without control characters, for the agent that
  writes it: the hub records it beside the text and shows it as "via".
- READMEs, other Markdown files, and the bodies of issues, change
  requests, comments and reviews are Markdown. The hub keeps them as they
  were written, and renders them to HTML itself, on its pages and in the
  `html` fields of its answers, by the rules below.

### Markdown

The text is CommonMark, with GitHub's tables, strikethrough (`~~text~~`),
task lists (`- [ ]`, `- [x]`) and footnotes (`[^1]`). What it renders to is
safe on the hub's pages, whoever wrote it:

- Raw HTML shows as text, as it was written, and HTML comments are left
  out.
- A link keeps its destination when it is an `http://`, `https://` or
  `mailto:` URL, a fragment (`#install`), a path on the hub that starts
  with one `/`, or a path relative to the text. A relative path leads to
  its file's page in the same repository and revision (`tree` for a path
  that ends with `/`, `blob` otherwise), and `..` never leads out of the
  repository. The destination is read as a browser reads it: without
  control characters and white space, with `\` as `/`, and its scheme in
  any case. Any other link (`javascript:`, `data:`, `//host`, …) shows its
  text without the link. Every link in a text carries
  `rel="nofollow ugc noopener noreferrer"`.
- Images show in a repository's own Markdown (its READMEs and the Markdown
  files its pages show) when they are `.png`, `.jpg`, `.jpeg`, `.gif` or
  `.webp` files of the same repository named by a relative path: they load
  from the repository's raw route. Any other image, and every image in an
  issue, a comment or a review, is a link to where it is, so that no text
  makes a reader's browser fetch anything from elsewhere.
- Headings get ids: `user-content-` and a slug made as GitHub makes it
  (lowercase; letters, digits, `-` and `_` kept; spaces as `-`; `-1`, `-2`,
  … after a repeat), and a link to `#Install` leads to the heading
  `Install`. Footnotes are numbered in the order they are referred to, and
  listed at the end. The first word after a code fence names the code's
  language, which the page carries as `class="language-…"` when it is 1 to
  32 of `a-z 0-9 + # -`.
- Elements nested more than 32 deep keep their text and lose their tags. A
  README renders up to 512 KiB of it, and no text renders to more than
  2 MiB of HTML; past either, a note says that only the beginning is shown.
- A text that would cost the hub far more to render than its size, such as
  tables whose head is wide and whose rows leave out many thousands of
  cells, or thousands of emphasis marks in one paragraph that never pair
  up, shows as it is written, in a block of plain text with a note that
  says so. It links no references and names no issues to close.

References in the prose of a text, outside code, links, images and raw
HTML, link where the reader may follow them, and stay text otherwise, as a
reference to something that does not exist does:

- `#N`: thread `N` of the same repository, an issue or a change request;
- `owner/name#N`: thread `N` of another repository;
- `@login`: an account;
- 12 to 64 lowercase hex digits, at least one of them a letter: a changeset
  of the repository, by its id or the start of it.

A reference stands apart from what is around it: nothing that would make
it part of a word, a path, a URL, an entity or an e-mail address comes just
before it (`a#4`, `x/#4`, `&#12;` and `ada@example.com` hold none), and no
letter, digit or `_` comes just after it. `N` is 1 to 999,999,999, written
without leading zeros. The first 50 different references of a text link;
the rest stay text. What a reference does when a text is written is in
section 18.

Closing words name the issues that a landing on the default branch closes
(section 8): `close`, `closes`, `closed`, `fix`, `fixes`, `fixed`,
`resolve`, `resolves` or `resolved`, in any case, then spaces, then `#N`,
outside code. `Fixes #4` names issue 4, and `closes#6` names nothing; a
message names at most 50.

## 2. Authentication

**Tokens.** `brd_<id>_<secret>`: `id` is 8 characters and `secret` 52, both
lowercase RFC 4648 base32 without padding (`a-z 2-7`); the secret holds 32
random bytes. A client sends `Authorization: Bearer brd_…` on every request,
SSE included. Never in a URL, a query string, argv or a log: the server logs
the token's `id` only, and keeps a keyed hash of the whole token, compared in
constant time.

A token has an expiry (required, at most one year) and scopes. A scope is
`{"repo": "<owner>/<name>" | "<owner>/*" | "*", "rights": [<right>…]}`, where
a right is `read`, `interact`, `write`, `land:<ref glob>` or `admin`.
`write` includes `read` and `interact`, and `admin` includes all three; a
`land:<glob>` right counts only together with `write`. `interact` lets a
token take part where its account reads (section 5): star, watch, open
issues and change requests, comment, review, and mark its notifications;
on `*`, it also follows and blocks accounts and changes the account's
settings. It gives no `read` of its own, and forking also needs `write`
where the fork goes (section 20). A token never grants more than its
account has: the effective right on a repository is the token's scopes
intersected with the account's own permission (section 5).

A request without `Authorization` is anonymous, and reads public
repositories. A token that is malformed, unknown, revoked or expired, or
whose account is gone or suspended (section 22), is 401 `unauthorized`,
even on a route anonymous callers may use.

**Accounts.** An account is a GitHub account: its login, lowercase, names
it, and it keeps GitHub's numeric id. A sign-in whose id differs from the
one an account keeps is 403 `forbidden`: a login renamed away from, or
reused after its account was deleted, does not take over that login's
repositories. A GitHub account that renamed itself signs in as a new
account; the old one keeps its repositories. An account made for a login
before it ever signed in takes the id of its first sign-in. A suspended
account's sign-ins and sessions are refused.

When the hub cannot sign people in with GitHub (sign-up is not open, or
GitHub does not answer), its sign-in routes answer 503 `unavailable`.

**Device flow (CLI and agents).** `braid login` runs GitHub's device flow
through the hub, so no browser is needed on the agent's machine:

1. `POST /v1/login/device`, with an empty body or `{}` → `{"poll",
   "user_code", "verification_uri", "expires_in", "interval"}`. `poll` is an
   opaque id the hub keeps for the flow, 64 hex digits; GitHub's device code
   never leaves the hub.
2. The person opens `verification_uri` anywhere and types `user_code`.
3. `POST /v1/login/device/poll {"poll"}` every `interval` seconds →
   `{"status": "pending"}`, `{"status": "slow_down", "interval"}`,
   `{"status": "denied"}`, `{"status": "expired"}`, or
   `{"status": "ok", "token", "login", "expires"}`. The token has the scope
   `{"repo": "*", "rights": ["read", "write", "land:*"]}`, the note `braid
   login`, and lives 90 days. A poll id the hub does not know (it forgets
   flows when it restarts) or whose flow ended is `expired`; a second poll
   while one is asking GitHub is `slow_down`.

The GitHub token of a sign-in says who signed in, and is dropped. The
client stores the hub's token and the hub URL in `~/.config/braid/hub.json`,
mode 0600, or reads `BRAID_TOKEN` and `BRAID_HUB_URL` from the environment.

**Web.** Signing up and signing in are one: the first sign-in makes the
account. `GET /login` works as a link from anywhere: it sends a visitor who
is signed out straight to GitHub's authorization page, and GitHub's answer
brings them back signed in, to `/`, or to `/-/onboarding` for an account
its first sign-in made (section 23); one signed in already goes to `/`.
While sign-up is not open, `GET /login` answers with a page that says so
(503). `GET /login` redirects to GitHub with a random `state`, which a
cookie keeps for ten minutes (`HttpOnly`, `SameSite=Lax`,
`Path=/auth/github`, signed by the hub); GitHub calls back `GET /auth/github/callback`, which goes on
only with that cookie and that state, exchanges the code with the app's
client secret, and starts a session: a random secret in the cookie
`braidhub_session` (`HttpOnly`, `Secure`, `SameSite=Lax`, 30 days). The hub
keeps it under a keyed hash of the secret, never the secret. Every form
carries the session's CSRF token in the field `csrf`; an API call made with
a session sends it in `X-CSRF-Token`. `POST /logout` (with `csrf`) ends the
session. Expired sessions go with the daily purge.

**GitHub access.** Signing in asks GitHub for the account's identity only
(`read:user`). Importing a private GitHub repository, mirroring to one and
syncing the account's GitHub repositories (section 23) need repository
access, which the account grants in a browser or with a device flow:

- In a browser, `GET /auth/github/repo-access?scope&next`, for the account
  signed in (a browser signed out is sent to `/login`). `scope=private`
  (the default) asks GitHub for `repo`: the account's private and public
  repositories; `scope=public` asks for `public_repo`: its public ones
  only. `next` is where the browser goes once access is granted:
  `onboarding` (`/-/onboarding/github`), `settings` (`/settings`) or `home`
  (`/`, the default). Any other value of either is 400, before GitHub
  hears of anything. Both travel in the signed state of the grant, never
  in the callback's own query: parameters added to the callback steer
  nothing. The pages offer grants as links, since forms post to the hub
  only.
- For agents and the command line, as `braid login` signs in: `POST
  /v1/github/access/device {"scope": "private" | "public"}` → `{"poll",
  "user_code", "verification_uri", "expires_in", "interval"}`, as `POST
  /v1/login/device` answers; a scope that is neither is 400. The person
  opens `verification_uri` anywhere, types `user_code` and approves,
  signed in to GitHub as the account's GitHub account. Then `POST
  /v1/github/access/device/poll {"poll"}` every `interval` seconds →
  `{"status": "pending"}`, `{"status": "slow_down", "interval"}`,
  `{"status": "denied"}`, `{"status": "expired"}`, or `{"status": "ok",
  "login", "scope"}`, where `scope` is what GitHub granted (`repo` or
  `public_repo`). A poll id the hub does not know, of a flow that ended,
  of a sign-in, or of another account's grant is `expired`. An account has
  at most three grants under way: starting a fourth ends its oldest, whose
  poll then answers `expired`.

A grant made as another GitHub account than the one the account signed in
with, and one in which GitHub granted less than was asked, are refused with
403 `forbidden`, and nothing changes. A grant replaces the one before; the
account's mirrors that waited for one (`needs_grant`, section 11) go on.
The device routes, and the listing and batches of section 23, act for the
caller's own account: with a token that has `write` (or `admin`) on
`<login>/*` or `*`,
as the token of `braid login` has, or with a session that sends its CSRF
token in `X-CSRF-Token`. A token that only reads or interacts, or reaches
one repository only, is 403 `forbidden`; no token and no session is 401
`unauthorized`. A hub that cannot reach GitHub repositories answers them
503 `unavailable`.

The hub keeps the GitHub token of a grant sealed, and uses it only for the
account's own imports, mirrors, webhooks and permission checks, passed to
`git` through the environment, never in a URL or argv. If the hub's keys
change, the account grants access again.

## 3. Accounts and tokens

- `GET /v1/me` → `{"login", "name", "avatar_url", "github_repo_access":
  bool, "token": {"id", "scopes", "created", "expires", "last_used",
  "note"}, "settings": {"tdm_default", "stars_public"}, "github_scope"}`.
  Needs a token. `settings` are the account's (below); `github_scope` holds
  the scopes of the GitHub access the account granted (section 2), as
  GitHub wrote them, and is left out when it granted none.
- `PATCH /v1/me {"tdm_default", "stars_public"}` → what `GET /v1/me`
  gives, as it is now. `tdm_default` is `allowed` or `reserved`: what text
  and data mining the account's new repositories start with (section 24).
  `stars_public` says whether the repositories it starred show on its
  profile and in `GET /v1/users/{login}/starred` to others (section 17). A
  field left out stays as it is; any other value is 400 `invalid`. It needs
  a token with `interact`, `write` or `admin` on `*`, and counts as a
  change of settings toward the account's limits (section 12).
- `POST /v1/tokens {"note", "scopes", "expires_in_days"}` → 201
  `{"token", "id", "scopes", "expires"}`. The token is shown this once, and
  acts for the same account. At least one scope (at most 100), each valid;
  `expires_in_days` from 1 to 366; a note of at most 200 characters; an
  account holds at most 1,000 tokens. Anything else is 400 `invalid`.
- `GET /v1/tokens` → `{"tokens": [{"id", "note", "scopes", "created",
  "expires", "last_used"}]}`: the account's, oldest first. `last_used` is
  noted at most once an hour, and left out for a token never used.
- `DELETE /v1/tokens/{id}` → 204: the token stops at once. 404 for a token
  of another account, 400 for an id that is not 8 characters of `a-z 2-7`.
- Managing tokens (the last three) needs a token with `admin` on `*`, or a
  session with its CSRF token in `X-CSRF-Token` (403 without it), except
  that any token may revoke itself (`braid logout`).

## 4. Repositories

Every repository has an `id`: `r_` and 26 characters of `a-z` and `2-7`,
given when it is made and never changed. No other repository ever takes
it, not even one made later under the same name. What belongs to a
repository (its stars, watchers, issues, change requests and the
notifications about them) follows its id, so a repository deleted and made
again under its name starts with none of it.

- `POST /v1/repos {"name", "private": true, "description", "import":
  "<git URL>" | null, "mirror": "<git URL>" | null}` → 201 with the
  repository, and its URL in `Location`. The owner is the caller, whose
  token needs `write` on `<login>/<name>`; 409 `exists` when the name is
  taken, 403 `quota` when the caller owns as many repositories as an account
  may (section 12). A description has at most 1,000 characters, without
  control characters but line breaks. With `import`, the hub
  takes the remote's history before it answers (a `git sync --direction
  follow` of the remote's default branch, which becomes the repository's
  default branch), with the owner's GitHub access when it has one; an
  import that fails is 400, one that leaves the owner's repositories
  storing more than an account may is 403 `quota`, and neither leaves a
  repository. Until the import is done, the repository answers as a
  missing one to everyone but its owner. With `mirror`, every
  landing on the default branch is then published there (section 11).
  `import` and `mirror` are GitHub repositories, `https://github.com/
  <owner>/<name>`, with or without `.git` (400 otherwise); a mirror needs
  the owner's GitHub repository access (section 2; 400 without it), and a
  GitHub repository that another repository of the hub has as its mirror
  is 409 `exists` (section 11). The repository's `tdm` is its owner's
  `tdm_default` (section 3).
- `GET /v1/repos` → `{"repos": [...]}`: the repositories the caller owns,
  those it is a collaborator of, and those mirrored on GitHub that GitHub
  lets it write to or administer, in order of owner and name, at most
  `max_list` (section 12). Each shows the caller's permission on it within
  the token's scopes (section 5); a repository the token cannot read is not
  listed. A change of collaborators shows at once. Listing never asks
  GitHub: a repository that only GitHub shares with the caller is listed on
  the terms of section 5, once a request of the caller's needed GitHub's
  answer about it, as opening the repository does, and for as long as the
  hub keeps that answer, with the permission GitHub gave. Needs a token.
- `GET /v1/r/{owner}/{name}` → `{"id", "owner", "name", "private",
  "description", "default_branch", "created", "mirror": {"url",
  "last_sync", "outcome", "detail", "follow", "github_id", "synced",
  "hook"} | null, "permission": "read" | "write" | "admin", "topics",
  "fork_of": {"owner", "name"} | null, "tdm": "allowed" | "reserved",
  "pushed", "counts": {"stars", "forks", "watchers", "open_issues",
  "open_requests"}, "viewer": {"starred", "watching"}, "settings":
  {"issues", "requests_from", "allow_forks", "require_approvals",
  "mirror_follow"}}`:
  - `topics`: what it is about (below); `tdm`: whether its owner reserves
    text and data mining of it (section 24).
  - `fork_of`: the repository it was forked from (section 20), when the
    caller may read that one, `null` otherwise.
  - `pushed`: when one of its refs last moved: the last landing, to within
    a minute (several landings within a minute leave the time of the
    first), or the history an import or a fork brought; `null` while it
    holds nothing.
  - `counts`, to whoever reads it: `stars`, the accounts that starred it
    (section 17); `forks`, its public forks, as a private fork counts for
    nobody (section 20); `watchers`, the accounts that watch all of it, its
    owner among them unless it chose otherwise; `open_issues` and
    `open_requests` (sections 18 and 19).
  - `viewer`, to a signed-in caller: whether it starred the repository, and
    how it watches it (`all`, `participating` or `ignore`, section 17).
  - `settings`, to its admins: whether it takes issues (section 18), who
    may open change requests and how many approvals one needs before it
    lands (section 19), whether it may be forked (section 20), and how its
    mirror follows GitHub (section 11; `null` without a mirror).

  This route, `PATCH` and `POST …/mirror/sync` answer the repository whole.
  The other routes that answer a repository (`POST /v1/repos`, `GET
  /v1/repos`, `POST …/mirror/follow`) leave out `counts`, `viewer` and
  `settings`, and give `fork_of` and `pushed` as `null`.
- `PATCH /v1/r/{owner}/{name}` with any of these → the repository.
  `admin`.
  - `private`. A fork of a private repository, or of a fork of one, stays
    private: `false` is 400 `invalid` (section 20). Making a repository
    public makes everything it ever stored readable by anyone who knows its
    ids (section 6), not only what its branches hold now: its whole
    history, branches deleted since, uploads that never landed, and its
    issues and change requests with their comments. A fresh public
    repository is the way to publish only what one holds now. A repository
    that turns private leaves every list, search and count of those who
    may no longer read it at once.
  - `description`, as for `POST /v1/repos`.
  - `mirror`: a new mirror, as for `POST /v1/repos`; `""` removes it.
  - `default_branch`: a git branch name outside braid's own refs (`braid/`,
    `git/`, `remote/`, `tags/`). A new mirror, or a new default branch with
    a mirror, is published at once.
  - `topics`: the whole list, which replaces the old one, each topic once:
    at most 20 (`topics` of `limits.social`), each 1 to 50 (`topic`) of
    lowercase letters, digits and `-`, starting with a letter or a digit.
  - `tdm`: `allowed` or `reserved` (section 24).
  - `issues`: `true` or `false`, whether it takes new issues and comments
    on them; turned off, what it holds is read as before (section 18).
  - `requests_from`: `readers` (the default), `collaborators` (its owner
    and collaborators) or `off`: who opens change requests (section 19).
  - `allow_forks`: `true` or `false`; yes by default for a public
    repository, no for a private one (section 20).
  - `require_approvals`: 0 to 10, how many approvals of a change request's
    current revision it needs before it lands; it applies to change
    requests only, never to `POST …/land` (section 19).
  - `mirror_follow`: `off`, `ff-only` or `merge` (section 11). On a
    repository without a mirror it is 400 `invalid`; a new mirror URL
    starts again at `off`, unless the same `PATCH` sets it too.

  Any other value is 400 `invalid`, and nothing changes. A `PATCH` that
  names a field counts as a change of settings toward the account's limits
  (section 12), as each change of collaborators does.
- `GET /v1/r/{owner}/{name}/collaborators` → `{"collaborators": [{"login",
  "permission"}]}`, by login. `PUT
  /v1/r/{owner}/{name}/collaborators/{login} {"permission"}` (`read`,
  `write` or `admin`) → 204, and `DELETE …/collaborators/{login}` → 204,
  also for a login that is not a collaborator. The owner is not one (400).
  At most 1,000. `admin`. Whoever a change leaves unable to read the
  repository loses it at once: its lists and its inbox no longer show the
  repository (section 21).
- `DELETE /v1/r/{owner}/{name}` → 204. `admin`. The repository is moved
  aside, not erased, and kept 30 days, counting toward its owner's quota
  until then (section 12); the name is free at once. A task that runs when
  the hub starts and once a day removes what was deleted longer ago. What
  belonged to it (its stars, watchers, issues, change requests and their
  notifications) goes with it.

## 5. Permissions

An account's permission on a repository is the highest of:

- `admin` for its owner;
- what a collaborator entry gives it;
- `read` for anyone, signed in or not, on a public repository;
- for a repository whose mirror is on GitHub, what GitHub lets the same
  login push there: `admin` for admin, `write` for maintain and write.
  Triage and read give nothing: GitHub may say read of anyone for a public
  repository, and the hub's repository holds every branch, where the mirror
  holds only one. The hub asks with the GitHub access the owner granted
  (none granted, nothing given).

The hub asks GitHub only when the rest gives the account less than a
request needs: reading a public repository never asks it, and neither does
anything the owner or a collaborator entry already allows. It keeps each
answer ten minutes (a failure to ask, one minute). What pages and answers
show (the `permission` of a repository, links such as the repository a fork
came from, buttons, list rows) comes from what the hub knows already, and
asks GitHub nothing: a permission that comes from GitHub shows once a
request needed GitHub's answer, for as long as the hub keeps it, and lists
include such a repository on the same terms.

GitHub limits how many requests the owner's access makes in an hour. While
GitHub says fewer than 500 are left, the hub asks it nothing for other
accounts until the limit resets; and one account makes at most 100
questions in any hour on the access of one owner. A question not asked gives
nothing: a repository only GitHub would open answers 404, and a write only
GitHub would allow answers 403. The owner's own work is not held back.

Before any of that, some repositories give nothing at all:

- the repositories of a suspended account answer 404 to everyone, as if
  they did not exist (section 22);
- a repository taken down, or still being filled by an import, answers 404
  to everyone but its owner;
- a fork of a private repository stays private, and only those who may
  read the repository it was forked from read it, and that one's upstream
  in turn, up to eight forks up; to anyone else it answers 404, whatever its
  own collaborators say, its owner included. What counts is what the
  account may read: a token whose scopes reach the fork alone reads it when
  its account reads every repository up the line. When its upstream is
  deleted it answers 404 to everyone; a repository made later under the
  upstream's name is another, and changes nothing.

`read` covers reading: every `GET` and `HEAD` of a repository but those of
its collaborators and its blocks, `POST …/missing`, `POST …/lacks`, the ref
watch, and checking a change request; `interact` covers taking part
(below); `write` covers uploads and managing issues and change requests
(sections 18 and 19); landing on a ref needs `write` and a `land:<glob>`
right whose glob matches the ref (`*` matches anything, `release/*` one
level); `admin` covers the settings, the collaborators, the repository's
blocks and deleting. A token's rights are cut down to its account's
permission, never below what anyone may do: any token reads a public
repository, where the rules above let anyone read it. A web session has
all its account may do.

Interacting with a repository (starring, watching, opening issues and
change requests, commenting, reviewing, forking) needs an account that may
read it, that is not suspended and that is not blocked there (section 22),
through a web session or a token with a scope that reaches the repository
with `interact`, `write` or `admin`. A token that reads a public repository
only because anyone may does not interact with it, and an anonymous caller
never does (401). A repository the caller may not read answers 404 here
too, as a missing one does; one it reads but may not interact with, 403.
Following and blocking accounts need a web session, or a token with
`interact`, `write` or `admin` on `*`.

## 6. Objects

Objects are immutable and named by their braid id, so every read is
cacheable forever.

- `GET /v1/r/{owner}/{name}/o/{id}` → the payload, `Content-Type:
  application/octet-stream`, `Braid-Kind: blob | tree | changeset |
  chunklist | derived`, `ETag: "<id>"`, `Cache-Control: public, max-age=31536000,
  immutable` (`private` for a private repository). 404 when absent. A blob
  is always its whole content, however the store keeps it. `HEAD
  /v1/r/{owner}/{name}/o/{id}` gives the same headers and
  `Content-Length`. `If-None-Match` with the tag gives 304.
- `PUT /v1/r/{owner}/{name}/o/{id}` with `Braid-Kind` and the payload →
  204. The hub hashes the payload as that kind and refuses a mismatch, a
  missing or unknown kind (400 `invalid`). At most 64 MiB (413); larger
  objects go through the stream. Idempotent.
- `POST /v1/r/{owner}/{name}/lacks {"ids": [...]}` (at most 10,000, 413
  beyond) → `{"lacks": [...]}`: the ids of the list the hub does not store,
  in the order asked, each once.
- `POST /v1/r/{owner}/{name}/objects`, `Content-Type:
  application/x-braid-objects`, body an object stream (section 7) → 200
  `{"stored": n}`. Each record is hashed and checked like a `PUT`. Another
  media type, a stream that is truncated, holds a bad record or an unknown
  kind, or goes on after its terminator is 400 `invalid`; a body over
  256 MiB, or a record announcing more, is 413. The records before a bad
  one are kept: each was checked before it was stored.
- `POST /v1/r/{owner}/{name}/missing` → what a client needs to hold the
  wanted changesets:

  ```json
  {"want": ["<changeset id>"], "have": ["<changeset id>"], "history": null,
   "paths": null, "limit": 10000, "after": null}
  ```

  `want` names changesets the hub holds (404 for one it does not, 400 for
  an id of another kind). `have` lists changesets the client holds with
  everything they reach (the hub ignores ids it does not know). Each holds
  at most 10,000 ids (413). `history` is `null` for every ancestor, `0` for
  the wanted changesets alone, `n` for n generations along all parents.
  `paths`, when given, limits trees to those path prefixes (at most 1,000;
  leading and trailing `/` are ignored; the trees on the way are always
  included). The walk takes, for each changeset, the changeset, its
  external change list, its git origin, and every tree node and blob of its
  root tree, skipping anything reachable from a `have` changeset's root
  tree, and lists parents before children only in that a changeset comes
  before the objects it names. The response is `{"missing": [{"id", "kind",
  "size"}], "next": "<cursor>" | null}`, paged by `limit` (default 10,000,
  at most 100,000; 0 is 400) and `after`, the `next` of the page before.
  Each page walks again; a cursor that does not belong to the walk (another
  request, or a walk that changed meanwhile) is 400 `invalid`: start again
  without it. A walk lists at most 1,000,000 objects, counting the trees of
  `have`; a larger one is 413 `too_large`: ask for less history, for fewer
  paths, or give what you hold in `have`. With `Accept:
  application/x-braid-objects` the same request returns the objects
  themselves as an object stream, unpaged (`limit` and `after` do not
  apply); a failure half-way ends it without its terminator.

What a change request from another repository brings is not the target's
until it lands: these routes of the target never serve it, and the
request's own routes do (section 19).

## 7. The object stream

`application/x-braid-objects`: the 10 bytes `BRAIDOBJ1\n`, then records,
then a terminator.

- A record: 1 byte kind code (1 blob, 2 tree, 3 changeset, 4 chunklist,
  5 derived), the 32-byte id, the payload length as a little-endian u64,
  then the payload.
- The terminator: one byte 0. A stream without it is truncated and refused.

A reader checks every record's id against its kind and payload before using
it.

## 8. Refs and landing

- `GET /v1/r/{owner}/{name}/refs` → `{"seq": n, "refs": {"<name>":
  "<id>"}}`: the op log sequence number (0 before the first operation) and
  every ref, the hub's own included.
- `GET /v1/r/{owner}/{name}/refs/watch` → `text/event-stream`. Each change
  is an event `refs` with `id: <seq>` and the same body as `GET refs`:

  ```text
  event: refs
  id: 12
  data: {"seq":12,"refs":{"main":"<id>"}}
  ```

  The first event is the current state. `Last-Event-ID` resumes: when it is
  the current sequence number, the first event is left out, and otherwise
  it is the current state, which holds every ref, so it is all a client
  that missed events needs. A comment line (`: keep-alive`) every 15
  seconds keeps the stream open; changes made outside the hub's own
  landings show up by then. The stream ends when the hub stops.
- `DELETE /v1/r/{owner}/{name}/refs/{ref}?expect=<id>` → 204: a compare
  and swap, which deletes the ref only if it points to `expect`. It needs
  `write` and a `land:<glob>` right matching the ref (403). `{ref}` may
  hold `/`, each part percent-encoded. A missing or malformed `expect`, one
  of the hub's own refs (`braid/`, `remote/`), or the default branch while
  a mirror publishes it is 400; a ref that does not exist is 404 and one
  that points elsewhere 409 `moved`. It takes its turn in the ref's queue,
  and watchers get a `refs` event.
- `POST /v1/r/{owner}/{name}/land`:

  ```json
  {"ref": "main", "changes": ["<id>", "..."], "mode": "rebase", "agent": "agent-1"}
  ```

  Landing needs `write` and a `land:<glob>` right matching the ref (403).
  The ref is a braid ref name outside `braid/` and `remote/` (400 otherwise).
  `changes` is a stack of 1 to 10,000 changesets (413 beyond), oldest
  first: each one's first parent is the one before it, and the first one's
  is on the ref's first-parent chain (or the ref does not exist yet).
  `agent`, 1 to 100 characters without control characters, names who lands
  in the op log, as `<login>/<agent>` (`<login>` without it).

  The hub admits every changeset first, and every ancestor of them it has
  not admitted yet, oldest first: its change list is canonical, its root
  tree is its first parent's with the changes applied, and every object it
  names is stored. One 409 `missing` lists everything the hub lacks for the
  stack: the changesets it does not have, or when it has them all, their
  change lists, git origins, blobs and the tree nodes their changes
  rebuild. A changeset that can never be admitted (a root its changes do
  not give, a change list that is not canonical, an id of another kind) is
  400 `invalid`, saying why. Then one task per ref lands the stack in turn
  with the others:
  - `mode: "rebase"` (the default) fast-forwards when the first change's
    base is the head, and otherwise rebases the stack onto the head, path by
    path, as `braid land` does; when the first change's base is not on the
    ref's first-parent chain, it answers 409 `not_on_branch`;
  - `mode: "ff-only"` fast-forwards only: when neither the first change's
    first parent nor any change of the stack is the head, it answers
    `refused` with the head.

  On a ref that does not exist, either mode creates it at the last change.
  A list that is empty or is not a stack is 400 `invalid`. Landing the same
  stack again returns `already-applied`. The response (200 in every other
  case):

  ```json
  {"ref": "main", "outcome": "fast-forward | rebased | already-applied | conflicts | refused",
   "head_before": "<id>|null", "head": "<id>|null",
   "landed": [{"from": "<id>", "to": "<id>|null"}],
   "merged_paths": [], "conflicts": [], "change": "<id>|null",
   "git_commit": "<sha1>|null", "operation": 12}
  ```

  `conflicts` uses the same objects as `braid land --json` (`path`, `kind`,
  `base`, `branch`, `workspace`, `hunks`), from the stack's point of view;
  every field but `git_commit` is what `braid land --json` gives for the
  same situation. `head` is the head after the landing (the head before
  when it did not move, as with `refused`), and `operation` is `null` when
  the ref did not move. `git_commit` is the git commit the head exports to,
  computed as `braid git export` does, so a git mirror holds exactly that
  commit. History imported from git and uploaded exports to its original
  commits: the hub checks each against the canonical tree of its root and
  the commits of its parents. It is `null` when the hub cannot export the
  head, such as history whose git trees are not canonical (only the map of
  the repository that imported them reproduces those), and never a commit
  the head would not export to.

  Changesets the hub rebases keep their author, message and extras, with
  `rebased-from` added, and name the hub as their committer: `BraidHub
  <noreply@<host of the hub's URL>>`. The hub admits them too. A landing is
  durable before its answer, and a landing that moved the ref sends a
  `refs` event to every watcher.

  A landing that moves the repository's default branch closes the open
  issues that the messages of its changesets close with closing words
  (section 1: `Fixes #4`, outside code). Each closes as `completed`, by the
  account that landed, naming the changeset (`closed_by_changeset`,
  section 18). A landing on any other ref closes nothing.

## 9. Git

- `GET /v1/r/{owner}/{name}/git/commit/{sha1}` → `{"changeset", "tree",
  "git_tree"}` for a commit the repository knows (imported, or the export
  of one of its changesets); 404 otherwise. The exports a repository knows
  are those of every head landed on it and their history, computed again
  when asked after the hub restarts.
- `GET /v1/r/{owner}/{name}/git/changeset/{id}` → `{"changeset",
  "git_commit"}`: the commit a changeset exports to, as a landing's
  `git_commit` says, without landing again; 404, saying why, when the hub
  does not have the changeset or cannot export it.
- `GET /v1/r/{owner}/{name}/git/o/{sha1}` → the git object's content,
  without git's `<type> <size>\0` header, with `Git-Kind: commit | tree |
  blob | tag`. A client that cares recomputes the SHA-1 itself: the hub's
  answer is never trusted for it.
- All three are immutable: their `Cache-Control` is that of the objects
  (section 6), `public` or `private` with the repository; `git/o` answers
  `application/octet-stream`.

## 10. Trees

`GET /v1/r/{owner}/{name}/t/{tree}?prefix=src/net&depth=2` → `{"tree",
"prefix", "entries": [{"path", "kind", "id", "size", "truncated"}]}`, where
`kind` is `file`, `executable`, `symlink`, `tree` or `gitlink`, `size` is
given for files, executables and symlinks, and `truncated: true` marks a
directory below `depth`, whose entries are not listed. `prefix` names a
directory of the tree (none for the tree itself; leading and trailing `/`
are ignored); 404 when it is not one. `depth`, 1 (the default) to 64, is
how many levels below the prefix are listed. Entries come in name order,
each directory followed by what it holds; `path` starts at the tree listed,
so it begins with the prefix. A gitlink's `id` is its commit in hex. At most
100,000 entries (413 beyond). A tree the hub does not have is 404.
Immutable, like the objects. Section 15 reads a revision's files by path.

## 11. Mirrors

A repository with a mirror publishes its default branch there after each
landing on it, with `git sync --direction publish` and the owner's GitHub
access: the push is a fast-forward with a lease, and the remote's history is
never rewritten. Publishing runs after the landing's answer, one at a time
per repository: landings during a publish fold into one more. A publish
that fails (git cannot reach the mirror, or the mirror moved during the
push) is tried again a second later, then twice as long after each failure,
at least every five minutes, until it goes through, and when the hub
starts. So is one still waiting when the hub stopped: when it starts, the
hub also publishes each mirror whose default branch moved since its last
sync went through.

**Following GitHub.** A mirror can follow GitHub as well as receive the
hub's landings. `PATCH /v1/r/{owner}/{name}` (`admin`) takes
`mirror_follow`:

- `off` (the default for a mirror set by hand): the hub publishes each
  landing on the default branch, and takes nothing in by itself. When
  someone pushed to the mirror directly, the sync stops with `diverged`;
  the hub records it, shows it on the repository's page, and keeps
  landing. The owner settles it by moving the remote back, or by taking
  the remote's commits in: `POST /v1/r/{owner}/{name}/mirror/follow`
  (`admin`) syncs the default branch with the mirror both ways, so the
  branch fast-forwards to the mirror's commits when it has nothing new,
  and otherwise merges them (the hub is the merge's committer) and
  publishes the merge; conflicts move nothing. It answers the repository,
  with the outcome in its `mirror`, after the sync, in the branch's queue.
- `ff-only`: the hub also takes in what is pushed to the mirror, by
  fast-forwarding its default branch, when it has landed nothing GitHub
  lacks. When both moved, nothing moves: the outcome is `diverged`, and
  the repository's admins are told in their inbox (section 21).
- `merge`: the hub takes in what is pushed to the mirror, and when both
  moved it merges them (the hub is the merge's committer) and publishes
  the merge. Conflicts move nothing: the outcome is `conflicts`, the hub
  publishes nothing more until a sync goes through, and the admins are
  told.

Repositories brought in from GitHub by syncing them (section 23) follow
with `merge` unless the sync says otherwise. `mirror_follow` on a
repository without a mirror is 400 `invalid`; a new mirror URL starts
again at `off`, unless the same `PATCH` sets `mirror_follow` too. `POST
…/mirror/sync` (section 23) looks at the mirror at once.

**What both sides hold.** The mirror keeps `synced`: the git commit both
sides held after its last publish, follow or import. A publish runs only
when the default branch moved since, and a follow only when GitHub's head
is another commit: the push the hub makes itself, coming back from GitHub,
changes nothing, so the two sides never chase each other.

**A rewritten history is never merged back.** When GitHub's branch no
longer descends from `synced`, someone rewrote its history (a force
push). The hub records `rewritten`, stops following and publishing, tells
the admins, and merges nothing, since a merge would bring back what the
rewrite took out; `POST …/mirror/follow` refuses it as well, with the same
outcome. The way out is on GitHub: put the commit `synced` names back in
the branch's history (push it again, or merge it in), then sync now
(section 23); or point the mirror at another repository, or take it off.

**One repository here per repository there.** A GitHub repository, named
in any case, with or without `.git` or a trailing `/`, is the mirror of at
most one repository of the hub: setting it as the mirror of another, with
`POST /v1/repos` or `PATCH`, is 409 `exists`, until the repository that
holds it lets it go or is deleted. The GitHub repository's numeric id
counts too, so that a renamed repository stays one: a repository that
names a renamed GitHub repository's new address before the hub has found
the rename is marked `duplicate` once it does, and the repository that
held it keeps it.

**How the hub hears of pushes.** A mirror that follows gets a webhook on
the GitHub repository when the owner administers it there; otherwise the
hub asks GitHub now and then: every ten minutes, hourly once GitHub's
branch has not moved for a week, every six hours while a webhook works,
and less often after failures. A webhook only says when to look: what is
taken in is fetched from GitHub, as always. When the mirror stops
following, moves to another repository, or its repository is deleted, the
hub deletes its webhook.

**The owner's access.** The hub reaches GitHub with the access the owner
granted (section 2). When GitHub refuses it (revoked, expired, or not
reaching that far), the outcome is `needs_grant`, the account's mirrors
neither publish nor follow, not even by `POST …/mirror/follow`, and the
account is told; granting access again, at `/auth/github/repo-access`,
lets them go on, and what waited is published.

**Renames.** A mirror whose address finds nothing is looked up on GitHub
by its id: renamed or transferred, the mirror follows the new name, and
`url` says it; deleted, or no longer shown to the owner's access, the
outcome is `gone` and the hub stops following it.

`GET /v1/r/{owner}/{name}` reports the mirror in `mirror`:

- `url`; `last_sync`, when the last sync ran; `outcome`: `pushed`,
  `created` (the branch was made on the mirror), `up_to_date`, `merged`,
  `fast_forwarded`, `diverged`, `remote_moved`, `rejected`, `conflicts`,
  `failed` (git could not reach the mirror, or refused), `rewritten`,
  `needs_grant`, `duplicate` (another repository of the hub mirrors that
  GitHub repository: this one neither publishes nor follows, and `POST
  …/mirror/follow` takes nothing in) or `gone`; and `detail`, what went
  wrong.
- `follow`: `ff-only` or `merge`; absent is `off`.
- `synced`: the commit both sides held after the last publish, follow or
  import, as 40 hex digits.
- `github_id`: the GitHub repository's numeric id, once the hub knows it.
- `hook`: how the hub hears of pushes: `webhook`, `polling`, or `pending`
  while it asks GitHub for a webhook; absent when the mirror does not
  follow.

`settings.mirror_follow` (admins) says the policy too. What the hub
imports or takes in from a mirror counts as admitted.

Git runs with none of the machine's or the user's settings, no prompt or
credential helper, no redirect, HTTPS only, no hooks, and in bare
repositories only: nothing of a repository runs on the hub. The owner's
GitHub token reaches git as an `Authorization` header for
`https://github.com/` alone, through the environment.

## 12. Limits

Per token, and per address for requests without one: requests per minute,
bytes served per day, and open watches. A limit reached answers 429
`rate_limited`, with `retry_after` seconds in the body and in `Retry-After`.
The limits are 6,000 requests a minute for a token and 600 for an address,
refilled evenly (one back every sixtieth of the limit); 100 GiB a day for a
token and 10 GiB for an address, counted as answers are sent, over a day
that starts with the first byte; 64 open watches for a token and 16 for an
address. A request counts for its token when the hub has that token (a
made-up one counts for its address). The address is the one the request
came from. `GET /healthz` is never limited. Counts start again when the hub
restarts.

Some requests count for more than one against the requests a minute, for
the work they cost the hub: explore, search and topics, their pages and
their routes (`GET /v1/explore`, `/v1/topics`, `/v1/topics/{topic}`), 10;
diffs and comparisons (`…/changesets/{rev}/diff`, `…/compare`, the diff of
a change request, the pages that compare a head for a new request, and
their pages), 10; the log (`…/log`) and the pages of commits, 5; checking
and landing a change request (`…/requests/{n}/check`,
`…/requests/{n}/land`), 20. A request that costs more than a whole minute's
limit waits for the whole minute's budget, and then goes through. Webhook
deliveries (`POST /v1/hooks/github/{key}`) are not limited per address,
since GitHub sends every delivery from a few addresses: their route limits
them per hook (section 23). A request that shows a token counts for its
token there too.

**Account quotas.** Anyone with a GitHub account may sign up, so each account
owns at most 100 repositories (`POST /v1/repos` is 403 `quota` beyond), and
its repositories store at most 10 GiB together, counted for the owner
whoever uploads: an upload (`PUT …/o/{id}`, `POST …/objects`) or an import
that would pass it is 403 `quota`. What an account stores is the size of
the files of its repositories, their git mirrors included, and of the
repositories it deleted, until the purge erases them (section 4); it is
measured at most every half minute per account, with what was uploaded
since added on top, a body without `Content-Length` by what it carried. An
upload counts from its start, with its body's `Content-Length` (one without
counts as its route's size), so that uploads at once do not pass the cap
together; one under way may pass it by its own size. An import that fails
or is refused leaves nothing behind. A fork's job reserves, from the
account's storage, what its upstream stores, from when it is asked for
until it ends, and the jobs and imports under way count toward the
repositories it owns (sections 20 and 23). What a change request from
another repository brings counts for the request's author until it lands
(section 19).

**Social writes.** Each account may make so many of each kind of social
write. Every token and the web session of an account spend one budget, so
that more tokens buy no more writes. The hourly limits refill evenly, one
write back every hour-th of the limit, so a burst need not wait for the
hour to turn; the daily ones count per UTC day. An account made on the hub
less than seven days ago, or whose GitHub account is less than thirty days
old, gets a quarter of each limit, and at least one write. A limit reached
answers 429 `rate_limited`, with `retry_after` the seconds until the write
would go through.

| Writes | Limit |
|---|---|
| Starring and unstarring | 120 an hour, 1,000 a day |
| Following and unfollowing | 60 an hour, 500 a day |
| Changing what is watched | 120 an hour |
| New issues | 20 an hour, 100 a day |
| Comments, reviews and edits of their text, together | 60 an hour, 500 a day |
| New change requests | 20 an hour, 100 a day |
| Updates of change requests | 120 an hour |
| Forks | 10 an hour, 30 a day |
| Labels, assignees, states and settings | 300 an hour |
| Blocking and unblocking | 60 an hour |
| Marking notifications | 600 an hour |
| Batches of repositories synced from GitHub | 6 an hour |

How each write counts: a star taken back as a star, an account unfollowed
as a follow; a level of watching changed, and a thread followed or muted,
as a watch (sections 17 and 21); a comment, a review, and a change of a
title or a text as a comment; a change of state, labels, assignees or lock,
a comment hidden or deleted, a label made or deleted, a draft marked ready,
reviewers asked, a change request deleted, a retry of a sync, and a change
of an account's or a repository's settings or collaborators as an edit
(sections 3, 4, 18, 19 and 23); a change request's update as a request
update; marking notifications read or done as a mark.

Besides, a repository takes at most 100 new threads (issues and change
requests together) an hour from the accounts that cannot write to it,
between them, so that many accounts cannot bury its issues. Accounts with
`write` on it are not counted, whatever the scopes of the token they use.
The 429 says how long until the repository takes another. Such an account
has at most 100 open threads in one repository (403 `forbidden` past it,
until it closes some).

**Sizes.** A body whose `Content-Length` is over its route's size is
refused at once, before the hub reads any of it. The sizes, each answered
413 `too_large`, and given by `GET /v1` (section 1): an object through
`PUT`, 64 MiB; a request body, 256 MiB (an object stream, and each of its
records); a JSON body, 4 MiB; a list of ids (`lacks`, `want`, `have`, a
stack), 10,000; the path prefixes of `/missing`, 1,000; a `/missing` page,
100,000 entries, and its walk, 1,000,000 objects; a tree listing, 100,000
entries; the repositories `GET /v1/repos` lists, an account's tokens and a
repository's collaborators, 1,000. `braid_hubproto::limits` holds them.

The sizes of browsing and of the social features, which `limits.social` of
`GET /v1` gives: a page of a list, 100 entries (`page`); a page of a
directory, 1,000 entries (`dir_page`); a file shown as text, 1 MiB
(`text_view`); a title, 256 characters (`title`); a text, 64 KiB (`text`);
a repository's topics, 20 (`topics`), each of at most 50 characters
(`topic`); a repository's labels, 100 (`labels`), and a thread's, 20
(`thread_labels`); a thread's assignees, 10 (`assignees`); a thread's
comments, 2,500, at which it locks itself (`comments_per_thread`); the line
comments of one review, 100 (`review_comments`); the mentions of one text
that notify, 10 (`mentions`), and the references of one text that link, 50
(`references`); the changesets of a change request, 1,000
(`request_changesets`), and what one brings from another repository,
256 MiB (`request_bytes`).

The sizes of diffs: a page of a diff, a changeset's `files` and a
comparison's `files` hold at most 300 files; a version larger than 256 KiB
is not diffed; the hunks of one file stop after 5,000 lines; a whole diff
as text holds at most 8 MiB; and the lines of one page as much, each line
counting 64 bytes more than it holds, so that a page of short lines holds
fewer of them.

## 13. The client

- `braid login [--hub URL]`, `braid logout [--hub URL]`, which revokes the
  token on the hub, and `braid hub whoami`, which says who is signed in and
  fails when nobody is.
- `braid hub autopublish [on|off]` records whether coding agents publish
  the new projects they start (`on`), or only when asked (`off`), for every
  project and every hub; without an argument it prints `on`, `off` or
  `unset`. braid never publishes by itself: an agent reads the answer
  (`/agents.md` says when).
- `braid hub publish [NAME] [--public] [--description TEXT] [--mirror URL]`
  creates the repository on the hub, uploads every branch and tag of the
  local repository, and links it: `.braid/hub` names the hub and the
  repository. With `--json` it prints, with the rest, `url`, the
  repository's address on the hub (`<hub>/<owner>/<name>`).
- `braid hub clone OWNER/NAME [DIR] [--paths PREFIX…]` makes a local
  repository from a hub repository, linked to it.
- `braid hub fetch` brings every branch and tag of the hub in, moving each
  local branch forward; a local branch with changesets the hub does not have
  is left as it is and reported.
- `braid hub status` shows the link, and how each branch compares.
- In a linked repository, `braid land` lands on the hub: it fetches, uploads
  what the hub lacks of the workspace's changesets, asks the hub to land the
  stack, fetches again, and moves the workspace onto the new head. Its
  output and exit statuses are those of a local landing. `braid update`
  fetches first. `braid change create --land` lands on the hub the same
  way.

Those of the commands below that act on one repository of the hub take it
as `-R OWNER/NAME` (`--hub-repo`), else the one the repository braid runs
in is linked to (`braid hub request` takes, in a fork's clone, the
repository it was forked from). Each takes `--json`, which prints what the
hub answered, as its section gives it, unless it says otherwise below.
Their exit status is 0 when the hub did it, 1 when it refused (a
repository, revision, path or thread that is not there or that the caller
may not read, a write not allowed, a limit), with its reason on standard
error, 2 for conflicts where a command says so, and 3 when the hub could
not be reached or failed. Without `--json`, what anyone wrote and the hub
gives (names, descriptions, topics, titles, texts, comments, logins,
labels, a README) is printed with its control characters escaped
(`\u{1b}`, `\n`), line breaks and tabs of texts kept, so that it can
neither drive the terminal nor print a line that looks like braid's;
`--json` and `braid hub cat` give the exact text. The texts they send carry the global `--agent` (or
`BRAID_AGENT`) as `agent`, unless it is `cli`. Commands that open, comment
or review send an `Idempotency-Key` (section 1), and send the request once
more with the same key when the answer was lost, so that nothing is done
twice; when it is lost twice, the error says that the hub may have done it
all the same, and which command shows whether it did.

**Reading without cloning** (section 15).

- `braid hub show [-R OWNER/NAME] [--ref REV]` shows a repository on the
  hub: its visibility, description, default branch, counts, topics, mirror
  and README. With `--json`, `{"repo", "readme"}`: the repository as `GET
  /v1/r/{owner}/{name}` gives it, and its README as `GET …/readme` does, or
  `null`.
- `braid hub ls [PATH] [-R OWNER/NAME] [--ref REV]` lists a directory,
  every page of it, an entry a line: its kind, its size and its name, a
  directory's ending in `/`; for a file, its own line. With `--json`, the
  answer of `GET …/contents` with the entries of every page.
- `braid hub cat PATH [-R OWNER/NAME] [--ref REV]` writes a file's bytes as
  they are, whatever its size. With `--json`, the file's answer of `GET
  …/contents`, with its text.
- `braid hub log [-R OWNER/NAME] [--ref REV] [--limit N]` lists the
  changesets down the first parents of a branch, newest first (30 unless
  `--limit` says otherwise), a line each, as `braid log` writes them: the
  short id, the author's date, the author with the agent, and the summary.
  With `--json`, the log as `GET …/log` gives it, its pages joined.
- `braid hub diff REV [-R OWNER/NAME] [--path PATH]` writes what a
  changeset changed against its first parent, as `git diff` writes it
  (`Accept: text/x-diff`), or, with `--path`, the diff of that file alone,
  for a diff larger than the hub gives whole. With `--json`, every page of
  `GET …/changesets/{rev}/diff` as one.

**Finding repositories and accounts** (section 16). Neither needs a
sign-in.

- `braid hub explore [QUERY…] [--topic TOPIC] [--owner LOGIN] [--sort
  match|recent|stars|new] [--forks] [--limit N] [--after CURSOR]` lists
  public repositories as `GET /v1/explore` does, the words of QUERY being
  its terms. A query or a login the hub would refuse is refused before
  anything is sent.
- `braid hub user LOGIN [--repos | --stars | --followers | --following]
  [--limit N] [--after CURSOR]` shows an account's profile, or one of its
  lists: its repositories, what it starred, its followers, or the accounts
  it follows, as far as the account signed in may see them.

**Stars, watching and following** (section 17).

- `braid hub star [-R OWNER/NAME]` and `braid hub unstar [-R OWNER/NAME]`
  star a repository, or take the star back. With `--json` they print
  `repo` and `starred`.
- `braid hub stars [--user LOGIN] [--limit N] [--after CURSOR]` lists the
  repositories the account signed in starred, or those `--user` starred
  that it may read, newest first.
- `braid hub watch [-R OWNER/NAME] [--level all|participating|ignore]`
  (`all` unless given) and `braid hub unwatch [-R OWNER/NAME]`, which goes
  back to `participating`. With `--json` they print `repo`, `level` and,
  for a level chosen, `since`.
- `braid hub follow LOGIN` and `braid hub unfollow LOGIN`. With `--json`
  they print `login` and `following`.

**Issues and labels** (section 18).

- `braid hub issue list [-R …] [--state open|closed|all] [--label
  LABEL]… [--assignee LOGIN] [--author LOGIN] [--search TEXT] [--sort
  created|updated|comments] [--limit N] [--after CURSOR]` lists issues;
  `--search` works on public repositories only.
- `braid hub issue show N [-R …]` prints the issue and its first 100
  comments; with `--json`, `{"issue", "comments", "next"}`.
- `braid hub issue create --title TITLE [--body TEXT | --body-file FILE]
  [--label LABEL]… [--assignee LOGIN]… [-R …]` opens an issue, and `braid
  hub issue comment N [--body TEXT | --body-file FILE] [-R …]` comments on
  one; `--body-file -` reads standard input, and a text holds at most
  64 KiB. With `--json` they print the issue, or the comment.
- `braid hub issue close N [--reason completed|not-planned] [-R …]`,
  `braid hub issue reopen N [-R …]`, and `braid hub issue edit N [--title
  TITLE] [--body TEXT | --body-file FILE] [--add-label LABEL]…
  [--remove-label LABEL]… [--add-assignee LOGIN]… [--remove-assignee
  LOGIN]… [--lock | --unlock] [-R …]` change an issue; with `--json` they
  print it.
- `braid hub label list [-R …]`, `braid hub label create NAME [--color
  0-11] [--description TEXT] [-R …]` and `braid hub label delete NAME
  [-R …]`.

**Change requests** (section 19). `braid hub request --help` describes the
whole flow.

- `braid hub request create --title TITLE [--body TEXT | --body-file FILE]
  [--head [OWNER/NAME:]BRANCH] [--base BRANCH] [--expose] [--draft]
  [--reviewer LOGIN]… [-R …]` opens a change request. Without `--head`, in
  a workspace, it takes the workspace's changesets that its branch on the
  hub lacks, as `braid land` finds them, uploads them, and puts them on a
  new branch on the hub: `request/<login>/<slug>` of the repository itself
  when the caller may write there and its token may land on that name, and
  otherwise `request/<slug>` of the fork the repository is linked to (the
  slug comes from the title: lowercase ASCII letters, digits and `-`, at
  most 40 characters, with `-2`, `-3` and so on when the name holds other
  work). The workspace and its branch do not move. With `--head`, it
  proposes that branch as it is on the hub. It prints the request's number
  and its address.
- `braid hub request list [--state open|closed|landed|all] [--author
  LOGIN] [--base BRANCH] [--reviewer LOGIN] [--limit N] [--after CURSOR]`,
  `braid hub request show N [--diff]` (what it proposes, whether it lands,
  its approvals, reviews and comments) and `braid hub request diff N [--rev
  REV] [--path PATH]` (the diff of its current revision, or of `--rev`,
  `A..B` for what changed from revision A to revision B).
- `braid hub request comment N [--body TEXT | --body-file FILE] [--path
  PATH --line N [--side old|new] [--revision N]]` comments on it, or on a
  line of its diff; `braid hub request review N (--approve |
  --request-changes | --comment) [--body TEXT | --body-file FILE]
  [--revision N]` reviews it, and the hub refuses the review (`moved`)
  when the request has moved past `--revision` since.
- `braid hub request update N [--head [ID]]` gives it a new revision, its
  author only: in a workspace, the workspace's changesets that its branch
  on the hub lacks, put on the request's head branch (as a fast-forward
  when they build on it, otherwise in its place); with `--head`, what the
  head branch holds on the hub now, or the changeset ID on its
  first-parent chain.
- `braid hub request land N [--expect SNAPSHOT] [--mode rebase|ff-only]`
  lands the snapshot `--expect` names, or else the one the request
  proposes when it is read, which it prints first. Exit status 0 landed;
  2 conflicts, printed as `braid land --json` prints them, the request
  staying open; 1 refused (the request moved past the snapshot, the base
  no longer holds where its stack starts, landing not allowed); 3 the hub
  could not be reached, or failed. With `--json`, `{"request", "land"}`.
- `braid hub request close N`, `braid hub request reopen N` and `braid hub
  request ready N` (a draft ready for review).
- `braid hub request checkout N [--branch NAME] [--force]` brings the
  request's snapshot into this repository as the branch `request/<N>`, its
  changesets coming through the request itself (`POST
  …/requests/{n}/missing`), so that whoever reads the repository the
  request is on fetches it, even without reading the repository it comes
  from. The branch is created, or
  moved forward; one with changesets the snapshot lacks stays where it is,
  unless `--force`.

**The inbox** (section 21).

- `braid hub inbox [--all] [-R OWNER/NAME] [--limit N] [--after CURSOR]`
  lists the notifications of the account signed in, newest first: the
  unread ones, all of them with `--all`, those of one repository with
  `-R`. It marks nothing. Each line gives `*` for an unread one, its id,
  the thread or the notice, and who did what, why and when, then how many
  are unread.
- `braid hub inbox read ID…` and `braid hub inbox read --all` mark
  notifications read; with `--json` they print `marked`. `braid hub inbox
  done ID` takes one out of the inbox; with `--json` it prints `id` and
  `done`.
- `braid hub inbox mute OWNER/NAME#NUMBER` and `braid hub inbox unmute
  OWNER/NAME#NUMBER` stop, or bring back, the notifications of a thread;
  with `--json` they print `repo`, `number`, `subscribed` and `reason`.

**Forks and jobs** (section 20).

- `braid hub fork OWNER/NAME [--name NAME] [--private] [--clone [DIR]]
  [--no-wait]` forks a repository into the account signed in to, waits
  until its job ends and prints the job and the fork; with `--clone`, it
  then clones the fork as `braid hub clone` does; with `--no-wait`, it
  prints the job as it starts. It sends an `Idempotency-Key`. Exit status
  1 when the hub refused or the job failed.
- `braid hub sync-fork [-R OWNER/NAME] [--ref REF]` syncs a fork with its
  upstream; exit status 2 when they diverged.
- `braid hub jobs [ID] [--wait] [--limit N] [--after CURSOR]` lists the
  account's jobs, or shows one; with `--wait`, until it ends, with exit
  status 1 when it failed or was cancelled.

**GitHub** (sections 2 and 23).

- `braid hub github grant [--public-only]`: the device flow of section 2;
  braid prints the address and the code on standard error, and waits.
- `braid hub github repos [--refresh]`: the listing of the repositories
  the account owns on GitHub.
- `braid hub github sync (--all | FULL_NAME...) [--name FULL_NAME=NAME]...
  [--all-private] [--reserve-tdm] [--follow merge|ff-only|off] [--wait]`:
  a batch; it sends an `Idempotency-Key`. With `--wait`, braid waits until
  it ends, and the exit status is 1 when any repository failed or was
  skipped.
- `braid hub github status [BATCH]`, `braid hub github cancel BATCH` and
  `braid hub github retry BATCH FULL_NAME [--name NAME]`.

**Settings and blocks** (sections 3, 4 and 22).

- `braid hub settings [-R OWNER/NAME] [--public | --private] [--description
  TEXT] [--default-branch BRANCH] [--topics T1,T2] [--tdm
  allowed|reserved] [--issues on|off] [--requests-from
  readers|collaborators|off] [--forks on|off] [--approvals N] [--follow
  off|ff-only|merge]`: with no flags it prints the repository's settings,
  all of them to its admins; with flags it changes those, as `PATCH
  /v1/r/{owner}/{name}` does, and prints the repository as it is now.
  `--topics ""` takes every topic off. With `--public` it says on standard
  error what the repository shows now.
- `braid hub block LOGIN [-R OWNER/NAME]` and `braid hub unblock LOGIN [-R
  OWNER/NAME]`: without `-R`, from every repository of the account signed
  in; with it, from that repository, which the account administers. With
  `--json` they print `login`, `blocked` and, with `-R`, `repo`.
- `braid hub account [--tdm-default allowed|reserved] [--stars-public
  on|off]`: with no flags it prints the account's settings; with flags it
  changes them, as `PATCH /v1/me` does. With `--json` it prints what `GET
  /v1/me` gives.

**Any route.** `braid hub api METHOD PATH [--data JSON | --data-file FILE]
[--hub URL]` sends one request to the hub's API, with the token braid
holds for the hub, and prints its answer's body as it came. `METHOD` is
`GET`, `HEAD`, `POST`, `PUT`, `PATCH` or `DELETE`; `PATH` starts with
`/v1`, such as `/v1/me`; `--data` or `--data-file` (`-` for standard
input) give a JSON body. Exit status 0 when the hub did it, 1 when it
refused, with its error printed on standard output, and 3 when it could
not be reached or failed.

## 14. The web

Pages are rendered on the hub, in English, with no script, font or anything
else from elsewhere; their stylesheet is `GET /static/hub.css`, and their
icon `GET /static/icon.svg`. Every answer carries `Content-Security-Policy:
default-src 'self'; script-src 'none'; object-src 'none'; base-uri 'none';
form-action 'self'; frame-ancestors 'none'` (an answer with a policy of its
own, as a raw file's, keeps it: section 15), `X-Content-Type-Options:
nosniff` and `Referrer-Policy: no-referrer`, so that a link followed from a
page never tells another site the address of a private repository; pages
also carry `Cache-Control: no-store`. The pages of a repository carry the
`<meta>` tags of section 24 in their `<head>`.

The header of every page links Explore and holds a search box, a `GET` to
`/explore`; signed in, it also links the inbox, with how many
notifications are unread, a new repository, Import (`/-/onboarding`), the
tokens, the account's profile, and signing out. The footer of every page
links `/agents.md`, `/terms`, braid's site and the hub's version.

**The hub's pages.**

- `GET /`: what BraidHub is, "Sign up or sign in with GitHub" (the first
  sign-in makes the account; repositories are private by default; the hub
  reads the GitHub identity only), the guide for agents, and a link to
  explore. Signed in: the offer to bring the account's GitHub repositories
  (section 23), until it imports something or skips it, or, while a batch
  runs, a link to its progress; the account's repositories and a form to
  make one (name, private by default, description, GitHub mirror); its five
  latest unread notifications; and what the accounts it follows did lately
  in public repositories: the repositories of theirs that moved, and the
  ones they starred, unless they keep their stars private.
- `GET /explore?q&topic&owner&sort&forks&after`: what explore and search
  list (section 16), 30 cards a page, with a search box, the orders as
  tabs, a switch for forks, and the 20 most used topics. When nothing is
  found, it says how to bring repositories to the hub. A query the
  protocol does not take shows the page again with why, as a 400.
- `GET /topics/{topic}?q&owner&sort&forks&after`: the repositories with the
  topic, and how many; what is not a topic is a missing page.
- `GET /{login}`: an account's profile: its initials, name and login, when
  it joined, a Follow button for whoever is signed in, and tabs for its
  repositories, its stars (`?tab=stars`), its followers
  (`?tab=followers`) and the accounts it follows (`?tab=following`), each
  with its count and holding what the viewer may see; its stars are shown
  to others only when it lets them be (section 17). No activity is shown.
  Every profile is at `GET /-/u/{login}` too, which is where the profile of
  a login that names a page of the hub (`explore`, `settings`, `tokens`,
  …) lives. A login that is no account, or a suspended one, answers as a
  route that does not exist: a page for a browser, the JSON 404 of
  section 1 otherwise.
- `GET /tokens`: the account's tokens, a form to make one (repositories,
  rights, expiry, note), shown once, and revoking.
- `GET /settings`, for a browser signed in (one signed out is sent to
  `/login`): the account's settings: the accounts it blocked, each with a
  form that lifts the block, and a form to block one more; whether its
  stars show to others; what text and data mining its new repositories
  start with; its GitHub access, with the scope it granted, links that
  grant it again (for all its repositories, or its public ones only) and a
  notice when GitHub refused it; and where it brings its GitHub
  repositories to the hub.
- `GET /notifications?all&repo&after`: the inbox of the account signed in
  (section 21), its unread notifications or all of them (`?all=1`), of one
  repository with `?repo=owner/name`, newest first, grouped by repository,
  a page at a time. A repository the session may not read is the page of a
  missing one; anyone not signed in is sent to sign in.
- `GET /-/jobs/{id}`: a job of the session's account (section 20), reloaded
  every 3 seconds until it ends, with a form to cancel it while it has not
  started. Another account's job, or none, is the page of a missing one.
- `GET /-/onboarding`: what syncing the account's GitHub repositories does
  (section 23), the access it needs and how to take it back on GitHub, the
  grant links, and a form that skips it.
- `GET /-/onboarding/github?refresh`: the account's repositories on GitHub
  in a table, with the defaults checked (owned, neither forks nor
  archived, most recently pushed first, within the room left), a name to
  edit for each, and the options of a batch.
- `GET /-/onboarding/github/progress?batch`: how the batch stands, with a
  `<progress>` for each repository; it reloads itself every five seconds
  while the batch runs. Another account's batch is the page of something
  missing.
- `GET /terms`, `GET /terms.md`, `GET /robots.txt` and `GET
  /.well-known/tdmrep.json`: section 24.
- `GET /agents.md`: the guide for coding agents, `text/markdown;
  charset=utf-8`. `GET /healthz`: `ok`, for anyone.
- Sign-in: `GET /login`, `GET /auth/github/callback`, `GET
  /auth/github/repo-access`, `POST /logout` (section 2).

**A repository's pages** need what reading it needs: a private repository's
pages and forms are 404 to whoever may not read it, as a missing
repository's are, whatever the path.

- `GET /{owner}/{name}`: the repository's home: its files at the default
  branch (or, while that has nothing, its first branch, then its first
  tag), with the last changeset and the README rendered; a switcher of its
  branches and tags (the first 100); what it is about: its description,
  topics and default branch; how to clone it (`braid hub clone
  owner/name`, and `git clone` from its mirror when the repository is
  public and mirrored on GitHub); and the mirror's state (section 11).
  While nothing has landed, it says how to publish.
- The header of a repository's pages shows its stars, linked to its
  stargazers, how many watch all of it, and how many public forks it has,
  linked to its forks. To a signed-in reader who may interact with it, it
  also has a Star button (Starred, once it is), a Watch menu with the three
  levels, and, where forks are on, a Fork button. A fork's header says
  which repository it was forked from, when the viewer may read that one,
  and how many changesets its default branch is behind the upstream's, up
  to 1,000, with a form that syncs it for whoever may land on that branch,
  where a sync may bring the upstream in (section 20). Its admins find a
  Settings tab.
- `GET /{owner}/{name}/tree/{REV}/{path}?after`: a directory, `dir_page`
  entries a page, with its README.
- `GET /{owner}/{name}/blob/{REV}/{path}?plain`: a file: its text, escaped,
  with numbered lines and their anchors (`#L1`, `#L2`, …), when it is UTF-8
  and at most `text_view` bytes, and at most 20,000 lines of it; Markdown
  rendered (`?plain=1` for its source); a PNG, JPEG, GIF or WebP image from
  the raw page; for a binary file, its size and a link to its bytes; a
  symlink's target, as text; a gitlink's commit. Each page links to the
  same file or directory by the changeset's full id, an address that never
  changes.
- `GET /{owner}/{name}/raw/{REV}/{path}`: the file's bytes, with the
  headers of `GET …/raw` (section 15), but shown rather than saved when they
  are UTF-8 text (`text/plain; charset=utf-8`), or a PNG, JPEG, GIF or
  WebP image whose name says so and whose first bytes are one. HTML, SVG
  and XML files, and a file named as an image whose bytes are not one, are
  always attachments: nothing a repository holds runs on the hub's pages.
- In these addresses, `REV` is the longest branch or tag that the rest of
  the path starts with, followed by a `/`, a full id, or the start of one;
  a ref's `/` may also be written `%2F`. The address of a directory that
  names a file leads to the file's page, and the other way round (`303 See
  Other`). A git commit shows only when the hub knows it already. The
  pages' links, and those of the READMEs and Markdown files they render,
  name a branch or tag by its changeset's full id instead when an address
  that names it would be read otherwise: when a longer ref starts with its
  name and a `/`, or when its name has a part a browser drops before
  asking (empty, `.` or `..`).
- History, under the History tab: `GET /{owner}/{name}/commits` and `GET
  /{owner}/{name}/commits/{rev}`, the changesets down the first parents of
  the default branch, or of `rev`, 50 to a page; `GET
  /{owner}/{name}/changeset/{rev}?page`, a changeset, its message as text
  (never rendered), who made it and when, its parents and extra fields, and
  its diff, a page of files at a time, saying what it leaves out; `GET
  /{owner}/{name}/branches` and `GET /{owner}/{name}/tags`; and `GET
  /{owner}/{name}/compare/{base}...{head}`, or
  `/{owner}/{name}/compare/{base}...{owner}/{name}:{head}` for a head in
  another repository of the same fork network: what the head brings, and
  its diff. `GET /{owner}/{name}/compare?base&head` asks for the two
  revisions, and with both leads to their comparison.
- `GET /{owner}/{name}/stargazers?after`: the accounts that starred the
  repository, latest first, a page at a time, but those that keep their
  stars private (each sees itself).
- `GET /{owner}/{name}/forks?after`: the forks the viewer may read, newest
  first.
- `GET /{owner}/{name}/issues?state&labels&assignee&author&q&sort&after`:
  open or closed issues, filtered by label, assignee or author and, on a
  public repository, by title, a page at a time. `GET
  /{owner}/{name}/issues/new`: a new issue (signed in), with a preview.
  `GET /{owner}/{name}/issues/{n}?after`: the issue and its timeline, with
  the forms of what the viewer may do: comment; edit, close and reopen it
  (its author and writers); label, assign and lock it, and hide comments
  (writers). `GET /{owner}/{name}/labels`: the repository's labels, with
  the forms that make and delete them, for writers.
- `GET /{owner}/{name}/requests?state&after`: the change requests, open
  ones first. `GET /{owner}/{name}/requests/new?head_repo&head&base`
  compares a head with a base and opens the request (the box that
  publishes a private head's work shows to its admins only). `GET
  /{owner}/{name}/requests/{n}?after`: the request, its conversation a page
  at a time, its reviews, whether it lands cleanly with its conflicts, and
  the form that lands its snapshot for those who may. `GET
  /{owner}/{name}/requests/{n}/changesets`: its stack, and what it brings
  besides. `GET /{owner}/{name}/requests/{n}/changes?rev&page`: the diff of
  a revision with its line comments, or what changed between two.
- `GET /{owner}/{name}/settings`, for its admins: a section with a form of
  its own for each of its visibility, its description and default branch,
  its topics, its features (issues, who may open change requests, forks,
  the approvals a change request needs before it lands), text and data
  mining, its mirror (with the follow policy, the outcome and what it says,
  `synced`, how the hub hears of pushes and when it looked last), its
  collaborators, the accounts it blocked, and deleting it (typing its name
  again). Making a repository public asks for its name typed again, under a
  warning: everything it ever stored becomes readable by anyone who knows
  its id (section 4), so a fresh public repository is the way to publish
  only what it holds now.

**Forms** post to the hub with the session's CSRF token in the field
`csrf` (403 without it, with it in a header instead, or without the
session's cookie), need what their route of the API needs, count toward
the account's limits as it does (section 12), and do what it does, with
what the session's account may do. They lead back with `303 See Other`:
to the page named in `back`, where a form takes one and it is a path of
the hub, else to the page or the section they belong to. The forms:

- the account: `/new` (`name`, `private`, `description`, `mirror`);
  `/tokens` (repositories, rights, expiry, note), which answers the tokens
  page with the token made, shown that once; `/tokens/{id}/revoke`;
  `/logout`; `/settings/blocks` (`login`) and
  `/settings/blocks/{login}/remove`; `/settings/stars` (`stars_public`);
  `/settings/tdm` (`tdm_default`).
- following and the inbox: `/-/follow/{login}` and `/-/unfollow/{login}`;
  `/notifications/read` (`id`, given again for more, or `all` and,
  optionally, `repo`) and `/notifications/done` (`id`), which lead back to
  the inbox unless `back` says otherwise;
  `/{owner}/{name}/threads/{number}/subscription` (`subscribed`, `1` to
  follow the thread or `0` to mute it).
- jobs and syncing GitHub: `/-/jobs/{id}/cancel`; `/-/onboarding/skip`,
  after which the home page offers syncing no more;
  `/-/onboarding/github/sync`, which names repositories by `github_id`
  only: `repo` (repeated, at most 1,000), `name.<github_id>` (one a row, at
  most 1,000; given twice, the last counts), `all=1` for every eligible
  one, `all_private`, `reserve_tdm` and `follow`, more than 1,000 of either
  being 400, and which leads to the batch's progress;
  `/-/onboarding/github/retry` (`batch`, `github_id`, `name`) and
  `/-/onboarding/github/cancel` (`batch`), which lead back to it.
- a repository's settings: `/{owner}/{name}/settings` (`description` and
  `default_branch`, and, from forms that send them, `visibility` and
  `mirror`; it leaves alone what a form does not send);
  `/{owner}/{name}/settings/visibility` (`visibility`, `private` or
  `public`, and `confirm`, the name typed again to make it public);
  `/{owner}/{name}/settings/topics` (`topics`, parted by commas or spaces);
  `/{owner}/{name}/settings/features` (`issues`, `requests_from`,
  `allow_forks`, `require_approvals`); `/{owner}/{name}/settings/tdm`
  (`tdm`); `/{owner}/{name}/settings/mirror` (`mirror`, the address, empty
  to remove it); `/{owner}/{name}/mirror/settings` (`follow`: `off`,
  `ff-only` or `merge`); `/{owner}/{name}/mirror/sync`, held to the minute
  of the API (section 23); `/{owner}/{name}/mirror/follow`;
  `/{owner}/{name}/collaborators` (`login`, `permission`) and
  `/{owner}/{name}/collaborators/{login}/remove`; `/{owner}/{name}/blocks`
  (`login`) and `/{owner}/{name}/blocks/{login}/remove`; and
  `/{owner}/{name}/delete` (`confirm`, the name typed again). Each leads
  back to its section of the settings page, but these: deleting leads to
  `/`; `…/mirror/follow` leads to the repository's page, where its outcome
  shows; and `/{owner}/{name}/settings` and the collaborators' forms lead
  to the repository's page unless `back` names another page of the hub, as
  the settings page's own forms do.
- stars, watching and forks: `/{owner}/{name}/star` and
  `/{owner}/{name}/unstar`; `/{owner}/{name}/subscription` (`level`);
  `/{owner}/{name}/fork` (`name`, `private`), which leads to the page of
  its job; `/{owner}/{name}/sync-upstream` (`ref`, the default branch when
  empty), which answers a page that says so (409) when the fork diverged.
- issues: `/{owner}/{name}/issues` (`title`, `body`, `label`…,
  `assignees`), `/{owner}/{name}/issues/{n}/comments` (`body`),
  `/{owner}/{name}/issues/{n}/edit` (`title`, `body`),
  `/{owner}/{name}/issues/{n}/state` (`state`, `reason`),
  `/{owner}/{name}/issues/{n}/labels` (`label`…, all of them),
  `/{owner}/{name}/issues/{n}/assignees` (`assignees`, the logins parted by
  commas, all of them), `/{owner}/{name}/issues/{n}/lock` (`locked`, `1` or
  `0`), `/{owner}/{name}/comments/{id}/edit` (`body`),
  `/{owner}/{name}/comments/{id}/delete`,
  `/{owner}/{name}/comments/{id}/hide` (`reason`, empty to show it again),
  `/{owner}/{name}/labels` (`name`, `color`, `description`) and
  `/{owner}/{name}/labels/{label}/delete`. `/{owner}/{name}/preview`
  (`body`, with the fields of the form it came from) shows a text as it
  will show, with its form filled in again.
- change requests: `/{owner}/{name}/requests` (`title`, `body`,
  `head_repo`, `head`, `base`, `draft`, `expose`), which leads to the
  request; `/{owner}/{name}/requests/preview`, which shows its text as it
  will show, with the form filled in again;
  `/{owner}/{name}/requests/{n}/comments` (`body` and, for a line comment,
  `path`, `line`, `side` and `revision`);
  `/{owner}/{name}/requests/{n}/review` (`verdict`, `body`, and `revision`,
  the revision the page showed); `/{owner}/{name}/requests/{n}/reviewers`
  (`logins`, all of them, parted by commas);
  `/{owner}/{name}/requests/{n}/state` (`state`, `open` or `closed`);
  `/{owner}/{name}/requests/{n}/ready`; `/{owner}/{name}/requests/{n}/update`
  (a new revision from its head's branch as it is now, its author only);
  `/{owner}/{name}/requests/{n}/land` (`snapshot`, the one the page showed,
  and `mode`); and `/{owner}/{name}/requests/{n}/delete`.

Under `/v1`, a path that is not a route is the JSON 404 of section 1;
elsewhere a browser (`Accept: text/html`) gets a page.

## 15. Browsing

A repository can be read without cloning it. Every route of this section
needs `read` on the repository; a private repository the caller may not
read answers 404 `not_found`, exactly as a missing one does, whatever the
path, the revision or the query.

**Revisions.** `REV` (the `ref` of a query, `{rev}`, `base` or `head`, or
the start of a page's path) is a branch, a tag as `tags/<name>`, a
changeset's full id, or the first 12 to 63 hex digits of the id of a
changeset the repository took in by a landing, an import or a fork; the
default branch when none is given. The hub's own refs (`braid/…`,
`remote/…`, `git/…`) are never revisions. An id names only a changeset the
repository took in: an object uploaded and never landed is no revision, by
its whole id or by its start, though `GET …/o/{id}` still serves it
(section 6). A revision that names nothing, and the start of an id that
more than one changeset's id begins with, are 404, and the answer names no
changeset. In a route's path, `{rev}` encodes a `/` as `%2F`.

**Paths.** A path is names joined by `/`, percent-encoded in the route; its
bytes need not be UTF-8. It must be one a repository may hold (no empty
name, no `.` or `..`, no `.braid` or `.git`): 400 `invalid` otherwise. One
`/` at either end is ignored. Symlinks are never followed: a path through
one is 404. In answers, names and paths are UTF-8, with what is not UTF-8
replaced; one that is not UTF-8 also comes percent-encoded in `raw_name` or
`raw_path`, which a route takes as it is.

- `GET /v1/r/{owner}/{name}/contents` and `GET
  /v1/r/{owner}/{name}/contents/{*path}?ref&text&after` → what is at the
  path (the root, without one):
  - a directory: `{"type": "dir", "ref", "changeset", "path", "raw_path",
    "entries": [{"name", "raw_name", "kind", "id", "size"}], "next"}`. Its
    directories come first, then the rest, each in order of their names'
    bytes, at most `dir_page` entries a page (`limits.social` of `GET /v1`;
    1,000 by default). `next` is the cursor of the next page, for `?after=`,
    and `null` on the last. `kind` is `file`, `executable`, `symlink`,
    `tree` or `gitlink`; `id` is the blob or the tree, or a gitlink's git
    commit in hex; `size` is given for files, executables and symlinks.
  - anything else: `{"type": "file", "ref", "changeset", "path",
    "raw_path", "kind", "id", "size", "binary", "text", "target",
    "commit"}`. `binary` is true when a NUL is among the first 8,000 bytes.
    `text` is the content of a file or an executable that is not binary,
    is UTF-8 and is at most `text_view` bytes (`limits.social`; 1 MiB by
    default), and `null` otherwise; `text=false` leaves it out. A symlink
    gives its `target`, never what it points to; a gitlink its `commit`,
    and no `size`.
  - `ref` is the branch or tag the revision named, `null` for an id;
    `changeset` is the changeset read, as 64 hex digits. `raw_name` and
    `raw_path` are there only for names and paths that are not UTF-8.
  - 404 for a revision or a path that is not there; 400 for a path or a
    cursor that is not valid.
- `GET /v1/r/{owner}/{name}/raw/{*path}?ref` → the file's bytes, as an
  attachment: `Content-Type: application/octet-stream`,
  `Content-Disposition: attachment` with the file's name in `filename*`,
  `Braid-Blob` (the blob's id) and `ETag` (the blob's id, quoted; a request
  whose `If-None-Match` names it answers 304),
  `Content-Security-Policy: default-src 'none'; sandbox`,
  `X-Content-Type-Options: nosniff` and
  `Cross-Origin-Resource-Policy: same-origin`. A symlink gives its target,
  as text; a directory or a gitlink has no bytes (404).
- `GET /v1/r/{owner}/{name}/readme?ref&path` → the README of the directory
  `path` (the root, without one): `{"path", "raw_path", "blob", "size",
  "markdown", "html", "truncated"}`. A directory's README is its file
  named `README` or `README.<extension>`, in any case: Markdown (`.md`,
  `.markdown`, `.mdown`, `.mkdn`, `.mkd`) first, then `README` and
  `README.txt`, then any other, by name; a symlink is never one.
  `markdown` is its text, cut at 512 KiB on a character's boundary, when
  `truncated` is true; `html` is that text rendered as section 1 renders
  Markdown, or as preformatted text when the file is not Markdown. 404 when
  the directory has none, or the path is not a directory.

The answers of these three routes about a private repository carry
`Cache-Control: private, no-store`. A public repository's are kept a
minute (`public, max-age=60`) when they were asked by a branch, a tag or
the start of an id, which may name another changeset later, and for good
(`public, max-age=31536000, immutable`) when they were asked by a full id.

### History

The times of changesets are RFC 3339 with the offset from UTC the
changeset recorded (`Z` for UTC). What a changeset holds as bytes (names,
e-mail addresses, messages, paths) comes as UTF-8, with what is not UTF-8
replaced. Lists take `limit` and `after` as section 1 says; anything else
is 400 `invalid`.

A changeset in a list is `{"id", "parents", "author", "committer",
"summary", "agent"}`: `author` and `committer` are `{"name", "email",
"time"}`, `summary` is the first line of the message that is not blank,
trimmed, at most 200 characters, and `agent` the agent that made it, or
`null`. A file that changed is `{"path", "status", "old", "new"}`:
`status` is `added`, `deleted`, `modified` (its content or its executable
bit) or `type_changed` (between a file, a symlink and a gitlink); `old`
and `new` are what was there and what is, `null` where there is nothing,
each `{"kind", "blob", "size"}` (`kind` `file`, `executable` or
`symlink`, whose content is its target) or `{"kind": "gitlink",
"commit"}`. Files come in order of their paths' bytes, as change lists
order them.

- `GET /v1/r/{owner}/{name}/log?ref&after&limit` → `{"ref",
  "changesets": [...], "next"}`: the changesets down the first parents of
  `ref` (the default branch when it is not given), newest first. `next` is
  `null` at the first changeset; a cursor names where its log goes on,
  whatever `ref` says since.
- `GET /v1/r/{owner}/{name}/changesets/{rev}` → `{"id", "parents",
  "root", "author", "committer", "message", "agent", "rebased_from",
  "git_commit", "extras", "files", "files_truncated"}`: the changeset, its
  whole message, its extra fields as `[name, value]` pairs, and the files
  it changes against its first parent (against nothing for a changeset
  without parents), at most 300, with `files_truncated: true` when there
  are more. `rebased_from` is the changeset a landing rebased it from, or
  `null`. `git_commit` is the git commit it exports to when the hub knows
  it already, as it does for what landed since it started; otherwise
  `null`, and `GET …/git/changeset/{id}` (section 9) computes it.
- `GET /v1/r/{owner}/{name}/changesets/{rev}/diff?page&path&context` →
  `{"files": [...], "next_page"}`: a page of the changeset's diff against
  its first parent, at most 300 files, `next_page` the number of the page
  after it or `null`. `page` counts from 1 (a page past the last one is
  empty), `context` is the lines around each change, 0 to 10 (3 by
  default), and `path` asks for that one file alone (404 when the
  changeset does not change it). Each file is the file that changed with
  `"binary", "additions", "deletions", "hunks", "truncated"`. A hunk is
  `{"old_start", "old_lines", "new_start", "new_lines", "lines"}`, numbered
  as a unified diff numbers it (from 1; a hunk without lines of one
  version starts at the line it comes after, 0 at the start), and each
  line is `[op, text]`: `op` is `" "` for a line both versions have, `"-"`
  for one of the old version and `"+"` for one of the new, and `text` the
  line with its line ending, when it has one. A version with a NUL byte
  among its first 8,000 bytes is binary and is not diffed by lines; a
  gitlink diffs as the line `Subproject commit <hex>`. Within the sizes of
  section 12, a version too large to diff gives its file no hunks, the
  hunks of a file stop after a number of lines, and once the lines of a
  page hold as much as a whole diff may, the files after them come without
  hunks: each such file says `truncated: true`, and asking for it alone
  with `path` gives it whole when it fits. A file whose content stays the
  same (a new executable bit alone) has no lines to diff: it has no hunks,
  and is neither `binary` nor `truncated`, whatever its content.

  With `Accept: text/x-diff`, the answer is the whole diff as `git diff`
  writes it, `Content-Type: text/x-diff`, every file and every hunk whole:
  `git apply` takes it. Names are quoted as git quotes them; a changed
  type is the old file deleted and the new one added; a binary file, or a
  version too large to diff, is `Binary files a/<path> and b/<path>
  differ`, so that `git apply` refuses it rather than apply less; a new
  mode alone is its `old mode` and `new mode` lines, as git writes it,
  whatever the content. A diff larger than the whole diff may be is 413
  `too_large`, unless `path` asks for one file. `page` is ignored.
- `GET /v1/r/{owner}/{name}/compare?base&head&head_repo` → `{"base",
  "head", "merge_base", "stack", "brings", "files", "truncated"}`: what
  `head` brings to `base` (the default branch when it is not given).
  `head` is needed (400 without it). `base` and `head` are the changesets
  the two revisions name; `merge_base` is their best common ancestor (the
  one with the smallest id when there are several), or `null` when they
  share no history. `stack` is the head's first-parent chain down to a
  changeset the base has, oldest first; `brings` is everything else the
  head has and the base lacks, which merges in the stack bring, parents
  before children; `files` are the files the head changes since the merge
  base (since nothing, when there is none), at most 300. At most 1,000
  changesets are listed, the newest of the stack first, and `truncated:
  true` says that changesets or files were left out. `head_repo`
  (`owner/name`) takes the head from another repository: one the caller
  may read, or the answer is the 404 of a missing repository, and one
  forked from the same repository as this one, directly or not, or the
  answer is 400 `invalid`.
- `GET /v1/r/{owner}/{name}/branches?after&limit` and `GET
  /v1/r/{owner}/{name}/tags?after&limit` → `{"refs": [{"name", "head",
  "time", "summary"}], "next"}`: the branches, or the tags, in order of
  their names, each with the changeset it points to, when that changeset
  was recorded and its summary. `name` is the ref as a revision names it:
  `main`, `tags/v1`. The hub's own refs are never listed.

These answers say how long they may be kept: an answer that never changes
(a diff of a full id, a log from a full id or a cursor) may be kept, by
anyone for a public repository and by the client alone for a private one;
any other answer about a private repository is `Cache-Control: private,
no-store`, and about a public one, `public, max-age=60`. A diff's answer
is JSON or text at the same address, and says `Vary: Accept`.

## 16. Discovery

Explore, search and topics list **public repositories that something
landed on, of established owners**, and no fork unless asked. An owner is
established once its account on the hub is seven days old and, when the
hub knows when its GitHub account was made, that account thirty days old:
until then, its public repositories are read by their links and shown on
its profile, but listed by none of these. A repository the hub's operator
hid or took down is listed by none of them either (section 22).

No private repository is listed, matched or counted by explore, search or
topics, to anyone, its owner and its readers included: those find it in
their own lists (section 4) and on the owner's profile. A repository
leaves these lists as soon as it turns private, is hidden or taken down,
or its owner is suspended: the next request lists it no more.

Anyone may ask, with or without a token; a token sent must be valid.

A **card** is what lists of repositories hold, here and in sections 17 and
20: `{"owner", "name", "private", "description", "topics", "stars",
"forks", "created", "pushed", "default_branch", "fork_of", "mirror_url",
"tdm"}`, with `starred_at` or `level` where section 17 says. `fork_of`
names the repository it was forked from only when the caller may read
that one; `mirror_url` is the GitHub repository it mirrors to, if any.

- `GET /v1/explore?q&topic&owner&sort&forks&limit&after` → `{"repos":
  [<card>…], "next"}`.
  - `q`: at most 100 characters, in at most 10 terms parted by spaces or
    `/`, so that `owner/name` finds a repository by both. A term holds
    letters, digits, `.`, `_` and `-`, and matches the start of a word of
    a repository's owner, name, description or topics, whatever its case
    and accents; every term must match. A term of `.`, `_` and `-` alone
    matches nothing and is left out. Any other character (a quote, `*`, a
    parenthesis, `:`) is 400 `invalid`: a query is never read as a query
    language, and `OR` or `NEAR` are words like any other.
  - `topic`: only the repositories with this topic (section 4); `owner`:
    only those of this account.
  - `sort`: `match`, best match first, the default with `q`: a match in
    the name counts the most, then one in the owner, then in the topics,
    then in the description. `recent`, the latest landing first, the
    default without `q`. `stars`, the most stars first. `new`, the newest
    first. `match` without `q` orders as `recent`.
  - `forks=1` lists forks too.
  - A query, topic, owner, order or flag the protocol does not take is 400
    `invalid`.
- `GET /v1/topics?limit&after` → `{"topics": [{"name", "repos"}…],
  "next"}`: the topics of what explore lists without forks, each with how
  many of those repositories have it, the most used first, then by name.
- `GET /v1/topics/{topic}?q&owner&sort&forks&limit&after` → `{"repos":
  [<card>…], "next"}`: what `GET /v1/explore?topic={topic}` lists. What is
  not a topic is 404 `not_found`.
- `GET /v1/users/{login}` → `{"login", "name", "created", "followers",
  "following", "repos", "stars", "viewer"}`: an account, as the caller may
  see it. `repos` counts the account's repositories the caller may read;
  `stars`, its stars of repositories the caller may read, `null` when the
  account keeps its stars private (section 3) and the caller is not that
  account. `viewer`, for a caller with a token: `{"follows"}`, whether the
  caller follows it. A login that is not one (section 1) is 400 `invalid`;
  an account that does not exist, or is suspended, 404 `not_found`.
- `GET /v1/users/{login}/repos?type&sort&limit&after` → `{"repos":
  [<card>…], "next"}`: the account's public repositories, and its private
  ones the caller may read, marked `private`, each with what its settings
  say now. `type`: `all`, the default, `public`, `private`, `sources` (no
  forks) or `forks`. `sort`: `pushed`, the latest landing first and those
  that never landed last, the default; `name`; or `created`, the newest
  first. Any other is 400 `invalid`; an account as for its profile.

Lists come a page at a time, as section 1 says. A cursor belongs to its
query: given with another query, order, topic, owner or `forks`, it is
400 `invalid`. Explore, search and the topics give at most 50 pages of one
query: the fiftieth answers `next: null`. Each page goes on after the last
entry of the page before, so that what is listed meanwhile neither repeats
nor skips what was listed already; a repository whose stars or rank moved
meanwhile may move in the list.

## 17. Stars, watching and following

Starring and watching a repository interact with it (section 5): they need
an account that may read it, that is not suspended and not blocked there,
through a web session or a token whose scopes reach the repository with
`interact`, `write` or `admin`. A repository the caller may not read
answers 404 `not_found` to every method of these routes, as a missing one
does, before the body or anything else is looked at; one it reads but may
not interact with answers 403 `forbidden`, and an anonymous caller 401.
Following needs a web session, or a token with `interact`, `write` or
`admin` on `*` (403 without one).

Every write may be sent again: it answers as the first one did and changes
nothing more. Writes count toward the account's limits (section 12): a star
taken back counts as a star, a level of watching changed counts as a
watch, an account unfollowed counts as a follow. Lists come a page at a
time, as section 1 says.

**Stars.**

- `PUT /v1/r/{owner}/{name}/star` → 204, and `DELETE
  /v1/r/{owner}/{name}/star` → 204, whether the caller starred it before
  or not. An account stars at most 10,000 repositories: one more is 400
  `invalid`, until it takes a star back.
- `GET /v1/r/{owner}/{name}/stargazers?limit&after` → `{"users": [{"login",
  "name", "since"}], "next"}`: the accounts that starred it, latest first,
  with when each did. An account that keeps its stars private is left out
  of every list of stargazers but for itself, and still counts in
  `counts.stars`. `read`.
- `GET /v1/me/starred?limit&after` → `{"repos": [<card>…], "next"}`: the
  repositories the token's account starred, newest first, each with
  `starred_at`, of those the token may read now. A repository its account
  no longer reads, one it lost access to or one made private, is left out
  while that lasts; its star stays, and shows again if the access comes
  back. Needs a token.
- `GET /v1/users/{login}/starred?limit&after` → the same for any account,
  of the repositories the caller may read. An account may keep its stars
  private (section 3): then this is 404 `not_found`, "this account keeps
  its stars private", to everyone but the account itself. Anyone may ask.

**Watching.** An account watches a repository at one of three levels: `all`
(everything that happens in it), `participating` (the threads it takes part
in or is mentioned in) or `ignore` (nothing, not even those). Its owner
watches `all` of it until it chooses otherwise, and everyone else
`participating`.

- `GET /v1/r/{owner}/{name}/subscription` → `{"level", "since"}`: how the
  caller watches it; `since`, when it chose that level, is left out while
  the caller watches it as it does by default.
- `PUT /v1/r/{owner}/{name}/subscription {"level"}` → the same, as it is
  now. Choosing the default level again forgets the choice. An account
  chooses a level other than its default for at most 1,000 repositories:
  one more is 400 `invalid`; another level for one of those is not. A level
  that is not one of the three is 400 `invalid`.
- `DELETE /v1/r/{owner}/{name}/subscription` → 204: the caller watches
  what it takes part in, the owner too.
- `GET /v1/me/watching?limit&after` → `{"repos": [<card>…], "next"}`, each
  with `level`: the repositories the account chose a level for, and those it
  owns, which it watches `all` of unless it chose otherwise, in order of
  owner and name, of those the token may read. Needs a token.

**Following.**

- `PUT /v1/users/{login}/follow` → 204, and `DELETE
  /v1/users/{login}/follow` → 204, whether the caller followed the account
  before or not. A login that is not one (section 1) is 400 `invalid`; an
  account that does not exist, or is suspended, 404 `not_found`; following
  oneself, 400 `invalid`. An account cannot follow one that blocked it (403
  `forbidden`), and may always stop following it. An account follows at
  most 5,000 accounts: one more is 400 `invalid`.
- `GET /v1/users/{login}/followers?limit&after` and `GET
  /v1/users/{login}/following?limit&after` → `{"users": [{"login",
  "name", "since"}], "next"}`: the accounts that follow it, or that it
  follows, latest first, with since when. Follows are public: anyone may
  ask. A suspended account shows in no list, and its own lists are 404.

## 18. Issues, comments and labels

An issue is a thread of a repository: a title, a text and its comments,
with labels, assignees and a state. Issues and change requests (section 19)
share one numbering in their repository: a number names one thread,
whichever kind it is. `GET /v1/r/{owner}/{name}/threads/{n}` → `{"kind":
"issue"|"request", "number", "title", "state"}` says which (`read`).

Every route finds the repository before it looks at anything else of the
request: one the caller may not read answers 404 `not_found`, as a missing
one does, whatever the method, the number or the body. Reading needs
`read`. Every write interacts with the repository (section 5): it needs an
account that may read it, that is not suspended and not blocked there,
through a web session or a token whose scopes reach it with `interact`,
`write` or `admin` (401 for an anonymous caller, 403 otherwise), and the
body is read only for one that may. What a write may do is decided again
once its body is read, by the repository as it is then: one deleted
meanwhile, or made again under its name, answers 404 as a missing one, and
access lost meanwhile is lost for the write. Managing issues needs `write`:
labels, assignees, locks, and the issues and comments of other accounts.

Texts are as section 1 says: titles of 1 to 256 characters on one line,
texts of at most 64 KiB in Markdown, and an `agent` label shown as "via".
Writes count toward the account's limits (section 12): opening an issue as
an issue; a comment, or a change of a title or a text, as a comment; a
change of state, labels, assignees or lock, a comment hidden or deleted, a
label made or deleted, as an edit. A repository takes at most 100 new
threads an hour from accounts that do not write to it, between them, and
such an account has at most 100 open threads in one repository (403
`forbidden` past it, until it closes some).

**An issue** is `{"number", "title", "body", "state": "open"|"closed",
"state_reason": "completed"|"not_planned"|null, "author", "agent",
"labels": [<name>…], "assignees": [<login>…], "locked", "comments",
"created", "updated", "closed", "closed_by", "closed_by_changeset"}`.
`closed_by` names who closed it; `closed_by_changeset`, the changeset whose
landing closed it (section 8). The routes that answer one issue add `html`,
its text as the caller's pages show it.

- `GET /v1/r/{owner}/{name}/issues?state&labels&assignee&author&mentioned&q&sort&direction&limit&after`
  → `{"issues": [<issue>…], "next"}`. `state` is `open` (the default),
  `closed` or `all`; `labels` names labels, parted by commas, all of which
  an issue has; `assignee` is a login, `none` or `*`; `author` and
  `mentioned` are logins; `sort` is `created` (the default), `updated` or
  `comments`, the newest or the most first unless `direction=asc`. `q`
  keeps the issues whose title holds it, without regard to case, on a
  public repository; on a private one it is 403 `forbidden`, "text search is
  off for private repositories": a private repository's text is never
  searched. Pages as section 1 says; a cursor belongs to its sort and
  direction. `read`.
- `POST /v1/r/{owner}/{name}/issues {"title", "body", "labels",
  "assignees", "agent"}` → 201, the issue, with `Location`. Takes
  `Idempotency-Key` (section 1): a retry gets the first answer again, its
  `Location` too, and one too large to keep whole comes back without its
  `html`, and if need be without its `body`; the issue is never made
  twice. A repository whose admins turned issues off (section 4) takes
  none: 403 `forbidden`. Labels and assignees come from writers only (403);
  a label the repository does not have, or an assignee who may not read the
  repository, is 400 `invalid`; at most 20 labels and 10 assignees, and a
  longer list is 400 `invalid`, as is a list that asks for more than 20
  labels.
- `GET /v1/r/{owner}/{name}/issues/{n}` → the issue. A change request's
  number is 404 here. `read`.
- `PATCH /v1/r/{owner}/{name}/issues/{n} {"title", "body", "state",
  "state_reason", "labels", "assignees", "locked"}` → the issue, with the
  fields given changed. Its author may change its title, its text and its
  state; writers anything (403 otherwise). `labels` and `assignees` are the
  whole lists. `state` is `open` or `closed`; `state_reason`, `completed`
  (the default) or `not_planned`, goes with closing or with a closed issue
  (400 otherwise). Each change is an entry of the timeline; a closed
  issue given another reason has a `closed` entry with it.

**Comments** are `{"id", "author", "agent", "body", "created", "edited",
"hidden"}`; ids are the repository's own, so that a comment of another
repository is 404 here.

- `GET /v1/r/{owner}/{name}/issues/{n}/comments?limit&after` →
  `{"comments": [<comment>…], "next"}`, oldest first. `read`.
- `POST /v1/r/{owner}/{name}/issues/{n}/comments {"body", "agent"}` → 201,
  the comment, with its `html` and `Location`. Takes `Idempotency-Key`, as
  opening an issue does. A locked issue takes comments from writers only
  (403). A thread takes at most 2,500 comments: the one that reaches it
  locks it, and past it nobody comments (403), so that a new issue goes on.
- `GET /v1/r/{owner}/{name}/comments/{id}` → the comment, with its `html`.
- `PATCH /v1/r/{owner}/{name}/comments/{id} {"body"}` → the comment, with
  `edited`: its author only.
- `DELETE /v1/r/{owner}/{name}/comments/{id}` → 204: its author, or a
  writer.
- `POST /v1/r/{owner}/{name}/comments/{id}/hide {"reason"}` → the comment:
  writers only. `reason` is `spam`, `off_topic`, `outdated` or `resolved`,
  or `""` to show it again. A hidden comment shows to those who do not
  write to the repository with its `hidden` reason and an empty `body`;
  writers read it still. The comments of a suspended account are left out
  for those who do not write to the repository, and show to writers as
  `hidden: "suspended"`.

**The timeline.** `GET
/v1/r/{owner}/{name}/issues/{n}/timeline?limit&after` → `{"items":
[<item>…], "next"}`, oldest first: the comments, and what happened, each
`{"kind", "actor", "created"}` and what the kind says: `comment` with
`comment`; `renamed` with `from` and `to`; `edited`; `closed` with
`reason`, and `changeset` when a landing closed it; `reopened`; `labeled`
and `unlabeled` with `label`; `assigned` and `unassigned` with `assignee`;
`locked`, with `reason: "full"` and an empty `actor` when the hub locked a
thread that holds the most comments one may; `unlocked`; `referenced` with
`source: {"number"}`, a thread of the same repository that named this
one; `cross_referenced` with `source: {"repo": {"owner", "name"},
"number"}`, a thread of another repository that did, shown only to
callers who may read that repository. `read`.

**References.** A text names threads with `#4` and `owner/name#4`,
accounts with `@login`, and changesets with 12 to 64 hex digits of their id
(section 1). When it is written:

- `#4` gives thread 4 of the same repository a `referenced` entry, once per
  thread that names it.
- `owner/name#4` gives thread 4 of that repository a `cross_referenced`
  entry, when the writer may read the repository and the thread exists.
- `@login` tells the account (section 21), when it may read the repository
  and did not block the writer: at most 10 mentions of one text count. A
  mention of anyone else does nothing, and says nothing.

Where a text shows, a reference links to what it names, for each caller:
to a thread that exists, of the repository or of another one the caller
may read; to an account; to a changeset the repository admitted. Anything
else, a thread of a repository the caller may not read among them, is
plain text, exactly as a reference to a repository that does not exist.

**Labels** are `{"name", "color", "description"}`: names of 1 to 50
characters, without commas, compared without regard to case; colours `0`
to `11`, of the hub's palette; descriptions at most 200 characters. A
repository starts with `bug`, `enhancement`, `documentation`, `question`,
`good first issue` and `help wanted`, and has at most 100.

- `GET /v1/r/{owner}/{name}/labels` → `{"labels": [<label>…]}`, by name.
  `read`.
- `PUT /v1/r/{owner}/{name}/labels/{label} {"color", "description"}` → the
  label, made, or changed when the repository has one of that name; what
  is left out is kept, or `0` and `""` for a new one. Writers only. One
  past the most is 400 `invalid`.
- `DELETE /v1/r/{owner}/{name}/labels/{label}` → 204, and the label leaves
  every issue it was on: writers only; 404 for a label the repository does
  not have.

**One's issues.** `GET /v1/me/issues?filter&state&limit&after` →
`{"issues": [<issue with "repo": {"owner", "name"}>…], "next"}`: the issues
the token's account is assigned to (`filter=assigned`, the default), opened
(`created`) or is mentioned in (`mentioned`), latest first, in `state` as
above (`open` by default), of the repositories the token may read now. One
the account no longer reads, a collaborator removed or a repository made
private, is left out while that lasts. Needs a token.

## 19. Change requests

A change request proposes the work of a branch, its head, for a branch of
the repository, its base. The head is a branch of the same repository, or
of another repository of its fork network (section 20): one forked,
directly or not, from the same repository. A request is pinned: its
snapshot is the changeset its head held when it was opened or last
updated, and each update makes a new revision, numbered from 1. What a
revision proposes is worked out when it is made: its stack, the snapshot's
first-parent chain down to an ancestor of the base's head, oldest first;
what the merges of the stack bring in besides; and the merge base. Landing
names the snapshot it lands, and approvals count for the revision they
approve: what was reviewed is what lands. Change requests and issues share
one numbering in their repository (section 18).

Every route finds the repository before it looks at anything else of the
request: one the caller may not read answers 404 `not_found`, as a missing
one does, whatever the method, the number or the body; so does a head
repository the caller may not read, when a request names one. Reading
needs `read`. Writes interact with the repository (section 5): an account
that may read it, is not suspended and not blocked there, through a web
session or a token whose scopes reach it with `interact`, `write` or
`admin` (401 for an anonymous caller, 403 otherwise); the body is read only
for one that may. Texts are as in section 18: titles of 1 to 256
characters on one line, texts of at most 64 KiB, and an `agent` label
shown as "via". References in texts work as in section 18.

**What a request from another repository brings** is held apart from the
target until it lands: the target's `o/{id}`, `lacks` and `missing`
(section 6) never serve it, and the request's own routes do, so that
whoever reads the target reads and fetches the request without reading
its head. It counts toward the quota of the request's author (section 12)
until it lands; landing copies it into the target, where it counts for the
target's owner, and only a landing that lands it does: one that conflicts,
that `ff-only` refuses, or whose stack is not built on the base leaves it
apart. It is let go when the request lands, when it is deleted, and 30
days after it closed: a closed request may be reopened until then, and
after that its `head.available` is false and a new request goes on. A
request from the same repository brings nothing of its own.

**A private head goes to another repository only when its admin says
so.** A request whose head is in a private repository other than the
target publishes what it brings to everyone who reads the target. It is
opened by an admin of the head, with `"expose": true`; otherwise it is 400
`invalid`, and the detail says to whom it would publish. Updating such a
request needs an admin of the head too. A request opened while its head
was public is not updated once the head is private (400 `invalid`): nobody
said to publish what the head holds since, and a new request, opened with
`expose`, goes on. The name of a request's head repository shows only to
those who may read it: to others, `head.repo` is `null`.

**A request** is `{"number", "title", "body", "state":
"open"|"closed"|"landed", "draft", "author", "agent", "head", "base",
"revision", "stack", "brings", "mergeable", "approvals",
"review_requests", "last_attempt", "landed", "closes", "comments",
"locked", "created", "updated", "closed", "closed_by"}`:

- `head` is `{"repo": {"owner", "name"}|null, "ref", "snapshot",
  "available"}`: `available` says whether the hub still holds what the
  snapshot brings, to show, fetch and land.
- `base` is `{"ref", "head_at_snapshot", "merge_base"}`: the base's head
  when the current revision was made, and the merge base then.
- `stack` and `brings` list the changesets as `GET …/log` does (section
  15), each with its author, its time and the first line of its message;
  the routes that answer one request give them, a list leaves them empty.
- `mergeable` is `{"state", "against", "conflicts"}`: whether the request
  lands on its base now, worked out against the base's head `against`.
  `state` is `clean`, `conflicts` (with the conflicts of the first
  changeset that does not land, as `braid land --json` prints them),
  `needs_update` (the base moved to a history the stack is not built on:
  its author updates it), or `unknown` while it is being worked out, which
  happens in the background once a request is read; `POST …/check` works
  it out at once.
- `approvals` is `{"count", "required", "by"}`: the writers of the
  repository other than its author whose last review of the current
  revision approves it, and how many the repository needs (section 4).
- `last_attempt`, while it is open, is the last landing that did not land
  it: `{"outcome": "conflicts"|"refused", "by", "agent", "at", "head",
  "conflicts"}`. `landed`, once it landed, is `{"head", "operation", "by",
  "agent", "at"}`.
- `closes` lists the issues of the repository its text closes (`closes
  #4`, `fixes #4`, `resolves #4`, as section 8 reads messages): those that
  landing it on the default branch closes, and once it landed, those it
  closed.
- The routes that answer one request add `html`, its text as the caller's
  pages show it.

**Opening a request.**

- `POST /v1/r/{owner}/{name}/requests {"title", "body", "head": {"repo",
  "ref"}, "base", "expose", "draft", "reviewers", "agent"}` → 201, the
  request, with `Location`. `head.repo` is `owner/name`, the target itself
  when left out; `base` is the target's default branch when left out.
  Takes `Idempotency-Key` (section 1).
- 403 `forbidden`: the repository takes no requests from the caller (its
  `requests_from`, section 4: `collaborators` takes them from its owner and
  collaborators only, `off` from nobody); the caller may not write to the
  head; reviewers named by a caller who does not write to the target.
- 400 `invalid`: a head or a base that is no branch (a tag, one of the
  hub's own refs, one that does not exist); a head repository outside the
  fork network; a branch proposed for itself; a head that shares no history
  with the base, or brings nothing to it; a private head without the
  exposure of its admin; a reviewer who may not read the target, or is the
  author.
- 409 `exists`: an open request proposes the same head for the same base.
- An account that does not write to the repository has at most 100 open
  threads there, issues and requests together (403 `forbidden`), as in
  section 18.
- 413 `too_large`: the request would hold more than 1,000 changesets, its
  stack and what it brings together, or bring more than 256 MiB from
  another repository (section 12). 403 `quota`: what it brings would take
  its author past what it may store.
- A draft (`"draft": true`) does not land until it is marked ready.

**Reading.**

- `GET /v1/r/{owner}/{name}/requests?state&author&base&reviewer&limit&after`
  → `{"requests": [<request>…], "next"}`, newest first. `state` is `open`
  (the default), `closed` (closed or landed), `landed` or `all`; `author`
  and `reviewer` (asked to review) are logins; `base` is a branch. Pages as
  section 1 says. `read`.
- `GET /v1/r/{owner}/{name}/requests/{n}` → the request. An issue's number
  is 404 here. `read`.
- `GET /v1/r/{owner}/{name}/requests/{n}/diff?rev&page&path&context` → a
  page of the diff of revision `rev` (the current one by default), from its
  merge base to its snapshot, or of what changed from revision `A` to
  revision `B` (`rev=A..B`), as `GET …/changesets/{rev}/diff` gives one
  (section 15), JSON or, with `Accept: text/x-diff`, all of it as text.
  `read`.
- `GET /v1/r/{owner}/{name}/requests/{n}/changesets/{id}` → a changeset of
  the request, as `GET …/changesets/{rev}` gives one: only those of the
  stacks of its revisions and of what they bring (404 otherwise). `read`.
- `POST /v1/r/{owner}/{name}/requests/{n}/missing {"want", "have",
  "history", "paths", "limit", "after"}` → what a client needs to hold the
  request's changesets, as section 6 answers, as JSON pages or an object
  stream: from the repository and from what the request brings. `want`
  names the current snapshot or changesets of its stack (400 otherwise).
  This is how a reviewer who cannot read the head fetches the request
  (`braid hub request checkout`). `read`.

**Updating.** `POST /v1/r/{owner}/{name}/requests/{n}/update {"head"}` →
the request at its next revision: its author only (403), with write on the
head (403), while it is open (400). The snapshot is what the head's branch
holds now, or the changeset `head` names, which the branch must hold on
its first-parent chain: otherwise 409 `not_on_branch`, with `head`, where
the branch points. A snapshot that would not change is 400 `invalid`, and a
branch gone from its repository is 409 `not_on_branch`. A head repository
that is gone, or that the author may no longer read, is 400 `invalid`, the
same for both. The new revision is worked out against the base's head now,
within the sizes of a new request (413), and what it brings from another
repository counts for its author (403 `quota`). A request has at most as
many revisions as a thread has comments, 2,500 (403 `forbidden` past
them). Reviews of earlier revisions no longer count, and say `stale`.

**Changing a request.** `PATCH /v1/r/{owner}/{name}/requests/{n}
{"title", "body", "state", "draft"}` → the request: its author or the
repository's writers (403 otherwise). `state` is `open` or `closed`; a
landed request stays landed (400). Reopening needs what it brings to be
held still (400) and no other open request of the same head and base
(409). `"draft": false` marks it ready for review.

`DELETE /v1/r/{owner}/{name}/requests/{n}` → 204: an admin of the
repository, or its author while it is open (403 otherwise). Its texts
leave the hub, and what it brought is let go.

**Comments** are those of section 18, with what they are about on a diff:
`{…, "path", "line", "side", "revision", "outdated", "review"}`.

- `GET /v1/r/{owner}/{name}/requests/{n}/comments?limit&after` →
  `{"comments": [<comment>…], "next"}`: its conversation, oldest first,
  line comments included, those of reviews left out. `read`.
- `POST /v1/r/{owner}/{name}/requests/{n}/comments {"body", "path",
  "line", "side", "revision", "agent"}` → 201, the comment, with its
  `html` and `Location` (`…/comments/{id}`, which section 18 edits,
  deletes and hides). With `path` and `line`, both, it is a line comment:
  `side` is `old` or `new` (the default), `revision` the revision whose
  diff it is about (the current one by default), and the line must show in
  that diff, with the context a diff has (400 `invalid` otherwise). A line
  comment about a revision before the current one says `outdated`. Takes
  `Idempotency-Key`. Locked requests and full threads as in section 18.
- `GET /v1/r/{owner}/{name}/requests/{n}/timeline?limit&after` → its
  timeline, as section 18 gives one, with these entries besides: `updated`
  with `changeset`, the new snapshot; `reviewed` with `reason`, the
  verdict; `review_requested` and `review_request_removed` with `assignee`,
  the account asked; `ready_for_review`; `converted_to_draft`; `landed`
  with `changeset`, the base's head once it landed. An issue a landing
  closed has a `closed` entry with `changeset`, and `source`, the request.

**Reviews** are `{"id", "author", "agent", "verdict", "binding", "stale",
"revision", "body", "created", "comments"}`.

- `GET /v1/r/{owner}/{name}/requests/{n}/reviews?limit&after` →
  `{"reviews": [<review>…], "next"}`, oldest first. `read`.
- `POST /v1/r/{owner}/{name}/requests/{n}/reviews {"verdict", "body",
  "comments", "revision", "agent"}` → 201, the review. `verdict` is
  `approve`, `request_changes` or `comment`; the last two say what, with a
  text or line comments (400 otherwise). `comments` are `{"path", "line",
  "side", "body"}`, at most 100, each on a line the current revision's
  diff shows (400 otherwise). `revision`, when given, must be the current
  one: otherwise 409 `moved`, since what was read is what is approved. A
  review's verdict counts (`binding`) when it approves or requests changes
  and comes from a writer of the repository other than the request's
  author; an approval counts until the same writer requests changes, or
  the request moves to a new revision. An open request only (400). Takes
  `Idempotency-Key`.
- `GET /v1/r/{owner}/{name}/requests/{n}/reviewers` → `{"logins"}`: the
  accounts asked to review it. `read`.
- `PUT /v1/r/{owner}/{name}/requests/{n}/reviewers {"logins"}` → the
  accounts asked, all of them: writers only (403), each an account that may
  read the repository and is not the author (400), at most 10. Those asked
  are told (section 21).

**Landing.**

- `POST /v1/r/{owner}/{name}/requests/{n}/check` → the request, with
  `mergeable` worked out now. `read`.
- `POST /v1/r/{owner}/{name}/requests/{n}/land {"snapshot", "mode",
  "agent"}` → `{"request", "land"}`: `write` (403; 404 for a repository
  the caller may not read), and a `land:<glob>` right whose glob matches
  the base (403). The request must be open and not a draft (400), have the
  approvals the repository needs at its current revision (403), and
  `snapshot` must be its current one: otherwise 409 `moved`, and nothing
  lands. `mode` is `rebase` (the default) or `ff-only`, as in section 8.
  The stack lands as `POST …/land` lands one, with `<login>/request-<n>` as
  the op log's agent, and `land` is what that route answers. A base whose
  history no longer holds the stack's base is 409 `not_on_branch`. A
  landing that conflicts, or that `ff-only` refuses, is an answer too
  (200): the request stays open, with its `last_attempt`. Once it landed,
  the request is `landed`, and on the default branch the issues its text
  closes are closed, as completed, naming the changeset that closed them.

## 20. Forks and jobs

A fork is a repository of the caller's account made from another one, its
upstream. It holds what the upstream's branches, tags and other git refs
reach when the fork is made, and nothing else of it: neither what was
uploaded there and never landed, nor what only a deleted ref reached. It
holds the git map too, so that history imported from git exports to the
same commits in the fork as in its upstream (section 9), and the
changesets the upstream admitted are admitted in the fork: a landing on
them uploads only what it adds. Its collaborators, settings, mirror,
issues, change requests and stars start empty; its description and topics
are the upstream's. `fork_of` (section 4) names the upstream, for whoever
may read it.

A fork is made by a job, which runs in the background: the answer comes at
once, and the fork appears whole once the job is done. Until then it does
not exist (404), and a job that fails leaves nothing of it.

**Forking.**

- `POST /v1/r/{owner}/{name}/forks {"name", "private"}` → 202 `{"job":
  <job>, "repo": {"owner", "name"}}`, with `Location: /v1/jobs/{id}`. Both
  fields may be left out: `name` is the upstream's name, as it was given,
  and `private` its visibility. A fork of a private repository is private:
  `"private": false` is 400 `invalid`.
- Forking interacts with the repository (section 5): one the caller may not
  read answers 404 `not_found`, as a missing one does, before the body is
  looked at; an anonymous caller is 401, and one that reads but may not
  interact, or is blocked there, 403. A token also needs `write` on
  `<login>/<name>`, the fork's place (403).
- A repository takes forks when its `allow_forks` says so (section 4): yes
  by default for a public repository, no for a private one, which its
  admins may turn on. Otherwise the answer is 403 `forbidden`. A repository
  still being imported is 409 `exists`. One taken down (section 22) is not
  forked, by its owner either: 403 `forbidden`.
- 409 `exists`: the name is taken, by a repository of the account or by a
  job of it that is not done yet; names are compared without regard to
  case. 400 `invalid`: a name that is not one (section 1).
- 403 `quota`: the account owns as many repositories as it may, counting
  those its jobs are making; or what the upstream stores, added to what the
  account stores and what its jobs not done yet will store, passes what it
  may store (section 12).
- Takes `Idempotency-Key` (section 1), which gives the first answer again.
- Each fork counts toward the account's forks (section 12).

When the job runs, the account must still read and interact with the
repository, which must still take forks and not be taken down; otherwise
the job fails, saying that the repository is gone or no longer readable,
whichever it is. A repository that turned private since is forked private,
as any fork of a private repository is. A repository of more than
1,000,000 objects fails the job: too large to fork here.

A fork of a private repository, or of a fork of one, at any depth, is read
only by those who read every repository it was forked from (section 5), and
stays private: `PATCH …` with `"private": false` is 400 `invalid`. Its own
collaborator entries are not enough. Once its owner no longer reads its
upstream, or the upstream is deleted, the hub deletes the fork within the
hour, as its owner would (section 4), and tells its owner (`fork_retired`,
section 21). A read that GitHub gives (section 5) counts until GitHub says
otherwise: while GitHub cannot be asked, the fork stays. A fork of a public
repository is the account's own: it stays when its upstream turns private
or goes.

**The forks of a repository.**

- `GET /v1/r/{owner}/{name}/forks?limit&after` → `{"repos": [<card>…],
  "next"}`: its forks the caller may read, the public ones and the private
  ones it reads, newest first, a page at a time (the cards of section 16).
  `read`.
- `counts.forks` (section 4) counts its public forks; a private fork
  counts for nobody.

**Syncing a fork with its upstream.**

- `POST /v1/r/{owner}/{name}/sync-upstream {"ref"}` → `{"ref", "outcome",
  "head_before", "head", "behind", "more"}`: brings what the upstream's
  ref holds into the fork's ref of the same name. `ref` is a branch or a
  tag, the fork's default branch when left out; one of the hub's own refs
  (`braid/`, `remote/`) is 400. It needs `write` on the fork and, for a
  token, a `land:<glob>` right that matches the ref (403).
- A repository that is not a fork is 400 `invalid`. An upstream that is
  gone, or that the caller may not read, is 404 `not_found`, as a missing
  repository; so is a ref the upstream does not have.
- A sync never shows the upstream's work to others than those who may read
  it. An upstream that turned private after the fork was made does not
  sync into a fork that others may read: a public fork, or a private one
  made while the upstream was public. The answer is 400 `invalid`, and
  nothing is copied. A fork of a private repository syncs, since only the
  upstream's readers read it (section 5). An upstream taken down does not
  sync: 403 `forbidden`.
- `outcome` is:
  - `fast-forward`: the fork's head was an ancestor of the upstream's, or
    the fork lacked the ref. The hub copied what the fork lacked, and the
    ref now points to the upstream's head, `head`.
  - `up-to-date`: the fork has the upstream's head already, as its head or
    an ancestor of it. Nothing moved.
  - `diverged`: each has changesets the other has not. Nothing moved; the
    fork takes the upstream's work by merging it and landing.
- `head_before` is the fork's head before (`null` when it lacked the ref)
  and `head` after. `behind` counts the changesets of the upstream's ref
  the fork still lacks, up to 1,000, with `more: true` when there are more:
  0 but when it diverged.
- The move takes its turn in the ref's queue (section 8), and watchers get
  a `refs` event; a ref that moved while the sync ran is 409 `moved`. What
  the sync copies counts for the fork's owner: 403 `quota` past what it may
  store, and 413 `too_large` past 1,000,000 objects.

**Jobs.** A job is `{"id", "kind", "state", "progress": {"objects",
"bytes"}, "repo", "error", "created", "started", "finished"}`: `id` is `j_`
and 26 characters of `a-z 2-7`; `kind` is `fork` or `import` (section 23);
`state` is `queued`, `running`, `waiting` (for something outside the hub,
such as GitHub's rate limit), `done`, `failed`, `cancelled` or `skipped`;
`repo` is the repository it makes, `owner/name`; `error` says why it failed
or was skipped; times are RFC 3339, `started` the last time it started.

- `GET /v1/jobs?limit&after` → `{"jobs": [<job>…], "next"}`: the account's
  jobs, newest first. A token sees those of the repositories its scopes
  reach. Needs a token.
- `GET /v1/jobs/{id}` → the job. Another account's job, one the token's
  scopes do not reach, and one that does not exist answer the same 404.
- `POST /v1/jobs/{id}/cancel` → the job, cancelled: only a job that has not
  started may be (409 `exists` for one that runs or ended); cancelling one
  cancelled answers it again. A token needs `write` on the repository the
  job makes (403).

An account runs one job at a time, oldest first; the hub runs a few at
once, taking turns between accounts. A job the hub stops in the middle of
runs again when it starts, from the beginning, at most three times.

## 21. The inbox

The hub tells an account, in its inbox, what happens in the threads it
takes part in and in the repositories it watches, and gives it notices
about its own requests, such as a fork made or a grant to renew. The inbox
is where they are: the hub sends no e-mail, and tells no other service.

**Who is told** of what happens in a thread (an issue or a change request
opened, a comment, a state changed, an assignment, a review, a landing):

- the accounts that take part in it: its author, those who commented,
  those it is assigned to, those it mentions, those asked to review it and
  those who reviewed it;
- the accounts that watch all of its repository (section 17), its owner
  among them unless it chose otherwise;
- the accounts that chose to follow the thread (below).

Never the account that did it, one that ignores the repository, one that
muted the thread, one that blocked the account that did it, nor one that
may not read the repository. Nor, while the block lasts, one that the
repository's owner or the repository blocked (section 22): it may not mute
the thread, nor watch the repository less. A mention, an assignment and a
review asked for are told only to the accounts the thread mentions, is
assigned to, or asks to review. Who may read is decided when the
notification is made, and again each time it is shown.

An account has one notification per thread: what happens next changes it,
and makes it unread again. Its `reason` says why the account is told, the
strongest of `mention`, `review_requested`, `assigned`, `review` (it
reviewed the change request), `author`, `comment` (it commented), `state`
(the thread was closed or opened again), `landed` and `subscribed` (it
watches the repository, or follows the thread). An unread notification
keeps the strongest reason it had; once read, the next one says why it
came. A notice of the hub has the reason `system`. What one account does
makes at most 10 notifications an hour for another: past that, that hour
tells that account nothing more of it. A notification counts when it
comes into the inbox, new or unread again: once, however many things one
change tells of (a comment and the mentions in it, a thread opened with
its assignees), and not again for what follows in its thread while it
waits unread.

A notification is `{"id", "repo": {"owner", "name"}|null, "thread":
{"kind": "issue"|"request", "number", "title"}|null, "reason", "actor",
"code", "updated", "unread", "url"}`. `thread` is `null` for a notice of
the hub, and `repo` for a notice about no repository. `actor` is who did
what happened last, `null` for the hub. `code` is what happened last:
`opened`, `commented`, `closed`, `reopened`, `assigned`, `mentioned`,
`review_requested`, `reviewed` or `landed`; for a notice, what it says:
`fork_done`, `fork_failed` and `fork_retired` (section 20), `import_done`
and `import_failed` (section 23), `needs_grant`, `mirror_conflicts`,
`mirror_diverged`, `mirror_rewritten` or `mirror_gone` (section 11). `url`
is the page of the hub it leads to. A thread's title is read when the inbox
is listed, as it is then.

A notification of a repository the account may no longer read (it was
removed as a collaborator, or the repository was made private), or that
was deleted, or of a thread that is gone, leaves the inbox: it is in no
list and no count, and is deleted. For a repository mirrored on GitHub
whose reader only GitHub can vouch for, the hub shows the notification
only while it holds GitHub's answer (section 5), and keeps it meanwhile.

**Tokens.** The inbox is the token's account's. A token sees the
notifications of the repositories its scopes read (section 2), and the
notices of no repository with a scope on `*`; it marks them read or done
where its scopes also have `interact`, `write` or `admin`, and marking at
all needs such a scope (403 `forbidden` otherwise). Marking, read or done,
counts toward the account's limits (section 12).

- `GET /v1/notifications?all&repo&limit&after` → `{"notifications":
  [<notification>…], "unread", "next"}`: the unread notifications, newest
  first; with `all=true`, the read ones too; with `repo=owner/name`, those
  of that repository only, which is 404 `not_found` when the token may not
  read it, as for one that does not exist. `unread` counts the unread
  notifications the token sees, of `repo` when it is given. Pages as
  section 1 says; `all` is `true` or `false` (400 `invalid` otherwise).
  Needs a token.
- `POST /v1/notifications/read` → `{"marked"}`, with `{"ids": [<id>…]}`
  (at most 100), or any of `{"all": true}`, `{"repo": "owner/name"}` and
  `{"before": <time>}`: marks read the notifications of those ids, or every
  one the token may mark, of `repo` when it is given, that last changed at
  or before `before` when it is given (what a client showed, and nothing
  that came since). `marked` is how many were unread. An id the token does
  not see is passed over. A body that names nothing, or names ids and
  anything else, is 400 `invalid`; a `repo` the token may not read is 404.
- `DELETE /v1/notifications/{id}` → 204: the notification is done and
  leaves the inbox, until what happens next in its thread brings it back.
  One the token does not see is 404 `not_found`, whoever's it is, with the
  same words.

**Following a thread.**

- `GET /v1/r/{owner}/{name}/threads/{n}/subscription` → `{"subscribed",
  "reason"}`: whether thread `n` notifies the caller, and why: `manual` (it
  chose to follow it), `muted` (it chose not to), `participating` (it takes
  part in it), `watching` (it watches all of the repository) or `ignoring`
  (it ignores the repository, which silences every thread of it).
  `reason` is left out when nothing ties the caller to the thread.
- `PUT /v1/r/{owner}/{name}/threads/{n}/subscription {"subscribed":
  true|false}` → the same, as it is now. `false` mutes the thread, whatever
  ties the caller to it, and marks its notification read; `true` follows
  it. A change counts as a watch (section 12).

Both interact with the repository (section 5): a repository the caller may
not read is 404 `not_found`, as a missing one, whatever the method or the
body; an anonymous caller is 401, and a token without `interact`, `write`
or `admin` there 403. A thread the repository does not have is 404.

## 22. Blocks and moderation

An account blocks another from every repository it owns; an admin of a
repository blocks an account from that repository alone. Where it is
blocked, an account cannot star, watch, comment, open issues or change
requests, review or fork: each answers 403 `forbidden`, "you cannot
interact with this repository", the words a suspended account gets too,
which name no block. It cannot follow the account that blocked it (403).
Its mentions do not notify the account that blocked it. It still reads
every public repository, and whatever else it may read.

Since a blocked account cannot take a star back or watch less where it is
blocked either, a block ends them: blocking an account ends its follow of
the blocker, takes its stars off the repositories the block covers and
ends its watching of all of each, so that it hears of them no more than of
any other repository it reads. A repository it chose to hear nothing of
keeps that choice. Lifting the block gives none of them back.

The owner of a repository is never blocked from it. An account blocks at
most 1,000 accounts, and a repository at most 1,000: one more is 400
`invalid`, until a block is lifted. Blocking and unblocking count toward
the account's limits (section 12).

**An account's blocks** need a token with `interact`, `write` or `admin` on
`*` (403 without one, 401 without a token).

- `GET /v1/me/blocks?limit&after` → `{"users": [{"login", "name", "since"}],
  "next"}`: the accounts the caller blocked, newest first, with since when
  each is, in pages as section 1 says.
- `GET /v1/me/blocks/{login}` → 204 when the caller blocked the account,
  404 `not_found` when it did not.
- `PUT /v1/me/blocks/{login}` → 204, and `DELETE /v1/me/blocks/{login}` →
  204, whether the caller blocked the account before or not.
- A login that is not one (section 1), or the caller's own, is 400
  `invalid`; an account that does not exist is 404 `not_found`, and so is
  a suspended one for `PUT`.

**A repository's blocks** need `admin` on it. A repository the caller may
not read answers 404 to every method of these routes, as a missing one
does, before anything else is looked at.

- `GET /v1/r/{owner}/{name}/blocks?limit&after` → the same page, of the
  accounts the repository blocked.
- `GET /v1/r/{owner}/{name}/blocks/{login}` → 204 or 404, `PUT
  /v1/r/{owner}/{name}/blocks/{login}` → 204 and `DELETE
  /v1/r/{owner}/{name}/blocks/{login}` → 204, as for an account's own.
  Blocking the owner, or oneself, is 400 `invalid`.

**The operator** may also:

- suspend an account: its tokens answer 401 `unauthorized`, its web
  sessions and its sign-ins are refused, its repositories answer 404 to
  everyone, as missing ones do, and leave every listing; it shows in no
  list of accounts, its profile is 404, and its comments show only to the
  writers of their repositories, with `"hidden": "suspended"` (section 18).
  Lifting the suspension gives all of it back, its tokens and sessions
  included.
- hide a repository: it leaves Explore, search and topics (section 16),
  and stays readable as before.
- take a repository down: it answers 404 to everyone but its owner, as a
  missing one does, leaves every listing, and is not forked.

No route of the API does these.

## 23. Syncing with GitHub

A new account's first sign-in leads to `/-/onboarding` (section 14), where
it may bring the repositories it owns on GitHub to the hub, each kept as a
mirror that follows GitHub (section 11). Only the account's own
repositories are offered, never those of an organisation. The routes of
the listing and of batches act for the caller's own account, as section 2
says: with a token that has `write` (or `admin`) on `<login>/*` or `*`, or
a session that sends its CSRF token in `X-CSRF-Token`; and they reach
GitHub with the access the account granted (section 2).

**The listing.** `GET /v1/github/repos?refresh` → `{"scope", "repos",
"truncated", "quota"}`: the repositories the account owns on GitHub, most
recently pushed first, each once, at most 1,000 (`truncated` is true when
GitHub has more). A repository pushed to while GitHub's pages were read
may be missing until the listing is asked again. `scope` is `private` or
`public`, as the account granted; with `public`, private repositories are
not listed. The hub keeps a listing for ten minutes; `refresh` asks GitHub
again, at most once a minute. Without a grant it is 403 `forbidden`, with
a detail that names the grant's page and `braid hub github grant`; while
GitHub holds the hub back, 503 `unavailable`.

Each repository is `{"github_id", "full_name", "private", "fork",
"archived", "empty", "size_kb", "default_branch", "pushed_at", "admin",
"hub_name", "synced", "eligible", "reason", "estimate_bytes"}`:

- `github_id`, `full_name` (`owner/name` on GitHub), `private`, `fork`,
  `archived`, `size_kb`, `default_branch` and `pushed_at` are GitHub's.
  `empty` is true for a repository that holds nothing yet. `admin` says
  whether the account administers it, as a webhook needs.
- `hub_name` is the name it takes here: its own, with a leading `.` made
  `_` (`.github` becomes `_github`).
- `eligible` says whether it may be imported; `reason` says why not:
  `name not allowed here` (a sync may give it another name), `name taken`
  (a repository of the account, or an import under way, has that name),
  or `already synced`, when a repository here is its mirror already: then
  `synced` names it, `owner/name`, when it is the account's. An archived
  repository is eligible, and `reason` says it is imported without a
  mirror; an empty one too, made empty with its mirror set.
- `estimate_bytes`, about what it takes here: four times its size on
  GitHub.

`quota` is `{"repos_left", "bytes_left"}`: what the account may still own
and store (section 12), less what its imports and forks under way will
make.

**Batches.** `POST /v1/github/sync {"all", "repos", "names",
"keep_public", "reserve_tdm", "follow"}` → 202 `{"batch", "queued",
"skipped"}`, with `Location: /v1/github/sync/{batch}`. Takes
`Idempotency-Key` (section 1).

- `all: true` takes every repository not synced already, most recently
  pushed first; otherwise `repos` names them, each by its `github_id` or
  its `full_name`.
- `names` gives some another name here, by `github_id` (in decimal) or
  `full_name`. A name that cannot be one here skips its repository with
  `name not allowed here`, and the batch shows it in `hub_name` cut to
  its first 100 characters, with `…` where it was cut.
- `keep_public` (default true): public repositories stay public here;
  `false` imports them all as private. Private ones stay private either
  way.
- `reserve_tdm` (default false) reserves text and data mining of what is
  imported (section 24); otherwise each takes the account's default.
- `follow` (default `merge`): how each mirror takes in what is pushed to
  GitHub (section 11): `merge`, `ff-only` or `off`.

Repositories are taken most recently pushed first, as many as the
account's room allows. `queued` lists each one queued, `{"github_id",
"full_name", "hub_name", "job"}`; `skipped` the others, `{"github_id",
"full_name", "reason"}`, where `reason` is `quota` (no room left, in
repositories or bytes), `name taken`, `name not allowed here`, `already
synced`, or `not found` (not a repository the account owns on GitHub, as
its grant shows them; `github_id` is then 0 when a name was given). An
account runs one batch at a time: another while one runs is 409 `exists`.
A body that names nothing and does not ask for all, an unknown `follow`, or
more than 1,000 repositories or names is 400. Batches count against the
account's write limit for syncs (section 12).

- `GET /v1/github/sync?batch` and `GET /v1/github/sync/{batch}` →
  `{"batch", "state", "counts", "repos"}`: a batch of the account, its
  latest without `batch` (404 when it has none). `state` is `running`,
  `done`, or `cancelled` when some of its imports were cancelled. `counts`
  holds how many of its repositories are `queued`, `running`, `waiting`,
  `done`, `failed`, `cancelled` and `skipped`; `repos` lists them in the
  batch's order, `{"github_id", "full_name", "hub_name", "state", "error",
  "job"}`, with why each failed or was skipped in `error`.
- `DELETE /v1/github/sync/{batch}` cancels the batch's imports that have
  not started; those under way go on. It answers the batch.
- `POST /v1/github/sync/{batch}/retry {"github_id", "name"}` → 202, as
  `POST /v1/github/sync` answers: imports again a repository of the batch
  that failed, was cancelled or was skipped, under `name` if given, else
  the name it had. The batch runs again until the import ends. A
  repository that is queued, running or done is 409; a name that cannot be
  one here is 400, and one taken 409; no room is 403 `quota`; another
  batch running is 409. Retries count against the account's write limit
  for edits.

A batch of another account answers every route as one that does not
exist: 404 `not_found`.

Each import is a job of the account (section 20), of kind `import`, which
`GET /v1/jobs` lists with the repository it makes. It ends `done`, or
`failed` with `error`; while GitHub holds the hub back it is `waiting`.

**What an import does.**

- It looks the repository up on GitHub again by `github_id`, so that one
  renamed since the listing is still found. One that is gone, no longer
  the account's, or no longer shown to its grant fails.
- It makes the repository here with the history of the default branch,
  from `https://github.com/<owner>/<name>.git` as GitHub names the
  repository now. Until the import is done, the repository answers as a
  missing one to everyone but its owner, and no list shows it: it appears
  whole or not at all.
- The repository is private or public as above; it takes GitHub's
  description, and those of its topics that are topics here.
- Its mirror is the GitHub repository, follows it as `follow` says, knows
  its `github_id`, and starts with `synced` at the commit imported, so that
  neither side has anything to send the other (section 11). It gets a
  webhook when the account administers the repository and the mirror
  follows; otherwise the hub polls it.
- An archived repository is imported without a mirror. An empty one is made
  empty, with its mirror set, and takes its history from the first push to
  GitHub.
- When GitHub's rate limit for the account's grant runs low, the import
  waits until it resets; when GitHub's secondary limits hold the hub back,
  it waits as long as GitHub says. A failure to reach GitHub, or of git, is
  tried twice more before the import fails.
- A hub that stops in the middle of a batch goes on with it when it starts
  again: an import cut short leaves nothing, and starts again, at most
  three times in all.

The account hears of it in its inbox (section 21): `import_done` once a
batch that imported something ends, and `import_failed` for each import
that fails.

**Syncing a mirror now.** `POST /v1/r/{owner}/{name}/mirror/sync`
(`admin`, no body): looks at the mirror now and does what its policy says
(section 11): follows GitHub, merges both, or publishes. A paused mirror
(`rewritten`) is looked at again, and goes on when GitHub's branch holds
`synced` again. At most once a minute per repository: sooner is 429
`rate_limited`, with `retry_after`. Answers the repository (section 4),
with the outcome in its `mirror`. A repository without a mirror is 400
`invalid`.

**Webhooks.** `POST /v1/hooks/github/{key}`: where GitHub delivers the
webhooks the hub made, each to an address of its own; nobody else has a
use for it. A key the hub does not know is 404, answered before the body
is read. The body, at most 5 MiB (413 `too_large` beyond), must carry the
webhook's `X-Hub-Signature-256` (401 `unauthorized` otherwise). A verified
delivery is answered 202 at once, with no body: `ping` and anything but
`push` do nothing; a delivery whose `X-GitHub-Delivery` came in the last
day does nothing again; a push to another branch, of another repository,
or of the commit both sides hold already does nothing; any other push to
the default branch makes the hub look at the mirror a couple of seconds
later, at most once every ten seconds. At most 60 deliveries a minute per
webhook, and 600 verified ones for the whole hub: 429 `rate_limited`
beyond. A key the hub does not know, and a delivery its webhook's secret
did not sign, spend nothing of the hub's 600. Nothing of a delivery is
kept but its id, for a day.

## 24. Terms, and text and data mining

**Terms.** `GET /terms` is a page with the hub's terms: what it commits to
about private code, text and data mining of public repositories, and what
its logs keep, in English and then in Spanish (`#en`, `#es`). Each part is
a section with an id: `#private`, `#tdm` and `#logs`, and `#es-private`,
`#es-tdm` and `#es-logs` in Spanish. `#tdm` is where `tdm-policy` leads.
The terms carry their version and its date at their top (version 1,
pending legal review). Every page's footer links them, and search engines
may index them. `GET /terms.md` gives the same text as Markdown, English
first, `text/markdown; charset=utf-8`. Anyone may read both.

**Text and data mining.** A repository's `tdm` (section 4) is `allowed`, the
default, or `reserved`, as article 4 of Directive (EU) 2019/790 lets the
holder of the rights reserve it. `PATCH /v1/r/{owner}/{name}` with `tdm`
changes it (`admin`). A new repository starts with its owner's
`tdm_default` (section 3), `allowed` unless the account says otherwise.

Every answer about a public repository whose `tdm` is `reserved`, once the
caller may read it, carries:

```text
tdm-reservation: 1
tdm-policy: <hub>/terms#tdm
X-Robots-Tag: noai, noimageai
```

whatever the route: its pages, its raw files, the API under
`/v1/r/{owner}/{name}`, its objects and git, and the errors those routes
answer once the caller's permission is checked (a file that is not there,
say). Its pages also carry `<meta name="robots" content="noai,
noimageai">`, `<meta name="tdm-reservation" content="1">` and `<meta
name="tdm-policy" content="<hub>/terms#tdm">`. Every answer about a private
repository, to whoever may read it, carries `X-Robots-Tag: noindex,
nofollow, noai, noimageai`, and its pages `<meta name="robots"
content="noindex, nofollow, noai, noimageai">`, whatever its `tdm`. An
answer about a repository the caller may not read carries none of these,
as one about a repository that does not exist does; nor does any answer
that names no repository.

**`GET /robots.txt`**: `text/plain; charset=utf-8`, `Cache-Control: public,
max-age=60`. A group for every crawler (`User-agent: *`) disallows the API
(`/v1/`), the hub's pages under `/-/` and `/auth/`, raw files
(`/*/*/raw/`), and the pages `/notifications`, `/settings`, `/login`,
`/logout` and `/tokens`, each as the page (`/login$`), with a query
(`/login?`) and below it (`/login/`), never as a bare prefix, which would
also cover the repositories of the logins that start with it. A second
group, for the crawlers that gather training data for AI models (GPTBot,
ClaudeBot, anthropic-ai, CCBot, Google-Extended, Applebot-Extended,
Bytespider, Meta-ExternalAgent, cohere-ai, cohere-training-data-crawler,
Diffbot, Amazonbot, AI2Bot, Timpibot, ImagesiftBot, PanguBot, Omgilibot,
omgili and YouBot), repeats each of those rules, since a crawler follows
only the group that names it, and adds, for each public repository whose
`tdm` is `reserved` once something has landed on it:

```text
Disallow: /{owner}/{name}$
Disallow: /{owner}/{name}?
Disallow: /{owner}/{name}/
Disallow: /v1/r/{owner}/{name}/
```

with the name as the repository was made and again lowercase when that
differs, since the hub answers both. An owner every one of whose
repositories that anyone may read is reserved gets `Disallow: /{owner}/`
and `Disallow: /v1/r/{owner}/` instead: what the owner says of a
repository that answers as a missing one never changes the file. Crawlers
that fetch a page because a person asked them to are not named.
`robots.txt` never names a private repository. A repository that turns
private, or whose reservation is taken back, leaves it at once, and one
reserved or listed since comes in at once; anything else it follows
within a minute. It holds at most 400 KiB: the rules of every crawler are
always there whole, and when the reserved repositories do not all fit,
those of owners not yet established (section 16) go first, then those
with the fewest stars; a comment says so, and their answers keep their
headers.

**`GET /.well-known/tdmrep.json`**: `application/json`, `Cache-Control:
public, max-age=60`, the file of the TDM Reservation Protocol: an array of
rules for the repositories and owners `robots.txt` names, a repository's
page, with a query and what is below it, in each spelling,

```json
[
  {"location": "/{owner}/{name}$", "tdm-reservation": 1, "tdm-policy": "<hub>/terms#tdm"},
  {"location": "/{owner}/{name}?", "tdm-reservation": 1, "tdm-policy": "<hub>/terms#tdm"},
  {"location": "/{owner}/{name}/*", "tdm-reservation": 1, "tdm-policy": "<hub>/terms#tdm"},
  {"location": "/{owner}/*", "tdm-reservation": 1, "tdm-policy": "<hub>/terms#tdm"}
]
```

(`/{owner}/*` for an owner `robots.txt` names whole), `[]` when nothing is
reserved. The protocol matches a location as the start of a path unless
it ends in `$`, so a page's location ends in `$`, and covers no other
repository whose name starts with that one's. It follows a change as
`robots.txt` does, and holds at most 400 KiB, its rules a part of those of
`robots.txt` when not all fit.
