mod commands;
mod lifecycle;
pub(crate) mod runtime;

use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::unix::{
        fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use tokio::sync::RwLock;

use crate::{AppState, Database};
use runtime::{async_runtime, AppHandle};

const MAX_BYTES: u64 = 16 * 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(120);
const PROTOCOL: u16 = 1;

#[derive(Deserialize)]
struct Request {
    protocol: u16,
    command: String,
    #[serde(default)]
    args: Map<String, Value>,
}

pub fn run() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let action = args.next().context("Expected an SSH agent action")?;
    if action == "--version" {
        println!(
            "cc-switch-ssh-agent {} (protocol {PROTOCOL})",
            env!("CARGO_PKG_VERSION")
        );
        return Ok(());
    }
    let directory = PathBuf::from(
        args.next()
            .context("Expected the private SSH agent directory")?,
    );
    let metadata = fs::symlink_metadata(&directory)?;
    ensure!(
        directory.is_absolute()
            && metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o777 == 0o700,
        "The SSH agent directory must be owned by the SSH user with mode 0700"
    );

    if action == "--sync-usage" {
        let previous = PathBuf::from(args.next().context("Expected the previous usage cache")?);
        ensure!(args.next().is_none(), "Unexpected SSH agent arguments");
        return sync_usage(&directory, &previous);
    }
    if action == "--ensure" || action == "--call" {
        let build = args.next();
        ensure!(args.next().is_none(), "Unexpected SSH agent arguments");
        let build = build
            .as_deref()
            .map(|value| value.to_str().context("Invalid SSH agent build identifier"))
            .transpose()?;
        if action == "--ensure" {
            return lifecycle::ensure_running(
                &directory,
                build.context("Expected an SSH agent build identifier")?,
            );
        }
        return call(&directory, build);
    }
    ensure!(args.next().is_none(), "Unexpected SSH agent arguments");
    if action == "--serve" {
        serve(&directory)
    } else {
        anyhow::bail!("Expected --serve, --ensure, or --call")
    }
}

fn sync_usage(directory: &Path, previous: &Path) -> Result<()> {
    use rusqlite::{backup::Backup, Connection, OpenFlags};
    use std::os::fd::AsRawFd;

    unsafe { libc::umask(0o077) };
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(directory.join("scan.lock"))?;
    ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0,
        "Could not lock the usage cache"
    );

    let database = directory.join("cc-switch.db");
    if !database.exists() && previous.is_file() {
        let staging = directory.join("usage-import.db");
        {
            let source = Connection::open_with_flags(previous, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            let mut target = Connection::open(&staging)?;
            Backup::new(&source, &mut target)?.run_to_completion(
                128,
                Duration::from_millis(10),
                None,
            )?;
        }
        fs::rename(staging, database)?;
    }
    crate::app_store::update_cached_override(Some(directory.to_owned()));
    let database = Database::init()?;
    let result = crate::services::session_usage::sync_all_unlocked(&database);
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}

fn serve(directory: &Path) -> Result<()> {
    unsafe { libc::umask(0o077) };
    use std::os::fd::AsRawFd;

    let lock = lifecycle::open_lock(directory, "rpc.lock")?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        anyhow::bail!("The SSH agent already has an active listener");
    }
    let socket = directory.join("rpc.sock");
    if let Ok(metadata) = fs::symlink_metadata(&socket) {
        ensure!(
            metadata.file_type().is_socket() && metadata.uid() == unsafe { libc::geteuid() },
            "Invalid SSH agent socket"
        );
        fs::remove_file(&socket)?;
    }

    let app = initialize(directory.join("data"))?;
    let listener = UnixListener::bind(socket)?;
    for stream in listener.incoming() {
        let mut stream = stream?;
        let result = handle(&app, &mut stream);
        if let Err(error) = result {
            log::debug!("SSH config RPC connection ended: {error}");
        }
    }
    Ok(())
}

