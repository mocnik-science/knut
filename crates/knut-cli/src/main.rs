//! `knut` command line: run scripts, build, info, query, verify, dump, bench.

mod manage;
mod parquet_export;

use clap::{Args, Parser, Subcommand};
use knut_builder::pbf::PbfFile;
use knut_builder::{BuildOptions, Element, Source};
use knut_format::time::{format_time, parse_time};
use knut_format::{BBox, ObjType, e7_to_deg};
use knut::{Feature, Filter, Geometry, OpenOptions, Reader};
use serde_json::{Value, json};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

#[derive(Parser)]
#[command(name = "knut", version, about = "knut: compact OSM history format with space-time index")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a .knut script (also: `knut script.knut`)
    Run(RunArgs),
    /// Web interface on a map for writing and running queries
    Web(WebArgs),
    /// Changesets with comments from a dump (into the container), notes (side file)
    Index {
        #[command(subcommand)]
        what: IndexCmd,
    },
    /// Convert an OSM history PBF (*.osh.pbf) into .knutdb
    Build(BuildArgs),
    /// Show what a database contains
    Info {
        file: PathBuf,
        /// Also the technical details (sections, blocks, trees, build parameters)
        #[arg(long)]
        detailed: bool,
        /// Also list the user table
        #[arg(long)]
        users: bool,
    },
    /// Look into a database and add, remove or compact its indexes (interactive without a command)
    Manage {
        file: PathBuf,
        #[command(subcommand)]
        cmd: Option<ManageCmd>,
    },
    /// Search objects by name (needs the name index)
    Search {
        file: PathBuf,
        text: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Find an address, e.g. "Am Wall 201, Bremen" (needs the address index)
    Geocode {
        file: PathBuf,
        address: String,
        #[arg(long, default_value_t = 5)]
        limit: usize,
    },
    /// The nearest addresses to a point lon,lat (needs the address index)
    Reverse {
        file: PathBuf,
        #[arg(allow_hyphen_values = true)]
        point: String,
        /// Search radius in metres
        #[arg(long, default_value_t = 100.0)]
        radius: f64,
        #[arg(long, default_value_t = 1)]
        limit: usize,
    },
    /// Run a query and print JSON lines
    Query {
        file: PathBuf,
        #[command(subcommand)]
        q: QueryCmd,
    },
    /// Verify checksums and structure; optionally compare with the source PBF
    Verify {
        file: PathBuf,
        /// Decode every block
        #[arg(long)]
        deep: bool,
        /// Compare all versions against this PBF (order independent digest)
        #[arg(long)]
        against: Option<PathBuf>,
        /// Additionally compare N objects per type exactly via get()
        #[arg(long, default_value_t = 0)]
        exact: usize,
    },
    /// Print a canonical text dump (one line per version, sorted by type/id/version)
    Dump {
        file: PathBuf,
        #[arg(long)]
        r#type: Option<String>,
    },
    /// Baseline: export a history PBF to Parquet (nodes/ways/relations.parquet)
    ExportParquet {
        input: PathBuf,
        dir: PathBuf,
        #[arg(long, default_value_t = 9)]
        zstd_level: i32,
    },
    /// Per-column size statistics (raw and individually zstd-compressed)
    Stats { file: PathBuf },
    /// Run reference queries and print latencies
    Bench {
        file: PathBuf,
        /// lon_min,lat_min,lon_max,lat_max (default: centered 0.05° box)
        #[arg(long, allow_hyphen_values = true)]
        bbox: Option<String>,
        #[arg(long, default_value_t = 5)]
        repeat: usize,
    },
}

#[derive(Args)]
struct WebArgs {
    /// Database used when a query names none (`knut_db`)
    #[arg(long)]
    db: Option<PathBuf>,
    /// Port (the next free one is taken if it is in use)
    #[arg(long, default_value_t = 8765)]
    port: u16,
    /// Address to listen on; the default only accepts connections from this computer
    #[arg(long, default_value = "127.0.0.1")]
    host: std::net::IpAddr,
    /// Do not open the page in the browser
    #[arg(long)]
    no_open: bool,
    /// URL of a MapLibre style for the base map (default: OpenStreetMap tiles)
    #[arg(long)]
    style: Option<String>,
    /// Server of a language model with an OpenAI-compatible API (llama.cpp, Ollama, vLLM),
    /// e.g. http://localhost:11434: enables the text interface for questions in plain language
    #[arg(long)]
    llm: Option<String>,
    /// Name of the model on that server (default: qwen3:1.7b)
    #[arg(long)]
    llm_model: Option<String>,
    /// Load all indexes of the database (routing network, hierarchies, name and address index,
    /// …) at the start, so that no query has to wait for them
    #[arg(long)]
    preload: bool,
}

#[derive(Args)]
struct RunArgs {
    script: PathBuf,
    /// Database (overrides `knut_db` in the script)
    #[arg(long)]
    db: Option<PathBuf>,
    /// Only check the script, run nothing
    #[arg(long)]
    check: bool,
    /// Show the execution plan, run nothing
    #[arg(long)]
    explain: bool,
    /// Output format: text, csv, json or parquet
    #[arg(long, default_value = "text")]
    format: String,
    /// Write results as files into this directory
    #[arg(long)]
    out: Option<PathBuf>,
    #[arg(long)]
    threads: Option<usize>,
    /// Rows shown per result in text output
    #[arg(long, default_value_t = 20)]
    limit: usize,
    /// Do not show the run time per result
    #[arg(long)]
    no_timing: bool,
}

