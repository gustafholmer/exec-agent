#!/usr/bin/env python3
"""Claude Code session summaries in the exec_agent Postgres.

Hook entry points (read hook JSON on stdin, always exit 0):
  save              SessionEnd and PreCompact: insert a pending row, summarize in the background
  load              SessionStart: print the latest summary for this folder as additionalContext
  record            UserPromptSubmit and Stop: append the prompt or reply to our own session log,
                    used when Claude Code has not written a transcript file
Internal:
  summarize <id>    build the summary for one row with `claude -p`

Separate from the exec-agent daemon and its migrations. See
docs/superpowers/specs/2026-09-30-session-summaries-design.md.
"""

import glob
import json
import os
import shutil
import subprocess
import sys
import time
from datetime import datetime
from pathlib import Path

GUARD = "EA_SESSION_SUMMARY"
URL_FILE = Path.home() / ".config/exec-agent/database.url"
STATE_DIR = Path.home() / ".local/state/exec-agent"
LOG_FILE = STATE_DIR / "session-summaries.log"
SESSION_LOG_DIR = STATE_DIR / "session-logs"
PSQL_CANDIDATES = ["/opt/homebrew/opt/postgresql@17/bin/psql", "/usr/local/opt/postgresql@17/bin/psql"]
CLAUDE_CANDIDATES = [str(Path.home() / ".local/bin/claude")]

MAX_TRANSCRIPT_CHARS = 150_000
MIN_USER_MESSAGES = 2
LOAD_WAIT_SECONDS = 45
LOAD_PENDING_MAX_AGE = 300
SUMMARIZE_TIMEOUT = 240

SCHEMA_SQL = """
CREATE SCHEMA IF NOT EXISTS claude_code;
CREATE TABLE IF NOT EXISTS claude_code.session_summaries (
  id              bigserial PRIMARY KEY,
  session_id      text        NOT NULL,
  cwd             text        NOT NULL,
  git_branch      text,
  reason          text        NOT NULL,
  transcript_path text        NOT NULL,
  summary         text,
  status          text        NOT NULL DEFAULT 'pending',
  created_at      timestamptz NOT NULL DEFAULT now(),
  updated_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS session_summaries_cwd_created
  ON claude_code.session_summaries (cwd, created_at DESC);
"""

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


def ensure_schema():
    psql(SCHEMA_SQL)


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
    """Return (text, user_message_count): user and assistant text, tool traffic dropped."""
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
    joined = "\n\n".join(parts)
    return joined[-MAX_TRANSCRIPT_CHARS:], users


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


def cmd_save():
    data = read_hook_input()
    session_id = data.get("session_id") or ""
    cwd = data.get("cwd") or os.getcwd()
    event = data.get("hook_event_name") or ""
    reason = "compact" if event == "PreCompact" else (data.get("reason") or "other")
    transcript = data.get("transcript_path") or ""
    ensure_schema()
    row_id = psql(
        "INSERT INTO claude_code.session_summaries (session_id, cwd, git_branch, reason, transcript_path)"
        " VALUES (:'sid', :'cwd', NULLIF(:'branch', ''), :'reason', :'tp') RETURNING id;",
        sid=session_id, cwd=cwd, branch=git_branch(cwd) or "", reason=reason, tp=transcript,
    ).strip()
    log(f"save id={row_id} session={session_id} reason={reason} transcript={transcript}")
    env = dict(os.environ, **{GUARD: "1"})
    subprocess.Popen(
        [sys.executable, os.path.abspath(__file__), "summarize", row_id],
        env=env, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        start_new_session=True,
    )


def set_status(row_id, status, summary=None):
    psql(
        "UPDATE claude_code.session_summaries SET status = :'st', summary = NULLIF(:'sm', ''),"
        " updated_at = now() WHERE id = :'id'::bigint;",
        st=status, sm=summary or "", id=row_id,
    )


def cmd_summarize(row_id):
    row = psql(
        "SELECT json_build_object('sid', session_id, 'tp', transcript_path)"
        " FROM claude_code.session_summaries WHERE id = :'id'::bigint;",
        id=row_id,
    ).strip()
    if not row:
        log(f"summarize id={row_id}: no such row")
        return
    row = json.loads(row)
    path = find_transcript(row["tp"], row["sid"])
    if not path:
        log(f"summarize id={row_id}: transcript not found ({row['tp']})")
        set_status(row_id, "failed", "transcript not found")
        return
    if path != row["tp"]:
        psql("UPDATE claude_code.session_summaries SET transcript_path = :'tp' WHERE id = :'id'::bigint;",
             tp=path, id=row_id)
    text, users = extract_text(path)
    if users < MIN_USER_MESSAGES:
        set_status(row_id, "skipped")
        log(f"summarize id={row_id}: skipped, {users} user message(s)")
        return
    claude = first_existing(CLAUDE_CANDIDATES, "claude")
    work = STATE_DIR / "summarizer"
    work.mkdir(parents=True, exist_ok=True)
    try:
        r = subprocess.run(
            [claude, "-p", "--model", "haiku"],
            input=PROMPT.format(transcript=text), capture_output=True, text=True,
            timeout=SUMMARIZE_TIMEOUT, cwd=work,
        )
    except (OSError, subprocess.SubprocessError) as e:
        set_status(row_id, "failed")
        log(f"summarize id={row_id}: claude error {e}")
        return
    summary = r.stdout.strip()
    if r.returncode != 0 or not summary:
        set_status(row_id, "failed")
        log(f"summarize id={row_id}: claude exit {r.returncode}: {r.stderr.strip()[:300]}")
        return
    set_status(row_id, "done", summary)
    log(f"summarize id={row_id}: done ({len(summary)} chars)")


def latest_row(cwd, exclude_session):
    out = psql(
        "SELECT json_build_object('id', id, 'status', status, 'summary', summary,"
        " 'tp', transcript_path, 'branch', git_branch, 'reason', reason,"
        " 'created', to_char(created_at, 'YYYY-MM-DD HH24:MI'),"
        " 'age', extract(epoch FROM now() - created_at))"
        " FROM claude_code.session_summaries"
        " WHERE cwd = :'cwd' AND session_id <> :'sid' AND status IN ('pending', 'done')"
        " ORDER BY created_at DESC LIMIT 1;",
        cwd=cwd, sid=exclude_session,
    ).strip()
    return json.loads(out) if out else None


def cmd_load():
    data = read_hook_input()
    if data.get("source") not in ("startup", "clear"):
        return
    cwd = data.get("cwd") or os.getcwd()
    ensure_schema()
    row = latest_row(cwd, data.get("session_id") or "")
    if not row:
        return
    deadline = time.monotonic() + LOAD_WAIT_SECONDS
    while row and row["status"] == "pending" and row["age"] < LOAD_PENDING_MAX_AGE and time.monotonic() < deadline:
        time.sleep(2)
        row = latest_row(cwd, data.get("session_id") or "")
    if not row:
        return
    head = f"Previous Claude Code session in this folder ({row['created']}, ended by {row['reason']}"
    head += f", branch {row['branch']})" if row["branch"] else ")"
    if row["status"] == "done":
        body = f"{head}. Summary:\n\n{row['summary']}\n\nFull transcript: {row['tp']}"
    else:
        body = f"{head}. Its summary is not ready yet. Full transcript: {row['tp']}"
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
            cmd_summarize(sys.argv[2])
        else:
            print(__doc__, file=sys.stderr)
    except Exception as e:  # a hook must never break a session
        log(f"{cmd} error: {e!r}")


if __name__ == "__main__":
    main()
