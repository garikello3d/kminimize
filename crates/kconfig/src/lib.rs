use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error reading {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse Kconfig at {path}: {reason}")]
    ParseError { path: PathBuf, reason: String },
}

pub type Result<T> = std::result::Result<T, Error>;

/// Kconfig symbol type as declared in `Kconfig` files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymbolKind {
    Bool,
    Tristate,
    Int,
    Hex,
    String,
}

/// A single parsed Kconfig symbol with its dependency and select metadata.
#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    /// Raw `depends on` expression (unparsed string for now).
    pub depends_on: Option<String>,
    /// Symbols this one unconditionally `select`s when enabled.
    pub selects: Vec<String>,
    /// Symbols this one `imply`s when enabled (soft select).
    pub implies: Vec<String>,
}

/// Dependency and selection graph parsed from the `Kconfig` file tree.
pub struct KconfigGraph {
    symbols: HashMap<String, Symbol>,
}

impl KconfigGraph {
    /// Parses all `Kconfig` files reachable from `<kernel_src>/Kconfig`.
    pub fn load(kernel_src: &Path) -> Result<Self> {
        todo!()
    }

    /// Returns metadata for a single CONFIG symbol, if known.
    pub fn symbol(&self, name: &str) -> Option<&Symbol> {
        self.symbols.get(name)
    }

    /// Returns all CONFIG symbols that must also be disabled when `config`
    /// is disabled, because they unconditionally `select` it.
    ///
    /// Follows the reverse-select chain transitively.
    pub fn disable_cascade(&self, config: &str) -> HashSet<String> {
        todo!()
    }

    /// Given a proposed set of configs to disable, returns the subset that
    /// would be silently re-enabled by `select` chains from configs that
    /// remain enabled.  An empty return means the proposed set is safe.
    pub fn validate_disabled(&self, disabled: &HashSet<String>) -> HashSet<String> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disable_cascade_follows_reverse_select() {
        todo!()
    }

    #[test]
    fn validate_disabled_detects_select_conflict() {
        todo!()
    }
}
