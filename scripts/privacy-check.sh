#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright (c) 2026 UtopiaIA
#
# Fails if the repository would publish data that identifies a machine or a
# person: home-directory paths in tracked files, co-author or tool-attribution
# trailers in history, or maintainer commits without the noreply address.
# CI runs it on every push; run it locally before pushing too.
set -euo pipefail

status=0
report() {
  echo "privacy-check: $1" >&2
  status=1
}

# 1. Home-directory paths in tracked text files. CI runner paths are fine.
if hits=$(git grep -nIE '/(Users|home)/[A-Za-z0-9._-]+/' -- . ':(exclude)scripts/privacy-check.sh' |
  grep -vE '/[h]ome/runner/'); then
  echo "$hits" >&2
  report "home-directory paths in tracked files"
fi

# 2. Co-author and tool-attribution trailers anywhere in history.
if git log --format=%B | grep -qiE '^co-authored-by:|generated with .*claude'; then
  report "attribution trailer in a commit message"
fi

# 3. The maintainer account commits with its noreply address only.
if bad=$(git log --format='%an <%ae>%n%cn <%ce>' | sort -u | grep -E '^KikoCis <' |
  grep -v '<KikoCis@users.noreply.github.com>$'); then
  echo "$bad" >&2
  report "maintainer identity without the noreply address"
fi

if [ "$status" -eq 0 ]; then
  echo "privacy-check: clean"
fi
exit "$status"
