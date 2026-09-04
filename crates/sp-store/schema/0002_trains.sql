-- Schema version 2: the signal train (docs/DESIGN.md §6.6).
--
-- Groups are not independent captures. One signal train resolves to several of
-- them — dwells, scans, blocks of a file — and those groups are one capture.
-- Version 1 hung a group directly off its dataset, which said the opposite, so
-- a train now sits between the two and owns the groups.
--
-- One imported file is one train, which is what the backfill below assumes:
-- every existing dataset gets a single train holding everything it already had.
--
-- `signal_group` is rebuilt rather than altered, because `dataset_id` carries a
-- UNIQUE constraint that ALTER TABLE cannot drop. `signal` and `pulse_field`
-- reference `signal_group(id)` and those ids are preserved, so their rows are
-- untouched; the migrator disables foreign keys around the rebuild the way
-- SQLite's own procedure prescribes, and checks them again afterwards.

CREATE TABLE signal_train (
    id          INTEGER PRIMARY KEY,
    dataset_id  INTEGER NOT NULL REFERENCES dataset(id) ON DELETE CASCADE,
    ordinal     INTEGER NOT NULL,
    name        TEXT,
    -- Source unit of the train's TOA columns, for a train of pulse records.
    toa_unit    TEXT,
    attributes  TEXT    NOT NULL DEFAULT '{}',
    UNIQUE (dataset_id, ordinal)
);

CREATE INDEX ix_train_dataset ON signal_train(dataset_id);

-- One train per existing dataset, named after it, taking the TOA unit its
-- groups already agreed on.
INSERT INTO signal_train (dataset_id, ordinal, name, toa_unit, attributes)
SELECT d.id,
       0,
       d.name,
       (SELECT g.toa_unit FROM signal_group g
         WHERE g.dataset_id = d.id AND g.toa_unit IS NOT NULL
         LIMIT 1),
       '{}'
FROM dataset d;

CREATE TABLE signal_group_v2 (
    id              INTEGER PRIMARY KEY,
    train_id        INTEGER NOT NULL REFERENCES signal_train(id) ON DELETE CASCADE,
    ordinal         INTEGER NOT NULL,
    name            TEXT,
    declared_count  INTEGER NOT NULL,
    actual_count    INTEGER NOT NULL,
    -- Pulse groups (§6.6): the TOA column, shared by every pulse_field.
    toa_blob_id     INTEGER REFERENCES sample_blob(id),
    toa_unit        TEXT,
    attributes      TEXT    NOT NULL DEFAULT '{}',
    UNIQUE (train_id, ordinal)
);

INSERT INTO signal_group_v2
    (id, train_id, ordinal, name, declared_count, actual_count,
     toa_blob_id, toa_unit, attributes)
SELECT g.id, t.id, g.ordinal, g.name, g.declared_count, g.actual_count,
       g.toa_blob_id, g.toa_unit, g.attributes
FROM signal_group g
JOIN signal_train t ON t.dataset_id = g.dataset_id;

DROP TABLE signal_group;
ALTER TABLE signal_group_v2 RENAME TO signal_group;

CREATE INDEX ix_group_train ON signal_group(train_id);
