mod load_ltp;
mod self_test;

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "kminimize")]
#[command(about = "Generate a minimal kernel .config based on runtime coverage and module usage data")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub(crate) enum LoadType {
    /// Linux Test Project — exercises a broad set of kernel interfaces.
    Ltp,
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub(crate) enum DistroKind {
    Debian,
    Arch,
}

impl DistroKind {
    pub(crate) fn build(self) -> Box<dyn qoc::Distro> {
        match self {
            DistroKind::Debian => Box::new(qoc::Debian),
            DistroKind::Arch => Box::new(qoc::Arch),
        }
    }
}

#[derive(clap::Args)]
pub(crate) struct SelfTestArgs {
    /// Path to the Linux kernel source tree to check out and build.
    #[arg(long)]
    linux_dir: PathBuf,

    /// Distro to use for the VM rootfs.
    #[arg(long, value_enum)]
    distro: DistroKind,

    /// Keep the temporary work directory after the test completes (default: delete on exit).
    #[arg(long, default_value_t = false)]
    keep_dir: bool,

    /// Workload to run in the VM during the gathering window.
    /// kgather runs until the load finishes; without a load it runs for 15 s.
    #[arg(long, value_enum)]
    pub(crate) load_type: Option<LoadType>,
}

#[derive(Subcommand)]
enum Commands {
    /// Produce a minimized .config by disabling kernel options unused during the observation period.
    Disable(DisableArgs),
    /// Report every way to disable a named module from the build, ranked by collateral damage.
    ModulePlan(ModulePlanArgs),
    /// Gather /proc/modules snapshots from a remote system over SSH, then report peak usage.
    RemoteGather(RemoteGatherArgs),
    /// End-to-end self-test: boot a VM, gather modules, reduce config, rebuild, and re-boot.
    SelfTest(SelfTestArgs),
}

#[derive(clap::Args)]
struct DisableArgs {
    /// Path to the kernel source tree; the output .config is written here.
    #[arg(long)]
    kernel_src: PathBuf,

    /// Path to the gcov directory structure gathered from the target system.
    #[arg(long)]
    gcov_dir: PathBuf,

    /// Path to the module usage list produced by kgather.
    #[arg(long)]
    module_list: PathBuf,
}

#[derive(clap::Args)]
struct ModulePlanArgs {
    /// Path to the kernel source tree (Makefiles + Kconfig files are read here).
    /// Required in normal mode. In --self-test mode: use this dir instead of cloning.
    #[arg(long)]
    kernel_src: Option<PathBuf>,

    /// Module name as it appears in /proc/modules (e.g. "nfsd").
    #[arg(long)]
    module: String,

    /// Reference .config for the current (full) build.
    #[arg(long)]
    config: PathBuf,

    /// Show all collateral files instead of truncating.
    #[arg(long)]
    verbose: bool,

    /// Build the kernel twice and verify that predictions match reality.
    #[arg(long)]
    self_test: bool,

    /// Clone source URL or local git path. Defaults to kernel.org stable.
    /// Mutually exclusive with --kernel-src.
    #[arg(long, conflicts_with = "kernel_src")]
    clone_from: Option<String>,

    /// Git tag to check out, e.g. v6.8.12. Required with --self-test.
    #[arg(long)]
    ver: Option<String>,

    /// Compiler to set as CC and HOSTCC for every make invocation, e.g. gcc-8.
    #[arg(long)]
    cc: Option<String>,
}

#[derive(clap::Args)]
struct RemoteGatherArgs {
    /// SSH target as user@hostname.
    #[arg(long)]
    ssh_url: String,

    /// SSH port.
    #[arg(long, default_value = "22")]
    port: u16,

    /// Seconds between /proc/modules samples.
    #[arg(long)]
    interval: u64,

    /// Total observation duration in seconds.
    #[arg(long)]
    duration: u64,
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Commands::Disable(args) => disable(args),
        Commands::ModulePlan(args) => module_plan(args),
        Commands::RemoteGather(args) => remote_gather(args),
        Commands::SelfTest(args) => self_test::run(args),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn disable(args: DisableArgs) -> anyhow::Result<()> {
    let config_path = args.kernel_src.join(".config");
    let module_list = gather::ModuleList::load(&args.module_list)
        .map_err(|e| anyhow::anyhow!("failed to load module list: {e}"))?;
    kminimize::reduce_config(&args.kernel_src, &module_list, &config_path)
}

fn module_plan(args: ModulePlanArgs) -> anyhow::Result<()> {
    if args.self_test {
        return self_test_module_plan(args);
    }

    let kernel_src = args
        .kernel_src
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("--kernel-src is required in normal mode"))?;

