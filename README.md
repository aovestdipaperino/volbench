# volbench

[![GitHub stars](https://img.shields.io/github/stars/aovestdipaperino/volbench?logo=github&color=brightgreen)](https://github.com/aovestdipaperino/volbench/stargazers)

A tiny standalone benchmark that measures **filesystem metadata latency** and
**SQLite WAL/mmap behaviour** for a given folder.

It was built to diagnose a report that a tool (tokensave) runs **~3 orders of
magnitude slower** when its project lives on a **VHD-mounted volume in Windows**.
It reproduces the two hot paths that touch the project volume:

1. **Filesystem** — walk the tree once, then re-`stat()` every file. This mirrors
   tokensave's `find_stale_files`, which walks the project and then issues a
   second `fs::metadata()` per file. On a local SSD a stat is ~1–5 µs; on a VHD
   or network volume it can be 1–10 ms. That gap *is* the 1000×.
2. **SQLite** — create a temporary DB **on the target folder** and exercise it
   under two pragma profiles:
   - `WAL + mmap` (exactly what tokensave uses today), and
   - `DELETE journal, mmap off` (the proposed fallback for slow volumes).

   The delta between them *on the same volume* tells you whether WAL/mmap is the
   problem.

## Install

```sh
cargo install --git https://github.com/aovestdipaperino/volbench
```

**Prerequisites:** a Rust toolchain (`rustup`) and a C compiler. SQLite is
compiled from source via `rusqlite`'s `bundled` feature, so no system SQLite is
required — but you do need a C toolchain:

- **Windows:** "Desktop development with C++" / MSVC Build Tools.
- **macOS:** `xcode-select --install`.
- **Linux:** `gcc`/`clang` (usually already present).

To run from a clone instead of installing:

```sh
git clone https://github.com/aovestdipaperino/volbench && cd volbench
cargo run --release -- <FOLDER>
```

## Usage

```sh
volbench <FOLDER>
```

Point it at the folder on the volume you want to test. The FS phase only reads
the folder; the DB phase writes a temporary `volbench_*.db` **inside** it and
deletes it (plus `-wal`/`-shm`) when done.

```
OPTIONS:
  --no-fs              Skip the filesystem benchmark
  --no-db              Skip the SQLite benchmark
  --max-stats <N>      Cap the per-file stat pass (default 200000)
  --db-rows <N>        Rows to bulk-insert (default 20000)
  --db-queries <N>     Random point queries (default 5000)
  --db-small-txns <N>  Single-row commits, the fsync-heavy case (default 500)
  -h, --help
```

### The key comparison

Run it **twice** — once on the VHD, once on a local-disk copy of the same
folder — and compare:

```sh
# On the VHD
volbench V:\path\to\project

# On a local copy
volbench C:\local\project
```

## Interpreting the output

- **`per-stat` latency** is the headline. Local disks: tens of µs or less,
  warming to single digits. If you see **hundreds of µs to milliseconds per
  stat**, the virtual-disk/network layer is the dominant cost — that is the
  3-orders-of-magnitude finding, reproduced directly.
- **Cold vs warm stat pass** shows whether the OS metadata cache helps. On a
  local disk pass 2 is much faster than pass 1; on some VHD setups it barely
  improves, which makes repeated scans pathological.
- **SQLite table** — if `WAL + mmap` is ≥2× slower than the `DELETE/mmap-off`
  fallback on the same volume, the database layer is also implicated and should
  switch profiles on such volumes.

The tool prints an explicit `FS verdict:` and `SQLite verdict:` summarizing
which layer dominates.

## License

MIT
