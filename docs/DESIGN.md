# SignalPlayback — Design Document

**Status:** v1.4
**Date:** 2026-09-15
**Author:** Justin Perkey
**Repository:** `d:\Repos\SignalPlayback`

> **Changes in v1.4** — M11 is built, so §9.8's "planned" table loses the three
> families it named and §10.1's "built so far" gains the artifacts they emit:
> **peak find** (`Peaks`), **slice**, **symbol decode** (`Symbols`), **bit pack**
> (`Bits`) and **pulse metrics** (`Metrics`). §14 stops saying the stage
> conformance harness is not built and says what it checks; §9.9 records that the
> reference library declares itself impure, which is what the harness found the
> first time it was pointed at it; §16 gains an M11 row and §16.1 marks the
> milestone done; §15.3 moves the entries M11 delivered; and §18's
> non-deterministic-stage row is no longer half-mitigated.
>
> **Changes in v1.3** — An audit rather than a milestone: every entry in §16 was re-checked
> against the code, and where this document described an intention the entry now describes
> what exists. §3 no longer names `csv`, `rustfft` or `realfft` — the block parser and the
> FFT are both hand-rolled and neither dependency is in the workspace. §4.1 is the crate
> tree as it stands and §4.2 names the cancellation flag that was actually written. §5.2
> carries schema version 5, the train index and the derived-run columns; §9.6 gains
> `stage_cache` and the columns the migrations added; §9.2 gains `cache_salt` and `pure`.
> §9.8, §10.1 and §9.9's opening separate what is built from what the families still plan.
> §11.4, §12.1, §12.2 and §12.3 say what the screens do today. §13 drops a benchmark suite
> that was never written and records the G2 measurement that was; §14 stops crediting a
> conformance harness, a per-stage `catch_unwind` and a verify-on-read that do not exist,
> and says what the 903 tests do cover. §15 marks what has shipped and §16.1 follows it —
> most of M12's generation work was already built during M3 — and §17 is renumbered, with
> question 7 reopened because the scheduler was written without the flag it asked for.
>
> **Changes in v1.2** — M10 is built, so §9.5 now says what the cache and the retention
> policies actually do: `on_failure` output is written as the stage records and swept when
> the group comes out well, because a group's fate is not settled until its last stage has
> run; only `always` output is published to the cache, since a key must point at samples
> that are still there; a stale key is unpublished on the lookup that finds it dangling;
> and the run-level size cap is a claim per stage that leaves the account of what a stage
> did intact. §12.4 gains the sample cap, §12.1 the Settings screen's cache figure and
> `Clear stage cache`, §15.6 the CLI's `--sample-cap`, and open question 10 is settled.
>
> **Changes in v1.1** — M9 is built, so §9.9 now describes what exists rather than what was
> proposed: the disposition array is per input signal and `sp_output` carries its count,
> the host lends `f64`, provenance is a per-stage diagnostic, and a `sequential` library
> gets mutual exclusion rather than ordered delivery. §9.9 gains a "Settled during M9"
> table, §4.1 the real `sp-ext` layout and `sp-ext-sample`, §12.4 the allowed-library
> setting, and open question 8 is no longer a blocker.
>
> **Changes in v1.0** — First non-draft revision. M0–M8 are built, so the **[MVP]** scope in
> §15.3 is what v1 ships and the milestone table in §16 is closed; work from here is the
> **[V1.x]** track, tracked as post-v1 milestones in §16.1. §9.9 added: an **external
> stage** that hands one group at a time to a native library over a flat C ABI and reads
> its output back as an ordinary `StageOutput`. `sp-ext` added to §4.1, the feature listed
> under V1.x in §15.3, and the ABI shape raised as open question 8.
>
> **Changes in v0.6** — Written while building M8. §12.4 added for the settings file and
> what each setting does; §12.1 says what the Inspector computes and how the Runs screen
> hands a run to Results; §7.5 covers the command-line export and why only an imported
> dataset can be exported.
>
> **Changes in v0.5** — Schema corrections found while building M1: `sample_chunk` keeps
> its rowid because `sqlite3_blob_open` cannot address a `WITHOUT ROWID` table; `signal`
> gains `nan_count` so cached statistics reload with their sums intact; `dataset` gains
> `attributes` to match the domain model. §5.2 and §5.3 updated.
>
> **Changes in v0.4** — The library is now a **single SQLite file**: column data is stored
> as chunked BLOBs inside `library.db` rather than as `.sigbin` files beside it. §5.1, §5.3
> and §5.4 rewritten, §3 and §3.1 updated, and the blob/SQLite desync risk is gone.
>
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
| G4 | Single-file everything — one executable, and one `library.db` holding metadata *and* sample data. No server, no external database process, no companion directory to keep in step. |
| G5 | Every long-running operation (import, generation, processing, export) is cancellable and reports progress without blocking the UI. |
| G6 | The whole library is inspectable and repairable with standard tooling: `sqlite3` reaches every table and every byte of column data. |
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
| Sample store | **Chunked SQLite BLOBs** read with incremental blob I/O (`sqlite3_blob_open`) | Keeps the library a single file (G4) and every byte reachable from `sqlite3` (G6). Chunking sidesteps SQLite's 1 GB blob ceiling and bounds WAL churn; incremental I/O reads a slice without materialising the whole column. Content addressing still makes pipeline passthrough free. |
| CSV | A hand-written block framer and field decoder (`sp-csv`, no external dependency) | The grammar is a fixed preamble, file-level headers and count-driven framing (§7.2), which is not what a general CSV reader is for; quoting, embedded delimiters, BOMs and lone `\r` are handled in `parse.rs` and pinned by the fixture corpus. |
| Serialization | `serde` + `serde_json` | Generator specs, stage parameters, artifact payloads and property values. |
| Hashing | `blake3` | Content addressing, integrity checks, and pipeline cache keys. |
| RNG | `rand` + `rand_chacha` | `ChaCha12Rng` is portable and deterministic across platforms and versions — required by G3. |
| DSP | None — `sp-dsp` has no dependency beyond `sp-core`, `sp-proc` and `serde` | The FFT stage is an iterative radix-2 Cooley-Tukey in `transform.rs` and the filters are hand-rolled biquads. A library is worth taking on when a stage needs a transform size or a speed this does not reach; until then the stages stay readable and the dependency list stays short. |
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
- **External blob files with mmap** (`.sigbin` beside `library.db`) — the fastest option:
  zero-copy slice reads, no write amplification through the WAL. Rejected because it makes
  the library a directory rather than a file, and puts the sample bytes outside anything
  `sqlite3` can see. It also introduces the one failure mode a single file cannot have —
  a blob and its row disagreeing after a crash. The cost of the choice is one memcpy per
  chunk read and a heavier import; both are bounded and measured in §13.
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
├── sample/                    # sample.csv — the file the §7 grammar was settled against
└── crates/
    ├── sp-core/               # Domain vocabulary. No IO, no UI.
    │   ├── signal.rs          #   Signal, SignalId, SampleBuffer, DType, Domain
    │   ├── group.rs           #   SignalGroup, SignalTrain, Dataset
    │   ├── time.rs            #   TimeRange, SampleRange, Timebase
    │   ├── props.rs           #   PropertyDef, PropKind, Attributes
    │   ├── pulse.rs           #   PulseRef, PulseField, pulse-record vocabulary
    │   ├── run.rs             #   Run/stage/assertion status, Disposition, Retention
    │   ├── artifact/          #   Artifact trait, ArtifactSchema, ViewHint (mod.rs), the
    │   │                      #   decoded columnar payload (data.rs), the kind registry
    │   └── stats.rs           #   Streaming min/max/mean/rms/σ, zero crossings, histogram
    ├── sp-store/              # Persistence: SQLite schema, migrations, blob store.
    │   ├── schema/            #   NNNN_name.sql migration files, embedded (§5.2)
    │   ├── db.rs              #   Connections, the migrator, the writer actor
    │   ├── blob.rs            #   Chunked BLOB read/write, incremental I/O, checksums
    │   ├── library.rs         #   Dataset/train/group/signal rows, search, tags
    │   ├── props.rs           #   Property definitions and the indexed mirror
    │   ├── pulses.rs          #   Column read/scan, zone-map prefilter, pulse search
    │   ├── trains.rs          #   Trains and the groups under them
    │   ├── pyramid.rs         #   The render-pyramid index (§5.4)
    │   ├── runs.rs            #   Run/stage/signal/artifact recording; the stage cache
    │   ├── regress.rs         #   Baselines and recorded assertion outcomes
    │   ├── profiles.rs        #   Saved import profiles
    │   ├── stats.rs           #   What the library is made of, for the Settings screen
    │   └── verify.rs          #   Rehash every blob, reconcile references (§14)
    ├── sp-csv/                # Block-grammar parser/writer + import profiles.
    │   ├── framer.rs          #   Count-driven block framing
    │   ├── parse.rs           #   Header/row decoding, error positions
    │   ├── sniff.rs           #   A bounded first pass: delimiter, preamble, a proposal
    │   ├── profile.rs         #   Preamble/delimiter/count/time settings, column mapping
    │   ├── ingest.rs          #   Streaming file → chunked columns → library
    │   ├── diag.rs            #   The error list an import shows instead of a dialog
    │   ├── control.rs         #   Progress and cancellation for a long import
    │   └── export.rs          #   Database → CSV, reversing the grammar (§7.5)
    ├── sp-gen/                # Parametric synthesis.
    │   ├── spec.rs            #   GenSpec tree (serde), Node, NodeKind
    │   ├── tree.rs            #   JSON-pointer addressing of nodes and parameters
    │   ├── waveform.rs        #   Primitives: oscillators, chirp, pulse, step, ramp
    │   ├── noise.rs           #   Gaussian / uniform / pink / brown / PRBS, all seekable
    │   ├── expr.rs            #   The f(t) node: shunting-yard compiler + stack machine
    │   ├── render.rs          #   Spec → SampleBuffer, AM/FM/PM and Resample included
    │   ├── generate.rs        #   Render a spec, or a sweep, into the library
    │   ├── sweep.rs           #   One swept parameter → a group of signals
    │   ├── train.rs           #   Pulse-train generation (§8.5)
    │   ├── preset.rs          #   The built-in preset library, load/save
    │   ├── validate.rs        #   Nyquist, duty, negative frequency — up front
    │   └── control.rs         #   Progress and cancellation
    ├── sp-proc/               # Pipeline orchestration. Knows nothing about DSP.
    │   ├── stage.rs           #   Stage trait, StageDescriptor, ports, StageOutput
    │   ├── param.rs           #   ParamSpec/ParamSet: the generated parameter form
    │   ├── frame.rs           #   GroupFrame, SignalRef, applying a stage's output
    │   ├── registry.rs        #   StageRegistry: kind → constructor + descriptor
    │   ├── pipeline.rs        #   Pipeline model, port validation
    │   ├── scheduler.rs       #   Group-at-a-time execution, cancellation, progress
    │   ├── cache.rs           #   Content-hash keyed stage-output reuse (§9.5)
    │   ├── assert.rs          #   The assertion grammar and its evaluation (§9.7)
    │   ├── compare.rs         #   Run-vs-run and run-vs-baseline diffing
    │   └── conform.rs         #   The contract any Stage impl is held to (§14)
    ├── sp-ext/                # External stages: native-library loading over the §9.9 C ABI.
    │   ├── abi.rs             #   Flat C structs, symbol names, version negotiation
    │   ├── descriptor.rs      #   The library's published JSON -> StageDescriptor
    │   ├── allow.rs           #   The paths this installation may load
    │   ├── library.rs         #   Load, resolve, hash; one loaded file
    │   └── stage.rs           #   Marshal a group in, read an output back
    ├── sp-ext-sample/         # A conforming library, built as a cdylib: the ABI's
    │                          # reference implementation, and what sp-ext's tests load.
    ├── sp-dsp/                # Built-in Stage implementations. Depends on sp-proc.
    │   ├── condition.rs       #   Gain, detrend (mean/linear), normalise (peak/RMS)
    │   ├── filter.rs          #   IIR biquad — low / high / band / notch
    │   ├── transform.rs       #   FFT → Spectrum, and the windows it applies
    │   ├── measure.rs         #   Statistics, and pulse metrics over a detector's spans
    │   ├── detect.rs          #   Threshold → Detections; peak find → Peaks
    │   ├── digital.rs         #   Slice → logic, symbol decode → Symbols, bit pack → Bits
    │   ├── artifacts.rs       #   The artifact types those stages publish
    │   └── util.rs            #   Passthrough: a labelled inspection point
    ├── sp-engine/             # Playback clock, transport, render pyramids.
    │   ├── transport.rs       #   State machine: Stopped/Playing/Paused, loop modes
    │   ├── clock.rs           #   Monotonic virtual time, rate scaling
    │   ├── pyramid.rs         #   Multi-resolution min/max mipmaps
    │   ├── source.rs          #   A column plus the pyramid over it, built on demand
    │   ├── reduce.rs          #   Column + viewport → a drawable snapshot
    │   ├── compare.rs         #   The residual trace between two stages' signals
    │   └── viewport.rs        #   Time/amplitude window, follow modes
    └── sp-app/                # Iced binary. The only crate that knows about pixels.
        ├── main.rs            #   A window, or a subcommand
        ├── cli.rs             #   The headless command line (§10.4)
        ├── state.rs           #   App state, the root Message, the key bindings
        ├── jobs.rs            #   Store work, off the UI thread
        ├── stages.rs          #   The registry: sp-dsp plus the allowed external libraries
        ├── settings.rs        #   settings.json and its defaults (§12.4)
        ├── paths.rs           #   Platform-correct library, settings and log locations
        ├── logging.rs         #   The rolling file log, plus stderr
        ├── ui.rs · theme.rs · typography.rs  #  The design system every screen draws from
        ├── screens/           #   library, inspector, properties, import, generate,
        │                      #   pipeline, runs, results, scope, settings
        └── widgets/
            ├── scope.rs       #   The scope's canvas::Program
            ├── panes.rs       #   ViewHint → an artifact pane: table, chart, scalars, tree
            ├── histogram.rs   #   The Inspector's distribution
            └── glyph.rs       #   The marks the icon font cannot draw
