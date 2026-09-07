# Fonts

These files are compiled into the binary by `src/typography.rs`. The
application does not fall back to a platform face: a column of samples has to
lay out the same on every machine it is read on.

| File | Family | Weight |
|---|---|---|
| `Archivo-Regular.ttf` | Archivo | 400 |
| `Archivo-Medium.ttf` | Archivo | 500 |
| `Archivo-SemiBold.ttf` | Archivo | 600 |
| `ArchivoExpanded-SemiBold.ttf` | Archivo Expanded | 600 |
| `ArchivoCondensed-Medium.ttf` | Archivo Condensed | 500 |
| `MartianMonoCondensed-Regular.ttf` | Martian Mono Condensed | 400 |

The family names above are the typographic family (`name` ID 16), which is
what Iced's font database matches on — which is why three weights of Archivo
are one family rather than three. `typography::tests::every_face_has_bytes_loaded`
reads them back out of these files and checks them against what the code asks
for, so a renamed or replaced file fails the test rather than silently
rendering as something else.

Both families are static instances. Their variable originals carry a width
axis, but Iced exposes no way to set one, so each width the design uses is
shipped as its own file.

## Licences

Both families are licensed under the SIL Open Font License, Version 1.1. The
full text of each is beside the fonts:

- **Archivo** — Copyright The Archivo Project Authors
  (<https://github.com/Omnibus-Type/Archivo>). See `OFL-Archivo.txt`.
- **Martian Mono** — Copyright The Martian Mono Project Authors
  (<https://github.com/evilmartians/mono>). See `OFL-MartianMono.txt`.

The OFL permits embedding in a program and redistribution of the program;
neither font is sold on its own, and neither is renamed. `LICENSE` at the root
of the repository covers the application itself and not these files.
