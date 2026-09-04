-- 0004_runs.sql — pipelines, runs and everything a run records
-- (docs/DESIGN.md §9.6).
--
-- A run is the audit trail of a pipeline over a dataset: one row per group,
-- one per (group, stage), and the signals and artifacts each stage produced.
-- `stage_ordinal = -1` records the source signals as they entered, so "before"
-- is a real row rather than a special case in the UI.

CREATE TABLE pipeline (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL,
    notes       TEXT,
    created_utc TEXT NOT NULL
);

CREATE TABLE pipeline_stage (
    pipeline_id INTEGER NOT NULL REFERENCES pipeline(id) ON DELETE CASCADE,
    ordinal     INTEGER NOT NULL,
    stage_kind  TEXT    NOT NULL,      -- 'dsp.filter.biquad'
    label       TEXT,                  -- user's name for this instance
    params_json TEXT    NOT NULL DEFAULT '{}',
    enabled     INTEGER NOT NULL DEFAULT 1,
    retention   TEXT    NOT NULL DEFAULT 'always'
                CHECK (retention IN ('always','on_failure','never')),
    PRIMARY KEY (pipeline_id, ordinal)
);

CREATE TABLE run (
    id            INTEGER PRIMARY KEY,
    pipeline_id   INTEGER NOT NULL REFERENCES pipeline(id),
    dataset_id    INTEGER REFERENCES dataset(id),
    started_utc   TEXT NOT NULL,
    finished_utc  TEXT,
    status        TEXT NOT NULL
                  CHECK (status IN ('running','ok','failed','cancelled')),
    -- Kinds + versions + params, canonicalised: two runs with the same hash
    -- ran the same algorithm, whatever the pipeline has been edited into
    -- since.
    pipeline_hash TEXT NOT NULL,
    app_version   TEXT NOT NULL,
    notes         TEXT
);
CREATE INDEX ix_run_pipeline ON run(pipeline_id);

CREATE TABLE run_group (
    run_id   INTEGER NOT NULL REFERENCES run(id) ON DELETE CASCADE,
    group_id INTEGER NOT NULL REFERENCES signal_group(id) ON DELETE CASCADE,
    status   TEXT    NOT NULL
             CHECK (status IN ('running','ok','failed','cancelled')),
    wall_ms  INTEGER,
    message  TEXT,                     -- why a failed group failed
    PRIMARY KEY (run_id, group_id)
);

CREATE TABLE run_stage (
    run_id           INTEGER NOT NULL REFERENCES run(id) ON DELETE CASCADE,
    group_id         INTEGER NOT NULL,
    stage_ordinal    INTEGER NOT NULL,
    status           TEXT    NOT NULL
                     CHECK (status IN ('ok','failed','skipped','cached')),
    wall_ms          INTEGER,
    cache_key        TEXT    NOT NULL,
    metrics_json     TEXT    NOT NULL DEFAULT '{}',
    diagnostics_json TEXT    NOT NULL DEFAULT '[]',
    message          TEXT,              -- the stage error, when it failed
    PRIMARY KEY (run_id, group_id, stage_ordinal)
);

-- Signals as they existed at the OUTPUT of a given stage.
CREATE TABLE run_signal (
    run_id         INTEGER NOT NULL REFERENCES run(id) ON DELETE CASCADE,
    group_id       INTEGER NOT NULL,
    stage_ordinal  INTEGER NOT NULL,    -- -1 = the untouched source
    signal_ordinal INTEGER NOT NULL,
    name           TEXT    NOT NULL,
    domain         TEXT    NOT NULL
                   CHECK (domain IN ('analog','digital_logic','baseband_iq',
                                     'symbols','bits')),
    disposition    TEXT    NOT NULL
                   CHECK (disposition IN ('replaced','added','passthrough','dropped')),
    blob_id        INTEGER REFERENCES sample_blob(id),
    sample_rate_hz REAL, t0_s REAL, sample_count INTEGER,
    min_value      REAL, max_value REAL, mean_value REAL, rms_value REAL,
    attrs_json     TEXT NOT NULL DEFAULT '{}',
    PRIMARY KEY (run_id, group_id, stage_ordinal, signal_ordinal)
);

CREATE TABLE artifact (
    id            INTEGER PRIMARY KEY,
    run_id        INTEGER NOT NULL REFERENCES run(id) ON DELETE CASCADE,
    group_id      INTEGER,              -- NULL ⇒ run-level (from end_run)
    stage_ordinal INTEGER NOT NULL,
    port          TEXT    NOT NULL,
    kind          TEXT    NOT NULL,     -- 'detections.v1'
    kind_version  INTEGER NOT NULL,
    payload_json  TEXT,                 -- small payloads inline
    blob_id       INTEGER REFERENCES sample_blob(id),   -- large ones out of line
    summary       TEXT                  -- '4 detections, best score 0.91'
);
CREATE INDEX ix_artifact_lookup ON artifact(run_id, group_id, stage_ordinal);

-- Which recorded output a cache key resolves to (§9.5). Hanging it off the run
-- means a deleted run takes its cache entries with it, so a key never points
-- at rows that are gone.
CREATE TABLE stage_cache (
    cache_key     TEXT    PRIMARY KEY,
    run_id        INTEGER NOT NULL REFERENCES run(id) ON DELETE CASCADE,
    group_id      INTEGER NOT NULL,
    stage_ordinal INTEGER NOT NULL,
    created_utc   TEXT    NOT NULL
);
CREATE INDEX ix_stage_cache_run ON stage_cache(run_id);

-- A run promoted to golden, for regression comparison (M7).
CREATE TABLE baseline (
    id             INTEGER PRIMARY KEY,
    name           TEXT NOT NULL UNIQUE,
    run_id         INTEGER NOT NULL REFERENCES run(id),
    created_utc    TEXT NOT NULL,
    tolerance_json TEXT NOT NULL DEFAULT '{}'
);

-- A derived signal names the run and stage that produced it (§6.1). The
-- `provenance` token said 'derived' from schema 1 onwards; these columns are
-- what make it traceable.
ALTER TABLE signal ADD COLUMN derived_run_id INTEGER REFERENCES run(id);
ALTER TABLE signal ADD COLUMN derived_stage_ordinal INTEGER;
