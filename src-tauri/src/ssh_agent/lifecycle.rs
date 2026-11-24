use std::{
    fs::{self, File, OpenOptions},
    io,
    os::{
        fd::AsRawFd,
        unix::{
            fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
            net::UnixStream,
            process::CommandExt,
        },
    },
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{ensure, Context, Result};
use serde_json::Value;

use super::{exchange, PROTOCOL};

const POLL_INTERVAL: Duration = Duration::from_millis(100);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) fn open_lock(directory: &Path, name: &str) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(directory.join(name))
}

pub(super) fn connect(directory: &Path) -> Result<Option<UnixStream>> {
    let path = directory.join("rpc.sock");
    match fs::symlink_metadata(&path) {
        Ok(metadata) => ensure!(
            metadata.file_type().is_socket() && metadata.uid() == unsafe { libc::geteuid() },
            "Invalid SSH agent socket"
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    match UnixStream::connect(path) {
        Ok(socket) => Ok(Some(socket)),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}

fn is_current(socket: &mut UnixStream, build: &str) -> Result<bool> {
    let request = serde_json::json!({"protocol": PROTOCOL, "command": "hello"});
    let mut reply = Vec::new();
    exchange(
        socket,
        &serde_json::to_vec(&request)?,
        HANDSHAKE_TIMEOUT,
        &mut reply,
    )?;
    let reply: Value = serde_json::from_slice(&reply)?;
    Ok(reply["data"]["protocol"] == PROTOCOL
        && reply["data"]["version"] == env!("CARGO_PKG_VERSION")
        && reply["data"]["build"] == build)
}

pub(super) fn check_upgrade_lock(directory: &Path) -> Result<()> {
    let probe = open_lock(directory, "upgrade.lock")?;
    // Only --ensure owns this lock exclusively. Ordinary calls hold shared
    // locks, so requesting retirement through the regular RPC cannot stop it.
    if unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0 {
        anyhow::bail!("Retirement requires an agent upgrade");
    }
    let error = io::Error::last_os_error();
    ensure!(
        error.kind() == io::ErrorKind::WouldBlock,
        "Checking upgrade lock: {error}"
    );
    Ok(())
}

fn verify_build(executable: &Path, build: &str) -> Result<()> {
    use sha2::{Digest, Sha256};
    let mut sha = Sha256::new();
    io::copy(&mut File::open(executable)?, &mut sha)?;
    ensure!(
        format!("{:x}", sha.finalize()) == build,
        "SSH agent build changed during installation"
    );
    Ok(())
}

pub(super) fn ensure_running(directory: &Path, build: &str) -> Result<()> {
    // Keep concurrent startup requests from attempting the same upgrade.
    let upgrade = open_lock(directory, "upgrade.lock")?;
    if unsafe { libc::flock(upgrade.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error()).context("Locking the SSH backend upgrade");
    }
    #[cfg(target_os = "linux")]
    let executable = Path::new("/proc/self/exe");
    // Darwin has no procfs executable handle. Pin the installed inode so an
    // atomic replacement during startup cannot change what we verify and run.
    #[cfg(target_os = "macos")]
    let staging = tempfile::Builder::new()
        .prefix(".launch-")
        .tempdir_in(directory)?;
    #[cfg(target_os = "macos")]
    let executable = {
        let path = staging.path().join("agent");
        fs::hard_link(directory.join("agent"), &path)?;
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o022 == 0,
            "Expected an SSH agent executable owned by the SSH user"
        );
        path
    };
    verify_build(executable.as_ref(), build)?;
    if let Some(mut socket) = connect(directory)? {
        if is_current(&mut socket, build)? {
            return Ok(());
        }
        drop(socket);
        let mut socket = connect(directory)?.context("SSH agent stopped during upgrade")?;
        let request = serde_json::json!({"protocol": PROTOCOL, "command": "retire"});
        let mut response = Vec::new();
        exchange(
            &mut socket,
            &serde_json::to_vec(&request)?,
            HANDSHAKE_TIMEOUT,
            &mut response,
        )?;
        ensure!(response == b"true", "SSH agent declined retirement");
    }

    let listener = open_lock(directory, "rpc.lock")?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if unsafe { libc::flock(listener.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            break;
        }
        let error = io::Error::last_os_error();
        ensure!(
            error.kind() == io::ErrorKind::WouldBlock,
            "Waiting for the SSH backend: {error}"
        );
        ensure!(
            Instant::now() < deadline,
            "The old SSH backend did not release its listener"
        );
        thread::sleep(POLL_INTERVAL);
    }
    drop(listener);

    let log = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(directory.join("startup.log"))?;
    // Start the verified inode in a new session so disconnecting SSH leaves the
    // service running. Keep argv[0] stable for identifying the managed listener.
    let mut command = Command::new(executable);
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .arg0(directory.join("agent"))
        .arg("--serve")
        .arg(directory)
        .env("CC_SWITCH_AGENT_BUILD", build)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()
        .context("Starting the SSH configuration backend")?;
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        if let Some(mut socket) = connect(directory)? {
            ensure!(
                is_current(&mut socket, build)?,
                "The SSH backend did not switch to the requested build"
            );
            return Ok(());
        }
        ensure!(
            child.try_wait()?.is_none(),
            "The SSH configuration backend exited during startup"
        );
        ensure!(
            Instant::now() < deadline,
            "The SSH configuration backend did not start"
        );
        thread::sleep(POLL_INTERVAL);
    }
}