```

**Dependency rule:** dependencies point left-to-right only. `sp-core` depends on nothing
in the workspace; `sp-dsp` depends on `sp-proc` and `sp-core` only; `sp-app` may depend on
everything; no crate depends on `sp-app`.

**Why `sp-proc` and `sp-dsp` are separate.** The orchestration layer must not know what a
Butterworth filter is. Keeping them apart is what makes G9 true — and it is the seam a
plugin interface would later slot into. M9 went through that seam: `sp-ext` sits beside
`sp-dsp`, depends on the same two crates, and registers stages the scheduler cannot tell
apart from compiled ones.

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
│ Tokio blocking pool — one task per job, none on the UI thread│
│   • CSV import   (streaming, chunked, cancellable)           │
│   • Generation   (rayon fan-out over sample ranges)          │
│   • Pipeline run (a rayon pool of in_flight_cap over GROUPS; │
│                   stages within a group strictly in order)   │
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
- **Cancellation** via a shared `AtomicBool` — `RunControl` for a run, an equivalent
  control for import and generation — checked at chunk boundaries (a block of samples, a
  block of CSV rows, or between stages), so cancel latency stays under ~50 ms. A flag
  rather than tokio's `CancellationToken`, because what has to notice it is synchronous
  rayon work rather than a future.

---

## 5. Data Model

### 5.1 Library Layout on Disk

A library is **one file**. Metadata, sample and pulse-field columns, render pyramids and
artifact payloads all live in it; the `-wal` and `-shm` files are SQLite's own and exist
only while the library is open or after an unclean shutdown.

```
my-library.db             # everything: metadata, columns, pyramids, artifacts
my-library.db-wal         # SQLite write-ahead log (transient)
my-library.db-shm         # SQLite shared-memory index (transient)
```

Default: `%LOCALAPPDATA%\SignalPlayback\library\library.db` on Windows, via `directories`.
The path is user-selectable; multiple libraries are supported (one open at a time in v1).
Backing a library up, or handing one to a colleague, is copying a single file — the point
of the choice.

**Pragmas.**

```sql
PRAGMA journal_mode = WAL;      -- one writer, many readers, no SQLITE_BUSY dance
PRAGMA synchronous  = NORMAL;   -- WAL makes this safe against process crashes
PRAGMA foreign_keys = ON;
PRAGMA page_size    = 8192;     -- fewer pages per MB of column data
PRAGMA wal_autocheckpoint = 4000;  -- ~32 MB, so bulk import checkpoints steadily
PRAGMA temp_store   = MEMORY;
```

The original CSV is **not** archived by default — re-importing costs one pass, and copying
a multi-GB file into the library to sit unread is a poor trade. A per-import "keep a copy
of the source" option stores it as a compressed blob when the provenance matters.

### 5.2 Core Schema

```sql
-- Connection pragmas are in §5.1.

CREATE TABLE app_meta (
    key    TEXT PRIMARY KEY,
    value  TEXT NOT NULL
);  -- ('schema_version','5') as of migration 0005

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
    attributes    TEXT    NOT NULL DEFAULT '{}'   -- JSON property values
);

-- One capture: everything a source file carries. One imported file is one
-- train, and the groups under it are its segments, not separate recordings
-- (§6.6).
CREATE TABLE signal_train (
    id          INTEGER PRIMARY KEY,
    dataset_id  INTEGER NOT NULL REFERENCES dataset(id) ON DELETE CASCADE,
    ordinal     INTEGER NOT NULL,
    name        TEXT,
    toa_unit    TEXT,                      -- set for a train of pulse records
    attributes  TEXT    NOT NULL DEFAULT '{}',
    UNIQUE (dataset_id, ordinal)
);

-- One block within a train: a dwell, a scan, a generation batch. The group is
-- the unit of processing (§9).
CREATE TABLE signal_group (
    id              INTEGER PRIMARY KEY,
    train_id        INTEGER NOT NULL REFERENCES signal_train(id) ON DELETE CASCADE,
    ordinal         INTEGER NOT NULL,
    name            TEXT,
    declared_count  INTEGER NOT NULL,      -- the 'count' field from the group header
    actual_count    INTEGER NOT NULL,      -- pulse rows actually read
    -- Pulse groups (§6.6): the TOA column, shared by every pulse_field.
    toa_blob_id     INTEGER REFERENCES sample_blob(id),
    toa_unit        TEXT,                  -- source unit, e.g. 'us'; NULL for sampled groups
    attributes      TEXT    NOT NULL DEFAULT '{}',   -- JSON property values
    UNIQUE (train_id, ordinal)
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
    nan_count      INTEGER NOT NULL DEFAULT 0,      -- samples excluded from the stats
    attributes     TEXT    NOT NULL DEFAULT '{}',    -- JSON property values
    UNIQUE (group_id, ordinal)
);

-- An immutable, content-addressed byte array: a signal's samples, a pulse
-- field's column, a TOA column, a render pyramid, or a large artifact payload.
-- The bytes live in sample_chunk (§5.3), never on the filesystem.
CREATE TABLE sample_blob (
    id         INTEGER PRIMARY KEY,
    checksum   TEXT    NOT NULL UNIQUE,     -- blake3 hex; this is the address
    byte_len   INTEGER NOT NULL,            -- total payload length across chunks
    chunk_size INTEGER NOT NULL,            -- bytes per chunk, last one may be short
    kind       TEXT    NOT NULL             -- what the bytes are, for Verify/maintenance
               CHECK (kind IN ('samples','pyramid','artifact')),
    refcount   INTEGER NOT NULL DEFAULT 0   -- identical columns share one blob
);

-- Blob payload, split so no single BLOB approaches SQLite's 1 GB ceiling and
-- so a bulk import checkpoints the WAL at a predictable rate. Keeps its rowid:
-- incremental blob I/O addresses a cell by rowid and cannot open one in a
-- WITHOUT ROWID table.
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
CREATE INDEX ix_train_dataset ON signal_train(dataset_id);
CREATE INDEX ix_group_train   ON signal_group(train_id);
CREATE INDEX ix_signal_name   ON signal(name);

-- External-content FTS5, kept in step with `signal` by insert/update/delete
-- triggers: the index is derived, so it never holds a row the table does not.
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

Property definitions and the indexed mirror of `attributes` are in §6.3, the render-pyramid
index in §5.4, and everything a run records in §9.6. Migration 0004 also adds
`signal.derived_run_id` and `signal.derived_stage_ordinal`, which are what make the
`derived` provenance token (§6.1) traceable back to the stage that produced the signal.

**Migrations.** Numbered SQL files (`0001_init.sql`, `0002_…`) embedded in the binary and
applied in order inside a transaction, gated on `app_meta.schema_version`. Downgrades are
refused with a clear message rather than attempted. Five are applied as of v1.2: the initial
schema, the signal train (§6.6), the pyramid index (§5.4), runs (§9.6) and regression
(§9.7). A migration that has to restructure a table rebuilds it the way SQLite's own
procedure prescribes — 0002 does, because `signal_group` carried a UNIQUE constraint on a
column that had to go — so the ids other tables reference survive.

### 5.3 Column Blob Format

A blob's logical payload is a fixed 64-byte header followed by packed little-endian
values. It is the same layout whether the bytes hold a signal's samples, a pulse field's
column, or a group's TOA column.

| Offset | Size | Field |
|--------|------|-------|
| 0  | 4 | Magic `SGB1` |
| 4  | 2 | Format version (u16) |
| 6  | 1 | DType (0=f32, 1=f64, 2=i16, 3=i32, 4=complex64, 5=u8) |
| 7  | 1 | Channel count (u8), 1 for v1 |
| 8  | 8 | Sample rate Hz (f64, 0.0 ⇒ irregular) |
| 16 | 8 | t0 seconds (f64) |
| 24 | 8 | Value count (u64) |
| 32 | 8 | Scale factor (f64, for integer dtypes) |
| 40 | 8 | Offset (f64, for integer dtypes) |
| 48 | 16 | Reserved (zeroed) |
| 64 | … | Value payload |

**Stored as chunks.** That byte array is split across `sample_chunk` rows of a fixed
`chunk_size` (**4 MiB** by default, the last chunk short). Chunking buys three things:
SQLite's 1 GB per-BLOB ceiling never applies, a bulk import checkpoints the WAL at a
steady rate instead of growing it by the size of the whole column, and a random slice read
touches one chunk rather than deserialising the column.

