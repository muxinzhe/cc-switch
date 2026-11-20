# Only return saved profile names and the active flag. Codex TOML is parsed by
# the native desktop process and only the resulting profile metadata reaches UI.
import json
import os
import sqlite3
import sys
from pathlib import Path

app = sys.argv[1]
home = Path.home()
original = os.environ.get("CC_SWITCH_SSH_ORIGINAL_CONFIG_DIR")
store = Path(original).expanduser() if original else home / ".cc-switch"
path = store / "cc-switch.db"
profiles = {}
if path.is_file():
    with sqlite3.connect(path.absolute().as_uri() + "?mode=ro", uri=True) as db:
        if db.execute("SELECT 1 FROM sqlite_master WHERE name='providers' AND type='table'").fetchone():
            for id_, name, current in db.execute(
                "SELECT id, name, is_current FROM providers WHERE app_type=? ORDER BY sort_index, name", (app,)
            ):
                profiles[id_] = {"id": id_, "name": name, "active": bool(current)}

codex_config = None
if app == "codex":
    path = Path(os.environ.get("CODEX_HOME") or home / ".codex") / "config.toml"
    if path.is_file():
        with path.open(encoding="utf-8") as handle:
            codex_config = handle.read(1024 * 1024 + 1)
        if len(codex_config) > 1024 * 1024:
            raise ValueError("Codex config is larger than 1 MiB")
elif app in ("opencode", "openclaw", "pi"):
    path, keys = {
        "opencode": (Path(os.environ.get("XDG_CONFIG_HOME") or home / ".config") / "opencode/opencode.json", ("provider",)),
        "openclaw": (home / ".openclaw/openclaw.json", ("models", "providers")),
        "pi": (Path(os.environ.get("PI_CODING_AGENT_DIR") or home / ".pi/agent") / "models.json", ("providers",)),
    }[app]
    if path.is_file():
        try:
            with path.open(encoding="utf-8") as handle:
                node = json.load(handle)
            for key in keys:
                node = node.get(key, {}) if isinstance(node, dict) else {}
            for id_, record in (node.items() if isinstance(node, dict) else []):
                name = record.get("name") if isinstance(record, dict) else None
                profiles.setdefault(id_, {"id": id_, "name": name if isinstance(name, str) else id_, "active": False})
        except ValueError:
            # A native config may use JSONC. In that case keep the saved profiles.
            pass
elif not profiles:
    path = {
        "claude": Path(os.environ.get("CLAUDE_CONFIG_DIR") or home / ".claude") / "settings.json",
        "gemini": home / ".gemini/settings.json",
        "hermes": Path(os.environ.get("HERMES_HOME") or home / ".hermes") / "config.yaml",
    }[app]
    if path.is_file():
        profiles["live"] = {"id": "live", "name": path.name, "active": True, "live": True}
print(json.dumps({"providers": list(profiles.values()), "codexConfig": codex_config}))