fn initialize(data: PathBuf) -> Result<AppHandle> {
    use crate::commands::*;
    crate::app_store::update_cached_override(Some(data.clone()));
    let had_database = data.join("cc-switch.db").is_file();
    let db = Arc::new(Database::init()?);
    let first_start = db.init_default_official_providers()? > 0;
    let proxy = db.get_global_proxy_url()?;
    crate::proxy::http_client::init(proxy.as_deref()).map_err(anyhow::Error::msg)?;

    let app = AppHandle::default();
    let state = AppState::new(db);
    state.proxy_service.set_app_handle(app.clone());
    app.manage(CodexOAuthState(state.codex_oauth_manager.clone()));
    if first_start {
        for application in [
            crate::AppType::Claude,
            crate::AppType::Codex,
            crate::AppType::Gemini,
        ] {
            // Use the same startup importer as desktop. This reads live files
            // and registers their settings in the private agent database.
            if let Err(error) = crate::ProviderService::import_default_config(&state, application) {
                log::debug!(
                    "No importable native provider: {}",
                    crate::error_for_log(&error.to_string())
                );
            }
        }
    }
    app.manage(state.clone());
    app.manage(CopilotAuthState(Arc::new(RwLock::new(
        crate::proxy::providers::copilot_auth::CopilotAuthManager::new(data.clone()),
    ))));
    app.manage(XaiOAuthState(Arc::new(RwLock::new(
        crate::proxy::providers::xai_oauth_auth::XaiOAuthManager::new(data.clone()),
    ))));
    app.manage(AgyAccountState(Arc::new(
        crate::services::agy_account::AgyAccountManager::new(data.clone()),
    )));
    app.manage(WorkBuddyAccountState(Arc::new(
        crate::services::workbuddy_account::WorkBuddyAccountManager::new(data.clone()),
    )));
    app.manage(ClaudeAccountState(Arc::new(
        crate::services::claude_account::ClaudeAccountManager::new(data),
    )));
    app.manage(crate::services::kimi_oauth::KimiOAuthManager::default());
    if had_database {
        async_runtime::block_on(crate::mode::controller::startup(&state));
    }
    Ok(app)
}

fn read_bounded(reader: impl Read) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    reader.take(MAX_BYTES + 1).read_to_end(&mut data)?;
    ensure!(
        data.len() as u64 <= MAX_BYTES,
        "The SSH RPC message exceeds 16 MiB"
    );
    Ok(data)
}

fn handle(app: &AppHandle, socket: &mut UnixStream) -> Result<()> {
    socket.set_read_timeout(Some(IO_TIMEOUT))?;
    socket.set_write_timeout(Some(IO_TIMEOUT))?;
    let input = read_bounded(&mut *socket)?;
    let reply = match serde_json::from_slice::<Request>(&input) {
        Ok(value) if value.protocol == PROTOCOL => {
            async_runtime::block_on(commands::dispatch(app.clone(), value))
        }
        Ok(_) => Err(json!("Incompatible SSH configuration protocol")),
        Err(_) => Err(json!("Invalid SSH configuration request")),
    };
    let events = app.take_events();
    let output = match reply {
        Ok(data) => json!({"data": data, "events": events}),
        Err(error) => json!({"error": error, "events": events}),
    };
    serde_json::to_writer(&mut *socket, &output)?;
    socket.flush()?;
    Ok(())
}

fn exchange(socket: &mut UnixStream, input: &[u8], timeout: Duration) -> Result<Vec<u8>> {
    socket.set_read_timeout(Some(timeout))?;
    socket.set_write_timeout(Some(timeout))?;
    socket.write_all(input)?;
    socket.shutdown(std::net::Shutdown::Write)?;
    read_bounded(socket)
}

fn call(directory: &Path, build: Option<&str>) -> Result<()> {
    let input = read_bounded(io::stdin().lock())?;
    let mut socket = match lifecycle::connect(directory)? {
        Some(socket) => socket,
        None => {
            lifecycle::ensure_running(
                directory,
                build.context("SSH configuration backend is not running")?,
            )?;
            lifecycle::connect(directory)?.context("SSH configuration backend did not start")?
        }
    };
    io::stdout()
        .lock()
        .write_all(&exchange(&mut socket, &input, IO_TIMEOUT)?)?;
    Ok(())
}
