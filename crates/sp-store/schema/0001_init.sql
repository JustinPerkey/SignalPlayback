-- SignalPlayback library schema, version 1 (docs/DESIGN.md §5.2, §6.3, §6.6).
--
-- Applied inside one transaction by sp-store's migrator. Connection pragmas
-- (WAL, foreign keys, page size) are set by the opener, not here.

CREATE TABLE app_meta (
    key    TEXT PRIMARY KEY,
    value  TEXT NOT NULL
);

-- One import run, one generation batch, or one derived collection.
CREATE TABLE dataset (
    id            INTEGER PRIMARY KEY,
    name          TEXT    NOT NULL,
    source_kind   TEXT    NOT NULL
                  CHECK (source_kind IN ('csv_import','generated','derived')),
    source_uri    TEXT,
    profile_id    INTEGER REFERENCES import_profile(id),
    created_utc   TEXT    NOT NULL,
    notes         TEXT,
    attributes    TEXT    NOT NULL DEFAULT '{}'
);

-- A group block from the CSV, or a bundle of generated signals.
-- The group is also the unit of processing (§9).
CREATE TABLE signal_group (
    id              INTEGER PRIMARY KEY,
    dataset_id      INTEGER NOT NULL REFERENCES dataset(id) ON DELETE CASCADE,
    ordinal         INTEGER NOT NULL,
    name            TEXT,
    declared_count  INTEGER NOT NULL,
    actual_count    INTEGER NOT NULL,
    -- Pulse groups (§6.6): the TOA column, shared by every pulse_field.
    toa_blob_id     INTEGER REFERENCES sample_blob(id),
    toa_unit        TEXT,
    attributes      TEXT    NOT NULL DEFAULT '{}',
    UNIQUE (dataset_id, ordinal)
);

CREATE TABLE signal (
    id             INTEGER PRIMARY KEY,
    group_id       INTEGER NOT NULL REFERENCES signal_group(id) ON DELETE CASCADE,
    ordinal        INTEGER NOT NULL,
    name           TEXT    NOT NULL,
    units          TEXT,
    dtype          TEXT    NOT NULL
                   CHECK (dtype IN ('f32','f64','i16','i32','c64','u8')),
    domain         TEXT    NOT NULL DEFAULT 'analog'
                   CHECK (domain IN ('analog','digital_logic','baseband_iq',
                                     'symbols','bits')),
    provenance     TEXT    NOT NULL DEFAULT 'imported'
                   CHECK (provenance IN ('imported','generated','derived')),
    sample_rate_hz REAL,                   -- NULL ⇒ irregular, see time_blob_id
    t0_s           REAL    NOT NULL DEFAULT 0.0,
    sample_count   INTEGER NOT NULL,
    blob_id        INTEGER REFERENCES sample_blob(id),
    time_blob_id   INTEGER REFERENCES sample_blob(id),
    gen_spec       TEXT,
    min_value      REAL, max_value REAL, mean_value REAL, rms_value REAL,
    nan_count      INTEGER NOT NULL DEFAULT 0,
    attributes     TEXT    NOT NULL DEFAULT '{}',
    UNIQUE (group_id, ordinal)
);

-- An immutable, content-addressed byte array: a signal's samples, a pulse
-- field's column, a TOA column, a render pyramid, or a large artifact payload.
CREATE TABLE sample_blob (
    id         INTEGER PRIMARY KEY,
    checksum   TEXT    NOT NULL UNIQUE,     -- blake3 hex; this is the address
    byte_len   INTEGER NOT NULL,            -- total payload length across chunks
    chunk_size INTEGER NOT NULL,            -- bytes per chunk, last one may be short
    kind       TEXT    NOT NULL
               CHECK (kind IN ('samples','pyramid','artifact')),
    refcount   INTEGER NOT NULL DEFAULT 0
);

-- Blob payload, split so no single BLOB approaches SQLite's 1 GB ceiling and
-- so a bulk import checkpoints the WAL at a predictable rate. This table keeps
-- its rowid: incremental blob I/O (sqlite3_blob_open) addresses a cell by
-- rowid and cannot open one in a WITHOUT ROWID table.
CREATE TABLE sample_chunk (
    id      INTEGER PRIMARY KEY,
    blob_id INTEGER NOT NULL REFERENCES sample_blob(id) ON DELETE CASCADE,
    ordinal INTEGER NOT NULL,               -- 0-based chunk index
    data    BLOB    NOT NULL,
    UNIQUE (blob_id, ordinal)
);

