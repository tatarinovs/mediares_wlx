//! Recursive profiler for audio fingerprinting, PCM hashing, and WDX prefetch.
//!
//! ```text
//! cargo run --release -p mediares_core --features audio-decode,tags,profile-stats --example audio_profile -- "D:\MP3"
//! cargo run --release -p mediares_core --features audio-decode,tags,profile-stats --example audio_profile -- "D:\MP3" --mode wdx
//! ```

use std::collections::BTreeMap;
use std::ffi::{c_char, c_void, CStr};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use mediares_core::audio_fingerprint::{
    analyze_audio_profiled, fast_fingerprint_audio_profiled, AudioError, AudioProfile, FastProfile,
};
use mediares_core::cache::stats;
use mediares_core::probe::{probe_file, MediaType};
use mediares_core::tc_api::{FT_FIELDEMPTY, FT_NOMOREFIELDS, FT_STRINGW};
use mediares_core::wdx_api::{
    content_get_supported_field, content_get_value_w, content_plugin_unloading,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Phases,
    Wdx,
}

struct Args {
    root: PathBuf,
    log_path: PathBuf,
    mode: Mode,
    threads: usize,
    limit: Option<usize>,
    run_fast: bool,
    fast_only: bool,
}

fn parse_args() -> Args {
    let mut args = std::env::args().skip(1);
    let mut root = None;
    let mut log_path = None;
    let mut mode = Mode::Phases;
    let mut threads = 1usize;
    let mut limit = None;
    let mut run_fast = true;
    let mut fast_only = false;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--mode" => {
                mode = match args.next().expect("--mode requires phases or wdx").as_str() {
                    "phases" => Mode::Phases,
                    "wdx" => Mode::Wdx,
                    other => panic!("Unknown --mode: {other} (use phases or wdx)"),
                };
            }
            "--log" => {
                log_path = Some(PathBuf::from(args.next().expect("--log requires a path")));
            }
            "--threads" => {
                threads = args
                    .next()
                    .expect("--threads requires a number")
                    .parse::<usize>()
                    .expect("invalid --threads value")
                    .max(1);
            }
            "--limit" => {
                limit = Some(
                    args.next()
                        .expect("--limit requires a number")
                        .parse::<usize>()
                        .expect("invalid --limit value"),
                );
            }
            "--no-fast" => {
                run_fast = false;
            }
            "--fast-only" => {
                fast_only = true;
            }
            "--help" | "-h" => {
                eprintln!(
                    "Usage: audio_profile <dir_or_file> [--mode phases|wdx] [--log <path>] [--threads <N>] [--limit <N>] [--no-fast] [--fast-only]"
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

    let default_log = match mode {
        Mode::Phases => PathBuf::from("target/audio_profile.tsv"),
        Mode::Wdx => PathBuf::from("target/audio_profile_wdx.tsv"),
    };

    Args {
        root: root.expect("Pass a directory or audio file path, e.g. \"D:\\MP3\""),
        log_path: log_path.unwrap_or(default_log),
        mode,
        threads,
        limit,
        run_fast,
        fast_only,
    }
}

fn collect_audio_files(root: &Path, out: &mut Vec<PathBuf>) {
    if root.is_file() {
        if probe_file(root) == MediaType::Audio {
            out.push(root.to_path_buf());
        }
        return;
    }
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let mut dirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() {
            dirs.push(path);
        } else if ft.is_file() && probe_file(&path) == MediaType::Audio {
            out.push(path);
        }
    }
    dirs.sort();
    for dir in dirs {
        collect_audio_files(&dir, out);
    }
}

#[derive(Default)]
struct ExtStats {
    count: usize,
    ok: usize,
    silent: usize,
    unsupported: usize,
    invalid: usize,
    bytes: u64,
    total_us: u64,
    open_us: u64,
    decode_us: u64,
    math_us: u64,
    fast_us: u64,
    mf_count: usize,
    fast_seek: usize,
    fast_short: usize,
    fast_fallback: usize,
    fast_err: usize,
}

struct SlowFile {
    total_ms: f64,
    decode_ms: f64,
    math_ms: f64,
    open_ms: f64,
    backend: &'static str,
    status: &'static str,
    path: String,
}

