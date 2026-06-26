use std::collections::HashSet;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse /proc/modules line {line_no}: {reason}\n  line: {line:?}")]
    ProcModules {
        line_no: usize,
        reason: String,
        line: String,
    },
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Load state of a kernel module as reported in column 5 of `/proc/modules`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModuleState {
    Live,
    Loading,
    Unloading,
}

/// One loaded module as reported by a single `/proc/modules` line.
///
/// `/proc/modules` columns:
///   1. name
///   2. memory size (bytes)  — not stored, irrelevant for our use
///   3. use_count            — 0 means loaded but currently not in use
///   4. used_by              — comma-separated list of modules using this one, or "-"
///   5. state                — Live | Loading | Unloading
///   6. base address         — not stored
///   7. optional flags       — "(OE)" marks out-of-tree modules
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleEntry {
    pub name: String,
    /// Number of in-kernel users of this module at snapshot time.
    pub use_count: u32,
    /// Modules that directly depend on (use) this module.
    pub used_by: Vec<String>,
    /// Load state at snapshot time.  Only `Live` entries are counted as
    /// evidence of usage; `Loading`/`Unloading` are transitional.
    pub state: ModuleState,
    /// `true` when the `(OE)` flag is present — module is out-of-tree and
    /// has no `CONFIG_*` entry in the kernel source tree.  Must be excluded
    /// from all analysis.
    pub is_out_of_tree: bool,
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
    /// Device modalias strings collected from `/sys/bus/*/devices/*/modalias`.
    pub device_aliases: Vec<String>,
    /// Module alias map from `/lib/modules/<ver>/modules.alias`: (pattern, module_name).
    pub modules_alias: Vec<(String, String)>,
    /// Bare config symbol names (without `CONFIG_` prefix) that must remain enabled
    /// regardless of observed usage.  Populated via [`parse_forced_configs`]; stub for now.
    #[serde(default)]
    pub pinned_configs: Vec<String>,
}

/// Per-module statistics aggregated across all observed snapshots.
#[derive(Debug, Clone)]
pub struct ModuleStats {
    pub name: String,
    /// Live snapshots in which this module appeared.
    pub total: usize,
    /// Live snapshots in which `use_count > 0`.
    pub active: usize,
    pub max_use: u32,
}

/// Module usage grouped into three activity categories.
/// Produced by [`ModuleList::usage_report`].
#[derive(Debug, Clone)]
pub struct UsageReport {
    /// Total number of snapshots in the observation.
    pub snapshots: usize,
    /// Modules whose `use_count > used_by.len()` in at least one snapshot —
    /// something beyond loaded dependents holds a reference.
    /// Sorted alphabetically.
    pub directly_active: Vec<ModuleStats>,
    /// Modules whose use_count is entirely explained by loaded dependents,
    /// but at least one other module ever depended on them.
    /// Each entry pairs the stats with the sorted list of modules that
    /// ever appeared in its `used_by`.
    /// Sorted alphabetically by name.
    pub infrastructure: Vec<(ModuleStats, Vec<String>)>,
    /// Modules with no direct use and no dependents. Sorted alphabetically.
    pub idle: Vec<ModuleStats>,
}

impl ModuleList {
    pub fn new() -> Self {
        Self {
            snapshots: Vec::new(),
            device_aliases: Vec::new(),
            modules_alias: Vec::new(),
            pinned_configs: Vec::new(),
        }
    }

    /// Deserializes a `ModuleList` from a JSON file produced by `ktinify-gather`.
    pub fn load(path: &Path) -> Result<Self> {
        let data = std::fs::read(path).map_err(Error::Io)?;
        Ok(serde_json::from_slice(&data)?)
    }

    /// Serializes the full list to a JSON file (overwrites on each save).
    pub fn save(&self, path: &Path) -> Result<()> {
        let data = serde_json::to_vec_pretty(self)?;
        std::fs::write(path, data).map_err(Error::Io)
    }

    /// Aggregates per-module statistics and groups them into three activity
    /// categories.  See [`UsageReport`] for the category definitions.
    pub fn usage_report(&self) -> UsageReport {
        use std::collections::HashMap;

        struct Stats {
            total: usize,
            active: usize,
            max_use: u32,
            has_direct_use: bool,
        }

        let mut by_name: HashMap<String, Stats> = HashMap::new();
        for snap in &self.snapshots {
            for entry in &snap.modules {
                if entry.state != ModuleState::Live || entry.is_out_of_tree {
                    continue;
                }
                let s = by_name.entry(entry.name.clone()).or_insert(Stats {
                    total: 0,
                    active: 0,
                    max_use: 0,
                    has_direct_use: false,
                });
                s.total += 1;
                if entry.use_count > 0 {
                    s.active += 1;
                    s.max_use = s.max_use.max(entry.use_count);
                }
                if entry.use_count as usize > entry.used_by.len() {
                    s.has_direct_use = true;
                }
            }
        }

        let dep_map = self.ever_depended_on_by();

        let to_stats = |name: &String, s: &Stats| ModuleStats {
            name: name.clone(),
            total: s.total,
            active: s.active,
            max_use: s.max_use,
        };

        let mut directly_active: Vec<ModuleStats> = by_name
            .iter()
            .filter(|(_, s)| s.has_direct_use)
            .map(|(n, s)| to_stats(n, s))
            .collect();
        directly_active.sort_unstable_by(|a, b| a.name.cmp(&b.name));

        let mut infrastructure: Vec<(ModuleStats, Vec<String>)> = by_name
            .iter()
            .filter(|(n, s)| !s.has_direct_use && dep_map.contains_key(n.as_str()))
            .map(|(n, s)| {
                let mut deps: Vec<String> =
                    dep_map[n.as_str()].iter().cloned().collect();
                deps.sort_unstable();
                (to_stats(n, s), deps)
            })
            .collect();
        infrastructure.sort_unstable_by(|a, b| a.0.name.cmp(&b.0.name));

        let mut idle: Vec<ModuleStats> = by_name
            .iter()
            .filter(|(n, s)| !s.has_direct_use && !dep_map.contains_key(n.as_str()))
            .map(|(n, s)| to_stats(n, s))
            .collect();
        idle.sort_unstable_by(|a, b| a.name.cmp(&b.name));

        UsageReport {
            snapshots: self.snapshots.len(),
            directly_active,
            infrastructure,
            idle,
        }
    }

