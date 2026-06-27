use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::load_ltp;

static KGATHER_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/kgather"));

struct TempDir {
    path: PathBuf,
    keep: bool,
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if self.keep {
            println!("--keep-dir set — leaving test directory at {}", self.path.display());
        } else {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

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

fn scp_to_vm(port: u16, local_path: &Path, remote_path: &str) -> anyhow::Result<()> {
    let port_str = port.to_string();
    let dest = format!("root@localhost:{remote_path}");
    let status = Command::new("scp")
        .args([
            "-P",
            &port_str,
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            local_path.to_str().unwrap(),
            &dest,
        ])
        .status()
        .map_err(|e| anyhow::anyhow!("failed to spawn scp: {e}"))?;
    if !status.success() {
        anyhow::bail!("scp to VM failed with {status}");
    }
    Ok(())
}

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

fn gather_collect_json(handle: &qoc::VmHandle, work_dir: &Path) -> anyhow::Result<gather::ModuleList> {
    let json_out = handle
        .ssh(&["cat", "/tmp/modules.json"])
        .map_err(|e| anyhow::anyhow!("failed to read modules.json from VM: {e}"))?;
    anyhow::ensure!(json_out.success, "cat /tmp/modules.json failed");

    let path = work_dir.join("modules.json");
    fs::write(&path, json_out.stdout.as_bytes())
        .map_err(|e| anyhow::anyhow!("failed to write modules.json locally: {e}"))?;

    gather::ModuleList::load(&path)
        .map_err(|e| anyhow::anyhow!("failed to parse modules.json: {e}"))
}

fn run_load(handle: &qoc::VmHandle, load_type: crate::LoadType) -> anyhow::Result<()> {
    match load_type {
        crate::LoadType::Ltp => load_ltp::run(handle),
    }
}

pub fn run(args: crate::SelfTestArgs) -> anyhow::Result<()> {
    let kernel_src = args.linux_dir;
    anyhow::ensure!(
        kernel_src.exists(),
        "--linux-dir path does not exist: {}",
        kernel_src.display()
    );

    let test_dir = PathBuf::from(format!("/tmp/kminimize-test-{}", std::process::id()));
    let rootfs_path = test_dir.join("kminimize-test-rootfs");

    fs::create_dir_all(&test_dir)
        .map_err(|e| anyhow::anyhow!("failed to create temp dir {}: {e}", test_dir.display()))?;
    if args.keep_dir {
        println!("--keep-dir set — test directory will be kept at {}", test_dir.display());
    }
    let _cleanup = TempDir { path: test_dir, keep: args.keep_dir };

    let distro = args.distro.build();

    // Step 1: create VM rootfs; returns the kernel versions available after creation.
    println!("Creating {} rootfs at {}", distro.name(), rootfs_path.display());
    let boot_kernels = qoc::create(&*distro, rootfs_path.clone())
        .map_err(|e| anyhow::anyhow!("qoc::create failed: {e}"))?;
    anyhow::ensure!(
        boot_kernels.len() == 1,
        "expected exactly one kernel after create, found: {boot_kernels:?}"
    );
    let boot_kernel_ver = boot_kernels.into_iter().next().unwrap();
    println!("boot kernel: {boot_kernel_ver}");

    // Step 2: boot the VM.
    println!("Starting VM...");
    let (handle, _) = qoc::start(rootfs_path.clone(), 1, false, &boot_kernel_ver)
        .map_err(|e| anyhow::anyhow!("qoc::start failed: {e}"))?;

    // Step 3: wait for the VM to become reachable and collect kernel info.
    println!("Waiting for VmInfo...");
    let info = handle
        .wait_for_info()
        .map_err(|e| anyhow::anyhow!("wait_for_info failed: {e}"))?;

    let kernel_version = info
        .kernel_version
        .ok_or_else(|| anyhow::anyhow!("VmInfo::kernel_version is None"))?;
    let upstream_version = info
        .upstream_version
        .ok_or_else(|| anyhow::anyhow!("VmInfo::upstream_version is None"))?;
    let kernel_config_body = info
        .kernel_config
        .ok_or_else(|| anyhow::anyhow!("VmInfo::kernel_config is None — kernel must be built with CONFIG_IKCONFIG_PROC or have /boot/config-*"))?;

    println!("kernel_version:   {kernel_version}");
    println!("upstream_version: {upstream_version}");
    println!("kernel_config:    {} bytes", kernel_config_body.len());

    // Step 4: deploy the embedded static kgather binary to the VM.
    let kgather_host_path = _cleanup.path.join("kgather");
    fs::write(&kgather_host_path, KGATHER_BIN)
        .map_err(|e| anyhow::anyhow!("failed to write kgather binary: {e}"))?;
    fs::set_permissions(&kgather_host_path, fs::Permissions::from_mode(0o755))
        .map_err(|e| anyhow::anyhow!("failed to chmod kgather: {e}"))?;

    println!("Copying kgather to VM...");
    scp_to_vm(handle.ssh_port, &kgather_host_path, "/tmp/kgather")
        .map_err(|e| anyhow::anyhow!("failed to copy kgather to VM: {e}"))?;

    // Step 4b: run kgather — either finite (no load) or background + load + kill (with load).
    let module_list = if let Some(load_type) = args.load_type {
        // Start kgather in background on the VM (no --duration), capture its PID.
        println!("Starting kgather on VM in background...");
        let bg = handle
            .ssh(&["nohup /tmp/kgather /tmp/modules.json 1 >/tmp/kgather.log 2>&1 & echo $!"])
            .map_err(|e| anyhow::anyhow!("failed to start background kgather: {e}"))?;
        anyhow::ensure!(bg.success, "background kgather launch failed");
        let kgather_pid = bg.stdout.trim().to_owned();
        anyhow::ensure!(!kgather_pid.is_empty(), "kgather PID was empty");
        println!("kgather PID: {kgather_pid}");

        // Run the load; it drives the observation window.
        run_load(&handle, load_type)?;

        // Terminate kgather and give it a moment to finish the last JSON write.
        handle
            .ssh(&["kill", &kgather_pid])
            .map_err(|e| anyhow::anyhow!("failed to kill kgather: {e}"))?;
        handle
            .ssh(&["sleep", "1"])
            .map_err(|e| anyhow::anyhow!("post-kill sleep failed: {e}"))?;

        // Print kgather's log for visibility.
        if let Ok(log) = handle.ssh(&["cat", "/tmp/kgather.log"]) {
            print!("{}", log.stdout.trim());
        }

        gather_collect_json(&handle, &_cleanup.path)?
    } else {
        // No load: run kgather synchronously for a fixed duration.
        println!("Running kgather on VM (15 seconds)...");
        let gather_run = handle
            .ssh(&["/tmp/kgather", "/tmp/modules.json", "1", "--duration", "15"])
            .map_err(|e| anyhow::anyhow!("kgather SSH call failed: {e}"))?;
        anyhow::ensure!(
            gather_run.success,
            "kgather exited with error:\n{}",
            gather_run.stderr
        );
        println!("{}", gather_run.stderr.trim());

        gather_collect_json(&handle, &_cleanup.path)?
    };

    println!(
        "Gathered: {} snapshots, {} device aliases, {} alias entries, {} pinned configs",
        module_list.snapshots.len(),
        module_list.device_aliases.len(),
        module_list.modules_alias.len(),
        module_list.pinned_configs.len(),
    );
    let modules_json_path = _cleanup.path.join("modules.json");
    if args.keep_dir {
        println!("Gather data at {}", modules_json_path.display());
    }

    // Step 5: find the canonical upstream git tag.
    let git_tag = upstream_version_to_git_tag(&kernel_src, &upstream_version)
        .map_err(|e| anyhow::anyhow!("failed to find upstream git tag: {e}"))?;
    println!("upstream git tag: {git_tag}");

    // Step 6: VM is no longer needed — terminate it before the build work.
    println!("Stopping VM...");
    handle
        .stop()
        .map_err(|e| anyhow::anyhow!("VM stop failed: {e}"))?;

    let kernels_before_build = qoc::list_kernels(&rootfs_path)
        .map_err(|e| anyhow::anyhow!("list_kernels failed: {e}"))?;
    println!("Kernels before build ({}):", kernels_before_build.len());
    for k in &kernels_before_build {
        println!("  {k}");
    }

    // Step 7: check out the exact upstream tag and clean the source tree.
    println!("Checking out {git_tag}...");
    run_cmd(&kernel_src, &["git", "checkout", &git_tag])
        .map_err(|e| anyhow::anyhow!("git checkout failed: {e}"))?;
    run_cmd(&kernel_src, &["git", "clean", "-fddx"])
        .map_err(|e| anyhow::anyhow!("git clean failed: {e}"))?;

    // Step 8: place the VM's kernel config as .config.
    let dot_config = kernel_src.join(".config");
    fs::write(&dot_config, &kernel_config_body)
        .map_err(|e| anyhow::anyhow!("failed to write .config: {e}"))?;
    println!(".config written ({} bytes)", kernel_config_body.len());

    // Step 9: normalise the config for the checked-out source version.
    println!("Running make olddefconfig (initial)...");
    run_cmd(&kernel_src, &["make", "olddefconfig"])
        .map_err(|e| anyhow::anyhow!("make olddefconfig (initial) failed: {e}"))?;

    // Step 9.5: apply known changes, each with a list of candidate commits.
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
                .map_err(|e| anyhow::anyhow!("failed to spawn git cherry-pick: {e}"))?;
            if status.success() {
                println!("    Applied {hash}.");
                applied = true;
                break;
            }
            run_cmd(&kernel_src, &["git", "cherry-pick", "--abort"])
                .map_err(|e| anyhow::anyhow!("git cherry-pick --abort failed: {e}"))?;
        }
        if !applied {
            println!("    No candidate commit could be applied.");
        }
    }