struct Record {
    status: &'static str,
    ext: String,
    size: u64,
    duration_sec: u32,
    prof: AudioProfile,
    total_us: u64,
    fast_prof: FastProfile,
    fast_us: u64,
    fast_fp: String,
    fp: String,
    pcm_hash: String,
    path: String,
}

#[derive(Default)]
struct Summary {
    done: usize,
    bytes: u64,
    ok: usize,
    silent: usize,
    unsupported: usize,
    invalid: usize,
    open_us: u64,
    decode_us: u64,
    math_us: u64,
    total_us: u64,
    fast_us: u64,
    symphonia: usize,
    mf: usize,
    fast_seek: usize,
    fast_short: usize,
    fast_fallback: usize,
    fast_err: usize,
    by_ext: BTreeMap<String, ExtStats>,
    slowest: Vec<SlowFile>,
    errors: Vec<(String, &'static str)>,
}

impl Summary {
    fn add(&mut self, r: &Record) {
        self.done += 1;
        self.bytes += r.size;
        self.open_us += r.prof.open_us;
        self.decode_us += r.prof.decode_us;
        self.math_us += r.prof.math_us;
        self.total_us += r.total_us;
        self.fast_us += r.fast_us;

        match r.status {
            "ok" => self.ok += 1,
            "silent" => self.silent += 1,
            "unsupported" => {
                self.unsupported += 1;
                if self.errors.len() < 100 {
                    self.errors.push((r.path.clone(), r.status));
                }
            }
            "invalid" => {
                self.invalid += 1;
                if self.errors.len() < 100 {
                    self.errors.push((r.path.clone(), r.status));
                }
            }
            _ => {}
        }

        match r.prof.backend {
            "symphonia" => self.symphonia += 1,
            "mf" => self.mf += 1,
            _ => {}
        }

        match r.fast_prof.mode {
            "seek" => self.fast_seek += 1,
            "short" => self.fast_short += 1,
            "fallback_seq" => self.fast_fallback += 1,
            "error" => self.fast_err += 1,
            _ => {}
        }

        let ext = self.by_ext.entry(r.ext.clone()).or_default();
        ext.count += 1;
        ext.bytes += r.size;
        ext.total_us += r.total_us;
        ext.open_us += r.prof.open_us;
        ext.decode_us += r.prof.decode_us;
        ext.math_us += r.prof.math_us;
        ext.fast_us += r.fast_us;
        match r.status {
            "ok" => ext.ok += 1,
            "silent" => ext.silent += 1,
            "unsupported" => ext.unsupported += 1,
            "invalid" => ext.invalid += 1,
            _ => {}
        }
        if r.prof.backend == "mf" {
            ext.mf_count += 1;
        }
        match r.fast_prof.mode {
            "seek" => ext.fast_seek += 1,
            "short" => ext.fast_short += 1,
            "fallback_seq" => ext.fast_fallback += 1,
            "error" => ext.fast_err += 1,
            _ => {}
        }

        let item = SlowFile {
            total_ms: r.total_us as f64 / 1000.0,
            decode_ms: r.prof.decode_us as f64 / 1000.0,
            math_ms: r.prof.math_us as f64 / 1000.0,
            open_ms: r.prof.open_us as f64 / 1000.0,
            backend: r.prof.backend,
            status: r.status,
            path: r.path.clone(),
        };
        if self.slowest.len() < 20 || item.total_ms > self.slowest.last().unwrap().total_ms {
            self.slowest.push(item);
            self.slowest
                .sort_by(|a, b| b.total_ms.total_cmp(&a.total_ms));
            self.slowest.truncate(20);
        }
    }

