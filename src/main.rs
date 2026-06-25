//! volbench — diagnose filesystem + SQLite latency on a folder.
//!
//! Built to localize a "3 orders of magnitude slower on a VHD-mounted volume"
//! report. It reproduces the two hot paths a tool like tokensave exercises on
//! the project volume:
//!
//!   1. A gitignore-agnostic tree walk followed by a per-file `metadata()`
//!      stat — the same double-stat pattern as tokensave's `find_stale_files`.
//!   2. A SQLite database created *on the target folder*, exercised under two
//!      pragma profiles: WAL + mmap (what tokensave uses today) vs. the
//!      DELETE-journal / mmap-off fallback. The delta between them on the same
//!      volume tells you whether WAL/mmap is the problem.
//!
//! On a local SSD a stat is ~1-5 us; on a VHD/network volume it can be
//! 1-10 ms. That gap *is* the three orders of magnitude. This tool measures it.

use std::path::{Path, PathBuf};
use std::time::Instant;

use rusqlite::{params, Connection};
use walkdir::WalkDir;

// ── Config ──────────────────────────────────────────────────────────

struct Args {
    target: PathBuf,
    run_fs: bool,
    run_db: bool,
    max_stats: usize,
    db_rows: usize,
    db_queries: usize,
    db_small_txns: usize,
}

fn print_usage() {
    eprintln!(
        "volbench — filesystem + SQLite latency benchmark for a folder\n\
\n\
USAGE:\n\
    volbench <FOLDER> [OPTIONS]\n\
\n\
ARGS:\n\
    <FOLDER>    Directory to benchmark. The folder is only read for the FS\n\
                phase; the DB phase writes a temporary DB *inside* it (cleaned\n\
                up afterwards), so point it at the volume you want to test.\n\
\n\
OPTIONS:\n\
    --no-fs              Skip the filesystem walk/stat benchmark\n\
    --no-db              Skip the SQLite benchmark\n\
    --max-stats <N>      Cap the per-file stat pass at N files (default 200000)\n\
    --db-rows <N>        Rows to bulk-insert in the DB phase (default 20000)\n\
    --db-queries <N>     Random point queries to run (default 5000)\n\
    --db-small-txns <N>  Single-row commits to run, the fsync-heavy case\n\
                         (default 500)\n\
    -h, --help           Print this help\n\
\n\
Run it once on a fast local disk and once on the VHD, then compare."
    );
}

fn parse_args() -> Result<Args, String> {
    let mut target: Option<PathBuf> = None;
    let mut run_fs = true;
    let mut run_db = true;
    let mut max_stats = 200_000usize;
    let mut db_rows = 20_000usize;
    let mut db_queries = 5_000usize;
    let mut db_small_txns = 500usize;

    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            "--no-fs" => run_fs = false,
            "--no-db" => run_db = false,
            "--max-stats" => max_stats = parse_num(&mut it, "--max-stats")?,
            "--db-rows" => db_rows = parse_num(&mut it, "--db-rows")?,
            "--db-queries" => db_queries = parse_num(&mut it, "--db-queries")?,
            "--db-small-txns" => db_small_txns = parse_num(&mut it, "--db-small-txns")?,
            s if s.starts_with('-') => return Err(format!("unknown flag: {s}")),
            s => {
                if target.is_some() {
                    return Err(format!("unexpected extra argument: {s}"));
                }
                target = Some(PathBuf::from(s));
            }
        }
    }

    let target = target.ok_or_else(|| "missing <FOLDER> argument".to_string())?;
    if !target.is_dir() {
        return Err(format!("not a directory: {}", target.display()));
    }
    Ok(Args {
        target,
        run_fs,
        run_db,
        max_stats,
        db_rows,
        db_queries,
        db_small_txns,
    })
}

