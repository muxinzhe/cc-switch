import json
import os
import re
import sqlite3
import sys
from collections import Counter, defaultdict
from contextlib import closing
from datetime import datetime, timezone
from pathlib import Path
from urllib.parse import urlsplit

try:
    import tomllib
except ImportError:
    tomllib = None


def read_only(path):
    return sqlite3.connect(path.as_uri() + "?mode=ro", uri=True)


def codex_base_url(config):
    if tomllib is not None:
        doc = tomllib.loads(config)
        active = doc.get("model_providers", {}).get(doc.get("model_provider"), {})
        return active.get("base_url") or doc.get("base_url") or doc.get("openai_base_url")

    # Older Python 3: recognize the plain assignments and provider table
    # written by CC Switch. Leave more complex TOML to the full parser.
    if '"""' in config or "'''" in config:
        return None
    values = {}
    section = ""
    for line in config.splitlines():
        line = line.strip()
        if line.startswith("["):
            section = line.split("#", 1)[0].strip()
        match = re.fullmatch(
            r"""(model_provider|base_url|openai_base_url)\s*=\s*("(?:[^"\\]|\\.)*"|'[^']*')\s*(?:#.*)?""",
            line,
        )
        if match:
            key, value = match.groups()
            values[section, key] = json.loads(value) if value.startswith('"') else value[1:-1]
    provider = values.get(("", "model_provider"))
    if provider:
        for spelling in (provider, json.dumps(provider), "'" + provider + "'"):
            url = values.get((f"[model_providers.{spelling}]", "base_url"))
            if url:
                return url
    return values.get(("", "base_url")) or values.get(("", "openai_base_url"))


def endpoint(url, base=False):
    try:
        parsed = urlsplit(url.strip())
        if parsed.scheme not in ("http", "https") or not parsed.hostname:
            return None
        port = parsed.port or (443 if parsed.scheme == "https" else 80)
    except (AttributeError, ValueError):
        return None
    path = parsed.path.rstrip("/")
    if base:
        path += "/responses"
    return parsed.scheme, parsed.hostname, port, path


def codex_providers(path):
    providers = {}
    endpoints = defaultdict(set)
    try:
        with closing(read_only(path)) as db:
            rows = db.execute(
                "SELECT id, name, category, settings_config FROM providers WHERE app_type = 'codex'"
            ).fetchall()
    except sqlite3.Error:
        return providers, endpoints

    for provider_id, name, category, settings in rows:
        providers[provider_id] = name
        try:
            base = codex_base_url(json.loads(settings).get("config", ""))
        except (AttributeError, TypeError, ValueError):
            continue
        if base is None and category == "official":
            base = "https://chatgpt.com/backend-api/codex"
        key = endpoint(base, base=True)
        if key is not None:
            endpoints[key].add(provider_id)
    return providers, endpoints


TURN_ID = re.compile(r'\bturn_id="?([0-9a-fA-F-]{36})(?=["\s},]|$)')
HTTP_REQUEST = re.compile(
    r"(?:^|\}: )Request completed method=POST url=(\S+) status=(2\d{2})\b"
)
SAMPLING_USAGE = re.compile(r"(?:^|\}: )post sampling token usage\b")
ROLLOUT_THREAD = re.compile(r"([0-9a-fA-F-]{36})(?:_[0-9a-fA-F-]{36})?\.jsonl$")
COMPACTION_WINDOW = 120


def sampling_endpoints(db, thread_id, first, last):
    pending = defaultdict(set)
    samples = defaultdict(list)
    requests = defaultdict(list)
    for stamp, target, body in db.execute(
        """SELECT ts, target, feedback_log_body FROM logs
           WHERE thread_id = ? AND ts BETWEEN ? AND ?
             AND target IN ('codex_http_client::client', 'codex_core::session::turn')
             AND feedback_log_body IS NOT NULL
           ORDER BY ts, ts_nanos, id""",
        (thread_id, first - 600, last + COMPACTION_WINDOW),
    ):
        turn_ids = set(TURN_ID.findall(body))
        if len(turn_ids) != 1:
            continue
        turn_id = turn_ids.pop()
        if target == "codex_http_client::client":
            request = HTTP_REQUEST.search(body)
            if request:
                key = endpoint(request[1])
                if key is not None and key[-1].endswith("/responses"):
                    pending[turn_id].add(key)
                    requests[turn_id].append((stamp, key))
        elif SAMPLING_USAGE.search(body):
            # A completed sampling step consumes this turn's HTTP evidence.
            # A subsequent WebSocket step cannot inherit the previous address.
            matches = pending.pop(turn_id, set())
            samples[stamp].append(next(iter(matches)) if len(matches) == 1 else None)
    return samples, requests


