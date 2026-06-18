use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

/// Removes a directory tree on drop, silently ignoring errors.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
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
#[ignore = "requires VM infrastructure (QEMU, virtiofsd, proot); set TEST_KERNEL_SRC and run with --ignored"]
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
    let _cleanup = TempDir(test_dir);

    // Step 1: create VM rootfs.
    println!("Creating Debian rootfs at {}", rootfs_path.display());
    qoc::create(&qoc::Debian, rootfs_path.clone()).expect("qoc::create failed");

    // Step 2: boot the VM.
    println!("Starting VM...");
    let handle =
        qoc::start(&qoc::Debian, rootfs_path, 1, false).expect("qoc::start failed");

    // Step 3: wait for the VM to become reachable and collect kernel info.
    println!("Waiting for VmInfo...");
    let info = handle.wait_for_info().expect("wait_for_info failed");

    let kernel_version = info
        .kernel_version
        .expect("VmInfo::kernel_version is None");
    let upstream_version = info
        .upstream_version
        .expect("VmInfo::upstream_version is None");
    let config_len = info.kernel_config.as_deref().unwrap_or("").len();

    println!("kernel_version:   {kernel_version}");
    println!("upstream_version: {upstream_version}");
    println!("kernel_config:    {config_len} bytes");

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

        println!(
            "snapshot {}: {} modules",
            i + 1,
            snapshot.modules.len()
        );
        module_list.snapshots.push(snapshot);

        if i < 9 {
            thread::sleep(Duration::from_secs(1));
        }
    }

    // Step 5: find the canonical upstream git tag.
    let git_tag = upstream_version_to_git_tag(&kernel_src, &upstream_version)
        .expect("failed to find upstream git tag");
    println!("upstream git tag: {git_tag}");

    // Step 6: terminate the VM gracefully.
    println!("Stopping VM...");
    handle.stop().expect("VM stop failed");

    assert_eq!(module_list.snapshots.len(), 10);
    assert!(!git_tag.is_empty());
}
