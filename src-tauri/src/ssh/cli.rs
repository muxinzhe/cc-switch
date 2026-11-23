use std::{future::Future, io::Read, process::Stdio, sync::Arc, time::Duration};

use super::{transcript, Endpoint};
use crate::session_manager::model::SessionMessage;
use anyhow::{bail, ensure, Context, Result};
use flate2::read::GzDecoder;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdout, Command},
};
use tokio_util::io::SyncIoBridge;

// Pin the GitHub release and both digests. Only this reviewed, non-interactive
// CLI is run from the private installation, never a binary found on remote PATH.
const RELEASE: &str = "5.11.4";
const BASE_URL: &str = "https://github.com/SaladDay/cc-switch-cli/releases/download";
const MAX_BINARY: usize = 64 * 1024 * 1024;
const MAX_RESPONSE: usize = 32 * 1024 * 1024;
const RESPONSE_MARKER: &[u8] = b"\ncc-switch-ssh-response\n";
const STDERR_TAIL: usize = 64 * 1024;

struct Asset {
    arch: &'static str,
    name: &'static str,
    archive_sha256: &'static str,
    binary_sha256: &'static str,
}

const X64: Asset = Asset {
    arch: "x86_64",
    name: "cc-switch-cli-linux-x64-musl.tar.gz",
    archive_sha256: "027a8c68b1ee85123ede966be74e1b08ebfb2b99e2549e35c1bc266ba9fa3191",
    binary_sha256: "7b3eae28cf6b029034121a05b61033b62a4603f918bbf431a1cdf493278f86c9",
};
const ARM64: Asset = Asset {
    arch: "aarch64",
    name: "cc-switch-cli-linux-arm64-musl.tar.gz",
    archive_sha256: "ea1ee67c4c9a72383a95f9ce1b97d13b9ea7d5f1e896e16aba2c1999b7c77fec",
    binary_sha256: "4e8120cc24aaafb649511c8f133e1cbdf6e4eb86bb3745ea977605d6cba21430",
};

