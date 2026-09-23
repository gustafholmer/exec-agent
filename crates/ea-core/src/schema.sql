CREATE TABLE IF NOT EXISTS events (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  source      TEXT NOT NULL,
  external_id TEXT NOT NULL,
  kind        TEXT NOT NULL,
  payload     TEXT NOT NULL,
  salience    INTEGER,
  triaged_at  TEXT,
  created_at  TEXT NOT NULL,
  UNIQUE (source, external_id)
);

CREATE TABLE IF NOT EXISTS actions (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  connector   TEXT NOT NULL,
  tool        TEXT NOT NULL,
  args        TEXT NOT NULL,
  preview     TEXT NOT NULL,
  rationale   TEXT NOT NULL,
  status      TEXT NOT NULL,
  reason      TEXT,
  result      TEXT,
  created_at  TEXT NOT NULL,
  expires_at  TEXT NOT NULL,
  decided_at  TEXT,
  executed_at TEXT
);
CREATE INDEX IF NOT EXISTS actions_status ON actions (status);

CREATE TABLE IF NOT EXISTS conversations (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  claude_session TEXT,
  created_at     TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS messages (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  conversation_id INTEGER NOT NULL REFERENCES conversations (id),
  role            TEXT NOT NULL,
  surface         TEXT NOT NULL,
  body            TEXT NOT NULL,
  created_at      TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS messages_conversation ON messages (conversation_id, id);

CREATE TABLE IF NOT EXISTS facts (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  topic      TEXT NOT NULL,
  body       TEXT NOT NULL,
  created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS runs (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  kind        TEXT NOT NULL,
  prompt      TEXT NOT NULL,
  outcome     TEXT NOT NULL,
  detail      TEXT,
  action_ids  TEXT,
  cost_usd    REAL,
  duration_ms INTEGER,
  started_at  TEXT NOT NULL,
  finished_at TEXT
);

CREATE TABLE IF NOT EXISTS schedules (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  name        TEXT NOT NULL UNIQUE,
  cron        TEXT NOT NULL,
  enabled     INTEGER NOT NULL DEFAULT 1,
  last_run_at TEXT
);

CREATE TABLE IF NOT EXISTS kv (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