    fn format_report(&self, wall: Duration, threads: usize, run_fast: bool) -> String {
        let mut out = String::new();
        let wall_s = wall.as_secs_f64().max(0.001);
        let mib = self.bytes as f64 / (1024.0 * 1024.0);
        let cpu_total_s = (self.total_us as f64) / 1_000_000.0;
        let open_s = (self.open_us as f64) / 1_000_000.0;
        let decode_s = (self.decode_us as f64) / 1_000_000.0;
        let math_s = (self.math_us as f64) / 1_000_000.0;
        let other_s = (cpu_total_s - open_s - decode_s - math_s).max(0.0);
        let pct = |x: f64| {
            if cpu_total_s > 0.0 {
                x * 100.0 / cpu_total_s
            } else {
                0.0
            }
        };

        use std::fmt::Write;
        let _ = writeln!(out, "\n=== AUDIO PROFILE SUMMARY ===");
        let _ = writeln!(
            out,
            "Files: {} (ok={}, silent={}, unsupported={}, invalid={}) | Size: {:.1} MiB | Threads: {}",
            self.done, self.ok, self.silent, self.unsupported, self.invalid, mib, threads
        );
        let _ = writeln!(
            out,
            "Wall time: {:.2} s | Throughput: {:.2} files/s, {:.2} MiB/s",
            wall_s,
            self.done as f64 / wall_s,
            mib / wall_s
        );
        let _ = writeln!(
            out,
            "Sequential CPU sum: {:.2} s -> open={:.2}s ({:.1}%), decode={:.2}s ({:.1}%), math={:.2}s ({:.1}%), other={:.2}s ({:.1}%)",
            cpu_total_s,
            open_s, pct(open_s),
            decode_s, pct(decode_s),
            math_s, pct(math_s),
            other_s, pct(other_s),
        );
        let _ = writeln!(
            out,
            "Backends: symphonia={}, mf={}",
            self.symphonia, self.mf
        );
        if run_fast {
            let _ = writeln!(
                out,
                "Fast fingerprint ({:.2} s sum): seek={}, short={}, fallback_seq={}, error={}",
                self.fast_us as f64 / 1_000_000.0,
                self.fast_seek,
                self.fast_short,
                self.fast_fallback,
                self.fast_err
            );
        }

        let _ = writeln!(out, "\nBy extension:");
        let _ = writeln!(
            out,
            "  {:<8} {:>6} {:>9} {:>8} {:>7} {:>7} {:>7} {:>5} {:>8} {:>10}",
            "ext", "count", "MiB", "avg_ms", "open%", "dec%", "math%", "mf", "fast_ms", "fast_fb"
        );
        for (ext, s) in &self.by_ext {
            let t = (s.total_us as f64).max(1.0);
            let _ = writeln!(
                out,
                "  {:<8} {:>6} {:>9.1} {:>8.1} {:>6.1}% {:>6.1}% {:>6.1}% {:>5} {:>8.1} {:>10}",
                ext,
                s.count,
                s.bytes as f64 / (1024.0 * 1024.0),
                (s.total_us as f64 / 1000.0) / s.count.max(1) as f64,
                s.open_us as f64 * 100.0 / t,
                s.decode_us as f64 * 100.0 / t,
                s.math_us as f64 * 100.0 / t,
                s.mf_count,
                (s.fast_us as f64 / 1000.0) / s.count.max(1) as f64,
                s.fast_fallback,
            );
        }

        let _ = writeln!(out, "\nTop {} slowest files:", self.slowest.len());
        for s in &self.slowest {
            let _ = writeln!(
                out,
                "  {:>8.1} ms (open={:>5.1} dec={:>7.1} math={:>6.1}) [{} / {}] {}",
                s.total_ms, s.open_ms, s.decode_ms, s.math_ms, s.backend, s.status, s.path
            );
        }

        if !self.errors.is_empty() {
            let _ = writeln!(out, "\nSample errors (up to 100):");
            for (p, st) in &self.errors {
                let _ = writeln!(out, "  [{}] {}", st, p);
            }
        }
        out
    }
}

fn process_one(path: &Path, run_fast: bool) -> Record {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);

    let t0 = Instant::now();
    let (res, prof) = analyze_audio_profiled(path, &|| false);
    let total_us = t0.elapsed().as_micros() as u64;

