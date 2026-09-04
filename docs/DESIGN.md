# SignalPlayback — Design Document

**Status:** Draft v0.3
**Date:** 2026-09-04
**Author:** Justin Perkey
**Repository:** `d:\Repos\SignalPlayback`

> **Changes in v0.3** — `sample/sample.csv` settled the input format, which carries **pulse
> records** rather than sampled waveforms. §7 rewritten to the real grammar (fixed preamble,
> file-level headers, count-driven framing, TOA in µs), §6.6 added for the pulse record
> model and cross-group search, §5.2 extended with `pulse_field` / `pulse`, and §17.1–17.4
> closed.
>
> **Changes in v0.2** — Added the processing pipeline (§9), the artifact/result model and
> stage inspection (§10), and customizable signal properties (§6). Sections renumbered.

---

## 1. Purpose

SignalPlayback is a desktop workbench for building a library of time-domain signals and
running signal-processing algorithms over them. It gives an engineer one place to:

1. **Import** recorded data from the project's CSV format — groups of pulse records, each
   with a time of arrival and numeric fields, plus per-group metadata (§7).
2. **Generate** synthetic signals from parameters (waveform type, frequency, amplitude,
   noise, modulation, composition).
3. **Persist** everything in a durable, queryable local signal database with a
   user-definable property schema.
4. **Process** signals a group at a time through an ordered pipeline of algorithm stages.
5. **Inspect** the result of *every* stage — altered signals, or new typed outputs such as
   spectra, detections, symbols and metrics — on a shared timeline.
6. **Play back** any of it on a time-synchronised on-screen scope with transport controls.

Playback output is **on-screen visualization only**. Hardware, network, and audio sinks
are out of scope for v1, but the engine is structured so they can be added later as
additional sinks.

The processing side is a **test harness**, not just a viewer: runs are recorded,
reproducible, comparable against a baseline, and can assert pass/fail on metrics.

---

## 2. Goals and Non-Goals

### 2.1 Goals

| # | Goal |
|---|------|
| G1 | Round-trip the project's CSV format losslessly: import → database → export produces an equivalent file. |
| G2 | Handle libraries of at least 10 000 signals and individual signals of at least 100 M samples without the UI dropping below 60 fps. The same budget covers a group of 10 M pulse records. |
| G3 | Generated signals are **reproducible**: the parameter spec plus a stored seed regenerates bit-identical samples. |
| G4 | Single-file distribution — one executable plus a self-contained library directory. No server, no external database process. |
| G5 | Every long-running operation (import, generation, processing, export) is cancellable and reports progress without blocking the UI. |
| G6 | The database is inspectable and repairable with standard tooling (`sqlite3`, a hex editor). |
| G7 | **Every stage's output is inspectable.** After a run, the user can select any (group, stage) pair and see exactly what that stage produced, with the stage before it available for comparison. |
| G8 | **Runs are reproducible and comparable.** A run records the pipeline, stage versions, parameters and input hashes; re-running the same inputs produces identical results, and any two runs can be diffed. |
| G9 | **A new algorithm is cheap to add.** Implementing one trait plus a parameter descriptor is enough to make a stage appear in the pipeline editor with a generated parameter form. |
| G10 | **Pulses are findable across the library.** A numeric predicate over pulse fields returns matching `(group, index)` records from every group in under a second at library scale, without materialising a row per pulse. |

### 2.2 Non-Goals (v1)

- Real-time output to DAC / SDR / DAQ hardware.
- Network streaming of samples to other processes.
- Audio playback.
- Multi-user access, syncing, or a client/server split.
- Real-time (deadline-bound) processing. The pipeline is batch: correctness and
  inspectability first, throughput second.
- A general-purpose DSP library. `sp-dsp` ships the stages needed to exercise the harness;
  the user's own algorithms are the point.

---

## 3. Technology Choices

| Layer | Choice | Rationale |
|-------|--------|-----------|
| Language | Rust (2021 edition, stable toolchain) | Repo is already Cargo-configured. Memory safety plus the throughput needed for large sample arrays. |
| GUI | **Iced** (`iced` 0.13.x, `wgpu` backend) | Retained-mode Elm architecture: a single `Message` enum and pure `update` make the transport and pipeline state machines easy to reason about and test. Pure Rust, no JS toolchain. Custom `canvas::Program` gives full control of the scope renderer. |
| Metadata store | **SQLite** via `rusqlite` (bundled feature, WAL mode) | Ad-hoc SQL over groups/signals/properties/runs, transactional integrity, single-file backup, universally inspectable. |
| Sample store | **File-backed binary blobs** (`.sigbin`) with mmap reads | Keeps multi-GB sample arrays out of the SQLite page cache. Zero-copy slice reads for the renderer. Content addressing makes pipeline passthrough free. |
| CSV | `csv` crate over a custom block framer | Handles quoting/escaping correctly; the framer above it enforces the group/signal block grammar. |
| Serialization | `serde` + `serde_json` | Generator specs, stage parameters, artifact payloads and property values. |
| Hashing | `blake3` | Content addressing, integrity checks, and pipeline cache keys. |
| RNG | `rand` + `rand_chacha` | `ChaCha12Rng` is portable and deterministic across platforms and versions — required by G3. |
| DSP | `rustfft`, `realfft` | FFT stages and the spectrogram artifact. Filters are hand-rolled (biquad/FIR) to keep dependencies thin. |
| Parallelism | `rayon` (CPU work), `tokio` (task orchestration/IO) | Generation, decimation and cross-group pipeline execution parallelise cleanly; Iced `Task`s need an async executor. |
| Logging | `tracing` + `tracing-subscriber` | Structured spans around import/generate/run for diagnosing slow files and slow stages. |
| Dialogs | `rfd` | Native file pickers on Windows/macOS/Linux. |
| Paths | `directories` | Platform-correct default library location. |

### 3.1 Rejected Alternatives

- **egui** — better plotting out of the box (`egui_plot`), but immediate-mode redraw makes
  the transport/pipeline state model messier, and per-frame re-tessellation of 100 M-point
  traces is harder to cache than Iced's explicit `canvas::Cache`.
- **Tauri** — richest charting ecosystem, but the IPC boundary would force sample data
  through serialization on every viewport change and every stage inspection.
- **Pure-SQLite BLOB storage** — simplest, but chunked BLOB reads add a copy per access,
  bloat the WAL during import, and lose the content-addressed dedup that makes storing
  every stage's intermediate signals affordable.
- **DuckDB/Parquet-first** — excellent for cross-signal analytics, but heavier and awkward
  as live application state.
- **A dataflow graph engine (petgraph-based DAG with arbitrary topology)** — considered for
  the pipeline. Rejected for v1: a linear stage list with a typed side-channel (§9.3)
  covers the stated need, is far easier to inspect step-by-step, and does not force a
  graph editor into the UI. The stage trait is graph-ready if that changes.

---

## 4. Architecture

### 4.1 Crate Layout

A Cargo workspace, so the domain logic is testable without a window:

```
SignalPlayback/
├── Cargo.toml                 # [workspace]
├── docs/
│   └── DESIGN.md
└── crates/
    ├── sp-core/               # Domain vocabulary. No IO, no UI.
    │   ├── signal.rs          #   Signal, SignalId, SampleBuffer, DType, Domain
    │   ├── group.rs           #   SignalGroup, Dataset, GroupFrame
    │   ├── time.rs            #   TimeRange, SampleIndex, Timebase
    │   ├── props.rs           #   PropertyDef, PropertyValue, PropertySet
    │   ├── pulse.rs           #   PulseRef, PulseField, pulse-record vocabulary
    │   ├── artifact.rs        #   Artifact trait, ArtifactSchema, ViewHint
    │   └── stats.rs           #   min/max/mean/rms summarisation
    ├── sp-store/              # Persistence: SQLite schema, migrations, blob store.
    │   ├── schema/            #   NNNN_name.sql migration files (embedded)
    │   ├── db.rs              #   Connection management, writer actor
    │   ├── blob.rs            #   .sigbin read/write, mmap, checksums
    │   ├── runs.rs            #   Run/stage/artifact recording and retrieval
    │   ├── pulses.rs          #   Column read/scan, zone-map prefilter, pulse search
    │   └── query.rs           #   Typed query API (no SQL escapes this crate)
    ├── sp-csv/                # Block-grammar parser/writer + import profiles.
    │   ├── framer.rs          #   Count-driven block framing
    │   ├── parse.rs           #   Header/row decoding, error positions
    │   ├── profile.rs         #   Preamble/delimiter/count/time settings, column mapping
    │   └── export.rs          #   Database → CSV
    ├── sp-gen/                # Parametric synthesis.
    │   ├── spec.rs            #   GenSpec DAG (serde)
    │   ├── waveform.rs        #   Primitive oscillators
    │   ├── noise.rs           #   Gaussian / uniform / pink / PRBS
    │   ├── modulate.rs        #   AM/FM/PM, chirps, envelopes
    │   └── render.rs          #   Spec → SampleBuffer
    ├── sp-proc/               # Pipeline orchestration. Knows nothing about DSP.
    │   ├── stage.rs           #   Stage trait, StageDescriptor, ports, params
    │   ├── registry.rs        #   StageRegistry: kind → constructor + descriptor
    │   ├── pipeline.rs        #   Pipeline model, port validation
    │   ├── scheduler.rs       #   Group-at-a-time execution, cancellation, progress
    │   ├── cache.rs           #   Content-hash keyed stage-output reuse
    │   └── compare.rs         #   Run-vs-baseline diffing, assertions
    ├── sp-dsp/                # Built-in Stage implementations. Depends on sp-proc.
    │   ├── condition.rs       #   Detrend, DC block, normalise, resample, window
    │   ├── filter.rs          #   FIR/IIR low/high/band/notch
    │   ├── transform.rs       #   FFT, spectrogram, Hilbert/envelope
    │   ├── digital.rs         #   Threshold/slice, clock recovery, symbol decode
    │   └── detect.rs          #   Peak/edge/pulse detection → Detections artifact
    ├── sp-engine/             # Playback clock, scheduler, render pyramids.
    │   ├── transport.rs       #   State machine: Stopped/Playing/Paused
    │   ├── clock.rs           #   Monotonic virtual time, rate scaling
    │   ├── pyramid.rs         #   Multi-resolution min/max mipmaps
    │   └── viewport.rs        #   Time/amplitude window → draw commands
    └── sp-app/                # Iced binary. The only crate that knows about pixels.
        ├── main.rs
        ├── state.rs           #   App state, Message enum
        ├── screens/           #   library / import / generate / pipeline / results / scope
        ├── widgets/scope.rs   #   canvas::Program implementation
        ├── widgets/stagerail.rs #  Stage breadcrumb + compare pinning
        └── views/             #   ViewHint → concrete artifact viewer widgets
```

