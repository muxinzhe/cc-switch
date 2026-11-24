mod commands;
mod lifecycle;
pub(crate) mod runtime;
mod sessions;
mod usage;

use std::{
    fs,
    io::{self, Read, Write},
    os::unix::{
        fs::{FileTypeExt, MetadataExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use tokio::sync::RwLock;

use crate::{AppState, Database};
use runtime::{async_runtime, AppHandle};

const MAX_BYTES: u64 = 16 * 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(300);
use crate::SSH_AGENT_PROTOCOL as PROTOCOL;

#[derive(Deserialize)]
struct Request {
    protocol: u16,
    #[serde(default)]
    build: String,
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

    let app = initialize(directory.to_owned())?;
    let commands = Arc::new(Mutex::new(()));
    let retiring = Arc::new(AtomicBool::new(false));
    let listener = UnixListener::bind(&socket)?;
    for stream in listener.incoming() {
        let mut stream = stream?;
        if retiring.load(Ordering::Acquire) {
            break;
        }
        let retiring = retiring.clone();
        let socket = socket.clone();
        let app = app.clone();
        let commands = commands.clone();
        std::thread::spawn(move || {
            match handle(&app, &commands, &mut stream) {
                Ok(true) => {
                    retiring.store(true, Ordering::Release);
                    // Wake the accept loop so it drops the listener and its lock.
                    let _ = UnixStream::connect(socket);
                }
                Ok(false) => {}
                Err(error) => log::debug!("SSH agent connection ended: {error}"),
            }
        });
    }
    Ok(())
}

fn initialize(data: PathBuf) -> Result<AppHandle> {
    use crate::commands::*;
    crate::app_store::update_cached_override(Some(data.clone()));
    let had_database = data.join("cc-switch.db").is_file();
    let db = Arc::new(Database::init()?);
    crate::settings::initialize_agent_settings(db.clone())?;
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
            if let Err(error) = crate::services::provider::import_default_config_with_codex_seed(
                &state,
                application,
                !had_database,
            ) {
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

// A one-byte status precedes the JSON response. Large transcripts and usage
// snapshots flow directly from the serializer to SSH without an RPC size cap.
fn handle(app: &AppHandle, commands: &Mutex<()>, socket: &mut UnixStream) -> Result<bool> {
    socket.set_read_timeout(Some(IO_TIMEOUT))?;
    socket.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut output = io::BufWriter::new(socket.try_clone()?);
    let mut started = false;
    let mut retire = false;
    fn send<T: serde::Serialize>(out: &mut impl Write, started: &mut bool, data: &T) -> Result<()> {
        *started = true;
        out.write_all(&[0])?;
        serde_json::to_writer(out, data)?;
        Ok(())
    }
    let response = (|| -> Result<()> {
        let request: Request = serde_json::from_slice(&read_bounded(socket)?)?;
        ensure!(
            request.protocol == PROTOCOL,
            "Incompatible SSH agent protocol"
        );
        if request.command == "retire" {
            lifecycle::check_upgrade_lock(&crate::config::get_app_config_dir())?;
            retire = true;
            return send(&mut output, &mut started, &true);
        }
        if request.command != "hello" || !request.build.is_empty() {
            ensure!(
                std::env::var("CC_SWITCH_AGENT_BUILD").ok().as_deref()
                    == Some(request.build.as_str()),
                "SSH agent build changed. Reconnect to this host."
            );
        }
        match request.command.as_str() {
            "sessions" => send(&mut output, &mut started, &sessions::list()),
            "messages" => {
                let app = request
                    .args
                    .get("app")
                    .and_then(Value::as_str)
                    .context("Missing session app")?;
                let source = request
                    .args
                    .get("sourcePath")
                    .and_then(Value::as_str)
                    .context("Missing session source")?;
                send(&mut output, &mut started, &sessions::messages(app, source)?)
            }
            "usage" => {
                use runtime::Manager;
                let scan = request
                    .args
                    .get("scan")
                    .and_then(Value::as_bool)
                    .context("Missing usage scan flag")?;
                let rows = usage::snapshot(&app.state::<AppState>().db, scan)?;
                send(&mut output, &mut started, &rows)
            }
            "hello" => {
                let data = json!({"protocol": PROTOCOL, "version": env!("CARGO_PKG_VERSION"), "build": std::env::var("CC_SWITCH_AGENT_BUILD").ok()});
                send(
                    &mut output,
                    &mut started,
                    &json!({"data": data, "events": []}),
                )
            }
            _ => {
                // Each configuration request owns the events emitted by its handlers.
                let _guard = commands
                    .lock()
                    .map_err(|_| anyhow::anyhow!("SSH agent command lock failed"))?;
                let reply = async_runtime::block_on(commands::dispatch(app.clone(), request));
                let events = app.take_events();
                let value = match reply {
                    Ok(data) => json!({"data": data, "events": events}),
                    Err(error) => json!({"error": error, "events": events}),
                };
                send(&mut output, &mut started, &value)
            }
        }
    })();
    if let Err(error) = response {
        if started {
            return Err(error);
        }
        output.write_all(&[1])?;
        write!(output, "{error:#}")?;
    }
    output.flush()?;
    Ok(retire)
}

fn exchange(
    socket: &mut UnixStream,
    input: &[u8],
    timeout: Duration,
    output: &mut impl Write,
) -> Result<()> {
    socket.set_read_timeout(Some(timeout))?;
    socket.set_write_timeout(Some(timeout))?;
    socket.write_all(input)?;
    socket.shutdown(std::net::Shutdown::Write)?;
    let mut status = [0];
    socket.read_exact(&mut status)?;
    if status[0] != 0 {
        anyhow::bail!("{}", String::from_utf8_lossy(&read_bounded(socket)?));
    }
    io::copy(socket, output)?;
    Ok(())
}

fn call(directory: &Path, build: Option<&str>) -> Result<()> {
    let input = read_bounded(io::stdin().lock())?;
    // An upgrade takes the exclusive lock. Let current commands finish writing
    // native configuration and committing their SQLite transactions first.
    use std::os::fd::AsRawFd;
    loop {
        let lock = lifecycle::open_lock(directory, "upgrade.lock")?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_SH) } != 0 {
            return Err(io::Error::last_os_error()).context("Locking the SSH agent");
        }
        if let Some(mut socket) = lifecycle::connect(directory)? {
            return exchange(&mut socket, &input, IO_TIMEOUT, &mut io::stdout().lock());
        }
        drop(lock);
        lifecycle::ensure_running(directory, build.context("Missing SSH agent build")?)?;
    }
}
