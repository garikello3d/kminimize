use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};

const GCOV_DATA_MAGIC: u32 = 0x67636461;
const GCOV_TAG_FUNCTION: u32 = 0x01000000;
const GCOV_TAG_COUNTER_ARCS: u32 = 0x01a10000;

// Subdirectory names used to recognise the embedded kernel source root.
const KERNEL_MARKERS: &[&str] = &["drivers", "arch", "kernel", "mm", "net", "fs"];
const MARKER_THRESHOLD: usize = 3;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error reading {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid .gcda format in {path}: {reason}")]
    InvalidFormat { path: PathBuf, reason: String },
    #[error("kernel source root not detectable under gcov directory {0}")]
    KernelRootNotFound(PathBuf),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Per-file coverage statistics derived from `.gcda` arc counters.
///
/// "Lines hit" is approximated by non-zero arc counter slots; exact
/// line counts would require the paired `.gcno` file.
#[derive(Debug, Clone)]
pub struct FileCoverage {
    /// Kernel-root-relative path of the source file (e.g. `drivers/usb/core/usb.c`).
    pub source_file: PathBuf,
    /// Functions with at least one non-zero arc counter.
    pub hit_functions: usize,
    /// Total functions in the translation unit.
    pub total_functions: usize,
    /// Non-zero arc counter slots (proxy for "lines hit").
    pub hit_arcs: usize,
    /// Total arc counter slots in the file.
    pub total_arcs: usize,
}

// ---------- internal GCDA parser ----------

struct FunctionStats {
    hit_arcs: usize,
    total_arcs: usize,
}

struct GcdaStats {
    functions: Vec<FunctionStats>,
}

fn parse_gcda_stats(path: &Path) -> Result<GcdaStats> {
    let data = std::fs::read(path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })?;

    if data.len() < 12 {
        return Err(Error::InvalidFormat {
            path: path.to_owned(),
            reason: "file too short for header".into(),
        });
    }

    // Detect byte order from the magic word.
    // GCC writes all values in host byte order; on LE hosts the magic reads
    // as 0x67636461 when interpreted as a LE u32.
    let big_endian = {
        let le = u32::from_le_bytes(data[0..4].try_into().unwrap());
        if le == GCOV_DATA_MAGIC {
            false
        } else {
            let be = u32::from_be_bytes(data[0..4].try_into().unwrap());
            if be != GCOV_DATA_MAGIC {
                return Err(Error::InvalidFormat {
                    path: path.to_owned(),
                    reason: "bad magic".into(),
                });
            }
            true
        }
    };

    let r = |p: usize| -> Option<u32> {
        let b: [u8; 4] = data.get(p..p + 4)?.try_into().ok()?;
        if big_endian {
            Some(u32::from_be_bytes(b))
        } else {
            Some(u32::from_le_bytes(b))
        }
    };

    // Skip header: magic(4) + version(4) + stamp(4) = 12 bytes
    let mut pos = 12;
    let mut functions: Vec<FunctionStats> = Vec::new();
    let mut cur_fn: Option<usize> = None;

    while pos + 8 <= data.len() {
        let tag = match r(pos) {
            Some(v) => v,
            None => break,
        };
        let length = match r(pos + 4) {
            Some(v) => v as usize,
            None => break,
        };
        pos += 8;
        let record_end = pos + length * 4;
        if record_end > data.len() {
            break; // truncated record — stop gracefully
        }

        match tag {
            GCOV_TAG_FUNCTION => {
                // All function record variants (GCC 4.7 through 13+) are handled
                // by skipping to record_end; we only need to register a new function.
                functions.push(FunctionStats { hit_arcs: 0, total_arcs: 0 });
                cur_fn = Some(functions.len() - 1);
            }
            GCOV_TAG_COUNTER_ARCS => {
                // Each counter occupies two u32 words (lo + hi of a u64).
                let n = length / 2;
                if let Some(idx) = cur_fn {
                    let func = &mut functions[idx];
                    func.total_arcs += n;
                    for i in 0..n {
                        let base = pos + i * 8;
                        let lo = r(base).unwrap_or(0) as u64;
                        let hi = r(base + 4).unwrap_or(0) as u64;
                        if lo | (hi << 32) != 0 {
                            func.hit_arcs += 1;
                        }
                    }
                }
            }
            _ => {} // summary records and unknown tags are skipped via record_end
        }

        pos = record_end;
    }

    Ok(GcdaStats { functions })
}

