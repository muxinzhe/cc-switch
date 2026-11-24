use std::{future::Future, process::Stdio, time::Duration};

use anyhow::{bail, ensure, Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdout, Command},
};
use tokio_util::io::SyncIoBridge;

use super::{transcript, Endpoint};
use crate::session_manager::{model::SessionMessage, SessionMeta};

include!(concat!(env!("OUT_DIR"), "/ssh_agent_assets.rs"));

const MAX_RESPONSE: usize = 32 * 1024 * 1024;
const RESPONSE_MARKER: &[u8] = b"\ncc-switch-ssh-response\n";
const STDERR_TAIL: usize = 64 * 1024;

#[derive(Clone)]
pub(crate) struct RemoteAgent {
    host: Endpoint,
    os: HostOs,
    arch: &'static str,
    digest: String,
}

#[derive(Clone, Copy)]
enum HostOs {
    Linux,
    Darwin,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HostInfo {
    agent_version: &'static str,
    arch: &'static str,
}

impl RemoteAgent {
    pub(crate) async fn connect(host: Endpoint) -> Result<Self> {
        let platform = run_ssh(
            &host,
            "printf '\\ncc-switch-ssh-response\\n'; uname -s; uname -m",
            None,
            Duration::from_secs(25),
        )
        .await?;
        let (os, arch, payload) = match platform.trim() {
            "Linux\nx86_64" => (HostOs::Linux, "x86_64", X86_64),
            "Linux\naarch64" => (HostOs::Linux, "aarch64", AARCH64),
            "Darwin\narm64" => (HostOs::Darwin, "arm64", DARWIN_ARM64),
            other => bail!(
                "Unsupported SSH host platform: {}",
                other.replace('\n', " ")
            ),
        };
        let payload = payload.with_context(|| {
            format!(
                "This desktop build is missing the {} SSH agent",
                platform.trim().replace('\n', " ")
            )
        })?;
        let agent = Self {
            host,
            os,
            arch,
            digest: format!("{:x}", Sha256::digest(payload)),
        };
        agent.install(payload).await?;
        Ok(agent)
    }

    pub(super) fn endpoint(&self) -> &Endpoint {
        &self.host
    }

    pub(crate) fn info(&self) -> HostInfo {
        HostInfo {
            agent_version: env!("CARGO_PKG_VERSION"),
            arch: self.arch,
        }
    }