const PRIVATE_PATHS: &str = r#"
case "${HOME-}" in
    /*) ;;
    *) printf '%s\n' 'The SSH user needs an absolute home directory.' >&2; exit 1 ;;
esac
umask 077
root="$HOME/.cc-switch-ssh"
private_dir() {
    if [ ! -d "$1" ]; then mkdir -m 700 -- "$1" || [ -d "$1" ]; fi
    if [ -L "$1" ] || [ "$(stat -c '%u:%a' -- "$1")" != "$(id -u):700" ]; then
        printf '%s\n' "The CC Switch SSH directory must be owned by the SSH user with mode 0700: $1" >&2
        exit 1
    fi
}
verified_binary() {
    [ -f "$1" ] && [ ! -L "$1" ] && [ -x "$1" ] &&
        [ "$(sha256sum < "$1")" = "$expected_digest  -" ]
}
"#;

#[derive(Clone)]
pub(crate) struct RemoteCli {
    host: Endpoint,
    asset: &'static Asset,
    agent_binary: Arc<tokio::sync::OnceCell<String>>,
    agent: Arc<tokio::sync::OnceCell<String>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HostInfo {
    cli_version: &'static str,
    arch: &'static str,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Session {
    provider_id: String,
    session_id: String,
    title: Option<String>,
    project_dir: Option<String>,
    created_at: Option<i64>,
    last_active_at: Option<i64>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Provider {
    id: String,
    name: String,
    active: bool,
    #[serde(default)]
    live: bool,
}

impl RemoteCli {
    pub(crate) async fn connect(host: Endpoint) -> Result<Self> {
        let platform = run_ssh(
            &host,
            "printf '\\ncc-switch-ssh-response\\n'; uname -s; uname -m",
            None,
            Duration::from_secs(25),
        )
        .await?;
        let asset = match platform.trim() {
            "Linux\nx86_64" => &X64,
            "Linux\naarch64" => &ARM64,
            value => bail!(
                "Unsupported SSH host platform: {}",
                value.replace('\n', " ")
            ),
        };
        let remote = Self {
            host,
            asset,
            agent_binary: Arc::new(tokio::sync::OnceCell::new()),
            agent: Arc::new(tokio::sync::OnceCell::new()),
        };
        remote.ensure_installed().await?;
        Ok(remote)
    }

    pub(super) fn endpoint(&self) -> &Endpoint {
        &self.host
    }

    pub(super) fn arch(&self) -> &str {
        self.asset.arch
    }

    pub(super) fn agent(&self) -> &tokio::sync::OnceCell<String> {
        &self.agent
    }

    pub(super) fn agent_binary(&self) -> &tokio::sync::OnceCell<String> {
        &self.agent_binary
    }

    pub(crate) fn info(&self) -> HostInfo {
        HostInfo {
            cli_version: RELEASE,
            arch: self.asset.arch,
        }
    }

    async fn ensure_installed(&self) -> Result<()> {
        let probe = self.script(
            r#"
printf '\ncc-switch-ssh-response\n'
if [ -e "$root" ] || [ -L "$root" ]; then
    private_dir "$root"
    if verified_binary "$binary"; then
        exec "$binary" --version
    fi
fi
printf 'missing\n'
"#,
        );
        let version = run_ssh(&self.host, &probe, None, Duration::from_secs(25)).await?;
        if version.trim() == format!("cc-switch {RELEASE}") {
            return Ok(());
        }
        ensure!(
            version.trim() == "missing",
            "Unexpected CC Switch CLI version: {}",
            version.trim()
        );

        let payload = download(self.asset).await?;
        let install = self.script(
            r#"
private_dir "$root"
temp=$(mktemp "$root/.upload.XXXXXXXX")
trap 'rm -f -- "$temp"' EXIT
trap 'exit 1' HUP INT TERM
cat > "$temp"
chmod 700 "$temp"
if ! verified_binary "$temp"; then
    printf '%s\n' 'CC Switch CLI checksum did not match after upload.' >&2
    exit 1
fi
# Atomically publish without following or overwriting an existing file.
if ! ln -T -- "$temp" "$binary" 2>/dev/null && ! verified_binary "$binary"; then
    printf '%s\n' "An unexpected file already occupies the managed CLI path: $binary" >&2
    exit 1
fi
printf '\ncc-switch-ssh-response\n'
"$binary" --version
"#,
        );
        let version = run_ssh(
            &self.host,
            &install,
            Some(&payload),
            Duration::from_secs(150),
        )
        .await?;
        ensure!(
            version.trim() == format!("cc-switch {RELEASE}"),
            "Installed CLI version does not match"
        );
        Ok(())
    }

    pub(crate) async fn sessions(&self) -> Result<Vec<Session>> {
        // The upstream Sessions commands bypass startup migrations of live
        // configuration. Their caches are explicitly limited to our directory.
        self.read_session_data("exec \"$binary\" sessions list --all --json", |reader| {
            serde_json::from_reader(reader).context("Invalid session list from CC Switch CLI")
        })
        .await
    }

    pub(super) async fn usage_rows(&self, scan: bool) -> Result<super::usage::Rows> {
        let agent = if scan && self.has_agent_asset() {
            Some(self.ensure_agent_binary().await?)
        } else {
            None
        };
        let sync = if let Some(digest) = agent {
            format!(
                r#"private_dir "$root/usage-v1"
"$root/provider-agent-{digest}" --sync-usage "$root/usage-v1" "$usage_db" >/dev/null"#
            )
        } else if scan {
            "\"$binary\" sessions sync-usage --all --json >/dev/null".to_owned()
        } else {
            String::new()
        };
        let body = format!(
            r#"if ! command -v python3 >/dev/null 2>&1; then
    printf '%s\n' 'Usage statistics require Python 3 on this host.' >&2
    exit 1
fi
usage_db="$CC_SWITCH_CONFIG_DIR/cc-switch.db"
{sync}
if [ -f "$root/usage-v1/cc-switch.db" ] && [ '{use_agent}' = 'true' ]; then
    usage_db="$root/usage-v1/cc-switch.db"
fi
if [ ! -f "$usage_db" ]; then
    printf '%s\n' '{{"logs":[],"rollups":[]}}'
    exit 0
fi
exec python3 - "$usage_db" "$root/provider-v{}/data/cc-switch.db" '{}' '{}' "$CC_SWITCH_CONFIG_DIR/codex-usage-attribution.db" <<'CC_SWITCH_USAGE'
{}
CC_SWITCH_USAGE"#,
            super::agent::PROTOCOL,
            super::usage::LOG_COLUMNS,
            super::usage::ROLLUP_COLUMNS,
            include_str!("usage_snapshot.py"),
            use_agent = self.has_agent_asset(),
        );
        let data = self.read_only(&body).await?;
        Deserialize::deserialize(data).context("Invalid usage records from SSH host")
    }

    pub(crate) async fn stream_messages(
        &self,
        app: &str,
        session_id: &str,
        send: impl FnMut(usize, &[SessionMessage]) -> Result<()> + Send + 'static,
    ) -> Result<u64> {
        ensure!(supported_app(app), "Unsupported remote session application");
        let app = shlex::try_quote(app)?;
        let id = shlex::try_quote(session_id)?;
        self.read_session_data(
            &format!("exec \"$binary\" sessions messages --provider {app} --json -- {id}"),
            move |reader| transcript::read(reader, send),
        )
        .await
    }

    pub(crate) async fn providers(&self, app: &str) -> Result<Vec<Provider>> {
        if !supported_app(app) {
            return Ok(Vec::new());
        }
        let body = format!(
            "python3 - {} <<'CC_SWITCH_PROVIDERS'\n{}\nCC_SWITCH_PROVIDERS",
            shlex::try_quote(app)?,
            include_str!("providers.py"),
        );
        let response = self.read_only(&body).await?;
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Snapshot {
            providers: Vec<Provider>,
            codex_config: Option<String>,
        }
        let snapshot = Snapshot::deserialize(response).context("Invalid remote provider list")?;
        let mut providers = snapshot.providers;
        if let Some(content) = snapshot.codex_config {
            append_codex_profiles(&mut providers, &content)?;
        }
        Ok(providers)
    }

    pub(super) async fn config_file<T: DeserializeOwned>(
        &self,
        key: super::config_file::ConfigKey,
        draft: Option<&super::config_file::ConfigWrite>,
    ) -> Result<T> {
        let key = serde_json::to_value(key)?;
        let key = key.as_str().context("Invalid config key")?;
        let operation = if draft.is_some() { "save" } else { "read" };
        let payload = draft.map(serde_json::to_vec).transpose()?;
        let script = self.script(&format!(
            r#"if [ "{operation}" = save ]; then
    private_dir "$root"
    private_dir "$root/config-backups"
fi
printf '\ncc-switch-ssh-response\n'
exec python3 -c {} {operation} {} "$root"
"#,
            shlex::try_quote(include_str!("config_file.py"))?,
            shlex::try_quote(key)?,
        ));
        let output = run_ssh(
            &self.host,
            &script,
            payload.as_deref(),
            Duration::from_secs(45),
        )
        .await?;
        serde_json::from_str(&output).context("Invalid remote configuration response")
    }

    async fn read_only(&self, body: &str) -> Result<serde_json::Value> {
        let script = self.read_only_script(body);
        let response = run_ssh(&self.host, &script, None, Duration::from_secs(300)).await?;
        serde_json::from_str(&response).context("Invalid JSON from SSH host")
    }

    async fn read_session_data<T: Send + 'static>(
        &self,
        body: &str,
        read: impl FnOnce(SessionReader) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        run_ssh_with(
            &self.host,
            &self.read_only_script(body),
            None,
            Duration::from_secs(300),
            |stdout| parse_session_data(stdout, read),
        )
        .await
    }

    fn read_only_script(&self, body: &str) -> String {
        self.script(&format!(
            r#"private_dir "$root"
if ! verified_binary "$binary"; then
    printf '%s\n' 'The managed CLI is unavailable. Reconnect to the host.' >&2
    exit 1
fi
export CC_SWITCH_SSH_ORIGINAL_CONFIG_DIR="${{CC_SWITCH_CONFIG_DIR-}}"
export CC_SWITCH_CONFIG_DIR="$root/state-v{RELEASE}"
private_dir "$CC_SWITCH_CONFIG_DIR"
unset CC_SWITCH_TEST_HOME
printf '\ncc-switch-ssh-response\n'
{body}
"#
        ))
    }

    pub(super) fn script(&self, body: &str) -> String {
        format!(
            "set -eu\n{PRIVATE_PATHS}\nbinary=\"$root/cc-switch-v{RELEASE}-{}\"\nexpected_digest={}\n{body}",
            self.asset.arch, self.asset.binary_sha256
        )
    }
}

fn supported_app(app: &str) -> bool {
    matches!(
        app,
        "claude" | "codex" | "gemini" | "opencode" | "openclaw" | "hermes" | "pi"
    )
}

fn append_codex_profiles(providers: &mut Vec<Provider>, content: &str) -> Result<()> {
    let config: toml::Table = toml::from_str(content)
        .map_err(|_| anyhow::anyhow!("Remote Codex config is not valid TOML"))?;
    let current = config
        .get("model_provider")
        .and_then(toml::Value::as_str)
        .unwrap_or("openai");
    let native = config
        .get("model_providers")
        .and_then(toml::Value::as_table);
    if let Some(native) = native {
        for (id, profile) in native {
            if let Some(existing) = providers.iter_mut().find(|provider| provider.id == *id) {
                existing.active |= id == current;
                continue;
            }
            providers.push(Provider {
                id: id.clone(),
                name: profile
                    .get("name")
                    .and_then(toml::Value::as_str)
                    .unwrap_or(id)
                    .to_string(),
                active: id == current,
                live: false,
            });
        }
    }
    if !providers.iter().any(|provider| provider.active) {
        providers.push(Provider {
            id: current.to_string(),
            name: if current == "openai" {
                "OpenAI".into()
            } else {
                current.into()
            },
            active: true,
            live: false,
        });
    }
    if let Some(profiles) = config.get("profiles").and_then(toml::Value::as_table) {
        providers.extend(profiles.keys().map(|name| Provider {
            id: format!("profile:{name}"),
            name: name.clone(),
            active: config.get("profile").and_then(toml::Value::as_str) == Some(name),
            live: false,
        }));
    }
    Ok(())
}

async fn download(asset: &'static Asset) -> Result<Vec<u8>> {
    let url = format!("{BASE_URL}/v{RELEASE}/{}", asset.name);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(90))
        .build()?;
    let mut response = client
        .get(url)
        .send()
        .await
        .context("Could not download CC Switch CLI from GitHub")?
        .error_for_status()?;
    let mut archive = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            archive.len() + chunk.len() <= MAX_BINARY,
            "CLI archive exceeds size limit"
        );
        archive.extend_from_slice(&chunk);
    }
    tauri::async_runtime::spawn_blocking(move || extract_binary(&archive, asset))
        .await
        .context("CLI archive extraction task failed")?
}

fn extract_binary(bytes: &[u8], asset: &Asset) -> Result<Vec<u8>> {
    ensure!(
        format!("{:x}", Sha256::digest(bytes)) == asset.archive_sha256,
        "CC Switch CLI archive checksum did not match"
    );
    let mut archive = tar::Archive::new(GzDecoder::new(bytes));
    for entry in archive.entries()? {
        let entry = entry?;
        if entry.path()?.as_ref() != std::path::Path::new("cc-switch")
            || !entry.header().entry_type().is_file()
        {
            continue;
        }
        let mut payload = Vec::new();
        entry
            .take(MAX_BINARY as u64 + 1)
            .read_to_end(&mut payload)?;
        ensure!(payload.len() <= MAX_BINARY, "CLI binary exceeds size limit");
        ensure!(
            format!("{:x}", Sha256::digest(&payload)) == asset.binary_sha256,
            "CC Switch CLI binary checksum did not match"
        );
        return Ok(payload);
    }
    bail!("The CLI archive has no cc-switch binary")
}

pub(super) async fn run_ssh(
    host: &Endpoint,
    script: &str,
    upload: Option<&[u8]>,
    timeout: Duration,
) -> Result<String> {
    run_ssh_with(host, script, upload, timeout, |stdout| async {
        let mut response = Vec::new();
        stdout
            .take(MAX_RESPONSE as u64 + 1)
            .read_to_end(&mut response)
            .await?;
        ensure!(
            response.len() <= MAX_RESPONSE,
            "SSH response exceeds size limit"
        );
        String::from_utf8(response).context("SSH host returned invalid UTF-8")
    })
    .await
}

type SessionReader = std::io::BufReader<SyncIoBridge<BufReader<ChildStdout>>>;

async fn parse_session_data<T: Send + 'static>(
    stdout: BufReader<ChildStdout>,
    read: impl FnOnce(SessionReader) -> Result<T> + Send + 'static,
) -> Result<T> {
    let bridge = SyncIoBridge::new(stdout);
    tokio::task::spawn_blocking(move || read(std::io::BufReader::new(bridge))).await?
}

async fn run_ssh_with<T, F>(
    host: &Endpoint,
    script: &str,
    upload: Option<&[u8]>,
    timeout: Duration,
    read: impl FnOnce(BufReader<ChildStdout>) -> F,
) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    let remote = format!("exec /bin/sh -c {}", shlex::try_quote(script)?);
    let mut command = Command::new("ssh");
    command.args([
        "-T",
        "-a",
        "-x",
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        "ClearAllForwardings=yes",
        "-o",
        "RemoteCommand=none",
        "-o",
        "ControlMaster=no",
        "-o",
        "ControlPath=none",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
    ]);
    if let Some(port) = host.port {
        command.args(["-p", &port.to_string()]);
    }
    if let Some(identity) = &host.identity_file {
        command.args(["-i", identity, "-o", "IdentitiesOnly=yes"]);
    }
    command.args(["--", &host.destination, &remote]);
    run_command(command, upload, timeout, read).await
}

async fn run_command<T, F>(
    mut command: Command,
    upload: Option<&[u8]>,
    timeout: Duration,
    read: impl FnOnce(BufReader<ChildStdout>) -> F,
) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    command
        .stdin(if upload.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(target_os = "windows")]
    command.creation_flags(0x08000000);

    let mut child = command.spawn().context("Could not start OpenSSH")?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let response = async {
        let mut stdout = BufReader::new(stdout);
        // Keep the read error until the process exits so a remote command failure
        // can still report its stderr, including when it prints no response marker.
        let response = match skip_banner(&mut stdout).await {
            Ok(()) => read(stdout).await,
            Err(error) => Err(error),
        };
        Ok::<_, anyhow::Error>(response)
    };
    let send = async {
        if let (Some(mut input), Some(bytes)) = (stdin, upload) {
            // A rejected upload can close stdin early. Preserve the server's
            // specific error message on stderr in that case.
            if let Err(error) = input.write_all(bytes).await {
                if error.kind() != std::io::ErrorKind::BrokenPipe {
                    return Err(error.into());
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    };
    let (status, out, err, ()) = tokio::time::timeout(timeout, async {
        tokio::try_join!(
            async { Ok::<_, anyhow::Error>(child.wait().await?) },
            response,
            read_stderr_tail(stderr),
            send,
        )
    })
    .await
    .context("SSH operation timed out")??;

    if !status.success() {
        let error = String::from_utf8_lossy(&err);
        if error.trim().is_empty() {
            bail!("SSH exited with {status}");
        }
        bail!("{}", error.trim());
    }
    out
}

async fn skip_banner(input: &mut (impl AsyncBufRead + Unpin)) -> Result<()> {
    let mut matched = 0;
    loop {
        let bytes = input.fill_buf().await?;
        ensure!(
            !bytes.is_empty(),
            "SSH host did not return a CC Switch response"
        );
        let mut consumed = bytes.len();
        for (index, &byte) in bytes.iter().enumerate() {
            if byte == RESPONSE_MARKER[matched] {
                matched += 1;
                if matched == RESPONSE_MARKER.len() {
                    consumed = index + 1;
                    break;
                }
            } else {
                matched = usize::from(byte == RESPONSE_MARKER[0]);
            }
        }
        input.consume(consumed);
        if matched == RESPONSE_MARKER.len() {
            return Ok(());
        }
    }
}

async fn read_stderr_tail(mut input: impl AsyncRead + Unpin) -> Result<Vec<u8>> {
    let mut tail = Vec::with_capacity(STDERR_TAIL);
    let mut chunk = [0; 8192];
    loop {
        let len = input.read(&mut chunk).await?;
        if len == 0 {
            return Ok(tail);
        }
        let excess = (tail.len() + len).saturating_sub(STDERR_TAIL);
        tail.drain(..excess);
        tail.extend_from_slice(&chunk[..len]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_cli_archives_with_unexpected_digest() {
        let error = extract_binary(b"not a release", &X64).unwrap_err();
        assert!(error.to_string().contains("archive checksum"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn streams_large_transcript_with_a_banner_and_noisy_stderr() {
        use std::io::Write;

        let mut fixture = tempfile::NamedTempFile::new().unwrap();
        let large_message = "界".repeat(110_000);
        let count = MAX_RESPONSE / large_message.len() + 2;
        write!(fixture, r#"{{"provider":"codex","messages":["#).unwrap();
        for index in 0..count {
            if index > 0 {
                write!(fixture, ",").unwrap();
            }
            serde_json::to_writer(
                &mut fixture,
                &serde_json::json!({
                    "role": "assistant",
                    "content": &large_message,
                    "ts": index,
                }),
            )
            .unwrap();
        }
        write!(fixture, "]}}").unwrap();
        assert!(fixture.as_file().metadata().unwrap().len() > MAX_RESPONSE as u64);

        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                concat!(
                    r#"head -c 90000 "$1" >&2; printf 'login banner\ncc-switch-ssh-response\n';"#,
                    r#"exec cat "$1""#,
                ),
                "ssh-fixture",
            ])
            .arg(fixture.path());
        let actual = run_command(command, None, Duration::from_secs(30), |stdout| {
            parse_session_data(stdout, move |reader| {
                let mut read = 0;
                transcript::read(reader, |start, messages| {
                    assert_eq!(start, read);
                    assert_eq!(messages.len(), 1);
                    assert_eq!(messages[0].content, large_message);
                    assert_eq!(messages[0].ts, Some(read as i64));
                    read += messages.len();
                    Ok(())
                })?;
                Ok(read)
            })
        })
        .await
        .unwrap();
        assert_eq!(actual, count);
    }

    #[tokio::test]
    async fn skips_banner_and_preserves_response_across_buffer_boundaries() {
        let mut raw = vec![b'x'; 8193];
        raw.extend_from_slice(b"\ncc-switch-ssh-respo");
        raw.extend_from_slice(RESPONSE_MARKER);
        raw.extend_from_slice(b"response");
        let mut reader = BufReader::with_capacity(7, raw.as_slice());
        skip_banner(&mut reader).await.unwrap();
        let mut response = String::new();
        reader.read_to_string(&mut response).await.unwrap();
        assert_eq!(response, "response");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn noisy_failed_command_still_reports_the_remote_error() {
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "head -c 90000 /dev/zero >&2; printf '\\nTranscript missing\\n' >&2; exit 1",
        ]);
        let error = run_command(command, None, Duration::from_secs(10), |_| async { Ok(()) })
            .await
            .unwrap_err();
        assert!(error.to_string().ends_with("Transcript missing"));
        assert!(error.to_string().len() <= STDERR_TAIL);
    }

    #[tokio::test]
    #[ignore = "requires an SSH test host and installs the CLI in its private user directory"]
    async fn remote_cli_smoke() {
        let host = std::env::var("CC_SWITCH_SSH_TEST_HOST").expect("set CC_SWITCH_SSH_TEST_HOST");
        let host = Endpoint {
            destination: host,
            port: None,
            identity_file: None,
        }
        .validate()
        .unwrap();

        let cli = RemoteCli::connect(host.clone())
            .await
            .expect("install and connect");
        let sessions = cli.sessions().await.expect("read sessions");
        let usage_rows = cli.usage_rows(true).await.expect("read usage rows");
        let host_id = uuid::Uuid::new_v4().to_string();
        let local = crate::database::Database::memory().unwrap();
        let usage = super::super::usage::create_snapshot(
            &local,
            false,
            vec![(host_id.clone(), usage_rows)],
        )
        .expect("import remote rows for the native usage page");
        let summary = usage
            .get_usage_summary(None, None, None, None, None)
            .expect("read original usage summary");
        usage
            .get_daily_trends(None, None, None, None, None)
            .expect("read original usage trends");
        usage
            .get_provider_stats(None, None, None, None, None)
            .expect("read original provider stats");
        usage
            .get_model_stats(None, None, None, None, None)
            .expect("read original model stats");
        let logs = usage
            .get_request_logs(&Default::default(), 0, 20)
            .expect("read original usage detail table");
        if let Some(log) = logs.data.first() {
            assert!(log.request_id.starts_with(&format!("ssh:{host_id}:")));
            assert!(usage.get_request_detail(&log.request_id).unwrap().is_some());
        }
        println!(
            "Native usage queries returned {} remote requests.",
            summary.total_requests
        );
        let profiles = cli
            .providers("codex")
            .await
            .expect("read configured Codex profiles");
        let _: super::super::config_file::RemoteConfigFile = cli
            .config_file(super::super::config_file::ConfigKey::Codex, None)
            .await
            .expect("read the remote Codex configuration");
        if let Some(session) = sessions
            .iter()
            .find(|session| session.provider_id == "codex")
        {
            cli.stream_messages("codex", &session.session_id, |_, _| Ok(()))
                .await
                .expect("read a Codex transcript");
        }
        RemoteCli::connect(host)
            .await
            .expect("reuse the verified installation");
        println!(
            "Read {} Codex profiles without invoking configuration mutations.",
            profiles.len()
        );

        println!(
            "CC Switch CLI {}, {}. Sessions: {}.",
            cli.info().cli_version,
            cli.info().arch,
            sessions.len()
        );
    }
}
