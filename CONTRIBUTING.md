# Contributing

Thanks for wanting to help. Braid is at version 0.1, so the most useful
contributions right now are reviews of the [design](docs/design.md) and the
[format](docs/spec/format-v1.md), bug reports, and tests.

Coding agents: part 2 of [AGENTS.md](AGENTS.md) says the same in short,
with the checks to run before every commit.

## Build and test

You need a stable Rust toolchain, 1.89 or newer.

```bash
cargo build
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check          # needs cargo-deny
```

CI runs the same checks on every platform in its matrix, checks the minimum
Rust version, builds the format crate for WebAssembly, and runs the privacy
check in `scripts/privacy-check.sh`.

## Rules

- **No code from git or any other GPL project.** Git is GPLv2 and braid is
  Apache-2.0. Do not port, translate or paste from git's sources. Reading
  documentation and observing behaviour is fine. The git bridge uses
  [gitoxide](https://github.com/GitoxideLabs/gitoxide), which is MIT or
  Apache-2.0.
- **Dependencies must be compatible with Apache-2.0.** `deny.toml` holds the
  list of accepted licences.
- **Hashed formats are specified first.** A change to the bytes of an id, a
  tree or a changeset needs an update to `docs/spec/format-v1.md` and to its
  golden vectors in the same pull request, and `docs/spec/check_vectors.py`
  must still pass. Format v1 is frozen, so such a change needs a new format
  version.
- **Performance claims come with a reproducible script** and are stated as a
  ratio against git on the same machine. Do not publish absolute timings or
  describe your hardware. [`bench/run.py`](bench/README.md) is that script
  for every number in the README.
- **Tests come with the change.** Bug fixes include a test that fails without
  the fix.

## Licence of contributions

Unless you say otherwise, any contribution you submit is licensed under
Apache-2.0, as section 5 of the [licence](LICENSE) describes.
