-- Liveness beacons: one row per long-running process (e.g. 'watch'),
-- UPSERTed every tick. Readers derive alive/dead from row freshness.
CREATE TABLE IF NOT EXISTS heartbeats (
  name TEXT PRIMARY KEY,
  ts TEXT NOT NULL
);