    let (status, duration_sec, fp, pcm_hash) = match res {
        Ok(a) => (
            if a.fingerprint.is_some() {
                "ok"
            } else {
                "silent"
            },
            a.duration_sec,
            a.fingerprint.unwrap_or_else(|| "-".into()),
            a.pcm_hash,
        ),
        Err(AudioError::Unsupported) => ("unsupported", 0, "-".into(), "-".into()),
        Err(AudioError::Invalid) => ("invalid", 0, "-".into(), "-".into()),
        Err(AudioError::Cancelled) => ("cancelled", 0, "-".into(), "-".into()),
    };

    let (fast_us, fast_prof, fast_fp) = if run_fast {
        let t1 = Instant::now();
        let (f_res, f_prof) = fast_fingerprint_audio_profiled(path, &|| false);
        let f_us = t1.elapsed().as_micros() as u64;
        let f_fp = match f_res {
            Ok(f) => f.fingerprint.unwrap_or_else(|| "-".into()),
            Err(_) => "-".into(),
        };
        (f_us, f_prof, f_fp)
    } else {
        (
            0,
            FastProfile {
                backend: "-",
                mode: "-",
            },
            "-".into(),
        )
    };

    Record {
        status,
        ext,
        size,
        duration_sec,
        prof,
        total_us,
        fast_prof,
        fast_us,
        fast_fp,
        fp,
        pcm_hash,
        path: path.display().to_string(),
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

use std::ffi::c_int;

fn run_wdx_mode(args: &Args, files: &[PathBuf]) {
    let db_path = std::env::current_dir().unwrap().join(format!(
        "target/audio_profile_wdx_{}.sqlite3",
        std::process::id()
    ));
    let _ = fs::remove_file(&db_path);
    std::env::set_var("MEDIARES_CONTENT_CACHE", &db_path);

    let field_name = if args.fast_only {
        "Audio_Fast_Fingerprint"
    } else {
        "Audio_Fingerprint"
    };
    let field_idx = find_field_index(field_name);
    let total = files.len();

    eprintln!(
        "WDX mode: field={field_name} (index={field_idx}), temp_db={}",
        db_path.display()
    );

    let file = File::create(&args.log_path).expect("cannot create log file");
    let mut w = BufWriter::new(file);
    writeln!(w, "idx\tcall_ms\tkind\tvalue\tpath").unwrap();

    let started = Instant::now();
    let mut last_print = Instant::now();
    let mut ok = 0usize;
    let mut empty = 0usize;
    let mut err = 0usize;
    let mut total_bytes = 0u64;
    let mut slowest: Vec<(f64, String)> = Vec::new();
    let mut buf = [0u16; 512];

    for (i, path) in files.iter().enumerate() {
        total_bytes += fs::metadata(path).map(|m| m.len()).unwrap_or(0);
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
        let val = if kind == FT_STRINGW {
            ok += 1;
            let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            String::from_utf16_lossy(&buf[..len])
        } else if kind == FT_FIELDEMPTY {
            empty += 1;
            "-".into()
        } else {
            err += 1;
            format!("kind={kind}")
        };

        let _ = writeln!(w, "{i}\t{call_ms:.3}\t{kind}\t{val}\t{}", path.display());
        if i % 25 == 0 {
            let _ = w.flush();
        }

        if slowest.len() < 20 || call_ms > slowest.last().unwrap().0 {
            slowest.push((call_ms, path.display().to_string()));
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
        "Field: {field_name} | Files: {total} (ok={ok}, empty={empty}, err={err}) | Size: {:.1} MiB",
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
        "RAM hits: fg={} (prefetched ahead of TC!), bg={}",
        s.ram_hits_fg, s.ram_hits_bg
    );
    let _ = writeln!(
        report,
        "Disk hits: fg={}, bg={}",
        s.disk_hits_fg, s.disk_hits_bg
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
    let _ = writeln!(report, "\nTop {} slowest foreground calls:", slowest.len());
    for (ms, p) in &slowest {
        let _ = writeln!(report, "  {:>8.1} ms  {p}", ms);
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

fn main() {
    let args = parse_args();
    if let Some(parent) = args.log_path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = fs::create_dir_all(parent);
        }
    }

    eprintln!("Scanning {} ...", args.root.display());
    let mut files = Vec::new();
    collect_audio_files(&args.root, &mut files);
    if let Some(limit) = args.limit {
        files.truncate(limit);
    }
    let total = files.len();
    if total == 0 {
        eprintln!("No audio files found.");
        return;
    }

    if args.mode == Mode::Wdx {
        run_wdx_mode(&args, &files);
        return;
    }

    eprintln!(
        "Found {} audio files. Logging to {} (threads={}, fast={})",
        total,
        args.log_path.display(),
        args.threads,
        args.run_fast
    );

    let file = File::create(&args.log_path).expect("cannot create log file");
    let writer = Mutex::new(BufWriter::new(file));
    {
        let mut w = writer.lock().unwrap();
        writeln!(
            w,
            "status\text\tsize_bytes\tduration_sec\tsample_rate\tchannels\tchunks\tbackend\topen_ms\tdecode_ms\tmath_ms\ttotal_ms\tfast_backend\tfast_mode\tfast_ms\tfingerprint\tfast_fingerprint\tpcm_hash\tpath"
        )
        .unwrap();
    }

    let summary = Mutex::new(Summary::default());
    let next_idx = AtomicUsize::new(0);
    let started = Instant::now();
    let last_print = Mutex::new(Instant::now());

    std::thread::scope(|s| {
        for _ in 0..args.threads {
            s.spawn(|| loop {
                let i = next_idx.fetch_add(1, Ordering::Relaxed);
                if i >= total {
                    break;
                }
                let rec = process_one(&files[i], args.run_fast);
                {
                    let mut w = writer.lock().unwrap();
                    let _ = writeln!(
                        w,
                        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.3}\t{:.3}\t{:.3}\t{:.3}\t{}\t{}\t{:.3}\t{}\t{}\t{}\t{}",
                        rec.status,
                        rec.ext,
                        rec.size,
                        rec.duration_sec,
                        rec.prof.sample_rate,
                        rec.prof.channels,
                        rec.prof.chunks,
                        rec.prof.backend,
                        rec.prof.open_us as f64 / 1000.0,
                        rec.prof.decode_us as f64 / 1000.0,
                        rec.prof.math_us as f64 / 1000.0,
                        rec.total_us as f64 / 1000.0,
                        rec.fast_prof.backend,
                        rec.fast_prof.mode,
                        rec.fast_us as f64 / 1000.0,
                        rec.fp,
                        rec.fast_fp,
                        rec.pcm_hash,
                        rec.path
                    );
                    if i % 25 == 0 {
                        let _ = w.flush();
                    }
                }
                let mut sum = summary.lock().unwrap();
                sum.add(&rec);
                let done = sum.done;
                if done % 100 == 0 || done == total {
                    let mut lp = last_print.lock().unwrap();
                    if done == total || lp.elapsed() >= Duration::from_secs(3) {
                        *lp = Instant::now();
                        let el = started.elapsed().as_secs_f64().max(0.001);
                        eprintln!(
                            "[{}/{} | {:.0}s | {:.1} files/s | {:.1} MiB/s] ok={} silent={} err={} | open={:.1}s dec={:.1}s math={:.1}s | fast(seek={}, short={}, fb={})",
                            done,
                            total,
                            el,
                            done as f64 / el,
                            (sum.bytes as f64 / (1024.0 * 1024.0)) / el,
                            sum.ok,
                            sum.silent,
                            sum.unsupported + sum.invalid,
                            sum.open_us as f64 / 1_000_000.0,
                            sum.decode_us as f64 / 1_000_000.0,
                            sum.math_us as f64 / 1_000_000.0,
                            sum.fast_seek,
                            sum.fast_short,
                            sum.fast_fallback,
                        );
                    }
                }
            });
        }
    });

    {
        let mut w = writer.lock().unwrap();
        let _ = w.flush();
    }

    let report =
        summary
            .lock()
            .unwrap()
            .format_report(started.elapsed(), args.threads, args.run_fast);
    println!("{report}");

    let summary_path = args.log_path.with_extension("summary.txt");
    let _ = fs::write(&summary_path, &report);
    eprintln!("Saved TSV log: {}", args.log_path.display());
    eprintln!("Saved summary: {}", summary_path.display());
}