// ---------- public API ----------

/// Returns the set of kernel source files that have at least one coverage
/// counter > 0.
///
/// **Path invariant**: every `PathBuf` in the returned set is relative to the
/// kernel source root (e.g. `drivers/usb/core/usb.c`).  This matches the
/// convention used by `kbuild::KernelModule::files` so that callers can
/// intersect the two sets directly without further normalization.
///
/// `gcov_dir`: root of the gathered gcov directory structure (from debugfs)
/// `kernel_src`: root of the kernel source tree; used to validate that
///   detected paths actually exist in the source tree.
///
/// The gcov directory tree embeds the build machine's absolute path, e.g.:
///   <gcov_dir>/home/builduser/linux-6.12/drivers/usb/core/usb.gcda
/// The kernel build root is detected heuristically and stripped to recover
/// the kernel-relative path, which is then returned with `.gcda` replaced
/// by `.c`.
pub fn covered_files(gcov_dir: &Path, kernel_src: &Path) -> Result<HashSet<PathBuf>> {
    let kernel_root = detect_kernel_root_in_gcov(gcov_dir)?;
    let mut covered = HashSet::new();

    for entry in walkdir::WalkDir::new(gcov_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map_or(false, |x| x == "gcda"))
    {
        let gcda = entry.path();
        let rel = match gcda.strip_prefix(&kernel_root) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let src = rel.with_extension("c");
        if kernel_src.join(&src).exists() && has_coverage(gcda)? {
            covered.insert(src);
        }
    }

    Ok(covered)
}

/// Returns `true` if any arc counter in the given `.gcda` file is non-zero.
///
/// This does not require the paired `.gcno` file: it only checks whether
/// any counter fired, not which lines they correspond to.
pub fn has_coverage(gcda_path: &Path) -> Result<bool> {
    let stats = parse_gcda_stats(gcda_path)?;
    Ok(stats.functions.iter().any(|f| f.hit_arcs > 0))
}

/// Detects the kernel build root directory embedded inside the gcov tree.
///
/// Walks into `gcov_dir` (breadth-first) until it finds a directory whose
/// immediate children include at least three of the canonical kernel
/// subdirectory names (`drivers`, `arch`, `kernel`, `mm`, `net`, `fs`).
/// Returns that directory, which can be stripped from gcov paths to recover
/// kernel-relative source paths.
pub fn detect_kernel_root_in_gcov(gcov_dir: &Path) -> Result<PathBuf> {
    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    queue.push_back(gcov_dir.to_owned());

    while let Some(dir) = queue.pop_front() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue, // skip unreadable dirs
        };

        let mut subdirs: Vec<PathBuf> = Vec::new();
        let mut markers = 0usize;

        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name();
            if KERNEL_MARKERS.contains(&name.to_string_lossy().as_ref()) {
                markers += 1;
            }
            subdirs.push(entry.path());
        }

        if markers >= MARKER_THRESHOLD {
            return Ok(dir);
        }

        for sub in subdirs {
            queue.push_back(sub);
        }
    }

    Err(Error::KernelRootNotFound(gcov_dir.to_owned()))
}

