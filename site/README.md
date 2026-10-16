# braid.elffuss.com

The site of Elffuss Braid: one static page, in English and Spanish, with no
build step and no requests to anything but its own files.

| File | What it is |
|---|---|
| `index.html` | The page. The logo, the diagram and the git loop are inline SVG. |
| `site.css` | Dark by default, as on elffuss.com; light under `prefers-color-scheme: light`. Terminals stay dark in both. |
| `site.js` | The language toggle, the copy buttons, the faded edge of a code block that scrolls, which blocks take keyboard focus, and the markers of the ratio table. Without it the page is complete, in English, every block takes focus, and the toggle and the copy buttons stay hidden. |
| `og.png`, `og/og.svg` | The Open Graph image, 1200 × 630, and its source. |
| `favicon.svg`, `favicon-32.png`, `apple-touch-icon.png` | The icons: the strands around one crossing of the logo. |

To look at it locally:

```bash
python3 -m http.server --directory site 8000
```

## Two languages in one page

Every translated block is in the page twice, once with `data-l="en"
lang="en"` and once with `data-l="es" lang="es"`. `site.css` hides the one
that does not match `html[lang]`. `site.js` sets `html[lang]` before the body
is drawn, from the first of: `?lang=en` or `?lang=es`, the choice stored when
someone pressed EN or ES, a browser language that starts with `es`, English.
Elements with `data-aria-en` and `data-aria-es` get the matching
`aria-label`; the page title and the "Copied" message follow too.

Transcripts, commands and JSON are not translated, and carry `lang="en"`.
In the ratio table, what each row compares is written in both languages;
its commands and its ratio are not translated.

Every code block keeps its lines and scrolls in its own box, and so do the
two story transcripts that are columns (`braid blame` and `braid op log`,
marked `term-cols`). In the HTML, the `<pre>` of each, and the wrapper of
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
  line at 1. Then rewrite the sentences in both languages. Keep the
  `table-wrap` around the table, with its attributes. Outside the block, the
  line about speed under "What is honest about this" names the commit
  measured and what that run showed: change it with the table.

## What the page may say

- Only what is true of the code today.
- Ratios against git appear only between the ratios markers. No absolute
  times, sizes or throughput, no description of any machine, no user names
  and no disk paths.
- The story is braid's own test, `crates/cli/tests/three_agents.rs`, run as
  the "Run it yourself" script does. If the transcripts are made again, leave
  out the op log's times and blame's dates, never paste what `workspace add`
  prints (it names the directory), and change the hero card and the story
  together.
- The quick start and the agent's commands under "Many agents on one
  repository" run as one session, top to bottom. `braid undo` comes after the
  agent lands, so it takes that landing back; before it, it would take the
  import back and leave the agent an empty repository.
- Every link into the repository points at `main` on GitHub. Before the page
  is served, check them against what `main` holds, not against github.com,
  which answers 200 for any path of an empty repository: a file answers 200
  at `https://raw.githubusercontent.com/KikoCis/elffuss-braid/main/<path>`,
  and a directory is listed by
  `gh api 'repos/KikoCis/elffuss-braid/contents/<path>?ref=main'`.

## Images

`og.png` is `og/og.svg` rendered at 1200 × 630 in a headless browser. The
icons are `favicon.svg` rendered at 32 and 180 pixels; the 180-pixel one is
drawn on a square without rounded corners, since iOS rounds it. Before a PNG
is committed, strip every chunk but the image data and look through it with
`strings` for anything that names a person or a machine. Check a PNG by its
chunks (only `IHDR`, `IDAT` and `IEND`), not with a text search: compressed
image data can match any short pattern. A text search of the page skips
binary files and also catches units written after `&nbsp;`:

```bash
grep -rnIE --exclude=README.md "/(Users|home)/|[0-9]+(&nbsp;|\s)?(ms|MB|GB|MiB|GiB)\b" site/
```

The only hits it should find are the secrets guard's `1&nbsp;MiB`, in
English and in Spanish: a threshold of braid's, not a measurement.

Every strand on the page is the logo's own curve, from
`docs/assets/braid.svg`: three strands of `y = 48 + 27 cos(2π (x − c) / 92)`,
a third of a period apart, and at each crossing the strand on top is cut out
of the one beneath. The SVGs are plain markup and can be edited by hand; an
inlined copy of the logo needs its own mask ids.
