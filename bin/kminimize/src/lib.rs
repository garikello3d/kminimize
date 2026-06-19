use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Applies config reduction to `config_path` (.config file) in place.
///
/// Two categories of symbols are disabled:
/// 1. Loaded-but-idle modules — present in `/proc/modules` snapshots but with
///    no active users across all samples (from `safely_disableable_clusters`).
/// 2. Built-as-module but never observed in any snapshot (=m in .config, never loaded).
///
/// Symbols that would be forced back on by an active `select` constraint are
/// retained.  After this call, run `make olddefconfig` to propagate cascades.
pub fn reduce_config(
    kernel_src: &Path,
    module_list: &gather::ModuleList,
    config_path: &Path,
) -> anyhow::Result<()> {
    let kbuild = kbuild::KbuildMaps::load(kernel_src)
        .map_err(|e| anyhow::anyhow!("failed to load kbuild maps: {e}"))?;
    let config_vals = kbuild::parse_dotconfig(config_path)
        .map_err(|e| anyhow::anyhow!("failed to parse {}: {e}", config_path.display()))?;

    let clusters = module_list.safely_disableable_clusters();

    let total: usize = clusters.iter().map(|c| c.len()).sum();
    println!(
        "Safe to disable — {} module{} in {} cluster{}",
        total,
        if total == 1 { "" } else { "s" },
        clusters.len(),
        if clusters.len() == 1 { "" } else { "s" },
    );
    for cluster in &clusters {
        println!("  [{:>3}]  {}", cluster.len(), cluster.join(", "));
    }
    println!();

    // Category 1: loaded-but-idle modules (from clusters).
    let mut idle_to_disable: HashSet<String> = HashSet::new();
    let mut skipped: Vec<String> = Vec::new();

    for module_name in clusters.iter().flatten() {
        match kbuild.module(module_name) {
            Some(m) => {
                let bare = m.module_config.trim_start_matches("CONFIG_");
                let val = config_vals.get(bare).map(String::as_str).unwrap_or("n");
                if val == "y" || val == "m" {
                    idle_to_disable.insert(format!("CONFIG_{}", m.module_config));
                }
            }
            None => skipped.push(module_name.clone()),
        }
    }

    if !skipped.is_empty() {
        skipped.sort_unstable();
        println!(
            "Modules not found in kernel source (skipped): {}",
            skipped.join(", ")
        );
        println!();
    }

    // Category 2: modules built (enabled in .config) but never appeared in any snapshot.
    let ever_observed: HashSet<String> = {
        let used = module_list.ever_used();
        let never = module_list.never_used();
        used.union(&never).cloned().collect()
    };

    let mut never_loaded_to_disable: HashSet<String> = HashSet::new();
    for (module_name, m) in kbuild.modules() {
        if ever_observed.contains(module_name) {
            continue;
        }
        let bare = m.module_config.trim_start_matches("CONFIG_");
        let val = config_vals.get(bare).map(String::as_str).unwrap_or("n");
        if val == "m" {
            never_loaded_to_disable.insert(format!("CONFIG_{}", m.module_config));
        }
    }
    let never_loaded_candidate_count = never_loaded_to_disable.len();

    // Build combined set for the select filter.
    let mut combined: HashSet<String> = idle_to_disable.clone();
    combined.extend(never_loaded_to_disable.iter().cloned());

    if combined.is_empty() {
        println!("Nothing to disable — all candidate modules are already off.");
        return Ok(());
    }

    // Drop any symbol that an active `select` would force back on.
    let kconfig = kconfig::KconfigGraph::load(kernel_src)
        .map_err(|e| anyhow::anyhow!("failed to load kconfig graph: {e}"))?;
    let cv: HashMap<&str, &str> = config_vals
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let combined_bare: HashSet<String> = combined
        .iter()
        .map(|s| s.trim_start_matches("CONFIG_").to_owned())
        .collect();
    let forced_on: HashSet<String> = kconfig
        .validate_disabled(&combined_bare, &cv)
        .into_iter()
        .map(|bare| format!("CONFIG_{bare}"))
        .collect();

    if !forced_on.is_empty() {
        let mut idle_forced: Vec<&str> = forced_on
            .iter()
            .filter(|s| idle_to_disable.contains(*s))
            .map(String::as_str)
            .collect();
        if !idle_forced.is_empty() {
            idle_forced.sort_unstable();
            println!(
                "Skipping {} loaded-but-idle symbol{} — `select`ed by a still-enabled symbol (make oldconfig would restore them):",
                idle_forced.len(),
                if idle_forced.len() == 1 { "" } else { "s" }
            );
            for sym in &idle_forced {
                println!("  {sym}");
            }
            println!("  (use 'module-plan' to find a variant that also disables the selector)");
            println!();
        }
        for sym in &forced_on {
            idle_to_disable.remove(sym);
            never_loaded_to_disable.remove(sym);
        }
    }

    let never_loaded_skipped = never_loaded_candidate_count - never_loaded_to_disable.len();

    if !idle_to_disable.is_empty() {
        let mut sorted: Vec<&str> = idle_to_disable.iter().map(String::as_str).collect();
        sorted.sort_unstable();
        println!(
            "Disabling {} loaded-but-idle CONFIG symbol{}:",
            sorted.len(),
            if sorted.len() == 1 { "" } else { "s" }
        );
        for sym in &sorted {
            println!("  {}=n", sym);
        }
        println!();
    }

    if never_loaded_candidate_count > 0 {
        let skipped_note = if never_loaded_skipped > 0 {
            format!(", {} skipped: forced on by select", never_loaded_skipped)
        } else {
            String::new()
        };
        println!(
            "Never-loaded modules: {} built but never observed — disabling {}{}",
            never_loaded_candidate_count,
            never_loaded_to_disable.len(),
            skipped_note,
        );
        println!();
    }

    // Merge both categories for writing.
    let mut to_disable = idle_to_disable;
    to_disable.extend(never_loaded_to_disable);

    if to_disable.is_empty() {
        println!("Nothing left to disable after filtering selected symbols.");
        return Ok(());
    }

    write_dotconfig(config_path, &to_disable)?;
    println!("{} updated.", config_path.display());
    println!("Run 'make olddefconfig' to propagate cascades.");
    Ok(())
}

/// Rewrites `config_path` in place, replacing `CONFIG_FOO=y/m` lines for each
/// symbol in `to_disable` with `# CONFIG_FOO is not set`.  Symbols absent from
/// the file are appended at the end.
pub fn write_dotconfig(config_path: &Path, to_disable: &HashSet<String>) -> anyhow::Result<()> {
    let original = std::fs::read_to_string(config_path)
        .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", config_path.display()))?;

    let mut output = String::with_capacity(original.len());
    let mut seen: HashSet<&str> = HashSet::new();

    for line in original.lines() {
        let sym = line
            .split_once('=')
            .and_then(|(lhs, _)| to_disable.get(lhs).map(String::as_str));
        if let Some(sym) = sym {
            output.push_str(&format!("# {} is not set\n", sym));
            seen.insert(sym);
        } else {
            output.push_str(line);
            output.push('\n');
        }
    }

    let mut unseen: Vec<&str> = to_disable
        .iter()
        .map(String::as_str)
        .filter(|s| !seen.contains(s))
        .collect();
    unseen.sort_unstable();
    for sym in unseen {
        output.push_str(&format!("# {} is not set\n", sym));
    }

    std::fs::write(config_path, output)
        .map_err(|e| anyhow::anyhow!("failed to write {}: {e}", config_path.display()))
}
