import json
import os
import shutil
import socket
import sys
import tempfile
import threading
import unittest
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
    def test_load_prints_summary(self):
        row = {"session_id": "old", "reason": "clear", "git_branch": "main", "summary": "DONE\n- x",
               "created_at": "2026-09-30T12:34:56.1+00:00"}
        with mock.patch.object(ss, "read_hook_input", return_value={"source": "startup", "cwd": "/w", "session_id": "cur"}), \
                mock.patch.object(ss, "daemon_call", return_value=row) as dc, \
                mock.patch.object(ss, "find_transcript", return_value=None), \
                mock.patch("builtins.print") as pr:
            ss.cmd_load()
        self.assertEqual(dc.call_args[0][1], {"cwd": "/w", "exclude_session": "cur"})
        ctx = json.loads(pr.call_args[0][0])["hookSpecificOutput"]["additionalContext"]
        self.assertIn("(2026-09-30 12:34, ended by clear, branch main)", ctx)
        self.assertIn("DONE", ctx)
        self.assertNotIn("Full transcript", ctx)


if __name__ == "__main__":
    unittest.main()