fn parse_num(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<usize, String> {
    it.next()
        .ok_or_else(|| format!("{flag} needs a value"))?
        .parse()
        .map_err(|_| format!("{flag} value must be a non-negative integer"))
}

// ── Latency stats ───────────────────────────────────────────────────

/// Collected per-operation latencies, in microseconds.
struct Lat {
    micros: Vec<u128>,
}

impl Lat {
    fn new(cap: usize) -> Self {
        Lat {
            micros: Vec::with_capacity(cap),
        }
    }
    fn push(&mut self, d: std::time::Duration) {
        self.micros.push(d.as_micros());
    }
    fn pct(sorted: &[u128], p: f64) -> u128 {
        if sorted.is_empty() {
            return 0;
        }
        let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
        sorted[idx.min(sorted.len() - 1)]
    }
    /// Print min / mean / p50 / p90 / p99 / max in microseconds.
    fn report(&self, label: &str) {
        if self.micros.is_empty() {
            println!("  {label}: (no samples)");
            return;
        }
        let mut s = self.micros.clone();
        s.sort_unstable();
        let n = s.len();
        let sum: u128 = s.iter().sum();
        let mean = sum as f64 / n as f64;
        println!(
            "  {label}: n={n}  min={}  mean={:.1}  p50={}  p90={}  p99={}  max={}  (us)",
            s[0],
            mean,
            Self::pct(&s, 0.50),
            Self::pct(&s, 0.90),
            Self::pct(&s, 0.99),
            s[n - 1],
        );
    }
    fn mean_us(&self) -> f64 {
        if self.micros.is_empty() {
            return 0.0;
        }
        self.micros.iter().sum::<u128>() as f64 / self.micros.len() as f64
    }
}

fn secs(d: std::time::Duration) -> f64 {
    d.as_secs_f64()
}

// ── Filesystem benchmark ────────────────────────────────────────────

fn bench_fs(target: &Path, max_stats: usize) {
    section("FILESYSTEM");

    // Phase 1: walk the tree once, collecting file paths. walkdir stats every
    // entry to classify file-vs-dir — this is the unavoidable cost of any
    // directory traversal, and the first place a VHD shows its latency.
    let t0 = Instant::now();
    let mut files: Vec<PathBuf> = Vec::new();
    let mut dirs = 0usize;
    for entry in WalkDir::new(target).follow_links(false) {
        match entry {
            Ok(e) => {
                let ft = e.file_type();
                if ft.is_dir() {
                    dirs += 1;
                } else if ft.is_file() {
                    files.push(e.into_path());
                }
            }
            Err(_) => continue,
        }
    }
    let walk_elapsed = t0.elapsed();
    let total = files.len() + dirs;
    println!(
        "Walk: {} files + {} dirs = {} entries in {:.3}s  ({:.0} entries/s)",
        files.len(),
        dirs,
        total,
        secs(walk_elapsed),
        total as f64 / secs(walk_elapsed).max(1e-9),
    );

    if files.is_empty() {
        println!("  (no files found; skipping stat passes)");
        return;
    }

    let sample: &[PathBuf] = if files.len() > max_stats {
        println!(
            "  (capping stat pass at --max-stats={} of {} files)",
            max_stats,
            files.len()
        );
        &files[..max_stats]
    } else {
        &files[..]
    };

    // Phase 2: the double-stat. This mirrors tokensave's find_stale_files,
    // which re-runs fs::metadata() + .modified() on every file *after* the
    // walk already touched them. Two passes reveal whether the OS metadata
    // cache helps (it usually does on local disks, often not on a VHD).
    let stat_pass = |label: &str| {
        let mut lat = Lat::new(sample.len());
        let t = Instant::now();
        let mut ok = 0usize;
        for p in sample {
            let s = Instant::now();
            if let Ok(m) = std::fs::metadata(p) {
                let _ = m.modified();
                ok += 1;
            }
            lat.push(s.elapsed());
        }
        let total = t.elapsed();
        println!(
            "{label}: {} stats in {:.3}s  ({:.0} stats/s)",
            ok,
            secs(total),
            ok as f64 / secs(total).max(1e-9),
        );
        lat.report("per-stat");
        lat.mean_us()
    };

    println!();
    let cold = stat_pass("Stat pass 1 (cold)");
    let warm = stat_pass("Stat pass 2 (warm)");

    println!();
    verdict_fs(cold, warm, sample.len(), walk_elapsed, total);
}

fn verdict_fs(cold_us: f64, warm_us: f64, n: usize, walk: std::time::Duration, entries: usize) {
    println!("FS verdict:");
    let per_walk_us = if entries > 0 {
        walk.as_micros() as f64 / entries as f64
    } else {
        0.0
    };
    println!("  ~{per_walk_us:.1} us per walked entry; ~{cold_us:.1} us per cold stat, ~{warm_us:.1} us per warm stat.");
    // tokensave does ~2*N metadata ops per staleness check (walk + re-stat).
    let projected_ms = (per_walk_us + cold_us) * n as f64 / 1000.0;
    println!(
        "  A tokensave-style staleness check (walk + re-stat of {n} files) would cost ~{projected_ms:.0} ms here."
    );
    if cold_us >= 500.0 {
        println!("  >> SLOW: sub-millisecond stats are expected on local disks. {cold_us:.0} us/stat");
        println!("     indicates per-op latency through a virtual-disk/network layer — this is");
        println!("     the dominant cost and explains the 1000x report. Move the source (or at");
        println!("     least .tokensave/) onto a local volume, or cut the redundant second stat.");
    } else if cold_us >= 50.0 {
        println!("  >> ELEVATED: stats are slower than a warm local disk but not catastrophic.");
    } else {
        println!("  >> FAST: filesystem metadata latency looks local-disk-class here.");
    }
}

// ── SQLite benchmark ────────────────────────────────────────────────

/// A pragma profile to test.
struct Profile {
    name: &'static str,
    pragmas: &'static str,
}

const PROFILE_TOKENSAVE: Profile = Profile {
    name: "WAL + mmap (tokensave today)",
    pragmas: "PRAGMA page_size = 8192;
              PRAGMA journal_mode = WAL;
              PRAGMA synchronous = NORMAL;
              PRAGMA busy_timeout = 120000;
              PRAGMA cache_size = -8192;
              PRAGMA temp_store = MEMORY;
              PRAGMA mmap_size = 268435456;",
};

const PROFILE_FALLBACK: Profile = Profile {
    name: "DELETE journal, mmap off (proposed fallback)",
    pragmas: "PRAGMA page_size = 8192;
              PRAGMA journal_mode = DELETE;
              PRAGMA synchronous = NORMAL;
              PRAGMA busy_timeout = 5000;
              PRAGMA cache_size = -8192;
              PRAGMA temp_store = MEMORY;
              PRAGMA mmap_size = 0;",
};

struct DbResult {
    bulk_insert_s: f64,
    queries_s: f64,
    small_txn_s: f64,
}

fn bench_db_profile(
    dir: &Path,
    profile: &Profile,
    rows: usize,
    queries: usize,
    small_txns: usize,
) -> Result<DbResult, String> {
    let db_path = dir.join(format!("volbench_{}.db", sanitize(profile.name)));
    cleanup_db(&db_path);

    let conn = Connection::open(&db_path).map_err(|e| format!("open: {e}"))?;
    conn.execute_batch(profile.pragmas)
        .map_err(|e| format!("pragmas: {e}"))?;
    conn.execute_batch(
        "CREATE TABLE nodes (
            id INTEGER PRIMARY KEY,
            file TEXT NOT NULL,
            name TEXT NOT NULL,
            kind TEXT NOT NULL,
            start_line INTEGER,
            body TEXT
         );
         CREATE INDEX idx_nodes_name ON nodes(name);",
    )
    .map_err(|e| format!("schema: {e}"))?;

    // A representative ~240-byte body so pages actually turn over.
    let body = "x".repeat(240);

    // 1) Bulk insert in a single transaction (WAL append throughput).
    let t = Instant::now();
    conn.execute_batch("BEGIN;").map_err(|e| e.to_string())?;
    {
        let mut stmt = conn
            .prepare("INSERT INTO nodes (id, file, name, kind, start_line, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")
            .map_err(|e| e.to_string())?;
        for i in 0..rows {
            stmt.execute(params![
                i as i64,
                format!("src/file_{}.rs", i % 500),
                format!("symbol_{i}"),
                "function",
                (i % 1000) as i64,
                body,
            ])
            .map_err(|e| e.to_string())?;
        }
    }
    conn.execute_batch("COMMIT;").map_err(|e| e.to_string())?;
    let bulk_insert_s = secs(t.elapsed());

    // 2) Random point queries (mmap / page-cache read latency).
    let mut rng = Xorshift::new(0x9E3779B97F4A7C15);
    let t = Instant::now();
    let mut hits = 0u64;
    for _ in 0..queries {
        let id = (rng.next() as usize % rows.max(1)) as i64;
        let r: Result<String, _> =
            conn.query_row("SELECT body FROM nodes WHERE id = ?1", params![id], |row| {
                row.get(0)
            });
        if r.is_ok() {
            hits += 1;
        }
    }
    let queries_s = secs(t.elapsed());
    let _ = hits;

    // 3) Many single-row transactions (per-commit fsync / journal churn —
    //    the most volume-latency-sensitive pattern).
    let t = Instant::now();
    for i in 0..small_txns {
        let id = (i % rows.max(1)) as i64;
        conn.execute(
            "UPDATE nodes SET start_line = start_line + 1 WHERE id = ?1",
            params![id],
        )
        .map_err(|e| e.to_string())?;
    }
    let small_txn_s = secs(t.elapsed());

    // Checkpoint + close before measuring nothing else; then clean up.
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    drop(conn);
    cleanup_db(&db_path);

    Ok(DbResult {
        bulk_insert_s,
        queries_s,
        small_txn_s,
    })
}

