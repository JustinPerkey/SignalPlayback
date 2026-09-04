# SignalPlayback

A desktop workbench for building a library of time-domain signals and running
signal-processing algorithms over them: import or generate signals, store them
in a local database, run them through a pipeline of algorithm stages a group at
a time, inspect what every stage produced, and play any of it back on an
on-screen scope.

See [`docs/DESIGN.md`](docs/DESIGN.md) for the full design.

## Status

**M2 — Import.** CSV files reach the library. The grouped-block format (§7 of
the design) is framed by its count column alone, previewed from its first
group however large the file, and streamed in one pass into one column blob
per pulse field — with a column-mapping panel for the count and time columns,
the time unit, per-column storage and property binding; saved import profiles
for recurring file shapes; strict or tolerant count handling with a positioned
error list; and cancellable progress. The writer reverses the grammar exactly,
so import → database → export round-trips (goal G1).

Before it, **M1 — Store**: one SQLite file holding metadata and sample data,
with numbered migrations, a chunked content-addressed blob store read through
incremental blob I/O, a single-writer/many-reader store actor, user-defined
property definitions with an indexed query mirror, pulse groups with zone-map
cross-group search, and a `Verify Library` pass.

Generation, playback and processing are the next milestones (§16).

## Build and run

Requires a stable Rust toolchain (1.85+).

```sh
cargo run -p sp-app          # launch the application
cargo test --workspace       # run the test suite
cargo clippy --workspace --all-targets -- -D warnings
```

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
| `sp-proc` | `Stage` trait, registry, ports, scheduler, run recording | M5 |
| `sp-dsp` | Built-in stages: conditioning, filtering, transforms, detection | M5 |
| `sp-app` | The Iced application — the only crate that knows about pixels | M0+ |

Inside `sp-store`, row-level functions (`library`, `props`, `pulses`, `blob`,
`profiles`, `verify`) take a connection and do one thing; `Store` owns the connections and
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
