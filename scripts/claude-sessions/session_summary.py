#!/usr/bin/env python3
"""Claude Code session summaries in the exec_agent Postgres.

Hook entry points (read hook JSON on stdin, always exit 0):
  save              SessionEnd and PreCompact: upsert a pending row, summarize in the background
  load              SessionStart: print the latest summary for this folder as additionalContext
  record            UserPromptSubmit and Stop: append the prompt or reply to our own session log,
                    used when Claude Code has not written a transcript file
Internal:
  summarize <session_id> <transcript_path>
                    build the summary with `claude -p` and store it with the transcript

Writes go through the exec-agent daemon (claude_session.* over its unix socket). When the
daemon is not running, the same rows are written with psql. The table claude_sessions is
owned by the daemon's migrations. See
docs/superpowers/specs/2026-09-30-claude-sessions-in-daemon-design.md.
"""

import glob
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import time
import uuid
from datetime import datetime, timezone
from pathlib import Path

GUARD = "EA_SESSION_SUMMARY"
URL_FILE = Path.home() / ".config/exec-agent/database.url"
STATE_DIR = Path.home() / ".local/state/exec-agent"
SOCKET = Path(os.environ.get("EA_STATE_DIR") or STATE_DIR) / "daemon.sock"
LOG_FILE = STATE_DIR / "session-summaries.log"
SESSION_LOG_DIR = STATE_DIR / "session-logs"
PSQL_CANDIDATES = ["/opt/homebrew/opt/postgresql@17/bin/psql", "/usr/local/opt/postgresql@17/bin/psql"]
CLAUDE_CANDIDATES = [str(Path.home() / ".local/bin/claude")]

MAX_TRANSCRIPT_CHARS = 150_000
MAX_TRANSCRIPT_BYTES = 400_000
MAX_SUMMARY_BYTES = 8_000
MIN_USER_MESSAGES = 2
DAEMON_TIMEOUT = 5
SUMMARIZE_TIMEOUT = 240
LOAD_WAIT_SECONDS = 45
LOAD_PENDING_MAX_AGE = 300
LOAD_POLL_SECONDS = 2

PROMPT = """Below is a Claude Code session transcript (user and assistant text only).
Write a handoff note for the next session in the same folder. Format:

DONE
- ...
OPEN
- ...
NEXT
- ...

Rules: at most 15 lines in total. Be concrete: file paths, commit hashes, commands,
decisions and the reasons for them. Leave out anything that is only chit-chat.
Use commas, never em dashes. Plain wording. Output only the note.

<transcript>
{transcript}
</transcript>
"""


def log(msg):
    try:
        STATE_DIR.mkdir(parents=True, exist_ok=True)
        with LOG_FILE.open("a") as f:
            f.write(f"{datetime.now().isoformat(timespec='seconds')} [{os.getpid()}] {msg}\n")
    except OSError:
        pass


def first_existing(candidates, fallback):
    for c in candidates:
        if os.access(c, os.X_OK):
            return c
    return shutil.which(fallback)


def psql(sql, **params):
    """Run SQL with psql, values passed as -v variables (use :'name' in the SQL)."""
    url = URL_FILE.read_text().strip()
    exe = first_existing(PSQL_CANDIDATES, "psql")
    if not exe:
        raise RuntimeError("psql not found")
    cmd = [exe, "-X", "-q", "-t", "-A", "-v", "ON_ERROR_STOP=1", "-d", url]
    for k, v in params.items():
        cmd += ["-v", f"{k}={'' if v is None else v}"]
    r = subprocess.run(cmd, input=sql, capture_output=True, text=True, timeout=20)
    if r.returncode != 0:
        raise RuntimeError(f"psql failed: {r.stderr.strip()}")
    return r.stdout


def sql_lit(v):
    """SQL string literal (standard_conforming_strings), NULL for None; NUL bytes cannot be stored."""
    if v is None:
        return "NULL"
    return "'" + v.replace("\0", "").replace("'", "''") + "'"


