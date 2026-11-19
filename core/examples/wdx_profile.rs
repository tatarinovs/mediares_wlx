//! Universal WDX + prefetch profiler for photos, videos, and audio.
//!
//! ```text
//! cargo run --release -p mediares_core --features audio-decode,tags,profile-stats --example wdx_profile -- "D:\фотки" --field Image_pHash
//! cargo run --release -p mediares_core --features audio-decode,tags,profile-stats --example wdx_profile -- "X:\video" --field Video_Fingerprint
//! ```

use std::collections::BTreeMap;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mediares_core::cache::stats;
use mediares_core::probe::{probe_file, MediaType};
use mediares_core::tc_api::{FT_FIELDEMPTY, FT_NOMOREFIELDS, FT_NUMERIC_32, FT_STRINGW};
use mediares_core::wdx_api::{
    content_get_supported_field, content_get_value_w, content_plugin_unloading,
};

#[derive(Clone, Copy)]
enum TargetKind {
    Image,
    Video,
    Audio,
    AnyMedia,
}

impl TargetKind {
    fn from_field(field: &str) -> Self {
        if field.starts_with("Image_") || field.starts_with("Photo_") {
            Self::Image
        } else if field.starts_with("Video_") {
            Self::Video
        } else if field.starts_with("Audio_") {
            Self::Audio
        } else {
            Self::AnyMedia
        }
    }

    fn matches(self, mt: MediaType) -> bool {
        match self {
            Self::Image => mt.is_image_kind(),
            Self::Video => mt == MediaType::Video,
            Self::Audio => mt == MediaType::Audio,
            Self::AnyMedia => mt.is_image_kind() || mt.is_playable(),
        }
    }
}

struct Args {
    root: PathBuf,
    field: String,
    log_path: PathBuf,
    limit: Option<usize>,
    threads: Option<usize>,
}