    run_analysis(kernel_src, &args.module, &args.config, args.verbose).map(|_| ())
}

// ── Normal analysis (shared with self-test step 5) ───────────────────────────

fn run_analysis(
    kernel_src: &Path,
    module_name: &str,
    config_path: &Path,
    verbose: bool,
) -> anyhow::Result<Vec<kbuild::DisableVariant>> {
    eprint!("Loading Kbuild maps from {} ...", kernel_src.display());
    let kbuild = kbuild::KbuildMaps::load(kernel_src)
        .map_err(|e| anyhow::anyhow!("kbuild load failed: {e}"))?;
    eprintln!(" done ({} modules)", kbuild.modules().len());

    eprint!("Loading Kconfig graph ...");
    let kconfig = kconfig::KconfigGraph::load(kernel_src)
        .map_err(|e| anyhow::anyhow!("kconfig load failed: {e}"))?;
    eprintln!(" done");

    let module = kbuild.module(module_name).ok_or_else(|| {
        anyhow::anyhow!(
            "module '{}' not found in kbuild maps; known modules: {}",
            module_name,
            {
                let mut names: Vec<_> = kbuild.modules().keys().cloned().collect();
                names.sort();
                names.truncate(20);
                names.join(", ")
            }
        )
    })?;

    eprint!("Parsing .config ...");
    let dotconfig = kbuild::parse_dotconfig(config_path)
        .map_err(|e| anyhow::anyhow!("config parse failed: {e}"))?;
    eprintln!(" done ({} symbols)", dotconfig.len());

    let cv: HashMap<&str, &str> = dotconfig
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    println!(
        "\nDisabling module '{}'  (module_config: {}, {} source files)",
        module.name,
        module.module_config,
        module.files.len()
    );
    {
        let mut files: Vec<_> = module.files.iter().map(|(p, _)| p).collect();
        files.sort();
        files.dedup();
        for f in &files {
            println!("  {}", f.display());
        }
    }
    println!();

    let variants = kbuild::generate_disable_variants(&kbuild, &kconfig, module, &cv);

    if variants.is_empty() {
        println!("No single-symbol variant found that silences this module.");
        println!("The module may already be disabled, or requires multi-symbol changes.");
        return Ok(variants);
    }

    let show = variants.len().min(5);
    println!(
        "Found {} variant(s), showing top {} by fewest side effects.\n",
        variants.len(),
        show
    );

    for (i, v) in variants.iter().take(show).enumerate() {
        print_variant(i, v, verbose);
    }

    // Safety check: warn if any enabled symbol outside the best variant's disable
    // set would still force the target back on (should be empty after config-aware cascade).
    if let Some(best) = variants.first() {
        let disabled: HashSet<String> = std::iter::once(best.change.0.clone())
            .chain(best.cascade_configs().map(|s| s.to_owned()))
            .collect();
        let conflicts = kconfig.validate_disabled(&disabled, &cv);
        if !conflicts.is_empty() {
            println!(
                "Warning: the following symbol(s) would STILL re-enable CONFIG_{} \
                 after applying the best variant. Their select condition involves \
                 symbols outside the proposed disable set — a single-symbol change \
                 may be insufficient:",
                best.change.0
            );
            let mut cs: Vec<_> = conflicts.iter().collect();
            cs.sort();
            for c in cs {
                println!("  CONFIG_{}", c);
            }
            println!();
        }
    }

    Ok(variants)
}

