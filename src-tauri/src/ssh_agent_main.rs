#[cfg(not(feature = "desktop"))]
fn main() -> anyhow::Result<()> {
    cc_switch_lib::run_ssh_agent()
}

#[cfg(feature = "desktop")]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("Build the SSH agent with --no-default-features --features ssh-agent")
}
