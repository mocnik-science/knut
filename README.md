# knut

> *knut* is short for *knutpunkt*, the Swedish word for ‘junction’ or ‘node’, one of the building blocks of OpenStreetMap data.

This software reads OpenStreetMap PBF files (with history) and converts them into its own compact, high-performance data format, which enables fast queries of OpenStreetMap data in Rust, in Python, on the command line, and in a web interface, using the knut query language.

## Overview

| Crate | Inhalt |
|-------|--------|
| `crates/knut-format` | Structures, codecs (varint/zigzag/FOR bit-packing), block encoding, layouts. |
| `crates/knut-builder` | Custom history PBF reader, node classification, segmentation, block building, writer. |
| `crates/knut` | mmap reader, query engine, Arrow export, benchmarks, integration tests. |
| `crates/knut-cli` | Binary `knut` |
| `crates/knut-py` | Python-Paket `knut` (PyO3 + maturin). |

## Build

```bash
cargo build --release
```

```bash
cargo test --release
```

## CLI

```bash
./target/release/knut build data/dump/bremen-internal.osh.pbf -o data/knut/bremen.knutdb
```

```bash
./target/release/knut info data/knut/bremen.knutdb
```

```bash
./target/release/knut query data/knut/bremen.knutdb snapshot 8.80,53.07,8.82,53.08 2024-01-01 --tag highway --types way
```

```bash
./target/release/knut query data/knut/bremen.knutdb changes 8.80,53.07,8.82,53.08 2023-01-01 2024-01-01 --count
```

```bash
./target/release/knut query data/knut/bremen.knutdb get way 4040158
```

```bash
./target/release/knut query data/knut/bremen.knutdb geom-changes 4040158
```

```bash
./target/release/knut verify data/knut/bremen.knutdb --deep --against data/dump/bremen-internal.osh.pbf --exact 200
```

Important build options (all default values chosen via benchmarks):

| Option | Default | Meaning |
|--------|---------|---------|
| `--block-kib` | 256 | Target size of a block (uncompressed) |
| `--zstd-level` | 19 | zstd level |
| `--no-dict` | off | Disable trained zstd dictionaries |
| `--keyframe` | 32 | Keyframe interval for node/member lists |
| `--seg-grow` / `--seg-min-size` / `--seg-jump` | 4 / 1000 m / 10 km | Segmentation thresholds X, S_min, Y |
| `--large-deg` | 1.0 | Threshold for the large-object section |
| `--mem-gb` | 40 % of RAM | Memory budget; larger intermediate data is sorted externally |
| `--tmp-dir` | next to the output | Directory for temporary files |
| `--s2` | off | S2-style instead of Hilbert sort key (experiment) |
| `--strip-users` | off | Omit usernames |
| `--pseudonymize-uids SECRET` | – | Replace uids with pseudonyms (implies `--strip-users`) |

`knut query` outputs JSON Lines. Times: `YYYY-MM-DD`, `YYYY-MM-DDTHH:MM:SSZ`, or Unix seconds.  Bbox: `lon_min,lat_min,lon_max,lat_max`; `lon_min > lon_max` crosses the antimeridian.

## Rust API

```rust
use knut::{Reader, Filter, BBox, ObjType};

let r = Reader::open("data/knut/bremen.knutdb")?;
let history = r.get(ObjType::Way, 4040158)?;                  // all versions
let v = r.at(ObjType::Node, 1, 1_600_000_000)?;               // Version for t
let bbox = BBox::from_deg(8.80, 53.07, 8.82, 53.08);
let roads = r.snapshot(&bbox, 1_700_000_000, &Filter::default().tag("highway", None))?;
let edits = r.changes(&bbox, 1_670_000_000, 1_700_000_000, &Filter::default())?;
let geom = r.way_geometry_at(4040158, 1_600_000_000)?;        // [(node_id, Some((lon, lat)))]
```

## Python

```bash
cd crates/knut-py && uv venv .venv && uv pip install --python .venv/bin/python maturin pyarrow
```

```bash
cd crates/knut-py && VIRTUAL_ENV=$PWD/.venv .venv/bin/maturin develop --release
```

```python
import knut, pyarrow as pa, polars as pl, duckdb

db = knut.open("data/knut/bremen.knutdb")
roads = db.snapshot((8.80, 53.07, 8.82, 53.08), "2024-01-01", key="highway", types=["way"])
t = pa.table(roads)                      # Arrow PyCapsule, ohne Kopie
df = pl.DataFrame(roads)
duckdb.sql("select count(*) from t")
db.get("way", 4040158).to_pylist()
db.changes(None, "2024-01-01", "2024-02-01", key="amenity")   # None = entire file
db.way_geometry_at(4040158, "2020-01-01")
```

Columns of the result tables.

## Web

```bash
./target/release/knut web --db data/knut/bremen.knutdb
```

## Tests

```bash
cargo bench -p knut-exec --bench constructs -- --save-baseline previous
python3 scripts/bench_lang.py previous new
```

## Privacy

History files contain personal data (uids, usernames). `--strip-users` removes the username table, `--pseudonymize-uids` replaces all uids with key-dependent pseudonyms.

## Building all extracts

`scripts/build_all.sh` builds all PBFs from `data/dump/` into `data/knut/` and verifies them against the original (logs in `data/knut/logs/`).

## Author

This application is written and maintained by Franz-Benjamin Mocnik, <mail@mocnik-science.net>.

(c) by Franz-Benjamin Mocnik, 2023-2026.

Its parts written after September 2023 have been supported by the *EXDIGIT~(Excellence in Digital Sciences and Interdisciplinary Technologies)* project, funded by Land Salzburg under grant number 20204-WISS/263/6-6022.

The code is licensed under the [MIT License](https://github.com/mocnik-science/knut/blob/main/LICENSE).
