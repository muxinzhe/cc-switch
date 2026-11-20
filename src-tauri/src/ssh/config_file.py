import fcntl
import hashlib
import json
import os
import stat
import sys
import tempfile
from pathlib import Path

LIMIT = 1024 * 1024
operation, key, private_root = sys.argv[1:]
root = Path(private_root)
home = Path.home()


def directory(variable, default):
    return Path(os.environ.get(variable) or default).expanduser()


def config_path():
    if key == "openCode":
        path = directory("XDG_CONFIG_HOME", home / ".config") / "opencode"
        jsonc = path / "opencode.jsonc"
        return jsonc if jsonc.exists() else path / "opencode.json"

    if key == "openClaw" and os.environ.get("OPENCLAW_CONFIG_PATH"):
        return Path(os.environ["OPENCLAW_CONFIG_PATH"]).expanduser()

    gemini = directory("GEMINI_CLI_HOME", home) / ".gemini"
    return {
        "claude": directory("CLAUDE_CONFIG_DIR", home / ".claude") / "settings.json",
        "codex": directory("CODEX_HOME", home / ".codex") / "config.toml",
        "geminiSettings": gemini / "settings.json",
        "geminiEnv": gemini / ".env",
        "openClaw": directory("OPENCLAW_STATE_DIR", home / ".openclaw") / "openclaw.json",
        "hermes": directory("HERMES_HOME", home / ".hermes") / "config.yaml",
        "pi": directory("PI_CODING_AGENT_DIR", home / ".pi/agent") / "models.json",
    }[key]


def read_bytes(path):
    try:
        fd = os.open(str(path), os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except FileNotFoundError:
        return None
    with os.fdopen(fd, "rb") as handle:
        info = os.fstat(handle.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid():
            raise ValueError("The application config must be a regular file owned by the SSH user")
        data = handle.read(LIMIT + 1)
        if len(data) > LIMIT:
            raise ValueError("The application config exceeds 1 MiB")
        return data


def revision(path, data):
    marker = b"missing" if data is None else b"file:" + data
    return hashlib.sha256(os.fsencode(str(path)) + b"\0" + marker).hexdigest()


def private_temp(directory, prefix, content):
    fd, name = tempfile.mkstemp(dir=str(directory), prefix=prefix)
    try:
        with os.fdopen(fd, "wb") as handle:
            handle.write(content)
            handle.flush()
            os.fsync(handle.fileno())
    except BaseException:
        os.unlink(name)
        raise
    return name


def save(path, expected, contents):
    current = read_bytes(path)
    if current == contents:
        return {"backupPath": None}
    if revision(path, current) != expected:
        raise ValueError("The remote configuration has changed. Reopen it before saving.")

    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    draft = private_temp(path.parent, ".cc-switch-config-", contents)
    backup = None
    try:
        if current is not None:
            backup = private_temp(root / "config-backups", key + "-", current)
        if read_bytes(path) != current:
            raise ValueError("The remote configuration has changed. Reopen it before saving.")
        os.replace(draft, str(path))
        draft = None
    finally:
        if draft is not None:
            os.unlink(draft)
            if backup is not None:
                os.unlink(backup)
    return {"backupPath": backup}


try:
    path = config_path().resolve()
    if operation == "read":
        current = read_bytes(path)
        response = {
            "path": str(path),
            "contents": current.decode("utf-8") if current is not None else "",
            "revision": revision(path, current),
            "exists": current is not None,
        }
    elif operation == "save":
        payload = json.loads(sys.stdin.buffer.read(6 * LIMIT + 1024).decode("utf-8"))
        contents = payload["contents"].encode("utf-8")
        if len(contents) > LIMIT:
            raise ValueError("The application config exceeds 1 MiB")
        lock_path = root / "config-backups" / ".lock"
        lock_fd = os.open(str(lock_path), os.O_CREAT | os.O_WRONLY | os.O_NOFOLLOW, 0o600)
        with os.fdopen(lock_fd, "wb") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            response = save(path, payload["revision"], contents)
    else:
        raise ValueError("Unknown configuration operation")
    print(json.dumps(response))
except UnicodeError:
    sys.exit("The application config must use UTF-8")
except (OSError, ValueError, KeyError) as error:
    sys.exit(str(error))
