// Metadata-extraction cost benchmark. Runs one extraction approach over a corpus
// (one path per line) and prints wall time, CPU time and peak RSS.
//
// Modes:
//   baseline   <list>            stat only (name/size/mtime) — current daemon work
//   rust-type  <list>            magic sniff via `infer` (read head, match magic)
//   rust-audio <list>            audio container/tags via `lofty`
//   rust-image <list>            image dimensions via `imagesize` (header only)
//   rust-exif  <list>            full EXIF via `kamadak-exif`
//   rust-pdf   <list>            pdf structure/info via `lopdf`
//   native     <list> -- CMD..   spawn CMD per file ({} = path); measure children
//
// Measurement: wall = monotonic clock around the loop. For rust-* modes CPU is
// RUSAGE_SELF and peak RSS is /proc/self/status VmHWM (steady in-process cost).
// For native, CPU is RUSAGE_CHILDREN and RSS is ru_maxrss (peak of one spawned
// child — the transient per-file footprint). Both are the honest cost for a
// long-lived daemon: native pays a spawn + child RSS spike per file, the Rust
// path pays a persistent in-process footprint.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

fn getrusage(who: libc::c_int) -> libc::rusage {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe {
        libc::getrusage(who, &mut ru);
    }
    ru
}

fn cpu_ms(ru: &libc::rusage) -> f64 {
    let t = |tv: libc::timeval| tv.tv_sec as f64 * 1000.0 + tv.tv_usec as f64 / 1000.0;
    t(ru.ru_utime) + t(ru.ru_stime)
}

/// Peak resident set of THIS process, KB (steady cost of the rust-* paths).
fn vm_hwm_kb() -> i64 {
    let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            return rest
                .split_whitespace()
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
        }
    }
    0
}

fn read_list(path: &str) -> Vec<PathBuf> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn run_rust(files: &[PathBuf], extract: impl Fn(&Path) -> bool) -> (usize, usize) {
    let (mut ok, mut err) = (0, 0);
    for f in files {
        let done = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| extract(f)));
        match done {
            Ok(true) => ok += 1,
            _ => err += 1,
        }
    }
    (ok, err)
}

fn run_native(files: &[PathBuf], template: &[String]) -> (usize, usize) {
    let (mut ok, mut err) = (0, 0);
    for f in files {
        let args: Vec<String> = template
            .iter()
            .map(|a| {
                if a == "{}" {
                    f.to_string_lossy().into_owned()
                } else {
                    a.clone()
                }
            })
            .collect();
        // bigagents: app-local-subprocess - benchmark measures native spawn cost.
        let status = Command::new(&args[0])
            .args(&args[1..])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .stdin(Stdio::null())
            .status();
        match status {
            Ok(s) if s.success() => ok += 1,
            _ => err += 1,
        }
    }
    (ok, err)
}

// --- rust extractors: parse only; success = the parse succeeded. ---

fn x_baseline(p: &Path) -> bool {
    fs::metadata(p)
        .map(|m| m.len() > 0 || m.len() == 0)
        .unwrap_or(false)
}

fn x_audio(p: &Path) -> bool {
    lofty::read_from_path(p).is_ok()
}

fn x_image(p: &Path) -> bool {
    imagesize::size(p).is_ok()
}

fn x_exif(p: &Path) -> bool {
    let Ok(file) = fs::File::open(p) else {
        return false;
    };
    let mut reader = std::io::BufReader::new(&file);
    exif::Reader::new().read_from_container(&mut reader).is_ok()
}

// Broad pure-Rust content type detection (≈hundreds of formats).
fn x_type_ff(p: &Path) -> bool {
    match file_format::FileFormat::from_file(p) {
        // octet-stream is the "unrecognised" fallback — count it as a miss.
        Ok(f) => f.media_type() != "application/octet-stream",
        Err(_) => false,
    }
}

// Round 2: one safe (no-unsafe) demuxer for audio + mkv/webm/mp4. Success =
// container header probed and a track found (duration/tags are header-resident).
fn x_av_symphonia(p: &Path) -> bool {
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;
    let Ok(file) = fs::File::open(p) else {
        return false;
    };
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    match symphonia::default::get_probe().format(
        &hint,
        mss,
        &FormatOptions::default(),
        &MetadataOptions::default(),
    ) {
        Ok(probed) => probed.format.default_track().is_some(),
        Err(_) => false,
    }
}

fn main() {
    std::panic::set_hook(Box::new(|_| {})); // silence per-file parser panics
    let argv: Vec<String> = std::env::args().collect();
    if argv.len() < 3 {
        eprintln!("usage: metadata-bench <mode> <list> [-- CMD with {{}}]");
        std::process::exit(2);
    }
    let mode = argv[1].as_str();
    let files = read_list(&argv[2]);
    if files.is_empty() {
        eprintln!("metadata-bench: empty corpus {}", argv[2]);
        std::process::exit(2);
    }

    let is_native = mode == "native";
    let who = if is_native {
        libc::RUSAGE_CHILDREN
    } else {
        libc::RUSAGE_SELF
    };
    let ru0 = getrusage(who);
    let t0 = Instant::now();

    let (ok, err) = match mode {
        "baseline" => run_rust(&files, x_baseline),
        "rust-audio" => run_rust(&files, x_audio),
        "rust-image" => run_rust(&files, x_image),
        "rust-exif" => run_rust(&files, x_exif),
        "rust-type-ff" => run_rust(&files, x_type_ff),
        "rust-av" => run_rust(&files, x_av_symphonia),
        "native" => {
            let sep = argv
                .iter()
                .position(|a| a == "--")
                .expect("native needs -- CMD");
            run_native(&files, &argv[sep + 1..])
        }
        other => {
            eprintln!("unknown mode {other}");
            std::process::exit(2);
        }
    };

    let wall_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let ru1 = getrusage(who);
    let cpu = cpu_ms(&ru1) - cpu_ms(&ru0);
    let n = files.len();
    let rss_kb = if is_native {
        ru1.ru_maxrss
    } else {
        vm_hwm_kb()
    };

    println!(
        "MODE={mode} FILES={n} OK={ok} ERR={err} \
         WALL_MS={wall_ms:.1} WALL_PER_FILE_MS={:.3} CPU_MS={cpu:.1} PEAK_RSS_KB={rss_kb}",
        wall_ms / n as f64
    );
}