fn print_variant(rank: usize, v: &kbuild::DisableVariant, verbose: bool) {
    let n_files = v.side_effect_files.len();
    let n_cascade = v.cascade_len();
    let rank_label = if rank == 0 { " ★ best" } else { "" };
    println!(
        "Variant {}{}  [{} collateral file{}, {} cascade config{}]",
        rank + 1,
        rank_label,
        n_files,
        if n_files == 1 { "" } else { "s" },
        n_cascade,
        if n_cascade == 1 { "" } else { "s" },
    );
    println!("  CONFIG_{}=n", v.change.0);

    if n_files == 0 {
        println!("  Collateral files   : none");
    } else {
        let mut files: Vec<_> = v.side_effect_files.iter().collect();
        files.sort();
        const TRUNCATE: usize = 10;
        println!("  Collateral files:");
        if verbose || files.len() <= TRUNCATE {
            for f in &files {
                println!("    {}", f.display());
            }
        } else {
            for f in files.iter().take(TRUNCATE) {
                println!("    {}", f.display());
            }
            println!("    ... ({} more, use --verbose to list all)", files.len() - TRUNCATE);
        }
    }

    if n_cascade == 0 {
        println!("  Cascade configs    : none");
    } else {
        println!("  Cascade configs (must also disable — would re-enable CONFIG_{} via `select`):", v.change.0);
        let mut uncond: Vec<_> = v.cascade_unconditional.iter().collect();
        uncond.sort();
        for c in &uncond {
            println!("    CONFIG_{}=n  (unconditional: {} selects {})", c, c, v.change.0);
        }
        let mut active: Vec<_> = v.cascade_active.iter().collect();
        active.sort();
        for c in &active {
            println!("    CONFIG_{}=n  (active select: {} selects {} if <cond>, currently true)", c, c, v.change.0);
        }
    }

    let n_collat = v.collateral_modules.len();
    if n_collat > 0 {
        println!(
            "  \u{26a0} Collateral modules (will also be disabled \u{2014} depends-on chain): {}",
            n_collat
        );
        for mod_name in &v.collateral_modules {
            println!("    {}", mod_name);
        }
    }

    println!();
}

// ── Self-test ─────────────────────────────────────────────────────────────────

const DEFAULT_CLONE_URL: &str =
    "https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git";

