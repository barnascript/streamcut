-- Streamcut schema for ScyllaDB.
-- Each table is shaped around one query the API or worker runs, so every
-- read hits a single partition. No ALLOW FILTERING anywhere.
-- The API runs this file on start-up; every statement is safe to repeat.
-- The replication factor is filled in from SCYLLA_RF (1 locally, 3 in a cluster).

CREATE KEYSPACE IF NOT EXISTS streamcut
  WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': ${RF}};

-- "Show me job X": one row per job, updated as the worker moves along.
CREATE TABLE IF NOT EXISTS streamcut.jobs (
  job_id       uuid PRIMARY KEY,
  status       text,       -- queued | running | done | failed
  stage        text,       -- probe | audio | transcribe | analyse | render | done
  progress     int,        -- 0..100
  message      text,
  source_name  text,
  source_url   text,
  source_key   text,       -- path under DATA_DIR once the file is local
  settings     text,       -- JSON, see worker/src/pipeline.rs Settings
  duration_s   double,
  clip_count   int,
  words        int,
  attempts     int,
  worker       text,
  error        text,
  created_at   timestamp,
  updated_at   timestamp
);

-- "Show the latest jobs": partition by day, newest first inside the day.
-- Partitions stay small, and old days simply stop being read.
CREATE TABLE IF NOT EXISTS streamcut.jobs_by_day (
  day          date,
  created_at   timestamp,
  job_id       uuid,
  source_name  text,
  PRIMARY KEY ((day), created_at, job_id)
) WITH CLUSTERING ORDER BY (created_at DESC, job_id ASC);

-- "Show the clips for job X, best first": one partition per job.
CREATE TABLE IF NOT EXISTS streamcut.clips_by_job (
  job_id       uuid,
  rank         int,
  clip_id      uuid,
  start_s      double,
  end_s        double,
  score        float,
  title        text,
  reasons      frozen<list<text>>,
  transcript   text,
  video_key    text,
  thumb_key    text,
  width        int,
  height       int,
  size_bytes   bigint,
  PRIMARY KEY ((job_id), rank)
);

-- The per-second scores behind the chart on the job page. Frozen lists are
-- stored as one cell, which is far cheaper than 1,500 separate list cells.
CREATE TABLE IF NOT EXISTS streamcut.signals_by_job (
  job_id       uuid PRIMARY KEY,
  step_s       double,
  loudness     frozen<list<float>>,
  speech       frozen<list<float>>,
  hype         frozen<list<float>>,
  score        frozen<list<float>>
);
