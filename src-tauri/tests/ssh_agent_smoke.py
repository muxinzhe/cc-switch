"""Exercise the packaged SSH agent in a fresh, isolated user directory."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import sqlite3
import struct
import subprocess
import sys
import tempfile
import time


def check(binary, home, config, auth, current_codex_id):
    root = home / ".cc-switch-ssh"
    root.mkdir(mode=0o700)
    agent = root / "agent"
    shutil.copy2(binary, agent)
    digest = hashlib.sha256(agent.read_bytes()).hexdigest()
    # Exercise the agent with no CLI, Python, or other executables on its PATH.
    tools = home / "bin"
    tools.mkdir()
    codex = home / ".codex"
    env = {"HOME": str(home), "CC_SWITCH_TEST_HOME": str(home), "CODEX_HOME": str(codex),
           "PATH": str(tools), "LANG": "C"}
    codex.mkdir()
    config_path = codex / "config.toml"
    auth_path = codex / "auth.json"
    config_path.write_text(config)
    auth_text = json.dumps(auth, indent=2) + "\n"
    auth_path.write_text(auth_text)
    source = home / ".claude/projects/project/fixture.jsonl"
    source.parent.mkdir(parents=True)
    records = [
        {"type": "user", "message": {"role": "user", "content": "fixture question"}},
        {"type": "assistant", "message": {"id": "msg_fixture", "role": "assistant",
         "model": "claude-sonnet-4-5", "content": [{"type": "text", "text": "fixture answer"}],
         "stop_reason": "end_turn", "usage": {"input_tokens": 12, "output_tokens": 5}}},
    ]
    with source.open("w") as out:
        for record in records:
            record.update(sessionId="fixture", cwd="/project", timestamp="2026-01-01T10:00:00Z")
            print(json.dumps(record), file=out)

    def run(mode, request=None):
        return subprocess.run([agent, mode, root, digest], env=env, input=request, text=True,
                              stdout=subprocess.PIPE, timeout=45, check=True).stdout

    def call(command, **args):
        request = {"protocol": 2, "build": digest, "command": command, "args": args}
        reply = json.loads(run("--call", json.dumps(request)))
        if command in ("sessions", "messages", "usage"):
            return reply
        assert "error" not in reply, (command, reply)
        return reply["data"]

    def listener():
        with socket.socket(socket.AF_UNIX) as client:
            client.connect(str(root / "rpc.sock"))
            if sys.platform == "darwin":
                # Darwin sys/un.h: SOL_LOCAL = 0, LOCAL_PEERPID = 2.
                return client.getsockopt(0, 2)
            return struct.unpack("3i", client.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))[0]

    def check_codex_import():
        cards = call("get_providers", app="codex")
        assert set(cards) == {"codex-official", current_codex_id}, list(cards)
        assert cards["codex-official"]["name"] == "OpenAI Official"
        assert cards["codex-official"]["category"] == "official"
        assert call("get_current_provider", app="codex") == current_codex_id
        assert cards[current_codex_id]["settingsConfig"] == {"config": config, "auth": auth}
        if current_codex_id == "default":
            assert cards["codex-official"]["settingsConfig"] == {"config": "", "auth": {}}
        assert config_path.read_bytes() == config.encode(), "Codex config.toml changed"
        assert auth_path.read_bytes() == auth_text.encode(), "Codex auth.json changed"

    run("--ensure")
    pid = listener()
    try:
        run("--ensure")
        assert listener() == pid
        assert call("hello")["build"] == digest
        check_codex_import()
        sessions = call("sessions")
        session = next(s for s in sessions if s["sourcePath"] == str(source))
        messages = call("messages", app=session["providerId"], sourcePath=session["sourcePath"])
        assert len(messages["messages"]) == 2 and "fixture answer" in json.dumps(messages)
        for _ in range(2):
            rows = call("usage", scan=True)["logs"]
            assert len(rows) == 1 and rows[0][6:8] == [12, 5], rows

        app = "claude"
        original = next(iter(call("get_providers", app=app)))
        provider = {"id": "fixture", "name": "First", "settingsConfig": {
            "env": {"ANTHROPIC_BASE_URL": "http://127.0.0.1:9", "ANTHROPIC_AUTH_TOKEN": "fixture"}}}
        call("add_provider", app=app, provider=provider, addToLive=False)
        provider["name"] = "Updated"
        call("update_provider", app=app, provider=provider)
        assert call("get_providers", app=app)["fixture"]["name"] == "Updated"
        call("switch_provider", app=app, id="fixture")
        native = call("native_config", key="claude")
        assert json.loads(native["contents"])["env"]["ANTHROPIC_AUTH_TOKEN"] == "fixture"
        edited = json.loads(native["contents"])
        edited["fixture"] = True
        backup = call("native_config", key="claude", draft={
            "revision": native["revision"], "contents": json.dumps(edited)})["backupPath"]
        assert Path(backup).read_text() == native["contents"]

        settings = call("get_settings")
        settings["language"] = "ja"
        call("save_settings", settings=settings)

        os.kill(pid, signal.SIGTERM)
        for _ in range(100):
            try:
                listener()
            except ConnectionRefusedError:
                break
            time.sleep(.02)
        assert call("get_current_provider", app=app) == "fixture"
        check_codex_import()
        assert call("get_settings")["language"] == "ja"
        assert len(call("usage", scan=False)["logs"]) == 1

        # Replace the binary as an upload would. A new ad-hoc signature changes
        # the Mach-O digest while keeping the executable valid on Apple Silicon.
        previous = listener()
        staging = root / ".upgrade"
        shutil.copy2(agent, staging)
        if sys.platform == "darwin":
            subprocess.run(["/usr/bin/codesign", "--force", "--sign", "-", "--identifier",
                            "cc-switch-ssh-agent.smoke-upgrade", staging], check=True)
        else:
            with staging.open("ab") as out:
                out.write(b"smoke-test-build")
        staging.replace(agent)
        old_digest = digest
        digest = hashlib.sha256(agent.read_bytes()).hexdigest()
        assert digest != old_digest
        run("--ensure")
        assert listener() != previous and call("hello")["build"] == digest
        check_codex_import()
        assert call("get_current_provider", app=app) == "fixture"
        call("switch_provider", app=app, id=original)
        call("delete_provider", app=app, id="fixture")
        assert "fixture" not in call("get_providers", app=app)

        assert list(root.rglob("*.db")) == [root / "cc-switch.db"]
        assert [f for f in root.iterdir() if f.is_file() and os.access(f, os.X_OK)] == [agent]
        with sqlite3.connect(root / "cc-switch.db") as db:
            assert db.execute("select count(*) from proxy_request_logs").fetchone() == (1,)
            assert db.execute("select count(*) from providers").fetchone()[0] > 0
            assert db.execute("select count(*) from session_log_sync").fetchone()[0] > 0
    finally:
        try:
            os.kill(listener(), signal.SIGTERM)
        except (FileNotFoundError, ConnectionRefusedError):
            pass


if __name__ == "__main__":
    login = {"auth_mode": "chatgpt", "tokens": {"access_token": "fixture-login"}}
    # An unused third-party definition can coexist with the active official route.
    config = ('model = "gpt-5"\n[model_providers.vendor]\nname = "Fixture"\n'
              'base_url = "https://fixture.invalid/v1"\nrequires_openai_auth = true\n')
    for native_config, auth, current in (
        (config, login, "codex-official"),
        ('openai_base_url = "https://chatgpt.com/backend-api/codex/"\n' + config, login, "codex-official"),
        ('openai_base_url = "https://fixture.invalid/v1"\n' + config, login, "default"),
        ('model_provider = "vendor"\n' + config, login, "default"),
        ('profile = "work"\n' + config + '[profiles.work]\nmodel_provider = "vendor"\n', login, "default"),
        (config, {"OPENAI_API_KEY": "fixture-api-key"}, "default"),
    ):
        # macOS's per-user TMPDIR can exceed the Unix socket pathname limit.
        with tempfile.TemporaryDirectory(dir="/tmp") as directory:
            check(Path(sys.argv[1]).resolve(), Path(directory).resolve(), native_config, auth, current)
    print("SSH agent Codex import, sessions, usage, configuration CRUD, restart, and single database passed.")