fn self_test_module_plan(args: ModulePlanArgs) -> anyhow::Result<()> {
    let ver = args
        .ver
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("--ver is required with --self-test"))?;

    // ── Step 1: prepare workdir ───────────────────────────────────────────────
    let workdir: PathBuf = if let Some(kernel_src) = &args.kernel_src {
        // Warn if --config points inside the workdir (would be overwritten)
        if let (Ok(cfg_canon), Ok(ksrc_canon)) =
            (args.config.canonicalize(), kernel_src.canonicalize())
        {
            if cfg_canon.starts_with(&ksrc_canon) {
                eprintln!(
                    "warning: --config ({}) is inside --kernel-src; \
                     it will be overwritten by the .config copy step",
                    args.config.display()
                );
            }
        }
        eprintln!("Using existing kernel tree: {}", kernel_src.display());
        eprintln!("Checking out tag {} ...", ver);
        run_git(&["-C", kernel_src.to_str().unwrap(), "checkout", ver])?;
        eprintln!("Running make mrproper ...");
        run_make(kernel_src, &["mrproper"], args.cc.as_deref(), args.verbose)?;
        kernel_src.clone()
    } else {
        let url = args.clone_from.as_deref().unwrap_or(DEFAULT_CLONE_URL);
        let dest = std::env::temp_dir().join(format!("kminimize-selftest-{}", std::process::id()));
        eprintln!("Cloning {} @ {} → {} ...", url, ver, dest.display());
        std::fs::create_dir_all(&dest)?;
        let shallow = url.starts_with("http://")
            || url.starts_with("https://")
            || url.starts_with("git://");
        let mut git_args: Vec<&str> = vec!["clone"];
        if shallow {
            git_args.extend_from_slice(&["--depth=1"]);
        }
        git_args.extend_from_slice(&["--branch", ver, url, dest.to_str().unwrap()]);
        run_git(&git_args)?;
        eprintln!("Cloned to {}", dest.display());
        dest
    };

    // ── Step 2: install .config + olddefconfig ────────────────────────────────
    let workdir_config = workdir.join(".config");
    eprintln!("Copying {} → {} ...", args.config.display(), workdir_config.display());
    std::fs::copy(&args.config, &workdir_config)?;
    eprintln!("Running make olddefconfig ...");
    run_make(&workdir, &["olddefconfig"], args.cc.as_deref(), args.verbose)?;

    // ── Step 3: first build ───────────────────────────────────────────────────
    let nproc = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    eprintln!("Building kernel (make -j{}) ...", nproc);
    run_make(&workdir, &[&format!("-j{}", nproc)], args.cc.as_deref(), args.verbose)?;

    // ── Step 4: pre-disable snapshot ─────────────────────────────────────────
    eprintln!("Scanning built files (pre-disable) ...");
    let pre_built = scan_built_c_files(&workdir)?;
    eprintln!("Pre-disable: {} .c files have a matching .o", pre_built.len());

    // ── Step 5: analysis ──────────────────────────────────────────────────────
    println!("\n── Module-plan analysis ──────────────────────────────────────────────────");
    let variants = run_analysis(&workdir, &args.module, &workdir_config, args.verbose)?;

    if variants.is_empty() {
        anyhow::bail!(
            "no disable variant found for module '{}'; cannot proceed with self-test",
            args.module
        );
    }
    let best = &variants[0];
    println!(
        "Using best variant: CONFIG_{}=n  ({} collateral files, {} cascade configs, {} collateral modules)\n",
        best.change.0,
        best.side_effect_files.len(),
        best.cascade_len(),
        best.collateral_modules.len(),
    );

    // ── Step 6: apply best variant ────────────────────────────────────────────
    eprintln!("Appending CONFIG changes to .config ...");
    {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&workdir_config)?;
        writeln!(f, "CONFIG_{}={}", best.change.0, best.change.1)?;
        for cascade in best.cascade_configs() {
            writeln!(f, "CONFIG_{}=n", cascade)?;
        }
    }

    // ── Step 6b: snapshot before olddefconfig ─────────────────────────────────
    let config_written = kbuild::parse_dotconfig(&workdir_config)
        .map_err(|e| anyhow::anyhow!("config pre-snapshot failed: {e}"))?;

    // ── Step 7: olddefconfig after change ─────────────────────────────────────
    eprintln!("Running make olddefconfig (post-change) ...");
    run_make(&workdir, &["olddefconfig"], args.cc.as_deref(), args.verbose)?;

    // ── Step 7b: detect extra symbols disabled by olddefconfig ────────────────
    let config_post_olddefconfig = kbuild::parse_dotconfig(&workdir_config)
        .map_err(|e| anyhow::anyhow!("config post-snapshot failed: {e}"))?;
    let extra_disabled = config_diff_disabled(&config_written, &config_post_olddefconfig);

    // ── Step 8: second build ──────────────────────────────────────────────────
    eprintln!("Rebuilding kernel (make -j{}) ...", nproc);
    run_make(&workdir, &[&format!("-j{}", nproc)], args.cc.as_deref(), args.verbose)?;

    // ── Step 9: post-disable snapshot ────────────────────────────────────────
    eprintln!("Scanning built files (post-disable) ...");
    let post_built = scan_built_c_files(&workdir)?;
    eprintln!("Post-disable: {} .c files have a matching .o", post_built.len());

    // ── Step 10: compare ──────────────────────────────────────────────────────
    // Load module file list from the pre-disable kbuild analysis.
    // We re-parse quickly just for the module file set.
    let kbuild = kbuild::KbuildMaps::load(&workdir)
        .map_err(|e| anyhow::anyhow!("kbuild reload failed: {e}"))?;
    let module = kbuild
        .module(&args.module)
        .ok_or_else(|| anyhow::anyhow!("module '{}' vanished after reload", args.module))?;

    let module_files: HashSet<PathBuf> = module.files.iter().map(|(p, _)| p.clone()).collect();

    let removed: HashSet<PathBuf> = pre_built.difference(&post_built).cloned().collect();
    let removed_module: HashSet<&PathBuf> = removed
        .iter()
        .filter(|f| module_files.contains(*f))
        .collect();
    let removed_collat: HashSet<PathBuf> = removed
        .iter()
        .filter(|f| !module_files.contains(*f))
        .cloned()
        .collect();

    // Augment predicted collateral with files controlled by symbols that
    // make olddefconfig additionally disabled (depends-on chain propagation).
    let extra_files: HashSet<PathBuf> = extra_disabled
        .iter()
        .flat_map(|sym| {
            kbuild
                .config_to_files()
                .get(sym.as_str())
                .into_iter()
                .flatten()
        })
        .filter(|f| !module_files.contains(*f))
        .cloned()
        .collect();
    let expected_collat: HashSet<PathBuf> =
        best.side_effect_files.union(&extra_files).cloned().collect();

    // ── Step 11: report ───────────────────────────────────────────────────────
    // make olddefconfig side effects
    if !extra_disabled.is_empty() {
        println!(
            "── make olddefconfig side effects ──────────────────────────────────────────"
        );
        let mut syms: Vec<_> = extra_disabled.iter().collect();
        syms.sort();
        println!(
            "  Symbols additionally disabled by make olddefconfig: {}",
            syms.len()
        );
        for sym in &syms {
            // Flag if this symbol is the module_config of any known module.
            let driven_modules: Vec<&str> = kbuild
                .modules()
                .iter()
                .filter(|(mod_name, m)| {
                    *mod_name != &args.module && m.module_config == **sym
                })
                .map(|(name, _)| name.as_str())
                .collect();
            if driven_modules.is_empty() {
                println!("    CONFIG_{}=n", sym);
            } else {
                let mut names = driven_modules;
                names.sort();
                println!(
                    "    CONFIG_{}=n  \u{26a0} also disables module(s): {}",
                    sym,
                    names.join(", ")
                );
            }
        }
        println!(
            "  Files additionally removed (included in expected collateral): {}",
            extra_files.len()
        );
        println!();
    }

    println!("── Self-test results ─────────────────────────────────────────────────────");
    println!(
        "  Module files removed from build : {}/{}",
        removed_module.len(),
        module_files.len()
    );
    println!("  Collateral files removed         : {}", removed_collat.len());
    println!(
        "  Collateral files predicted        : {} ({} static + {} from olddefconfig)",
        expected_collat.len(),
        best.side_effect_files.len(),
        extra_files.len(),
    );

    let false_positives: HashSet<&PathBuf> = expected_collat.difference(&removed_collat).collect();
    let missed: HashSet<&PathBuf> = removed_collat.difference(&expected_collat).collect();

    if false_positives.is_empty() && missed.is_empty() {
        println!("\n  PASS: predictions match reality exactly");
    } else {
        println!();
        if !false_positives.is_empty() {
            println!(
                "  FALSE POSITIVES (predicted but build did not remove, {}):",
                false_positives.len()
            );
            let mut fps: Vec<_> = false_positives.iter().collect();
            fps.sort();
            for f in fps {
                println!("    {}", f.display());
            }
        }
        if !missed.is_empty() {
            println!(
                "  MISSED (build removed but not predicted, {}):",
                missed.len()
            );
            let mut ms: Vec<_> = missed.iter().collect();
            ms.sort();
            for f in ms {
                println!("    {}", f.display());
            }
        }
        println!("\n  FAIL");
        std::process::exit(2);
    }

    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Symbols that were enabled (y/m) in `before` and are now absent or `n` in `after`.