**Reading.** `sqlite3_blob_open` (rusqlite's `Connection::blob_open`) gives `Read + Seek`
over one chunk's BLOB without loading it, so reading samples `[i, j)` costs one `memcpy`
of exactly that span out of the chunks it falls in. This is the concrete price of a
single-file library: one copy per read where an mmap'd file would have had none.

**Writing.** Chunks are appended inside the import transaction with a zero-blob-then-fill
pattern (`INSERT … zeroblob(n)`, then `blob_open` and write), which keeps a 4 MiB chunk
from ever being materialised twice in memory.

Blobs are immutable and content-addressed by blake3 over the payload. A write streams
into a row carrying a placeholder address and, on completion, either takes its blake3
address or — if a blob with that address already exists — discards its chunks and bumps
the existing blob's `refcount`. Editing a signal writes a new blob and releases the old
one; a blob is deleted, chunks included, when its last reference goes, and `VACUUM`
returns the pages. `Verify Library` (§14) reconciles `refcount` against the rows that
actually reference each blob.

**This is what makes storing every intermediate stage affordable.** A stage that passes a
signal through unchanged produces the same content hash and therefore the same blob — the
run records a reference, not a copy. Only signals a stage actually altered cost storage.

### 5.4 Render Pyramid

Rendering 100 M points per frame is not feasible, so each blob gets a derived
multi-resolution min/max pyramid, itself stored as a blob (`kind = 'pyramid'`) keyed by
the source blob's checksum:

- Level *k* stores one `(min, max)` `f32` pair per `2^(k+6)` source values — level 0 is a
  64:1 reduction, each level halves again. Level 0 adds ~3% to an `f32` source and the
  levels above it sum to one more of the same, so the whole pyramid costs ~6%.
- Levels are built once, lazily, on the worker pool and cached in the library. Measured at
  M4 and re-measured at v1.3: **~0.8 s per 100 M samples** in a release build, which is the
  streaming read of the column out of its chunks plus one linear fold; the build is a
  background job, never on the frame path.
- The renderer picks the level where **samples-per-pixel lands in [1, 2]** and reads that
  slice — a few KB, one chunk — then draws vertical min/max bars.
- Below one sample per pixel the renderer reads the raw span and draws a polyline with
  point markers.

Draw cost is proportional to viewport width in pixels, not to signal length — the key to
G2, and the reason flipping between stage outputs stays instant. Because a pyramid read is
kilobytes, the extra copy imposed by BLOB storage never lands on the frame path.

Pyramids are derived data: deleting every `kind = 'pyramid'` blob is always safe and is
what the *Rebuild pyramids* maintenance action does.

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
of them.

**Groups are not independent — they are segments of a train.** One signal train resolves to
several groups, and those groups are one capture: a file's blocks are its dwells or scans,
not separate recordings. A `signal_train` therefore sits between a dataset and its groups,
and **one imported file is one train**. Generation produces a train too (§8.5), so an
imported and a generated capture are the same shape and are interchangeable as pipeline
input.

The train is what a capture is named, listed and reasoned about as; the group stays the
unit of *processing* (§9.1), and a stage that needs the rest of the capture reaches it
through `GroupMeta::train_id`. Searching for pulses across groups remains a first-class
operation and is unchanged by the train level.

```
dataset ── train ─┬─ group 0 ── toa[] + one column per field
                  ├─ group 1 ── …
                  └─ group 2 ── …
```

Two views of the same bytes serve those two needs.

**Stored as columns.** Each field of a group becomes one array — a `pulse_field` backed by
an ordinary content-addressed blob inside `library.db` — and the group's TOA column becomes the shared
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

**Why a train row rather than one column per field spanning the whole capture.** Storing a
train as a single pair of columns with groups as `(start, count)` spans would make a
whole-capture scan one read. It was rejected for now: a group's columns are already
content-addressed blobs that dedupe and stream independently, ingest can flush at each
group boundary with one group in memory (§7.4), and a group stays independently
re-importable. The train row buys the missing relationship at the cost of one join, and
leaves the column layout — the part that is expensive to change — alone.

**Cross-group search.** Each `pulse_field` records min/max — a zone map — so a predicate
such as `pulse_width < 2 AND angle BETWEEN 30 AND 40` first eliminates whole groups in
SQL, then reads only the surviving groups' columns chunk by chunk, in parallel, returning
`PulseRef` hits. A 4-byte-per-value scan runs at roughly memory bandwidth even with the
chunk copy in the way, so a selective query over a 10 000-group library touches a few
hundred MB at most.

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
| **Numeric storage** | Pulse columns default to `f64`, not the `f32` of §17.14: G1 asks for a lossless round trip and a decimal that survives `f32` is the exception. Narrowing a column to `f32` or `i32` halves or quarters its storage and is a per-column choice in the mapping UI. The sniff pass *offers* a narrowing it can see is safe but never applies one, because it has read only the first group. |
| **Text columns** | A text field array is stored the way every other column is — as a numeric blob — by dictionary-encoding it: the column holds `i32` indices and the group carries the distinct cells in `csv_text.<key>`. Export restores the original spelling, so the round trip is lossless; the column's zone map orders by first appearance rather than by value, which is why a text column is not searchable by range. |
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

An import writes **one train** and hangs every group it frames off it (§6.6); export walks
the dataset's trains in order and then each train's groups, which is what makes the round
trip a fixed point.

### 7.5 Export

The writer reverses the grammar exactly — same preamble, same header rows, same column
order, same delimiter, TOA rescaled back to its source unit — satisfying G1. Group
properties and pulse fields that were never mapped to a definition are written back from
`attributes` verbatim. A round-trip test fixture set lives in
`crates/sp-csv/tests/fixtures/`, seeded from `sample/sample.csv`.

Export is reachable two ways, and both call the same writer: the Library screen's
`Export` on a dataset, and `signalplayback export --dataset <name|#id> --out <path>` on
the command line. `signalplayback import --file <path>` is the other half, reading a file
with the profile the sniff pass proposes — no mapping is invented on the terminal, since a
mapping is a decision — so import → run → export is scriptable without the window. Only a
dataset that was imported can be exported: the layout it was read with is stored on the
dataset (`csv_layout`) and is what the writer reverses, so a generated or derived dataset
has nothing to reverse and the action is refused rather than guessed at.

---

## 8. Signal Generation

### 8.1 Spec Model

A generated signal is described by a serialisable DAG, stored as JSON in `signal.gen_spec`.
Samples are a *cache* of the spec — deleting a blob is always safe.

```rust
pub struct GenSpec {
    pub timebase: Timebase,   // sample_rate_hz, t0_s
    pub duration_s: f64,      // a stored signal's length is its sample count,
                              //   so the duration is a field of the spec
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
    Expr      { source: String },              // f(t), t in seconds

    // Combinators
    Sum       { terms: Vec<Node> },
    Product   { terms: Vec<Node> },
    Concat    { parts: Vec<ConcatPart> },      // { node, duration_s }
    Gain      { input: Box<Node>, factor: f64 },
    Delay     { input: Box<Node>, by_s: f64 },
    Clip      { input: Box<Node>, lo: f64, hi: f64 },
    Envelope  { input: Box<Node>, env: EnvelopeSpec },  // ADSR | Gaussian | Tukey
    Modulate  { carrier: Box<Node>, modulator: Box<Node>, kind: ModKind },
                                               // Am{depth} | Fm{dev_hz} | Pm{dev_rad}
    Awgn      { input: Box<Node>, snr_db: f64 },  // Gaussian noise at a target SNR,
                                               //   measured against the input's own power
    Resample  { input: Box<Node>, to_rate_hz: f64 },
    FromSignal{ signal_id: SignalId },         // reference a stored signal as a source
}
```

Every node is addressed by its **JSON pointer** into the serialised spec — `/root`,
`/root/terms/1`, `/root/carrier`, `/root/parts/0/node`. One address serves the tree
editor's selection, a validation issue, the parameter form and a sweep target, which is
why `Concat` holds a named struct rather than a tuple: a tuple has no field to point at.

**Settled during M3.**

| Decision | Why |
|----------|-----|
| **Node time** starts at the signal, not at the timeline: a node sees `t = index / fs`, and `t0_s` only places the finished signal. | Moving a signal in time must not change its samples. |
| **`Expr` is evaluated by a hand-rolled shunting-yard compiler**, not `meval`. | `meval` 0.2 pulls `nom` 1.2 (2016), which `cargo` already reports as future-incompatible. Compiling once to RPN also makes the per-sample cost a stack machine rather than a tree walk. |
| **FM and PM require an oscillator carrier** (`Sine`/`Square`/`Triangle`/`Sawtooth`), and FM requires a modulator with a closed-form integral. AM accepts any carrier. | Both rewrite the carrier's *phase*, which only a periodic primitive has. Anything else is rejected up front with that as the fix. |
| **`Delay` shifts by whole samples.** | A sub-sample shift needs interpolation, and `Resample` is the node that owns interpolation. |
| **`Resample` only ever lowers the rate.** | Above the output rate there is nothing to reconstruct; below it is what models a slower converter. Validation warns rather than blocking. |
| **`serde_json`'s `float_roundtrip` feature is required.** | Without it the parser can return an `f64` a bit out from the one written, which would break both the stored `gen_spec` re-rendering identically (G3) and an attribute surviving export unchanged (G1). |

**Settled during M12.**

| Decision | Why |
|----------|-----|
| **`Awgn` measures its input rather than taking an amplitude**, and measures the noise stream too, so the *achieved* ratio is the requested one whatever the stream's nominal variance. | A rung of a ladder is a signal-to-noise ratio, not a noise amplitude; an amplitude that means a different SNR for every source is not a rung. |
| **The measurement is over a bounded probe window fixed by the node's own grid**: the whole signal when it is short, otherwise sixteen blocks of 4 096 samples spread evenly from its first sample to its last. | Power is a property of the whole signal, and §8.2 requires a node to be a function of its own index — so the window it measures over cannot be the window being rendered. Bounding it keeps a ladder over a 100 M-sample source from costing a second render of it; spreading it keeps a chirp's power from being read off its opening. |
| **Against silence no noise is added.** | There is no ratio to hit, and any amplitude would be arbitrary. Validation warns where it can see it coming. |
| **One probe per node per render**, memoised by the node's pointer. | The value is the same whichever chunk asks for it, so the first chunk to need it pays for it and the rest read it — a ladder costs one extra probe, not one per chunk. |

### 8.2 Rendering

- Pure function `render(&GenSpec, range: SampleRange) -> SampleBuffer`, so any window can
  be produced independently. Rayon splits the range across cores.
- Noise nodes derive a per-node stream from `seed` by hashing `(seed, node_path)` — a
  range rendered in parallel chunks matches a range rendered serially, and adding a node
  elsewhere in the tree does not perturb an existing node's noise.
- Node parameter validation happens up front: negative frequencies, duty outside (0,1),
  Nyquist violations (`freq_hz ≥ fs/2`) become blocking errors with a fix suggestion.

Independence is what everything else follows from: every source is a **function of the
sample index** rather than a running state.

- `ChaCha12Rng` is seekable, so the draw for sample *i* sits at a fixed word position and
  a chunk starting at *i* seeks there. The key is `blake3(seed ‖ node pointer)`.
- Pink and brown noise are Voss-McCartney octave sums, `white(k, i >> k)`, which are
  addressable. An exact `1/f²` random walk is a prefix sum and is not; window-independent
  rendering is the harder constraint, so the octave sum is what ships.
- A PRBS is a linear recurrence, so its state at index *i* is `Mⁱ · s₀` over GF(2);
  repeated squaring reaches any index in `O(log i)`.
- `Concat` gives each part its own origin and duration, and `Resample` renders its input
  on the resampled grid. Validation follows the same two rules, so a Nyquist message
  about a node under a `Resample` names that node's own grid.

"Bit-identical" (G3) means: the same spec and seed on the same build produce the same
bits, and a chunked render equals a whole one. Cross-platform bit equality is not claimed
and cannot be — `sin`, `ln` and `cos` are not bit-specified by IEEE 754.

### 8.3 Generator UI

- Left: a node tree with add/remove/reorder. Right: parameters for the selected node.
- A **live preview** strip renders the first ~2 seconds on every keystroke (debounced
  150 ms), so the shape is visible before committing. It renders at the real rate and
  *then* reduces to one min/max pair per column, rather than rendering at a reduced rate:
  a lower rate would alias the preview into showing something the signal does not do.
- **Batch/sweep mode**: mark one numeric parameter as swept (`start`, `stop`, `step`, or an
  explicit list) to emit a whole group of signals in one action — e.g. 20 sine waves from
  100 Hz to 2 kHz. The sweep becomes a `signal_group` with the swept value stored as a
  property, which is exactly the shape a pipeline wants as test input.
- **Layout**: the same rungs write one of two shapes — one group holding a signal per rung,
  or one group *per* rung, which is the impairment ladder of §8.4. The choice is a field of
  the request rather than of the sweep, because it changes nothing about what is rendered.
- Presets are `GenSpec` JSON files; a small built-in library ships with the app.

The parameter form and the sweep target picker are both generated from the spec's
serialised fields rather than from a match over `Node`, so a node variant added later is
editable and sweepable with no UI code. A sweep substitutes through the same JSON
pointer, and declares its property in `property_def` so the swept value shows up as a
typed column rather than as an unrecognised attribute (§6.4).

A preset file is a `GenSpec` with a name and a description wrapped around it, so the
browser has something to list; a file holding a bare `GenSpec` loads too and takes its
name from the file.

### 8.4 Test-Vector Generation

Generation exists primarily to feed §9. Three patterns get first-class support:

- **Known-answer vectors** — a generated signal whose correct processing result is known
  analytically (a single tone at a known frequency, a pulse train with known edges). Stored
  alongside an *expectation* the pipeline can assert against (§9.7).
- **Impairment ladders** — one clean source rendered at a sweep of SNRs or clock offsets,
  producing a group per rung, so an algorithm's degradation curve falls out of one run.
  Built at M12: the `Awgn` node makes the rung a ratio rather than an amplitude, and the
  `GroupPerRung` layout gives each rung its own group — the unit a pipeline processes,
  caches, asserts over and charts against. Every group carries its rung as a declared
  group property and is named by it, so the chart across groups (§10.5) is plotted against
  the ladder rather than against a group's place in the list. Any numeric parameter is
  sweepable this way, so a clock-offset or duration ladder is the same machinery with a
  different pointer.
- **Preprocessed inputs** — generation can emit `digital_logic` or `symbols` signals
  directly, so a stage that expects an already-sliced input can be tested without first
  running the analog front end.

### 8.5 Pulse-Train Generation

§8.1 synthesises *sampled* signals. An import carries *pulse records* (§6.6), so a
generated waveform can never stand in for a captured train — different shape, different
viewer, different stage inputs. A `TrainSpec` is the other half of generation, and emits
exactly what an import does: a train, its groups, a time-of-arrival column and one column
per field.

```rust
pub struct TrainSpec {
    pub toa_unit: TimeUnit,   // as a file would express it; storage is seconds
    pub t0_s:     f64,
    pub groups:   u32,        // dwells, scans, blocks of a file
    pub pulses_per_group: u32,
    pub seed:     u64,        // makes the jitter and the random fields reproducible (G3)
    pub pri:      Pri,        // Fixed | Stagger{positions} | Jitter{fraction} | Drift{per_pulse}
    pub fields:   Vec<FieldSpec>,   // name, unit, and how the value varies
}

pub enum FieldValue {
    Constant{value}, Uniform{lo,hi}, Gaussian{mean,sigma},
    Ramp{start,end}, Sequence{values}, Scan{mean,amp,period_pulses},
}
```

Everything is a function of the **pulse index within the train**, for the same reason the
sample renderer is a function of the sample index: a group renders identically whether or
not the groups before it were rendered, and the same spec and seed reproduce the same
numbers (G3). The interval patterns all have closed forms — a stagger is whole cycles plus
a partial, a drift is an arithmetic series — and jitter dithers each arrival rather than
accumulating, so the train never walks away from its nominal PRF.

The generator screen carries both modes. The parameter form and the validator are the same
machinery in each: a spec is edited through JSON pointers into its serialised form, so a
train's `/pri/mode` and a node's `/root/env/shape` are handled by one code path.

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

    /// Extra identity this instance adds to its cache key, beyond the kind,
    /// version and parameters the descriptor already accounts for. A compiled
    /// stage has none; an external one returns its library file's hash (§9.9).
    fn cache_salt(&self) -> Option<String> { None }

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
    pub pure:     bool,                // false opts the stage out of caching (§9.5)
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
    pub signals:  Vec<SignalRef>,   // lazy column handles; read by span, never copied whole
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
        key ← blake3(stage.kind, stage.version, params_hash, salt, input_hash)
        if cache.hit(key) and stage.pure:
            output ← cache.get(key)           # stage skipped entirely
        else:
            output ← stage.process(ctx, frame)
            cache.put(key, output)            # `always` retention only
        record(run, group, stage, output)     # blobs + artifact rows
        frame ← frame.apply(output)
    if group passed:                          # §9.5, retention
        forget the samples of every `on_failure` stage
```

- `input_hash` folds the content hashes of every input signal and inbound artifact, so the
  key changes exactly when the inputs do. `salt` is what the stage instance adds beyond its
  declaration — nothing for a compiled stage, the library file's hash for an external one
  (§9.9).
- **Editing stage 4's parameters re-runs stages 4…n only.** Stages 1–3 hit the cache. On a
  long pipeline this is the difference between iterating on an algorithm and waiting on one.
- Stages may declare themselves impure (`descriptor().pure == false`) to opt out of caching.
- A hit is **copied into this run**, sharing blobs rather than recomputing or referencing
  across runs, so a cached stage is indistinguishable from one that ran apart from its
  `cached` status — and deleting either run leaves the other whole. A key whose rows are
  gone is unpublished on the lookup that finds it dangling, rather than missed again.
- **Retention policy** per stage: `Always` / `OnFailure` / `Never`. Content addressing means
  unchanged signals never consume space regardless of policy.
  - `on_failure` is **written as the stage records and swept when the group comes out
    well**. A group's fate is not settled until its last stage has run, and the samples a
    failure is diagnosed from are exactly the ones that would already be gone. Sweeping is
    not deleting the row: what the stage did to each signal, its statistics, its metrics and
    its diagnostics stay — a group that passed is read for its numbers, not for its
    intermediate waveforms.
  - Only `always` output is **published to the cache**. A key must point at samples that are
    still there, and `on_failure` output is swept the moment its group passes.
- **Run-level size cap.** A stage claims room for the bytes it produced — a passthrough
  shares the blob it was given and claims nothing — and a stage that does not get it records
  everything except the samples, plus a diagnostic saying so. The claim is per stage rather
  than a latch, so one stage too large to fit does not stop a smaller one later, and a
  capped stage is not published to the cache either. The cap limits storage, never evidence.

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
    group_id INTEGER NOT NULL REFERENCES signal_group(id) ON DELETE CASCADE,
    status   TEXT NOT NULL,
    wall_ms  INTEGER,
    message  TEXT,                      -- why a failed group failed
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
    message       TEXT,                 -- the stage error, when it failed
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
    blob_id       INTEGER REFERENCES sample_blob(id),   -- NULL once swept (§9.5)
    sample_rate_hz REAL, t0_s REAL, sample_count INTEGER,
    -- Cached with the row, so a swept stage still answers for what it produced.
    min_value     REAL, max_value REAL, mean_value REAL, rms_value REAL,
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

-- Which recorded output a cache key resolves to (§9.5). Hanging it off the run
-- means a deleted run takes its cache entries with it, so a key never points at
-- rows that are gone — and only an `always` stage is ever published here.
CREATE TABLE stage_cache (
    cache_key     TEXT    PRIMARY KEY,
    run_id        INTEGER NOT NULL REFERENCES run(id) ON DELETE CASCADE,
    group_id      INTEGER NOT NULL,
    stage_ordinal INTEGER NOT NULL,
    created_utc   TEXT    NOT NULL
);
CREATE INDEX ix_stage_cache_run ON stage_cache(run_id);

-- A run promoted to golden, for regression comparison.
CREATE TABLE baseline (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    run_id      INTEGER NOT NULL REFERENCES run(id),
    created_utc TEXT NOT NULL,
    tolerance_json TEXT NOT NULL DEFAULT '{}'
);

-- Assertions belong to the pipeline as source text (§9.7) and are evaluated
-- per group; each outcome records the text it was evaluated from, so a run
-- says what it actually tested after the pipeline has been edited.
CREATE TABLE pipeline_assertion (
    pipeline_id INTEGER NOT NULL REFERENCES pipeline(id) ON DELETE CASCADE,
    ordinal     INTEGER NOT NULL,
    expression  TEXT    NOT NULL,      -- 'metrics.snr_db > 12.0'
    enabled     INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (pipeline_id, ordinal)
);
CREATE TABLE run_assertion (
    run_id     INTEGER NOT NULL REFERENCES run(id) ON DELETE CASCADE,
    group_id   INTEGER NOT NULL,
    ordinal    INTEGER NOT NULL,
    expression TEXT    NOT NULL,
    status     TEXT    NOT NULL
               CHECK (status IN ('pass','fail','not_applicable','error')),
    actual     REAL, expected REAL,
    message    TEXT,
    PRIMARY KEY (run_id, group_id, ordinal)
);
```

`stage_ordinal = -1` records the source signals as they entered the pipeline, so "before"
is a real row and not a special case in the UI.

A stage whose samples were swept by retention, or that did not fit under the run's sample
cap, keeps its `run_signal` row with `blob_id` NULL: the disposition, the timebase, the
sample count and the cached statistics are all still there, and only the waveform is gone
(§9.5).

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

Not-applicable is a first-class outcome rather than a quiet pass: an assertion about an
artifact a group never produced, or one comparing against a baseline when no baseline was
given, has nothing to say — and a suite whose subjects have stopped existing must not go
green. An assertion that *cannot* be evaluated — a baseline with no such value, a text
value under `<` — fails, for the same reason. An assertion judges a finished group, so a
group whose stages did not all run is not asked: it has already failed.

### 9.8 Built-in Stages (`sp-dsp`)

Enough to exercise the harness and cover common preprocessing, all implemented against the
same public trait a user's algorithm would use — nothing in `sp-dsp` is privileged, which
is what makes G9 testable rather than aspirational.

**Built.** Thirteen stage kinds, every one registered through the same `StageRegistry` a
plugin uses, and every one run through the conformance harness of §14 by
`sp-dsp/tests/conformance.rs`:

| Kind | Family | What it does |
|------|--------|--------------|
| `dsp.util.passthrough` | Utility | A labelled inspection point: shares its input's blob and claims no storage |
| `dsp.condition.gain` | Conditioning | Scalar gain |
| `dsp.condition.detrend` | Conditioning | Removes the mean or a least-squares line |
| `dsp.condition.normalise` | Conditioning | To unit peak or unit RMS |
| `dsp.filter.biquad` | Filtering | Low / high / band / notch, as a cascade of identical RBJ sections; coefficients recomputed for a group whose rate differs rather than filtering at the wrong corner |
| `dsp.transform.fft` | Transform | → `Spectrum` artifact — magnitude in dB and phase in radians — with a rectangular / Hann / Hamming / Blackman-Harris / flat-top window; changes no samples |
| `dsp.detect.threshold` | Detection | → `Detections` artifact, `detections` and `widest_s` metrics, and a `detection_count` on the group |
| `dsp.detect.peaks` | Detection | → `Peaks` artifact. Local extrema ranked by **prominence** rather than by level, so a ripple on the flank of a real return is not a second detection; a minimum separation and a count keep the strongest, and a window bounds the prominence search for a long signal |
| `dsp.digital.slice` | Digital | Adds a `DigitalLogic` signal beside the waveform it came from — a comparator with hysteresis, keeping the input so a bit's reason survives |
| `dsp.digital.symbols` | Digital | → `Symbols` artifact. One decision per symbol period at two or four levels, with the clock phase recovered from the first edge, and the margin to the nearest decision boundary recorded per symbol |
| `dsp.digital.bits` | Digital | → `Bits` artifact. Packs a `symbols.v1` artifact into words of a chosen width and bit order |
| `dsp.measure.statistics` | Measurement | → `Statistics` artifact and per-signal metrics, reading the summary the store already holds; writes `rms` back as a property when asked to |
| `dsp.measure.pulse` | Measurement | → `Metrics` artifact. Width, PRI, PRF, jitter and duty cycle of a `detections.v1` artifact, measured per signal so two interleaved channels are not read as one train; writes `prf_hz` and `duty` back when asked to |

The three stages that read an artifact rather than samples — `dsp.digital.bits`,
`dsp.measure.pulse` — are the typed-port model of §9.3 earning its keep: the packer never
learns what a waveform looks like, the decoder never learns about bit order, and pipeline
validation refuses the chain before it runs if the producer is edited out.

**Planned**, the families M11 did not need:

| Family | Stages still to write |
|--------|-----------------------|
| Conditioning | DC block, clip, resample, window, trim/pad |
| Filtering | FIR (windowed-sinc); a designed higher-order IIR whose sections differ — Butterworth, Chebyshev — rather than one section repeated |
| Transform | STFT → `Spectrogram`, Hilbert → envelope + instantaneous phase |
| Digital | Clock recovery that tracks rather than aligning once (Gardner, Mueller–Müller) |
| Detection | Edge find, CFAR |
| Measurement | THD/SNR/SINAD |
| Utility | Split, merge, tee-to-property |

### 9.9 External Stages (Native Libraries)

The algorithm under test often already exists as a compiled library — a C/C++ codebase, a
vendor SDK, MATLAB Coder or Simulink output — and rewriting it in Rust to test it defeats
the point of the harness. So a stage kind may come from outside the binary: `sp-ext` loads
a DLL (`.dll` / `.so` / `.dylib`), hands it **one group at a time**, and reads the output
back into the same `StageOutput` every compiled stage produces. The library names its own
kind, which must sit in the **`ext.`** namespace (`ext.sample.gain` is what the reference
library declares) so that it cannot shadow a built-in and a recorded run says at a glance
that its stage came from outside this build. From the pipeline editor, the results
screen and the run schema it is an ordinary stage — the same parameter form, the same
per-stage recording, the same stage rail.

**The reference library declares itself impure.** `ext.sample.gain` publishes a
`groups_seen` counter beside its output to prove the instance handle really is the state's
home, and a counter means the same group handed over twice gives two different outputs. The
conformance harness (§14) said so the first time it was pointed at the library, and the
descriptor now carries `"pure": false` — which costs it the cache and costs the harness
nothing: it passes every other check, waiving none, through exactly the code a built-in is
run through.

**Where it lives.** A new crate `sp-ext`, depending on `sp-proc` only, sits beside `sp-dsp`
in the dependency rule. All `unsafe` FFI, library loading and buffer marshalling is
quarantined there; `sp-proc` still knows nothing about who implements a stage. This is the
seam §4.1 predicted, made real for one consumer rather than opened as a general plugin API.

#### The ABI

Flat C, no Rust types across the boundary, so any toolchain can produce a conforming
library. Every call takes an opaque handle; every buffer is caller-described and
callee-allocated one way only. Structs are versioned by an explicit `abi_version` the host
checks before anything else.

```c
/* Descriptor, fetched once at load time. JSON: kind, name, version, purity,
   thread-safety, param schema (ParamSpec, §9.2), declared ports (§9.3).
   The host generates the parameter form from this, exactly as for a Rust stage. */
uint32_t sp_abi_version(void);
const char* sp_describe(void);

typedef struct {                     /* one signal, borrowed from the host */
    const char* name;
    uint8_t     dtype;               /* the host lends f64; a library returns f32 or f64 */
    uint8_t     domain;
    const void* data;                /* host-owned, valid for this call only */
    uint64_t    len;
    double      sample_rate_hz;
    double      t0_s;
    const char* attrs_json;
} sp_signal;

typedef struct {                     /* one group = the unit of work */
    uint64_t          group_id;
    const sp_signal*  signals;
    uint32_t          signal_count;
    const char*       group_json;    /* group metadata + pulse records (§6.6) */
    const char*       inbound_json;  /* upstream artifacts on the blackboard */
} sp_group;

typedef struct {                     /* what the library produced */
    sp_signal*  signals;             /* library-owned until sp_free_output:
                                        the replacements first, in input order,
                                        then any added signals */
    uint32_t    signal_count;
    const uint8_t* dispositions;     /* one per INPUT signal, in input order:
                                        0 replaced | 1 passthrough | 2 dropped */
    uint32_t    disposition_count;   /* must equal the group's signal_count */
    const char* artifacts_json;      /* typed outputs, §9.4 */
    const char* metrics_json;
    const char* diagnostics_json;
} sp_output;

sp_handle sp_open(const char* params_json, char* err, size_t err_len);
int32_t   sp_begin_run(sp_handle, const char* run_json);
int32_t   sp_process(sp_handle, const sp_group* in, sp_output* out,
                     char* err, size_t err_len);
int32_t   sp_end_run(sp_handle, sp_output* out);          /* run-level artifacts */
void      sp_free_output(sp_handle, sp_output*);
void      sp_close(sp_handle);
```

`sp_process` maps one-to-one onto `Stage::process`. Input buffers are borrowed — the host
lends the group's samples for the duration of the call and the library must not retain
them. Output buffers are allocated by the library and freed by it through `sp_free_output`
after the host has copied them into the store, so neither side frees the other's memory.
A non-zero return is a `StageError` carrying the message the library wrote into `err`.

#### Execution

- **One group per call, in pipeline order.** Nothing about the group-at-a-time model
  changes: the loop in §9.5 calls `sp_process` where it would call `Stage::process`.
- **Concurrency is declared, not assumed.** The descriptor says `thread_safe` (host may run
  groups in parallel against separate handles), `sequential` (one call at a time), or
  `process_isolated`. Default is `sequential`, the safe reading of an unknown library. What
  a `sequential` library gets is mutual exclusion, not ordering: a library-wide lock is
  taken around every call, so it sees one group at a time but not necessarily in group
  order. Ordered delivery is the cross-group state question of §17.7 and waits for it.
- **Isolation is opt-in.** In-process is the fast path and the default; a segfault there
  takes the app with it. `process_isolated` runs the library inside a small host binary
  (`sp-stage-host`) that speaks the same ABI over shared memory, so a crash fails one group
  with a diagnostic instead of losing the run — that is M14. Until then the mode is
  accepted and recorded, and runs in process like the others.
- **Caching.** `Stage::cache_salt` is the seam: a compiled stage adds nothing, an external
  one returns its library file's BLAKE3 hash, and the key folds it in alongside the usual
  kind, version and params. Recompiling the DLL therefore invalidates its outputs and
  nothing else. A library that declares itself impure opts out entirely.
- **Reproducibility.** Every group's `run_stage` row carries a diagnostic naming the stage
  kind, the declared version, the resolved path and the file hash, so a run says exactly
  which build produced it (G8). A diagnostic rather than a metric, because a path is not a
  number and diagnostics are already the per-stage text the results screen shows.

#### Limits

The host validates what it can — ABI version, descriptor schema, buffer lengths and dtypes
against the declared ports, output disposition count against input signal count — and
trusts the rest. Loading a native library is running arbitrary code in the app's address
space; the settings screen keeps an explicit list of allowed library paths, and loading is
a deliberate user action rather than a scan of a plugin directory.

#### Settled during M9

| Question | Answer | Reasoning |
|----------|--------|-----------|
| How does an output say what happened to each input? | `dispositions` is one byte per **input** signal, in input order; `signals` carries the replacement buffers first, in that same order, then any additions | The §9.4 rule that every input is accounted for then holds by construction rather than by trusting the library, and `sp_signal` needs no ordinal field |
| What dtype crosses the boundary? | The host lends `f64`; a library may return `f32` or `f64`, and any other dtype is refused by name rather than reinterpreted | §17.14 has stages doing their arithmetic in `f64` anyway. The dtype byte stays in the struct, so lending native buffers later is a host change rather than an ABI break |
| What may an external stage be called? | Every external kind must start with `ext.`, checked at load time | A library then cannot shadow a built-in, and a recorded run says at a glance that its stage came from outside this build |
| How does a loaded stage reach the registry? | `StageRegistry::register_loaded`, which takes a descriptor and a closure rather than a `fn` pointer | An external stage's factory has to carry its library along with it; the compiled path is untouched |
| Who owns a descriptor, which must be `'static`? | The strings and slices parsed from the library's JSON are leaked, once per library per process | A descriptor genuinely lives as long as the library it describes, and a library is never unloaded once a run has referenced it |
| Is a library loaded per screen, or per run? | Once per process, cached by path, shared by every registry | Loading maps code in and leaks a descriptor; doing it per registry would do both repeatedly for one file |

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
    /// `y2` is a second scale at the right, for a Bode plot's phase.
    Series   { x: FieldRef, y: Vec<FieldRef>, y2: Vec<FieldRef>, x_log: bool, y_log: bool },
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

Built: `Statistics`, `Detections`, `Spectrum`, `Peaks`, `Symbols`, `Bits` and `Metrics`,
the outputs of the seven §9.8 stages that emit one. Each arrived with the stage that emits
it, and each cost one `impl` and no storage or viewer code — which is the claim above,
tested four times over at M11. `Peaks` draws as markers and `Symbols` as stems, so both
land on the scope's own time axis beside the waveform they were taken from; `Bits` and
`Metrics` are tables, the second carrying a unit per row because a pulse measurement
reports seconds, hertz and a bare ratio in the same breath.

The viewers are still ahead of the stages — every `ViewHint` above has a pane, heatmap and
scatter included — so `Spectrogram`, `Constellation` and `FilterResponse` remain a stage
away rather than a viewer away.

**Storage.** Payloads under 64 KB are stored as JSON in `artifact.payload_json`; larger
ones (a spectrogram matrix) go to a content-addressed blob (`kind = 'artifact'`) with the
JSON holding only the header. The viewer sees no difference.

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
- The rail lives on the Results screen. The Scope screen keeps its own transport and
  viewport and shares the playhead conventions, but has no rail of its own: a run is
  inspected where its stages are, and the two screens each keep their place across
  navigation (§12.3).

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

The x axis is the group's place in the list, unless every group in the run carries the
same sweep property: then it is the rung itself, in the units the ladder was swept in, so
unevenly spaced rungs plot where they belong. A rung named after the metric it is charted
against — an SNR ladder under a measured SNR — is marked as the rung rather than given up
on.

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
amplitude, drag = box zoom, double-click = fit, click = move the playhead, `Home`/`End` =
jump to bounds, space = play/pause. On the Results screen `←`/`→` walk the stage rail
(§10.2). Loop in and out are buttons on the transport rather than `[` / `]`: the canvas
takes the keyboard for the playhead, and a key that silently redefines the loop while the
pointer is elsewhere is worse than a labelled control. Binding them is part of the
configurable shortcut map (§15.11).

Per-trace controls: visibility, colour, gain, vertical offset, and a **stacked vs.
overlaid** layout toggle. Domain drives the default renderer — `DigitalLogic` signals get
logic lanes, `BasebandIq` gets I/Q or magnitude, `Symbols` gets labelled stems.

---

## 12. User Interface

### 12.1 Screens

| Screen | Purpose |
|--------|---------|
| **Library** | Tree of Dataset → Group → Signal / pulse field, with search, tag and property filters (every filter narrows: a query, two tags and `prf_hz >= 1000` asks for the signals that satisfy all of them), and a detail table sortable on any column — numeric columns as numbers, and a signal with no cached statistics last either way. Hosts cross-group pulse search (§6.6): a field predicate returns matching pulses across every group, each row jumping to its group and playhead position. Multi-select feeds the scope, a playlist, or a pipeline run. |
| **Import** | File picker → preview grid of the headers and first group → column-mapping panel (which column is the time of arrival and in what unit, which columns bind to property definitions) → profile save/load → progress with a live error list. |
| **Generate** | Node tree editor, parameter form, live preview, sweep configuration, preset browser. |
| **Pipeline** | Stage palette on the left, ordered stage list in the middle, generated parameter form on the right. Port validation inline. Run controls with group selection. |
| **Results** | Group list + stage rail + scope + artifact panes (§10.3). The main working surface for algorithm development. |
| **Runs** | History of runs with pipeline hash, status, timing, assertion results; promote to baseline; diff two runs. A run whose stages all ran but whose assertions failed reads as a failing run (§9.7), and the failures-only filter is how one is found in a long history. Opening or diffing a run hands it to the Results screen, which already draws both — one renderer, one set of conventions. |
| **Scope** | Playback-focused view of stored signals: transport, loop region, per-trace controls, and a viewport that survives navigation. A run's stages are walked on the Results screen, which has the rail (§10.2). |
| **Inspector** | Detail for one signal, pulse field or pulse: full metadata, property editor, tags, statistics, histogram, and a virtualised value table — for a pulse group, the table is the pulse records themselves, one row per pulse across every field. Statistics are recomputed from the samples in one streaming pass (min, max, peak-to-peak, mean, RMS, standard deviation, zero crossings and the distribution), not read from the cached row, so they answer for what is actually stored; the table is a window on the column, paged, so a 100 M-sample signal costs a read rather than a copy. A pulse is addressed as a row of its group's table: an unannotated pulse has no row of its own (§6.6). |
| **Properties** | The list of property definitions and the form that declares a new one (§6.3). Property *sets* — named reusable bundles — have their tables in the schema but no screen yet (§15.5). |
| **Settings** | Library location, theme, default sample rate, strict/tolerant import, retention defaults, the run-level sample cap, decimation quality, histogram bins, the allowed external libraries, and the (fixed, v1) keyboard map. It also reports what the open library is made of — rows, bytes by blob kind, published stage-cache keys — with `Reclaim unused blobs` and `Clear stage cache` beside the figures. Clearing unpublishes keys and nothing else: the output they named belongs to the run that recorded it and stays there, so the cost is the next run's reuse. `Verify Library`, `VACUUM` and `Rebuild pyramids` are written and tested in `sp-store` but have no button yet (§15.6). |

### 12.2 Iced Application Shape

```rust
/// Which screen is showing. A tag, not a state container: every screen's state
/// is a field of `App` and outlives navigation, which is what §12.3 promises.
enum Screen {
    Library, Inspector, Properties, Import, Generate,
    Pipeline, Runs, Results, Scope, Settings,
}

struct App {
    screen:   Screen,
    settings: Settings,          // written back to settings.json as it changes
    store:    Option<Store>,     // None when the library would not open
    store_error: Option<String>, // why, for the status bar
    library:  library::State,    // one field per screen, each owning its own state
    import:   import::State,
    generate: generate::State,
    pipeline: pipeline::State,
    properties: properties::State,
    inspector:  inspector::State,
    runs:     runs::State,
    results:  results::State,    // run, group, stage, pinned stage and playhead
    scope:    scope::State,      // playback keeps its position across screens
    settings_screen: settings::State,
}

enum Message {
    Nav(Screen),
    ToggleTheme,
    Library(library::Message),
    Import(import::Message),
    Generate(generate::Message),
    Pipeline(pipeline::Message),
    Properties(properties::Message),
    Inspector(inspector::Message),
    Runs(runs::Message),
    Results(results::Message),   // SelectStage, PinStage, SelectGroup, StepStage, Tick
    Scope(scope::Message),
    Settings(settings::Message),
}

/// What the Inspector is looking at. A pulse is addressed as a row of its
/// group's record table rather than as a target of its own: an unannotated
/// pulse has no row to point at (§6.6).
enum Target {
    Signal(SignalId),
    PulseField { group: GroupId, ordinal: u32 },
}
```

Each screen module owns its own `State`/`Message`/`update`/`view`, and the root `update`
delegates — progress, job completion, store errors and the playback tick are all variants
of the owning screen's message rather than of the root's. A long job is a
`Task::perform` over `jobs::read`/`jobs::write`, so the store is touched on the blocking
pool and the UI thread only ever sees the message that comes back. Per-stage progress
arrives while a run is still going, so the stage rail fills in live — the user can inspect
stage 1's output while stage 4 is still computing.

### 12.3 UX Principles

- **No modal blocking on IO.** Import, generation and runs happen in the background; the
  library stays browsable.
- **Inspection is never destructive.** Selecting or pinning a stage only changes the view.
  Nothing in the results screen mutates the library.
- **The view survives navigation.** Playhead, viewport, selected group and selected stage
  persist across screen switches, so comparing two stages never costs the user their place.
- **Destructive actions are few and are refused when they would cost evidence.** Deleting a
  run a baseline names is refused by the store with the baseline's name in the message, and
  retention never prunes one. What is not built is the softer half of this: there is no
  confirmation step, no trash table and no undo, and §15.11 tracks it. The library screen
  offers no delete at all, which is why the gap has not bitten.
- **Errors are data, not dialogs.** Import problems and stage diagnostics land in
  filterable lists the user can work through, not a popup per row.
- **Keyboard first** for transport, stage stepping and navigation.

### 12.4 Settings

Settings are *not* in the library: one of them is which library to open, and the theme
belongs to the user rather than to the file they happen to have open. They live in
`settings.json` in the application data directory, beside the default library and the
logs:

| Setting | Effect |
|---------|--------|
| Library location | Which library file opens; changing it reopens in place, and everything showing the old library is dropped rather than left pointing at ids that mean nothing in another file |
| Theme | Dark or light. The `Ctrl+T` shortcut and the Settings screen set the same value, so the choice survives a restart either way |
| Default sample rate | The rate a new generator spec starts at. The spec on screen follows it only while it is still on the old default — a rate the user typed is theirs |
| Strict / tolerant import | Whether an import refuses a file whose declared count does not match what was read (§7.3) |
| Retention | Finished runs kept per pipeline; the oldest go first. A run a baseline names is never deleted, and neither is one still going. Enforced in the window only: a headless run keeps everything it records, because CI is not the place to lose evidence |
| Sample cap | How many bytes of samples one run may record before it keeps only what its stages said about them (§9.5). It is a limit on storage, never on evidence: past the cap a stage still records what it did to every signal, its statistics, its metrics and its diagnostics, and says why the samples are not there. A cap of zero is a real choice — the numbers without the waveforms behind them. The headless run takes it from `--sample-cap` rather than from the file, so CI decides its own budget |
| Decimation quality | Shifts the reducer's automatic level choice by one either way (§5.4). It never promotes a level to a raw read: that bound is what keeps a frame inside its budget, not a preference |
| Histogram bins | Resolution of the Inspector's histogram |
| Allowed external libraries | The native libraries this installation may load as stages (§9.9). The list is consent rather than configuration: a library is on it because the user picked that file, and the load is attempted there and then, so a failure is reported beside the path that caused it. A headless run reads the same list, so CI loads what the window would |

Every control writes the file as it changes — there is nothing here that is only
half-decided, so there is no Save button. A settings file that will not parse, or a field
an older build did not write, falls back to the default for that field: losing a
preference must never cost the user their application. Out-of-range values are clamped
rather than refused, for the same reason.

The keyboard map is fixed in v1; the screen lists the ones that work from anywhere —
`Ctrl`+`1`…`9` and `Ctrl`+`0` to jump to a screen, `Ctrl`+`T` for the theme — so they are
discoverable. The transport and rail keys belong to the screen they act on and are labelled
there instead (§11.4). §15.11 tracks making the whole map configurable.

---

## 13. Performance Budget

| Operation | Target | Approach |
|-----------|--------|----------|
| Scope frame | < 8 ms at 1080p, 8 traces | Pyramid decimation to ≈1 pair/pixel; layered canvas caches |
| Playhead tick | < 0.5 ms | Overlay-layer redraw only |
| Stage switch in results | < 30 ms | Metadata-only load; pyramids already built; viewport preserved |
| CSV sniff (first 2 blocks) | < 100 ms on a 4 GB file | Bounded read, never a full scan |
| CSV ingest | ≥ 50 MB/s single-threaded | Streaming reader, no intermediate `String` per field, direct write into a column buffer |
| Pulse ingest | ≥ 1 M records/s | One append per field per row; group flushed to chunked blobs at the block boundary |
| Cross-group pulse search | < 1 s over 10 000 groups | Zone-map prefilter in SQL, then parallel chunked scan of surviving columns (§6.6) |
| Generation | ≥ 100 M samples/s (sine, 8 cores) | Rayon chunking, no per-sample allocation |
| Pyramid build | ≤ 1 linear pass over the blob | Build all levels in one traversal, streaming chunk by chunk |
| Column slice read | ≤ 1 copy of the requested span | Incremental blob I/O into the caller's buffer; no whole-column materialisation |
| Pipeline throughput | ≥ 0.8 × cores on group-parallel work | Groups fan out across rayon; bounded in-flight frames |
| Re-run after a param edit | Only stages ≥ the edited one | Content-hash cache (§9.5) |
| Library query | < 50 ms at 10 000 signals | Indexed SQLite + FTS5 + `signal_property` index, metadata only |
| Cold start | < 500 ms | Lazy pyramid load, no eager sample reads |

These are design targets, not all of them measured. The two that are, are the two the
goals name:

- **G2, the frame budget.** `sp-engine/tests/scale.rs` writes a 100 M-sample library and
  reduces the visible span to one `(min, max)` pair per pixel column at 1080p, sixty times
  over, at four zoom levels. Re-measured at v1.3 in a release build: **1.06 ms** a frame
  over the whole signal (pyramid level 9), **1.01 ms** over a tenth of it (level 6),
  **1.46 ms** over a thousandth and **0.65 ms** on raw samples — against the 8 ms budget,
  with the pyramid itself built in 0.84 s. That is one trace: the budget's eight are eight
  such reductions, which is what the margin is for, and drawing them is Iced's cost rather
  than this crate's. It is `#[ignore]`d because it writes ~400 MB and only means anything
  optimised; `cargo test -p sp-engine --release --test scale -- --ignored` is the command.
- **Re-run after a param edit.** `sp-dsp/tests/pipeline.rs` runs a pipeline, edits stage
  *n* and asserts that stages before it come back `cached` and stages from it on re-ran.

The rest are budgets the design was shaped around rather than numbers under test: there is
no `benches/` directory and no `criterion` dependency. A criterion suite over a fixed
fixture set is the honest way to hold the ingest, search and generation lines to account,
and belongs with the first milestone that has a reason to defend one of them.

---

## 14. Cross-Cutting Concerns

**Error handling.** `thiserror` enums per crate; `sp-app` maps them to user-facing text.
Nothing panics on bad input — a corrupt CSV, a truncated blob, or a stage given a
zero-length signal produces a diagnostic. A stage that *returns* an error fails its own
group and no other: the run carries on, the group is recorded `failed` with the message,
and the remaining groups still produce their numbers. A stage that **panics** is a
different matter and is not yet contained *at run time*: `catch_unwind` guards the store's
writer thread, so a panicking job cannot poison the connection, and the conformance harness
runs `process` under a guard of its own so a stage that panics on an empty group is a
finding rather than a crashed test — but there is no guard in the scheduler, so a panicking
algorithm still takes the run with it. That guard, and a per-stage timeout, belong with
M14's isolation work — in-process is where a native library crashes too (§9.9).

**Integrity.** A blob's checksum *is* its address: `sp-store` looks a column up by hash on
write, so an identical column is shared rather than stored twice, and a write that does not
finish never acquires a checksum to be found by. Verification is deliberate rather than
automatic — reads do not rehash, because a read is on the frame path and a rehash is a
pass over the whole column. `verify()` rehashes every blob and reports checksum mismatches,
length mismatches, refcount disagreements, blobs no row references and rows pointing at a
missing blob; `repair_references()` reconciles what can be reconciled. Both are tested and
neither has a button yet (§12.1). Because the bytes and the rows describing them commit in
one SQLite transaction, the two cannot disagree after a crash — the failure mode a split
file/database store has, and this one does not.

**Testing.** 966 tests pass at v1.4, over a workspace that is clean under
`cargo clippy --all-targets -- -D warnings` and `cargo fmt --check`. CI runs all three on
Windows and Linux for every push and pull request.

- Unit tests per crate, beside the code they test; property tests (`proptest`) for the CSV
  round-trip (G1), for generator determinism across chunk boundaries (G3), and for the
  playback invariants that hold at any zoom: a cell bounds its span, a pyramid builds the
  same whatever the chunk size, the playhead never leaves its range, and a reduction never
  outgrows the canvas.
- Golden-file tests for the parser against a fixture corpus in `crates/sp-csv/tests/` that
  includes the malformed cases: a wrong count, no count at all, a BOM, CRLF, a lone `\r`,
  semicolon delimiters, comment lines and uncooperative cells.
- `crates/sp-dsp/tests/pipeline.rs` runs the scheduler end to end — cache hits and misses,
  retention, the sample cap, cancellation, a failing group, a deleted run.
- `crates/sp-dsp/tests/families.rs` runs the M11 families over data whose answer is known
  in advance: a pulse train goes in and 10 Hz, 20 ms and a duty of 0.2 come back out of the
  database; an NRZ waveform goes in and the bits that made it come back out.
- `crates/sp-app/tests/data_flow.rs` follows the data the whole way: a spec generates a
  sweep, the sweep lands in the library, a run measures what the spec put into it, the
  Inspector recomputes the same numbers, the output plays back over a pyramid of its own,
  and an exported file re-imports to the rows it came from.
- `crates/sp-gen/tests/ladder.rs` holds the other layout: one group per rung, each carrying
  its own SNR, bit-identical on a second generation, and every rung measured back to the
  ratio it asked for.
- `crates/sp-app/tests/cli.rs` is the headless smoke test: build a library, run the
  pipeline, promote a baseline, change the algorithm, and check the exit code says
  *regressed* rather than *misused* (G8).
- Each screen's `update` is tested directly — the buttons and the forms, screen by screen —
  because an Iced `update` is a pure function and needs no window.
- **The stage conformance harness** (`sp-proc/src/conform.rs`) is the generic test any
  `Stage` impl can be run through, and it is what makes G8 hold for a stage nobody here
  wrote. It supplies its own groups — an ordinary one, then empty, single-sample, all-NaN,
  DC-only and non-finite — and asks seven questions of the stage over each: the descriptor
  is coherent and namespaced (**declaration**); `configure` takes what the descriptor
  declares and *refuses* what it does not (**configuration**); every input signal is
  accounted for and the next stage's frame can be built (**contract**); every required
  output port carries something of the kind it promised, and nothing is published on a port
  nobody declared (**ports**); the same group gives the same output through this instance
  and through a fresh one, and two instances salt the cache key the same way, asked only of
  a stage that declares itself pure (**determinism**); a cancelled run stops it
  (**cancellation**); and nothing panics, since `process` runs under `catch_unwind` and a
  panic is a finding rather than a crashed test (**survival**). An error is a legitimate
  answer throughout — a stage that cannot work with what it was handed says so, which is
  §14's rule, not a failure to conform.

  A waived check is named in the report, because a waiver is a promise nobody is holding
  the stage to. `sp-dsp/tests/conformance.rs` runs all thirteen built-ins through it with
  nothing waived, and `sp-ext/tests/native.rs` runs the sample external library through the
  same code — the harness lives in `sp-proc` and knows nothing about DSP or about dynamic
  loading, which is the point. It earned its keep on the first run: it found a passthrough
  that ignored both its parameters and the cancel flag, and a reference external library
  that called itself pure while publishing a call counter (§9.9).

**Observability.** `tracing` spans around every job and every stage invocation, written to
stderr and to a daily rolling file in the application data directory — under
`SignalPlayback/logs`, beside `settings.json` rather than inside the library, because the
library is a file the user may move or replace and the log should not follow it.
`RUST_LOG` sets verbosity. Per-stage wall time is recorded in
`run_stage`, so a slow stage is visible in the rail without profiling, and per-stage
diagnostics are shown on the Results screen. An in-app log panel is not built: the file and
the diagnostics pane between them have covered it so far.

**Distribution.** `cargo build --release` → one binary, no runtime, no companion directory.
A `v*` tag builds and attaches Windows and Linux x86-64 binaries to a GitHub release;
macOS builds from the same source but is not produced by the workflow. An optional MSI via
`cargo-wix` later.

**Accessibility.** The trace palette is the Okabe–Ito colour-blind-safe set, and trace and
artifact identity is also carried by the legend, never by colour alone. Minimum 14 px UI
text. Keyboard coverage is partial rather than full: every screen is reachable by
`Ctrl`+*digit*, the transport and the stage rail have keys, and text inputs take focus in
order — but there is no focus ring walking every control, and §15.11's configurable
shortcut map is where finishing the job belongs.

---

## 15. Potential Features

Grouped by area and tiered: **[MVP]** shipped in v1 · **[done]** shipped since, in the
milestone named · **[V1.x]** near-term · **[V2]** larger effort · **[Stretch]** speculative.

Every **[MVP]** line below is built — that is what closed §16 — so the tier that carries
information now is **[done]**, which marks a post-v1 entry the code has caught up with.
Several are marked from M3 rather than from a 1.x milestone: the generator was written to
the whole of §8.1 at the time, so nodes this section had filed under *near-term* have
existed since M3 and only the milestone table had not noticed.

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
- **[done — M3]** Chirps (linear/log/quadratic) and AM/FM/PM modulation. FM and PM require
  an oscillator carrier, for the reason in §8.1.
- **[done — M3]** Arbitrary `f(t)` expression node, compiled once to RPN (`sp-gen/expr.rs`).
- **[done — M3]** PRBS / m-sequence generator, seekable by repeated squaring over GF(2).
- **[done — M3]** Direct generation of `digital_logic` and `symbols` signals as preprocessed
  inputs: the domain is a field of the spec and the Generate screen offers it.
- **[done — M3]** Preset library with import/export: nine built-ins embedded in the binary
  (tone, two-tone, noisy tone, linear chirp, pulse train, AM, FM, PRBS-9 NRZ, step edge),
  plus load and save of a `GenSpec` file.
- **[done — M12]** Impairment ladders — one source across a sweep of SNRs, **one group per
  rung**, with an `Awgn` node that makes the rung a ratio of the source's own power rather
  than a noise amplitude. The old layout is still there: one group holding a signal per
  rung is a curve across signals, a group per rung is a curve across groups, and only the
  second is what a *pipeline* asserts over.
- **[V2]** Digital modulation: ASK/FSK/PSK/QPSK/QAM with configurable symbol rate and pulse shaping.
- **[V2]** Impairment stack: phase noise, IQ imbalance, DC offset, clock drift, dropouts,
  clipping. AWGN at a target SNR was pulled forward to M12, which needed it to have an SNR
  rung at all; the rest of the stack is untouched.
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
- **[done — M10]** Content-hash stage cache — editing stage *n* re-runs only *n…end*.
- **[done — M10]** Retention policy per stage (always / on failure / never), plus the
  run-level sample cap the default needed (§9.5).
- **[done — M7]** Assertions on metrics, statistics, artifacts and stage timings; run status
  aggregates them.
- **[done — M7]** Baseline promotion and automatic regression comparison.
- **[done — M11]** Stage conformance test harness for new algorithms (§14). Every built-in
  and the reference external library are run through it, with nothing waived.
- **[done — M11]** Detection, symbol-decode and measurement stage families: peak find,
  slice, symbol decode, bit pack and pulse metrics, with the four artifact kinds they emit
  (§9.8, §10.1).
- **[V2]** Parameter sweep over a stage — run the pipeline across a grid, get a metrics table.
- **[V2]** Branching pipelines (a real DAG) with a graph editor.
- **[V2]** Per-stage breakpoints: pause a run at a stage and inspect before continuing.
- **[V2]** Stage-level unit fixtures — pin one group's input as a stage's test case.
- **[done — M9]** External stage: a native library (DLL/.so) fed one group at a time over a
  flat C ABI, recorded like any other stage (§9.9), with an allow-list in Settings and a
  conforming reference library (`sp-ext-sample`) the tests load.
- **[V1.x]** Process-isolated external stages — a crashing library fails one group, not the run.
- **[V2]** Plugin stages beyond the §9.9 ABI — custom artifact kinds and import formats.
- **[V2]** Distributed / multi-process run execution for large datasets.
- **[Stretch]** Stage authored in an embedded script for quick experiments.
- **[Stretch]** Auto-tuning — search stage parameters against an objective metric.

### 15.4 Results & Inspection
- **[MVP]** Stage rail with per-stage summary, status and wall time.
- **[MVP]** Artifact model with `ViewHint`-driven viewers: table, series, scalars, tree.
- **[MVP]** Overlay artifacts drawn on the scope's time axis.
- **[MVP]** Global playhead shared across scope and every time-aware artifact pane.
- **[MVP]** Group list with status, timing and metric columns.
- **[done — M6]** Pin-two-stages comparison with residual (A−B) trace and a field-level
  artifact diff (G7).
- **[done — M6]** Heatmap viewer for spectrograms and correlation surfaces, and
- **[done — M6]** Scatter viewer for constellations and feature spaces — both are panes
  driven by a `ViewHint`, and both are waiting on a stage that emits one (§10.1).
- **[done — M7]** Run-to-run diff: max abs error, RMS error, first divergence index.
- **[done — M7]** Metric-across-groups chart (the degradation curve), built as an artifact
  so the same viewer draws it.
- **[V1.x]** Jump from a diagnostic to the time span that produced it. Diagnostics are
  listed per stage; none of them carries a time span to jump to yet.
- **[V2]** Field-level artifact diff between runs.
- **[V2]** Save an inspection layout (panes, pinned stages, viewport) per pipeline.
- **[V2]** Export a stage's output back into the library as a new dataset.
- **[Stretch]** Side-by-side A/B of two *pipelines* on one group.

### 15.5 Signal Properties & Typing
- **[MVP]** Domain and provenance on every signal, driving default rendering.
- **[MVP]** User-defined property definitions with typed kinds, units and validation.
- **[MVP]** Property editor in the Inspector; unrecognised attributes preserved.
- **[V1.x]** Named property sets applied to a dataset or group. The tables are in the
  schema from migration 0001; nothing reads or writes them.
- **[done — M8]** Indexed property queries: the Library screen filters on a property with a
  comparison (`prf_hz >= 1000`, `coding` is `nrz`) through the `signal_property` index,
  alongside tags and full-text search. **[V1.x]** Saving one as a smart search.
- **[V1.x]** Stage write-back of estimated properties, **namespaced by stage**. A stage can
  patch a signal's or a group's attributes today — the statistics stage writes `rms` when
  asked, the threshold stage writes `detection_count` — but the patch lands in the run's
  record under its plain key, not in the library and not namespaced.
- **[V2]** Property templates inferred from a sample file.
- **[V2]** Derived/computed properties defined by an expression over other properties.
- **[Stretch]** Schema versioning with migration of existing property values.

### 15.6 Database & Library Management
- **[MVP]** Dataset/group/signal hierarchy, search, sortable detail table.
- **[MVP]** Tags with filter-by-tag.
- **[MVP]** Cross-group pulse search: numeric predicates over pulse fields, zone-map prefiltered (§6.6).
- **[V1.x]** Name and tag an individual pulse; saved pulse selections. The `pulse` and
  `pulse_tag` tables are in the schema and nothing writes them yet — a pulse is addressed as
  `(group, index)` until one is annotated, which is what §6.6 chose.
- **[V1.x]** Bulk edit: rename, retag, set properties across a selection.
- **[done — M1]** Deduplication on ingest: a column is addressed by its BLAKE3 hash, so an
  identical column is shared rather than stored twice and a passthrough stage costs nothing.
  **[V1.x]** Surfacing that as *duplicate detection* — telling the user which signals are
  the same bytes.
- **[done — M8]** Library statistics: the Settings screen reports rows by table, bytes by
  blob kind, unreferenced bytes and published cache keys. **[V1.x]** The rest of the
  dashboard — rate distribution, run storage over time.
- **[done — M8]** `Reclaim unused blobs` and `Clear stage cache`, and prune-old-runs as the
  retention setting. **[V1.x]** `Verify Library`, `VACUUM` and `Rebuild pyramids` are
  written and tested in `sp-store` but have no button (§12.1); a library that has shed data
  needs the `VACUUM` to get its pages back.
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
- **[done — M8]** Per-signal statistics: min, max, mean, RMS, std dev, peak-to-peak, zero
  crossings, recomputed from the samples in one streaming pass rather than read from the
  cached row.
- **[done — M8]** Histogram view, in the Inspector, at a configurable bin count.
- **[done — M8]** FFT magnitude with a selectable window (rectangular, Hann, Hamming,
  Blackman-Harris), published as a `Spectrum` artifact. **[done — M12]** Phase, on the
  spectrum's own second axis, and a flat-top window for amplitude accuracy.
- **[V2]** THD, SNR, SINAD, SFDR, ENOB measurements.
- **[V2]** Cross-correlation and time-delay estimation between two signals.
- **[V2]** Envelope detection, peak finding, edge/pulse measurements (rise time, width, duty).
- **[V2]** Signal math expression bar (`sig1 - sig2 * 0.5`) producing a derived signal.
- **[Stretch]** Anomaly flagging across a whole dataset.

### 15.9 Export & Interop
- **[MVP]** Export to the native pulse-record CSV format (round-trip fidelity, TOA restored to its source unit).
- **[V1.x]** Export a selection, a time range, or a stage's output. A whole imported
  dataset exports today, from the Library screen or the command line (§7.5); a generated
  one has no layout to reverse and is refused.
- **[V1.x]** Export the scope view as PNG/SVG.
- **[V1.x]** Export a run's metrics table as CSV.
- **[V2]** Export to WAV, `.npy`, Parquet, MATLAB, HDF5, SigMF.
- **[V2]** Copy samples to clipboard as CSV for a spreadsheet.
- **[V2]** Generate a run report (PDF/HTML) with plots, metrics, assertions and metadata.
- **[Stretch]** Python binding (`pyo3`) to read the library and run results from scripts.

### 15.10 Automation & Extensibility
- **[done — M7, M8]** CLI mode: `signalplayback run|baselines|promote|import|export` for
  scripting and CI. **[V1.x]** `generate` — a spec can be rendered into a library only from
  the window, which is the one gap in a scriptable generate → run → compare loop.
- **[done — M7]** `run --assert-baseline` with a distinct exit code for *misused* and
  *regressed*, so a pipeline is a CI check (G8).
- **[V2]** Scriptable batch jobs (a job file describing generate → run → compare → export).
- **[V2]** Plugin interface for custom stages, artifact kinds and import formats.
- **[Stretch]** Embedded scripting (Rhai/Lua) for on-the-fly signal math and quick stages.

### 15.11 Quality of Life
- **[MVP]** Last-opened library, remembered in `settings.json`. **[V1.x]** Window layout:
  the window opens at its default size and the pane widths are fixed.
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
| **M1 — Store** | SQLite schema + migrations, chunked blob store, store actor, property definitions, library screen | Signals insert and list; property queries work; a column round-trips through chunked BLOB storage byte-for-byte; `Verify Library` passes |
| **M2 — Import** | Block framer, parser, mapping UI (incl. property binding), streaming ingest, error list | Fixture corpus imports; round-trip property test passes (G1) |
| **M3 — Generate** | `GenSpec`, primitives, combinators, generator UI, sweeps, pulse-train mode | Determinism property test passes (G3); presets load; a generated train is the same shape as an imported one |
| **M4 — Playback** | Pyramid builder, viewport, scope canvas, transport, clock, domain renderers | 100 M-sample signal plays at 60 fps (G2) |
| **M5 — Pipeline** | `Stage` trait, registry, ports, scheduler, run recording, first `sp-dsp` stages | A pipeline runs over a dataset group-by-group and every stage's output is persisted |
| **M6 — Results** | Artifact model + viewers, stage rail, results screen, global playhead, comparison | Any (group, stage) is inspectable with its artifacts; pinned comparison works (G7) |
| **M7 — Regression** | Assertions, baselines, run diff, metrics chart, CLI `run` | A pipeline runs headless in CI and fails on a baseline deviation (G8) |
| **M8 — Polish** | Inspector, tags/search, export, settings, statistics, packaging | Full loop demoable end-to-end; release binary produced |

M2 and M3 are independent after M1 and can proceed in parallel. M5 depends on M1 (storage)
and benefits from M3 (test inputs) but not from M4; M6 depends on both M4 and M5.

**M0–M8 are delivered.** The table is closed: nothing is added to it, and later work is a
post-v1 milestone below.

**Re-checked at v1.3.** Each exit criterion was taken back to the code and to a test that
holds it, rather than to the commit that claimed it:

| Phase | What holds the exit criterion now |
|-------|-----------------------------------|
| M0 | CI runs `cargo fmt --check`, `clippy --all-targets -D warnings` and the test suite on Windows and Linux, all clean. Ten screens, each one's `update` tested directly. |
| M1 | `sp-store/tests/library.rs`: `signals_insert_and_list`, `property_definitions_and_queries`, `samples_round_trip_byte_for_byte_across_chunks`, `identical_columns_share_one_blob`, `verify_passes_on_a_healthy_library_and_catches_damage`. Verify passes as a tested store function; it has no button yet (§12.1). |
| M2 | `sp-csv/tests/corpus.rs` imports the fixture corpus, malformed files included; `roundtrip.rs` holds G1, `any_well_formed_file_round_trips` as a proptest. |
| M3 | `sp-gen/tests/determinism.rs` holds G3, chunk joins and per-node noise streams included; every built-in preset is parsed, validated and rendered by its own test; `a_generated_train_is_the_same_shape_as_an_imported_one` is the third clause, as written. |
| M4 | `sp-engine/tests/scale.rs` holds G2 at 100 M samples — re-measured for §13, worst case 1.46 ms a frame against an 8 ms budget. |
| M5 | `sp-dsp/tests/pipeline.rs`: `a_pipeline_runs_group_by_group_and_records_every_stage`, `each_stage_records_what_it_actually_computed`, `one_bad_group_fails_on_its_own_without_sinking_the_run`. |
| M6 | The Results screen's own tests hold G7: `a_pinned_stage_gives_every_matched_signal_a_residual_trace`, `a_pinned_artifact_is_diffed_field_by_field`, `an_overlay_artifact_lands_on_the_scope_and_follows_the_playhead`. |
| M7 | `sp-app/tests/cli.rs` holds G8 end to end: a baseline deviation exits 2, a bad request exits 1. `sp-proc/tests/compare.rs` covers what a diff must notice. |
| M8 | `sp-app/tests/data_flow.rs` runs the whole loop — spec → library → run → Inspector → playback → export → re-import. The release workflow builds the Windows and Linux binaries from a tag. |
| M9 | `sp-ext/tests/native.rs`: `a_dll_runs_as_a_stage_over_one_group_at_a_time_and_every_output_is_persisted`, `every_result_carries_the_build_that_produced_it`, `the_library_file_is_part_of_the_cache_key`, `a_library_on_disk_is_refused_until_it_is_allowed_and_then_loads`. |
| M10 | `sp-dsp/tests/pipeline.rs`: `editing_a_stage_re_runs_it_and_everything_after_it` is the first clause, `a_cached_run_records_what_a_cold_run_records` the second, with retention, the sample cap and `a_key_whose_run_is_gone_is_unpublished_rather_than_followed` beside them. |
| M11 | `sp-dsp/tests/conformance.rs`: `every_builtin_conforms` and `the_harness_covers_every_check_for_every_builtin` hold the second clause with nothing waived, and `sp-ext/tests/native.rs`'s `a_stage_from_outside_this_binary_is_held_to_the_same_contract` holds it for a stage this crate did not write. `sp-dsp/tests/families.rs` holds the first: `detection_and_measurement_run_end_to_end_over_a_pulse_train`, `symbol_decode_runs_end_to_end_and_the_bits_are_the_bits_that_went_in`, `a_family_pipeline_is_valid_before_it_is_run`. |
| M12 | `sp-gen/tests/ladder.rs`: `a_ladder_produces_one_group_per_snr_rung` and `a_ladder_is_the_same_ladder_the_second_time` are the criterion, with `every_rung_lands_at_the_noise_it_names` and `the_rung_is_a_declared_group_property_rather_than_a_loose_attribute` beside them. `determinism.rs` now carries an `Awgn` node in its strategy, so `chunks_join_up_into_the_whole_render` is what holds the probe window to G3. |

Two corrections came out of that pass rather than a milestone: §9.8 and §10.1 were
describing stage and artifact families that were planned rather than written, and §13 was
crediting a benchmark suite that does not exist. Neither is an exit criterion — every
criterion above is about the harness, and the harness is what got built — but a design
document that describes stages nobody wrote is one an M11 would be planned from wrongly.

### 16.1 Post-v1 Track

The **[V1.x]** entries in §15.3 are grouped into milestones the same way, each a
self-contained deliverable off the v1 baseline. Order is by what unblocks the most work,
not by section number.

| Phase | Deliverable | Exit criteria |
|-------|-------------|---------------|
| **M9 — External stages** (done) | `sp-ext`, the §9.9 C ABI, library allow-list in settings, sample conforming DLL | A sample DLL runs as a stage over one group at a time; its path, hash and version are recorded in `run_stage` (G8) |
| **M10 — Stage cache** (done) | Content-hash stage cache, per-stage retention policy, run-level sample cap | Editing stage *n* re-runs only *n…end*; a cached run and a cold run produce identical outputs |
| **M11 — Stage families** (done) | Detection, symbol-decode and measurement stages; stage conformance harness; the artifact kinds they emit (§10.1) | Each family has a stage that runs end-to-end and passes the conformance harness |
| **M12 — Generation** (done) | Impairment ladders, one group per rung; a flat-top window and FFT phase | A ladder produces one group per SNR rung, deterministically (G3) |
| **M13 — Ingest & UX** | Drag-and-drop and multi-file import, watch folder, command palette, configurable shortcuts | A watched folder imports without user action; every action is reachable from the palette |
| **M14 — Isolation** | `sp-stage-host`, shared-memory transport, `process_isolated` execution | A library that segfaults fails one group with a diagnostic and the run continues |

M9 comes first because the external stage is the reason the harness exists for algorithms
that are not written in Rust, and M14 only makes sense once M9 has a library to isolate.
M10's key was already computed and recorded per stage when M5 wrote the scheduler; what it
added is the part that decides what a key may point at — `on_failure` swept once its group
passes, only `always` published, a stale key unpublished on the lookup that finds it, and a
run-level cap that limits bytes without costing the account of what a stage did.

**M12 shrank when it was re-read against `sp-gen`, then grew by one node.** Chirps,
AM/FM/PM, the `f(t)` node and PRBS were all written at M3 — the generator was built to the
whole of §8.1 rather than to the MVP slice of §15.2 — so what was left of the milestone was
the impairment ladder that produces *one group per rung* and the two FFT gaps (phase, a
flat-top window).

The ladder is indeed a change to how a sweep writes rather than to what a node renders: the
rungs, the substitution and the property declaration are M3's, and the layout picks one of
two shapes at write time. But the exit criterion says *SNR* rung, and nothing in §8.1 could
express one. A swept `Noise.amp` is a ladder of noise amplitudes, which is a different
ladder for every source and not a ratio of anything; so `Awgn` came forward from §15.2's V2
impairment stack — the one item of it the criterion requires, with phase noise, IQ
imbalance, clock drift, dropouts and clipping left where they were.

That node is where the milestone's design work went. Power is a property of a whole signal
and §8.2 requires every node to be a function of its own sample index, so the window a rung
measures over cannot be the window being rendered: it is a bounded probe fixed by the
node's own grid, spread across the whole of it, memoised once per render. The property
test that holds §8.2 — a chunked render equals a whole one — now has `Awgn` in its
strategy, which is the only reason to believe any of that.

Two things fell out of the ladder rather than being asked for. A rung is a property of a
*group*, so the chart across groups is plotted against the rung and not against a group's
ordinal (§10.5) — without which the curve of §8.4 is a curve against 0, 1, 2, 3. And
publishing FFT phase needed a second scale on the `Series` view, since dB and radians share
an x axis and nothing else: `ViewHint::Series` gained a `y2`, which is what a Bode plot is
and what `FilterResponse` will want when a filter stage publishes one.

**M11 grew by that pass and is now delivered.** Five stages — peak find, slice, symbol
decode, bit pack and pulse metrics — give the three families the milestone named a stage
each, and the four artifact kinds they emit (`Peaks`, `Symbols`, `Bits`, `Metrics`) cost an
`impl` apiece, as §10.1 promised they would. The harness came first and was worth it
twice over before the families were written: it found a passthrough that ignored its
parameters and the cancel flag, and a reference external library calling itself pure while
publishing a call counter. Two of the new stages read an artifact rather than samples,
which is the first real exercise of §9.3's typed ports outside a test.

What M11 did *not* do is the rest of §9.8's planned table — FIR and designed IIR filters,
STFT and Hilbert, CFAR, THD/SNR — none of which the exit criterion asked for and none of
which the harness needs. They stay planned, in the same table, with one addition: the
clock recovery here aligns once from the first edge, which is honest for a clean capture
and not enough for a drifting one.

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
7. ⚠ **Cross-group state — still open, and now more expensive.** The design assumes stages
   are independent per group, with `begin_run`/`end_run` as the only cross-group hooks.
   Groups are segments of one train (§6.6), so an algorithm that carries adaptive state
   *between* them in order — a tracker, a deinterleaver, an adaptive equaliser — is the
   expected case rather than the exception, and group parallelism would have to become
   opt-out per stage. M5 was the cheap moment and it passed: the scheduler was written
   without the flag, `StageDescriptor` has `pure` but no `sequential`, and every stage
   instance serves exactly one group because `process` takes `&mut self`. What M9 added is
   not this: a `sequential` *library* gets a lock, which is mutual exclusion rather than
   ordered delivery (§9.9). Adding it now means a second execution mode in the scheduler —
   one instance per stage, groups fed to it in order, no fan-out — which is a day's work
   and a second path through the part of the system every run depends on. **Decide when a
   real algorithm needs it**, and take the cost then rather than building a mode nothing
   asks for.
8. **External-stage ABI shape (§9.9).** The C ABI assumes one group per call with
   borrowed input buffers and library-allocated outputs. If the libraries to be tested are
   really stream-oriented (fed pulse by pulse, holding state across calls) or expect the
   host to allocate output buffers up front, the ABI changes shape rather than extends. M9
   built the one-group-per-call shape and a conforming sample library against it, so the
   question is now what a real vendor library makes of it rather than what to build.
9. **Stage output signal count.** ✅ *Settled at M5, confirmed at M9.* A stage adds and
   drops freely, and the rule that makes it safe is the accounting one: an output must say
   what happened to every input, so `check_covers` refuses an output that quietly forgets
   one. M9 carried the same rule across the C ABI as a disposition byte per input signal
   (§9.9), which makes it structural rather than a matter of trusting the library. A group's
   declared `count` is the number of *pulse records* in the source block (§7.2), not a
   constraint on how many signals a stage may produce, so the two never had to agree.
10. **Retention default.** ✅ *Settled at M10 (2026-09-15): `Always` with a size cap.*
   Content addressing makes passthrough free, so the default keeps everything and costs
   nothing for the stages that change nothing. The pipeline that does cost — every stage
   rewriting every sample of a 100 M-sample signal, ~400 MB a stage a group — is met by the
   run-level cap (§9.5), which stops keeping samples while still recording what each stage
   did and measured, and by `OnFailure` per stage for the intermediates only a failure is
   read for. `OnFailure` as the default was rejected: the common case is a workbench being
   iterated on, where the run that passed is the one to compare against next.
11. **Artifact size ceiling.** 64 KB inline / blob beyond that is a guess, and still
   barely tested: of the seven artifact kinds that exist (§10.1) only `Bits` and `Symbols`
   can grow with the capture, and a long bitstream crosses the line into a blob without
   anything noticing — which is the design working, but on a payload of megabytes rather
   than the hundreds the ceiling was picked for. The kind that would strain it, a
   fine-resolution spectrogram, still arrives with the STFT stage nobody has written. If
   that turns out to be routine, the spectrogram artifact should store a decimated pyramid
   the way signals do.

### Playback — resolved at M4

12. **Complex signals on the scope (part of §17.15).** A `c64` column reduces to
    **magnitude** in the pyramid, because a min/max pair is a real-valued idea; the scope
    draws that envelope mirrored about zero, which is the shape an I/Q capture is read by.
    Constellation and separate I/Q traces need either a second pyramid per component or a
    complex-aware cell, and stay V1.x. Nothing about the stored format has to change for
    them: a pyramid is derived data and can be rebuilt in a new shape.
13. **Irregular signals on the scope.** A signal whose times come from a companion time
    column has no arithmetic index-to-time map, so the reducer cannot pick a pyramid cell
    by span. It reports the trace as undrawable rather than guessing, and no such signal
    exists in practice yet — import writes pulse groups, generation writes regular
    signals. The shape of the answer is known: a pyramid over the **time** column gives
    each cell's time extent, and pairing it with the value column's pyramid at the same
    level makes an irregular trace as cheap as a regular one. It belongs with pulse-field
    plotting, which the Results screen wants anyway (M6).

### Data model

14. **Numeric precision.** Default storage is `f32` for *signals* (halves memory, ample for
    most captured signals); `f64` is available per signal. **Pulse columns resolved to `f64`
    at M2** — G1 requires the CSV round trip to be lossless, and `f32` cannot promise it, so
    narrowing a pulse column is an explicit per-column choice (§7.3). Processing is always
    done in `f64` internally and narrowed on write — confirm that narrowing on every stage
    boundary is acceptable, or whether intermediate stages should stay `f64` end-to-end.
15. **Complex signals.** `c64` is in the dtype enum and `BasebandIq` is a domain, but full
    complex support (complex-aware pyramids, constellation rendering) is scheduled for
    V1.x. Confirm whether I/Q pairs arrive as two real signals or as one complex one — this
    affects M2, not just the renderer.
16. **Library scale.** Design targets 10 000 signals / ~100 GB of samples, plus run
    storage. An order of magnitude beyond that would argue for a columnar store.

---

## 18. Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| A later file shape differs from `sample/sample.csv` (§17.1–17.4) | Rework of `sp-csv` | Grammar confirmed against a real file; every framing constant (preamble length, count column, time column and unit, delimiter) is an import-profile setting rather than a literal |
| Per-pulse annotation demand outgrows the lazy `pulse` table (§17.5) | Model rework at M2+ | Identity is `(group, index)` either way, so materialising rows later is an additive migration, not a re-import |
| Intermediate-result storage grows unbounded | Library bloats, disk fills | Content-addressed dedup makes passthrough free; per-stage retention policy; run-level size cap; prune-old-runs maintenance action |
| Linear pipeline too restrictive for real algorithms | Model rework | Typed ports cover the common "needs an earlier artifact" case and have carried every pipeline written so far, M9's external stage included; the `Stage` trait is still graph-ready. §17.6 went unsettled through M5 and is now answered by use rather than by decision |
| Stage authors write non-deterministic stages | G8 silently false | *Mitigated.* An impure stage opts out of caching through `descriptor().pure`, a run records the hash of everything a stage read, and the conformance harness (§14) runs a pure stage's group twice — through the same instance and a fresh one — and compares a digest of everything it produced. It caught the first stage that lied about being pure the day it was written (§9.9). It is a test rather than a guarantee: a stage nobody runs through it is still unchecked, which is why every built-in and the reference library are |
| A user algorithm panics or hangs | Run lost, app unstable | *Open.* A stage that returns an error already fails one group and no more, the store's writer thread is guarded, and the conformance harness catches a panic on the degenerate inputs it knows to try — but there is no `catch_unwind` around `Stage::process` in the scheduler and no per-stage timeout, so a panicking or hanging algorithm still takes the run. It belongs with M14, which has to solve the harder version of the same problem for a native library (§9.9, §14) |
| Iced canvas performance at 8+ dense traces plus overlays | Misses G2 | Pyramid decimation caps draw cost at viewport width; layered caches; `wgpu` backend; fall back to instanced GPU line rendering if needed |
| Iced API churn between releases | Build breakage | Pin the minor version; isolate all Iced usage in `sp-app` |
| WAL growth and write amplification during a large import | Slow import, transient disk use several times the payload | 4 MiB chunks with `wal_autocheckpoint` tuned to match; import commits per group, not per file; measured against the §13 ingest budget |
| Library file stays large after data is deleted | Disk not reclaimed | Blob refcounting frees rows on delete and `Reclaim unused blobs` sweeps what nothing references; `VACUUM` is written in `sp-store` but has no button yet, so returning the pages to the filesystem currently needs `sqlite3` (§15.6) |
| Scope creep from §15 | Milestones slip | Tiers are contractual: nothing beyond **[MVP]** entered v1, and a post-v1 entry moves to **[done]** only when a milestone delivered it. The v1.3 pass found the failure mode this guards against runs both ways — §15 had generation work filed as *near-term* that M3 had already built, and §9.8 had stages written up as built that nobody had started |