**Dependency rule:** dependencies point left-to-right only. `sp-core` depends on nothing
in the workspace; `sp-dsp` depends on `sp-proc` and `sp-core` only; `sp-app` may depend on
everything; no crate depends on `sp-app`.

**Why `sp-proc` and `sp-dsp` are separate.** The orchestration layer must not know what a
Butterworth filter is. Keeping them apart is what makes G9 true — and it is the seam a
plugin interface would later slot into.

### 4.2 Process and Threading Model

```
┌──────────────────────────────────────────────────────────────┐
│ UI thread — Iced runtime                                     │
│   update(Message) → State                                    │
│   view(State) → Element                                      │
│   scope + artifact views draw from already-decimated snapshots│
└───────┬──────────────────────────────▲───────────────────────┘
        │ Task::perform / commands     │ Messages (progress, stage done, error)
        ▼                              │
┌──────────────────────────────────────┴───────────────────────┐
│ Tokio worker pool                                            │
│   • CSV import   (streaming, chunked, cancellable)           │
│   • Generation   (rayon fan-out over sample ranges)          │
│   • Pipeline run (rayon fan-out over GROUPS; stages in order)│
│   • Pyramid build, export, statistics                        │
└───────┬──────────────────────────────▲───────────────────────┘
        │ DbCommand (mpsc)             │ oneshot replies
        ▼                              │
┌──────────────────────────────────────┴───────────────────────┐
│ Store actor thread — single SQLite writer connection (WAL)   │
│ Read-only connection pool (N = cores) for queries            │
└──────────────────────────────────────────────────────────────┘
```

- **Single writer, many readers.** WAL mode plus one dedicated writer thread eliminates
  `SQLITE_BUSY` retry logic entirely.
- **The UI never touches raw samples.** The engine hands the canvas a `ViewportSnapshot`
  already reduced to roughly one min/max pair per horizontal pixel.
- **Groups are the unit of parallelism.** Stages within a group run strictly in order;
  different groups run concurrently, bounded to `min(cores, in_flight_cap)` live frames so
  memory stays predictable.
- **Cancellation** via `CancellationToken` checked at chunk boundaries (every 64 K samples,
  1 000 CSV rows, or between stages), so cancel latency stays under ~50 ms.

---

## 5. Data Model

### 5.1 Library Layout on Disk

```
<library-root>/
├── library.db            # SQLite: all metadata, runs, artifacts
├── library.db-wal
├── samples/
│   └── a3/a3f9c1…e2.sigbin   # content-addressed by blake3, 2-char fanout
├── pyramids/
│   └── a3/a3f9c1…e2.sigmip   # derived, safe to delete and rebuild
├── artifacts/
│   └── 7c/7c11ab…04.artbin   # large artifact payloads (spectrogram matrices, etc.)
└── imports/
    └── 2026-09-04T12-00-00_capture.csv   # optional archived original
```

Default root: `%LOCALAPPDATA%\SignalPlayback\library` on Windows, via `directories`.
The root is user-selectable; multiple libraries are supported (one open at a time in v1).

### 5.2 Core Schema

```sql
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE app_meta (
    key    TEXT PRIMARY KEY,
    value  TEXT NOT NULL
);  -- seeded with ('schema_version','1')

-- One import run, one generation batch, or one derived collection.
CREATE TABLE dataset (
    id            INTEGER PRIMARY KEY,
    name          TEXT    NOT NULL,
    source_kind   TEXT    NOT NULL
                  CHECK (source_kind IN ('csv_import','generated','derived')),
    source_uri    TEXT,
    profile_id    INTEGER REFERENCES import_profile(id),
    created_utc   TEXT    NOT NULL,
    notes         TEXT
);

-- A group block from the CSV, or a bundle of generated signals.
-- The group is also the unit of processing (§9).
CREATE TABLE signal_group (
    id              INTEGER PRIMARY KEY,
    dataset_id      INTEGER NOT NULL REFERENCES dataset(id) ON DELETE CASCADE,
    ordinal         INTEGER NOT NULL,
    name            TEXT,
    declared_count  INTEGER NOT NULL,      -- the 'count' field from the group header
    actual_count    INTEGER NOT NULL,      -- pulse rows actually read
    -- Pulse groups (§6.6): the TOA column, shared by every pulse_field.
    toa_blob_id     INTEGER REFERENCES sample_blob(id),
    toa_unit        TEXT,                  -- source unit, e.g. 'us'; NULL for sampled groups
    attributes      TEXT    NOT NULL DEFAULT '{}',   -- JSON property values
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
    gen_spec       TEXT,                   -- JSON GenSpec when generated
    min_value      REAL, max_value REAL, mean_value REAL, rms_value REAL,
    attributes     TEXT    NOT NULL DEFAULT '{}',    -- JSON property values
    UNIQUE (group_id, ordinal)
);

CREATE TABLE sample_blob (
    id        INTEGER PRIMARY KEY,
    relpath   TEXT    NOT NULL UNIQUE,
    byte_len  INTEGER NOT NULL,
    checksum  TEXT    NOT NULL,            -- blake3 hex
    refcount  INTEGER NOT NULL DEFAULT 0   -- identical signals share one blob
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

CREATE VIRTUAL TABLE signal_fts USING fts5(
    name, units, attributes, content='signal', content_rowid='id'
);

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
    idx        INTEGER NOT NULL,           -- row position within the group
    name       TEXT,
    attributes TEXT NOT NULL DEFAULT '{}',
    UNIQUE (group_id, idx)
);
CREATE TABLE pulse_tag (
    pulse_id INTEGER NOT NULL REFERENCES pulse(id) ON DELETE CASCADE,
    tag_id   INTEGER NOT NULL REFERENCES tag(id)   ON DELETE CASCADE,
    PRIMARY KEY (pulse_id, tag_id)
);
```

Processing-related tables are in §9.6.

**Migrations.** Numbered SQL files (`0001_init.sql`, `0002_…`) embedded in the binary and
applied in order inside a transaction, gated on `app_meta.schema_version`. Downgrades are
refused with a clear message rather than attempted.

### 5.3 Sample Blob Format (`.sigbin`)

Fixed 64-byte header, then packed little-endian samples:

| Offset | Size | Field |
|--------|------|-------|
| 0  | 4 | Magic `SGB1` |
| 4  | 2 | Format version (u16) |
| 6  | 1 | DType (0=f32, 1=f64, 2=i16, 3=i32, 4=complex64, 5=u8) |
| 7  | 1 | Channel count (u8), 1 for v1 |
| 8  | 8 | Sample rate Hz (f64, 0.0 ⇒ irregular) |
| 16 | 8 | t0 seconds (f64) |
| 24 | 8 | Sample count (u64) |
| 32 | 8 | Scale factor (f64, for integer dtypes) |
| 40 | 8 | Offset (f64, for integer dtypes) |
| 48 | 16 | Reserved (zeroed) |
| 64 | … | Sample payload |

Blobs are immutable and content-addressed. Editing a signal writes a new blob and
decrements the old blob's `refcount`; a vacuum pass removes blobs at refcount 0.

**This is what makes storing every intermediate stage affordable.** A stage that passes a
signal through unchanged produces the same content hash and therefore the same blob — the
run records a reference, not a copy. Only signals a stage actually altered cost storage.

### 5.4 Render Pyramid (`.sigmip`)

Rendering 100 M points per frame is not feasible, so each blob gets a derived
multi-resolution min/max pyramid:

- Level *k* stores one `(min, max)` `f32` pair per `2^(k+6)` source samples — level 0 is a
  64:1 reduction, each level halves again.
- Levels are built once, lazily, on the worker pool (~200 ms per 100 M samples, a single
  linear pass) and cached to disk.
- The renderer picks the level where **samples-per-pixel lands in [1, 2]**, reads that
  slice via mmap, and draws vertical min/max bars.
- Below one sample per pixel the renderer reads the raw blob and draws a polyline with
  point markers.

Draw cost is proportional to viewport width in pixels, not to signal length — the key to
G2, and the reason flipping between stage outputs stays instant.

---

## 6. Signal Identity and Customizable Properties

Signals are not uniform: one may be a raw sine sweep, the next a preprocessed digital
bitstream with a symbol rate and a coding scheme. The model carries this in three parts —
a fixed core, a declared domain, and a user-definable property schema.

### 6.1 Fixed Core

Every signal has: `name`, `dtype`, `sample_rate_hz` (or explicit timestamps), `t0_s`,
`sample_count`, and cached statistics. These are structural — the renderer, the pyramid
builder and the timeline depend on them, so they are columns, not properties.

### 6.2 Domain and Provenance

```rust
pub enum Domain {
    Analog,        // continuous-valued real; drawn as a trace
    DigitalLogic,  // two- or multi-level; drawn as logic lanes with transitions
    BasebandIq,    // complex; drawn as I/Q traces, magnitude, or a constellation
    Symbols,       // discrete decisions at symbol instants; drawn as stems + labels
    Bits,          // packed bitstream; drawn as a bit ribbon
}

pub enum Provenance {
    Imported,
    Generated,
    Derived { run_id: RunId, stage_ordinal: u16 },
}
```