def cap_tail(s, max_bytes):
    """The last max_bytes bytes of s, moved forward to a UTF-8 char boundary."""
    b = s.encode("utf-8")
    if len(b) <= max_bytes:
        return s
    return b[len(b) - max_bytes:].decode("utf-8", errors="ignore")


def cap_head(s, max_bytes):
    """The first max_bytes bytes of s, cut back to a UTF-8 char boundary."""
    b = s.encode("utf-8")
    if len(b) <= max_bytes:
        return s
    return b[:max_bytes].decode("utf-8", errors="ignore")


class DaemonError(Exception):
    """The daemon answered, with ok=false."""


def daemon_call(method, params, timeout=DAEMON_TIMEOUT):
    """One request/response on the daemon socket. Returns `data`; raises DaemonError on ok=false."""
    req = json.dumps({"id": str(uuid.uuid4()), "method": method, "params": params}) + "\n"
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
        s.settimeout(timeout)
        s.connect(str(SOCKET))
        s.sendall(req.encode("utf-8"))
        buf = b""
        while b"\n" not in buf:
            chunk = s.recv(65536)
            if not chunk:
                raise ConnectionError("daemon closed the connection without a full response")
            buf += chunk
    resp = json.loads(buf.split(b"\n", 1)[0])
    if not resp.get("ok"):
        raise DaemonError(resp.get("error") or "unknown daemon error")
    return resp.get("data")


def rpc(method, params, fallback):
    """Call the daemon; when it is down (socket error or timeout) run fallback() instead.
    A daemon-side error is logged and re-raised, never retried through the fallback."""
    try:
        return daemon_call(method, params)
    except DaemonError as e:
        log(f"{method}: daemon error: {e}")
        raise
    except (OSError, socket.timeout) as e:
        log(f"{method}: daemon unavailable ({e!r}), using psql")
    try:
        return fallback()
    except RuntimeError as e:
        if "does not exist" in str(e):
            log(f"{method}: claude_sessions table missing, is the daemon migrated? {e}")
            return None
        raise


def git_branch(cwd):
    try:
        r = subprocess.run(["git", "-C", cwd, "branch", "--show-current"],
                           capture_output=True, text=True, timeout=5)
        return r.stdout.strip() or None
    except (OSError, subprocess.SubprocessError):
        return None


def session_log(session_id):
    return SESSION_LOG_DIR / f"{os.path.basename(session_id)}.jsonl"


def find_transcript(path, session_id):
    """Claude Code's transcript when it has messages, else our own session log."""
    candidates = [path] if path else []
    if session_id:
        candidates += glob.glob(str(Path.home() / ".claude/projects/**" / f"{session_id}.jsonl"), recursive=True)
    candidates = [c for c in candidates if os.path.isfile(c) and extract_text(c)[1] > 0]
    if candidates:
        return max(candidates, key=os.path.getsize)
    if session_id and session_log(session_id).is_file():
        return str(session_log(session_id))
    return None


def extract_text(transcript_path):
    """Return (text, user_message_count): all user and assistant text, tool traffic dropped."""
    parts, users = [], 0
    with open(transcript_path, errors="replace") as f:
        for line in f:
            try:
                entry = json.loads(line)
            except json.JSONDecodeError:
                continue
            kind = entry.get("type")
            if kind not in ("user", "assistant") or entry.get("isMeta"):
                continue
            content = (entry.get("message") or {}).get("content")
            if isinstance(content, str):
                texts = [content]
            elif isinstance(content, list):
                texts = [b.get("text", "") for b in content if isinstance(b, dict) and b.get("type") == "text"]
            else:
                continue
            text = "\n".join(t for t in texts if t).strip()
            if not text or text.startswith(("<command-", "<local-command", "<system-reminder>")):
                continue
            if kind == "user":
                users += 1
            parts.append(f"[{kind}] {text}")
    return "\n\n".join(parts), users


def read_hook_input():
    try:
        return json.load(sys.stdin)
    except (json.JSONDecodeError, ValueError):
        return {}