    // Step 10: apply config reduction using the existing disable logic.
    println!("Reducing config...");
    kminimize::reduce_config(&kernel_src, &module_list, &dot_config)
        .map_err(|e| anyhow::anyhow!("reduce_config failed: {e}"))?;

    // Step 11: normalise again after reduction so cascades are resolved.
    println!("Running make olddefconfig (post-reduction)...");
    run_cmd(&kernel_src, &["make", "olddefconfig"])
        .map_err(|e| anyhow::anyhow!("make olddefconfig (post-reduction) failed: {e}"))?;

    // Step 12: build the kernel; capture bzImage path from make output.
    println!("Building kernel (make -j24)...");
    let bzimage_path = run_make_capture_bzimage(&kernel_src, &["make", "-j24"])
        .map_err(|e| anyhow::anyhow!("kernel build failed: {e}"))?;
    println!("bzImage: {}", bzimage_path.display());

    // Step 13: get the kernel release string.
    println!("Getting kernel release string...");
    let kernel_release = run_cmd_output(&kernel_src, &["make", "-s", "kernelrelease"])
        .map_err(|e| anyhow::anyhow!("make kernelrelease failed: {e}"))?;
    println!("kernel release: {kernel_release}");

    // Step 14: install bzImage and kernel config into rootfs /boot.
    let boot_dir = rootfs_path.join("boot");
    fs::create_dir_all(&boot_dir)
        .map_err(|e| anyhow::anyhow!("failed to create rootfs /boot: {e}"))?;
    let vmlinuz_dest = boot_dir.join(format!("vmlinuz-{kernel_release}"));
    fs::copy(&bzimage_path, &vmlinuz_dest)
        .map_err(|e| anyhow::anyhow!("failed to copy bzImage to rootfs /boot: {e}"))?;
    println!("Installed {}", vmlinuz_dest.display());
    let config_dest = boot_dir.join(format!("config-{kernel_release}"));
    fs::copy(&dot_config, &config_dest)
        .map_err(|e| anyhow::anyhow!("failed to copy .config to rootfs /boot: {e}"))?;
    println!("Installed {}", config_dest.display());

