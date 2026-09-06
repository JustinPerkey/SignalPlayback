# SignalPlayback

A desktop workbench for building a library of time-domain signals and running
signal-processing algorithms over them: import or generate signals, store them
in a local database, run them through a pipeline of algorithm stages a group at
a time, inspect what every stage produced, and play any of it back on an
on-screen scope.

See [`docs/DESIGN.md`](docs/DESIGN.md) for the full design.

## Status

**M7 — Regression.** A pipeline is now a test. A pipeline carries assertions —
one line each, evaluated per group after the last stage — over the metrics,
signal statistics, artifacts and stage timings the run recorded:

```text
detections.count == 4
metrics.snr_db > 12.0
signals["envelope"].rms within 5% of baseline
stage[3].wall_ms < 250
```

A run can be promoted to a named baseline with tolerances, and any two runs
diff group by group: per-signal max absolute error, RMS error and the first
sample that differs, a field-level artifact diff, and a metric-by-metric
comparison. The same binary runs headless, so the whole loop is a CI gate
(goal G8) — exit 1 says the request was wrong, exit 2 says the algorithm
regressed.

Before it, **M6 — Results**: any (group, stage) of a run is inspectable with
its artifacts, drawn by their own view hints, with a stage rail that walks the
algorithm, a global playhead, a metrics chart across groups, and stage-to-stage
comparison by pinning. **M5 — Pipeline**: the `Stage` trait, registry, port
typing, group-at-a-time scheduler and full run recording. **M4 — Playback**:
render pyramids, viewport reduction, transport and scope. **M3 — Generate**:
the `GenSpec` tree, primitives, combinators, sweeps and pulse trains.
**M2 — Import**: the grouped-block CSV framer, mapping UI and round-tripping
writer (G1). **M1 — Store**: one SQLite file, chunked blob store, property
index and `Verify Library`.

Polish — the inspector, tags and search, export, settings and packaging — is
the last milestone (§16).

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
| `sp-core` | Domain vocabulary: signals, groups, time, properties, artifacts, statistics | M0 |
| `sp-store` | SQLite schema and migrations, chunked blob store, store actor, property index, pulse search, verify | M1 |
| `sp-csv` | Grouped-block CSV framer, parser, import profiles, writer | M2 |
| `sp-gen` | `GenSpec` node tree and its renderer | M3 |
| `sp-engine` | Transport state machine, playback clock, render pyramids | M4 |
| `sp-proc` | `Stage` trait, registry, ports, scheduler, run recording, assertions, run diffing | M5, M7 |
| `sp-dsp` | Built-in stages: conditioning, filtering, transforms, detection | M5 |
| `sp-app` | The Iced application — the only crate that knows about pixels | M0+ |

Inside `sp-store`, row-level functions (`library`, `props`, `pulses`, `blob`,
`profiles`, `regress`, `verify`) take a connection and do one thing; `Store` owns the connections and
runs closures on the writer thread (`write`) or a pooled reader (`read`).

## Keyboard

| Keys | Action |
|------|--------|
| `Ctrl`+`1`…`9`, `Ctrl`+`0` | Jump to a screen |
| `Ctrl`+`T` | Toggle light/dark theme |

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