def rollout_second(stamp):
    if isinstance(stamp, str) and stamp.endswith("Z"):
        try:
            return int(datetime.strptime(stamp[:19], "%Y-%m-%dT%H:%M:%S")
                       .replace(tzinfo=timezone.utc).timestamp())
        except ValueError:
            pass
    return None


def compaction_usage(file):
    record = None
    with open(file, "rb") as rollout:
        for line in rollout:
            if b'"token_usage_record"' not in line and b'"compacted"' not in line:
                continue
            try:
                event = json.loads(line.decode("utf-8"))
            except (UnicodeError, ValueError):
                continue
            if not isinstance(event, dict):
                continue
            stamp = rollout_second(event.get("timestamp"))
            if event.get("type") == "token_usage_record":
                payload = event.get("payload")
                record = (stamp, payload) if isinstance(payload, dict) else None
            elif event.get("type") == "compacted" and record:
                used_at, payload = record
                if used_at is not None and stamp in (used_at, used_at + 1):
                    yield used_at, payload
                record = None


def compaction_providers(database, hints):
    if not hints:
        return {}
    resolved = defaultdict(set)
    try:
        with closing(read_only(database)) as db:
            paths = db.execute("SELECT file_path FROM session_log_sync").fetchall()
        for file, in paths:
            thread = ROLLOUT_THREAD.search(file)
            if not thread or thread[1] not in hints:
                continue
            try:
                for stamp, payload in compaction_usage(file):
                    session = payload.get("thread_id")
                    if session != thread[1]:
                        continue
                    hint = hints[session].get(stamp, {}).get(payload.get("turn_id"))
                    usage = payload.get("usage")
                    if hint and isinstance(usage, dict):
                        request, expected, provider = hint
                        tokens = tuple(usage.get(field) for field in (
                            "input_tokens", "output_tokens", "cached_input_tokens"
                        ))
                        if tokens == expected:
                            resolved[request].add(provider)
            except OSError:
                continue
    except sqlite3.Error:
        return {}
    return {request: next(iter(ids)) for request, ids in resolved.items() if len(ids) == 1}


def resolve_attributions(path, candidates, attributed, endpoints, database, index):
    pending = candidates.keys() - attributed.keys()
    if not pending or not endpoints:
        return []
    counts = Counter((session, stamp) for _, session, stamp in candidates)
    bounds = {}
    sessions = defaultdict(list)
    for key in pending:
        _, session, stamp = key
        first, last = bounds.get(session, (stamp, stamp))
        bounds[session] = min(first, stamp), max(last, stamp)
        if counts[session, stamp] == 1:
            sessions[session].append(key)

    matched = {}
    compactions = defaultdict(dict)
    try:
        with closing(read_only(path)) as db:
            for session, (first, last) in bounds.items():
                samples_by_time, requests = sampling_endpoints(db, session, first, last)
                for stamp, samples in samples_by_time.items():
                    if len(samples) != counts[session, stamp] or len(set(samples)) != 1:
                        continue
                    ids = endpoints.get(samples[0], ())
                    if len(ids) == 1:
                        matched[session, stamp] = next(iter(ids))

                # Codex can install compaction in the same turn without a sampling
                # completion trace. Require a unique provider on both sides and
                # confirm the turn and token counts against the native rollout.
                for turn, calls in requests.items():
                    keys = {endpoint_key for _, endpoint_key in calls}
                    if len(keys) != 1 or len(calls) < 2:
                        continue
                    ids = endpoints.get(next(iter(keys)), ())
                    if len(ids) != 1:
                        continue
                    provider = next(iter(ids))
                    for key in sessions[session]:
                        stamp = key[2]
                        if (session, stamp) in matched or not calls[0][0] < stamp < calls[-1][0]:
                            continue
                        nearby = [ts for ts, _ in calls if abs(ts - stamp) <= COMPACTION_WINDOW]
                        if not nearby or not nearby[0] < stamp < nearby[-1]:
                            continue
                        row = candidates[key]
                        expected = tuple(row[index[name]] for name in (
                            "input_tokens", "output_tokens", "cache_read_tokens"
                        ))
                        compactions[session].setdefault(stamp, {})[turn] = (key, expected, provider)
    except sqlite3.Error:
        # A missing/older Codex trace DB is normal. Usage still comes from rollouts.
        return []
    normal = [
        (request, session, stamp, matched[session, stamp])
        for request, session, stamp in pending
        if (session, stamp) in matched
    ]
    compact = compaction_providers(database, compactions)
    return normal + [(*key, provider) for key, provider in compact.items()]