#[derive(Subcommand)]
enum ManageCmd {
    /// List all kinds of elements and which exist
    Ls {
        /// With a short explanation of each
        #[arg(long)]
        explain: bool,
    },
    /// Build indexes and add them, one after the other: places, names, addresses, keys, changesets, routing, routing-ch, routing-cch
    Add {
        #[arg(required = true)]
        what: Vec<String>,
        /// latest (default), daily, weekly, monthly, yearly or full
        #[arg(long)]
        time: Option<String>,
    },
    /// Remove an element (by its id)
    Rm {
        id: u32,
        /// Do not ask
        #[arg(short, long)]
        yes: bool,
    },
    /// Rewrite the file without the space of removed elements
    Compact,
    /// Convert a file of format 1.0
    Upgrade,
}

#[derive(Subcommand)]
enum IndexCmd {
    /// Changesets and users derived from the history; optionally comments and tags from a
    /// changeset dump (changesets-latest.osm.bz2)
    Changesets {
        db: PathBuf,
        #[arg(long)]
        dump: Option<PathBuf>,
    },
    /// Notes from a notes dump (planet-notes-latest.osn.bz2)
    Notes { db: PathBuf, dump: PathBuf },
}

#[derive(Args)]
struct BuildArgs {
    input: PathBuf,
    #[arg(short, long)]
    output: PathBuf,
    /// Target uncompressed block size in KiB
    #[arg(long, default_value_t = 256)]
    block_kib: usize,
    #[arg(long, default_value_t = 19)]
    zstd_level: i32,
    /// Disable trained zstd dictionaries
    #[arg(long)]
    no_dict: bool,
    #[arg(long, default_value_t = 32)]
    keyframe: u32,
    /// Omit user names
    #[arg(long)]
    strip_users: bool,
    /// Replace uids by pseudonyms derived from this secret (implies --strip-users)
    #[arg(long)]
    pseudonymize_uids: Option<String>,
    /// Segmentation growth factor X
    #[arg(long, default_value_t = 4.0)]
    seg_grow: f64,
    /// Segmentation minimum size S_min in meters
    #[arg(long, default_value_t = 1000.0)]
    seg_min_size: f64,
    /// Segmentation jump distance Y in meters
    #[arg(long, default_value_t = 10000.0)]
    seg_jump: f64,
    /// Large-object threshold in degrees
    #[arg(long, default_value_t = 1.0)]
    large_deg: f64,
    #[arg(long, default_value_t = 16)]
    min_value_count: u64,
    /// Use S2-style cube-face cells as spatial sort key (experiment)
    #[arg(long)]
    s2: bool,
    /// Order segments by Hilbert key instead of id inside blocks (experiment)
    #[arg(long)]
    hilbert_in_block: bool,
    /// Memory budget in GiB for spillable buffers (default: 40 % of RAM); larger
    /// intermediate data is sorted externally on disk
    #[arg(long)]
    mem_gb: Option<f64>,
    /// Directory for temporary files (default: next to the output)
    #[arg(long)]
    tmp_dir: Option<PathBuf>,
    /// Do not add the place index, name index, address index and tag statistics
    #[arg(long)]
    no_index: bool,
    /// Number of worker threads (default: all cores)
    #[arg(long)]
    threads: Option<usize>,
}

