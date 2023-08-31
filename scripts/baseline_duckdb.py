#!/usr/bin/env python3
"""Compare knut against a Parquet + DuckDB baseline on equivalent queries.

usage: baseline_duckdb.py FILE.knutdb PARQUET_DIR lon0,lat0,lon1,lat1
Results of node queries are cross-checked (counts must match).
"""
import sys, time, statistics, datetime as dt
import duckdb, knut, pyarrow as pa

knut_file, pq, bbox_s = sys.argv[1], sys.argv[2], sys.argv[3]
bb = tuple(float(x) for x in bbox_s.split(","))
db = knut.open(knut_file)
info = db.info()
t_now = info["time_range"][1]
t_1y = t_now - 365 * 86400
con = duckdb.connect()
con.execute("PRAGMA threads=16")
for t in ("nodes", "ways", "relations"):
    con.execute(f"create view {t} as select * from read_parquet('{pq}/{t}.parquet')")

def ts(t):
    # arrow-rs stores second-resolution timestamps as plain INT64 seconds in Parquet
    return int(t)

def bench(fn, n=5):
    times, res = [], None
    for _ in range(n):
        s = time.perf_counter(); res = fn(); times.append((time.perf_counter() - s) * 1e3)
    return res, times[0], statistics.median(times)

ids = {
    "node": [r[0] for r in con.execute("select id from nodes where version=1 using sample 200 rows (reservoir, 7)").fetchall()],
    "way": [r[0] for r in con.execute("select id from ways where version=1 using sample 200 rows (reservoir, 7)").fetchall()],
}
rows = []
def row(name, o, d, check=True):
    (ro, of, om), (rd, df, dm) = o, d
    ok = "" if not check else ("ok" if ro == rd else f"MISMATCH {ro} vs {rd}")
    rows.append((name, ro, om, rd, dm, ok))

for t, tbl in (("node", "nodes"), ("way", "ways")):
    row(f"get({t}) x200",
        bench(lambda: sum(len(db.get(t, i)) for i in ids[t])),
        bench(lambda: sum(con.execute(f"select count(*) from {tbl} where id=?", [i]).fetchone()[0] for i in ids[t])))

x0, y0, x1, y1 = bb
for name, t in (("snapshot nodes bbox now", t_now), ("snapshot nodes bbox -1y", t_1y)):
    row(name,
        bench(lambda: len(db.snapshot(bb, t, types=["node"]))),
        bench(lambda: con.execute(f"""select count(*) from nodes where valid_from <= {ts(t)} and (valid_to is null or valid_to > {ts(t)})
              and visible and lon between {x0} and {x1} and lat between {y0} and {y1}""").fetchone()[0]))
row("changes nodes bbox 1y",
    bench(lambda: len(db.changes(bb, t_1y, t_now, types=["node"]))),
    bench(lambda: con.execute(f"""select count(*) from nodes where valid_from between {ts(t_1y)} and {ts(t_now)}
          and (not visible or (lon between {x0} and {x1} and lat between {y0} and {y1}))""").fetchone()[0]), check=False)
row("snapshot amenity (nodes+ways) world",
    bench(lambda: len(db.snapshot(None, t_now, key="amenity", types=["node", "way"]))),
    bench(lambda: con.execute(f"""select (select count(*) from nodes where valid_from <= {ts(t_now)} and (valid_to is null or valid_to > {ts(t_now)}) and visible and tags['amenity'] is not null)
          + (select count(*) from ways where valid_from <= {ts(t_now)} and (valid_to is null or valid_to > {ts(t_now)}) and visible and tags['amenity'] is not null)""").fetchone()[0]))
row("snapshot amenity world, geometry=False",
    bench(lambda: len(db.snapshot(None, t_now, key="amenity", types=["node", "way"], geometry=False))),
    bench(lambda: con.execute(f"""select (select count(*) from nodes where valid_from <= {ts(t_now)} and (valid_to is null or valid_to > {ts(t_now)}) and visible and tags['amenity'] is not null)
          + (select count(*) from ways where valid_from <= {ts(t_now)} and (valid_to is null or valid_to > {ts(t_now)}) and visible and tags['amenity'] is not null)""").fetchone()[0]))
row("changes building=yes world 1y, geometry=False",
    bench(lambda: len(db.changes(None, t_1y, t_now, key="building", value="yes", geometry=False))),
    bench(lambda: sum(con.execute(f"""select count(*) from {tb} where valid_from between {ts(t_1y)} and {ts(t_now)} and tags['building'] = 'yes'""").fetchone()[0] for tb in ("nodes","ways","relations"))))
row("changes building=yes world 1y",
    bench(lambda: len(db.changes(None, t_1y, t_now, key="building", value="yes"))),
    bench(lambda: sum(con.execute(f"""select count(*) from {tb} where valid_from between {ts(t_1y)} and {ts(t_now)} and tags['building'] = 'yes'""").fetchone()[0] for tb in ("nodes","ways","relations"))))
row("snapshot highway ways bbox (with geometry)",
    bench(lambda: len(db.snapshot(bb, t_now, key="highway", types=["way"]))),
    bench(lambda: con.execute(f"""
        with w as (select id, nodes from ways where valid_from <= {ts(t_now)} and (valid_to is null or valid_to > {ts(t_now)}) and visible and tags['highway'] is not null),
             n as (select id, lon, lat from nodes where valid_from <= {ts(t_now)} and (valid_to is null or valid_to > {ts(t_now)}) and visible),
             wn as (select w.id wid, unnest(w.nodes) nid from w)
        select count(distinct wid) from wn join n on n.id = wn.nid where n.lon between {x0} and {x1} and n.lat between {y0} and {y1}""").fetchone()[0]), check=False)

print(f"{'query':<42} {'knut n':>9} {'knut ms':>9} {'duckdb n':>9} {'duckdb ms':>10}  check")
for r in rows:
    print(f"{r[0]:<42} {r[1]:>9} {r[2]:>9.1f} {r[3]:>9} {r[4]:>10.1f}  {r[5]}")