`Domain` does real work. It selects the default renderer, it gates which pipeline stages
will accept a signal (§9.3 port typing), and it drives sensible defaults — a
`DigitalLogic` signal gets a threshold property and a discrete y-axis rather than an
autoscaled one. `Provenance` is what makes a derived signal traceable back to the run and
stage that produced it.

### 6.3 Property Definitions

Anything beyond the fixed core is a **property**, declared once and then typed, validated,
displayed and queried consistently.

```rust
pub struct PropertyDef {
    pub key:        String,        // 'prf_hz', snake_case, unique within scope
    pub label:      String,        // 'PRF'
    pub scope:      PropScope,     // Dataset | Group | Signal
    pub kind:       PropKind,
    pub unit:       Option<String>,// 'Hz', 'dB', 's'
    pub default:    Option<Value>,
    pub required:   bool,
    pub section:    Option<String>,// groups fields in the editor: 'RF', 'Timing'
    pub ordinal:    i32,
}

pub enum PropKind {
    Float  { min: Option<f64>, max: Option<f64>, step: Option<f64> },
    Int    { min: Option<i64>, max: Option<i64> },
    Bool,
    Text   { pattern: Option<String>, max_len: Option<usize> },
    Enum   { variants: Vec<String> },
    FreqHz { min: Option<f64>, max: Option<f64> },   // rendered with unit scaling
    DurationS,
    TimeUtc,
    Ratio  { as_db: bool },
    SignalRef,                                       // points at another signal
}
```

Properties live as JSON in `attributes` for round-trip fidelity, and are *mirrored* into an
indexed EAV table so they are queryable:

```sql
CREATE TABLE property_def (
    id        INTEGER PRIMARY KEY,
    key       TEXT NOT NULL,
    scope     TEXT NOT NULL CHECK (scope IN ('dataset','group','signal')),
    label     TEXT NOT NULL,
    kind_json TEXT NOT NULL,
    unit      TEXT,
    default_json TEXT,
    required  INTEGER NOT NULL DEFAULT 0,
    section   TEXT,
    ordinal   INTEGER NOT NULL DEFAULT 0,
    UNIQUE (scope, key)
);

-- Named reusable bundles: 'Pulse-Doppler capture', 'BPSK link test'.
CREATE TABLE property_set (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
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
```

This is what lets the library answer *"every `baseband_iq` signal with `symbol_rate_hz`
above 1 M that has not been run through pipeline 3"* without a table scan.

### 6.4 Where Properties Come From

- **CSV import.** The mapping UI offers each unmapped column a choice: bind to an existing
  property definition, create a new one (with an inferred `PropKind` from the sampled
  values), or keep it as an untyped attribute. Saved in the import profile, so the second
  file of the same shape is one click.
- **Generation.** A sweep writes the swept parameter as a property, so the generated group
  is immediately filterable by it.
- **Processing.** A stage may emit property patches — e.g. an estimator writes back
  `estimated_snr_db`. These are namespaced by stage to avoid collision with measured values.
- **Manual.** The Inspector edits any property, with validation from its `PropKind`.

### 6.5 Unknown Properties Are Never Lost

An attribute with no matching definition is retained verbatim in `attributes` and shown in
the Inspector under "Unrecognised", with a one-click "promote to property" action. This is
what keeps G1 true across schema evolution.

### 6.6 Pulse Records

The project's CSV format (§7) carries **pulse records** rather than sampled waveforms: a
group is a collection of pulses, and a pulse is one record — a time of arrival plus a fixed
set of numeric fields (`pulse width`, `power`, `angle`, …). Groups routinely hold millions
of them. Pulses are never processed across groups, but *searching* for pulses across groups
is a first-class operation.

Two views of the same bytes serve those two needs.

**Stored as columns.** Each field of a group becomes one array — a `pulse_field` backed by
an ordinary content-addressed blob — and the group's TOA column becomes the shared
irregular timebase every field is indexed against. A field array is a signal in every way
that matters: it has statistics, a render pyramid, a scope trace, and it flows into a
`GroupFrame` exactly like a sampled signal, so no stage, viewer or storage path needs a
second code path. Ingest costs one sequential write per field.

**Addressed as records.** A pulse is `PulseRef { group: GroupId, index: u32 }`, where
`index` is its row position in the source file. The identity is free — no row, no id
allocation — and it is stable across re-import because it is the file's own ordering. The
pulse table view reads across the field arrays at one index; the scope draws the same data
along the timebase.

```
group 7   toa[]  = [10 µs, 20 µs, 30 µs, …]      ← shared irregular timebase
          pulse_field "pulse width"  [100, 100, …]
          pulse_field "power"        [100, 100, …]
          pulse_field "angle"        [100, 100, …]
             ▲
             └── pulse (7, 1) is the vertical slice: toa=20 µs, pw=100, power=100, angle=100
```

**Naming and annotation.** A `pulse` row is created lazily, only for a pulse the user names
or tags, keyed by `(group_id, index)`. A file with 10 M pulses of which 40 are interesting
costs 40 rows, not 10 M.

**Cross-group search.** Each `pulse_field` records min/max — a zone map — so a predicate
such as `pulse_width < 2 AND angle BETWEEN 30 AND 40` first eliminates whole groups in
SQL, then scans only the surviving groups' columns through mmap, in parallel, returning
`PulseRef` hits. A 4-byte-per-value linear scan runs at memory bandwidth, so a selective
query over a 10 000-group library touches a few hundred MB at most.

**Why not one `signal` row per pulse.** It is the obvious model and it does not scale here:
10 M pulses would mean 10 M `signal` rows plus ~40 M `signal_property` rows — hundreds of
times the storage of the 160 MB of numbers involved, and minutes of insert time per file.
The column-plus-zone-map arrangement answers the same queries with the storage the data
actually warrants. Should per-pulse SQL identity later prove necessary, a stage can promote
a selected result set into `pulse` rows without changing how anything is stored.

---

## 7. CSV Import Format

### 7.1 Structure

Confirmed against `sample/sample.csv`. The file carries **pulse records**, not sampled
waveforms: a fixed preamble, then two header rows *once*, then group rows each followed by
that group's pulse rows. There is no blank-line separator — framing is driven entirely by
the group row's count column.

```
<preamble>              ← fixed number of lines to skip (default 1)
<group header row>      ← column names for group metadata; must include a count column
<pulse header row>      ← column names for the per-pulse rows; must include a time column
[ <group row>           ← one row of values matching the group header
  <pulse row> × count ] ← 'count' rows, each matching the pulse header
  × groups
```

```
   preamble ───────────► Skip Row
   headers (once) ─────► groupID, total time,  count,  info
                         time,  pulse width,  power,  angle
                       ┌─
   group 1 ──────────► │ 1,  1000,  2,  info
                       │ 10,  100,  100,  100
                       │ 20,  100,  100,  100
                       └─
                       ┌─  ← no separator; count said the group ended
   group 2 ──────────► │ 2,  1000,  2,  info
                       │ 30,  100,  100,  100
                       │ 40,  100,  100,  100
                       └─
```

### 7.2 Grammar

```ebnf
file          = preamble , group_header , pulse_header , group_block { group_block } ;
preamble      = line * P ;                        (* P from the profile, default 1 *)
group_block   = group_row , pulse_row * N ;       (* N = the group row's count field *)
group_header  = field , { SEP , field } , EOL ;   (* must contain a count column *)
pulse_header  = field , { SEP , field } , EOL ;   (* must contain a time column *)
group_row     = field , { SEP , field } , EOL ;   (* arity = |group_header| *)
pulse_row     = field , { SEP , field } , EOL ;   (* arity = |pulse_header| *)
field         = quoted | bare ;
quoted        = '"' , { CHAR | '""' } , '"' ;
```

The framer is therefore a two-state machine — *expect group row* / *consume N pulse rows*
— and never has to look ahead. That is what keeps ingest streaming at full IO speed over a
file with millions of rows per group.

### 7.3 Parser Behaviour

| Concern | Rule |
|---------|------|
| **Preamble** | A fixed line count skipped before the headers, from the profile (default 1). Skipped lines are retained verbatim so export reproduces them (G1). |
| **Headers** | Both header rows appear once, at file level, and apply to every group. A row matching the group header's text mid-file is data, not a header — the framer never re-reads headers. |
| **Count column** | Located by case-insensitive name match against a candidate list: `count`, `pulse_count`, `num_pulses`, `n`, `nrec`. Overridable per import profile. |
| **Count semantics** | The number of pulse rows following the group row. This is the sole block-termination rule. |
| **Count mismatch** | *Strict* mode: error, abort the import. Detected when a consumed row's arity or numeric shape does not match the pulse header, or when EOF arrives early. *Tolerant* mode (default): record `actual_count` alongside `declared_count`, resynchronise on the next row that parses as a group row, and raise a warning naming the line. |
| **Field whitespace** | Leading and trailing whitespace is trimmed from every field before parsing — the format writes `, ` as its delimiter run. |
| **Time column** | Located by name (`time`, `toa`, `time_of_arrival`). Its unit comes from the profile, defaulting to **microseconds**, and is scaled to seconds for the absolute timeline on ingest. The original unit is recorded so export restores it exactly. |
| **Pulse fields** | Every non-time column of the pulse header becomes one field array per group (§6.6). Files are numeric throughout; a non-numeric column is reported as a diagnostic and kept as a text field array rather than aborting the import. |
| **Group fields** | Every group-header column other than the count becomes a group property. Text is expected here (`info` in the sample) and is stored as a group attribute. |
| **Delimiter** | Auto-detected from the group header (`,` `;` `\t` `\|`), overridable. |
| **Encoding** | UTF-8, with UTF-8/UTF-16 BOM stripped. Invalid sequences are reported with byte offsets, not silently replaced. |
| **Line endings** | `LF` and `CRLF` both accepted; a lone `CR` is treated as a line ending with a warning. |
| **Comments** | Lines starting with `#` are skipped everywhere except inside quoted fields. Configurable. |
| **Missing values** | Empty, `NaN`, `nan`, `NA`, `null` → `f64::NAN`, drawn as a gap and excluded from statistics. |
| **Numeric parsing** | Decimal and scientific notation. Thousands separators rejected. Locale-independent (always `.` as decimal point). |
| **Errors** | Every diagnostic carries `(group_index, line_number, byte_offset, column_index, message)` and surfaces in a scrollable error panel with a jump-to-line action. |

