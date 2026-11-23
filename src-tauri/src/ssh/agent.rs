use anyhow::{ensure, Context, Result};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::time::Duration;
use tauri::State;

use super::{
    cli::{run_ssh, RemoteCli},
    remote, Registry,
};

include!(concat!(env!("OUT_DIR"), "/ssh_agent_assets.rs"));

pub(super) const PROTOCOL: u16 = 1;

#[tauri::command]
pub(crate) async fn ssh_provider_command(
    state: State<'_, Registry>,
    host_id: String,
    command: String,
    args: Map<String, Value>,
) -> Result<Value, String> {
    let cli = remote(&state, &host_id).await?;
    cli.provider_command(&command, args)
        .await
        .map_err(|error| format!("{error:#}"))
}

impl RemoteCli {
    async fn provider_command(&self, command: &str, args: Map<String, Value>) -> Result<Value> {
        let digest = self
            .agent()
            .get_or_try_init(|| self.install_agent())
            .await?;
        let script = self.agent_script(
            digest,
            r#"
printf '\ncc-switch-ssh-response\n'
exec "$binary" --call "$service" "$expected_digest"
"#,
        );
        let input = serde_json::to_vec(&json!({
            "protocol": PROTOCOL,
            "command": command,
            "args": args,
        }))?;
        let response = run_ssh(
            self.endpoint(),
            &script,
            Some(&input),
            Duration::from_secs(150),
        )
        .await?;
        serde_json::from_str(&response).context("Invalid SSH configuration response")
    }

    fn agent_asset(&self) -> Option<&'static [u8]> {
        match self.arch() {
            "x86_64" => X86_64,
            "aarch64" => AARCH64,
            _ => None,
        }
    }

    pub(super) fn has_agent_asset(&self) -> bool {
        self.agent_asset().is_some()
    }

    pub(super) async fn ensure_agent_binary(&self) -> Result<&String> {
        self.agent_binary()
            .get_or_try_init(|| self.install_agent_binary())
            .await
    }

    async fn install_agent_binary(&self) -> Result<String> {
        let payload = self
            .agent_asset()
            .context("This build has no matching Linux SSH configuration backend")?;
        let digest = format!("{:x}", Sha256::digest(payload));
        let probe = self.agent_script(
            &digest,
            "printf '\\ncc-switch-ssh-response\\n'\nif verified_binary \"$binary\"; then printf 'ready\\n'; else printf 'missing\\n'; fi",
        );
        let status = run_ssh(self.endpoint(), &probe, None, Duration::from_secs(30)).await?;
        if status.trim() == "missing" {
            let upload = self.agent_script(
                &digest,
                r#"
temp=$(mktemp "$root/.agent.XXXXXXXX")
trap 'rm -f -- "$temp"' EXIT
trap 'exit 1' HUP INT TERM
cat > "$temp"
chmod 700 "$temp"
verified_binary "$temp"
if ! ln -T -- "$temp" "$binary" 2>/dev/null; then
    verified_binary "$binary"
fi
printf '\ncc-switch-ssh-response\nready\n'
"#,
            );
            let status = run_ssh(
                self.endpoint(),
                &upload,
                Some(payload),
                Duration::from_secs(150),
            )
            .await?;
            ensure!(
                status.trim() == "ready",
                "SSH configuration backend installation failed"
            );
        } else {
            ensure!(
                status.trim() == "ready",
                "Unexpected SSH configuration backend status"
            );
        }

        Ok(digest)
    }

    async fn install_agent(&self) -> Result<String> {
        let digest = self.ensure_agent_binary().await?;
        let start = self.agent_script(
            digest,
            r#"
private_dir "$service"
printf '\ncc-switch-ssh-response\n'
exec "$binary" --ensure "$service" "$expected_digest"
"#,
        );
        run_ssh(self.endpoint(), &start, None, Duration::from_secs(45)).await?;
        Ok(digest.clone())
    }

    fn agent_script(&self, digest: &str, operation: &str) -> String {
        self.script(&format!(
            r#"
private_dir "$root"
service="$root/provider-v{PROTOCOL}"
binary="$root/provider-agent-{digest}"
expected_digest="{digest}"
{operation}
"#
        ))
    }
}

#[tauri::command]
pub(crate) async fn ssh_open_provider_terminal(
    state: State<'_, Registry>,
    host_id: String,
    args: Map<String, Value>,
) -> Result<bool, String> {
    #[cfg(not(unix))]
    {
        let _ = (state, host_id, args);
        Err("Opening the SSH terminal requires a Unix desktop".into())
    }
    #[cfg(unix)]
    {
        let cli = remote(&state, &host_id).await?;
        let reply = cli
            .provider_command("prepare_provider_terminal", args)
            .await
            .map_err(|error| format!("{error:#}"))?;
        if let Some(error) = reply.get("error") {
            return Err(error
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| error.to_string()));
        }
        let script = reply["data"]
            .as_str()
            .filter(|path| path.starts_with('/') && !path.chars().any(char::is_control))
            .ok_or("Invalid SSH terminal script path")?;
        let script = shlex::try_quote(script).map_err(|error| error.to_string())?;
        let endpoint = cli.endpoint();
        let mut argv = [
            "ssh",
            "-tt",
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
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        if let Some(port) = endpoint.port {
            argv.extend(["-p".to_owned(), port.to_string()]);
        }
        if let Some(identity) = &endpoint.identity_file {
            argv.extend([
                "-i".to_owned(),
                identity.clone(),
                "-o".to_owned(),
                "IdentitiesOnly=yes".to_owned(),
            ]);
        }
        argv.extend([
            "--".to_owned(),
            endpoint.destination.clone(),
            format!("exec /bin/sh {script}"),
        ]);
        let command = argv
            .iter()
            .map(|arg| shlex::try_quote(arg).map(|quoted| quoted.into_owned()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?
            .join(" ");
        crate::commands::launch_terminal_running(&command, "ssh_provider")?;
        Ok(true)
    }
}