def read_attributions(db):
    return {
        (request, session, stamp): (provider, name)
        for request, session, stamp, provider, name in db.execute("SELECT * FROM attribution")
    }


def attribute_codex(logs, fields, cache_path, provider_path, database, codex_home=None):
    index = {name: i for i, name in enumerate(fields.split(","))}
    candidates = {
        (row[index["request_id"]], row[index["session_id"]], row[index["created_at"]]): row
        for row in logs
        if row[index["data_source"]] == "codex_session"
        and row[index["provider_id"]] == "_codex_session"
        and row[index["session_id"]]
    }
    if not candidates:
        return {}

    names, endpoints = codex_providers(provider_path)
    if codex_home is None:
        codex_home = Path(os.environ.get("CODEX_HOME") or Path.home() / ".codex").expanduser()
    trace_path = codex_home.absolute() / "logs_2.sqlite"
    attributed = {}
    try:
        # Keep matches after Codex rotates its trace log. Only the provider
        # identity and the usage row key are saved in the private scanner cache.
        with closing(sqlite3.connect(str(cache_path))) as db:
            db.execute(
                """CREATE TABLE IF NOT EXISTS attribution (
                       request_id TEXT NOT NULL, session_id TEXT NOT NULL, created_at INTEGER NOT NULL,
                       provider_id TEXT NOT NULL, provider_name TEXT NOT NULL,
                       PRIMARY KEY (request_id, session_id, created_at)
                   )"""
            )
            attributed = read_attributions(db)
            new = resolve_attributions(trace_path, candidates, attributed, endpoints, database, index)
            if new:
                with db:
                    db.executemany(
                        "INSERT OR IGNORE INTO attribution VALUES (?, ?, ?, ?, ?)",
                        [(*match, names[match[-1]]) for match in new],
                    )
                attributed = read_attributions(db)
    except sqlite3.Error:
        for request, session, stamp, provider in resolve_attributions(
            trace_path, candidates, attributed, endpoints, database, index
        ):
            attributed[request, session, stamp] = provider, names[provider]

    used = {}
    for key, row in candidates.items():
        if key in attributed:
            provider, name = attributed[key]
            row[index["provider_id"]] = provider
            used[provider] = names.get(provider, name)
    return used


def local_attributions(database, codex_home):
    database = Path(database)
    columns = ("request_id,session_id,created_at,provider_id,data_source,"
               "input_tokens,output_tokens,cache_read_tokens")
    with closing(read_only(database)) as db:
        rows = [list(row) for row in db.execute(
            "SELECT " + columns + " FROM proxy_request_logs "
            "WHERE app_type = 'codex' AND data_source = 'codex_session' "
            "AND provider_id = '_codex_session' AND session_id IS NOT NULL"
        )]
    names = attribute_codex(
        rows, columns, database.with_name("codex-usage-attribution.db"),
        database, database, Path(codex_home),
    )
    print(json.dumps({
        "requests": [row[:4] for row in rows if row[3] in names],
        "providers": list(names.items()),
    }, separators=(",", ":")))


def main():
    if sys.argv[1] == "--attributions":
        return local_attributions(*sys.argv[2:])
    database, provider_database, columns, rollup_columns, *cache = sys.argv[1:]
    database = Path(database)
    with closing(read_only(database)) as db:
        if db.execute("PRAGMA user_version").fetchone()[0] not in (26, 27):
            raise ValueError("Unsupported CC Switch CLI usage cache schema")
        db.execute("BEGIN")
        logs = [
            list(row)
            for row in db.execute(
                "SELECT " + columns + " FROM proxy_request_logs WHERE data_source != 'proxy'"
            )
        ]
        rollups = db.execute("SELECT " + rollup_columns + " FROM usage_daily_rollups").fetchall()
    providers = attribute_codex(
        logs, columns,
        Path(cache[0]) if cache else database.with_name("codex-usage-attribution.db"),
        Path(provider_database),
        database,
    )
    print(json.dumps(
        {"logs": logs, "rollups": rollups, "codex_providers": list(providers.items())},
        separators=(",", ":"),
    ))


if __name__ == "__main__":
    main()