CREATE TABLE tag (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
CREATE TABLE signal_tag (
    signal_id INTEGER NOT NULL REFERENCES signal(id) ON DELETE CASCADE,
    tag_id    INTEGER NOT NULL REFERENCES tag(id)    ON DELETE CASCADE,
    PRIMARY KEY (signal_id, tag_id)
);

CREATE TABLE import_profile (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    rules_json  TEXT NOT NULL,
    created_utc TEXT NOT NULL
);

CREATE TABLE playlist (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
CREATE TABLE playlist_item (
    playlist_id INTEGER NOT NULL REFERENCES playlist(id) ON DELETE CASCADE,
    signal_id   INTEGER NOT NULL REFERENCES signal(id)   ON DELETE CASCADE,
    ordinal     INTEGER NOT NULL,
    t_offset_s  REAL    NOT NULL DEFAULT 0.0,
    gain        REAL    NOT NULL DEFAULT 1.0,
    colour      TEXT,
    PRIMARY KEY (playlist_id, ordinal)
);

CREATE INDEX ix_signal_group  ON signal(group_id);
CREATE INDEX ix_group_dataset ON signal_group(dataset_id);
CREATE INDEX ix_signal_name   ON signal(name);

-- Full-text search over signal names, units and attribute JSON, kept in step
-- with the signal table by the triggers below (external-content FTS5).
CREATE VIRTUAL TABLE signal_fts USING fts5(
    name, units, attributes, content='signal', content_rowid='id'
);
CREATE TRIGGER signal_fts_ai AFTER INSERT ON signal BEGIN
    INSERT INTO signal_fts(rowid, name, units, attributes)
    VALUES (new.id, new.name, new.units, new.attributes);
END;
CREATE TRIGGER signal_fts_ad AFTER DELETE ON signal BEGIN
    INSERT INTO signal_fts(signal_fts, rowid, name, units, attributes)
    VALUES ('delete', old.id, old.name, old.units, old.attributes);
END;
CREATE TRIGGER signal_fts_au AFTER UPDATE ON signal BEGIN
    INSERT INTO signal_fts(signal_fts, rowid, name, units, attributes)
    VALUES ('delete', old.id, old.name, old.units, old.attributes);
    INSERT INTO signal_fts(rowid, name, units, attributes)
    VALUES (new.id, new.name, new.units, new.attributes);
END;

-- One numeric field of a group's pulse records, stored as a column (§6.6).
CREATE TABLE pulse_field (
    id          INTEGER PRIMARY KEY,
    group_id    INTEGER NOT NULL REFERENCES signal_group(id) ON DELETE CASCADE,
    ordinal     INTEGER NOT NULL,          -- column order in the source file
    name        TEXT    NOT NULL,          -- 'pulse width', as written in the header
    key         TEXT    NOT NULL,          -- 'pulse_width'; matches property_def.key when bound
    unit        TEXT,
    dtype       TEXT    NOT NULL,
    blob_id     INTEGER REFERENCES sample_blob(id),
    -- Zone map plus cached statistics: the prefilter for cross-group search.
    min_value   REAL, max_value REAL, mean_value REAL, rms_value REAL,
    nan_count   INTEGER NOT NULL DEFAULT 0,
    UNIQUE (group_id, ordinal)
);
CREATE INDEX ix_pulse_field_zone ON pulse_field(key, min_value, max_value);

-- Annotation for an individual pulse. Rows exist only for pulses the user
-- named or tagged; an unannotated pulse is addressed as (group_id, idx) alone.
CREATE TABLE pulse (
    id         INTEGER PRIMARY KEY,
    group_id   INTEGER NOT NULL REFERENCES signal_group(id) ON DELETE CASCADE,
    idx        INTEGER NOT NULL,
    name       TEXT,
    attributes TEXT NOT NULL DEFAULT '{}',
    UNIQUE (group_id, idx)
);
CREATE TABLE pulse_tag (
    pulse_id INTEGER NOT NULL REFERENCES pulse(id) ON DELETE CASCADE,
    tag_id   INTEGER NOT NULL REFERENCES tag(id)   ON DELETE CASCADE,
    PRIMARY KEY (pulse_id, tag_id)
);

-- User-definable properties (§6.3).
CREATE TABLE property_def (
    id           INTEGER PRIMARY KEY,
    key          TEXT NOT NULL,
    scope        TEXT NOT NULL CHECK (scope IN ('dataset','group','signal')),
    label        TEXT NOT NULL,
    kind_json    TEXT NOT NULL,
    unit         TEXT,
    default_json TEXT,
    required     INTEGER NOT NULL DEFAULT 0,
    section      TEXT,
    ordinal      INTEGER NOT NULL DEFAULT 0,
    UNIQUE (scope, key)
);

-- Named reusable bundles: 'Pulse-Doppler capture', 'BPSK link test'.
CREATE TABLE property_set (
    id    INTEGER PRIMARY KEY,
    name  TEXT NOT NULL UNIQUE,
    notes TEXT
);
CREATE TABLE property_set_member (
    set_id  INTEGER NOT NULL REFERENCES property_set(id) ON DELETE CASCADE,
    def_id  INTEGER NOT NULL REFERENCES property_def(id) ON DELETE CASCADE,
    PRIMARY KEY (set_id, def_id)
);

-- Indexed mirror of `signal.attributes`, maintained on write.
CREATE TABLE signal_property (
    signal_id INTEGER NOT NULL REFERENCES signal(id) ON DELETE CASCADE,
    key       TEXT    NOT NULL,
    num_value REAL,
    txt_value TEXT,
    PRIMARY KEY (signal_id, key)
);
CREATE INDEX ix_prop_key_num ON signal_property(key, num_value);
CREATE INDEX ix_prop_key_txt ON signal_property(key, txt_value);
