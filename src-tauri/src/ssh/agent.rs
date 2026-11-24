use super::{remote, Registry};
use serde_json::{Map, Value};
use tauri::State;

#[tauri::command]
pub(crate) async fn ssh_provider_command(
    state: State<'_, Registry>,
    host_id: String,
    command: String,
    args: Map<String, Value>,
) -> Result<Value, String> {
    let cli = remote(&state, &host_id).await?;
    cli.provider_command(&command, args.into())
        .await
        .map_err(|error| format!("{error:#}"))
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
            .provider_command("prepare_provider_terminal", args.into())
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