def cmd_record():
    data = read_hook_input()
    session_id = data.get("session_id")
    event = data.get("hook_event_name")
    if event == "UserPromptSubmit":
        kind, text = "user", data.get("prompt")
    elif event == "Stop":
        kind, text = "assistant", data.get("last_assistant_message")
    else:
        return
    if not session_id or not isinstance(text, str) or not text.strip():
        return
    SESSION_LOG_DIR.mkdir(parents=True, exist_ok=True)
    entry = {"type": kind, "message": {"content": text}, "timestamp": datetime.now().isoformat(timespec="seconds")}
    with session_log(session_id).open("a") as f:
        f.write(json.dumps(entry) + "\n")


def save_fallback(p):
    psql(
        "INSERT INTO claude_sessions (session_id, cwd, git_branch, reason)"
        f" VALUES ({sql_lit(p['session_id'])}, {sql_lit(p['cwd'])}, {sql_lit(p['git_branch'])}, {sql_lit(p['reason'])})"
        " ON CONFLICT (session_id) DO UPDATE SET cwd = EXCLUDED.cwd, git_branch = EXCLUDED.git_branch,"
        " reason = EXCLUDED.reason, status = 'pending', updated_at = now();"
    )


def cmd_save():
    data = read_hook_input()
    session_id = data.get("session_id") or ""
    cwd = data.get("cwd") or os.getcwd()
    event = data.get("hook_event_name") or ""
    reason = "compact" if event == "PreCompact" else (data.get("reason") or "other")
    transcript = data.get("transcript_path") or ""
    if not session_id.strip():
        log("save: no session_id, nothing to record")
        return
    params = {"session_id": session_id, "cwd": cwd, "git_branch": git_branch(cwd), "reason": reason}
    rpc("claude_session.save", params, lambda: save_fallback(params))
    log(f"save session={session_id} reason={reason} transcript={transcript}")
    env = dict(os.environ, **{GUARD: "1"})
    subprocess.Popen(
        [sys.executable, os.path.abspath(__file__), "summarize", session_id, transcript],
        env=env, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        start_new_session=True,
    )


def finish_fallback(p):
    psql(
        f"UPDATE claude_sessions SET status = {sql_lit(p['status'])}, summary = {sql_lit(p.get('summary'))},"
        f" transcript = {sql_lit(p.get('transcript'))}, updated_at = now()"
        f" WHERE session_id = {sql_lit(p['session_id'])};"
    )


def finish(session_id, status, summary=None, transcript=None):
    params = {"session_id": session_id, "status": status}
    if summary:
        params["summary"] = cap_head(summary, MAX_SUMMARY_BYTES)
    if transcript:
        params["transcript"] = cap_tail(transcript, MAX_TRANSCRIPT_BYTES)
    rpc("claude_session.finish", params, lambda: finish_fallback(params))


def cmd_summarize(session_id, transcript_path):
    path = find_transcript(transcript_path, session_id)
    if not path:
        log(f"summarize {session_id}: transcript not found ({transcript_path})")
        finish(session_id, "failed", "transcript not found")
        return
    text, users = extract_text(path)
    if users < MIN_USER_MESSAGES:
        finish(session_id, "skipped", transcript=text)
        log(f"summarize {session_id}: skipped, {users} user message(s)")
        return
    claude = first_existing(CLAUDE_CANDIDATES, "claude")
    work = STATE_DIR / "summarizer"
    work.mkdir(parents=True, exist_ok=True)
    try:
        r = subprocess.run(
            [claude, "-p", "--model", "haiku"],
            input=PROMPT.format(transcript=text[-MAX_TRANSCRIPT_CHARS:]), capture_output=True, text=True,
            timeout=SUMMARIZE_TIMEOUT, cwd=work,
        )
    except (OSError, subprocess.SubprocessError) as e:
        finish(session_id, "failed", transcript=text)
        log(f"summarize {session_id}: claude error {e}")
        return
    summary = r.stdout.strip()
    if r.returncode != 0 or not summary:
        finish(session_id, "failed", transcript=text)
        log(f"summarize {session_id}: claude exit {r.returncode}: {r.stderr.strip()[:300]}")
        return
    finish(session_id, "done", summary, text)
    log(f"summarize {session_id}: done ({len(summary)} chars)")


