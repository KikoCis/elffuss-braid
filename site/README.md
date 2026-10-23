# braid.elffuss.com

The site of Elffuss Braid: two static pages, in English and Spanish, with no
build step and no requests to anything but their own files.

| File | What it is |
|---|---|
| `index.html` | The home page. The logo, the diagram and the git loop are inline SVG. |
| `scion.html` | What braid takes from the ideas Anthropic published about Scion, where it differs, the prior art it credits and other open implementations. It has the header, the footer, the CSS and the JavaScript of `index.html`; its links into the home page are `./#…`. |
| `site.css` | Dark by default, as on elffuss.com; light under `prefers-color-scheme: light`. Terminals stay dark in both. |
| `site.js` | The language toggle, the copy buttons, the faded edge of a code block that scrolls, which blocks take keyboard focus, and the markers of the ratio table. Without it the page is complete, in English, every block takes focus, and the toggle and the copy buttons stay hidden. |
| `og.png`, `og/og.svg` | The Open Graph image, 1200 × 630, and its source. |
| `favicon.svg`, `favicon-32.png`, `apple-touch-icon.png`, `favicon.ico` | The icons: the strands around one crossing of the logo. `favicon.ico` is for the clients that ask for it whatever the page declares. |
| `agents.md` | The guide for coding agents: installing braid and setting it up in a project. It is part 1 of the repository's `AGENTS.md`, with a title and an introduction of its own, and the prompt under "Ask your agent" points at it. |
| `llms.txt` | What braid is, in the [llms.txt](https://llmstxt.org) format, with links to `agents.md`, the README and the docs. |
| `robots.txt` | Lets every crawler in and names the sitemap. |
| `sitemap.xml` | Every page, `llms.txt` and `agents.md`. A new page goes in it in the same change. |

The HTML, the CSS, the JavaScript and the SVGs open with
`SPDX-License-Identifier: Apache-2.0` and `Copyright (c) 2026 UtopiaIA` in a
comment, as every source file of the repository does. `agents.md` and
`llms.txt` are documents that agents read whole, like the repository's
Markdown, and carry no such comment: an llms.txt starts with its title.

To look at it locally:

```bash
python3 -m http.server --directory site 8000
```

## Two languages in each page

Every translated block is in the page twice, once with `data-l="en"
lang="en"` and once with `data-l="es" lang="es"`. `site.css` hides the one
that does not match `html[lang]`. `site.js` sets `html[lang]` before the body
is drawn, from the first of: `?lang=en` or `?lang=es`, the choice stored when
someone pressed EN or ES, a browser language that starts with `es`, English.
Elements with `data-aria-en` and `data-aria-es` get the matching
`aria-label`; the page title and the "Copied" message follow too. The
English title is the page's `<title>` and the Spanish one is `data-title-es`
on `<html>`: a title is changed there, and nowhere else.

Transcripts, commands and JSON are not translated, and carry `lang="en"`.
In the ratio table, what each row compares is written in both languages;
its commands and its ratio are not translated.

Every code block keeps its lines and scrolls in its own box, and so does
the story's `braid blame`, whose columns must line up (`term-cols`). The
one exception is the prompt under "Ask your agent" (`.code.prompt`), a
sentence rather than a command, which wraps; it is in the page once per
language, since the Spanish page asks in Spanish. At 480
pixels and below, blame leaves its ids out (`span.bl-id`) and its bar note
says so (`span.bn-ids`); below 360 its type is a little smaller, so every
line fits a phone whole. The story's `braid op log` (`term-oplog`) never
scrolls: an entry that does not fit wraps under itself, four columns in, so
what each landing did stays in view. Each of its lines is a `span.ol`, an
inline block with its newline inside, so the text reads and copies as braid
prints it. The other story transcripts wrap as plain text.

In the HTML, the `<pre>` of every block that may scroll, and the wrapper of
each table, is a labelled region that takes keyboard focus. `site.js` keeps
that only where the box does scroll at the current width, and takes the
`tabindex`, the role and the label away elsewhere, so a keyboard does not
stop on text that does not move. A copy button, where there is one, names
what it copies. Both labels are given in the two languages with
`data-aria-en` and `data-aria-es`.

A flag with its argument, or any name with a hyphen (`Apache-2.0`,
`SHA-256`, `cargo-fuzz`, `(fast-forward)` in a transcript), sits in
`<span class="nw">` (or the whole `<code class="nw">`), so a narrow screen
never breaks a line inside it. A separator goes inside the span of the word
after it, so it never ends a line.

New text goes in both languages. The Spanish is written, not translated word
for word: land is *aterrizar*, workspace *espacio de trabajo*, store
*almacén*, op log *registro de operaciones*; changeset, fast-forward and lease
stay in English.

## Slots that will be filled later

- `<!-- autosync:start -->` … `<!-- autosync:end -->` holds a placeholder for
  automatic git sync. Each marker appears once. Until the feature ships, its
  heading and its text say it is coming, never that it works. Whoever fills
  it keeps the id `autosync` and both languages, and says nothing about it
  anywhere else on the page, in the meta tags or in the OG image. The one
  link to it from outside is "Automatic sync with git" at the end of `#git`,
  which stays true before and after; keep the id.
- `<!-- ratios:start -->` … `<!-- ratios:end -->` holds everything that comes
  from one run of `bench/run.py`: the commit measured, the table, the retry
  counts and what they showed, in the past tense. To refresh it, put the rows
  of the new table in the `<tbody>`, one `<tr>` per row. The first cell says
  what was compared, once in `<span data-l="en" lang="en">` and once in
  `<span data-l="es" lang="es">`; commands stay in `<code>`, and the Spanish
  writes thousands with a no-break space (`10&nbsp;287`) and four digits
  together (`1000`). The second cell holds the ratio alone, as the script
  prints it: `site.js` draws a marker per row from it, on a log scale with a
  line at 1. Then rewrite the sentences in both languages, including the
  list of work that came after the commit measured. Keep the `table-wrap`
  around the table, with its attributes. Nothing outside the block depends
  on the run: the line about speed under "What is honest about this" only
  sends the reader to the table.

## What the pages may say

- Only what is true of the code today.
- Ratios against git appear only between the ratios markers; another page
  sends the reader to `./#measured`. No absolute times, sizes or
  throughput, no description of any machine, no user names and no disk
  paths.
- Wherever Scion is named, the page also says that braid is an independent
  implementation, contains no Anthropic code and is not affiliated with or
  endorsed by Anthropic.
- `scion.html` quotes Anthropic's post word for word, or says plainly what
  the post says, and what it says of another project comes from that
  project's own README or docs. It compares and does not judge: what both
  projects do is said of both.
- The story is braid's own test, `crates/cli/tests/three_agents.rs`, run as
  the "Run it yourself" script does. If the transcripts are made again, leave
  out the op log's times and blame's dates, never paste what `workspace add`
  prints (it names the directory), and change the hero card and the story
  together.
- The prompt under "Ask your agent" is the same, word for word, in the
  English page, in the README, in `llms.txt` and under "Try it" in
  `scion.html`, and so is its Spanish in the Spanish pages and in the
  README's Spanish part. `agents.md` is part 1 of `AGENTS.md`, and its
  install command names the tag of the version in `Cargo.toml`.
  `crates/cli/tests/guides.rs` checks all of this, and that every braid
  command `agents.md` gives exists with its flags; every command it gives
  runs as written.
- The structured data in the head of `index.html` (JSON-LD) names the
  version in `Cargo.toml`, so a release bumps it too, and every page is in
  `sitemap.xml`. `crates/cli/tests/guides.rs` checks both, and that every
  text that names Scion says braid is independent.
- The quick start and the agent's commands under "Many agents on one
  repository" run as one session, top to bottom. `braid undo` comes after the
  agent lands, so it takes that landing back; before it, it would take the
  import back and leave the agent an empty repository.
- Every link into the repository points at `main` on GitHub.

## Before it is served

The page describes what `main` holds on GitHub, so it goes out only once
all of this is true:

1. `main` on GitHub holds this tree, `site/` included, pushed after
   `scripts/privacy-check.sh` passes. The quick start clones and builds it,
   and CI has run on it: the story says CI runs the scene on every push to
   `main`.
2. The tag that `agents.md` installs (`--tag v` and the version in
   `Cargo.toml`) is on GitHub, at a commit of `main` that holds this tree.
   Push the commits and the tag first and serve the page after, never the
   other way round: `agents.md` is served apart from the code, and an
   agent that follows it before the tag is there gets an error from cargo
   instead of braid.
3. Every link into the repository answers from `main`. Check them against
   what `main` holds, not against github.com, which answers 200 for any path
   of an empty repository: a file answers 200 at
   `https://raw.githubusercontent.com/utopia-ia/elffuss-braid/main/<path>`,
   and a directory (`crates/sim`, `fuzz/fuzz_targets`, `bench`) is listed by
   `gh api 'repos/utopia-ia/elffuss-braid/contents/<path>?ref=main'`.
4. The text matches `main`. The ratios block, for one, lists the work on
   speed that came after the commit measured, and that list grows.
5. `https://braid.elffuss.com/` answers 200 with a certificate for that
   name, `http://braid.elffuss.com/` redirects to it, and
   `https://braid.elffuss.com/og.png` answers 200 as `image/png`. Only then
   does the link go anywhere: the repository's homepage
   (`gh repo edit utopia-ia/elffuss-braid --homepage https://braid.elffuss.com`),
   and posts, whose link previews are fetched once and kept.
6. `https://braid.elffuss.com/agents.md` answers 200 as
   `text/markdown; charset=utf-8` and `https://braid.elffuss.com/llms.txt`
   as `text/plain; charset=utf-8`, so that an agent reads them as text:
   `curl -sI` shows the type. A server that does not know `.md` may send
   `application/octet-stream` instead. `https://braid.elffuss.com/robots.txt`
   answers 200 as `text/plain`, `https://braid.elffuss.com/sitemap.xml` as
   `text/xml` or `application/xml`, and every page in the sitemap answers
   200. A new page is then submitted again with the sitemap, in Google
   Search Console and Bing Webmaster Tools.

## Images

`og.png` is `og/og.svg` rendered at 1200 × 630 in a headless browser. The
icons are `favicon.svg` rendered at 32 and 180 pixels; the 180-pixel one is
drawn on a square without rounded corners, since iOS rounds it.
`favicon.ico` holds `favicon-32.png`, byte for byte, as its one image: an
ICO file may hold a PNG. Before a PNG is committed, strip every chunk but
the image data and look through it with `strings` for anything that names a
person or a machine. Check a PNG by its chunks (only `IHDR`, `IDAT` and
`IEND`), not with a text search: compressed image data can match any short
pattern. A text search of the page skips binary files and also catches
units written after `&nbsp;`:

```bash
grep -rnIE --exclude=README.md "/(Users|home)/|[0-9]+(&nbsp;|\s)?(ms|MB|GB|MiB|GiB)\b" site/
```

The only hits it should find are the secrets guard's `1&nbsp;MiB`, in
English and in Spanish, and its `1 MiB` in `agents.md`: a threshold of
braid's, not a measurement.

Every strand on the page is the logo's own curve, from
`docs/assets/braid.svg`: three strands of `y = 48 + 27 cos(2π (x − c) / 92)`,
a third of a period apart, and at each crossing the strand on top is cut out
of the one beneath. The SVGs are plain markup and can be edited by hand; an
inlined copy of the logo needs its own mask ids.