    // Step 15: install kernel modules into rootfs.
    println!("Installing modules (make modules_install)...");
    let install_mod_path = format!("INSTALL_MOD_PATH={}", rootfs_path.display());
    run_cmd(&kernel_src, &["make", "modules_install", &install_mod_path])
        .map_err(|e| anyhow::anyhow!("make modules_install failed: {e}"))?;
    println!("Modules installed.");

    // Step 16: regenerate initrd for the new kernel in the rootfs.
    println!("Regenerating initrd...");
    let initrd_path = qoc::make_initrd(&rootfs_path, &kernel_release)
        .map_err(|e| anyhow::anyhow!("make_initrd failed: {e}"))?;
    println!("initrd: {}", initrd_path.display());

    // Step 17: list kernels and assert exactly one was added by the build.
    println!("Listing kernels in rootfs...");
    let kernels = qoc::list_kernels(&rootfs_path)
        .map_err(|e| anyhow::anyhow!("list_kernels failed: {e}"))?;
    println!("Available kernels ({}):", kernels.len());
    for k in &kernels {
        println!("  {k}");
    }
    anyhow::ensure!(
        kernels.len() == kernels_before_build.len() + 1,
        "expected exactly one new kernel; before: {kernels_before_build:?}, after: {kernels:?}"
    );
    anyhow::ensure!(
        kernels.iter().any(|k| k == &kernel_release),
        "newly built kernel '{kernel_release}' not found in list_kernels output"
    );

    // Step 18: boot the VM with the reduced kernel; stream QEMU output to console.
    println!("Booting reduced kernel {kernel_release} (show_log=true)...");
    let (handle2, _) = qoc::start(rootfs_path.clone(), 1, true, &kernel_release)
        .map_err(|e| anyhow::anyhow!("qoc::start (reduced kernel) failed: {e}"))?;
    let info2 = handle2
        .wait_for_info()
        .map_err(|e| anyhow::anyhow!("wait_for_info (reduced kernel) failed: {e}"))?;
    println!(
        "Reduced kernel booted: version={:?} upstream={:?}",
        info2.kernel_version, info2.upstream_version
    );
    handle2
        .stop()
        .map_err(|e| anyhow::anyhow!("VM stop (reduced kernel) failed: {e}"))?;

    println!("All steps completed successfully.");
    anyhow::ensure!(!module_list.snapshots.is_empty(), "kgather produced no snapshots");
    anyhow::ensure!(!git_tag.is_empty(), "git_tag is empty");

    Ok(())
}
