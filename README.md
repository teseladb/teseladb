# TeselaDB

**English** · [Español](README.es.md)

TeselaDB is a distributed document database compatible with the MongoDB 9.0 wire protocol. Your applications keep using the
official MongoDB drivers, `mongosh` and the MongoDB database tools, with no change to their code or their connection
options beyond the address of the node.

A single node is already a complete database. You add nodes when you need more capacity or want to keep working while
one of them is down. TeselaDB ships as one binary for Linux and macOS and as a Docker image.

The website is [teseladb.com](https://teseladb.com), with the [documentation](https://teseladb.com/docs) and the
[downloads](https://teseladb.com/download). The Docker image is
[`teseladb/teseladb`](https://hub.docker.com/r/teseladb/teseladb) on Docker Hub and
[`ghcr.io/teseladb/teseladb`](https://github.com/teseladb/teseladb/pkgs/container/teseladb) on GitHub.

## Quick start

In about five minutes you will install TeselaDB, start a node on your machine and open a `mongosh` session on it. A
single node is already a complete database that stores your collections and serves the MongoDB drivers.

To follow it you need Linux on x86_64 or aarch64 with glibc 2.28 or later, or macOS on Apple Silicon, with `curl` and
[`mongosh`](https://www.mongodb.com/docs/mongodb-shell/install/). On any other system, go straight to
[Run it with Docker](#run-it-with-docker), which only needs Docker and `mongosh`.

### Step 1. Install the binary

The install script downloads the binary for your system and CPU, checks its SHA-256 and copies it to `/usr/local/bin`.
When your user cannot write to that folder, the script runs `sudo` and asks for your password. Set
`TESELADB_INSTALL_DIR` to install it in a folder of your own, or `TESELADB_VERSION` to install a given version.

```sh
curl -fsSL https://downloads.teseladb.com/install.sh | sh
```

### Step 2. Check the version

This prints the version you installed, such as `teseladb 0.1.10`. If you installed the binary in a folder of your own,
that folder has to be in your `PATH` for your shell to find the command.

```sh
teseladb --version
```

### Step 3. Start the node in the background

The node keeps its data in `./data` and writes its log to `teseladb.log`. You need no configuration file, because the
node secures itself on its first start in an empty folder.

```sh
teseladb start --dbpath ./data > teseladb.log 2>&1 &
```

### Step 4. Wait until it is ready

This command reads the log once a second and returns when the node prints `ready`. On a laptop that takes a second or
two.

```sh
until grep -q '^ready' teseladb.log; do sleep 1; done
```

### What the first start sets up

On its first start in an empty folder the node turns on TLS and users, creates a certificate authority for the cluster
and a first password for the `root` user, and writes both into `./data`, where you find the two files that the steps
below use to connect. It listens on `127.0.0.1` only, so no other machine can reach it. The second table lists the four ports it listens on.

| File | What it holds |
| --- | --- |
| `pki/ca.pem` | The certificate authority of the cluster. Clients need it to trust the node. |
| `root-credentials.txt` | The `root` user and its first password. Only your user can read the file. |

| Port | What for |
| --- | --- |
| 27017 | Clients and drivers, always over TLS |
| 27100 | Traffic between nodes |
| 27200 | Administration with `teseladb admin`, `/metrics` and `/healthz` |
| 27080 | REST API and MCP server |

### Step 5. Read the root password

This reads the password of the `root` user from `root-credentials.txt` into a shell variable, so the password never
shows on your screen.

```sh
ROOT_PASSWORD=$(sed -n 's/^password=//p' ./data/root-credentials.txt)
```

### Step 6. Open mongosh

This opens `mongosh` on the node as the `root` user, over TLS and with the certificate authority of the node.

```sh
mongosh "mongodb://root:$ROOT_PASSWORD@127.0.0.1:27017/?tls=true&tlsCAFile=./data/pki/ca.pem&authSource=admin"
```

### Step 7. Try it

In the `mongosh` session, insert two orders and look for the ones over 20. The `find` returns the order of Ana, the
only one with a total above 20.

```js
db.orders.insertMany([
  { customer: "Ana", total: 30 },
  { customer: "Luis", total: 12 },
]);
db.orders.find({ total: { $gt: 20 } });
```

### Connect your application

Your application uses the official MongoDB driver for its language with this connection string, filled in with your
user, its password, the address of the node and the path to `ca.pem`. It is the connection string of any MongoDB
deployment with TLS.

```text
mongodb://<user>:<password>@<node address>:27017/?tls=true&tlsCAFile=<path to ca.pem>&authSource=admin
```

The string needs `tls=true`, because the client port only takes TLS. The file `ca.pem` is not a secret, so you can
copy it to every machine that runs your applications. For an application, create a user with only the roles it needs
and keep `root` for administration.

### Run it with Docker

With Docker you need neither the binary nor glibc, because the node runs in a container from the image `teseladb/teseladb`, and you can follow these steps on any system that runs Docker. Each step runs a single command in the same terminal.

#### Step 1. Start the container

This starts a node in a container named `teseladb`, keeps its data in the `teseladb-data` volume and publishes the
client port. The image is `teseladb/teseladb` on Docker Hub, and the same image is at `ghcr.io/teseladb/teseladb` on
GitHub. With `-p 27017:27017` the port is open on every interface of your machine, so other machines on your network
can reach the node, and to keep the port on your machine only you write `-p 127.0.0.1:27017:27017` in its place when
you run the command.

```sh
docker run -d --name teseladb \
  -p 27017:27017 \
  -v teseladb-data:/data \
  teseladb/teseladb:latest \
  start --dbpath /data --bind_ip 0.0.0.0
```

#### Step 2. Wait until it is ready

This command reads the log of the container once a second and returns when the node prints `ready`.

```sh
until docker logs teseladb 2>&1 | grep -q '^ready'; do sleep 1; done
```

#### Step 3. Read the root password

This reads the password of the `root` user from the credentials file inside the container into a shell variable.

```sh
ROOT_PASSWORD=$(docker exec teseladb cat /data/root-credentials.txt | sed -n 's/^password=//p')
```

#### Step 4. Copy the certificate authority

`mongosh` runs outside the container. It needs its own copy of `ca.pem` to trust the node, and this command copies the
file out of the container into the folder where you are.

```sh
docker cp teseladb:/data/pki/ca.pem ./ca.pem
```

#### Step 5. Open mongosh

This opens `mongosh` on the node as the `root` user, over TLS and with your copy of `ca.pem`. From here you can try
the orders of [step 7](#step-7-try-it) above.

```sh
mongosh "mongodb://root:$ROOT_PASSWORD@127.0.0.1:27017/?tls=true&tlsCAFile=./ca.pem&authSource=admin"
```

### Stop and clean up

Leave `mongosh` with `exit` and follow the part that matches how you ran the node, with the binary or with Docker. The
commands remove everything this quick start created, so nothing of the test stays on your machine.

#### If you ran the binary

The first command stops the node, and the second deletes its data folder with its
log, so nothing of this test stays on your disk.

```sh
pkill -f "teseladb start --dbpath ./data"
rm -rf ./data teseladb.log
```

#### If you ran it with Docker

The commands remove the container, then its volume with the data, and last your
copy of `ca.pem`.

```sh
docker rm -f teseladb
docker volume rm teseladb-data
rm ./ca.pem
```

### If something fails

When `mongosh` does not connect, read `teseladb.log`, or `docker logs teseladb` if you used Docker. The node writes
there what it is missing. When the connection is refused, check that the connection string has `tls=true` and the
right path to `ca.pem`. When the install script says `there is no binary for` your system, TeselaDB has no binary for
it yet. On that system you run TeselaDB with Docker, as the section above shows.

## REST API and MCP server

Every node also serves a REST API and an MCP server on port 27080, with the same commands and the same permissions as
the wire protocol. This example runs everything with Docker: it starts a node with that port published, creates an API
key and calls both.

### Start a node with the REST port

```sh
docker run -d --name teseladb -p 27017:27017 -p 27080:27080 -v teseladb-data:/data \
  teseladb/teseladb:latest start --dbpath /data --bind_ip 0.0.0.0
until docker logs teseladb 2>&1 | grep -q '^ready'; do sleep 1; done
docker cp teseladb:/data/pki/ca.pem ./ca.pem
```

### Create an API key

`teseladb admin` runs inside the container and prints the new key, which this command keeps in a shell variable. The
key has the roles of the `root` user; for an application, create a user with only the roles it needs and a key for it.

```sh
export TESELADB_API_KEY=$(docker exec teseladb teseladb admin --dbpath /data apikey create --user root --db admin)
```

### Call the REST API

Each route is the command of the same name. The first call inserts an order and the second one finds it.

```sh
curl --cacert ca.pem -X POST https://127.0.0.1:27080/v1/databases/shop/collections/orders/insertOne \
  -H "Authorization: Bearer $TESELADB_API_KEY" -H "Content-Type: application/json" \
  -d '{"document": {"customer": "Ana", "total": 30}}'
curl --cacert ca.pem -X POST https://127.0.0.1:27080/v1/databases/shop/collections/orders/find \
  -H "Authorization: Bearer $TESELADB_API_KEY" -H "Content-Type: application/json" -d '{"filter": {}}'
```

### Connect an AI agent over MCP

The MCP server has the database tools of the MongoDB MCP Server, with the same names and arguments (`find`,
`aggregate`, `count`, `collection-schema` and the rest). It starts read-only: the tools that write appear once you turn
them on with the `mcpOptions` cluster parameter. An MCP client can talk to `https://<node>:27080/mcp` with the key in
`Authorization: Bearer`, or launch the server over stdio. With Docker, this is the entry for the `mcpServers` of your
client, such as Claude Desktop, Claude Code or Cursor; replace `tdb_…` with your key.

```json
{
  "mcpServers": {
    "teseladb": {
      "command": "docker",
      "args": ["exec", "-i", "-e", "TESELADB_API_KEY", "teseladb", "teseladb", "mcp",
               "--uri", "https://127.0.0.1:27080", "--tlsCAFile", "/data/pki/ca.pem"],
      "env": { "TESELADB_API_KEY": "tdb_…" }
    }
  }
}
```

The [REST API reference](https://teseladb.com/docs/reference/rest-api) and the
[MCP server page](https://teseladb.com/docs/ai/mcp-server) list every route and every tool.

## Install

| Platform | How to install |
| --- | --- |
| Linux x86_64 or aarch64 with glibc 2.28 or later, macOS on Apple Silicon | The install script above, or the archive of your platform from [Releases](https://github.com/teseladb/teseladb/releases) |
| Any system with Docker | `docker pull teseladb/teseladb:latest` from Docker Hub, or `docker pull ghcr.io/teseladb/teseladb:latest` from GitHub |

Each archive in Releases comes with its `.sha256` file. To check an archive by hand, run this in the folder where you
downloaded both files.

```sh
shasum -a 256 -c teseladb-0.1.10-linux-x86_64.tar.gz.sha256
```

## Your MongoDB clients work as they are

To your application, a TeselaDB node looks like the router of a sharded MongoDB cluster. The drivers find it with the
same `hello` handshake and send it the same commands, so the code you wrote for MongoDB runs unchanged. Every behaviour
is measured against a real MongoDB 9.0 server, command by command and error code by error code. Where TeselaDB answers
differently, the documentation lists the difference and its reason in its
[compatibility pages](https://teseladb.com/docs).

## How it is built and how it compares with MongoDB

TeselaDB was written from scratch in Rust. Its storage engine, the Raft consensus, the query engine, TLS, X.509 and BSON
are its own code, built from the public standards and specifications, with nothing taken from the MongoDB server and no
third-party libraries.

Each collection is split into ranges of keys. Every range is a small replicated group with its own Raft log, so a
write goes to the leader of its range and is applied once a majority of that range's copies has it. Inside each node,
one thread runs on each CPU core and owns its ranges, without locks shared between cores. All the logic runs as state
machines without input or output of their own, which lets a deterministic simulator run a whole cluster with the
production code, replay any failure from its seed and check the result.

The internal design differs from MongoDB in a few ways you can notice.

| | MongoDB | TeselaDB |
| --- | --- | --- |
| Roles | A replica set has one primary; a sharded cluster adds `mongos` routers and config servers | Every node runs the same binary with the same role. Any node takes clients |
| Replication | One oplog per replica set, copied from the primary | One Raft log per range, so the leaders of different ranges can sit on different nodes |
| Scale out | You choose a shard key and run the balancer | Ranges split when they grow or get busy and merge when they shrink. Their copies and leaders spread over the nodes by load |
| Reads with `readConcern: "local"` | Can return a `w: 1` write that a failover later rolls back | Return only writes that a majority of copies holds |
| Search indexes | Updated after the write, eventually consistent | Read the same snapshot as the documents they index |
| First start | No TLS and no users until you configure them | TLS and users are on from the first start |

Each node also serves a REST API and an MCP server, with the same commands and the same permissions as the wire
protocol.

## Features and roadmap

The tables go through the engine layer by layer and give the status of each part. "0.1.13" means it is in the version you can download today. "Next release" means it is written and tested and will ship in the next version. "In progress"
means it is being written now.

### Execution and input/output

One thread per CPU core owns its data and talks to the other cores through lock-free queues, so cores never wait on
each other and a core's data stays in its own CPU caches. The engine does its own disk and network input/output on
`io_uring`, with files opened with `O_DIRECT` and its own block cache, so the Linux page cache never adds a second copy
or an unexpected pause.

| Feature | Status |
| --- | --- |
| Thread per core, no locks shared between cores | 0.1.13 |
| Own `io_uring` runtime, `O_DIRECT` files and per-core block cache | 0.1.13 |
| Separate disk queues for the log and for data, with the log first and a bandwidth share for background work | Next release |
| Admission control that refuses new work when a core's queue waits more than 5 ms; short operations go before long scans | Next release |
| Client connections on `io_uring` rings (multishot accept and receive, provided buffer rings), optional `SQPOLL` | Next release |
| Buffers and block cache on 2 MiB huge pages | Next release |
| 1 GiB pages for the block cache; background writes capped while client reads wait | In progress |
| Kernel-bypass networking with `AF_XDP` | In progress |

### Durability and replication

Every range of keys is its own Raft group, so the leaders of different ranges spread over the nodes and a write only
waits for the copies of its own range. A write is acknowledged once a majority of copies has it on disk.

| Feature | Status |
| --- | --- |
| Multi-Raft by range with lease reads | 0.1.13 |
| Ranges that split and merge by size and load | 0.1.13 |
| Gradual write throttle (−20 % / +25 %) driven by the compaction debt | 0.1.13 |
| Whole files dropped at once when a range leaves a core | 0.1.13 |
| One shared log per disk for all cores, with one `fsync` per group of writes and the next group written while the previous one syncs | Next release |
| Ranges moved between cores with hard links to their files, without copying | Next release |
| Range files sent to another node with `SEND_ZC`, without copying them into the process | Next release |

### Storage engine

TeselaDB stores documents in its own log-structured merge tree. Writes are sequential, which suits NVMe drives and
heavy ingestion. Compaction is size-tiered in the upper levels, to absorb bursts, and leveled below, to keep reads and
disk space low.

| Feature | Status |
| --- | --- |
| LSM tree with MVCC versions and block compression (zlib by default) | 0.1.13 |
| Size-tiered compaction on top and leveled below; LZ4 and zstd with a per-collection dictionary | Next release |
| Blocks packed back to back on disk, without padding each one to a page | Next release |
| Old versions kept as a delta of the next version | Next release |
| Values over 4 KiB in separate value files, so compaction does not rewrite them | Next release |
| Ribbon filters, a learned index (PGM) and Elias-Fano over each file's blocks; zone maps per block let scans skip blocks | Next release |
| Memtable on an adaptive radix tree | Next release |
| Stored documents with a table of field offsets, a per-collection dictionary of field names; documents packed by the collection's dominant schema | Next release |

### Query engine

A query reads fields straight from the stored bytes, without building documents in memory. Its filters are compiled
once per query shape into a register-based bytecode and run on batches of documents. The planner picks among the
candidate plans the way MongoDB does, so `explain` shows the same plan shapes.

| Feature | Status |
| --- | --- |
| Query planner with plan cache, index intersection and MongoDB's plan selection | 0.1.13 |
| BSON read without copies, straight from the network buffer and the stored blocks | Next release |
| Register bytecode machine for filters and `$expr`, cached by query shape, run on batches of 64 documents | Next release |
| SIMD kernels (NEON, SSE2, AVX2) for comparisons and `$group`, radix sort and top-k | Next release |
| Aggregations split across the cores and the leaders on other nodes; filters and index lookups sent to the node that holds the data | Next release |
| Roaring bitmaps for `$and`, `$or` and `$in` | Next release |
| `$project` on the bytecode machine, the rest of the cost-based planner, a columnar copy of cold blocks for analytics; prefetching in index scans | In progress |

### Search

A document is searchable as soon as its write is acknowledged, because full-text and vector search run inside the same
binary and read the same snapshot as the documents. Every copy of a range builds its own search index from the same
replicated log, so search keeps working when a leader moves to another node.

| Feature | Status |
| --- | --- |
| `$text`, `$search`, `$searchMeta` and `$vectorSearch` | 0.1.13 |
| Immutable search segments per range, term dictionary as a finite state transducer (exact and prefix matches, fuzzy matches and regular expressions), BM25 with Block-Max WAND, HNSW for vectors | 0.1.13 |
| SIMD decoding of posting lists, a tokenizer that allocates nothing per term, a table of frequent terms, indexed filters applied inside the top-k search; results passed on in batches | In progress |

### Verification

Every layer runs as state machines without input or output of their own, so the same production code runs in a
deterministic simulator that injects network partitions, lost messages, messages out of order, torn writes and crashes.
The simulator replays any failure from its seed. These are development tools; they do not ship in the binary.

| Tool | Status |
| --- | --- |
| Deterministic simulator with fault injection and a linearizability checker | In use |
| TLA+ models of the system's protocols, checked with TLC | In use |
| Differential fuzzing of queries against a real MongoDB 9.0 server | In use |
| Open-loop benchmark with HDR histograms up to p99.99, run on the same hardware as MongoDB | In use |

## Repository layout

```text
crates/   the libraries of the workspace: sans-I/O logic, input and output, and the server
tools/    development tools that do not ship in the binary: the simulator, the benchmark and table generators
scripts/  quality gates and release scripts
spec/tla/ TLA+ models
docker/   the release image
```

## Build and test

`rust-toolchain.toml` pins Rust 1.98.0, which `rustup` installs for you on the first build. The workspace has no
third-party dependencies, so it builds without network. These three commands build the server, run the tests and pass
the quick gates.

```sh
cargo build --release -p teseladb-server
cargo test --workspace
scripts/ci.sh --quick    # format, Clippy, dependency check and tests
```

`scripts/ci.sh` without arguments runs every gate: formatting, Clippy on the host and on x86_64, the dependency check,
`shellcheck`, the tests, the crash tests on a simulated disk and on a real one, the coverage gate of 95 % per crate, the
simulator and TLC. The full run uses Docker for the x86_64 tests and for `shellcheck`.

To report a problem or ask a question, open an issue in this repository. [CONTRIBUTING.md](CONTRIBUTING.md) explains
how to propose a change.

## License

TeselaDB is source-available under the [Business Source License 1.1](LICENSE). Outside production you can change the
code and redistribute it without limits. Production use is free, commercial use included, while your nodes add up to
64 GB of RAM and 16 vCPUs or less and your organization's revenue in its last fiscal year was under EUR 1,000,000.
Production use past those limits needs a commercial license from VIUZI ESP S.L. Offering TeselaDB to third parties as a
database service always needs one, whatever your size. Four years after each version is published, that version also
becomes available under the Apache License, Version 2.0. [COMMERCIAL.md](COMMERCIAL.md) explains the terms in plain
words.