    /// Returns the set of in-tree module names that had `use_count > 0`
    /// and state `Live` in at least one snapshot.
    pub fn ever_used(&self) -> HashSet<String> {
        self.snapshots
            .iter()
            .flat_map(|s| &s.modules)
            .filter(|m| !m.is_out_of_tree && m.state == ModuleState::Live && m.use_count > 0)
            .map(|m| m.name.clone())
            .collect()
    }

    /// Returns the set of in-tree module names that appeared in at least one
    /// snapshot and were *never* observed with `use_count > 0` in any `Live`
    /// snapshot — i.e. loaded throughout but never actively used.
    pub fn never_used(&self) -> HashSet<String> {
        let ever = self.ever_used();
        self.snapshots
            .iter()
            .flat_map(|s| &s.modules)
            .filter(|m| !m.is_out_of_tree)
            .map(|m| m.name.clone())
            .collect::<HashSet<_>>()
            .difference(&ever)
            .cloned()
            .collect()
    }

    /// Returns a map from module name → set of in-tree modules that ever had
    /// this module in their `used_by` list across all snapshots.
    /// In other words: for each module M, which other modules depended on M.
    pub fn ever_depended_on_by(&self) -> std::collections::HashMap<String, HashSet<String>> {
        let mut map: std::collections::HashMap<String, HashSet<String>> =
            std::collections::HashMap::new();
        for snap in &self.snapshots {
            for entry in &snap.modules {
                if entry.is_out_of_tree {
                    continue;
                }
                for dep_name in &entry.used_by {
                    // dep_name uses/depends-on entry.name,
                    // so entry.name is the module being depended on.
                    map.entry(entry.name.clone())
                        .or_default()
                        .insert(dep_name.clone());
                }
            }
        }
        map
    }

    /// Returns the set of module names required to handle devices physically
    /// present on the target system, derived by matching collected sysfs
    /// modalias strings against the `modules.alias` map.
    pub fn device_required_modules(&self) -> HashSet<String> {
        let mut required = HashSet::new();
        for alias in &self.device_aliases {
            for (pattern, module) in &self.modules_alias {
                if glob_match(pattern, alias) {
                    required.insert(module.clone());
                }
            }
        }
        required
    }

