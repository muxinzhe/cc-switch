use std::{
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io,
    mem::{size_of, MaybeUninit},
    os::{
        fd::AsRawFd,
        unix::{
            ffi::OsStrExt,
            fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
            net::UnixStream,
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
    let reply: Value = serde_json::from_slice(&exchange(
        socket,
        &serde_json::to_vec(&request)?,
        HANDSHAKE_TIMEOUT,
    )?)?;
    Ok(reply["data"]["protocol"] == PROTOCOL
        && reply["data"]["version"] == env!("CARGO_PKG_VERSION")
        && reply["data"]["build"] == build)
}

fn retire_listener(socket: &UnixStream, directory: &Path) -> Result<()> {
    let mut peer = MaybeUninit::<libc::ucred>::uninit();
    let mut length = size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            peer.as_mut_ptr().cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error()).context("Identifying the old SSH backend");
    }
    ensure!(
        length as usize == size_of::<libc::ucred>(),
        "Invalid SSH backend peer credentials"
    );
    let peer = unsafe { peer.assume_init() };
    ensure!(
        peer.pid > 1 && peer.uid == unsafe { libc::geteuid() },
        "Unexpected SSH backend owner"
    );

    // Older agents have no shutdown RPC. Check the kernel-reported peer is
    // the managed listener for this exact service before signaling it.
    let command = match fs::read(format!("/proc/{}/cmdline", peer.pid)) {
        Ok(command) => command,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if command.is_empty() {
        return Ok(());
    }
    let args: Vec<_> = command
        .strip_suffix(&[0])
        .context("Invalid SSH backend command line")?
        .split(|&byte| byte == 0)
        .collect();
    let executable = args.first().map(|arg| Path::new(OsStr::from_bytes(arg)));
    let managed = executable.is_some_and(|exe| {
        exe.parent() == directory.parent()
            && exe.file_name().and_then(OsStr::to_str).is_some_and(|name| {
                name.strip_prefix("provider-agent-").is_some_and(|digest| {
                    digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
            })
    });
    ensure!(
        matches!(args.as_slice(), [_, b"--serve", service] if *service == directory.as_os_str().as_bytes())
            && managed,
        "The SSH socket is owned by an unexpected process"
    );
    if unsafe { libc::kill(peer.pid, libc::SIGTERM) } != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error).context("Stopping the old SSH backend");
        }
    }
    Ok(())
}

pub(super) fn ensure_running(directory: &Path, build: &str) -> Result<()> {
    // Keep concurrent startup requests from attempting the same upgrade.
    let upgrade = open_lock(directory, "upgrade.lock")?;
    if unsafe { libc::flock(upgrade.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error()).context("Locking the SSH backend upgrade");
    }
    if let Some(mut socket) = connect(directory)? {
        if is_current(&mut socket, build)? {
            return Ok(());
        }
        retire_listener(&socket, directory)?;
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
    let mut child = Command::new("nohup")
        .arg(std::env::current_exe()?)
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
