# TeselaDB

**English** · [Español](README.es.md)

TeselaDB is a distributed document database written in Rust. A single node is already a complete database, and you
add nodes when you need more capacity or want to keep working while one of them is down. It ships as one binary for
Linux and macOS and as a Docker image.

The website is [teseladb.com](https://teseladb.com), with the [documentation](https://teseladb.com/docs), the
[quick start](https://teseladb.com/docs/get-started/quickstart-single-node), the
[compatibility pages](https://teseladb.com/docs) and the [downloads](https://teseladb.com/download). The Docker image
is [`teseladb/teseladb`](https://hub.docker.com/r/teseladb/teseladb) on Docker Hub and
[`ghcr.io/teseladb/teseladb`](https://github.com/teseladb/teseladb/pkgs/container/teseladb) on GitHub.

## Install

```sh
curl -fsSL https://downloads.teseladb.com/install.sh | sh
```

The install script detects your system and CPU, downloads the matching binary, checks its SHA-256 and copies it to
`/usr/local/bin`. When your user cannot write to that folder, the script runs `sudo` and asks for your password. To
install it in a folder of your own, set `TESELADB_INSTALL_DIR` to that folder. The archives of each version, each with
its `.sha256` file, are also in [Releases](https://github.com/teseladb/teseladb/releases).

## How it is built

Each collection is split into ranges of keys. Every range is a small replicated group with its own Raft log, so a
write goes to the leader of its range and is acknowledged once a majority of that range's copies has it on disk.
Inside each node, one thread runs on each CPU core and owns its ranges, without locks shared between cores. On Linux
the engine does its own disk and network input and output on `io_uring`, with `O_DIRECT` files and its own block
cache. Documents live in its own log-structured merge tree with MVCC versions, and full-text and vector search run in
the same binary on the same snapshot as the documents.

All the logic runs as state machines without input or output of their own. That lets a deterministic simulator run a
whole cluster with the production code, inject partitions, lost messages, torn writes and crashes, and replay any
failure from its seed. The protocols also have TLA+ models in `spec/tla`, checked with TLC.

The storage engine, Raft, the query engine, TLS, X.509, BSON, compression and hashing are code of this repository,
built from the public standards and specifications. The workspace has no third-party dependencies: `Cargo.lock` holds
only its own crates and the build needs no network.

## Repository layout

```text
crates/   the libraries of the workspace: sans-I/O logic, input and output, and the server
tools/    development tools that do not ship in the binary: the simulator, the benchmark and table generators
scripts/  quality gates and release scripts
spec/tla/ TLA+ models
docker/   the release image
```

## Build and test

You need Rust 1.98.0, which `rust-toolchain.toml` selects for you through `rustup`.

```sh
cargo build --release -p teseladb-server
cargo test --workspace
scripts/ci.sh --quick    # format, Clippy, dependency check and tests
```

`scripts/ci.sh` without arguments runs every gate: formatting, Clippy on the host and on x86_64, the dependency check,
`shellcheck`, the tests, simulated and real crash tests, coverage of 95 % of lines, functions and branches per crate,
the simulator and TLC. The full run needs Docker.

To report a problem or ask a question, open an issue in this repository.

## License

TeselaDB is source-available under the [Business Source License 1.1](LICENSE). You can read, build, modify and
redistribute the code and use it outside production without limits. Production use is free, commercial use included,
while your instances add up to 64 GB of RAM and 16 vCPUs or less and your organization's revenue in its last fiscal year
was under EUR 1,000,000. Offering TeselaDB to third parties as a database service always needs a commercial license
from VIUZI ESP S.L., whatever your size, and so does production use past those limits. Four years after each version
is published, that version also becomes available under the Apache License, Version 2.0. [COMMERCIAL.md](COMMERCIAL.md)
explains the terms in plain words.
