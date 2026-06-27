pub fn run(handle: &qoc::VmHandle) -> anyhow::Result<()> {
    let out = handle
        .ssh(&["sh", "-c", "echo 'Running LTP (stub)...' && sleep 5"])
        .map_err(|e| anyhow::anyhow!("LTP stub failed: {e}"))?;
    print!("{}", out.stdout);
    anyhow::ensure!(out.success, "LTP stub exited with error");
    Ok(())
}