    /// Returns the connected components of the "safely disableable" sub-graph.
    ///
    /// A module is **must-keep** if it is directly active (`use_count >
    /// used_by.len()` in any Live snapshot) or is transitively depended on by
    /// any directly-active module.  Every other in-tree Live module is safely
    /// disableable.
    ///
    /// Modules in the same component depend on each other (directly or
    /// indirectly) and must be removed as a unit.  Each returned `Vec<String>`
    /// is one component, names sorted alphabetically.  Components are sorted by
    /// descending size, then alphabetically by first name.
    pub fn safely_disableable_clusters(&self) -> Vec<Vec<String>> {
        use std::collections::VecDeque;

        // deps_of[M] = set of modules that M depends on (across all snapshots).
        // From each entry: if D is in entry.used_by, then D depends on entry.name.
        let mut deps_of: std::collections::HashMap<String, HashSet<String>> =
            std::collections::HashMap::new();
        let mut all_live: HashSet<String> = HashSet::new();

        for snap in &self.snapshots {
            for entry in &snap.modules {
                if entry.is_out_of_tree || entry.state != ModuleState::Live {
                    continue;
                }
                all_live.insert(entry.name.clone());
                for depender in &entry.used_by {
                    deps_of.entry(depender.clone()).or_default().insert(entry.name.clone());
                }
            }
        }

        // Directly active: use_count > used_by.len() in any Live snapshot.
        let mut directly_active: HashSet<String> = HashSet::new();
        for snap in &self.snapshots {
            for entry in &snap.modules {
                if entry.is_out_of_tree || entry.state != ModuleState::Live {
                    continue;
                }
                if entry.use_count as usize > entry.used_by.len() {
                    directly_active.insert(entry.name.clone());
                }
            }
        }

        // BFS from directly-active through deps_of to find must-keep set.
        let mut must_keep: HashSet<String> = directly_active.clone();
        let mut queue: VecDeque<String> = directly_active.into_iter().collect();
        while let Some(m) = queue.pop_front() {
            if let Some(deps) = deps_of.get(&m) {
                for dep in deps {
                    if must_keep.insert(dep.clone()) {
                        queue.push_back(dep.clone());
                    }
                }
            }
        }

        // Also protect any module that handles a device physically present on
        // the target system (bus drivers, transport drivers, etc. that may have
        // use_count=0 even when actively serving hardware).  Run BFS from the
        // newly added modules so their own dependencies are protected too.
        let mut dev_queue: VecDeque<String> = self
            .device_required_modules()
            .into_iter()
            .filter(|m| must_keep.insert(m.clone()))
            .collect();
        while let Some(m) = dev_queue.pop_front() {
            if let Some(deps) = deps_of.get(&m) {
                for dep in deps {
                    if must_keep.insert(dep.clone()) {
                        dev_queue.push_back(dep.clone());
                    }
                }
            }
        }

        // Safely disableable = all Live in-tree modules not in must_keep.
        let disableable: HashSet<&str> = all_live
            .iter()
            .filter(|n| !must_keep.contains(*n))
            .map(String::as_str)
            .collect();

        // Build undirected adjacency within the disableable sub-graph.
        // Two disableable modules are adjacent if one depends on the other.
        let mut adj: std::collections::HashMap<&str, HashSet<&str>> =
            std::collections::HashMap::new();
        for &m in &disableable {
            adj.entry(m).or_default();
        }
        for snap in &self.snapshots {
            for entry in &snap.modules {
                if entry.is_out_of_tree { continue; }
                let m = entry.name.as_str();
                if !disableable.contains(m) { continue; }
                for dep_name in &entry.used_by {
                    let d = dep_name.as_str();
                    if disableable.contains(d) {
                        adj.entry(m).or_default().insert(d);
                        adj.entry(d).or_default().insert(m);
                    }
                }
            }
        }

        // BFS to collect connected components.
        let mut visited: HashSet<&str> = HashSet::new();
        let mut clusters: Vec<Vec<String>> = Vec::new();
        let mut sorted: Vec<&str> = disableable.iter().copied().collect();
        sorted.sort_unstable();

        for start in sorted {
            if visited.contains(start) { continue; }
            let mut component: Vec<String> = Vec::new();
            let mut q: VecDeque<&str> = VecDeque::new();
            q.push_back(start);
            visited.insert(start);
            while let Some(node) = q.pop_front() {
                component.push(node.to_owned());
                if let Some(neighbors) = adj.get(node) {
                    for &nb in neighbors {
                        if visited.insert(nb) {
                            q.push_back(nb);
                        }
                    }
                }
            }
            component.sort_unstable();
            clusters.push(component);
        }

        clusters.sort_unstable_by(|a, b| b.len().cmp(&a.len()).then_with(|| a[0].cmp(&b[0])));
        clusters
    }

    /// Returns the set of in-tree module names that must also be disabled
    /// when disabling `module` — every module that directly or transitively
    /// lists `module` in its `used_by` chain, across all snapshots.
    ///
    /// The returned set does *not* include `module` itself.
    pub fn disable_cascade(&self, module: &str) -> HashSet<String> {
        // Build a reverse-dep map: module → set of modules that directly depend on it.
        // We union across all snapshots so that any dependency ever observed is captured.
        let mut reverse: std::collections::HashMap<&str, HashSet<&str>> =
            std::collections::HashMap::new();

        for snap in &self.snapshots {
            for entry in &snap.modules {
                if entry.is_out_of_tree {
                    continue;
                }
                for dep in &entry.used_by {
                    reverse.entry(dep.as_str()).or_default().insert(entry.name.as_str());
                }
            }
        }

        // BFS from `module` through the reverse-dep graph.
        let mut result: HashSet<String> = HashSet::new();
        let mut queue: std::collections::VecDeque<&str> = std::collections::VecDeque::new();
        queue.push_back(module);

        while let Some(current) = queue.pop_front() {
            if let Some(dependents) = reverse.get(current) {
                for dep in dependents {
                    if result.insert(dep.to_string()) {
                        queue.push_back(dep);
                    }
                }
            }
        }

        result
    }
}

impl Default for ModuleList {
    fn default() -> Self {
        Self::new()
    }
}

/// Loads a `ModuleList` from `path` and returns the connected components of
/// the safely-disableable module sub-graph.  Each inner `Vec` is one cluster —
/// a group of modules that can be removed together and whose removal is safe
/// (no directly-active module depends on any of them, directly or transitively).
/// Clusters are sorted by descending size, then alphabetically by first name.
/// Within each cluster names are sorted alphabetically.
pub fn disableable_clusters(path: &Path) -> Result<Vec<Vec<String>>> {
    let list = ModuleList::load(path)?;
    Ok(list.safely_disableable_clusters())
}

