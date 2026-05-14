use std::collections::HashSet;
use std::path::{Path, PathBuf};

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

/// Returns paths relative to the kernel source root for all source files
/// that have at least one coverage counter > 0.
///
/// `gcov_dir`: root of the gathered gcov directory structure (from debugfs)
/// `kernel_src`: root of the kernel source tree
///
/// The gcov directory tree embeds the build machine's absolute path, e.g.:
///   <gcov_dir>/home/builduser/linux-6.12/drivers/usb/core/usb.gcda
/// The kernel build root is detected heuristically and stripped to recover
/// relative paths that match the kernel source tree layout.
pub fn covered_files(gcov_dir: &Path, kernel_src: &Path) -> Result<HashSet<PathBuf>> {
    todo!()
}

/// Returns `true` if any arc counter in the given `.gcda` file is non-zero.
///
/// This does not require the paired `.gcno` file: it only checks whether
/// any counter fired, not which lines they correspond to.
pub fn has_coverage(gcda_path: &Path) -> Result<bool> {
    todo!()
}

/// Detects the kernel build root directory embedded inside the gcov tree.
///
/// Walks into `gcov_dir` until it finds a subtree whose immediate children
/// look like a kernel source root (`Makefile`, `drivers/`, `include/`, `arch/`).
/// Returns the path to that subtree, which can be stripped from gcov paths
/// to recover kernel-relative source paths.
pub fn detect_kernel_root_in_gcov(gcov_dir: &Path) -> Result<PathBuf> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_coverage_zero_counters() {
        todo!()
    }

    #[test]
    fn has_coverage_nonzero_counters() {
        todo!()
    }

    #[test]
    fn detect_kernel_root_finds_correct_subtree() {
        todo!()
    }
}
