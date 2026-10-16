# braid.elffuss.com

The site of Elffuss Braid: one static page, in English and Spanish, with no
build step and no requests to anything but its own files.

| File | What it is |
|---|---|
| `index.html` | The page. The logo, the diagram and the git loop are inline SVG. |
| `site.css` | Dark by default, as on elffuss.com; light under `prefers-color-scheme: light`. Terminals stay dark in both. |
| `site.js` | The language toggle, the copy buttons and the markers of the ratio table. Without it the page is complete, in English. |
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

Transcripts, commands, JSON and the rows of the ratio table are not
translated, and carry `lang="en"`.

New text goes in both languages. The Spanish is written, not translated word
for word: land is *aterrizar*, workspace *espacio de trabajo*, store
*almacén*, op log *registro de operaciones*; changeset, fast-forward and lease
stay in English.

## Slots that will be filled later

- `<!-- autosync:start -->` … `<!-- autosync:end -->` holds a placeholder for
  automatic git sync. Each marker appears once. Whoever fills it keeps the id
  `autosync` and both languages, and says nothing about it anywhere else on
  the page, in the meta tags or in the OG image.
- `<!-- ratios:start -->` … `<!-- ratios:end -->` holds everything that comes
  from one run of `bench/run.py`: the commit measured, the table, the retry
  counts and what they say. To refresh it, put the rows of the new table in
  the `<tbody>`, one `<tr>` per row with the ratio alone in the second cell,
  and rewrite the sentences in both languages. `site.js` draws a marker per
  row from that cell, on a log scale with a line at 1, so nothing else needs
  to change.

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

## Images

`og.png` is `og/og.svg` rendered at 1200 × 630 in a headless browser. The
icons are `favicon.svg` rendered at 32 and 180 pixels; the 180-pixel one is
drawn on a square without rounded corners, since iOS rounds it. Before a PNG
is committed, strip every chunk but the image data and look through it with
`strings` for anything that names a person or a machine.

Every strand on the page is the logo's own curve, from
`docs/assets/braid.svg`: three strands of `y = 48 + 27 cos(2π (x − c) / 92)`,
a third of a period apart, and at each crossing the strand on top is cut out
of the one beneath. The SVGs are plain markup and can be edited by hand; an
inlined copy of the logo needs its own mask ids.
