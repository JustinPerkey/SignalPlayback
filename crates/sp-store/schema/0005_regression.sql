-- 0005_regression.sql — assertions and baselines, the two halves of turning a
-- run into a test (docs/DESIGN.md §9.7, §10.4).
--
-- An assertion belongs to a pipeline and is evaluated per group after the last
-- stage; its outcome is recorded per (run, group) so a failure names the case
-- rather than the suite. A baseline is a run promoted to golden, with the
-- tolerances a later run is allowed to drift by.

CREATE TABLE pipeline_assertion (
    pipeline_id INTEGER NOT NULL REFERENCES pipeline(id) ON DELETE CASCADE,
    ordinal     INTEGER NOT NULL,
    -- The source text, e.g. `metrics.snr_db > 12.0`. Stored as written: it is
    -- what the editor shows and what a failure quotes, and re-parsing it is
    -- cheaper than keeping a parsed form in step with the grammar.
    expression  TEXT    NOT NULL,
    enabled     INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (pipeline_id, ordinal)
);

CREATE TABLE run_assertion (
    run_id     INTEGER NOT NULL REFERENCES run(id) ON DELETE CASCADE,
    group_id   INTEGER NOT NULL,
    ordinal    INTEGER NOT NULL,
    -- Copied from the pipeline at run time, so a later edit does not rewrite
    -- history: the run says what it actually tested.
    expression TEXT    NOT NULL,
    status     TEXT    NOT NULL
               CHECK (status IN ('pass','fail','not_applicable','error')),
    actual     REAL,                  -- the value the subject resolved to
    expected   REAL,                  -- what it was tested against, when numeric
    message    TEXT,                  -- why it failed, in the user's terms
    PRIMARY KEY (run_id, group_id, ordinal)
);

CREATE INDEX ix_baseline_run ON baseline(run_id);
