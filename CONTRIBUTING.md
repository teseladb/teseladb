# Contributing to TeselaDB

Thank you for your interest in TeselaDB. Here you have how to propose a change, what a pull request needs before it can
be merged and the terms under which your contribution enters the project.

## Before you start

- For a bug, open an issue with the version, your platform, the command or driver call and what you expected. Use the
  bug report template.
- For a new feature or a change of behaviour, open an issue first and wait for an answer before you write the code.
  Every behaviour of TeselaDB is written in its specifications before it is implemented, so a feature needs that step.
- For a security problem, do not open a public issue. Follow [SECURITY.md](SECURITY.md).

## Build and test

`rust-toolchain.toml` pins Rust 1.98.0, which `rustup` installs for you on the first build. The workspace has no
third-party dependencies, so it builds without network. The first command builds the server; the second passes the quick gates.

```sh
cargo build --release -p teseladb-server
scripts/ci.sh --quick    # format, Clippy, dependency check and tests
```

`scripts/ci.sh` without arguments runs every gate, including the coverage gate of 95 % per crate. The full run needs Docker. Performance work is measured on Linux with `io_uring` and a release build.

## Rules for the code

- Write the tests first, then the code. A pull request without tests for what it changes is not merged.
- `cargo fmt` and `cargo clippy --workspace --all-targets -- -D warnings` must be clean. Do not silence a lint; fix its
  cause.
- Add no crate from crates.io or any other registry. `scripts/check-deps.sh` checks it.
- Keep the logic free of input and output. Real I/O lives only in `teseladb-sys`, `teseladb-io`, `teseladb-server` and
  the tools.
- `unsafe` code is allowed only in the modules listed in `scripts/check-deps.sh`, each block with its `// SAFETY:`
  comment and a test that exercises it.
- Nothing in the repository names another database product, commit messages included; the README is the only place
  that names the protocol TeselaDB is compatible with. The literals that clients send as they are live in
  `crates/teseladb-names`.
- Everything is written in English, from identifiers to commit messages.

## Pull requests

1. Fork the repository and create a branch from `main`.
2. Make your change with its tests. Then run `scripts/ci.sh --quick`.
3. Sign off every commit (`git commit -s`), as the next section explains.
4. Open a pull request against `main` and fill in its template.

Keep each pull request to one change. A maintainer reviews every pull request. It is merged by squash, with the title of the pull request as the commit
message, once a maintainer approves it and every conversation is resolved.

## Terms of your contribution

TeselaDB is distributed under the [Business Source License 1.1](LICENSE), and VIUZI ESP S.L. also licenses it
commercially to the organizations that go past its free limits. So that the project can keep offering both licenses,
every contribution comes with the three terms of this list.

- You sign off each commit with the Developer Certificate of Origin (https://developercertificate.org): the
  `Signed-off-by` line that `git commit -s` adds says that you wrote the change or have the right to submit it.
- You license your contribution under the license of this repository. You also grant VIUZI ESP S.L. a perpetual, worldwide, non-exclusive, royalty-free and irrevocable license over it, with the right to sublicense it under other licenses, commercial ones and the Change License of each version included.
- You keep the copyright of your contribution.

## Code of conduct

Everyone who takes part in the project follows the [code of conduct](CODE_OF_CONDUCT.md) in issues and in pull
requests alike, and when someone breaks it the maintainers apply its enforcement guidelines, which go from a private
warning up to a permanent ban from the repository for the most serious cases.
