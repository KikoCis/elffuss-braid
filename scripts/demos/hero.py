#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright (c) 2026 UtopiaIA
"""Builds the README's banner: the logo's strands, the tagline, and a
terminal with the landings of the agents recording, so that the banner and
docs/assets/demo-agents.gif show the same run, ids and all.

usage: hero.py <site/index.html> <agents.cast | transcript.txt> <out dir>

From a recording (asciicast v2, as record.py writes it) it takes the lines
from the note "# landing on main ..." to the end; a .txt file gives those
lines as they are. The lines about the working copy and what to run next
are left out to keep the banner short: the GIF has them all. A recording
whose story changed stops it, rather than giving a banner that no longer
matches.

It writes hero-dark.svg and hero-light.svg, 1280 wide, each a final frame
that CSS animates in once where the reader allows motion. There is no
narrow variant: for a reader who has chosen a theme, GitHub rewrites the
colour-scheme media queries of a README's <picture>, and a source that
also tests the screen's width then shows on wide screens too.

The terminal stays dark in both themes, as on the site. No scripts, no
external fonts or images: GitHub shows these files through <img>.
"""
import html
import json
import os
import re
import sys

SITE, SOURCE, OUT = sys.argv[1:4]

SERIF = "'Iowan Old Style', Georgia, 'Palatino Linotype', Palatino, serif"
MONO = "ui-monospace, 'SF Mono', Menlo, Consolas, 'Liberation Mono', monospace"

# site.css's tokens, and og/og.svg's glows: (colour, opacity, cx, cy, r).
THEMES = {
    "dark": {
        "ground": "#090a11",
        "glows": [("#54dcc6", .2, .12, .02, .62), ("#8272ff", .24, .9, .02, .66),
                  ("#c8a06a", .12, .3, 1.05, .7)],
        "edge": "rgba(150,146,184,.22)",
        "tagline": "#eae8f2",
        "url": "#7f7ca0",
        "strands": ("#c8a06a", "#54dcc6", "#8272ff"),
        "term_edge": "rgba(150,146,184,.28)",
    },
    "light": {
        "ground": "#f6f4ee",
        "glows": [("#13917f", .1, .12, .02, .62), ("#5b4ae6", .09, .9, .02, .66),
                  ("#a77a3a", .08, .3, 1.05, .7)],
        "edge": "rgba(21,20,31,.12)",
        "tagline": "#15141f",
        "url": "#5f5b7d",
        "strands": ("#a77a3a", "#13917f", "#5b4ae6"),
        "term_edge": "rgba(21,20,31,.22)",
    },
}
# The terminal's palette, the same in both themes: the recordings' own.
TERM = {"bg": "#0e0f18", "text": "#eae8f2", "dim": "#b3afce", "muted": "#7f7ca0",
        "exit": "#e779cf", "agent-a": "#c8a06a", "agent-b": "#54dcc6", "agent-c": "#8272ff"}

# The canvas and its side margin, the logo's scale and top, the font size
# and baseline of the URL and of the tagline, and the terminal's top, font
# size and line height.
WIDTH, MARGIN = 1280, 48
LOGO_SCALE, LOGO_TOP = .75, 24
URL_SIZE, URL_BASE = 22, 70
TAG_SIZE, TAG_BASE = 50, 158
TERM_TOP, SIZE, STEP = 194, 24, 35
TAGLINE = "Many agents. One main. No retry loop."


