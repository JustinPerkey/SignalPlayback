# SignalPlayback

A desktop workbench for building a library of time-domain signals and running
signal-processing algorithms over them: import or generate signals, store them
in a local database, run them through a pipeline of algorithm stages a group at
a time, inspect what every stage produced, and play any of it back on an
on-screen scope.

See [`docs/DESIGN.md`](docs/DESIGN.md) for the full design.

## Status

**M8 — Polish.** The last milestone: the loop runs end to end and the binary is
releasable.

- **Inspector** — one signal or pulse field in full: its metadata, an editable
  name, a property editor over the declared definitions (unrecognised
  attributes shown, never dropped), tags, statistics recomputed from the
  samples in a single streaming pass — min, max, peak-to-peak, mean, RMS,
  standard deviation, zero crossings — the distribution behind them, and a
  paged value table. A pulse group's table is the pulse records themselves,
  one row per pulse across every field.
- **Tags and search** — a tag filter beside full-text search on the Library
  screen. Both narrow: a query plus two tags asks for the signals that match
  the text *and* carry both.
- **Export** — a dataset writes back out in the format it was imported from,
  from the Library screen or from `signalplayback export`, so import → run →
  export is scriptable (G1).
- **Settings** — library location (it reopens in place), theme, default sample
  rate, strict/tolerant import, run retention, decimation quality and
  histogram bins, saved to `settings.json` as they change, plus what the open
  library is made of and a sweep for unreferenced blobs.
- **Runs** — the history of what has been run: pipeline, algorithm hash,
  status, timing, assertion outcomes and the baselines that name each run,
  with promote, delete, and `Open`/`Diff` that hand the run to the Results
  screen rather than drawing a second diff. Every screen in the design is now
  built.
- **Packaging** — a tagged release builds and publishes the binary for Windows
  and Linux.

Before it, **M7 — Regression**: a pipeline is a test. It carries assertions —
one line each, evaluated per group after the last stage — over the metrics,
signal statistics, artifacts and stage timings the run recorded:

```text
detections.count == 4
metrics.snr_db > 12.0
signals["envelope"].rms within 5% of baseline
stage[3].wall_ms < 250
```

A run can be promoted to a named baseline with tolerances, and any two runs
diff group by group. The same binary runs headless, so the whole loop is a CI
gate (goal G8) — exit 1 says the request was wrong, exit 2 says the algorithm
regressed.

**M6 — Results**: any (group, stage) of a run is inspectable with its
artifacts, drawn by their own view hints, with a stage rail that walks the
algorithm, a global playhead, a metrics chart across groups, and stage-to-stage
comparison by pinning. **M5 — Pipeline**: the `Stage` trait, registry, port
typing, group-at-a-time scheduler and full run recording. **M4 — Playback**:
render pyramids, viewport reduction, transport and scope. **M3 — Generate**:
the `GenSpec` tree, primitives, combinators, sweeps and pulse trains.
**M2 — Import**: the grouped-block CSV framer, mapping UI and round-tripping
writer (G1). **M1 — Store**: one SQLite file, chunked blob store, property
index and `Verify Library`.

## Build and run

Requires a stable Rust toolchain (1.85+).

```sh
cargo run -p sp-app          # launch the application
cargo test --workspace       # run the test suite
cargo clippy --workspace --all-targets -- -D warnings
```

## Running a pipeline headlessly

The same binary is the window and the test runner. With no arguments it opens
the application; with a subcommand it runs to completion on the terminal:

```sh
signalplayback run --library lib.db --pipeline "detector" \
                   --dataset "impairment ladder" --assert-baseline golden
signalplayback run --library lib.db --pipeline "detector" --promote golden
signalplayback baselines --library lib.db
signalplayback export --library lib.db --dataset "capture 1" --out out.csv
signalplayback help
```

| Exit | Meaning |
|------|---------|
| 0 | the run finished and everything it was asked to check passed |
| 1 | the request was wrong — bad arguments, no such pipeline, an unreadable library |
| 2 | the run finished and failed: a stage error, a failed assertion, or a deviation from the baseline |

## Workspace

Dependencies point left-to-right only: `sp-core` depends on nothing else here,
`sp-app` may depend on everything, and nothing depends on `sp-app`.

| Crate | Contents | Milestone |
|-------|----------|-----------|
| `sp-core` | Domain vocabulary: signals, groups, time, properties, artifacts, statistics, histograms | M0, M8 |
| `sp-store` | SQLite schema and migrations, chunked blob store, store actor, property index, pulse search, verify, tags, column profiling and storage figures | M1, M8 |
| `sp-csv` | Grouped-block CSV framer, parser, import profiles, writer | M2 |
| `sp-gen` | `GenSpec` node tree and its renderer | M3 |
| `sp-engine` | Transport state machine, playback clock, render pyramids | M4 |
| `sp-proc` | `Stage` trait, registry, ports, scheduler, run recording, assertions, run diffing | M5, M7 |
| `sp-dsp` | Built-in stages: conditioning, filtering, transforms, detection | M5 |
| `sp-app` | The Iced application — the only crate that knows about pixels | M0+ |

Inside `sp-store`, row-level functions (`library`, `props`, `pulses`, `blob`,
`profiles`, `regress`, `stats`, `verify`) take a connection and do one thing; `Store` owns
the connections and runs closures on the writer thread (`write`) or a pooled reader
(`read`).

## Keyboard

| Keys | Action |
|------|--------|
| `Ctrl`+`1`…`9`, `Ctrl`+`0` | Jump to a screen |
| `Ctrl`+`T` | Toggle light/dark theme |

## Settings

Settings live in `settings.json` in the application data directory — not in the
library, since one of them is which library to open. The Settings screen writes
each change as it is made:

| Setting | Effect |
|---------|--------|
| Library location | Which library opens; changing it reopens in place |
| Theme | Dark or light; `Ctrl`+`T` sets the same value |
| Default sample rate | The rate a new generator spec starts at |
| Strict / tolerant import | Whether an import refuses a file whose counts do not add up |
| Retention | Finished runs kept per pipeline; a run a baseline names is never deleted |
| Decimation quality | Shifts the scope's automatic pyramid level by one either way |
| Histogram bins | Resolution of the Inspector's histogram |

A settings file that will not parse, or one written by an older build, falls
back to the defaults for the fields it does not carry: losing a preference
never costs you the application.

## Where things live

A library is a single SQLite file — metadata and sample data both — so backing
one up is copying one file. It lives at
`%LOCALAPPDATA%\SignalPlayback\library\library.db` on Windows (the platform
equivalent elsewhere), beside the rolling log in `…\SignalPlayback\logs`. The
`-wal` and `-shm` files next to it are SQLite's own and are folded back into
the main file on a clean exit. Set `RUST_LOG` to change log verbosity (default:
`warn,signalplayback=info,sp_core=info,sp_store=info,sp_csv=info`).

Every table and every byte of column data is reachable with `sqlite3`:

```sh
sqlite3 library.db "SELECT id, name, sample_count FROM signal"
sqlite3 library.db "SELECT id, kind, byte_len, refcount FROM sample_blob"
```

## Licence

MIT — see [LICENSE](LICENSE).
