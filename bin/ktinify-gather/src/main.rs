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

    eprintln!("ktinify-gather: writing to {}", output.display());
    eprintln!("ktinify-gather: sampling every {interval_secs}s, press Ctrl-C to stop");

    loop {
        let snap = gather::snapshot()?;
        eprintln!(
            "ktinify-gather: snapshot at {} — {} modules",
            snap.timestamp_secs,
            snap.modules.len()
        );
        list.snapshots.push(snap);
        list.save(&output)?;
        thread::sleep(Duration::from_secs(interval_secs));
    }
}
