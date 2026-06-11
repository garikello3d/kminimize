use std::collections::HashSet;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use gather::ModuleList;

const DEFAULT_INTERVAL_SECS: u64 = 10;

fn usage(prog: &str) -> ! {
    eprintln!("Usage: {prog} <output_file> [interval_secs]");
    eprintln!();
    eprintln!("  output_file     Path to the JSON file where snapshots are accumulated.");
    eprintln!("  interval_secs   Sampling interval (default: {DEFAULT_INTERVAL_SECS}).");
    eprintln!();
    eprintln!("Runs until interrupted (SIGINT/SIGTERM). Appends one snapshot per interval.");
    std::process::exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage(&args[0]);
    }

    let output = PathBuf::from(&args[1]);
    let interval_secs: u64 = args
        .get(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_INTERVAL_SECS);

    if let Err(e) = gather_loop(output, interval_secs) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn gather_loop(output: PathBuf, interval_secs: u64) -> gather::Result<()> {
    let mut list = if output.exists() {
        ModuleList::load(&output)?
    } else {
        ModuleList::new()
    };

    // Collect device aliases and alias map fresh on every run.
    list.device_aliases = collect_device_aliases();
    list.modules_alias = read_modules_alias();
    eprintln!(
        "kgather: {} device aliases, {} alias map entries",
        list.device_aliases.len(),
        list.modules_alias.len(),
    );

    eprintln!("kgather: writing to {}", output.display());
    eprintln!("kgather: sampling every {interval_secs}s, press Ctrl-C to stop");

    loop {
        let content = std::fs::read_to_string("/proc/modules")?;
        let snap = gather::snapshot_from_content(&content)?;
        eprintln!(
            "kgather: snapshot at {} — {} modules",
            snap.timestamp_secs,
            snap.modules.len()
        );
        list.snapshots.push(snap);
        list.save(&output)?;
        thread::sleep(Duration::from_secs(interval_secs));
    }
}

/// Walk `/sys/bus/*/devices/*/modalias` and return the unique set of alias strings.
fn collect_device_aliases() -> Vec<String> {
    let mut aliases: HashSet<String> = HashSet::new();
    let bus_dir = std::path::Path::new("/sys/bus");
    let bus_rd = match std::fs::read_dir(bus_dir) {
        Ok(rd) => rd,
        Err(e) => {
            eprintln!("kgather: warning: cannot read /sys/bus: {e}");
            return vec![];
        }
    };
    for bus_entry in bus_rd.flatten() {
        let devices_dir = bus_entry.path().join("devices");
        let dev_rd = match std::fs::read_dir(&devices_dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for dev_entry in dev_rd.flatten() {
            let modalias_path = dev_entry.path().join("modalias");
            if let Ok(content) = std::fs::read_to_string(&modalias_path) {
                let trimmed = content.trim().to_owned();
                if !trimmed.is_empty() {
                    aliases.insert(trimmed);
                }
            }
        }
    }
    let mut v: Vec<String> = aliases.into_iter().collect();
    v.sort_unstable();
    v
}

/// Read `/lib/modules/<running-kernel>/modules.alias` and parse it into
/// (pattern, module_name) pairs.
fn read_modules_alias() -> Vec<(String, String)> {
    let version = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .unwrap_or_default();
    let version = version.trim();
    let path = format!("/lib/modules/{version}/modules.alias");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("kgather: warning: cannot read {path}: {e}");
            return vec![];
        }
    };
    let mut result = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Format: "alias PATTERN MODULE_NAME"
        let mut parts = line.splitn(3, ' ');
        if parts.next() != Some("alias") {
            continue;
        }
        if let (Some(pattern), Some(module)) = (parts.next(), parts.next()) {
            result.push((pattern.to_owned(), module.to_owned()));
        }
    }
    result
}
