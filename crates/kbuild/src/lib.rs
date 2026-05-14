use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error reading {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse Makefile at {path}: {reason}")]
    ParseError { path: PathBuf, reason: String },
}

pub type Result<T> = std::result::Result<T, Error>;

/// Boolean expression over CONFIG symbols describing the conditions under
/// which a source file is included in the build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigExpr {
    /// File is compiled when this CONFIG symbol is enabled.
    Symbol(String),
    And(Box<ConfigExpr>, Box<ConfigExpr>),
    Or(Box<ConfigExpr>, Box<ConfigExpr>),
    Not(Box<ConfigExpr>),
    /// File is compiled unconditionally (e.g. `obj-y += file.o`).
    Always,
}

impl ConfigExpr {
    /// Returns all CONFIG symbol names referenced in the expression.
    pub fn symbols(&self) -> HashSet<&str> {
        todo!()
    }

    /// Evaluates the expression given a set of enabled CONFIG symbols.
    pub fn eval(&self, enabled: &HashSet<&str>) -> bool {
        todo!()
    }
}

/// The two mappings produced by parsing the kernel Makefile tree.
pub struct KbuildMaps {
    /// CONFIG symbol → set of source files (relative to kernel root) it controls.
    ///
    /// A CONFIG is in this map only when setting it to `n` would stop at least
    /// one file from being compiled.  Files that are `Always` compiled are not
    /// reachable from this map.
    config_to_files: HashMap<String, HashSet<PathBuf>>,

    /// Source file (relative to kernel root) → boolean CONFIG expression.
    ///
    /// The expression encodes the conjunction of all `obj-$(CONFIG_X)` and
    /// subdirectory conditions on the path from the root Makefile to this file.
    file_to_expr: HashMap<PathBuf, ConfigExpr>,
}

impl KbuildMaps {
    /// Parses the entire Makefile tree rooted at `kernel_src` and builds both maps.
    pub fn load(kernel_src: &Path) -> Result<Self> {
        todo!()
    }

    pub fn config_to_files(&self) -> &HashMap<String, HashSet<PathBuf>> {
        &self.config_to_files
    }

    pub fn file_to_expr(&self) -> &HashMap<PathBuf, ConfigExpr> {
        &self.file_to_expr
    }

    /// Returns the set of CONFIG symbols that, when disabled, would stop
    /// every file in `uncovered` from compiling — without affecting any
    /// file outside `uncovered`.
    ///
    /// A CONFIG is safe to disable only if its entire file set is a subset
    /// of `uncovered`.
    pub fn configs_safe_to_disable(&self, uncovered: &HashSet<PathBuf>) -> HashSet<String> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_expr_eval_symbol() {
        todo!()
    }

    #[test]
    fn config_expr_eval_and() {
        todo!()
    }

    #[test]
    fn configs_safe_to_disable_respects_shared_files() {
        todo!()
    }
}
