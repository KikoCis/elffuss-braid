#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright (c) 2026 UtopiaIA
"""Exports the site's wide "How it works" diagram for the README, in
English, as standalone SVGs, dark and light.

usage: diagram.py <site/index.html> <site/site.css> <out dir>

The site's tall diagram, for narrow screens, stays on the site: for a
reader who has chosen a theme, GitHub rewrites the colour-scheme media
queries of a README's <picture>, and a source that also tests the screen's
width then shows on wide screens too.

The site's diagram tells the scene of crates/cli/tests/three_agents.rs, where
c lands as a fast-forward, a is rebased with 1 path merged, and b conflicts.
The README tells the scene of docs/assets/demo-agents.gif and of its quick
start, where a lands as a fast-forward, b is rebased onto it without
merging any path, and c conflicts. So the letters move round, a to b, b to
c and c to a, each with its colour, and the rebase is only "rebased". Every
other label is the site's.

The colours are site.css's tokens for each theme, written into a <style>,
and the ground is left transparent for GitHub's own page. No scripts and no
external fonts: GitHub shows these files through <img>.
"""
import os
import re
import sys

SITE, CSS, OUT = sys.argv[1:4]
ROUND = {"a": "b", "b": "c", "c": "a"}
WIDTH, HEIGHT = 1000, 408
TITLE = ("How braid works: workspaces commit in parallel over one shared store, "
         "land on main in turn, and git sync sends main to any git remote")


def tokens(css):
    """site.css's custom properties: the dark ones of :root, and the light
    ones that its prefers-color-scheme block puts over them."""
    dark_block = re.search(r"^:root \{(.*?)^\}", css, re.S | re.M).group(1)
    light_block = re.search(r"@media \(prefers-color-scheme: light\) \{\s*:root \{(.*?)\}", css, re.S).group(1)

    def props(block):
        block = re.sub(r"/\*.*?\*/", "", block, flags=re.S)
        return dict(re.findall(r"--([\w-]+):\s*([^;]+);", block))

    dark = props(dark_block)
    return {"dark": dark, "light": {**dark, **props(light_block)}}


def rules(css, values, used):
    """The diagram's rules of site.css, `.dgm .x { ... }`, for the classes
    the SVG uses, with every var() resolved."""
    out = []
    for cls, body in re.findall(r"^\.dgm \.([\w-]+) \{([^}]*)\}", css, re.M):
        # The loop back and its arrow belong to the agent that conflicts:
        # b's teal on the site, c's indigo here.
        if cls in ("loop", "ar-b"):
            body = body.replace("var(--g-teal)", "var(--g-indigo)")
            cls = "ar-c" if cls == "ar-b" else cls
        if cls in used:
            body = re.sub(r"var\(--([\w-]+)\)", lambda m: values[m[1]].strip(), body.strip())
            out.append(f".{cls} {{ {body} }}")
    return "\n".join(out)


def english(svg):
    svg = re.sub(r'<tspan data-l="es" lang="es"[^>]*>[^<]*</tspan>', "", svg)
    return svg.replace(' data-l="en" lang="en"', "")


def relabel(svg):
    """The README's scene: the letters move round, colours and all."""
    svg, n = re.subn(r"\b(acc|t|st|ar|dw-m|dt-m)-([abc])\b", lambda m: f"{m[1]}-{ROUND[m[2]]}", svg)
    assert n >= 12, n
    svg, n = re.subn(r"(>workspace(?: |</tspan><tspan [^>]*>))([abc])<",
                     lambda m: f"{m[1]}{ROUND[m[2]]}<", svg)
    assert n == 3, n
    svg, n = re.subn(r">rebased · 1 path merged<", ">rebased<", svg)
    assert n == 1, n
    return svg


def main():
    html = open(SITE, encoding="utf-8").read()
    css = open(CSS, encoding="utf-8").read()
    svg = re.search(r'<svg class="dgm dgm-wide".*?</svg>', html, re.S).group(0)
    svg = relabel(english(svg))
    svg, n = re.subn(r"^<svg [^>]*>", "", svg)
    assert n == 1
    used = set(re.findall(r'class="([^"]+)"', svg))
    used = {c for group in used for c in group.split()}
    for theme, values in tokens(css).items():
        out = (
            "<!-- SPDX-License-Identifier: Apache-2.0\n     Copyright (c) 2026 UtopiaIA\n"
            "     Made by scripts/demos/diagram.py from the site's diagram: edit that, not this. -->\n"
            f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {WIDTH} {HEIGHT}" '
            f'width="{WIDTH}" height="{HEIGHT}" role="img" aria-labelledby="dgm-title">\n'
            f'<title id="dgm-title">{TITLE}</title>\n'
            f"<style>\n{rules(css, values, used)}\n</style>\n"
            f"{svg}\n"
        )
        name = os.path.join(OUT, f"how-it-works-{theme}.svg")
        with open(name, "w", encoding="utf-8") as f:
            f.write(out)
        print(name, len(out.encode()), "bytes")


main()
