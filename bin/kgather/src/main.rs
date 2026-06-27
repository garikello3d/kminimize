use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use gather::ModuleList;

const DEFAULT_INTERVAL_SECS: u64 = 10;

fn usage(prog: &str) -> ! {
    eprintln!("Usage: {prog} <output_file> [interval_secs] [--duration <secs>]");
    eprintln!();
    eprintln!("  output_file       Path to the JSON file where snapshots are accumulated.");
    eprintln!("  interval_secs     Sampling interval (default: {DEFAULT_INTERVAL_SECS}).");
    eprintln!("  --duration <secs> Stop after this many seconds (default: run forever).");
    eprintln!();
    eprintln!("Runs until interrupted (SIGINT/SIGTERM) or --duration elapses.");
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
        .filter(|s| !s.starts_with('-'))
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_INTERVAL_SECS);

    let duration_secs: Option<u64> = args
        .windows(2)
        .find(|w| w[0] == "--duration")
        .and_then(|w| w[1].parse().ok());

    if let Err(e) = gather_loop(output, interval_secs, duration_secs) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn gather_loop(output: PathBuf, interval_secs: u64, duration_secs: Option<u64>) -> gather::Result<()> {
    let mut list = if output.exists() {
        ModuleList::load(&output)?
    } else {
        ModuleList::new()
    };

    // Collect device aliases, alias map, and pinned configs fresh on every run.
    list.device_aliases = collect_device_aliases();
    list.modules_alias = read_modules_alias();
    list.pinned_configs = collect_forced_configs();
    eprintln!(
        "kgather: {} device aliases, {} alias map entries, {} pinned configs",
        list.device_aliases.len(),
        list.modules_alias.len(),
        list.pinned_configs.len(),
    );

    eprintln!("kgather: writing to {}", output.display());
    if let Some(d) = duration_secs {
        eprintln!("kgather: sampling every {interval_secs}s for {d}s then exiting");
    } else {
        eprintln!("kgather: sampling every {interval_secs}s, press Ctrl-C to stop");
    }

    let deadline = duration_secs.map(|d| Instant::now() + Duration::from_secs(d));

    loop {
        if deadline.is_some_and(|dl| Instant::now() >= dl) {
            break;
        }
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
    Ok(())
}

/// Walk `/sys/bus/*/devices/*/modalias` and return the unique set of alias strings.
fn collect_device_aliases() -> Vec<String> {
    let bus_dir = std::path::Path::new("/sys/bus");
    let bus_rd = match std::fs::read_dir(bus_dir) {
        Ok(rd) => rd,
        Err(e) => {
            eprintln!("kgather: warning: cannot read /sys/bus: {e}");
            return vec![];
        }
    };
    let mut raw = String::new();
    for bus_entry in bus_rd.flatten() {
        let devices_dir = bus_entry.path().join("devices");
        let Ok(dev_rd) = std::fs::read_dir(&devices_dir) else { continue };
        for dev_entry in dev_rd.flatten() {
            let modalias_path = dev_entry.path().join("modalias");
            if let Ok(content) = std::fs::read_to_string(&modalias_path) {
                raw.push_str(content.trim());
                raw.push('\n');
            }
        }
    }
    gather::parse_device_aliases(&raw)
}

/// Collect config symbols that must remain enabled on this system.
fn collect_forced_configs() -> Vec<String> {
    let has_initcpio = std::path::Path::new("/usr/lib/initcpio/install").is_dir();
    gather::pinned_configs_for_system(has_initcpio)
}

/// Read `/lib/modules/<running-kernel>/modules.alias` and parse it into
/// (pattern, module_name) pairs.
fn read_modules_alias() -> Vec<(String, String)> {
    let version = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let path = format!("/lib/modules/{}/modules.alias", version.trim());
    match std::fs::read_to_string(&path) {
        Ok(content) => gather::parse_modules_alias(&content),
        Err(e) => {
            eprintln!("kgather: warning: cannot read {path}: {e}");
            vec![]
        }
    }
}