/// These are the configs that `make olddefconfig` automatically disabled (depends-on chain).
fn config_diff_disabled(
    before: &HashMap<String, String>,
    after: &HashMap<String, String>,
) -> HashSet<String> {
    before
        .iter()
        .filter(|(_, v)| *v == "y" || *v == "m")
        .filter(|(k, _)| after.get(*k).map(|v| v == "n").unwrap_or(true))
        .map(|(k, _)| k.clone())
        .collect()
}

/// Returns kernel-root-relative paths of `.c` files that have a matching `.o`
/// in the same directory (i.e. were compiled in the last build).
fn scan_built_c_files(kernel_src: &Path) -> anyhow::Result<HashSet<PathBuf>> {
    let mut result = HashSet::new();
    scan_dir_for_built(kernel_src, kernel_src, &mut result)?;
    Ok(result)
}

fn scan_dir_for_built(
    dir: &Path,
    kernel_src: &Path,
    out: &mut HashSet<PathBuf>,
) -> anyhow::Result<()> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return Ok(()), // skip unreadable dirs
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if ft.is_dir() {
            // Skip known non-source directories
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == ".git" || name == "Documentation" || name == "tools" {
                continue;
            }
            scan_dir_for_built(&path, kernel_src, out)?;
        } else if ft.is_file() {
            if path.extension().map(|e| e == "o").unwrap_or(false) {
                let c_path = path.with_extension("c");
                if c_path.exists() {
                    let rel = c_path
                        .strip_prefix(kernel_src)
                        .unwrap_or(&c_path)
                        .to_path_buf();
                    out.insert(rel);
                }
            }
        }
    }
    Ok(())
}

fn run_make(workdir: &Path, args: &[&str], cc: Option<&str>, verbose: bool) -> anyhow::Result<()> {
    let mut cmd = std::process::Command::new("make");
    cmd.current_dir(workdir).args(args);
    if let Some(compiler) = cc {
        cmd.arg(format!("CC={compiler}"))
            .arg(format!("HOSTCC={compiler}"));
    }
    if !verbose {
        cmd.stdout(std::process::Stdio::null());
    }
    let status = cmd.status()?;
    if !status.success() {
        anyhow::bail!("make {} exited with {}", args.join(" "), status);
    }
    Ok(())
}