def transcript(path):
    text = open(path, encoding="utf-8").read()
    if path.endswith(".cast"):
        events = [json.loads(line) for line in text.splitlines()[1:] if line.strip()]
        text = "".join(event[2] for event in events if event[1] == "o")
        text = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", text)
        text = text.replace("\r\n", "\n").replace("\r", "")
    lines = [line.rstrip() for line in text.splitlines()]
    starts = [i for i, line in enumerate(lines) if line.startswith("# landing on main")]
    if len(starts) != 1:
        sys.exit(f"{path}: no single '# landing on main' note")
    lines = lines[starts[0]:]
    while lines and not lines[-1]:
        lines.pop()
    lines = [line for line in lines if line and not line.startswith(("Updated ", "Run `"))]
    # The story the banner tells, in order; anything else means the
    # recording changed, and the banner and the README with it.
    story = [r"# landing on main", r"\[agent-a\] \$ braid land$",
             r"Landed \w{12} on main \(fast-forward\)$", r"\[agent-b\] \$ braid land$",
             r"Landed on main as \w{12}, rebased onto \w{12}$", r"\[agent-c\] \$ braid land$",
             r"\w{12} conflicts with main at \w{12}; main did not move:$",
             r"  CONTENT +shipping\.toml +\(1 region\)$", r"exit status 2$"]
    if len(lines) != len(story) or not all(re.match(s, l) for s, l in zip(story, lines)):
        sys.exit(f"{path}: the landings are not the story the banner tells:\n" + "\n".join(lines))
    return lines


def colours(line):
    """The coloured runs of one line: a prompt, a note, the exit status or
    braid's output."""
    if line.startswith("# "):
        return [(line, "muted")]
    prompt = re.match(r"\[(agent-[abc])\] \$ (.*)$", line)
    if prompt:
        return [(f"[{prompt[1]}]", prompt[1]), (" $ ", "muted"), (prompt[2], "text")]
    if re.match(r"exit status \d+$", line):
        return [(line, "exit")]
    return [(line, "dim")]


def logo_parts():
    page = open(SITE, encoding="utf-8").read()
    svg = re.search(r'<div class="hero-logo">(<svg.*?</svg>)</div>', page, re.S).group(1)
    defs = re.search(r"<defs>.*?</defs>", svg, re.S).group(0).replace("hl-cut", "hero-cut")
    strands = re.findall(r'<path d="([^"]+)"[^>]*class="(s[abc])"[^>]*mask="url\(#hl-cut(\d)\)"', svg)
    assert len(strands) == 3, strands
    paths = "\n".join(
        f'<path class="s {cls}" d="{d}" mask="url(#hero-cut{n})" pathLength="1"/>'
        for d, cls, n in strands)
    return defs, paths