### 7.4 Import Pipeline

```
 pick file → sniff (preamble + headers + first group) → preview & column mapping UI
     → confirm → streaming ingest → per-field column blob write → zone map + stats
     → dataset appears in library
```

The **sniff** pass reads only the preamble, the two headers and the first group, so a
multi-GB file previews instantly. The **ingest** pass streams the file once, appending to
one column buffer per pulse field and flushing each group's columns to a blob at the group
boundary, so peak memory is one group rather than one file.

Import is transactional at the dataset level: a failure rolls back the SQLite rows and
deletes any orphaned blobs.

### 7.5 Export

The writer reverses the grammar exactly — same preamble, same header rows, same column
order, same delimiter, TOA rescaled back to its source unit — satisfying G1. Group
properties and pulse fields that were never mapped to a definition are written back from
`attributes` verbatim. A round-trip test fixture set lives in
`crates/sp-csv/tests/fixtures/`, seeded from `sample/sample.csv`.

---

## 8. Signal Generation

### 8.1 Spec Model

A generated signal is described by a serialisable DAG, stored as JSON in `signal.gen_spec`.
Samples are a *cache* of the spec — deleting a blob is always safe.

```rust
pub struct GenSpec {
    pub timebase: Timebase,   // sample_rate_hz, duration_s, t0_s
    pub dtype:    DType,
    pub domain:   Domain,
    pub seed:     u64,        // ChaCha12 seed; makes noise reproducible (G3)
    pub root:     Node,
}

pub enum Node {
    // Primitives
    Sine      { freq_hz: f64, amp: f64, phase_rad: f64, offset: f64 },
    Square    { freq_hz: f64, amp: f64, duty: f64, phase_rad: f64 },
    Triangle  { freq_hz: f64, amp: f64, phase_rad: f64 },
    Sawtooth  { freq_hz: f64, amp: f64, rising: bool },
    Pulse     { period_s: f64, width_s: f64, amp: f64, rise_s: f64, fall_s: f64 },
    Chirp     { f0_hz: f64, f1_hz: f64, sweep: Sweep, amp: f64 },  // Linear|Log|Quadratic
    Dc        { level: f64 },
    Ramp      { start: f64, end: f64 },
    Step      { at_s: f64, before: f64, after: f64 },
    Impulse   { at_s: f64, amp: f64 },
    Noise     { kind: NoiseKind, amp: f64 },   // Gaussian|Uniform|Pink|Brown
    Prbs      { order: u8, taps: Option<u32>, amp: f64 },
    Expr      { source: String },              // f(t) via `meval`, t in seconds

    // Combinators
    Sum       { terms: Vec<Node> },
    Product   { terms: Vec<Node> },
    Concat    { parts: Vec<(Node, f64)> },     // (node, duration_s)
    Gain      { input: Box<Node>, factor: f64 },
    Delay     { input: Box<Node>, by_s: f64 },
    Clip      { input: Box<Node>, lo: f64, hi: f64 },
    Envelope  { input: Box<Node>, env: EnvelopeSpec },  // ADSR | Gaussian | Tukey
    Modulate  { carrier: Box<Node>, modulator: Box<Node>, kind: ModKind },
                                               // Am{depth} | Fm{dev_hz} | Pm{dev_rad}
    Resample  { input: Box<Node>, to_rate_hz: f64 },
    FromSignal{ signal_id: SignalId },         // reference a stored signal as a source
}
```

### 8.2 Rendering

- Pure function `render(&GenSpec, range: SampleRange) -> SampleBuffer`, so any window can
  be produced independently. Rayon splits the range across cores.
- Noise nodes derive a per-node stream from `seed` by hashing `(seed, node_path)` — a
  range rendered in parallel chunks matches a range rendered serially, and adding a node
  elsewhere in the tree does not perturb an existing node's noise.
- Node parameter validation happens up front: negative frequencies, duty outside (0,1),
  Nyquist violations (`freq_hz ≥ fs/2`) become blocking errors with a fix suggestion.

### 8.3 Generator UI

- Left: a node tree with add/remove/reorder. Right: parameters for the selected node.
- A **live preview** strip renders the first ~2 seconds at reduced rate on every keystroke
  (debounced 150 ms), so the shape is visible before committing.
- **Batch/sweep mode**: mark one numeric parameter as swept (`start`, `stop`, `step`, or an
  explicit list) to emit a whole group of signals in one action — e.g. 20 sine waves from
  100 Hz to 2 kHz. The sweep becomes a `signal_group` with the swept value stored as a
  property, which is exactly the shape a pipeline wants as test input.
- Presets are `GenSpec` JSON files; a small built-in library ships with the app.

### 8.4 Test-Vector Generation

Generation exists primarily to feed §9. Three patterns get first-class support:

- **Known-answer vectors** — a generated signal whose correct processing result is known
  analytically (a single tone at a known frequency, a pulse train with known edges). Stored
  alongside an *expectation* the pipeline can assert against (§9.7).
- **Impairment ladders** — one clean source rendered at a sweep of SNRs or clock offsets,
  producing a group per rung, so an algorithm's degradation curve falls out of one run.
- **Preprocessed inputs** — generation can emit `digital_logic` or `symbols` signals
  directly, so a stage that expects an already-sliced input can be tested without first
  running the analog front end.

---

## 9. Processing Pipeline

### 9.1 Model

A **pipeline** is an ordered list of **stages**. The unit of work is one **group**: a
group's signals and metadata enter stage 1, the output of each stage becomes the input of
the next, and the whole sequence is recorded so any intermediate point can be inspected.

```
                ┌───────────┐    ┌───────────┐    ┌───────────┐    ┌───────────┐
 group 7 ──────►│ 1 Detrend │───►│ 2 Bandpass│───►│ 3  FFT    │───►│ 4 Detect  │
                └─────┬─────┘    └─────┬─────┘    └─────┬─────┘    └─────┬─────┘
   signals out ───────┘                │                │                │
   signals out ────────────────────────┘                │                │
   Spectrum artifact ────────────────────────────────────┘               │
   Detections artifact ──────────────────────────────────────────────────┘
   + metrics + diagnostics at every stage
```

Groups are independent, so the scheduler runs several concurrently while keeping stage
order strict within each. This is also why the group is the natural retry unit: one bad
group fails on its own without sinking the run.

### 9.2 The Stage Trait

```rust
pub trait Stage: Send + Sync {
    /// Static identity and contract. Drives the pipeline editor's generated UI.
    fn descriptor(&self) -> &StageDescriptor;

    /// Validate and apply parameters. Called before any group is processed.
    fn configure(&mut self, params: &ParamSet) -> Result<(), ConfigError>;

    /// Optional: called once per run before the first group.
    fn begin_run(&mut self, _ctx: &RunCtx) -> Result<(), StageError> { Ok(()) }

    /// The work. One group in, one result out. Must be deterministic given
    /// (params, input) — this is what makes G8 hold.
    fn process(&mut self, ctx: &StageCtx, input: &GroupFrame)
        -> Result<StageOutput, StageError>;

    /// Optional: called after the last group; may emit run-level artifacts
    /// (e.g. an ROC curve accumulated across every group).
    fn end_run(&mut self, _ctx: &RunCtx) -> Result<StageOutput, StageError> {
        Ok(StageOutput::default())
    }
}

pub struct StageDescriptor {
    pub kind:     &'static str,        // 'dsp.filter.biquad'
    pub version:  u32,                 // bump when behaviour changes → cache invalidation
    pub label:    &'static str,        // 'Biquad Filter'
    pub summary:  &'static str,        // one line for the palette
    pub inputs:   &'static [PortSpec],
    pub outputs:  &'static [PortSpec],
    pub params:   &'static [ParamSpec],
}
```

`version` is not decoration: it is part of the cache key and is recorded in every run, so
"this result came from an older algorithm" is always answerable.

### 9.3 Ports and Typing

Stages are linear in execution but not in data: stage 4 may want the raw signal *and*
stage 3's spectrum. Ports give that without a graph editor.

```rust
pub struct PortSpec {
    pub name:     &'static str,     // 'signals', 'spectrum'
    pub kind:     PortKind,
    pub required: bool,
}

pub enum PortKind {
    Signals { domain: Option<Domain> },  // None ⇒ any domain
    Artifact(&'static str),              // by artifact kind, e.g. 'spectrum.v1'
    Any,
}
```

Each stage's outputs are published into a **frame-scoped, typed blackboard**. A downstream
stage's required inputs are resolved from the nearest upstream producer of that port kind.
The pipeline editor validates this **at edit time** — a stage whose required input nothing
upstream produces is flagged in place, with the missing port named, before the run starts.

`GroupFrame` is what flows:

```rust
pub struct GroupFrame {
    pub group:    GroupMeta,        // name, properties, ordinal
    pub signals:  Vec<SignalRef>,   // lazily mmap-backed; never eagerly copied
    pub inbound:  PortMap,          // artifacts published by upstream stages
    pub run:      RunId,
}
```

### 9.4 Stage Output

A stage may alter signals, emit entirely new typed structs, or both.