/// Returns per-file coverage statistics for every `.gcda` file under
/// `gcov_dir` that has at least one hit function, sorted by source path.
///
/// Paths are kernel-root-relative (same convention as `covered_files`).
/// "Lines hit" is approximated by non-zero arc counter slots; exact line
/// counts require the paired `.gcno` files.
pub fn coverage_report(gcov_dir: &Path) -> Result<Vec<FileCoverage>> {
    let kernel_root = detect_kernel_root_in_gcov(gcov_dir)?;
    let mut report: Vec<FileCoverage> = Vec::new();

    for entry in walkdir::WalkDir::new(gcov_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map_or(false, |x| x == "gcda"))
    {
        let gcda = entry.path();
        let stats = parse_gcda_stats(gcda)?;

        let hit_functions = stats.functions.iter().filter(|f| f.hit_arcs > 0).count();
        if hit_functions == 0 {
            continue;
        }

        let rel = match gcda.strip_prefix(&kernel_root) {
            Ok(r) => r,
            Err(_) => continue,
        };

        report.push(FileCoverage {
            source_file: rel.with_extension("c"),
            hit_functions,
            total_functions: stats.functions.len(),
            hit_arcs: stats.functions.iter().map(|f| f.hit_arcs).sum(),
            total_arcs: stats.functions.iter().map(|f| f.total_arcs).sum(),
        });
    }

    report.sort_by(|a, b| a.source_file.cmp(&b.source_file));
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a minimal valid little-endian GCDA with one function and the
    /// given arc counters.
    fn make_gcda(counters: &[u64]) -> Vec<u8> {
        let mut buf: Vec<u8> = Vec::new();
        let mut w = |v: u32| buf.extend_from_slice(&v.to_le_bytes());

        // Header
        w(GCOV_DATA_MAGIC); // magic
        w(0x4138312a); // version (arbitrary)
        w(0x00000000); // stamp

        // FUNCTION record: ident + 2 checksums = 3 words
        w(GCOV_TAG_FUNCTION);
        w(3);
        w(1); // ident
        w(0); // lineno_checksum
        w(0); // cfg_checksum

        // COUNTER_ARCS record: each counter = lo(u32) + hi(u32)
        w(GCOV_TAG_COUNTER_ARCS);
        w((counters.len() * 2) as u32);
        for &c in counters {
            w(c as u32);
            w((c >> 32) as u32);
        }

        buf
    }

    #[test]
    fn has_coverage_zero_counters() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(&make_gcda(&[0, 0, 0])).unwrap();
        assert!(!has_coverage(tmp.path()).unwrap());
    }

    #[test]
    fn has_coverage_nonzero_counters() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(&make_gcda(&[0, 42, 0])).unwrap();
        assert!(has_coverage(tmp.path()).unwrap());
    }

    #[test]
    fn detect_kernel_root_finds_correct_subtree() {
        let root = tempfile::TempDir::new().unwrap();
        // Build <root>/a/b/linux/ with enough kernel marker subdirs
        let linux = root.path().join("a").join("b").join("linux");
        for dir in &["drivers", "arch", "kernel", "mm"] {
            std::fs::create_dir_all(linux.join(dir)).unwrap();
        }
        // Put a .gcda file in drivers/ so the tree is non-trivial
        let mut f = std::fs::File::create(linux.join("drivers").join("foo.gcda")).unwrap();
        f.write_all(&make_gcda(&[1])).unwrap();

        let detected = detect_kernel_root_in_gcov(root.path()).unwrap();
        assert_eq!(detected, linux);
    }

    // ---- helpers for real-data tests ----------------------------------------

    fn require_gcov_dir() -> PathBuf {
        match std::env::var("TEST_GCOV_DIR") {
            Ok(p) => PathBuf::from(p),
            Err(_) => panic!(
                "TEST_GCOV_DIR not set; run with:\n  \
                 TEST_GCOV_DIR=/home/igor/gcov-data \
                 cargo test -p gcov -- --nocapture"
            ),
        }
    }

    // Resolve a kernel-relative source path (without extension) to the
    // absolute .gcda path inside the gcov tree.
    fn gcda_for(gcov_dir: &Path, kernel_rel: &str) -> PathBuf {
        let root = detect_kernel_root_in_gcov(gcov_dir).expect("kernel root not found");
        root.join(kernel_rel).with_extension("gcda")
    }

    // ---- real-data hit-count assertions -------------------------------------
    //
    // Values measured from /home/igor/gcov-data on 2026-05-16.
    //
    // Our parser counts one GCDA FUNCTION record per function *instance*
    // (including separate records for inline expansions merged into the TU).
    // LCOV deduplicates those via the paired .gcno file, so the HTML report
    // at /tmp/kernel-cov shows lower totals:
    //
    //   File                   HTML lines    HTML funcs   our funcs
    //   kernel/fork.c          1074 / 787    101 /  74    140 / 106
    //   mm/slub.c              1903 / 722    210 /  77    209 /  95
    //   kernel/sched/core.c    2187 / 1150   305 / 144    487 / 192
    //   fs/read_write.c         578 / 273    105 /  37    103 /  42
    //   kernel/printk/printk.c 1122 / 552    109 /  57    119 /  62
    //
    // Arc counts (our "lines hit" proxy) differ from LCOV line counts because
    // arcs represent control-flow edges, not source lines.

    fn assert_hit_counts(
        gcov_dir: &Path,
        kernel_rel: &str,
        fns_total: usize,
        fns_hit: usize,
        arcs_hit: usize,
        arcs_total: usize,
    ) {
        let stats = parse_gcda_stats(&gcda_for(gcov_dir, kernel_rel))
            .unwrap_or_else(|e| panic!("{kernel_rel}: {e}"));
        let got_hit: usize = stats.functions.iter().filter(|f| f.hit_arcs > 0).count();
        let got_arcs_hit: usize = stats.functions.iter().map(|f| f.hit_arcs).sum();
        let got_arcs_total: usize = stats.functions.iter().map(|f| f.total_arcs).sum();
        assert_eq!(stats.functions.len(), fns_total, "{kernel_rel}: fns_total");
        assert_eq!(got_hit, fns_hit, "{kernel_rel}: fns_hit");
        assert_eq!(got_arcs_hit, arcs_hit, "{kernel_rel}: arcs_hit");
        assert_eq!(got_arcs_total, arcs_total, "{kernel_rel}: arcs_total");
    }

    #[test]
    fn hit_counts_real_files() {
        let gcov_dir = require_gcov_dir();
        //                         kernel-relative stem          fns_total  fns_hit  arcs_hit  arcs_total
        assert_hit_counts(&gcov_dir, "kernel/fork",               140, 106,  593, 1111);
        assert_hit_counts(&gcov_dir, "mm/slub",                   209,  95,  627, 2066);
        assert_hit_counts(&gcov_dir, "kernel/sched/core",         487, 192, 1059, 3475);
        assert_hit_counts(&gcov_dir, "fs/read_write",             103,  42,  168,  669);
        assert_hit_counts(&gcov_dir, "kernel/printk/printk",      119,  62,  400, 1186);
    }

    #[test]
    fn no_coverage_acpi_thermal_lib_c() {
        let gcov_dir = require_gcov_dir();
        let path = gcda_for(&gcov_dir, "drivers/acpi/thermal_lib");
        assert!(
            !has_coverage(&path).unwrap(),
            "drivers/acpi/thermal_lib.gcda should have all-zero counters"
        );
    }

    // ---- visualization test -------------------------------------------------

    #[test]
    fn gcov_source_report() {
        let gcov_dir = match std::env::var("TEST_GCOV_DIR") {
            Ok(p) => PathBuf::from(p),
            Err(_) => panic!(
                "TEST_GCOV_DIR is not set.\n\
                 Provide a gcov directory gathered from debugfs:\n  \
                 TEST_GCOV_DIR=/path/to/gcov \
                 cargo test -p gcov gcov_source_report -- --nocapture"
            ),
        };

        let report = coverage_report(&gcov_dir).expect("failed to build coverage report");

        if report.is_empty() {
            println!("No files with coverage found in {}", gcov_dir.display());
            return;
        }

        let file_col = report
            .iter()
            .map(|r| r.source_file.display().to_string().len())
            .max()
            .unwrap()
            .max("Source file".len());

        let sep = format!(
            "+-{:-<file_col$}-+-{:-<18}-+-{:-<20}-+",
            "", "", ""
        );

        println!("{sep}");
        println!(
            "| {:<file_col$} | {:>18} | {:>20} |",
            "Source file", "Lines hit (arcs)", "Functions hit"
        );
        println!("{sep}");
        for fc in &report {
            println!(
                "| {:<file_col$} | {:>18} | {:>20} |",
                fc.source_file.display(),
                format!("{}/{}", fc.hit_arcs, fc.total_arcs),
                format!("{}/{}", fc.hit_functions, fc.total_functions),
            );
        }
        println!("{sep}");
        println!("  {} files with coverage hits", report.len());
        println!();
        println!("  Note: 'Lines hit (arcs)' counts non-zero arc-counter slots;");
        println!("        exact per-line counts require the paired .gcno files.");
    }
}