def latest_fallback(p):
    if p.get("include_pending"):
        status = "status IN ('pending', 'done')"
    else:
        status = "status = 'done' AND summary IS NOT NULL"
    out = psql(
        "SELECT row_to_json(t) FROM (SELECT id, session_id, cwd, git_branch, reason, status, summary,"
        " created_at, updated_at FROM claude_sessions"
        f" WHERE cwd = {sql_lit(p['cwd'])} AND {status}"
        + (f" AND session_id <> {sql_lit(p['exclude_session'])}" if p.get("exclude_session") else "")
        + " ORDER BY created_at DESC, id DESC LIMIT 1) t;"
    ).strip()
    return json.loads(out) if out else None


def latest_row(cwd, exclude_session):
    """Newest pending or done row for cwd, so load can wait for a summary still being written."""
    params = {"cwd": cwd, "include_pending": True}
    if exclude_session:
        params["exclude_session"] = exclude_session
    return rpc("claude_session.latest", params, lambda: latest_fallback(params))


def parse_ts(value):
    """A daemon or psql timestamp as an aware datetime, or None."""
    text = str(value).replace("Z", "+00:00")
    # older Pythons only accept 3 or 6 fractional digits
    text = re.sub(r"\.(\d+)", lambda m: "." + m.group(1)[:6].ljust(6, "0"), text, count=1)
    try:
        return datetime.fromisoformat(text)
    except ValueError:
        return None


def row_age(row):
    """Seconds since the row was last set pending (updated_at, else created_at)."""
    ts = parse_ts(row.get("updated_at") or row.get("created_at"))
    if ts is None:
        return float("inf")
    if ts.tzinfo is None:
        ts = ts.astimezone()
    return (datetime.now(timezone.utc) - ts).total_seconds()


def local_stamp(value):
    ts = parse_ts(value)
    if ts is None:
        return str(value or "")[:16].replace("T", " ")
    return ts.astimezone().strftime("%Y-%m-%d %H:%M")


def cmd_load():
    data = read_hook_input()
    if data.get("source") not in ("startup", "clear"):
        return
    cwd = data.get("cwd") or os.getcwd()
    exclude = data.get("session_id") or ""
    row = latest_row(cwd, exclude)
    if not row:
        return
    deadline = time.monotonic() + LOAD_WAIT_SECONDS
    while row and row["status"] == "pending" and row_age(row) < LOAD_PENDING_MAX_AGE and time.monotonic() < deadline:
        time.sleep(LOAD_POLL_SECONDS)
        row = latest_row(cwd, exclude)
    if not row:
        return
    head = f"Previous Claude Code session in this folder ({local_stamp(row.get('created_at'))}, ended by {row['reason']}"
    head += f", branch {row['git_branch']})" if row.get("git_branch") else ")"
    try:
        tp = find_transcript(None, row["session_id"])
    except OSError:
        tp = None
    tail = f" Full transcript: {tp}" if tp else ""
    if row["status"] == "done" and row.get("summary"):
        body = f"{head}. Summary:\n\n{row['summary']}" + (f"\n\nFull transcript: {tp}" if tp else "")
    else:
        body = f"{head}. Its summary is not ready yet.{tail}"
    print(json.dumps({"hookSpecificOutput": {"hookEventName": "SessionStart", "additionalContext": body}}))


def main():
    if os.environ.get(GUARD) and sys.argv[1:2] != ["summarize"]:
        return
    cmd = sys.argv[1] if len(sys.argv) > 1 else ""
    try:
        if cmd == "save":
            cmd_save()
        elif cmd == "load":
            cmd_load()
        elif cmd == "record":
            cmd_record()
        elif cmd == "summarize" and len(sys.argv) > 2:
            cmd_summarize(sys.argv[2], sys.argv[3] if len(sys.argv) > 3 else "")
        else:
            print(__doc__, file=sys.stderr)
    except Exception as e:  # a hook must never break a session
        log(f"{cmd} error: {e!r}")


if __name__ == "__main__":
    main()