/// Parses the content of `/proc/modules` (as a string) into a `Vec<ModuleEntry>`.
///
/// Exposed publicly so it can be unit-tested with synthetic input without
/// touching the filesystem.
pub fn parse_proc_modules(content: &str) -> Result<Vec<ModuleEntry>> {
    let mut entries = Vec::new();

    for (idx, line) in content.lines().enumerate() {
        let line_no = idx + 1;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let err = |reason: &str| Error::ProcModules {
            line_no,
            reason: reason.to_owned(),
            line: line.to_owned(),
        };

        let mut cols = line.splitn(6, ' ');

        let name = cols.next().ok_or_else(|| err("missing name"))?.to_owned();

        let _size = cols.next().ok_or_else(|| err("missing size"))?;

        let use_count: u32 = cols
            .next()
            .ok_or_else(|| err("missing use_count"))?
            .parse()
            .map_err(|_| err("use_count is not a u32"))?;

        let used_by_raw = cols.next().ok_or_else(|| err("missing used_by column"))?;
        let used_by: Vec<String> = if used_by_raw == "-" {
            Vec::new()
        } else {
            used_by_raw
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        };

        let state_str = cols.next().ok_or_else(|| err("missing state"))?.trim();
        let state = match state_str {
            "Live" => ModuleState::Live,
            "Loading" => ModuleState::Loading,
            "Unloading" => ModuleState::Unloading,
            other => return Err(err(&format!("unknown state {other:?}"))),
        };

        // The remainder of the line (after the 6th split) may contain the
        // base address and optional flags like "(OE)".
        let rest = cols.next().unwrap_or("");
        let is_out_of_tree = rest.contains("(OE)");

        entries.push(ModuleEntry { name, use_count, used_by, state, is_out_of_tree });
    }

    Ok(entries)
}

/// Parses `content` (the text of `/proc/modules`) into a timestamped snapshot.
///
/// Use this when the content was obtained by some means other than a local
/// filesystem read — for example, fetched over SSH.
pub fn snapshot_from_content(content: &str) -> Result<ModuleSnapshot> {
    let modules = parse_proc_modules(content)?;
    let timestamp_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    Ok(ModuleSnapshot { timestamp_secs, modules })
}

/// Parse newline-separated modalias strings into a sorted, deduplicated list.
/// Works for both sysfs-collected aliases (concatenated local reads) and SSH output.
pub fn parse_device_aliases(raw: &str) -> Vec<String> {
    let mut set: std::collections::HashSet<String> = std::collections::HashSet::new();
    for alias in raw.lines().map(str::trim).filter(|s| !s.is_empty()) {
        set.insert(alias.to_owned());
    }
    let mut v: Vec<String> = set.into_iter().collect();
    v.sort_unstable();
    v
}

/// Parse the content of a `modules.alias` file into (pattern, module_name) pairs.
/// Lines not starting with "alias" (comments, blank lines) are skipped.
pub fn parse_modules_alias(content: &str) -> Vec<(String, String)> {
    content
        .lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("alias ")?;
            let (pattern, module) = rest.split_once(' ')?;
            Some((pattern.trim().to_owned(), module.trim().to_owned()))
        })
        .collect()
}

/// Returns config symbols (bare, without `CONFIG_` prefix) that must remain enabled
/// based on detected properties of the target system.
///
/// `has_initcpio_install`: whether `/usr/lib/initcpio/install` is present —
/// indicates an mkinitcpio-based initramfs that requires LZ4 compression support.
pub fn pinned_configs_for_system(has_initcpio_install: bool) -> Vec<String> {
    let mut configs = Vec::new();
    if has_initcpio_install {
        configs.push("CRYPTO_LZ4".into());
        configs.push("CRYPTO_LZ4HC".into());
    }
    configs
}

/// Reads the current loaded-module state from `/proc/modules` and returns
/// a timestamped snapshot.
pub fn snapshot() -> Result<ModuleSnapshot> {
    let content = std::fs::read_to_string("/proc/modules").map_err(Error::Io)?;
    snapshot_from_content(&content)
}

fn glob_match(pattern: &str, text: &str) -> bool {
    glob_bytes(pattern.as_bytes(), text.as_bytes())
}

