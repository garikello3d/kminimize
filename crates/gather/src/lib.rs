use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse /proc/modules: {0}")]
    ProcModules(String),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// One loaded module as reported by `/proc/modules`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleEntry {
    pub name: String,
    /// Reference count field from `/proc/modules` (column 3).
    /// A value of 0 means loaded but not currently in use.
    pub use_count: u32,
}

/// A point-in-time snapshot of all loaded kernel modules.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleSnapshot {
    /// Unix timestamp (seconds) when this snapshot was taken.
    pub timestamp_secs: u64,
    pub modules: Vec<ModuleEntry>,
}

/// The full observation record written by `ktinify-gather` and later
/// consumed by the `tinify` command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleList {
    pub snapshots: Vec<ModuleSnapshot>,
}

impl ModuleList {
    pub fn new() -> Self {
        Self { snapshots: Vec::new() }
    }

    /// Deserializes a `ModuleList` from a JSON file produced by `ktinify-gather`.
    pub fn load(path: &Path) -> Result<Self> {
        todo!()
    }

    /// Serializes and writes (appends a snapshot to) the JSON file.
    pub fn save(&self, path: &Path) -> Result<()> {
        todo!()
    }

    /// Returns the set of module names that appeared with `use_count > 0`
    /// in at least one snapshot — i.e. were actively in use at some point.
    pub fn ever_used(&self) -> HashSet<String> {
        todo!()
    }

    /// Returns the set of module names that were loaded in every snapshot
    /// but always had `use_count == 0` — candidates for disabling.
    pub fn never_used(&self) -> HashSet<String> {
        todo!()
    }
}

impl Default for ModuleList {
    fn default() -> Self {
        Self::new()
    }
}

/// Reads the current loaded-module state from `/proc/modules` and returns
/// a timestamped snapshot.
pub fn snapshot() -> Result<ModuleSnapshot> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ever_used_excludes_zero_use_count() {
        todo!()
    }

    #[test]
    fn never_used_requires_presence_in_all_snapshots() {
        todo!()
    }
}
