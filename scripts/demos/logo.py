#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright (c) 2026 UtopiaIA
"""Builds the animated braid logo for the README from the site's hero logo.

usage: logo.py <site/index.html> <out.svg>
"""
import re
import sys

html = open(sys.argv[1]).read()
svg = re.search(r'<div class="hero-logo">(<svg.*?</svg>)</div>', html, re.S).group(1)
defs = re.search(r"<defs>.*?</defs>", svg, re.S).group(0).replace("hl-cut", "cut")
strands = re.findall(r'<path d="([^"]+)"[^>]*class="(s[abc])"[^>]*mask="url\(#hl-cut(\d)\)"', svg)
assert len(strands) == 3, strands

paths = "\n".join(
    f'<path class="s {cls}" d="{d}" mask="url(#cut{n})" pathLength="1"/>' for d, cls, n in strands
)

out = f"""<!-- SPDX-License-Identifier: Apache-2.0
     Copyright (c) 2026 UtopiaIA -->
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 320 96" width="320" height="96" role="img" aria-label="Three strands braided together">
<title>elffuss-braid</title>
<style>
.s {{ fill: none; stroke-width: 9.5; stroke-linecap: round; stroke-linejoin: round; stroke-dasharray: 1 2; stroke-dashoffset: 0; }}
.sa {{ stroke: #c8a06a; }}
.sb {{ stroke: #54dcc6; }}
.sc {{ stroke: #8272ff; }}
@media (prefers-color-scheme: light) {{
  .sa {{ stroke: #a77a3a; }}
  .sb {{ stroke: #13917f; }}
  .sc {{ stroke: #5b4ae6; }}
}}
/* each strand draws in from the left, holds, and leaves to the right, again and again */
@media (prefers-reduced-motion: no-preference) {{
  .s {{ animation: weave 7s infinite both; }}
  .sb {{ animation-delay: .16s; }}
  .sc {{ animation-delay: .32s; }}
}}
@keyframes weave {{
  0% {{ stroke-dashoffset: 1; animation-timing-function: cubic-bezier(.22, .61, .36, 1); }}
  22% {{ stroke-dashoffset: 0; }}
  84% {{ stroke-dashoffset: 0; animation-timing-function: cubic-bezier(.55, 0, .75, .4); }}
  100% {{ stroke-dashoffset: -1; }}
}}
</style>
{defs}
{paths}
</svg>
"""
open(sys.argv[2], "w").write(out)
print(len(out), "bytes")
