CREATE TABLE IF NOT EXISTS progressive_batch_jobs (
  id                   TEXT PRIMARY KEY NOT NULL,
  session_id           TEXT NOT NULL DEFAULT '',
  status               TEXT NOT NULL DEFAULT 'running',
  provider             TEXT NOT NULL DEFAULT '',
  model                TEXT NOT NULL DEFAULT '',
  base_url             TEXT NOT NULL DEFAULT '',
  language             TEXT NOT NULL DEFAULT '',
  segment_duration_ms  INTEGER NOT NULL DEFAULT 30000,
  overlap_ms           INTEGER NOT NULL DEFAULT 1000,
  max_concurrency      INTEGER NOT NULL DEFAULT 2,
  total_segments       INTEGER NOT NULL DEFAULT 0,
  completed_segments   INTEGER NOT NULL DEFAULT 0,
  failed_segments      INTEGER NOT NULL DEFAULT 0,
  abandoned_segments   INTEGER NOT NULL DEFAULT 0,
  created_at           TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  updated_at           TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  completed_at         TEXT,
  error                TEXT
) STRICT;

CREATE TABLE IF NOT EXISTS progressive_batch_segments (
  id               TEXT PRIMARY KEY NOT NULL,
  job_id           TEXT NOT NULL DEFAULT '',
  segment_index    INTEGER NOT NULL DEFAULT 0,
  global_start_ms  INTEGER NOT NULL DEFAULT 0,
  global_end_ms    INTEGER NOT NULL DEFAULT 0,
  status           TEXT NOT NULL DEFAULT 'pending',
  retry_count      INTEGER NOT NULL DEFAULT 0,
  max_retries      INTEGER NOT NULL DEFAULT 3,
  error            TEXT,
  response_json    TEXT,
  created_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  updated_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) STRICT;

CREATE INDEX IF NOT EXISTS idx_pbj_session ON progressive_batch_jobs(session_id);
CREATE INDEX IF NOT EXISTS idx_pbs_job ON progressive_batch_segments(job_id);
CREATE INDEX IF NOT EXISTS idx_pbs_status ON progressive_batch_segments(status);
