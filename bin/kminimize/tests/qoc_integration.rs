use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

/// Removes a directory tree on drop unless `keep` is set.
/// Set `KMINIMIZE_KEEP_TESTDIR=1` to suppress deletion for post-test analysis.
struct TempDir {
    path: PathBuf,
    keep: bool,
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if self.keep {
            println!("KMINIMIZE_KEEP_TESTDIR set — leaving test directory at {}", self.path.display());
        } else {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

/// Runs `argv[0]` with `argv[1..]` in `dir`, returning an error if the
/// process cannot be spawned or exits with a non-zero status.
fn run_cmd(dir: &Path, argv: &[&str]) -> anyhow::Result<()> {
    let status = Command::new(argv[0])
        .args(&argv[1..])
        .current_dir(dir)
        .status()
        .map_err(|e| anyhow::anyhow!("failed to spawn '{}': {e}", argv[0]))?;
    if !status.success() {
        anyhow::bail!("'{}' failed with {}", argv.join(" "), status);
    }
    Ok(())
}

/// Like `run_cmd` but captures stdout and returns it as a trimmed String.
fn run_cmd_output(dir: &Path, argv: &[&str]) -> anyhow::Result<String> {
    let out = Command::new(argv[0])
        .args(&argv[1..])
        .current_dir(dir)
        .output()
        .map_err(|e| anyhow::anyhow!("failed to spawn '{}': {e}", argv[0]))?;
    if !out.status.success() {
        anyhow::bail!("'{}' failed with {}", argv.join(" "), out.status);
    }
    String::from_utf8(out.stdout)
        .map(|s| s.trim().to_owned())
        .map_err(|_| anyhow::anyhow!("output of '{}' is not UTF-8", argv[0]))
}

/// Runs make, streams stdout to the terminal line-by-line, and returns the
/// bzImage path extracted from the "Kernel: <path> is ready" line.
fn run_make_capture_bzimage(dir: &Path, argv: &[&str]) -> anyhow::Result<PathBuf> {
    let mut child = Command::new(argv[0])
        .args(&argv[1..])
        .current_dir(dir)
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn '{}': {e}", argv[0]))?;

    let stdout = child.stdout.take().unwrap();
    let mut bzimage: Option<PathBuf> = None;

    for line in BufReader::new(stdout).lines() {
        let line = line.map_err(|e| anyhow::anyhow!("read error during make: {e}"))?;
        println!("{line}");
        // make emits e.g. "  Kernel: arch/x86/boot/bzImage is ready  (#3)"
        if let Some(rest) = line.trim().strip_prefix("Kernel: ") {
            if let Some(path_str) = rest.split(" is ready").next() {
                bzimage = Some(dir.join(path_str.trim()));
            }
        }
    }

    let status = child.wait()?;
    if !status.success() {
        anyhow::bail!("'{}' failed with {}", argv.join(" "), status);
    }
    bzimage.ok_or_else(|| anyhow::anyhow!("make did not emit a 'Kernel: ... is ready' line"))
}

/// Queries the remote for all tags and returns the one that matches
/// `v{upstream_version}` exactly, or the first tag that starts with that
/// prefix (covers release-candidate suffixes).
///
/// Uses `git ls-remote --tags origin` so no local fetch is required.
fn upstream_version_to_git_tag(kernel_src: &Path, upstream_version: &str) -> anyhow::Result<String> {
    let target = format!("v{upstream_version}");

    let out = Command::new("git")
        .args(["ls-remote", "--tags", "origin"])
        .current_dir(kernel_src)
        .output()
        .map_err(|e| anyhow::anyhow!("failed to spawn git: {e}"))?;

    if !out.status.success() {
        anyhow::bail!(
            "git ls-remote failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let stdout = String::from_utf8_lossy(&out.stdout);

    // Output format: "<sha>\trefs/tags/<name>" — peeled refs end with "^{}".
    let mut prefix_match: Option<String> = None;
    for line in stdout.lines() {
        let refs_part = match line.split('\t').nth(1) {
            Some(r) => r,
            None => continue,
        };
        let tag_name = match refs_part.strip_prefix("refs/tags/") {
            Some(n) => n.trim_end_matches("^{}"),
            None => continue,
        };
        if tag_name == target {
            return Ok(tag_name.to_owned());
        }
        if prefix_match.is_none() && tag_name.starts_with(&target) {
            prefix_match = Some(tag_name.to_owned());
        }
    }

    prefix_match.ok_or_else(|| anyhow::anyhow!("no remote tag found matching {target}"))
}

// Run with:
//   TEST_KERNEL_SRC=/path/to/linux \
//     cargo test -p kminimize vm_module_stats_integration -- --ignored --nocapture
#[test]
#[ignore = "requires VM infrastructure (QEMU, virtiofsd, proot) and a kernel build toolchain; set TEST_KERNEL_SRC and run with --ignored"]
fn vm_module_stats_integration() {
    let kernel_src_str = env::var("TEST_KERNEL_SRC").unwrap_or_else(|_| {
        panic!(
            "TEST_KERNEL_SRC is not set.\n\
             Provide a Linux kernel source tree:\n  \
             TEST_KERNEL_SRC=/path/to/linux \
             cargo test -p kminimize vm_module_stats_integration -- --ignored --nocapture"
        )
    });
    let kernel_src = PathBuf::from(&kernel_src_str);
    assert!(
        kernel_src.exists(),
        "TEST_KERNEL_SRC path does not exist: {}",
        kernel_src.display()
    );

    // /tmp/kminimize-test-<pid>/kminimize-test-rootfs
    let test_dir = PathBuf::from(format!("/tmp/kminimize-test-{}", std::process::id()));
    let rootfs_path = test_dir.join("kminimize-test-rootfs");

    fs::create_dir_all(&test_dir).expect("failed to create temp dir");
    let keep = env::var_os("KMINIMIZE_KEEP_TESTDIR").is_some();
    if keep {
        println!("KMINIMIZE_KEEP_TESTDIR set — test directory will be kept at {}", test_dir.display());
    }
    let _cleanup = TempDir { path: test_dir, keep };

    // Step 1: create VM rootfs; returns the kernel versions available after creation.
    println!("Creating Debian rootfs at {}", rootfs_path.display());
    let boot_kernels = qoc::create(&qoc::Debian, rootfs_path.clone()).expect("qoc::create failed");
    assert_eq!(boot_kernels.len(), 1, "expected exactly one kernel after create, found: {boot_kernels:?}");
    let boot_kernel_ver = boot_kernels.into_iter().next().unwrap();
    println!("boot kernel: {boot_kernel_ver}");

    // Step 2: boot the VM.
    println!("Starting VM...");
    let (handle, _) =
        qoc::start(rootfs_path.clone(), 1, false, &boot_kernel_ver).expect("qoc::start failed");

    // Step 3: wait for the VM to become reachable and collect kernel info.
    println!("Waiting for VmInfo...");
    let info = handle.wait_for_info().expect("wait_for_info failed");

    let kernel_version = info
        .kernel_version
        .expect("VmInfo::kernel_version is None");
    let upstream_version = info
        .upstream_version
        .expect("VmInfo::upstream_version is None");
    let kernel_config_body = info
        .kernel_config
        .expect("VmInfo::kernel_config is None — kernel must be built with CONFIG_IKCONFIG_PROC or have /boot/config-*");

    println!("kernel_version:   {kernel_version}");
    println!("upstream_version: {upstream_version}");
    println!("kernel_config:    {} bytes", kernel_config_body.len());

    // Step 4: gather /proc/modules snapshots every second for 10 seconds.
    let mut module_list = gather::ModuleList::new();
    for i in 0..10usize {
        let ssh_out = handle
            .ssh(&["cat", "/proc/modules"])
            .unwrap_or_else(|e| panic!("ssh failed on iteration {i}: {e}"));
        assert!(
            ssh_out.success,
            "cat /proc/modules returned non-zero on iteration {i}"
        );

        let snapshot = gather::snapshot_from_content(&ssh_out.stdout)
            .unwrap_or_else(|e| panic!("failed to parse /proc/modules on iteration {i}: {e}"));

        println!("snapshot {}: {} modules", i + 1, snapshot.modules.len());
        module_list.snapshots.push(snapshot);

        if i < 9 {
            thread::sleep(Duration::from_secs(1));
        }
    }

    // Step 4b: collect device aliases and modules.alias — mirrors what kgather does locally.
    // Shell glob follows symlinks (unlike find without -L), which is required because
    // /sys/bus/*/devices/* entries are symlinks to the real device directories.
    let alias_out = handle
        .ssh(&["sh", "-c", "cat /sys/bus/*/devices/*/modalias 2>/dev/null"])
        .expect("failed to collect device aliases");
    module_list.device_aliases = gather::parse_device_aliases(&alias_out.stdout);
    println!("device aliases: {}", module_list.device_aliases.len());

    let mod_alias_path = format!("/lib/modules/{}/modules.alias", kernel_version);
    let mod_alias_out = handle
        .ssh(&["cat", &mod_alias_path])
        .expect("failed to collect modules.alias");
    module_list.modules_alias = gather::parse_modules_alias(&mod_alias_out.stdout);
    println!("modules.alias entries: {}", module_list.modules_alias.len());

    // Step 5: find the canonical upstream git tag.
    let git_tag = upstream_version_to_git_tag(&kernel_src, &upstream_version)
        .expect("failed to find upstream git tag");
    println!("upstream git tag: {git_tag}");

    // Step 6: VM is no longer needed — terminate it before the build work.
    println!("Stopping VM...");
    handle.stop().expect("VM stop failed");

    // Step 7: check out the exact upstream tag and clean the source tree.
    println!("Checking out {git_tag}...");
    run_cmd(&kernel_src, &["git", "checkout", &git_tag])
        .expect("git checkout failed");
    run_cmd(&kernel_src, &["git", "clean", "-fddx"])
        .expect("git clean failed");

    // Step 8: place the VM's kernel config as .config.
    let dot_config = kernel_src.join(".config");
    fs::write(&dot_config, &kernel_config_body).expect("failed to write .config");
    println!(".config written ({} bytes)", kernel_config_body.len());

    // Step 9: normalise the config for the checked-out source version.
    println!("Running make olddefconfig (initial)...");
    run_cmd(&kernel_src, &["make", "olddefconfig"])
        .expect("make olddefconfig (initial) failed");

    // Step 9.5: apply known changes, each with a list of candidate commits.
    // For each change the first successfully cherry-picked commit wins; the
    // rest of the candidates for that change are skipped.  All changes are
    // attempted regardless of whether any individual one succeeded.
    const PATCHES: &[(&str, &[&str])] = &[
        (
            "libbpf: Fix -Wdiscarded-qualifiers under C23",
            &[
                "d70f79fef65810faf64dbae1f3a1b5623cdb2345",
                "ab21cf885fb2af179c44d8beeabd716133b9385d",
                "3dedeeecd1ae42a751721d83dc21877122cc1795",
                "bb42e9627aa92a0d6482a599cbea58708f1c3c63",
            ],
        ),
        (
            "KVM: VMX: Make vmread_error_trampoline() uncallable from C code",
            &["50bdbfa5d2fb29d548435de8ece57788d01e201f"],
        ),
    ];
    println!("Applying patch commits...");
    for (change, hashes) in PATCHES {
        println!("  Change: {change}");
        let mut applied = false;
        for hash in *hashes {
            let status = Command::new("git")
                .args(["cherry-pick", hash])
                .current_dir(&kernel_src)
                .status()
                .expect("failed to spawn git cherry-pick");
            if status.success() {
                println!("    Applied {hash}.");
                applied = true;
                break;
            }
            run_cmd(&kernel_src, &["git", "cherry-pick", "--abort"])
                .expect("git cherry-pick --abort failed");
        }
        if !applied {
            println!("    No candidate commit could be applied.");
        }
    }

    // Step 10: apply config reduction using the existing disable logic.
    println!("Reducing config...");
    kminimize::reduce_config(&kernel_src, &module_list, &dot_config)
        .expect("reduce_config failed");

    // Step 11: normalise again after reduction so cascades are resolved.
    println!("Running make olddefconfig (post-reduction)...");
    run_cmd(&kernel_src, &["make", "olddefconfig"])
        .expect("make olddefconfig (post-reduction) failed");

    // Step 12: build the kernel; capture bzImage path from make output.
    println!("Building kernel (make -j24)...");
    let bzimage_path = run_make_capture_bzimage(&kernel_src, &["make", "-j24"])
        .expect("kernel build failed");
    println!("bzImage: {}", bzimage_path.display());

    // Step 13: get the kernel release string (appended to /lib/modules/<release>).
    println!("Getting kernel release string...");
    let kernel_release = run_cmd_output(&kernel_src, &["make", "-s", "kernelrelease"])
        .expect("make kernelrelease failed");
    println!("kernel release: {kernel_release}");

    // Step 14: install bzImage and kernel config into rootfs /boot.
    let boot_dir = rootfs_path.join("boot");
    fs::create_dir_all(&boot_dir).expect("failed to create rootfs /boot");
    let vmlinuz_dest = boot_dir.join(format!("vmlinuz-{kernel_release}"));
    fs::copy(&bzimage_path, &vmlinuz_dest).expect("failed to copy bzImage to rootfs /boot");
    println!("Installed {}", vmlinuz_dest.display());
    let config_dest = boot_dir.join(format!("config-{kernel_release}"));
    fs::copy(&dot_config, &config_dest).expect("failed to copy .config to rootfs /boot");
    println!("Installed {}", config_dest.display());

    // Step 15: install kernel modules into rootfs.
    println!("Installing modules (make modules_install)...");
    let install_mod_path = format!("INSTALL_MOD_PATH={}", rootfs_path.display());
    run_cmd(&kernel_src, &["make", "modules_install", &install_mod_path])
        .expect("make modules_install failed");
    println!("Modules installed.");

    // Step 16: regenerate initrd for the new kernel in the rootfs.
    println!("Regenerating initrd...");
    let initrd_path = qoc::make_initrd(&rootfs_path, &kernel_release)
        .expect("make_initrd failed");
    println!("initrd: {}", initrd_path.display());

    // Step 17: list all kernel+initrd pairs now present in the rootfs.
    println!("Listing kernels in rootfs...");
    let kernels = qoc::list_kernels(&rootfs_path).expect("list_kernels failed");
    println!("Available kernels ({}):", kernels.len());
    for k in &kernels {
        println!("  {k}");
    }

    println!("All steps completed successfully.");
    assert_eq!(module_list.snapshots.len(), 10);
    assert!(!git_tag.is_empty());
    assert!(
        kernels.iter().any(|k| k == &kernel_release),
        "newly built kernel '{kernel_release}' not found in list_kernels output"
    );
}