fn run_git(args: &[&str]) -> anyhow::Result<()> {
    let status = std::process::Command::new("git").args(args).status()?;
    if !status.success() {
        anyhow::bail!("git {} exited with {}", args.join(" "), status);
    }
    Ok(())
}

fn remote_gather(args: RemoteGatherArgs) -> anyhow::Result<()> {
    use std::thread;
    use std::time::{Duration, Instant};

    let duration = Duration::from_secs(args.duration);
    let interval = Duration::from_secs(args.interval);
    let start = Instant::now();

    let mut snapshots: Vec<gather::ModuleSnapshot> = Vec::new();

    eprintln!(
        "remote-gather: {}:{} — interval {}s, duration {}s",
        args.ssh_url, args.port, args.interval, args.duration,
    );

    loop {
        let output = std::process::Command::new("ssh")
            .arg("-p")
            .arg(args.port.to_string())
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("ConnectTimeout=10")
            .arg("-o")
            .arg("StrictHostKeyChecking=no")
            .arg("-o")
            .arg("UserKnownHostsFile=/dev/null")
            .arg(&args.ssh_url)
            .arg("cat /proc/modules")
            .output()
            .map_err(|e| anyhow::anyhow!("failed to spawn ssh: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("ssh exited with {}: {}", output.status, stderr.trim());
        }

        let content = String::from_utf8(output.stdout)
            .map_err(|_| anyhow::anyhow!("ssh output is not valid UTF-8"))?;

        let snap = gather::snapshot_from_content(&content)
            .map_err(|e| anyhow::anyhow!("failed to parse /proc/modules: {e}"))?;

        eprintln!(
            "remote-gather: snapshot at {} — {} modules",
            snap.timestamp_secs,
            snap.modules.len(),
        );
        snapshots.push(snap);

        let elapsed = start.elapsed();
        if elapsed >= duration {
            break;
        }
        let remaining = duration - elapsed;
        thread::sleep(remaining.min(interval));
    }

    if snapshots.is_empty() {
        anyhow::bail!("no snapshots collected");
    }

    // Find the snapshot with the largest total use_count across Live modules
    // (highest combined reference count = peak usage exposure).
    let best = snapshots
        .iter()
        .max_by_key(|s| {
            s.modules
                .iter()
                .filter(|m| m.state == gather::ModuleState::Live)
                .map(|m| m.use_count as u64)
                .sum::<u64>()
        })
        .unwrap();

    println!();
    println!("Peak-usage snapshot (highest total use_count across {} snapshots):", snapshots.len());
    print_snapshot(best);

    Ok(())
}

fn print_snapshot(snap: &gather::ModuleSnapshot) {
    let active_count = snap
        .modules
        .iter()
        .filter(|m| m.state == gather::ModuleState::Live && m.use_count > 0)
        .count();
    let total_use: u64 = snap
        .modules
        .iter()
        .filter(|m| m.state == gather::ModuleState::Live)
        .map(|m| m.use_count as u64)
        .sum();

    println!(
        "  timestamp : {} (Unix seconds)",
        snap.timestamp_secs
    );
    println!(
        "  modules   : {} loaded, {} active (use_count > 0), total use_count = {}",
        snap.modules.len(),
        active_count,
        total_use,
    );
    println!();

    let mut live: Vec<&gather::ModuleEntry> = snap
        .modules
        .iter()
        .filter(|m| m.state == gather::ModuleState::Live)
        .collect();
    live.sort_by(|a, b| b.use_count.cmp(&a.use_count).then(a.name.cmp(&b.name)));

    println!("{:<36} {:>9}  {}", "module", "use_count", "used_by");
    println!("{}", "-".repeat(72));
    for m in &live {
        let used_by = if m.used_by.is_empty() {
            "-".to_owned()
        } else {
            m.used_by.join(", ")
        };
        let oot = if m.is_out_of_tree { "  (out-of-tree)" } else { "" };
        println!("{:<36} {:>9}  {}{}", m.name, m.use_count, used_by, oot);
    }

    let transitional: Vec<&gather::ModuleEntry> = snap
        .modules
        .iter()
        .filter(|m| m.state != gather::ModuleState::Live)
        .collect();
    if !transitional.is_empty() {
        println!();
        println!("  Transitional modules ({}):", transitional.len());
        for m in &transitional {
            println!("    {:?}  {}", m.state, m.name);
        }
    }
}
