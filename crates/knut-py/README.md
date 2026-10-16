# knut
<p align="center"><img src="https://raw.githubusercontent.com/mocnik-science/knut/main/crates/knut-web/assets/knut.svg" alt="knut" width="240"></p>

> *knut* is short for *knutpunkt*, the Swedish word for ‘junction’ or ‘node’, one of the building blocks of OpenStreetMap data.

This package gives Python fast access to OpenStreetMap history stored in knut databases (`.knutdb`): a compact, memory-mapped data format with a space-time index.  It reads versions of elements, snapshots, and changes, and runs scripts in the knut query language.

## Installation

```bash
pip install knut
```

For Arrow, polars, DuckDB, and pandas, also install `pyarrow`:

```bash
pip install "knut[arrow]"
```

A knut database is built from an OpenStreetMap PBF file (with history) with the command line tool:

```bash
cargo install knut-cli
```

```bash
knut build bremen-internal.osh.pbf -o bremen.knutdb
```

## Queries

```python
import knut

db = knut.open("bremen.knutdb")

db.get("way", 4040158)                       # all versions of an element
db.at("node", 1, "2020-01-01")               # the version valid at a time
roads = db.snapshot((8.80, 53.07, 8.82, 53.08), "2024-01-01", key="highway", types=["way"])
edits = db.changes(None, "2024-01-01", "2024-02-01", key="amenity")   # None = entire file
db.way_geometry_at(4040158, "2020-01-01")    # [(node_id, lon, lat)], lon/lat None if unknown
```

Bounding boxes are `(lon_min, lat_min, lon_max, lat_max)`; `lon_min > lon_max` crosses the antimeridian.  Times are `YYYY-MM-DD`, `YYYY-MM-DDTHH:MM:SSZ`, Unix seconds, or `datetime` objects.  Results can be filtered by `key`, `value`, `types`, `uid`, and `changeset`.

## Arrow, polars, DuckDB, pandas

Results implement the Arrow PyCapsule interface, so they can be passed on without copying:

```python
import pyarrow as pa, polars as pl, duckdb

t = pa.table(roads)
df = pl.DataFrame(roads)
duckdb.sql("select count(*) from t")
t.to_pandas()
roads.to_pylist()                            # without pyarrow
```

## The knut query language

```python
outputs = knut.run("count of nodes n with n:amenity=bench", db="bremen.knutdb")
outputs[0]["value"]

outputs = knut.run_file("analyse.knut")      # knut_db relative to the file
```

Each output is a dict with `line`, `expr`, `seconds`, and `value`.  Numbers with a unit are `{"value": …, "unit": …}`, groups are lists of dicts.  Warnings are reported through the `warnings` module; errors raise `knut.KnutScriptError`.

The language is documented in the repository and in the web interface, started with `knut web --db bremen.knutdb`.

## Links

https://github.com/mocnik-science/knut

## Author

This package is written and maintained by Franz-Benjamin Mocnik, <mail@mocnik-science.net>.

(c) by Franz-Benjamin Mocnik, 2023–2026.

Its parts written after September 2023 have been supported by the *EXDIGIT (Excellence in Digital Sciences and Interdisciplinary Technologies)* project, funded by Land Salzburg under grant number 20204-WISS/263/6-6022.