fn bench_db(dir: &Path, rows: usize, queries: usize, small_txns: usize) {
    section("SQLITE (database created on the target volume)");
    println!(
        "Config: {rows} rows bulk insert, {queries} point queries, {small_txns} single-row commits.\n"
    );

    let mut results: Vec<(&'static str, DbResult)> = Vec::new();
    for p in [&PROFILE_TOKENSAVE, &PROFILE_FALLBACK] {
        print!("Running profile: {} ... ", p.name);
        let _ = std::io::Write::flush(&mut std::io::stdout());
        match bench_db_profile(dir, p, rows, queries, small_txns) {
            Ok(r) => {
                println!("done");
                results.push((p.name, r));
            }
            Err(e) => println!("FAILED: {e}"),
        }
    }

    println!();
    println!(
        "{:<46} {:>14} {:>14} {:>16}",
        "profile", "bulk insert", "5k queries", "single-row txns"
    );
    println!("{}", "-".repeat(92));
    for (name, r) in &results {
        println!(
            "{:<46} {:>12.3}s {:>12.3}s {:>14.3}s",
            name, r.bulk_insert_s, r.queries_s, r.small_txn_s
        );
    }

    println!();
    if results.len() == 2 {
        let wal = &results[0].1;
        let del = &results[1].1;
        verdict_db(wal, del);
    }
}

fn verdict_db(wal: &DbResult, del: &DbResult) {
    println!("SQLite verdict:");
    let ratio = |a: f64, b: f64| if b > 0.0 { a / b } else { 0.0 };
    let q_ratio = ratio(wal.queries_s, del.queries_s);
    let t_ratio = ratio(wal.small_txn_s, del.small_txn_s);
    println!(
        "  WAL+mmap vs DELETE/mmap-off:  queries {q_ratio:.1}x,  single-row txns {t_ratio:.1}x."
    );
    if q_ratio >= 2.0 || t_ratio >= 2.0 {
        println!("  >> WAL/mmap is materially slower on this volume. The fallback profile");
        println!("     (journal_mode=DELETE, mmap_size=0) should be used here. This points at");
        println!("     the SQLite layer, not (only) the file walk.");
    } else {
        println!("  >> WAL/mmap is not the dominant cost on this volume; the slowdown is");
        println!("     more likely the filesystem-walk path above.");
    }
}

// ── helpers ─────────────────────────────────────────────────────────

/// Tiny deterministic PRNG (no rand dependency, reproducible across runs).
struct Xorshift(u64);
impl Xorshift {
    fn new(seed: u64) -> Self {
        Xorshift(seed | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn cleanup_db(db_path: &Path) {
    for ext in ["", "-wal", "-shm", "-journal"] {
        let p = if ext.is_empty() {
            db_path.to_path_buf()
        } else {
            PathBuf::from(format!("{}{}", db_path.display(), ext))
        };
        let _ = std::fs::remove_file(p);
    }
}

fn section(title: &str) {
    println!("\n========== {title} ==========");
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}\n");
            print_usage();
            std::process::exit(2);
        }
    };

    println!("volbench {} — target: {}", env!("CARGO_PKG_VERSION"), args.target.display());

    if args.run_fs {
        bench_fs(&args.target, args.max_stats);
    }
    if args.run_db {
        bench_db(&args.target, args.db_rows, args.db_queries, args.db_small_txns);
    }

    println!("\nDone. Run the same command on a local-disk copy of the folder to compare.");
}