#[derive(Subcommand)]
enum QueryCmd {
    /// All versions of an object
    Get { r#type: String, id: i64 },
    /// Version of an object at time T
    At { r#type: String, id: i64, time: String },
    /// Way geometry at time T
    Geom { id: i64, time: String },
    /// All geometry states of a way
    GeomChanges { id: i64 },
    /// Objects in BBOX at time T
    Snapshot {
        #[arg(allow_hyphen_values = true)]
        bbox: String,
        time: String,
        #[command(flatten)]
        f: FilterArgs,
    },
    /// Versions in BBOX between T0 and T1
    Changes {
        #[arg(allow_hyphen_values = true)]
        bbox: String,
        t0: String,
        t1: String,
        #[command(flatten)]
        f: FilterArgs,
    },
}

#[derive(Args, Clone)]
struct FilterArgs {
    /// Object types, e.g. "node,way"
    #[arg(long)]
    types: Option<String>,
    /// Tag filter: key or key=value
    #[arg(long)]
    tag: Option<String>,
    #[arg(long)]
    uid: Option<u32>,
    #[arg(long)]
    changeset: Option<u64>,
    /// Exclude private (untagged way-only) nodes
    #[arg(long)]
    no_private: bool,
    /// Print only the number of results
    #[arg(long)]
    count: bool,
    /// Skip geometry (faster attribute-only decoding for blocks inside the box)
    #[arg(long)]
    no_geometry: bool,
}

fn parse_bbox(s: &str) -> Result<BBox, String> {
    let v: Vec<f64> = s.split(',').map(|x| x.trim().parse::<f64>()).collect::<Result<_, _>>().map_err(|e| e.to_string())?;
    if v.len() != 4 {
        return Err("bbox must be lon_min,lat_min,lon_max,lat_max".into());
    }
    Ok(BBox::from_deg(v[0], v[1], v[2], v[3]))
}

fn time(s: &str) -> Result<i64, String> {
    parse_time(s).ok_or_else(|| format!("cannot parse time '{s}'"))
}

fn obj_type(s: &str) -> Result<ObjType, String> {
    ObjType::parse(s).ok_or_else(|| format!("unknown object type '{s}'"))
}

fn make_filter(f: &FilterArgs) -> Result<Filter, String> {
    let mut flt = Filter::default();
    if let Some(t) = &f.types {
        let ts: Vec<ObjType> = t.split(',').map(obj_type).collect::<Result<_, _>>()?;
        flt = flt.types(&ts);
    }
    if let Some(t) = &f.tag {
        match t.split_once('=') {
            Some((k, v)) => flt = flt.tag(k, Some(v)),
            None => flt = flt.tag(t, None),
        }
    }
    flt.uid = f.uid;
    flt.changeset = f.changeset;
    flt.include_private_nodes = !f.no_private;
    flt.geometry = !f.no_geometry;
    Ok(flt)
}

fn feature_json(f: &Feature) -> Value {
    let mut v = json!({
        "type": f.obj_type.name(),
        "id": f.id,
        "version": f.meta.version,
        "timestamp": format_time(f.meta.timestamp),
        "changeset": f.meta.changeset,
        "uid": f.meta.uid,
        "visible": f.meta.visible,
    });
    let o = v.as_object_mut().unwrap();
    if !f.tags.is_empty() {
        let tags: serde_json::Map<String, Value> =
            f.tags.iter().map(|(k, v)| (k.to_string(), Value::String(v.to_string()))).collect();
        o.insert("tags".into(), Value::Object(tags));
    }
    match &f.geometry {
        Geometry::Point(x, y) => {
            o.insert("lon".into(), json!(e7_to_deg(*x)));
            o.insert("lat".into(), json!(e7_to_deg(*y)));
        }
        Geometry::Line(pts) => {
            let c: Vec<Value> =
                pts.iter().map(|p| p.map_or(Value::Null, |(x, y)| json!([e7_to_deg(x), e7_to_deg(y)]))).collect();
            o.insert("coords".into(), Value::Array(c));
        }
        Geometry::None => {}
    }
    if f.obj_type == ObjType::Way {
        o.insert("nodes".into(), json!(f.nodes));
    }
    if f.obj_type == ObjType::Relation {
        let m: Vec<Value> =
            f.members.iter().map(|m| json!({"type": m.kind.name(), "ref": m.id, "role": m.role.as_ref()})).collect();
        o.insert("members".into(), Value::Array(m));
    }
    v
}

fn print_features(fs: &[Feature], count: bool) {
    if count {
        println!("{}", fs.len());
        return;
    }
    let out = std::io::stdout();
    let mut w = BufWriter::new(out.lock());
    for f in fs {
        let _ = writeln!(w, "{}", feature_json(f));
    }
}

fn geom_json(g: &[(i64, Option<(i32, i32)>)]) -> Value {
    Value::Array(
        g.iter()
            .map(|(n, p)| match p {
                Some((x, y)) => json!({"node": n, "lon": e7_to_deg(*x), "lat": e7_to_deg(*y)}),
                None => json!({"node": n}),
            })
            .collect(),
    )
}

fn main() -> ExitCode {
    // `knut analyse.knut …` runs a script
    let mut args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if args.len() > 1 && args[1].to_string_lossy().ends_with(".knut") {
        args.insert(1, "run".into());
    }
    let cli = Cli::parse_from(args);
    if let Cmd::Run(a) = cli.cmd {
        return ExitCode::from(run_script(a) as u8);
    }
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run_script(a: RunArgs) -> i32 {
    if let Some(t) = a.threads
        && let Err(e) = rayon::ThreadPoolBuilder::new().num_threads(t).build_global()
    {
        eprintln!("error: {e}");
        return knut_exec::EXIT_USAGE;
    }
    let format = match a.format.as_str() {
        "text" => knut_exec::Format::Text,
        "csv" => knut_exec::Format::Csv,
        "json" => knut_exec::Format::Json,
        "parquet" => knut_exec::Format::Parquet,
        other => {
            eprintln!("error: unknown format '{other}' (text, csv, json, parquet)");
            return knut_exec::EXIT_USAGE;
        }
    };
    if let Some(d) = &a.out
        && let Err(e) = std::fs::create_dir_all(d)
    {
        eprintln!("error: {}: {e}", d.display());
        return knut_exec::EXIT_USAGE;
    }
    let opts = knut_exec::Options {
        db: a.db,
        format,
        out_dir: a.out,
        check_only: a.check,
        explain: a.explain,
        limit: a.limit,
        timing: !a.no_timing,
        ..Default::default()
    };
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let stderr = std::io::stderr();
    let mut err = stderr.lock();
    knut_exec::run_file(&a.script, &opts, &mut out, &mut err)
}

fn index(what: IndexCmd) -> Result<(), String> {
    match what {
        IndexCmd::Changesets { db, dump } => {
            let d = knut_exec::db::Db::open(&db)?;
            let st = Instant::now();
            let bytes = knut_exec::cs::build_section(&d, dump.as_deref(), &mut |m| eprintln!("{m}"))?;
            let at = ("at".to_string(), format_time(d.latest));
            drop(d);
            use knut_format::container::{ElementKind, Temporal};
            let section = knut::container::NewSection { kind: knut_format::layout::SectionKind::Index as u32, flags: 1, bytes };
            let id = knut::container::add_element(&db, ElementKind::Changesets, Temporal::Full, vec![at], vec![section]).map_err(|e| e.to_string())?;
            eprintln!("added element {id} (changesets and users) to {} in {:.1} s", db.display(), st.elapsed().as_secs_f64());
        }
        IndexCmd::Notes { db, dump } => {
            let st = Instant::now();
            let (p, n) = knut_exec::notes::build_sidecar(&db, &dump)?;
            eprintln!("wrote {} ({n} notes) in {:.1} s", p.display(), st.elapsed().as_secs_f64());
        }
    }
    Ok(())
}

fn run(cli: Cli) -> Result<(), String> {
    let s = |e: &dyn std::fmt::Display| e.to_string();
    match cli.cmd {
        Cmd::Run(_) => unreachable!(),
        Cmd::Index { what } => index(what),
        Cmd::Web(a) => knut_web::serve(knut_web::WebOptions { db: a.db, host: a.host, port: a.port, open: !a.no_open, style: a.style, llm: a.llm, llm_model: a.llm_model, preload: a.preload }),
        Cmd::Build(a) => build(a),
        Cmd::Info { file, detailed, users } => info(&file, detailed, users),
        Cmd::Search { file, text, limit } => search(&file, &text, limit),
        Cmd::Geocode { file, address, limit } => geocode(&file, &address, limit),
        Cmd::Reverse { file, point, radius, limit } => reverse(&file, &point, radius, limit),
        Cmd::Manage { file, cmd } => manage::run(
            &file,
            cmd.map(|c| match c {
                ManageCmd::Ls { explain } => manage::Command::Ls { explain },
                ManageCmd::Add { what, time } => manage::Command::Add { what, temporal: time },
                ManageCmd::Rm { id, yes } => manage::Command::Rm { id, yes },
                ManageCmd::Compact => manage::Command::Compact,
                ManageCmd::Upgrade => manage::Command::Upgrade,
            }),
        ),
        Cmd::Query { file, q } => {
            let r = Reader::open(&file).map_err(|e| s(&e))?;
            match q {
                QueryCmd::Get { r#type, id } => print_features(&r.get(obj_type(&r#type)?, id).map_err(|e| s(&e))?, false),
                QueryCmd::At { r#type, id, time: t } => {
                    if let Some(f) = r.at(obj_type(&r#type)?, id, time(&t)?).map_err(|e| s(&e))? {
                        print_features(&[f], false);
                    }
                }
                QueryCmd::Geom { id, time: t } => match r.way_geometry_at(id, time(&t)?).map_err(|e| s(&e))? {
                    Some(g) => println!("{}", geom_json(&g)),
                    None => println!("null"),
                },
                QueryCmd::GeomChanges { id } => {
                    for (t, g) in r.way_geometry_changes(id).map_err(|e| s(&e))? {
                        println!("{}", json!({"timestamp": format_time(t), "geometry": geom_json(&g)}));
                    }
                }
                QueryCmd::Snapshot { bbox, time: t, f } => {
                    let flt = make_filter(&f)?;
                    let st = Instant::now();
                    let res = r.snapshot(&parse_bbox(&bbox)?, time(&t)?, &flt).map_err(|e| s(&e))?;
                    eprintln!("{} features in {:.1} ms", res.len(), st.elapsed().as_secs_f64() * 1e3);
                    print_features(&res, f.count);
                }
                QueryCmd::Changes { bbox, t0, t1, f } => {
                    let flt = make_filter(&f)?;
                    let st = Instant::now();
                    let res = r.changes(&parse_bbox(&bbox)?, time(&t0)?, time(&t1)?, &flt).map_err(|e| s(&e))?;
                    eprintln!("{} versions in {:.1} ms", res.len(), st.elapsed().as_secs_f64() * 1e3);
                    print_features(&res, f.count);
                }
            }
            Ok(())
        }
        Cmd::Verify { file, deep, against, exact } => verify(&file, deep, against, exact),
        Cmd::Dump { file, r#type } => dump(&file, r#type),
        Cmd::Bench { file, bbox, repeat } => bench(&file, bbox, repeat),
        Cmd::Stats { file } => stats(&file),
        Cmd::ExportParquet { input, dir, zstd_level } => parquet_export::export(&input, &dir, zstd_level),
    }
}

fn build(a: BuildArgs) -> Result<(), String> {
    if let Some(t) = a.threads {
        rayon::ThreadPoolBuilder::new().num_threads(t).build_global().map_err(|e| e.to_string())?;
    }
    let st = Instant::now();
    let mut src = PbfFile::open(&a.input).map_err(|e| e.to_string())?;
    eprintln!("input: {} ({} bytes, {} blobs)", a.input.display(), src.file_len(), src.n_blobs());
    eprintln!("blobs (count, bytes) nodes/ways/relations: {:?}", src.range_sizes());
    let mut opt = BuildOptions {
        block_size: a.block_kib * 1024,
        zstd_level: a.zstd_level,
        zstd_dict: !a.no_dict,
        keyframe_interval: a.keyframe,
        strip_users: a.strip_users || a.pseudonymize_uids.is_some(),
        pseudonymize_uids: a.pseudonymize_uids,
        min_value_count: a.min_value_count,
        sort_by_id_in_block: !a.hilbert_in_block,
        s2_sort_key: a.s2,
        tmp_dir: a.tmp_dir.clone(),
        source_name: a.input.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
        ..Default::default()
    };
    if let Some(g) = a.mem_gb {
        opt.mem_budget = (g * (1u64 << 30) as f64) as usize;
    }
    src.set_relation_batch_bytes(opt.mem_budget / 12);
    opt.seg.grow = a.seg_grow;
    opt.seg.min_size_m = a.seg_min_size;
    opt.seg.jump_m = a.seg_jump;
    opt.seg.large_threshold_e7 = (a.large_deg * 1e7) as i64;
    let log = |m: &str| eprintln!("[{:7.1}s] {m}", st.elapsed().as_secs_f64());
    let stats = knut_builder::build(&src, &a.output, &opt, &log).map_err(|e| e.to_string())?;
    eprintln!("{stats}");
    eprintln!(
        "size ratio knut/pbf: {:.3}  total {:.1}s",
        stats.output_bytes as f64 / src.file_len() as f64,
        st.elapsed().as_secs_f64()
    );
    if !a.no_index {
        use knut_format::container::{ElementKind, Temporal};
        for kind in [ElementKind::PlaceIndex, ElementKind::NameIndex, ElementKind::AddressIndex, ElementKind::TagStatistics] {
            let t = Instant::now();
            let id = knut_exec::indexes::add(&a.output, kind, Temporal::Latest, &knut_exec::indexes::Progress::default())?;
            log(&format!("added element {id}: {} ({:.1}s)", kind.info().name.to_lowercase(), t.elapsed().as_secs_f64()));
        }
    }
    Ok(())
}

fn info(file: &PathBuf, detailed: bool, users: bool) -> Result<(), String> {
    if knut::container::is_legacy(file) {
        return Err(format!("{} has format 1.0; convert it first:  knut manage {} upgrade", file.display(), file.display()));
    }
    let r = Reader::open(file).map_err(|e| e.to_string())?;
    let h = &r.header;
    manage::print_summary(file, h, r.meta_text().as_deref());
    println!();
    manage::print_table(h, false, false);
    if detailed {
        println!();
        info_detailed(&r)?;
    }
    if users {
        println!();
        for u in r.users().map_err(|e| e.to_string())? {
            println!("{}\t{}\t{}\t{}", u.uid, u.name, format_time(u.first_seen), format_time(u.last_seen));
        }
    }
    Ok(())
}

fn search(file: &PathBuf, text: &str, limit: usize) -> Result<(), String> {
    use knut_format::container::ElementKind;
    let r = Reader::open(file).map_err(|e| e.to_string())?;
    let ix = knut_exec::geocode::NameIx::new(knut_exec::indexes::element_bytes(&r, ElementKind::NameIndex)?)?;
    for h in ix.search(text, limit) {
        println!("{:<9} {:>12}  {:>10.6} {:>10.6}  {:<40} {}", h.otype.name(), h.id, h.lon, h.lat, h.name, h.class);
    }
    Ok(())
}

fn geocode(file: &PathBuf, address: &str, limit: usize) -> Result<(), String> {
    use knut_format::container::ElementKind;
    let r = Reader::open(file).map_err(|e| e.to_string())?;
    let ix = knut_exec::geocode::AddrIx::new(knut_exec::indexes::element_bytes(&r, ElementKind::AddressIndex)?)?;
    for a in ix.geocode(address, limit) {
        println!("{:>10.6} {:>10.6}  {:<9} {:>12}  {}", a.lon, a.lat, a.otype.name(), a.id, a.text());
    }
    Ok(())
}

fn reverse(file: &PathBuf, point: &str, radius: f64, limit: usize) -> Result<(), String> {
    use knut_format::container::ElementKind;
    let v: Vec<f64> = point.split(',').map(|x| x.trim().parse::<f64>()).collect::<Result<_, _>>().map_err(|_| "the point is lon,lat".to_string())?;
    let [lon, lat] = v[..] else { return Err("the point is lon,lat".into()) };
    let r = Reader::open(file).map_err(|e| e.to_string())?;
    let ix = knut_exec::geocode::AddrIx::new(knut_exec::indexes::element_bytes(&r, ElementKind::AddressIndex)?)?;
    let found = ix.reverse(lon, lat, radius, limit);
    if found.is_empty() {
        println!("no address within {radius} m");
    }
    for a in found {
        println!("{:>6.0} m  {:<9} {:>12}  {}", a.distance, a.otype.name(), a.id, a.text());
    }
    Ok(())
}

fn info_detailed(r: &Reader) -> Result<(), String> {
    let h = &r.header;
    println!("format     {}.{}.{}", h.version[0], h.version[1], h.version[2]);
    println!("features   {:#x}", h.features);
    println!("time       {} .. {} (base {})", format_time(h.tmin), format_time(h.tmax), format_time(h.time_base));
    println!(
        "bbox       {:.7},{:.7},{:.7},{:.7}",
        e7_to_deg(h.bbox.minx),
        e7_to_deg(h.bbox.miny),
        e7_to_deg(h.bbox.maxx),
        e7_to_deg(h.bbox.maxy)
    );
    println!("objects    nodes={} ways={} relations={}", h.n_objects[0], h.n_objects[1], h.n_objects[2]);
    println!("versions   nodes={} ways={} relations={}", h.n_versions[0], h.n_versions[1], h.n_versions[2]);
    println!("segments   {}", h.n_segments);
    println!("blocks     {}", h.n_blocks);
    println!("dicts      keys={} values={} roles={}", r.dicts.keys.len(), r.dicts.values.len(), r.dicts.roles.len());
    for t in 0..4 {
        let th = r.tree_header(t);
        println!("rtree[{}]   levels={} nodes={}", ["node", "way", "relation", "large"][t], th.n_levels, th.n_nodes);
    }
    let mut per = [[0u64; 3]; 6];
    for no in 0..r.n_blocks() as u32 {
        let e = r.block_entry(no).map_err(|e| e.to_string())?;
        per[e.section as usize][0] += 1;
        per[e.section as usize][1] += e.length as u64;
    }
    for (i, p) in per.iter().enumerate() {
        if p[0] > 0 {
            println!("blocks[{}] n={} bytes={}", ["node", "way", "relation", "large-node", "large-way", "large-rel"][i], p[0], p[1]);
        }
    }
    println!("sections");
    for s in &h.sections {
        let name = knut_format::layout::SectionKind::from_u32(s.kind).map_or("?".to_string(), |k| format!("{k:?}"));
        println!("  {:<12} off={:>12} len={:>12}{}", name, s.offset, s.length, if s.flags & 1 != 0 { " zstd" } else { "" });
    }
    if let Some(m) = r.meta_text() {
        println!("meta");
        for l in m.lines() {
            println!("  {l}");
        }
    }
    Ok(())
}

/// Canonical line for one version (shared by dump and verify).
fn canonical(t: ObjType, id: i64, version: u32, ts: i64, cs: u64, uid: u32, visible: bool, rest: &str) -> String {
    format!("{}{id} v{version} t{ts} c{cs} u{uid} {} {rest}", &t.name()[..1], if visible { "V" } else { "D" })
}

fn canonical_feature(f: &Feature) -> String {
    let tags: Vec<String> = f.tags.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let payload = match f.obj_type {
        ObjType::Node => match f.geometry {
            Geometry::Point(x, y) => format!("{x},{y}"),
            _ => String::new(),
        },
        ObjType::Way => f.nodes.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(","),
        ObjType::Relation => {
            f.members.iter().map(|m| format!("{}{}@{}", &m.kind.name()[..1], m.id, m.role)).collect::<Vec<_>>().join(",")
        }
    };
    let (v, c, u) = (f.meta.version, f.meta.changeset, f.meta.uid);
    canonical(f.obj_type, f.id, v, f.meta.timestamp, c, u, f.meta.visible, &format!("[{}] {payload}", tags.join("|")))
}

fn canonical_element(e: &Element) -> String {
    let m = e.meta();
    let tags = match e {
        Element::Node(n) => &n.tags,
        Element::Way(w) => &w.tags,
        Element::Relation(r) => &r.tags,
    };
    let tags: Vec<String> = tags.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let payload = match e {
        Element::Node(n) => {
            if m.visible {
                format!("{},{}", n.lon, n.lat)
            } else {
                String::new()
            }
        }
        Element::Way(w) => w.refs.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(","),
        Element::Relation(r) => {
            r.members.iter().map(|m| format!("{}{}@{}", &m.kind.name()[..1], m.id, m.role)).collect::<Vec<_>>().join(",")
        }
    };
    let (v, c, u) = (m.version, m.changeset, m.uid);
    canonical(e.obj_type(), e.id(), v, m.timestamp, c, u, m.visible, &format!("[{}] {payload}", tags.join("|")))
}

#[derive(Default)]
struct Digest {
    sum: AtomicU64,
    xor: AtomicU64,
    count: AtomicU64,
}

impl Digest {
    fn add(&self, line: &str) {
        let h = xxhash_rust::xxh3::xxh3_64(line.as_bytes());
        self.sum.fetch_add(h, Ordering::Relaxed);
        self.xor.fetch_xor(h.rotate_left(17).wrapping_mul(0x9E37_79B9_7F4A_7C15), Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    fn get(&self) -> (u64, u64, u64) {
        (self.count.load(Ordering::Relaxed), self.sum.load(Ordering::Relaxed), self.xor.load(Ordering::Relaxed))
    }
}

fn verify(file: &PathBuf, deep: bool, against: Option<PathBuf>, exact: usize) -> Result<(), String> {
    let st = Instant::now();
    let r = Reader::open_with(file, OpenOptions { verify_checksums: true, cache_blocks: 4096 }).map_err(|e| e.to_string())?;
    let rep = r.verify(deep).map_err(|e| e.to_string())?;
    println!(
        "checksums ok: {} sections{}",
        rep.sections,
        if deep { format!(", {} blocks, {} segments, {} records decoded", rep.blocks, rep.segments, rep.version_records) } else { String::new() }
    );
    let Some(pbf) = against else { return Ok(()) };
    let knut = Digest::default();
    r.for_each_version(&|f| knut.add(&canonical_feature(f))).map_err(|e| e.to_string())?;
    let src = PbfFile::open(&pbf).map_err(|e| e.to_string())?;
    let orig = Digest::default();
    // Exact samples: objects whose id hash selects them, up to `exact` per type.
    let mut samples: Vec<Vec<Element>> = Vec::new();
    let mut per_type = [0usize; 3];
    let mut take = |b: Vec<Element>| -> knut_builder::Result<()> {
        use rayon::prelude::*;
        b.par_iter().for_each(|e| orig.add(&canonical_element(e)));
        if exact > 0 {
            for e in b {
                let t = e.obj_type() as usize;
                let selected = xxhash_rust::xxh3::xxh3_64(&e.id().to_le_bytes()).is_multiple_of(97);
                if !selected {
                    continue;
                }
                match samples.last_mut() {
                    Some(g) if g[0].obj_type() == e.obj_type() && g[0].id() == e.id() => g.push(e),
                    _ if per_type[t] < exact => {
                        per_type[t] += 1;
                        samples.push(vec![e]);
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    };
    src.nodes(&mut take).map_err(|e| e.to_string())?;
    src.ways(&mut take).map_err(|e| e.to_string())?;
    src.relations(&mut take).map_err(|e| e.to_string())?;
    let (a, b) = (knut.get(), orig.get());
    println!("knut: {} versions, digest {:016x}/{:016x}", a.0, a.1, a.2);
    println!("pbf : {} versions, digest {:016x}/{:016x}", b.0, b.1, b.2);
    let mut ok = a == b;
    if exact > 0 {
        let mut bad = 0;
        for g in &samples {
            let t = g[0].obj_type();
            let got = r.get(t, g[0].id()).map_err(|e| e.to_string())?;
            let want: Vec<String> = g.iter().map(|e| canonical_element(e)).collect();
            let have: Vec<String> = got.iter().map(|f| canonical_feature(f)).collect();
            if want != have {
                bad += 1;
                if bad <= 3 {
                    eprintln!("mismatch {} {}:\n  pbf : {:?}\n  knut: {:?}", t.name(), g[0].id(), want, have);
                }
            }
        }
        println!("exact get() comparison: {} objects, {} mismatches", samples.len(), bad);
        ok &= bad == 0;
    }
    println!("{} ({:.1}s)", if ok { "ROUND-TRIP OK" } else { "ROUND-TRIP MISMATCH" }, st.elapsed().as_secs_f64());
    if ok { Ok(()) } else { Err("verification failed".into()) }
}

fn dump(file: &PathBuf, typ: Option<String>) -> Result<(), String> {
    let r = Reader::open_with(file, OpenOptions { verify_checksums: true, cache_blocks: 4096 }).map_err(|e| e.to_string())?;
    let types: Vec<ObjType> = match typ {
        Some(t) => vec![obj_type(&t)?],
        None => ObjType::ALL.to_vec(),
    };
    let out = std::io::stdout();
    let mut w = BufWriter::new(out.lock());
    for t in types {
        for id in r.all_ids(t).map_err(|e| e.to_string())? {
            for f in r.get(t, id).map_err(|e| e.to_string())? {
                let _ = writeln!(w, "{}", canonical_feature(&f));
            }
        }
    }
    Ok(())
}

fn bench(file: &PathBuf, bbox: Option<String>, repeat: usize) -> Result<(), String> {
    let e = |x: knut::Error| x.to_string();
    let t_open = Instant::now();
    let r = Reader::open(file).map_err(e)?;
    let open_ms = t_open.elapsed().as_secs_f64() * 1e3;
    let h = r.header.clone();
    let bb = match bbox {
        Some(b) => parse_bbox(&b)?,
        None => {
            let (cx, cy) = h.bbox.center();
            BBox::new(cx - 250_000, cy - 250_000, cx + 250_000, cy + 250_000)
        }
    };
    let t_mid = h.tmax - 365 * 86400;
    let ids = |t: ObjType| -> Result<Vec<i64>, String> {
        let all = r.all_ids(t).map_err(e)?;
        let step = (all.len() / 200).max(1);
        Ok(all.into_iter().step_by(step).take(200).collect())
    };
    let node_ids = ids(ObjType::Node)?;
    let way_ids = ids(ObjType::Way)?;
    println!("open: {open_ms:.2} ms");
    let run = |name: &str, f: &mut dyn FnMut() -> Result<usize, String>| -> Result<(), String> {
        let mut times = Vec::new();
        let mut n = 0;
        for _ in 0..repeat.max(1) {
            let st = Instant::now();
            n = f()?;
            times.push(st.elapsed().as_secs_f64() * 1e3);
        }
        let cold = times[0];
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!("{name:<34} results={n:>9}  first={cold:>9.2} ms  median={:>9.2} ms", times[times.len() / 2]);
        Ok(())
    };
    run("get(node) x200", &mut || {
        let mut n = 0;
        for &id in &node_ids {
            n += r.get(ObjType::Node, id).map_err(e)?.len();
        }
        Ok(n)
    })?;
    run("get(way) x200", &mut || {
        let mut n = 0;
        for &id in &way_ids {
            n += r.get(ObjType::Way, id).map_err(e)?.len();
        }
        Ok(n)
    })?;
    run("way_geometry_at x200", &mut || {
        let mut n = 0;
        for &id in &way_ids {
            n += r.way_geometry_at(id, h.tmax).map_err(e)?.map_or(0, |g| g.len());
        }
        Ok(n)
    })?;
    run("snapshot(bbox, now)", &mut || Ok(r.snapshot(&bb, h.tmax, &Filter::default()).map_err(e)?.len()))?;
    run("snapshot(bbox, now-1y)", &mut || Ok(r.snapshot(&bb, t_mid, &Filter::default()).map_err(e)?.len()))?;
    run("snapshot(bbox, now, highway)", &mut || {
        Ok(r.snapshot(&bb, h.tmax, &Filter::default().tag("highway", None)).map_err(e)?.len())
    })?;
    run("snapshot(bbox, now, building=yes)", &mut || {
        Ok(r.snapshot(&bb, h.tmax, &Filter::default().tag("building", Some("yes"))).map_err(e)?.len())
    })?;
    run("changes(bbox, last year)", &mut || Ok(r.changes(&bb, t_mid, h.tmax, &Filter::default()).map_err(e)?.len()))?;
    run("changes(world, last year, amenity)", &mut || {
        Ok(r.changes(&BBox::WORLD, t_mid, h.tmax, &Filter::default().tag("amenity", None)).map_err(e)?.len())
    })?;
    run("snapshot(world, now, amenity)", &mut || {
        Ok(r.snapshot(&BBox::WORLD, h.tmax, &Filter::default().tag("amenity", None)).map_err(e)?.len())
    })?;
    Ok(())
}

fn stats(file: &PathBuf) -> Result<(), String> {
    use std::collections::BTreeMap;
    let r = Reader::open(file).map_err(|e| e.to_string())?;
    let mut acc: BTreeMap<(u8, usize, &'static str), (u64, u64)> = BTreeMap::new();
    let mut frames: BTreeMap<u8, (u64, u64, u64, u64)> = BTreeMap::new();
    for no in 0..r.n_blocks() as u32 {
        let (h, dir, data, geom) = r.raw_frames(no).map_err(|e| e.to_string())?;
        let t = h.obj_type as u8;
        let fe = frames.entry(t).or_default();
        fe.0 += dir.len() as u64;
        fe.1 += h.dir_comp as u64;
        fe.2 += (data.len() + geom.len()) as u64;
        fe.3 += (h.data_comp + h.geom_comp) as u64;
        let mut cols = knut_format::block::data_column_ranges(h.obj_type, &data, &geom).map_err(|e| e.to_string())?;
        let mut p = 0;
        let mut dcols = Vec::new();
        for name in ["D:id", "D:flags", "D:nv", "D:ns", "D:tmin", "D:tmax", "D:minx", "D:miny", "D:w", "D:h"] {
            let st = p;
            knut_format::codec::get_u64_column(&dir, &mut p).map_err(|e| e.to_string())?;
            dcols.push((name, st..p));
        }
        for (i, (name, rg)) in dcols.into_iter().enumerate() {
            let raw = &dir[rg];
            let c = zstd::bulk::compress(raw, 9).map_err(|e| e.to_string())?.len();
            let e = acc.entry((t, 100 + i, name)).or_default();
            e.0 += raw.len() as u64;
            e.1 += c as u64;
        }
        cols.truncate(cols.len());
        let all: Vec<u8> = [data.as_slice(), geom.as_slice()].concat();
        for (i, (name, rg)) in cols.into_iter().enumerate() {
            let raw = &all[rg];
            let c = zstd::bulk::compress(raw, 9).map_err(|e| e.to_string())?.len();
            let e = acc.entry((t, i, name)).or_default();
            e.0 += raw.len() as u64;
            e.1 += c as u64;
        }
    }
    for (t, (dr, dc, xr, xc)) in &frames {
        println!("{}: dir raw={dr} comp={dc} | data raw={xr} comp={xc}", ObjType::from_u8(*t).unwrap().name());
    }
    println!("{:<10} {:<12} {:>12} {:>12}", "type", "column", "raw", "zstd(col)");
    for ((t, _, name), (raw, c)) in acc {
        println!("{:<10} {:<12} {:>12} {:>12}", ObjType::from_u8(t).unwrap().name(), name, raw, c);
    }
    Ok(())
}