def build(theme, lines, defs, paths):
    t = THEMES[theme]
    first = TERM_TOP + 44
    term_h = 44 + STEP * (len(lines) - 1) + 24
    height = TERM_TOP + term_h + 40
    term_x, term_w = MARGIN, WIDTH - 2 * MARGIN
    text_x = term_x + 34

    out = []
    w = out.append
    w("<!-- SPDX-License-Identifier: Apache-2.0\n     Copyright (c) 2026 UtopiaIA\n"
      "     Made by scripts/demos/hero.py from the agents recording: edit that, not this. -->")
    w(f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {WIDTH} {height}" '
      f'width="{WIDTH}" height="{height}" role="img" aria-labelledby="hero-title hero-desc">')
    w("<title id=\"hero-title\">Elffuss Braid: Many agents. One main. No retry loop.</title>")
    w("<desc id=\"hero-desc\">Three agents land on main in turn: agent-a as a fast-forward, "
      "agent-b rebased onto it, and agent-c with a conflict, so main did not move and braid "
      "exits with status 2.</desc>")
    sa, sb, sc = t["strands"]
    w("<style>")
    w(f".s {{ fill: none; stroke-width: 9.5; stroke-linecap: round; stroke-linejoin: round; "
      f"stroke-dasharray: 1 2; stroke-dashoffset: 0; }}")
    w(f".sa {{ stroke: {sa}; }} .sb {{ stroke: {sb}; }} .sc {{ stroke: {sc}; }}")
    w(f".tag {{ font-family: {SERIF}; font-size: {TAG_SIZE}px; font-weight: 600; "
      f"letter-spacing: -0.3px; fill: {t['tagline']}; }}")
    w(f".url {{ font-family: {MONO}; font-size: {URL_SIZE}px; letter-spacing: 1px; fill: {t['url']}; }}")
    w(f".ln {{ font-family: {MONO}; font-size: {SIZE}px; white-space: pre; }}")
    w("/* plays once, then holds: the last frame is the file as written */")
    w("@media (prefers-reduced-motion: no-preference) {")
    w("  .s { animation: draw 1.5s cubic-bezier(.22, .61, .36, 1) both; }")
    w("  .sb { animation-delay: .15s; } .sc { animation-delay: .3s; }")
    w("  .tag { animation: show .8s ease-out .45s both; }")
    w("  .url { animation: show .8s ease-out .8s both; }")
    w("  .term { animation: show .5s ease-out 1s both; }")
    w("  .ln { animation: show .3s ease-out both; }")
    w("}")
    w("@keyframes draw { from { stroke-dashoffset: 1; } to { stroke-dashoffset: 0; } }")
    w("@keyframes show { from { opacity: 0; } to { opacity: 1; } }")
    w("</style>")

    w("<defs>")
    w(f'<clipPath id="hero-card"><rect width="{WIDTH}" height="{height}" rx="24"/></clipPath>')
    for i, (colour, opacity, cx, cy, r) in enumerate(t["glows"]):
        w(f'<radialGradient id="hero-glow{i}" cx="{cx}" cy="{cy}" r="{r}">'
          f'<stop offset="0" stop-color="{colour}" stop-opacity="{opacity}"/>'
          f'<stop offset="1" stop-color="{colour}" stop-opacity="0"/></radialGradient>')
    w("</defs>")
    w('<g clip-path="url(#hero-card)">')
    w(f'<rect width="{WIDTH}" height="{height}" fill="{t["ground"]}"/>')
    for i in range(len(t["glows"])):
        w(f'<rect width="{WIDTH}" height="{height}" fill="url(#hero-glow{i})"/>')
    w("</g>")
    w(f'<rect x=".5" y=".5" width="{WIDTH - 1}" height="{height - 1}" rx="23.5" fill="none" '
      f'stroke="{t["edge"]}"/>')

    # The logo's strands start 17 units in, counting their round caps.
    w(f'<g transform="translate({MARGIN - 17 * LOGO_SCALE:.2f} {LOGO_TOP}) scale({LOGO_SCALE})">')
    w(defs)
    w(paths)
    w("</g>")
    w(f'<text class="url" x="{WIDTH - MARGIN}" y="{URL_BASE}" text-anchor="end">braid.elffuss.com</text>')
    w(f'<text class="tag" x="{MARGIN}" y="{TAG_BASE}">{html.escape(TAGLINE)}</text>')

    w('<g class="term">')
    w(f'<rect x="{term_x}" y="{TERM_TOP}" width="{term_w}" height="{term_h}" rx="16" '
      f'fill="{TERM["bg"]}" stroke="{t["term_edge"]}"/>')
    for i, colour in enumerate((TERM["agent-a"], TERM["agent-b"], TERM["agent-c"])):
        w(f'<rect x="{term_x + 1 + 3 * i}" y="{TERM_TOP + 22}" width="3" height="{term_h - 44}" '
          f'fill="{colour}"/>')
    w("</g>")
    for i, line in enumerate(lines):
        runs = "".join(f'<tspan fill="{TERM[c]}">{html.escape(s, quote=False)}</tspan>'
                       for s, c in colours(line))
        w(f'<text class="ln" x="{text_x}" y="{first + i * STEP}" xml:space="preserve" '
          f'style="animation-delay: {1.1 + .22 * i:.2f}s">{runs}</text>')
    w("</svg>")
    return "\n".join(out) + "\n"


def main():
    lines = transcript(SOURCE)
    defs, paths = logo_parts()
    for theme in THEMES:
        name = os.path.join(OUT, f"hero-{theme}.svg")
        svg = build(theme, lines, defs, paths)
        with open(name, "w", encoding="utf-8") as f:
            f.write(svg)
        print(name, len(svg.encode()), "bytes")


main()
