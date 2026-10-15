#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright (c) 2026 UtopiaIA
"""Recomputes the golden vectors of format-v1.md from the document alone.

This is a second, independent implementation of the hashed formats, written
from the specification and not from the Rust code. It needs the reference
BLAKE3 tool, b3sum (`cargo install b3sum`), on the PATH.

    python3 docs/spec/check_vectors.py
"""
import subprocess
import sys


def b3(context, payload):
    out = subprocess.run(["b3sum", "--derive-key", context, "--no-names"],
                         input=payload, capture_output=True, check=True)
    return bytes.fromhex(out.stdout.decode().strip())


def blob_id(data):
    return b3("elffuss-braid 2026 blob v1", data)


def tree_id(data):
    return b3("elffuss-braid 2026 tree v1", data)


def changeset_id(data):
    return b3("elffuss-braid 2026 changeset v1", data)


def uv(n):
    out = bytearray()
    while True:
        low = n & 0x7F
        n >>= 7
        if n == 0:
            out.append(low)
            return bytes(out)
        out.append(low | 0x80)


def sv(n):
    return uv(((n << 1) ^ (n >> 63)) & ((1 << 64) - 1))


def bstr(b):
    return uv(len(b)) + b


def entry(name, kind, *fields):
    out = bstr(name) + bytes([kind])
    if kind in (0, 1, 2):
        out += fields[0] + uv(fields[1])
    elif kind == 3:
        out += fields[0]
    elif kind == 4:
        out += bytes([fields[0]]) + fields[1]
    return out


def flat(entries):
    entries = sorted(entries, key=lambda e: e[0])
    return bytes([1, 0]) + uv(len(entries)) + b"".join(entry(*e) for e in entries)


def split_ends(level, name, count):
    if count >= 512:
        return True
    if count < 32:
        return False
    h = b3("elffuss-braid 2026 tree-split v1", bytes([level]) + name)
    return int.from_bytes(h[:4], "little") < (1 << 25)


def split_directory(entries):
    """Returns the ids of all nodes of a directory of more than 1024 entries."""
    entries = sorted(entries, key=lambda e: e[0])
    nodes, children, run = [], [], []
    for i, e in enumerate(entries):
        ends = split_ends(0, e[0], len(run) + 1)
        run.append(e)
        if ends or i + 1 == len(entries):
            body = bytes([1, 1]) + uv(len(run)) + b"".join(entry(*x) for x in run)
            nid = tree_id(body)
            nodes.append(nid)
            children.append((run[-1][0], nid, len(run)))
            run = []
    level = 1
    while len(children) > 1:
        parents, run = [], []
        for i, child in enumerate(children):
            ends = split_ends(level, child[0], len(run) + 1)
            run.append(child)
            if ends or i + 1 == len(children):
                body = bytes([1, 2, level]) + uv(len(run)) + b"".join(
                    bstr(c[0]) + c[1] + uv(c[2]) for c in run)
                nid = tree_id(body)
                nodes.append(nid)
                parents.append((run[-1][0], nid, sum(c[2] for c in run)))
                run = []
        children = parents
        level += 1
    return nodes


EXPECTED = {
    "empty tree": "3136b0f18562ee73b840ea348c54ae3c7e97e4cc3e547ea9dd03fa1cd8b384cf",
    "small tree": "a6c6d54bb7663d576e8f7a702e0670927e22098d6c15c10475e06b51dc3d8c1b",
    "large tree": "2c88a412664c1fd862c3487e1313a045a20fe4f7bdeaf1bb71e08564edc7dcce",
    "changeset": "4f8d49034a95a2938d30d13bee5508100211d04e1c125ef567a0c41c27f777d1",
}


def main():
    got = {}
    empty_id = tree_id(flat([]))
    got["empty tree"] = empty_id

    got["small tree"] = tree_id(flat([
        (b"README.md", 0, blob_id(b"hello\n"), 6),
        (b"run.sh", 1, blob_id(b"#!/bin/sh\n"), 10),
        (b"latest", 2, blob_id(b"README.md"), 9),
        (b"src", 3, empty_id),
        (b"vendor", 4, 1, bytes([0x11] * 20)),
    ]))

    x = blob_id(b"x")
    large = split_directory([(f"f{i:05}".encode(), 0, x, 1) for i in range(5000)])
    got["large tree"] = large[-1]

    changes = sorted([
        (b"README.md", 1, blob_id(b"hello\n"), 6),
        (b"src/lib.rs", 1, blob_id(b""), 0),
    ])
    change_list = uv(len(changes)) + b"".join(
        bstr(p) + bytes([op]) + bid + uv(size) for p, op, bid, size in changes)
    sig = bstr(b"Ada Lovelace") + bstr(b"ada@example.org") + sv(1_700_000_000) + sv(60)
    changeset = (bytes([1]) + uv(0) + empty_id + bytes([0]) + change_list + sig + sig
                 + bstr(b"Initial import\n") + uv(0) + bytes([0]))
    got["changeset"] = changeset_id(changeset)

    ok = True
    for name, expected in EXPECTED.items():
        match = got[name].hex() == expected
        ok &= match
        print(f"{'ok  ' if match else 'FAIL'} {name}: {got[name].hex()}")
    print(f"large tree nodes: {len(large)} (expected 30)")
    ok &= len(large) == 30
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
