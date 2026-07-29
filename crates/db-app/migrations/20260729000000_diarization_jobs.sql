CREATE TABLE IF NOT EXISTS diarization_jobs (
  id                   TEXT PRIMARY KEY NOT NULL,
  session_id           TEXT NOT NULL DEFAULT '',
  status               TEXT NOT NULL DEFAULT 'running',
  model                TEXT NOT NULL DEFAULT '',
  threshold            REAL NOT NULL DEFAULT 0.35,
  total_segments       INTEGER NOT NULL DEFAULT 0,
  completed_segments   INTEGER NOT NULL DEFAULT 0,
  failed_segments      INTEGER NOT NULL DEFAULT 0,
  created_at           TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  updated_at           TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  completed_at         TEXT,
  error                TEXT
) STRICT;

CREATE TABLE IF NOT EXISTS diarization_segments (
  id               TEXT PRIMARY KEY NOT NULL,
  job_id           TEXT NOT NULL DEFAULT '',
  segment_index    INTEGER NOT NULL DEFAULT 0,
  speaker          INTEGER NOT NULL DEFAULT 0,
  global_start_ms  INTEGER NOT NULL DEFAULT 0,
  status           TEXT NOT NULL DEFAULT 'pending',
  retry_count      INTEGER NOT NULL DEFAULT 0,
  response_json    TEXT,
  created_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  updated_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) STRICT;

CREATE INDEX IF NOT EXISTS idx_dj_session ON diarization_jobs(session_id);
CREATE INDEX IF NOT EXISTS idx_ds_job ON diarization_segments(job_id);
