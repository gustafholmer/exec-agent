import json
import os
import shutil
import socket
import sys
import tempfile
import threading
import unittest
from datetime import datetime, timedelta, timezone
from pathlib import Path
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import session_summary as ss  # noqa: E402


class FakeDaemon:
    """One-shot AF_UNIX server answering with a canned response; records the request."""

    def __init__(self, path, response):
        self.request = None
        self.response = response
        self.srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.srv.bind(str(path))
        self.srv.listen(1)
        self.thread = threading.Thread(target=self.serve, daemon=True)
        self.thread.start()

    def serve(self):
        conn, _ = self.srv.accept()
        with conn:
            buf = b""
            while b"\n" not in buf:
                buf += conn.recv(4096)
            self.request = json.loads(buf)
            resp = dict(self.response, id=self.request["id"])
            conn.sendall((json.dumps(resp) + "\n").encode())

    def close(self):
        self.thread.join(2)
        self.srv.close()


class Base(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.mkdtemp(dir="/tmp")
        self.addCleanup(shutil.rmtree, self.dir, True)
        self.sock = Path(self.dir) / "d.sock"
        for name, val in (("SOCKET", self.sock), ("LOG_FILE", Path(self.dir) / "log"),
                          ("STATE_DIR", Path(self.dir))):
            p = mock.patch.object(ss, name, val)
            p.start()
            self.addCleanup(p.stop)


class DaemonCallTests(Base):
    def test_round_trip(self):
        d = FakeDaemon(self.sock, {"ok": True, "data": {"updated": True}})
        self.addCleanup(d.close)
        out = ss.daemon_call("claude_session.finish", {"session_id": "s", "status": "done"})
        self.assertEqual(out, {"updated": True})
        self.assertEqual(d.request["method"], "claude_session.finish")
        self.assertEqual(d.request["params"], {"session_id": "s", "status": "done"})
        self.assertTrue(d.request["id"])

    def test_ok_false_raises_daemon_error(self):
        d = FakeDaemon(self.sock, {"ok": False, "error": "boom"})
        self.addCleanup(d.close)
        with self.assertRaisesRegex(ss.DaemonError, "boom"):
            ss.daemon_call("m", {})


class RpcTests(Base):
    def test_falls_back_when_socket_missing(self):
        fb = mock.Mock(return_value="fell back")
        self.assertEqual(ss.rpc("m", {}, fb), "fell back")
        fb.assert_called_once()

    def test_no_fallback_on_daemon_error(self):
        d = FakeDaemon(self.sock, {"ok": False, "error": "bad params"})
        self.addCleanup(d.close)
        fb = mock.Mock()
        with self.assertRaises(ss.DaemonError):
            ss.rpc("m", {}, fb)
        fb.assert_not_called()

    def test_missing_table_in_fallback_is_logged_not_raised(self):
        fb = mock.Mock(side_effect=RuntimeError('psql failed: relation "claude_sessions" does not exist'))
        self.assertIsNone(ss.rpc("m", {}, fb))
        self.assertIn("table missing", ss.LOG_FILE.read_text())

    def test_returns_daemon_data(self):
        d = FakeDaemon(self.sock, {"ok": True, "data": None})
        self.addCleanup(d.close)
        fb = mock.Mock()
        self.assertIsNone(ss.rpc("m", {}, fb))
        fb.assert_not_called()


class CapTests(unittest.TestCase):
    def test_cap_tail_multibyte(self):
        self.assertEqual(ss.cap_tail("aéb", 2), "b")
        self.assertEqual(ss.cap_tail("aéb", 3), "éb")
        self.assertEqual(ss.cap_tail("€€", 4), "€")
        self.assertEqual(ss.cap_tail("hello", 10), "hello")
        big = "é" * 300_000
        out = ss.cap_tail(big, ss.MAX_TRANSCRIPT_BYTES)
        self.assertLessEqual(len(out.encode()), ss.MAX_TRANSCRIPT_BYTES)
        self.assertEqual(len(out.encode()), ss.MAX_TRANSCRIPT_BYTES)

    def test_cap_head_multibyte(self):
        self.assertEqual(ss.cap_head("aéb", 2), "a")
        self.assertEqual(ss.cap_head("aéb", 3), "aé")

    def test_sql_lit(self):
        self.assertEqual(ss.sql_lit(None), "NULL")
        self.assertEqual(ss.sql_lit("it's\\"), "'it''s\\'")


class FinishTests(Base):
    def test_finish_caps_before_sending(self):
        with mock.patch.object(ss, "daemon_call") as dc:
            ss.finish("s", "done", "x" * 9000, "y" * 500_000 + "TAIL")
        params = dc.call_args[0][1]
        self.assertEqual(len(params["summary"]), ss.MAX_SUMMARY_BYTES)
        self.assertEqual(len(params["transcript"].encode()), ss.MAX_TRANSCRIPT_BYTES)
        self.assertTrue(params["transcript"].endswith("TAIL"))

    def _sent_line(self, transcript):
        d = FakeDaemon(self.sock, {"ok": True, "data": {}})
        self.addCleanup(d.close)
        captured = []
        real = ss.encode_request

        def spy(method, params):
            line = real(method, params)
            captured.append(line)
            return line
        with mock.patch.object(ss, "encode_request", spy):
            ss.finish("s", "done", "the summary", transcript)
        return captured[0], d.request

    def test_multibyte_transcript_fits_the_ipc_line_limit(self):
        # 400 kB of 4-byte chars would be 2.4 MB with ensure_ascii escapes.
        line, req = self._sent_line("\U0001F600" * 100_000 + "TAIL")
        self.assertLessEqual(len(line), 900000)
        self.assertEqual(req["params"]["summary"], "the summary")
        self.assertTrue(req["params"]["transcript"].endswith("TAIL"))

    def test_escape_heavy_transcript_is_shrunk_to_fit(self):
        line, req = self._sent_line('"\n\x01' * 133_000 + "TAIL")
        self.assertLessEqual(len(line), 900000)
        self.assertEqual(req["params"]["summary"], "the summary")
        self.assertTrue(req["params"]["transcript"].endswith("TAIL"))
        self.assertGreater(len(req["params"]["transcript"]), 1000)

    def test_finish_fallback_sql_targets_claude_sessions(self):
        with mock.patch.object(ss, "daemon_call", side_effect=FileNotFoundError), \
                mock.patch.object(ss, "psql") as ps:
            ss.finish("s'1", "skipped", transcript="t")
        sql = ps.call_args[0][0]
        self.assertIn("UPDATE claude_sessions", sql)
        self.assertIn("'s''1'", sql)
        self.assertIn("summary = NULL", sql)


class SaveTests(Base):
    def test_save_fallback_upserts_and_spawns_summarize(self):
        hook = {"session_id": "sid", "cwd": "/w", "hook_event_name": "PreCompact", "transcript_path": "/t.jsonl"}
        with mock.patch.object(ss, "read_hook_input", return_value=hook), \
                mock.patch.object(ss, "git_branch", return_value=None), \
                mock.patch.object(ss, "daemon_call", side_effect=ConnectionRefusedError), \
                mock.patch.object(ss, "psql") as ps, \
                mock.patch.object(ss.subprocess, "Popen") as po:
            ss.cmd_save()
        sql = ps.call_args[0][0]
        self.assertIn("ON CONFLICT (session_id) DO UPDATE", sql)
        self.assertIn("status = 'pending'", sql)
        self.assertIn("'compact'", sql)
        self.assertEqual(po.call_args[0][0][-3:], ["summarize", "sid", "/t.jsonl"])

    def test_no_spawn_when_daemon_rejects(self):
        hook = {"session_id": "sid", "cwd": "/w"}
        with mock.patch.object(ss, "read_hook_input", return_value=hook), \
                mock.patch.object(ss, "git_branch", return_value=None), \
                mock.patch.object(ss, "daemon_call", side_effect=ss.DaemonError("x")), \
                mock.patch.object(ss.subprocess, "Popen") as po:
            with self.assertRaises(ss.DaemonError):
                ss.cmd_save()
        po.assert_not_called()


class LoadTests(Base):
    HOOK = {"source": "clear", "cwd": "/w", "session_id": "cur"}

    def run_load(self, rows, monotonic=None):
        """Run cmd_load with daemon_call answering from rows; returns (context, call params, sleeps)."""
        answers = iter(rows)
        clock = iter(monotonic) if monotonic else None
        with mock.patch.object(ss, "read_hook_input", return_value=self.HOOK), \
                mock.patch.object(ss, "daemon_call", side_effect=lambda m, p: next(answers)) as dc, \
                mock.patch.object(ss, "find_transcript", return_value=None), \
                mock.patch.object(ss.time, "sleep") as sl, \
                mock.patch.object(ss.time, "monotonic", side_effect=(lambda: next(clock)) if clock else (lambda: 0.0)), \
                mock.patch("builtins.print") as pr:
            ss.cmd_load()
        ctx = json.loads(pr.call_args[0][0])["hookSpecificOutput"]["additionalContext"] if pr.call_args else None
        return ctx, dc, sl

    @staticmethod
    def row(status, summary=None, age=5):
        ts = (datetime.now(timezone.utc) - timedelta(seconds=age)).isoformat()
        return {"session_id": "old", "reason": "clear", "git_branch": "main", "status": status,
                "summary": summary, "created_at": ts, "updated_at": ts}

    def test_done_row_prints_summary_with_local_time(self):
        row = self.row("done", "DONE\n- x")
        row["created_at"] = "2026-09-30T12:34:56.1+00:00"
        ctx, dc, sl = self.run_load([row])
        self.assertEqual(dc.call_args[0][1], {"cwd": "/w", "include_pending": True, "exclude_session": "cur"})
        local = datetime(2026, 9, 30, 12, 34, tzinfo=timezone.utc).astimezone().strftime("%Y-%m-%d %H:%M")
        self.assertIn(f"({local}, ended by clear, branch main)", ctx)
        self.assertIn("DONE", ctx)
        self.assertNotIn("Full transcript", ctx)
        sl.assert_not_called()

    def test_pending_then_done_waits_and_prints_summary(self):
        ctx, dc, sl = self.run_load([self.row("pending"), self.row("pending"), self.row("done", "the note")])
        self.assertEqual(dc.call_count, 3)
        self.assertEqual(sl.call_count, 2)
        self.assertIn("Summary:\n\nthe note", ctx)

    def test_pending_until_timeout_says_not_ready(self):
        # monotonic: deadline base, then inside the window twice, then past it
        ctx, dc, sl = self.run_load([self.row("pending")] * 3, monotonic=[0, 1, 3, 100])
        self.assertEqual(sl.call_count, 2)
        self.assertIn("Its summary is not ready yet.", ctx)
        self.assertNotIn("Summary:", ctx)

    def test_stale_pending_is_not_waited_for(self):
        ctx, dc, sl = self.run_load([self.row("pending", age=ss.LOAD_PENDING_MAX_AGE + 60)])
        sl.assert_not_called()
        self.assertIn("not ready yet", ctx)

    def test_no_row_prints_nothing(self):
        ctx, dc, sl = self.run_load([None])
        self.assertIsNone(ctx)

    def test_fallback_query_includes_pending(self):
        with mock.patch.object(ss, "psql", return_value="") as ps:
            ss.latest_fallback({"cwd": "/w", "include_pending": True, "exclude_session": "cur"})
        self.assertIn("status IN ('pending', 'done')", ps.call_args[0][0])
        with mock.patch.object(ss, "psql", return_value="") as ps:
            ss.latest_fallback({"cwd": "/w"})
        self.assertIn("status = 'done' AND summary IS NOT NULL", ps.call_args[0][0])


if __name__ == "__main__":
    unittest.main()