```rust
#[derive(Default)]
pub struct StageOutput {
    pub signals:     Vec<SignalOut>,
    pub artifacts:   Vec<ArtifactOut>,
    pub properties:  Vec<PropertyPatch>,      // write-back onto group/signals
    pub metrics:     BTreeMap<String, f64>,   // scalar summaries, charted across groups
    pub diagnostics: Vec<Diagnostic>,         // info/warn/error with an optional time span
}

pub enum SignalOut {
    /// Same slot, new samples — a filter, a normaliser.
    Replace { ordinal: usize, samples: SampleBuffer, patch: AttrPatch },
    /// A new signal in the group — an envelope, a demodulated baseband.
    Add     { name: String, domain: Domain, samples: SampleBuffer, attrs: Attributes },
    /// Unchanged. Costs nothing: same content hash, same blob (§5.3).
    Passthrough { ordinal: usize },
    /// Removed from downstream stages (still visible in this stage's recorded output).
    Drop    { ordinal: usize },
}
```

`Passthrough` is explicit rather than implicit. A stage must say what happened to every
input signal, which means the UI can always answer "what did this stage do to signal 3?"
with `Replaced`, `Untouched`, or `Dropped` — no inference.

### 9.5 Execution and Caching

```
for each group  (parallel, bounded by in_flight_cap):
    frame ← load(group)                       # metadata + lazy signal refs
    for stage in pipeline:                    # strictly ordered
        key ← blake3(stage.kind, stage.version, params_hash, input_hash)
        if cache.hit(key) and stage.pure:
            output ← cache.get(key)           # stage skipped entirely
        else:
            output ← stage.process(ctx, frame)
            cache.put(key, output)
        record(run, group, stage, output)     # blobs + artifact rows
        frame ← frame.apply(output)
```

- `input_hash` folds the content hashes of every input signal and inbound artifact, so the
  key changes exactly when the inputs do.
- **Editing stage 4's parameters re-runs stages 4…n only.** Stages 1–3 hit the cache. On a
  long pipeline this is the difference between iterating on an algorithm and waiting on one.
- Stages may declare themselves impure (`descriptor().pure == false`) to opt out of caching.
- **Retention policy** per stage: `Always` / `OnFailure` / `Never`, plus a run-level size
  cap. Content addressing means unchanged signals never consume space regardless of policy.

### 9.6 Run Schema

```sql
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
    params_json TEXT    NOT NULL,
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
    pipeline_hash TEXT NOT NULL,       -- kinds + versions + params, canonicalised
    app_version   TEXT NOT NULL,
    notes         TEXT
);

CREATE TABLE run_group (
    run_id   INTEGER NOT NULL REFERENCES run(id) ON DELETE CASCADE,
    group_id INTEGER NOT NULL REFERENCES signal_group(id),
    status   TEXT NOT NULL,
    wall_ms  INTEGER,
    PRIMARY KEY (run_id, group_id)
);

CREATE TABLE run_stage (
    run_id        INTEGER NOT NULL REFERENCES run(id) ON DELETE CASCADE,
    group_id      INTEGER NOT NULL,
    stage_ordinal INTEGER NOT NULL,
    status        TEXT    NOT NULL,     -- ok | failed | skipped | cached
    wall_ms       INTEGER,
    cache_key     TEXT    NOT NULL,
    metrics_json  TEXT    NOT NULL DEFAULT '{}',
    diagnostics_json TEXT NOT NULL DEFAULT '[]',
    PRIMARY KEY (run_id, group_id, stage_ordinal)
);

-- Signals as they existed at the OUTPUT of a given stage.
CREATE TABLE run_signal (
    run_id        INTEGER NOT NULL REFERENCES run(id) ON DELETE CASCADE,
    group_id      INTEGER NOT NULL,
    stage_ordinal INTEGER NOT NULL,     -- -1 = the untouched source
    signal_ordinal INTEGER NOT NULL,
    name          TEXT    NOT NULL,
    domain        TEXT    NOT NULL,
    disposition   TEXT    NOT NULL
                  CHECK (disposition IN ('replaced','added','passthrough','dropped')),
    blob_id       INTEGER REFERENCES sample_blob(id),
    sample_rate_hz REAL, t0_s REAL, sample_count INTEGER,
    attrs_json    TEXT NOT NULL DEFAULT '{}',
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
    blob_id       INTEGER REFERENCES sample_blob(id),   -- large payloads out of line
    summary       TEXT                  -- '4 detections, best score 0.91'
);
CREATE INDEX ix_artifact_lookup ON artifact(run_id, group_id, stage_ordinal);

-- A run promoted to golden, for regression comparison.
CREATE TABLE baseline (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    run_id      INTEGER NOT NULL REFERENCES run(id),
    created_utc TEXT NOT NULL,
    tolerance_json TEXT NOT NULL DEFAULT '{}'
);
```

`stage_ordinal = -1` records the source signals as they entered the pipeline, so "before"
is a real row and not a special case in the UI.

### 9.7 Assertions

A pipeline may carry assertions, evaluated per group after the last stage. This is what
turns a run into a test:

```
detections.count == 4
metrics.snr_db > 12.0
signals["envelope"].rms within 5% of baseline
stage[3].wall_ms < 250
```

Each assertion resolves to pass / fail / not-applicable, and the run's status aggregates
them. Failures name the group, the assertion, and the actual value.

### 9.8 Built-in Stages (`sp-dsp`)

Enough to exercise the harness and cover common preprocessing, all implemented against the
same public trait a user's algorithm would use:

| Family | Stages |
|--------|--------|
| Conditioning | Detrend, DC block, normalise (peak/RMS), gain, clip, resample, window, trim/pad |
| Filtering | FIR (windowed-sinc), IIR biquad cascade — low / high / band / notch |
| Transform | FFT → `Spectrum`, STFT → `Spectrogram`, Hilbert → envelope + instantaneous phase |
| Digital | Threshold/slice → `DigitalLogic`, clock recovery, symbol decode → `Symbols`, bit pack → `Bits` |
| Detection | Peak find, edge find, pulse measure, CFAR → `Detections` |
| Measurement | Statistics, THD/SNR/SINAD, pulse metrics → `Metrics` |
| Utility | Passthrough (a labelled inspection point), split, merge, tee-to-property |

---

## 10. Results, Artifacts and Inspection

This section is the answer to *"show me the result after each processing step."*

### 10.1 The Artifact Model

Not every stage output is a signal. A detector emits spans, a demodulator emits symbols, an
FFT emits a spectrum, a measurement emits a scalar table. These are **artifacts**: typed,
serialisable structs with a declared schema and a display hint.

```rust
pub trait Artifact: Serialize + DeserializeOwned + Send + Sync + 'static {
    const KIND: &'static str;          // 'detections.v1'
    const VERSION: u32;
    fn schema() -> ArtifactSchema;     // fields, units, and how to draw it
    fn summary(&self) -> String;       // one line for the stage rail
}

pub struct ArtifactSchema {
    pub fields: Vec<FieldSpec>,        // name, kind, unit, description
    pub view:   ViewHint,
}

pub enum ViewHint {
    /// Rows in a sortable table. The default for anything tabular.
    Table    { columns: Vec<ColumnSpec> },
    /// Drawn on the scope's time axis, aligned with the signals.
    Overlay  { form: OverlayForm },    // Spans | Markers | Stems | Bands
    /// A line chart with its own axes (spectrum, filter response, ROC).
    Series   { x: FieldRef, y: Vec<FieldRef>, x_log: bool, y_log: bool },
    /// A 2-D intensity map (spectrogram, correlation surface).
    Heatmap  { rows: FieldRef, cols: FieldRef, values: FieldRef },
    /// Points in a plane (constellation, feature space).
    Scatter  { x: FieldRef, y: FieldRef, colour: Option<FieldRef> },
    /// Key/value scalars.
    Scalars,
    /// Fallback: a JSON tree. Always available, never the best choice.
    Tree,
}
```

**A new artifact type costs one `impl`.** Derive `Serialize`, declare a `KIND` and a
`ViewHint`, register it — the results screen can then display it, the table can sort it,
overlays land on the timeline, and it persists without any storage code. That is G9 applied
to outputs rather than algorithms.

Built-in kinds: `Spectrum`, `Spectrogram`, `Detections`, `Symbols`, `Bits`, `Metrics`,
`Constellation`, `Histogram`, `FilterResponse`, `Table`, `Text`.

**Storage.** Payloads under 64 KB are stored as JSON in `artifact.payload_json`; larger
ones (a spectrogram matrix) go to a content-addressed `.artbin` blob with the JSON holding
only the header. The viewer sees no difference.

**Overlay artifacts are the payoff.** A `Detections` artifact draws as shaded spans
directly on the scope, on the same time axis as the signal it came from, moving with the
playhead. Seeing where a detector fired *against the waveform that triggered it* is the
whole point of a step-by-step view.

### 10.2 The Stage Rail

The results screen is organised around a horizontal rail — one chip per stage, in order:

```
┌────────┐ ┌──────────┐ ┌──────────┐ ┌────────┐ ┌──────────┐
│ Source │ │1 Detrend │ │2 Bandpass│ │ 3 FFT  │ │4 Detect  │
│  3 sig │ │  3 sig   │ │  3 sig   │ │ +Spec  │ │ +4 det   │
└────────┘ └──────────┘ └──────────┘ └────────┘ └──────────┘
    ▲            ▲                                    ▲
  pinned A    selected                            9 ms · ok
```

- Each chip shows what the stage produced — signal count, new artifacts, a status dot, and
  wall time. A stage that only altered properties or emitted metrics still says so.
- **Click** a chip to display that stage's output. **Shift-click** a second to pin it as a
  comparison; the scope then draws A and B overlaid with a residual (A−B) trace beneath,
  and artifact panes show a field-level diff.
- Left/right arrow keys walk the pipeline, so stepping through an algorithm is one key.
- The rail is present in the Scope screen too, so playback and stage inspection are the
  same surface rather than two.

### 10.3 The Results Screen

Three regions, driven by the current (group, stage) selection:

1. **Group list** — every group in the run with per-group status, wall time, assertion
   result, and metric columns. Sortable, filterable; this is where a failing case gets
   found across a large dataset.
2. **Scope** — the selected stage's signals on the shared timeline, with overlay artifacts
   drawn on the same axis, plus the comparison trace when a stage is pinned.
3. **Artifact panes** — a docked pane per non-overlay artifact, each rendered by its
   `ViewHint`. Panes are collapsible and remembered per pipeline.

**The playhead is global.** Scrubbing moves the cursor in the scope, in the spectrogram, in
the detections table (which highlights the row under the cursor), and in every other
time-aware pane at once. Switching stages preserves the playhead and the viewport, so the
signal appears to transform in place rather than the view jumping.

### 10.4 Comparison and Regression

- **Stage-to-stage** (within a run): the pin mechanism above. Answers "what did this stage
  change?"
- **Run-to-run**: pick two runs over the same dataset and diff. For signals, per-signal max
  absolute error, RMS error and first-divergence sample index. For artifacts, a field-level
  diff with numeric tolerance. Answers "did my change alter anything, and where first?"
- **Baseline**: promote a run to a named baseline with tolerances. Later runs compare
  automatically and report pass/fail per group. Combined with §9.7 assertions this is a
  regression suite — and the CLI (`signalplayback run --pipeline X --assert-baseline Y`)
  makes it runnable in CI.

### 10.5 Metrics Across Groups

Every stage's `metrics` map is recorded per group, so a run yields a metrics table:
group × stage × metric. The results screen charts any metric across groups — with a
generated dataset whose groups are an impairment ladder (§8.4), that chart *is* the
algorithm's performance curve, produced by one run and no extra tooling.

---

## 11. Playback Engine

### 11.1 Transport State Machine

```
                 ┌──────────┐  play   ┌──────────┐
        seek ───►│ Stopped  ├────────►│ Playing  │◄─── rate change (no state change)
                 └────▲─────┘         └────┬─────┘
                      │ stop               │ pause
                      │              ┌─────▼────┐
                      └──────────────┤  Paused  │
                             stop    └──────────┘
```

`Playing` advances the playhead; `Paused` retains it; `Stopped` resets it to the loop
start. Seek is legal in every state and does not change state.

### 11.2 Clock

Playback time is **decoupled from frame rate**. Each tick:

```
now        = Instant::now()
dt_wall    = now - last_tick
playhead  += dt_wall.as_secs_f64() * rate      // rate ∈ [0.01, 100.0], may be negative
last_tick  = now
```

- A monotonic `Instant` avoids wall-clock jumps.
- If `dt_wall > 250 ms` (window unfocused, breakpoint hit), the tick is **clamped**
  instead of applied, so the playhead does not teleport after a stall.
- At the loop end: wrap to loop start (`Loop`), clamp and pause (`Once`), or reverse
  (`PingPong`).
- Ticks are driven by an Iced `Subscription` on window redraw requests, so the engine
  never runs faster than the display and idles at zero CPU when paused.

### 11.3 Timeline and Alignment

All signals map onto one **absolute timeline** in seconds. A signal's sample *i* sits at
`t = t0_s + i / sample_rate_hz` (or at `time_blob[i]` when irregular). Signals with
different rates therefore overlay correctly with no resampling — which matters after a
decimating stage, where a stage's output has a different rate from its input but must
still line up. The playlist's `t_offset_s` nudges an individual trace for manual alignment.

### 11.4 Scope Rendering

Two display modes:

- **Playhead mode** — the viewport is fixed; a vertical cursor sweeps across it.
- **Scrolling mode** — the playhead is pinned (typically at 80% width) and the window
  scrolls under it, like a strip-chart recorder.

Rendering runs in four Iced canvas layers so a moving playhead never re-tessellates the
traces:

| Layer | Contents | Invalidated when |
|-------|----------|------------------|
| Grid | Axes, gridlines, tick labels | Viewport changes |
| Traces | Signal geometry from the pyramid | Viewport or signal set changes |
| Artifacts | Overlay artifacts: spans, markers, stems | Stage selection changes |
| Overlay | Playhead, cursors, annotations, hover readout, selection band | Every tick (cheap) |

Interactions: scroll = zoom time about the pointer, shift+scroll = pan, ctrl+scroll = zoom
amplitude, drag = box zoom, double-click = fit, `Home`/`End` = jump to bounds,
space = play/pause, `[` / `]` = set loop in/out, `←`/`→` = previous/next stage.

Per-trace controls: visibility, colour, gain, vertical offset, and a **stacked vs.
overlaid** layout toggle. Domain drives the default renderer — `DigitalLogic` signals get
logic lanes, `BasebandIq` gets I/Q or magnitude, `Symbols` gets labelled stems.

---

## 12. User Interface

### 12.1 Screens

| Screen | Purpose |
|--------|---------|
| **Library** | Tree of Dataset → Group → Signal with search, property filters, and a sortable detail table. Multi-select feeds the scope, a playlist, or a pipeline run. |
| **Import** | File picker → preview grid of the first blocks → column-mapping panel (including bind-to-property) → profile save/load → progress with a live error list. |
| **Generate** | Node tree editor, parameter form, live preview, sweep configuration, preset browser. |
| **Pipeline** | Stage palette on the left, ordered stage list in the middle, generated parameter form on the right. Port validation inline. Run controls with group selection. |
| **Results** | Group list + stage rail + scope + artifact panes (§10.3). The main working surface for algorithm development. |
| **Runs** | History of runs with pipeline hash, status, timing, assertion results; promote to baseline; diff two runs. |
| **Scope** | Playback-focused view of stored signals, with the same stage rail available when a run is loaded. |
| **Inspector** | Single-signal detail: full metadata, property editor, tags, statistics, histogram, FFT magnitude, virtualised raw sample table. |
| **Properties** | Manage property definitions and property sets (§6.3). |
| **Settings** | Library location, theme, default sample rate, strict/tolerant import, retention defaults, decimation quality, keyboard map. |

### 12.2 Iced Application Shape

```rust
enum Screen {
    Library, Import(ImportState), Generate(GenState),
    Pipeline(PipelineState), Results(ResultsState), Runs,
    Scope(ScopeState), Inspector(SignalId), Properties, Settings,
}

struct App {
    store:   StoreHandle,       // channel to the writer actor
    stages:  StageRegistry,     // populated at startup by sp-dsp (+ future plugins)
    screen:  Screen,
    library: LibraryIndex,      // cached metadata; samples stay on disk
    scope:   ScopeState,        // survives screen switches so playback keeps its position
    active_run: Option<RunView>,// current run + selected (group, stage) + pinned stage
    toasts:  Vec<Toast>,
    modal:   Option<Modal>,
}

enum Message {
    Nav(Screen),
    Library(library::Message),
    Import(import::Message),
    Generate(generate::Message),
    Pipeline(pipeline::Message),
    Results(results::Message),      // SelectStage, PinStage, SelectGroup, TogglePane
    Scope(scope::Message),
    Store(StoreEvent),
    Tick(Instant),                  // playback clock
    Progress { job: JobId, done: u64, total: u64 },
    StageFinished { run: RunId, group: GroupId, stage: u16, status: StageStatus },
    JobFinished(JobId, Result<JobOutcome, JobError>),
    Error(AppError),
}
```

Each screen module owns its own `State`/`Message`/`update`/`view`, and the root `update`
delegates. Long jobs return a `JobId` immediately; progress arrives as messages.
`StageFinished` is separate from generic progress so the stage rail can fill in live as a
run proceeds — the user can inspect stage 1's output while stage 4 is still computing.

### 12.3 UX Principles

- **No modal blocking on IO.** Import, generation and runs happen in the background; the
  library stays browsable.
- **Inspection is never destructive.** Selecting or pinning a stage only changes the view.
  Nothing in the results screen mutates the library.
- **The view survives navigation.** Playhead, viewport, selected group and selected stage
  persist across screen switches, so comparing two stages never costs the user their place.
- **Destructive actions confirm and are undoable** where cheap (delete moves to a trash
  table for the session; permanent delete is a separate explicit action).
- **Errors are data, not dialogs.** Import problems and stage diagnostics land in
  filterable lists the user can work through, not a popup per row.
- **Keyboard first** for transport, stage stepping and navigation.

---

## 13. Performance Budget

| Operation | Target | Approach |
|-----------|--------|----------|
| Scope frame | < 8 ms at 1080p, 8 traces | Pyramid decimation to ≈1 pair/pixel; layered canvas caches |
| Playhead tick | < 0.5 ms | Overlay-layer redraw only |
| Stage switch in results | < 30 ms | Metadata-only load; pyramids already built; viewport preserved |
| CSV sniff (first 2 blocks) | < 100 ms on a 4 GB file | Bounded read, never a full scan |
| CSV ingest | ≥ 50 MB/s single-threaded | Streaming reader, no intermediate `String` per field, direct write into a column buffer |
| Pulse ingest | ≥ 1 M records/s | One append per field per row; group flushed to blobs at the block boundary |
| Cross-group pulse search | < 1 s over 10 000 groups | Zone-map prefilter in SQL, then parallel mmap scan of surviving columns (§6.6) |
| Generation | ≥ 100 M samples/s (sine, 8 cores) | Rayon chunking, no per-sample allocation |
| Pyramid build | ≤ 1 linear pass over the blob | Build all levels in one traversal |
| Pipeline throughput | ≥ 0.8 × cores on group-parallel work | Groups fan out across rayon; bounded in-flight frames |
| Re-run after a param edit | Only stages ≥ the edited one | Content-hash cache (§9.5) |
| Library query | < 50 ms at 10 000 signals | Indexed SQLite + FTS5 + `signal_property` index, metadata only |
| Cold start | < 500 ms | Lazy pyramid load, no eager sample reads |

Benchmarks live in `crates/*/benches/` using `criterion` and run in CI on a fixed fixture set.

