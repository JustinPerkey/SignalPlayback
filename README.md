# SignalPlayback

A desktop workbench for building a library of time-domain signals and running
signal-processing algorithms over them: import or generate signals, store them
in a local database, run them through a pipeline of algorithm stages a group at
a time, inspect what every stage produced, and play any of it back on an
on-screen scope.

See [`docs/DESIGN.md`](docs/DESIGN.md) for the full design.

## Status

**M0 — Skeleton.** The workspace, the domain model and the application shell
are in place: the app launches, screens switch, and the library, pipeline and
playback surfaces are stubs awaiting their milestones (§16 of the design).

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
| `sp-store` | SQLite schema and migrations, content-addressed blob store | M1 |
| `sp-csv` | Grouped-block CSV framer, parser, import profiles, writer | M2 |
| `sp-gen` | `GenSpec` node tree and its renderer | M3 |
| `sp-engine` | Transport state machine, playback clock, render pyramids | M4 |
| `sp-proc` | `Stage` trait, registry, ports, scheduler, run recording | M5 |
| `sp-dsp` | Built-in stages: conditioning, filtering, transforms, detection | M5 |
| `sp-app` | The Iced application — the only crate that knows about pixels | M0+ |

## Keyboard

| Keys | Action |
|------|--------|
| `Ctrl`+`1`…`9`, `Ctrl`+`0` | Jump to a screen |
| `Ctrl`+`T` | Toggle light/dark theme |

## Where things live

A library is a single SQLite file — metadata and sample data both — so backing
one up is copying one file. It and the rolling log default to
`%LOCALAPPDATA%\SignalPlayback` on Windows, and to the platform equivalent
elsewhere. Set `RUST_LOG` to change log verbosity (default:
`warn,sp_app=info,sp_core=info`).

## Licence

MIT — see [LICENSE](LICENSE).