    fn script(&self, operation: &str) -> String {
        format!(
            r#"set -eu
umask 077
case "$HOME" in /*) ;; *) exit 1 ;; esac
root="$HOME/.cc-switch-ssh"
binary="$root/agent"
expected='{}'
{}
"#,
            self.digest, operation
        )
    }

    fn install_script(&self, operation: &str) -> String {
        let utilities = match self.os {
            HostOs::Linux => {
                r#"owner_mode() { stat -c '%u:%a' -- "$1"; }
sha256() { sha256sum; }
replace_agent() { mv -Tf -- "$1" "$2"; }"#
            }
            HostOs::Darwin => {
                r#"owner_mode() { /usr/bin/stat -f '%u:%Lp' "$1"; }
sha256() { /usr/bin/shasum -a 256; }
replace_agent() { [ ! -d "$2" ] && /bin/mv -fh "$1" "$2"; }"#
            }
        };
        self.script(&format!("{utilities}\n{operation}"))
    }

    async fn install(&self, payload: &[u8]) -> Result<()> {
        let probe = self.install_script(r#"
mkdir -p -m 700 -- "$root"
[ ! -L "$root" ] && [ "$(owner_mode "$root")" = "$(id -u):700" ] || { echo 'Expected private SSH agent directory owned by the SSH user' >&2; exit 1; }
printf '\ncc-switch-ssh-response\n'
if [ -f "$binary" ] && [ ! -L "$binary" ] && [ -x "$binary" ] && [ "$(sha256 < "$binary")" = "$expected  -" ]; then
    printf 'ready\n'
else
    printf 'install\n'
fi"#);
        let state = run_ssh(&self.host, &probe, None, Duration::from_secs(30)).await?;
        match state.trim() {
            "ready" => {}
            "install" => {
                let upload = self.install_script(
                    r#"
temp=$(mktemp "$root/.agent.XXXXXXXX")
trap 'rm -f -- "$temp"' EXIT
trap 'exit 1' HUP INT TERM
cat > "$temp"
[ "$(sha256 < "$temp")" = "$expected  -" ] || { echo 'Agent checksum mismatch' >&2; exit 1; }
chmod 700 "$temp"
replace_agent "$temp" "$binary"
printf '\ncc-switch-ssh-response\n'
"#,
                );
                run_ssh(&self.host, &upload, Some(payload), Duration::from_secs(150)).await?;
            }
            other => bail!("Unexpected SSH agent installation response: {other}"),
        }
        let start = self.script(
            r#"printf '\ncc-switch-ssh-response\n'
exec "$binary" --ensure "$root" "$expected""#,
        );
        run_ssh(&self.host, &start, None, Duration::from_secs(60)).await?;
        Ok(())
    }

    async fn request<T: Send + 'static>(
        &self,
        command: &str,
        args: Value,
        read: impl FnOnce(SessionReader) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let input = serde_json::to_vec(&json!({"protocol": crate::SSH_AGENT_PROTOCOL,
            "build": self.digest, "command": command, "args": args}))?;
        let script = self.script(
            r#"printf '\ncc-switch-ssh-response\n'
exec "$binary" --call "$root" "$expected""#,
        );
        run_ssh_with(
            &self.host,
            &script,
            Some(&input),
            Duration::from_secs(300),
            |stdout| parse_session_data(stdout, read),
        )
        .await
    }

    pub(super) async fn provider_command(&self, command: &str, args: Value) -> Result<Value> {
        self.request(command, args, |reader| Ok(serde_json::from_reader(reader)?))
            .await
    }

    pub(super) async fn rpc<T: DeserializeOwned + Send + 'static>(
        &self,
        command: &str,
        args: Value,
    ) -> Result<T> {
        let mut reply = self.provider_command(command, args).await?;
        if let Some(error) = reply.get("error") {
            bail!(
                "{}",
                error
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| error.to_string())
            );
        }
        Ok(serde_json::from_value(
            reply
                .as_object_mut()
                .and_then(|reply| reply.remove("data"))
                .context("Missing SSH agent response data")?,
        )?)
    }

    pub(crate) async fn sessions(&self) -> Result<Vec<SessionMeta>> {
        self.request("sessions", json!({}), |reader| {
            Ok(serde_json::from_reader(reader)?)
        })
        .await
    }

    pub(crate) async fn stream_messages(
        &self,
        app: &str,
        source_path: &str,
        send: impl FnMut(usize, &[SessionMessage]) -> Result<()> + Send + 'static,
    ) -> Result<u64> {
        self.request(
            "messages",
            json!({"app":app,"sourcePath":source_path}),
            move |reader| transcript::read(reader, send),
        )
        .await
    }

    pub(super) async fn usage_rows(&self, scan: bool) -> Result<super::usage::Rows> {
        self.request("usage", json!({"scan":scan}), |reader| {
            Ok(serde_json::from_reader(reader)?)
        })
        .await
    }
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
    #[ignore = "requires an SSH test host and installs the agent in its private user directory"]
    async fn remote_agent_smoke() {
        let host = std::env::var("CC_SWITCH_SSH_TEST_HOST").expect("set CC_SWITCH_SSH_TEST_HOST");
        let host = Endpoint {
            destination: host,
            port: None,
            identity_file: None,
        }
        .validate()
        .unwrap();

        let agent = RemoteAgent::connect(host.clone())
            .await
            .expect("install and connect");
        let sessions = agent.sessions().await.expect("read sessions");
        let usage_rows = agent.usage_rows(true).await.expect("read usage rows");
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
        let _: serde_json::Value = agent
            .rpc("get_providers", serde_json::json!({"app":"codex"}))
            .await
            .unwrap();
        if let Some(session) = sessions
            .iter()
            .find(|session| session.provider_id == "codex")
        {
            agent
                .stream_messages(
                    "codex",
                    session
                        .source_path
                        .as_deref()
                        .expect("Codex session source"),
                    |_, _| Ok(()),
                )
                .await
                .expect("read a Codex transcript");
        }
        RemoteAgent::connect(host)
            .await
            .expect("reuse the verified installation");
        println!(
            "CC Switch agent {}, {}. Sessions: {}.",
            agent.info().agent_version,
            agent.info().arch,
            sessions.len()
        );
    }
}