---

## 14. Cross-Cutting Concerns

**Error handling.** `thiserror` enums per crate; `sp-app` maps them to user-facing text.
Nothing panics on bad input — a corrupt CSV, a truncated blob, or a stage given a
zero-length signal produces a diagnostic. Panics in worker tasks and in stage code are
caught at the task boundary (`catch_unwind`) and reported as a stage failure with the
group named, so one bad algorithm cannot take down a run.

**Integrity.** Blob checksums are verified on first read after app start and after any
crash-recovery. A `Verify Library` action rehashes everything and reports mismatches in
both directions, including artifact blobs.

**Testing.**
- Unit tests per crate; property tests (`proptest`) for the CSV round-trip (G1) and for
  generator determinism across chunk boundaries (G3).
- Golden-file tests for the parser against a fixture corpus including malformed files.
- **Stage conformance harness**: a generic test any `Stage` impl can be run through,
  checking determinism (same input twice → identical output hash), port contract
  compliance, cancellation responsiveness, and no-panic on degenerate inputs (empty
  signal, single sample, all-NaN, DC-only). New algorithms get this for free.
- `sp-engine` transport tests drive the state machine with a mock clock — no UI needed.
- A headless smoke test opens a library, imports a fixture, runs a pipeline, and asserts
  against a baseline.

**Observability.** `tracing` spans around every job and every stage invocation; a Log panel
in the app plus a rolling file log in the library root. Per-stage wall time is recorded in
`run_stage`, so a slow stage is visible in the rail without profiling.

**Distribution.** `cargo build --release` → single `.exe`. Windows is the primary target;
Linux and macOS build from the same source. An optional MSI via `cargo-wix` later.

**Accessibility.** Colour palettes are colour-blind-safe by default (Okabe–Ito for traces);
trace and artifact identity is also conveyed by legend and line style, never colour alone.
Minimum 14 px UI text, full keyboard navigation.

---

## 15. Potential Features

Grouped by area and tiered: **[MVP]** ships in v1 · **[V1.x]** near-term · **[V2]**
larger effort · **[Stretch]** speculative.

### 15.1 Import & Data Ingest
- **[MVP]** Pulse-record CSV import (§7) with preview and column mapping.
- **[MVP]** Saved import profiles for recurring file shapes.
- **[MVP]** Strict vs. tolerant count handling with a diagnostics list.
- **[MVP]** Time-of-arrival column with a configurable source unit (default µs).
- **[MVP]** Map CSV columns onto typed property definitions.
- **[V1.x]** Drag-and-drop import; import multiple files in one action.
- **[V1.x]** Watch-folder auto-import.
- **[V1.x]** Import from clipboard paste.
- **[V1.x]** Domain inference proposals (digital vs analog vs I/Q).
- **[V2]** Additional formats: WAV, MATLAB `.mat`, HDF5, SigMF, Parquet, NumPy `.npy`, TDMS, VCD.
- **[V2]** Direct import from an instrument save format (Tektronix/Keysight scope CSV/WFM).
- **[V2]** Incremental / append import — extend an existing signal from a newer capture.
- **[Stretch]** Streaming ingest from a live socket into the database.

### 15.2 Generation
- **[MVP]** Core primitives: sine, square, triangle, sawtooth, DC, ramp, step, impulse, noise.
- **[MVP]** Sum/product composition, gain, delay, envelope.
- **[MVP]** Deterministic seeded noise.
- **[MVP]** Parameter sweep to generate a whole group at once.
- **[V1.x]** Chirps (linear/log/quadratic) and AM/FM/PM modulation.
- **[V1.x]** Arbitrary `f(t)` expression node.
- **[V1.x]** PRBS / m-sequence generator.
- **[V1.x]** Direct generation of `digital_logic` and `symbols` signals as preprocessed inputs.
- **[V1.x]** Impairment ladders — one source across a sweep of SNRs, one group per rung.
- **[V1.x]** Preset library with import/export.
- **[V2]** Digital modulation: ASK/FSK/PSK/QPSK/QAM with configurable symbol rate and pulse shaping.
- **[V2]** Impairment stack: AWGN at a target SNR, phase noise, IQ imbalance, DC offset, clock drift, dropouts, clipping.
- **[V2]** Known-answer vectors carrying their own expected result for assertion.
- **[V2]** Multi-tone / comb generator with per-tone phase control.
- **[V2]** Draw-a-waveform: sketch on the canvas and fit it to a sample array.
- **[V2]** Derive a new signal from stored signals (`FromSignal` node) — mix, splice, resample.
- **[Stretch]** Node-graph visual editor with drag-wired connections.
- **[Stretch]** Learn a generator spec that approximates a selected recorded signal.

### 15.3 Processing & Algorithm Testing
- **[MVP]** Linear pipeline of stages, executed one group at a time.
- **[MVP]** `Stage` trait + registry; generated parameter forms from `ParamSpec`.
- **[MVP]** Typed ports with edit-time validation.
- **[MVP]** Per-stage recording of signals, artifacts, metrics and diagnostics.
- **[MVP]** Built-in conditioning, filtering, FFT and threshold stages.
- **[MVP]** Cancellable runs with per-group progress.
- **[V1.x]** Content-hash stage cache — editing stage *n* re-runs only *n…end*.
- **[V1.x]** Retention policy per stage (always / on failure / never).
- **[V1.x]** Assertions on metrics and artifacts; run status aggregates them.
- **[V1.x]** Baseline promotion and automatic regression comparison.
- **[V1.x]** Stage conformance test harness for new algorithms.
- **[V1.x]** Detection, symbol-decode and measurement stage families.
- **[V2]** Parameter sweep over a stage — run the pipeline across a grid, get a metrics table.
- **[V2]** Branching pipelines (a real DAG) with a graph editor.
- **[V2]** Per-stage breakpoints: pause a run at a stage and inspect before continuing.
- **[V2]** Stage-level unit fixtures — pin one group's input as a stage's test case.
- **[V2]** Plugin stages loaded from a dynamic library.
- **[V2]** Distributed / multi-process run execution for large datasets.
- **[Stretch]** Stage authored in an embedded script for quick experiments.
- **[Stretch]** Auto-tuning — search stage parameters against an objective metric.

### 15.4 Results & Inspection
- **[MVP]** Stage rail with per-stage summary, status and wall time.
- **[MVP]** Artifact model with `ViewHint`-driven viewers: table, series, scalars, tree.
- **[MVP]** Overlay artifacts drawn on the scope's time axis.
- **[MVP]** Global playhead shared across scope and every time-aware artifact pane.
- **[MVP]** Group list with status, timing and metric columns.
- **[V1.x]** Pin-two-stages comparison with residual (A−B) trace.
- **[V1.x]** Heatmap viewer for spectrograms and correlation surfaces.
- **[V1.x]** Scatter viewer for constellations and feature spaces.
- **[V1.x]** Run-to-run diff: max abs error, RMS error, first divergence index.
- **[V1.x]** Metric-across-groups chart (the degradation curve).
- **[V1.x]** Jump from a diagnostic to the time span that produced it.
- **[V2]** Field-level artifact diff between runs.
- **[V2]** Save an inspection layout (panes, pinned stages, viewport) per pipeline.
- **[V2]** Export a stage's output back into the library as a new dataset.
- **[Stretch]** Side-by-side A/B of two *pipelines* on one group.

### 15.5 Signal Properties & Typing
- **[MVP]** Domain and provenance on every signal, driving default rendering.
- **[MVP]** User-defined property definitions with typed kinds, units and validation.
- **[MVP]** Property editor in the Inspector; unrecognised attributes preserved.
- **[V1.x]** Named property sets applied to a dataset or group.
- **[V1.x]** Indexed property queries and saved smart searches.
- **[V1.x]** Stage write-back of estimated properties, namespaced by stage.
- **[V2]** Property templates inferred from a sample file.
- **[V2]** Derived/computed properties defined by an expression over other properties.
- **[Stretch]** Schema versioning with migration of existing property values.

### 15.6 Database & Library Management
- **[MVP]** Dataset/group/signal hierarchy, search, sortable detail table.
- **[MVP]** Tags with filter-by-tag.
- **[MVP]** Cross-group pulse search: numeric predicates over pulse fields, zone-map prefiltered (§6.6).
- **[V1.x]** Name and tag an individual pulse; saved pulse selections.
- **[V1.x]** Bulk edit: rename, retag, set properties across a selection.
- **[V1.x]** Duplicate detection via blob checksum; deduplicate on ingest.
- **[V1.x]** Library statistics dashboard (count, total size, rate distribution, run storage).
- **[V1.x]** Vacuum / verify / rebuild-pyramids / prune-old-runs maintenance actions.
- **[V2]** Multiple libraries open simultaneously with cross-library compare.
- **[V2]** Versioning: keep prior revisions of an edited signal.
- **[V2]** Archive/restore a dataset or a run to a single portable `.splib` bundle.
- **[V2]** Signal provenance graph — what was derived from what, across runs.
- **[Stretch]** Optional remote/shared library backend.

### 15.7 Playback & Visualization
- **[MVP]** Transport: play, pause, stop, seek, rate, loop; playhead and scrolling modes.
- **[MVP]** Multi-signal overlay and stacked lanes; per-trace colour/gain/offset.
- **[MVP]** Zoom/pan, box zoom, fit, hover readout.
- **[MVP]** Domain-aware renderers: analog trace, logic lanes, I/Q, symbol stems.
- **[V1.x]** Cursors: two time cursors with Δt/Δy and 1/Δt readout.
- **[V1.x]** Markers and annotations pinned to timeline positions, persisted in the DB.
- **[V1.x]** A/B loop region with per-region rate.
- **[V1.x]** Y-axis autoscale modes: per-trace, shared, fixed, percentile.
- **[V2]** XY (Lissajous) plot mode for signal pairs.
- **[V2]** Spectrogram / waterfall lane synchronised to the playhead.
- **[V2]** Live FFT lane updating at the playhead.
- **[V2]** Eye diagram and constellation views for modulated signals.
- **[V2]** Trigger modes — start playback at an edge/level crossing, like a scope.
- **[Stretch]** Synchronised video/telemetry track alongside the signals.
- **[Stretch]** GPU compute-shader path for >1 G-sample traces.

