-- Schema version 3: the render pyramid index (docs/DESIGN.md §5.4).
--
-- A pyramid is derived data: a multi-resolution min/max reduction of one
-- column, stored as an ordinary blob with `kind = 'pyramid'`. What is missing
-- from version 2 is the way back to it. Keying by the *source blob's checksum*
-- rather than by its id is deliberate: blobs are content-addressed, so two
-- signals that hold the same samples share one blob and therefore one pyramid,
-- and a stage that passes a signal through unchanged inherits the pyramid its
-- input already had (§5.3).
--
-- Dropping every row here plus every `kind = 'pyramid'` blob is always safe;
-- that is what the `Rebuild pyramids` maintenance action does.

CREATE TABLE render_pyramid (
    source_checksum TEXT    PRIMARY KEY,   -- blake3 of the column this reduces
    blob_id         INTEGER NOT NULL REFERENCES sample_blob(id),
    -- Denormalised from the pyramid header so the renderer can pick a level
    -- without reading any bytes.
    level_count     INTEGER NOT NULL,
    base_shift      INTEGER NOT NULL,      -- level 0 covers 1 << base_shift values
    source_count    INTEGER NOT NULL,      -- values the pyramid reduces
    built_utc       TEXT    NOT NULL
);

CREATE INDEX ix_pyramid_blob ON render_pyramid(blob_id);