fn glob_bytes(pat: &[u8], txt: &[u8]) -> bool {
    match pat.first() {
        None => txt.is_empty(),
        Some(b'*') => (0..=txt.len()).any(|i| glob_bytes(&pat[1..], &txt[i..])),
        Some(b'?') => !txt.is_empty() && glob_bytes(&pat[1..], &txt[1..]),
        Some(&c) => !txt.is_empty() && txt[0] == c && glob_bytes(&pat[1..], &txt[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_proc_modules ────────────────────────────────────────────────

    const SAMPLE: &str = "\
rpcrdma 483328 2 - Live 0x0000000000000000
rdma_cm 159744 1 rpcrdma, Live 0x0000000000000000
ib_core 573440 4 rpcrdma,rdma_cm,iw_cm,ib_cm, Live 0x0000000000000000
tcp_diag 20480 0 - Live 0x0000000000000000
zenpower 20480 0 - Live 0x0000000000000000 (OE)
vboxdrv 696320 2 vboxnetadp,vboxnetflt, Live 0x0000000000000000 (OE)
";

    #[test]
    fn parse_no_deps() {
        let entries = parse_proc_modules("rpcrdma 483328 2 - Live 0x0000000000000000").unwrap();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.name, "rpcrdma");
        assert_eq!(e.use_count, 2);
        assert!(e.used_by.is_empty());
        assert_eq!(e.state, ModuleState::Live);
        assert!(!e.is_out_of_tree);
    }

    #[test]
    fn parse_single_dep() {
        let entries =
            parse_proc_modules("rdma_cm 159744 1 rpcrdma, Live 0x0000000000000000").unwrap();
        let e = &entries[0];
        assert_eq!(e.used_by, vec!["rpcrdma"]);
        assert_eq!(e.use_count, 1);
    }

    #[test]
    fn parse_multiple_deps() {
        let entries = parse_proc_modules(
            "ib_core 573440 4 rpcrdma,rdma_cm,iw_cm,ib_cm, Live 0x0000000000000000",
        )
        .unwrap();
        let e = &entries[0];
        assert_eq!(e.used_by, vec!["rpcrdma", "rdma_cm", "iw_cm", "ib_cm"]);
    }

    #[test]
    fn parse_out_of_tree_flag() {
        let entries =
            parse_proc_modules("zenpower 20480 0 - Live 0x0000000000000000 (OE)").unwrap();
        assert!(entries[0].is_out_of_tree);

        let entries =
            parse_proc_modules("tcp_diag 20480 0 - Live 0x0000000000000000").unwrap();
        assert!(!entries[0].is_out_of_tree);
    }

    #[test]
    fn parse_states() {
        let entries =
            parse_proc_modules("foo 4096 0 - Loading 0x0000000000000000").unwrap();
        assert_eq!(entries[0].state, ModuleState::Loading);

        let entries =
            parse_proc_modules("bar 4096 0 - Unloading 0x0000000000000000").unwrap();
        assert_eq!(entries[0].state, ModuleState::Unloading);
    }

    #[test]
    fn parse_empty_lines_ignored() {
        let entries = parse_proc_modules("\n\nrpcrdma 483328 2 - Live 0x0\n\n").unwrap();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn parse_error_bad_use_count() {
        let result = parse_proc_modules("bad 4096 notanumber - Live 0x0");
        assert!(matches!(result, Err(Error::ProcModules { .. })));
    }

    #[test]
    fn parse_full_sample() {
        let entries = parse_proc_modules(SAMPLE).unwrap();
        assert_eq!(entries.len(), 6);
    }

    // ── ModuleList::ever_used ─────────────────────────────────────────────

    fn make_entry(name: &str, use_count: u32, oot: bool) -> ModuleEntry {
        ModuleEntry {
            name: name.to_owned(),
            use_count,
            used_by: vec![],
            state: ModuleState::Live,
            is_out_of_tree: oot,
        }
    }

    fn make_entry_state(name: &str, use_count: u32, state: ModuleState) -> ModuleEntry {
        ModuleEntry {
            name: name.to_owned(),
            use_count,
            used_by: vec![],
            state,
            is_out_of_tree: false,
        }
    }

    fn snap(ts: u64, modules: Vec<ModuleEntry>) -> ModuleSnapshot {
        ModuleSnapshot { timestamp_secs: ts, modules }
    }

    #[test]
    fn ever_used_includes_nonzero_use_count() {
        let list = ModuleList {
            snapshots: vec![snap(1, vec![make_entry("foo", 1, false)])],
            ..Default::default()
        };
        assert!(list.ever_used().contains("foo"));
    }

    #[test]
    fn ever_used_excludes_zero_use_count() {
        let list = ModuleList {
            snapshots: vec![snap(1, vec![make_entry("foo", 0, false)])],
            ..Default::default()
        };
        assert!(!list.ever_used().contains("foo"));
    }

    #[test]
    fn ever_used_excludes_out_of_tree() {
        let list = ModuleList {
            snapshots: vec![snap(1, vec![make_entry("vboxdrv", 2, true)])],
            ..Default::default()
        };
        assert!(!list.ever_used().contains("vboxdrv"));
    }

    #[test]
    fn ever_used_excludes_transitional_states() {
        let list = ModuleList {
            snapshots: vec![snap(
                1,
                vec![
                    make_entry_state("loading_mod", 1, ModuleState::Loading),
                    make_entry_state("unloading_mod", 1, ModuleState::Unloading),
                ],
            )],
            ..Default::default()
        };
        assert!(!list.ever_used().contains("loading_mod"));
        assert!(!list.ever_used().contains("unloading_mod"));
    }

    #[test]
    fn ever_used_sufficient_in_one_snapshot() {
        // Module is zero in snapshot 1, nonzero in snapshot 2 — counts as used.
        let list = ModuleList {
            snapshots: vec![
                snap(1, vec![make_entry("foo", 0, false)]),
                snap(2, vec![make_entry("foo", 3, false)]),
            ],
            ..Default::default()
        };
        assert!(list.ever_used().contains("foo"));
    }

    // ── ModuleList::never_used ────────────────────────────────────────────

    #[test]
    fn never_used_excludes_ever_used_module() {
        let list = ModuleList {
            snapshots: vec![snap(1, vec![make_entry("foo", 1, false)])],
            ..Default::default()
        };
        assert!(!list.never_used().contains("foo"));
    }

    #[test]
    fn never_used_includes_always_zero() {
        let list = ModuleList {
            snapshots: vec![
                snap(1, vec![make_entry("idle", 0, false)]),
                snap(2, vec![make_entry("idle", 0, false)]),
            ],
            ..Default::default()
        };
        assert!(list.never_used().contains("idle"));
    }

    #[test]
    fn never_used_excludes_out_of_tree() {
        let list = ModuleList {
            snapshots: vec![snap(1, vec![make_entry("zenpower", 0, true)])],
            ..Default::default()
        };
        assert!(!list.never_used().contains("zenpower"));
    }

    // ── ModuleList::disable_cascade ───────────────────────────────────────

    fn make_entry_deps(name: &str, used_by: Vec<&str>) -> ModuleEntry {
        ModuleEntry {
            name: name.to_owned(),
            use_count: 0,
            used_by: used_by.into_iter().map(str::to_owned).collect(),
            state: ModuleState::Live,
            is_out_of_tree: false,
        }
    }

    #[test]
    fn disable_cascade_direct_dependents() {
        // rdma_cm and iw_cm both depend on ib_core.
        // Disabling ib_core must also pull in rdma_cm and iw_cm.
        let list = ModuleList {
            snapshots: vec![snap(
                1,
                vec![
                    make_entry_deps("ib_core", vec![]),
                    make_entry_deps("rdma_cm", vec!["ib_core"]),
                    make_entry_deps("iw_cm", vec!["ib_core"]),
                    make_entry_deps("unrelated", vec![]),
                ],
            )],
            ..Default::default()
        };
        let cascade = list.disable_cascade("ib_core");
        assert!(cascade.contains("rdma_cm"));
        assert!(cascade.contains("iw_cm"));
        assert!(!cascade.contains("ib_core")); // not the module itself
        assert!(!cascade.contains("unrelated"));
    }

    #[test]
    fn disable_cascade_transitive() {
        // A ← B ← C: disabling A must cascade through B to C.
        let list = ModuleList {
            snapshots: vec![snap(
                1,
                vec![
                    make_entry_deps("a", vec![]),
                    make_entry_deps("b", vec!["a"]),
                    make_entry_deps("c", vec!["b"]),
                ],
            )],
            ..Default::default()
        };
        let cascade = list.disable_cascade("a");
        assert!(cascade.contains("b"));
        assert!(cascade.contains("c"));
    }

    #[test]
    fn disable_cascade_no_dependents() {
        let list = ModuleList {
            snapshots: vec![snap(1, vec![make_entry_deps("leaf", vec![])])],
            ..Default::default()
        };
        assert!(list.disable_cascade("leaf").is_empty());
    }

    #[test]
    fn disable_cascade_ignores_out_of_tree() {
        let mut oot = make_entry_deps("oot_mod", vec!["ib_core"]);
        oot.is_out_of_tree = true;
        let list = ModuleList {
            snapshots: vec![snap(1, vec![make_entry_deps("ib_core", vec![]), oot])],
            ..Default::default()
        };
        // oot_mod depends on ib_core, but it's out-of-tree — should not appear in cascade.
        assert!(!list.disable_cascade("ib_core").contains("oot_mod"));
    }

    #[test]
    fn disable_cascade_unions_across_snapshots() {
        // dep only shows up in snapshot 2, not snapshot 1 — should still be captured.
        let list = ModuleList {
            snapshots: vec![
                snap(1, vec![make_entry_deps("base", vec![])]),
                snap(
                    2,
                    vec![
                        make_entry_deps("base", vec![]),
                        make_entry_deps("late_dep", vec!["base"]),
                    ],
                ),
            ],
            ..Default::default()
        };
        assert!(list.disable_cascade("base").contains("late_dep"));
    }

    // ── ModuleList::ever_depended_on_by ──────────────────────────────────

    #[test]
    fn ever_depended_on_by_basic() {
        // ib_core is used by rpcrdma and rdma_cm (they depend on it).
        // rdma_cm is also used by rpcrdma.  rpcrdma has no users.
        let list = ModuleList {
            snapshots: vec![snap(
                1,
                vec![
                    make_entry_deps("ib_core", vec!["rpcrdma", "rdma_cm"]),
                    make_entry_deps("rdma_cm", vec!["rpcrdma"]),
                    make_entry_deps("rpcrdma", vec![]),
                ],
            )],
            ..Default::default()
        };
        let map = list.ever_depended_on_by();
        // ib_core is depended on by rpcrdma and rdma_cm
        let ib = map.get("ib_core").unwrap();
        assert!(ib.contains("rpcrdma"));
        assert!(ib.contains("rdma_cm"));
        // rdma_cm is depended on by rpcrdma
        let rdma = map.get("rdma_cm").unwrap();
        assert!(rdma.contains("rpcrdma"));
        // rpcrdma has no dependents
        assert!(!map.contains_key("rpcrdma"));
    }

    // ── Human-readable report from real gathered data ────────────────────
    //
    // Run with:
    //   TEST_MODULES_GATHERED=/path/to/modules.json \
    //       cargo test -p gather module_usage_report -- --nocapture

    #[test]
    fn module_usage_report() {
        let path = match std::env::var("TEST_MODULES_GATHERED") {
            Ok(p) => std::path::PathBuf::from(p),
            Err(_) => panic!(
                "TEST_MODULES_GATHERED is not set.\n\
                 Provide a module list file:\n  \
                 TEST_MODULES_GATHERED=/path/to/modules.json \
                 cargo test -p gather module_usage_report -- --nocapture"
            ),
        };

        let list = ModuleList::load(&path)
            .unwrap_or_else(|e| panic!("failed to load {}: {e}", path.display()));
        let report = list.usage_report();
        let clusters = disableable_clusters(&path)
            .unwrap_or_else(|e| panic!("disableable_clusters failed: {e}"));

        // ── table helpers ────────────────────────────────────────────────
        fn divider4(name_w: usize) {
            println!("  +{:-<name_w$}+{:->10}+{:->8}+{:->7}+", "", "", "", "",
                name_w = name_w + 2);
        }
        fn header4(name_w: usize) {
            println!("  | {:<name_w$} | {:>8} | {:>6} | {:>5} |",
                "Module", "Max use", "Active", "Total", name_w = name_w);
        }
        fn row4(s: &ModuleStats, name_w: usize) {
            println!("  | {:<name_w$} | {:>8} | {:>6} | {:>5} |",
                s.name, s.max_use, s.active, s.total, name_w = name_w);
        }

        // ── table 1: directly active modules ────────────────────────────
        let w1 = report.directly_active.iter().map(|s| s.name.len()).max().unwrap_or(6).max(6);
        println!("\nDirectly active modules — {} ({} snapshots total)",
            report.directly_active.len(), report.snapshots);
        divider4(w1);
        header4(w1);
        divider4(w1);
        for s in &report.directly_active { row4(s, w1); }
        divider4(w1);

        // ── table 2b: infrastructure modules ────────────────────────────
        let w2b_name = report.infrastructure.iter().map(|(s, _)| s.name.len()).max().unwrap_or(6).max(6);
        let w2b_deps = report.infrastructure.iter()
            .map(|(_, ds)| ds.join(", ").len()).max().unwrap_or(14).max(14);
        println!("\nInfrastructure modules (only referenced by loaded dependents) — {}",
            report.infrastructure.len());
        println!("  +{:-<w2b_name$}+{:->10}+{:->7}+{:-<w2b_deps$}+",
            "", "", "", "", w2b_name = w2b_name + 2, w2b_deps = w2b_deps + 2);
        println!("  | {:<w2b_name$} | {:>8} | {:>5} | {:<w2b_deps$} |",
            "Module", "Max use", "Total", "Depended on by",
            w2b_name = w2b_name, w2b_deps = w2b_deps);
        println!("  +{:-<w2b_name$}+{:->10}+{:->7}+{:-<w2b_deps$}+",
            "", "", "", "", w2b_name = w2b_name + 2, w2b_deps = w2b_deps + 2);
        for (s, ds) in &report.infrastructure {
            println!("  | {:<w2b_name$} | {:>8} | {:>5} | {:<w2b_deps$} |",
                s.name, s.max_use, s.total, ds.join(", "),
                w2b_name = w2b_name, w2b_deps = w2b_deps);
        }
        println!("  +{:-<w2b_name$}+{:->10}+{:->7}+{:-<w2b_deps$}+",
            "", "", "", "", w2b_name = w2b_name + 2, w2b_deps = w2b_deps + 2);

        // ── table 2a: idle modules ───────────────────────────────────────
        let w2a = report.idle.iter().map(|s| s.name.len()).max().unwrap_or(6).max(6);
        println!("\nIdle modules (never used, no dependents) — {}", report.idle.len());
        println!("  +{:-<w2a$}+{:->7}+", "", "", w2a = w2a + 2);
        println!("  | {:<w2a$} | {:>5} |", "Module", "Total", w2a = w2a);
        println!("  +{:-<w2a$}+{:->7}+", "", "", w2a = w2a + 2);
        for s in &report.idle {
            println!("  | {:<w2a$} | {:>5} |", s.name, s.total, w2a = w2a);
        }
        println!("  +{:-<w2a$}+{:->7}+", "", "", w2a = w2a + 2);

        // ── safe to disable ──────────────────────────────────────────────
        let total_disableable: usize = clusters.iter().map(|c| c.len()).sum();
        println!(
            "\nSafe to disable — {} module{} in {} cluster{}",
            total_disableable,
            if total_disableable == 1 { "" } else { "s" },
            clusters.len(),
            if clusters.len() == 1 { "" } else { "s" },
        );
        for cluster in &clusters {
            println!("  [{:>3}]  {}", cluster.len(), cluster.join(", "));
        }
        println!();
    }

    // ── ModuleList::safely_disableable_clusters ───────────────────────────

    fn make_entry_uc(name: &str, use_count: u32, used_by: Vec<&str>) -> ModuleEntry {
        ModuleEntry {
            name: name.to_owned(),
            use_count,
            used_by: used_by.into_iter().map(str::to_owned).collect(),
            state: ModuleState::Live,
            is_out_of_tree: false,
        }
    }

    #[test]
    fn directly_active_module_not_disableable() {
        // "active" has use_count=1, used_by=[] → directly active.
        let list = ModuleList {
            snapshots: vec![snap(1, vec![make_entry_uc("active", 1, vec![])])],
            ..Default::default()
        };
        let clusters = list.safely_disableable_clusters();
        let all: Vec<&str> = clusters.iter().flat_map(|c| c.iter().map(String::as_str)).collect();
        assert!(!all.contains(&"active"));
    }

    #[test]
    fn dependency_of_active_not_disableable() {
        // "dep" is depended on by "active"; "active" is directly active.
        // dep must stay even though it has no direct use itself.
        let list = ModuleList {
            snapshots: vec![snap(
                1,
                vec![
                    make_entry_uc("active", 2, vec!["dep"]), // active depends on dep; use_count=2 > used_by=1
                    make_entry_uc("dep", 1, vec![]),         // dep has use_count=1 from active loading it
                ],
            )],
            ..Default::default()
        };
        // active: use_count=2, used_by=["dep"] → direct = 2-1 = 1 > 0 → directly active
        // dep: use_count=1, used_by=[] → not directly active, but required by active → must keep
        let clusters = list.safely_disableable_clusters();
        let all: Vec<&str> = clusters.iter().flat_map(|c| c.iter().map(String::as_str)).collect();
        assert!(!all.contains(&"active"), "active should not be disableable");
        assert!(!all.contains(&"dep"), "dep is required by active, should not be disableable");
    }

    #[test]
    fn idle_module_is_own_cluster() {
        // Module with use_count=0, no deps, not depended on → isolated cluster.
        let list = ModuleList {
            snapshots: vec![snap(1, vec![make_entry_uc("idle", 0, vec![])])],
            ..Default::default()
        };
        let clusters = list.safely_disableable_clusters();
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0], vec!["idle"]);
    }

    #[test]
    fn chain_forms_cluster() {
        // A is directly active, A depends on B, B depends on C.
        // C also has an unrelated dependent D (not active).
        // D→C: both D and C are disableable? No: A→B→C means C is must-keep.
        // Actually: A(active) depends on B, B depends on C → B and C are must-keep.
        // D depends on C (must-keep) but D itself is not active → D is disableable.
        // D has no disableable neighbors (C is must-keep), so D is its own cluster.
        //
        // Simpler test: two inactive modules in a chain, no active module needs them.
        // P (inactive) depends on Q (inactive) — both disableable, same cluster.
        // active (active) depends on X — X must-keep, active must-keep.
        let list = ModuleList {
            snapshots: vec![snap(
                1,
                vec![
                    make_entry_uc("active", 2, vec!["x"]),
                    make_entry_uc("x", 1, vec![]),
                    make_entry_uc("p", 0, vec!["q"]), // p depends on q
                    make_entry_uc("q", 0, vec![]),
                ],
            )],
            ..Default::default()
        };
        // active: use_count=2 > used_by=["x"].len()=1 → direct=1 → directly active
        // x: must-keep (needed by active)
        // p: not active, no one depends on p → disableable
        // q: not active, p depends on q; p is disableable → q is disableable
        let clusters = list.safely_disableable_clusters();
        let all_names: Vec<&str> =
            clusters.iter().flat_map(|c| c.iter().map(String::as_str)).collect();
        assert!(!all_names.contains(&"active"));
        assert!(!all_names.contains(&"x"));
        assert!(all_names.contains(&"p"));
        assert!(all_names.contains(&"q"));
        // p and q must be in the same cluster
        let pq_cluster = clusters.iter().find(|c| c.contains(&"p".to_owned())).unwrap();
        assert!(pq_cluster.contains(&"q".to_owned()), "p and q should be in the same cluster");
    }

    #[test]
    fn disconnected_chains_are_separate_clusters() {
        // Two independent inactive chains: (p→q) and (r→s). Each is its own cluster.
        let list = ModuleList {
            snapshots: vec![snap(
                1,
                vec![
                    make_entry_uc("p", 0, vec!["q"]),
                    make_entry_uc("q", 0, vec![]),
                    make_entry_uc("r", 0, vec!["s"]),
                    make_entry_uc("s", 0, vec![]),
                ],
            )],
            ..Default::default()
        };
        let clusters = list.safely_disableable_clusters();
        assert_eq!(clusters.len(), 2);
        let names: Vec<Vec<&str>> = clusters
            .iter()
            .map(|c| c.iter().map(String::as_str).collect())
            .collect();
        // One cluster contains p and q, the other r and s
        assert!(names.iter().any(|c| c.contains(&"p") && c.contains(&"q")));
        assert!(names.iter().any(|c| c.contains(&"r") && c.contains(&"s")));
    }

    // ── ModuleList::save / load ───────────────────────────────────────────

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("modules.json");

        let original = ModuleList {
            snapshots: vec![snap(42, vec![make_entry("foo", 3, false)])],
            ..Default::default()
        };
        original.save(&path).unwrap();

        let loaded = ModuleList::load(&path).unwrap();
        assert_eq!(loaded.snapshots.len(), 1);
        assert_eq!(loaded.snapshots[0].timestamp_secs, 42);
        assert_eq!(loaded.snapshots[0].modules[0].name, "foo");
        assert_eq!(loaded.snapshots[0].modules[0].use_count, 3);
    }
}