### 15.8 Analysis
- **[V1.x]** Per-signal statistics: min, max, mean, RMS, std dev, peak-to-peak, zero crossings.
- **[V1.x]** Histogram view.
- **[V1.x]** FFT magnitude/phase with selectable window (Hann, Hamming, Blackman-Harris, flat-top).
- **[V2]** THD, SNR, SINAD, SFDR, ENOB measurements.
- **[V2]** Cross-correlation and time-delay estimation between two signals.
- **[V2]** Envelope detection, peak finding, edge/pulse measurements (rise time, width, duty).
- **[V2]** Signal math expression bar (`sig1 - sig2 * 0.5`) producing a derived signal.
- **[Stretch]** Anomaly flagging across a whole dataset.

### 15.9 Export & Interop
- **[MVP]** Export to the native pulse-record CSV format (round-trip fidelity, TOA restored to its source unit).
- **[V1.x]** Export a selection, a time range, or a stage's output.
- **[V1.x]** Export the scope view as PNG/SVG.
- **[V1.x]** Export a run's metrics table as CSV.
- **[V2]** Export to WAV, `.npy`, Parquet, MATLAB, HDF5, SigMF.
- **[V2]** Copy samples to clipboard as CSV for a spreadsheet.
- **[V2]** Generate a run report (PDF/HTML) with plots, metrics, assertions and metadata.
- **[Stretch]** Python binding (`pyo3`) to read the library and run results from scripts.

### 15.10 Automation & Extensibility
- **[V1.x]** CLI mode: `signalplayback import|generate|run|export` for scripting and CI.
- **[V1.x]** `run --assert-baseline` exit code, so a pipeline is a CI check.
- **[V2]** Scriptable batch jobs (a job file describing generate → run → compare → export).
- **[V2]** Plugin interface for custom stages, artifact kinds and import formats.
- **[Stretch]** Embedded scripting (Rhai/Lua) for on-the-fly signal math and quick stages.

### 15.11 Quality of Life
- **[MVP]** Persistent window layout and last-opened library.
- **[MVP]** Light/dark theme.
- **[V1.x]** Undo/redo for library and pipeline edits.
- **[V1.x]** Recent files, datasets and pipelines.
- **[V1.x]** Command palette (Ctrl+K) over every action, signal and stage.
- **[V1.x]** Configurable keyboard shortcuts.
- **[V2]** Workspaces — save a full scope + results setup and restore it.
- **[V2]** Session crash recovery.
- **[V2]** Localisation scaffolding.

---

## 16. Milestones

| Phase | Deliverable | Exit criteria |
|-------|-------------|---------------|
| **M0 — Skeleton** | Workspace, `sp-core` types, Iced window with screen nav, logging | App launches, screens switch, CI builds and clippy is clean |
| **M1 — Store** | SQLite schema + migrations, blob store, store actor, property definitions, library screen | Signals insert and list; property queries work; `Verify Library` passes |
| **M2 — Import** | Block framer, parser, mapping UI (incl. property binding), streaming ingest, error list | Fixture corpus imports; round-trip property test passes (G1) |
| **M3 — Generate** | `GenSpec`, primitives, combinators, generator UI, sweeps | Determinism property test passes (G3); presets load |
| **M4 — Playback** | Pyramid builder, viewport, scope canvas, transport, clock, domain renderers | 100 M-sample signal plays at 60 fps (G2) |
| **M5 — Pipeline** | `Stage` trait, registry, ports, scheduler, run recording, first `sp-dsp` stages | A pipeline runs over a dataset group-by-group and every stage's output is persisted |
| **M6 — Results** | Artifact model + viewers, stage rail, results screen, global playhead, comparison | Any (group, stage) is inspectable with its artifacts; pinned comparison works (G7) |
| **M7 — Regression** | Assertions, baselines, run diff, metrics chart, CLI `run` | A pipeline runs headless in CI and fails on a baseline deviation (G8) |
| **M8 — Polish** | Inspector, tags/search, export, settings, statistics, packaging | Full loop demoable end-to-end; release binary produced |

M2 and M3 are independent after M1 and can proceed in parallel. M5 depends on M1 (storage)
and benefits from M3 (test inputs) but not from M4; M6 depends on both M4 and M5.

---

## 17. Assumptions and Open Questions

Items marked ⚠ change the design materially and should be confirmed before the milestone
noted.

### CSV format — resolved by `sample/sample.csv` (2026-09-04)

1. **Block row order.** ✅ *Group header → pulse header → group row → pulse rows*, as
   originally stated — but the two header rows appear **once at file level**, not per
   group, and there is **no blank-line separator**. Framing is driven solely by the count
   column (§7.2). The auto-detecting framer is unnecessary and has been dropped.
2. **What a row contains.** ✅ Each row is one **pulse record**: a time of arrival plus a
   fixed set of numeric fields, with no trailing sample array. Groups hold up to millions
   of rows. Stored as one column per field with a shared TOA timebase, addressed as
   `(group, index)` — see §6.6 for why, and for what was rejected.
3. **Timebase source.** ✅ An explicit `time` column in every pulse row, in **microseconds**,
   scaled to seconds on ingest and restored on export. The mapping UI needs the "this
   column is time" affordance after all; the unit is a profile setting.
4. **Group `count` semantics.** ✅ The number of pulse rows following the group row, and the
   sole block-termination rule.
5. **Still open — per-pulse SQL identity.** §6.6 creates a `pulse` row only for an annotated
   pulse. If workflows turn out to need every pulse individually joinable, taggable or
   referenceable from a `SignalRef` property, that becomes a bulk-materialisation step with
   the storage cost set out there. Worth revisiting once cross-group search is in use.

### Processing (before M5)

6. ⚠ **Linear vs. branching pipelines.** v1 is a linear stage list with typed side-channel
   ports (§9.3). If real algorithms need genuine branching — two parallel chains reconciled
   at the end — that is a v2 graph editor, and knowing now would change the pipeline model
   rather than extend it later.
7. ⚠ **Cross-group state.** The design assumes stages are independent per group, with
   `begin_run`/`end_run` as the only cross-group hooks. If an algorithm needs to carry
   adaptive state *between* groups in order (a tracker, an adaptive equaliser), group
   parallelism must become opt-out per stage. A `sequential` flag in `StageDescriptor`
   covers it cheaply, but only if it goes in before the scheduler is written.
8. **Stage output signal count.** Assumed a stage may add and drop signals freely within a
   group. If downstream stages must see a fixed signal count matching the group's declared
   `count`, that is a validation rule worth stating now.
9. **Retention default.** `Always` is assumed, since content addressing makes passthrough
   free. A pipeline whose every stage rewrites every sample of a 100 M-sample signal will
   still cost ~400 MB per stage per group. Confirm whether the default should be
   `Always` with a size cap, or `OnFailure` with opt-in.
10. **Artifact size ceiling.** 64 KB inline / blob beyond that is a guess. A per-group
   spectrogram at fine resolution can reach hundreds of MB; if that is routine, the
   spectrogram artifact should store a decimated pyramid the way signals do.

### Data model

11. **Numeric precision.** Default storage is `f32` (halves memory, ample for most captured
    signals); `f64` is available per signal. Processing is always done in `f64` internally
    and narrowed on write — confirm that narrowing on every stage boundary is acceptable,
    or whether intermediate stages should stay `f64` end-to-end.
12. **Complex signals.** `c64` is in the dtype enum and `BasebandIq` is a domain, but full
    complex support (complex-aware pyramids, constellation rendering) is scheduled for
    V1.x. Confirm whether I/Q pairs arrive as two real signals or as one complex one — this
    affects M2, not just the renderer.
13. **Library scale.** Design targets 10 000 signals / ~100 GB of samples, plus run
    storage. An order of magnitude beyond that would argue for a columnar store.

---

## 18. Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| A later file shape differs from `sample/sample.csv` (§17.1–17.4) | Rework of `sp-csv` | Grammar confirmed against a real file; every framing constant (preamble length, count column, time column and unit, delimiter) is an import-profile setting rather than a literal |
| Per-pulse annotation demand outgrows the lazy `pulse` table (§17.5) | Model rework at M2+ | Identity is `(group, index)` either way, so materialising rows later is an additive migration, not a re-import |
| Intermediate-result storage grows unbounded | Library bloats, disk fills | Content-addressed dedup makes passthrough free; per-stage retention policy; run-level size cap; prune-old-runs maintenance action |
| Linear pipeline too restrictive for real algorithms | Model rework at M5+ | Typed ports cover the common "needs an earlier artifact" case; `Stage` trait is already graph-ready; settle §17.6 before M5 |
| Stage authors write non-deterministic stages | G8 silently false | Conformance harness runs every stage twice and compares output hashes; impure stages must opt out explicitly and are excluded from caching |
| A user algorithm panics or hangs | Run lost, app unstable | `catch_unwind` per stage invocation; per-stage timeout; failure isolated to one group |
| Iced canvas performance at 8+ dense traces plus overlays | Misses G2 | Pyramid decimation caps draw cost at viewport width; layered caches; `wgpu` backend; fall back to instanced GPU line rendering if needed |
| Iced API churn between releases | Build breakage | Pin the minor version; isolate all Iced usage in `sp-app` |
| Blob store and SQLite drifting out of sync after a crash | Orphaned or missing samples | Write blob first, then commit the row; `Verify Library` reconciles both directions |
| Scope creep from §15 | M1–M8 slip | Tiers are contractual: nothing beyond **[MVP]** enters v1 without cutting something else |