fn parse_args() -> Args {
    let mut args = std::env::args().skip(1);
    let mut root = None;
    let mut field = String::from("Image_pHash");
    let mut log_path = None;
    let mut limit = None;
    let mut threads = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--field" => {
                field = args.next().expect("--field requires a field name");
            }
            "--log" => {
                log_path = Some(PathBuf::from(args.next().expect("--log requires a path")));
            }
            "--limit" => {
                limit = Some(
                    args.next()
                        .expect("--limit requires a number")
                        .parse::<usize>()
                        .expect("invalid --limit value"),
                );
            }
            "--threads" => {
                threads = Some(
                    args.next()
                        .expect("--threads requires a number")
                        .parse::<usize>()
                        .expect("invalid --threads value"),
                );
            }
            "--help" | "-h" => {
                eprintln!(
                    "Usage: wdx_profile <dir_or_file> [--field <Image_pHash|Image_Sharpness|Video_Fingerprint|Video_dHash_Mid|Audio_Fingerprint|Audio_Fast_Fingerprint>] [--threads <N>] [--log <path>] [--limit <N>]"
                );
                std::process::exit(0);
            }
            other if !other.starts_with('-') && root.is_none() => {
                root = Some(PathBuf::from(other));
            }
            other => {
                eprintln!("Unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    let default_log = PathBuf::from(format!("target/wdx_profile_{field}.tsv"));
    Args {
        root: root.expect("Pass a directory or file path, e.g. \"D:\\фотки\""),
        field,
        log_path: log_path.unwrap_or(default_log),
        limit,
        threads,
    }
}

fn collect_files(root: &Path, kind: TargetKind, out: &mut Vec<PathBuf>, limit: usize) {
    if out.len() >= limit {
        return;
    }
    if root.is_file() {
        if kind.matches(probe_file(root)) {
            out.push(root.to_path_buf());
        }
        return;
    }
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let mut dirs = Vec::new();
    for entry in entries.flatten() {
        if out.len() >= limit {
            break;
        }
        let path = entry.path();
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() {
            dirs.push(path);
        } else if ft.is_file() && kind.matches(probe_file(&path)) {
            out.push(path);
        }
    }
    dirs.sort();
    for dir in dirs {
        collect_files(&dir, kind, out, limit);
        if out.len() >= limit {
            break;
        }
    }
}

fn find_field_index(target: &str) -> c_int {
    let mut buf = [0u8; 128];
    for idx in 0.. {
        let kind = unsafe {
            content_get_supported_field(
                idx,
                buf.as_mut_ptr() as *mut c_char,
                std::ptr::null_mut(),
                buf.len() as c_int,
            )
        };
        if kind == FT_NOMOREFIELDS {
            break;
        }
        let name = unsafe { CStr::from_ptr(buf.as_ptr() as *const c_char) }.to_string_lossy();
        if name == target {
            return idx;
        }
    }
    panic!("WDX field not found: {target}");
}

#[derive(Default)]
struct ExtStats {
    count: usize,
    ok: usize,
    empty: usize,
    err: usize,
    bytes: u64,
    sum_ms: f64,
    max_ms: f64,
}

fn main() {
    let args = parse_args();
    if let Some(parent) = args.log_path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = fs::create_dir_all(parent);
        }
    }

    let target_kind = TargetKind::from_field(&args.field);
    eprintln!(
        "Scanning {} for field {} ...",
        args.root.display(),
        args.field
    );
    let mut files = Vec::new();
    collect_files(
        &args.root,
        target_kind,
        &mut files,
        args.limit.unwrap_or(usize::MAX),
    );
    if let Some(limit) = args.limit {
        files.truncate(limit);
    }
    let total = files.len();
    if total == 0 {
        eprintln!("No matching files found.");
        return;
    }

    let db_path = std::env::current_dir()
        .unwrap()
        .join(format!("target/wdx_profile_{}.sqlite3", std::process::id()));
    let _ = fs::remove_file(&db_path);
    std::env::set_var("MEDIARES_CONTENT_CACHE", &db_path);
    if let Some(n) = args.threads {
        std::env::set_var("MEDIARES_PREFETCH_THREADS", n.to_string());
    }

    let field_idx = find_field_index(&args.field);
    eprintln!(
        "Found {total} files. field={} (index={field_idx}), threads={}, temp_db={}",
        args.field,
        args.threads
            .map_or_else(|| "auto".to_string(), |n| n.to_string()),
        db_path.display()
    );

    let file = File::create(&args.log_path).expect("cannot create log file");
    let mut w = BufWriter::new(file);
    writeln!(w, "idx\text\tsize_bytes\tcall_ms\tkind\tvalue\tpath").unwrap();

    let started = Instant::now();
    let mut last_print = Instant::now();
    let mut ok = 0usize;
    let mut empty = 0usize;
    let mut err = 0usize;
    let mut total_bytes = 0u64;
    let mut by_ext: BTreeMap<String, ExtStats> = BTreeMap::new();
    let mut buckets = [0usize; 5]; // <1ms, 1-50ms, 50-250ms, 250ms-1s, >=1s
    let mut slowest: Vec<(f64, i32, String)> = Vec::new();
    let mut empty_samples: Vec<String> = Vec::new();
    let mut err_samples: Vec<(i32, String)> = Vec::new();
    let mut buf = [0u16; 512];

    for (i, path) in files.iter().enumerate() {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        total_bytes += size;

        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let t0 = Instant::now();
        let kind = unsafe {
            content_get_value_w(
                wide.as_ptr(),
                field_idx,
                buf.as_mut_ptr() as *mut c_void,
                (buf.len() * 2) as c_int,
                0,
            )
        };
        let call_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let es = by_ext.entry(ext.clone()).or_default();
        es.count += 1;
        es.bytes += size;
        es.sum_ms += call_ms;
        if call_ms > es.max_ms {
            es.max_ms = call_ms;
        }

        let val = if kind == FT_STRINGW {
            ok += 1;
            es.ok += 1;
            let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            String::from_utf16_lossy(&buf[..len])
        } else if kind == FT_NUMERIC_32 {
            ok += 1;
            es.ok += 1;
            let raw = unsafe { *(buf.as_ptr() as *const i32) };
            raw.to_string()
        } else if kind == FT_FIELDEMPTY {
            empty += 1;
            es.empty += 1;
            if empty_samples.len() < 30 {
                empty_samples.push(path.display().to_string());
            }
            "-".into()
        } else {
            err += 1;
            es.err += 1;
            if err_samples.len() < 30 {
                err_samples.push((kind, path.display().to_string()));
            }
            format!("kind={kind}")
        };

        if call_ms < 1.0 {
            buckets[0] += 1;
        } else if call_ms < 50.0 {
            buckets[1] += 1;
        } else if call_ms < 250.0 {
            buckets[2] += 1;
        } else if call_ms < 1000.0 {
            buckets[3] += 1;
        } else {
            buckets[4] += 1;
        }

        let _ = writeln!(
            w,
            "{i}\t{ext}\t{size}\t{call_ms:.3}\t{kind}\t{val}\t{}",
            path.display()
        );
        if i % 25 == 0 {
            let _ = w.flush();
        }

        if slowest.len() < 20 || call_ms > slowest.last().unwrap().0 {
            slowest.push((call_ms, kind, path.display().to_string()));
            slowest.sort_by(|a, b| b.0.total_cmp(&a.0));
            slowest.truncate(20);
        }

        let done = i + 1;
        if done == total || last_print.elapsed() >= Duration::from_secs(3) {
            last_print = Instant::now();
            let el = started.elapsed().as_secs_f64().max(0.001);
            let s = stats::snapshot();
            eprintln!(
                "[{done}/{total} | {el:.0}s | {:.1} files/s | {:.1} MiB/s] ok={ok} empty={empty} err={err} | ram(fg={},bg={}) waits={} skips={} comp(fg={},bg={}) dups={} cancel={}",
                done as f64 / el,
                (total_bytes as f64 / (1024.0 * 1024.0)) / el,
                s.ram_hits_fg,
                s.ram_hits_bg,
                s.flight_waits,
                s.flight_skips,
                s.computes_fg,
                s.computes_bg,
                s.duplicate_computes,
                s.compute_cancelled,
            );
        }
    }

    let _ = w.flush();
    content_plugin_unloading();
    let wall = started.elapsed();
    let s = stats::snapshot();

    use std::fmt::Write as _;
    let mut report = String::new();
    let _ = writeln!(report, "\n=== WDX + PREFETCH SUMMARY ===");
    let _ = writeln!(
        report,
        "Field: {} | Files: {total} (ok={ok}, empty={empty}, err={err}) | Size: {:.1} MiB",
        args.field,
        total_bytes as f64 / (1024.0 * 1024.0)
    );
    let _ = writeln!(
        report,
        "Wall time: {:.2} s | Throughput: {:.2} files/s, {:.2} MiB/s",
        wall.as_secs_f64(),
        total as f64 / wall.as_secs_f64().max(0.001),
        (total_bytes as f64 / (1024.0 * 1024.0)) / wall.as_secs_f64().max(0.001)
    );
    let _ = writeln!(
        report,
        "Latency: <1ms={}, 1-50ms={}, 50-250ms={}, 250ms-1s={}, >=1s={}",
        buckets[0], buckets[1], buckets[2], buckets[3], buckets[4]
    );
    let _ = writeln!(
        report,
        "RAM hits: fg={}, bg={} | Disk hits: fg={}, bg={}",
        s.ram_hits_fg, s.ram_hits_bg, s.disk_hits_fg, s.disk_hits_bg
    );
    let _ = writeln!(
        report,
        "Flights: fg_waits={}, bg_skips={}",
        s.flight_waits, s.flight_skips
    );
    let _ = writeln!(
        report,
        "Computes: fg={}, bg={}, cancelled={}, DUPLICATE_COMPUTES={}",
        s.computes_fg, s.computes_bg, s.compute_cancelled, s.duplicate_computes
    );
    if !s.duplicate_paths.is_empty() {
        let _ = writeln!(report, "\nDuplicate compute paths (up to 100):");
        for p in &s.duplicate_paths {
            let _ = writeln!(report, "  {}", p.display());
        }
    }

    let _ = writeln!(report, "\nBy extension:");
    let _ = writeln!(
        report,
        "  {:<8} {:>6} {:>9} {:>8} {:>8} {:>8} {:>6} {:>5}",
        "ext", "count", "MiB", "total_s", "avg_ms", "max_ms", "empty", "err"
    );
    let mut exts_sorted: Vec<_> = by_ext.into_iter().collect();
    exts_sorted.sort_by(|a, b| b.1.sum_ms.total_cmp(&a.1.sum_ms));
    for (ext, es) in exts_sorted {
        let _ = writeln!(
            report,
            "  {:<8} {:>6} {:>9.1} {:>8.1} {:>8.1} {:>8.1} {:>6} {:>5}",
            ext,
            es.count,
            es.bytes as f64 / (1024.0 * 1024.0),
            es.sum_ms / 1000.0,
            es.sum_ms / es.count.max(1) as f64,
            es.max_ms,
            es.empty,
            es.err
        );
    }

    let _ = writeln!(report, "\nTop {} slowest foreground calls:", slowest.len());
    for (ms, k, p) in &slowest {
        let _ = writeln!(report, "  {:>8.1} ms [kind={k}] {p}", ms);
    }

    if !empty_samples.is_empty() {
        let _ = writeln!(report, "\nSample empty (FT_FIELDEMPTY) files (up to 30):");
        for p in &empty_samples {
            let _ = writeln!(report, "  {p}");
        }
    }
    if !err_samples.is_empty() {
        let _ = writeln!(report, "\nSample errors (up to 30):");
        for (k, p) in &err_samples {
            let _ = writeln!(report, "  [kind={k}] {p}");
        }
    }

    println!("{report}");
    let summary_path = args.log_path.with_extension("summary.txt");
    let _ = fs::write(&summary_path, &report);
    let _ = fs::remove_file(&db_path);
    let _ = fs::remove_file(db_path.with_extension("sqlite3-wal"));
    let _ = fs::remove_file(db_path.with_extension("sqlite3-shm"));
    eprintln!("Saved TSV log: {}", args.log_path.display());
    eprintln!("Saved summary: {}", summary_path.display());
}
