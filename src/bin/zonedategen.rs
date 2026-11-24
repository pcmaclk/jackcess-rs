//! Oracle-facing generator for the zoned date/time parity harness
//! (`tools/verify-zonedates.ps1`).
//!
//! Every command mirrors a command of `java-src/ZoneDateGen.java`, and the two
//! print the exact same lines from the same `zone`/`mode` arguments.  The text
//! format is line oriented, one record per value, so the comparison is an exact
//! line-by-line diff.
//!
//! Commands (the Java oracle takes the same arguments):
//!
//! * `conv <zone> <DATE|LOCAL_DATE_TIME>`: for the shared instant matrix
//!   (epoch milliseconds, including pre-1901, the 1900 `TimeZone` history
//!   boundary, DST gaps/overlaps, year 1, year 9999 and negative values) prints,
//!   for the given database zone and date/time type: the `TimeZone` and
//!   `ZoneId` offsets, the stored `SHORT_DATE_TIME` bits for a
//!   `java.util.Date` instant and for the `LocalDateTime` of that instant, and
//!   the values read back from those bits in both representations (`DateExt`
//!   millis + `LocalDateTime`, including the nanosecond text).
//! * `convlocal <zone> <DATE|LOCAL_DATE_TIME>`: the same conversions for a
//!   matrix of *local* wall clock times (the `LocalDateTime` write path), which
//!   is where `LocalDateTime.atZone` has to resolve a DST gap or overlap.
//! * `zoneall <zones-file>`: the raw offset (`TimeZone.getRawOffset`) and the
//!   `TimeZone`/`ZoneId` offsets of every listed zone at a coarse instant grid,
//!   one line per zone (this is the broad check of the zone model).
//! * `zonelist`: the zone ids this implementation knows, one per line, sorted.
//! * `tzdb`: the IANA database version this implementation uses.
//! * `write <out> <FORMAT> <MODE> <zone> [now-ldt]`: creates the fixture
//!   database (date columns, an indexed date column, a date default and, for
//!   V2010/V2016, a calculated date column, plus date valued table and column
//!   properties), writes the date rows through both value representations,
//!   prints the values read back and the catalog timestamp, so the Java and
//!   Rust files can be compared byte for byte.
//! * `dump <file> <MODE> <zone> <label>`: the fixture dump (catalog dates,
//!   rows, date valued properties) used for the cross-implementation reads.
//! * `lookup <out> <FORMAT> <MODE> <zone> [now-ldt]`: creates an indexed date
//!   column, inserts rows around a DST transition and searches the index with
//!   `LocalDateTime`, `java.util.Date` and read-back values, printing the row
//!   ids found.
//!
//! `now-ldt` is `year,month,day,hour,minute,second,nano` with a leading `-1`
//! option meaning "use the current clock" (the Java oracle prints the catalog
//! timestamp it created, and the harness hands it to this generator).

use std::path::Path;

use jackcess::column::{self, ColumnSpec};
use jackcess::cursor::{Cursor, IndexCursor, IndexRange};
use jackcess::database::{Database, DateTimeType};
use jackcess::datatype::DataType;
use jackcess::datenums::{self, LocalDateTime, TimeZone};
use jackcess::error::{Error, Result};
use jackcess::format::FileFormat;
use jackcess::tablecreator::{IndexSpec, TableSpec};
use jackcess::value::Value;

/// 2020-03-15T10:30:45Z, the instant the other harnesses use.
const DATE_BASE_MS: i64 = 1_584_246_645_000;

/// The instant matrix shared with `ZoneDateGen.INSTANTS`.
const INSTANTS: [i64; 26] = [
    0,
    1,
    -1,
    -86_400_000,
    DATE_BASE_MS,
    DATE_BASE_MS + 1,
    // DST spring forward / fall back, America/New_York (2020) and
    // Europe/London (2020), just before and just after each transition
    1_583_647_199_000,
    1_583_647_200_000,
    1_604_210_399_000,
    1_604_210_400_000,
    1_604_213_400_000,
    1_585_432_799_000,
    1_585_432_800_000,
    1_603_663_199_000,
    1_603_663_200_000,
    // the java.util.TimeZone history cut-off (1900-01-01T00:00:00Z)
    -2_208_988_800_001,
    -2_208_988_800_000,
    -2_208_988_799_999,
    // historical LMT (Asia/Shanghai before 1901, America/New_York before 1883)
    -2_500_000_000_000,
    -2_220_000_000_000,
    // 1582-10-15T00:00:00Z, 1899-12-30T00:00:00Z (the OLE epoch)
    -12_219_292_800_000,
    -2_209_161_600_000,
    // year 1 and year 9999
    -62_135_596_800_000,
    253_402_300_799_000,
    253_402_300_799_999,
    -2_208_988_800_000 - 1,
];

/// The local wall clock matrix shared with `ZoneDateGen.LOCAL_TIMES`: values
/// around the DST transitions of `America/New_York` and `Europe/London`, the
/// LMT era, the OLE epoch, year 1, year 9999 and a sub-millisecond time.
fn local_times() -> Vec<LocalDateTime> {
    vec![
        LocalDateTime::new(2020, 3, 15, 12, 30, 45, 0),
        LocalDateTime::new(2020, 3, 15, 12, 30, 45, 123_456_789),
        // America/New_York 2020-03-08T02:00 -> 03:00 (gap) and
        // 2020-11-01T02:00 -> 01:00 (overlap)
        LocalDateTime::new(2020, 3, 8, 1, 59, 59, 0),
        LocalDateTime::new(2020, 3, 8, 2, 0, 0, 0),
        LocalDateTime::new(2020, 3, 8, 2, 30, 0, 0),
        LocalDateTime::new(2020, 3, 8, 3, 0, 0, 0),
        LocalDateTime::new(2020, 11, 1, 1, 0, 0, 0),
        LocalDateTime::new(2020, 11, 1, 1, 30, 0, 0),
        LocalDateTime::new(2020, 11, 1, 2, 0, 0, 0),
        // Europe/London 2020-03-29T01:00 -> 02:00 and 2020-10-25T02:00 -> 01:00
        LocalDateTime::new(2020, 3, 29, 1, 30, 0, 0),
        LocalDateTime::new(2020, 3, 29, 0, 59, 59, 0),
        LocalDateTime::new(2020, 3, 29, 2, 0, 0, 0),
        LocalDateTime::new(2020, 10, 25, 1, 30, 0, 0),
        LocalDateTime::new(2020, 10, 25, 0, 59, 59, 0),
        // the TimeZone history cut-off and the LMT era
        LocalDateTime::new(1900, 1, 1, 0, 0, 0, 0),
        LocalDateTime::new(1899, 12, 31, 23, 59, 59, 0),
        LocalDateTime::new(1899, 6, 1, 0, 0, 0, 0),
        LocalDateTime::new(1899, 12, 30, 0, 0, 0, 0),
        // 1582-10-15, year 1 and year 9999
        LocalDateTime::new(1582, 10, 15, 0, 0, 0, 0),
        LocalDateTime::new(1, 1, 1, 0, 0, 0, 0),
        LocalDateTime::new(9999, 12, 31, 23, 59, 59, 999_000_000),
        // an 1899 value with a sub-millisecond part
        LocalDateTime::new(1899, 12, 31, 23, 59, 59, 123_456_789),
    ]
}

fn usage() -> ! {
    eprintln!(
        "usage: zonedategen conv|convlocal <zone> <DATE|LOCAL_DATE_TIME>\n\
         \x20      zonedategen zoneall <zones-file> | zonelist | tzdb\n\
         \x20      zonedategen write <out> <FORMAT> <MODE> <zone> [now-ldt]\n\
         \x20      zonedategen dump <file> <MODE> <zone> <label>\n\
         \x20      zonedategen lookup <out> <FORMAT> <MODE> <zone> [now-ldt]"
    );
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage();
    }
    let result = match args[1].as_str() {
        "conv" => {
            if args.len() < 4 {
                usage();
            }
            conv(&args[2], DateTimeType::from_name(&args[3]))
        }
        "convlocal" => {
            if args.len() < 4 {
                usage();
            }
            conv_local(&args[2], DateTimeType::from_name(&args[3]))
        }
        "zoneall" => {
            if args.len() < 3 {
                usage();
            }
            zone_all(Path::new(&args[2]))
        }
        "zonelist" => {
            let mut ids: Vec<&str> = chrono_tz::TZ_VARIANTS.iter().map(|tz| tz.name()).collect();
            ids.sort_unstable();
            ids.dedup();
            for id in ids {
                println!("{id}");
            }
            Ok(())
        }
        "tzdb" => {
            println!("tzdb={}", chrono_tz::IANA_TZDB_VERSION);
            Ok(())
        }
        "write" => {
            if args.len() < 6 {
                usage();
            }
            write(
                Path::new(&args[2]),
                FileFormat::from_name(&args[3]).unwrap_or(FileFormat::V2003),
                DateTimeType::from_name(&args[4]),
                &args[5],
                now_ldt(&args[6..]),
            )
        }
        "dump" => {
            if args.len() < 6 {
                usage();
            }
            dump(
                Path::new(&args[2]),
                DateTimeType::from_name(&args[3]),
                &args[4],
                &args[5],
            )
        }
        "lookup" => {
            if args.len() < 6 {
                usage();
            }
            lookup(
                Path::new(&args[2]),
                FileFormat::from_name(&args[3]).unwrap_or(FileFormat::V2003),
                DateTimeType::from_name(&args[4]),
                &args[5],
                now_ldt(&args[6..]),
            )
        }
        other => {
            eprintln!("unknown command {other}");
            usage();
        }
    };
    if let Err(error) = result {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

/// Parses `year,month,day,hour,minute,second,nano` (the `--now-ldt` option the
/// harness hands over from the Java file).
fn now_ldt(args: &[String]) -> Option<LocalDateTime> {
    let text = args.iter().find_map(|arg| arg.strip_prefix("--now-ldt="))?;
    let parts: Vec<i64> = text
        .split(',')
        .filter_map(|p| p.trim().parse().ok())
        .collect();
    if parts.len() != 7 {
        return None;
    }
    Some(LocalDateTime::new(
        parts[0] as i32,
        parts[1] as u32,
        parts[2] as u32,
        parts[3] as u32,
        parts[4] as u32,
        parts[5] as u32,
        parts[6] as u32,
    ))
}

fn zone_of(zone: &str) -> Result<TimeZone> {
    TimeZone::from_zone_id(zone)
        .ok_or_else(|| Error::IllegalArgument(format!("Unknown time-zone ID: '{}'", zone)))
}

/// The table definition page of a loaded (or loadable) table
/// (`DatabaseImpl.findTable`).
fn table_page(db: &mut Database, name: &str) -> Result<i32> {
    match db.find_table(name)? {
        Some(table) => Ok(table.table_def_page_number),
        None => Err(Error::Jackcess(format!("table {name} does not exist"))),
    }
}

fn ldt_text(ldt: &LocalDateTime) -> String {
    column::java_local_date_time_text(ldt)
}

/// The `year,month,day,hour,minute,second,nano` form the generator accepts for
/// `--now-ldt` (the Java oracle prints the same line).
fn ldt_args(ldt: LocalDateTime) -> String {
    format!(
        "{},{},{},{},{},{},{}",
        ldt.year, ldt.month, ldt.day, ldt.hour, ldt.minute, ldt.second, ldt.nano
    )
}

fn bits_hex(bits: u64) -> String {
    format!("{bits:016x}")
}

// ---------------------------------------------------------------------------
// conv / convlocal: the conversion matrix
// ---------------------------------------------------------------------------

fn conv(zone: &str, date_time_type: DateTimeType) -> Result<()> {
    let tz = zone_of(zone)?;
    println!(
        "zone={} mode={} raw={} zoneid={}",
        zone,
        date_time_type.name(),
        tz.get_raw_offset(),
        tz.zone_id()
    );
    for ms in INSTANTS {
        let ldt = tz.local_date_time_from_millis(ms);
        let w_date = column::date_value_bits_with(&Value::BigInt(ms), &tz, date_time_type)?;
        let w_ldt = column::date_value_bits_with(&Value::DateTime(ldt), &tz, date_time_type)?;
        let r_date = column::date_ext_millis(f64::from_bits(w_date), &tz);
        let r_date_ldt = column::ldt_from_local_date_double(f64::from_bits(w_date));
        let r_ldt = column::date_ext_millis(f64::from_bits(w_ldt), &tz);
        let r_ldt_ldt = column::ldt_from_local_date_double(f64::from_bits(w_ldt));
        println!(
            "i={ms} tzoff={} zoneoff={} ldt={} wdate={} wldt={} \
             rdate={} rdateldt={} rldt={} rldtldt={}",
            tz.get_offset(ms),
            tz.zone_offset(ms),
            ldt_text(&ldt),
            bits_hex(w_date),
            bits_hex(w_ldt),
            r_date,
            ldt_text(&r_date_ldt),
            r_ldt,
            ldt_text(&r_ldt_ldt),
        );
    }
    Ok(())
}

fn conv_local(zone: &str, date_time_type: DateTimeType) -> Result<()> {
    let tz = zone_of(zone)?;
    println!(
        "zone={} mode={} raw={} zoneid={}",
        zone,
        date_time_type.name(),
        tz.get_raw_offset(),
        tz.zone_id()
    );
    for ldt in local_times() {
        // `LocalDateTime.atZone(zoneId).toInstant().toEpochMilli()`
        let at_zone = tz.local_to_epoch_millis(ldt);
        let w = column::date_value_bits_with(&Value::DateTime(ldt), &tz, date_time_type)?;
        let r_date = column::date_ext_millis(f64::from_bits(w), &tz);
        let r_ldt = column::ldt_from_local_date_double(f64::from_bits(w));
        println!(
            "lt={} atzone={} w={} rdate={} rdateldt={}",
            ldt_text(&ldt),
            at_zone,
            bits_hex(w),
            r_date,
            ldt_text(&r_ldt),
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// zoneall: the broad zone model check
// ---------------------------------------------------------------------------

/// The instant grid `zoneall` samples (the same values as the Java oracle).
const ZONEALL_INSTANTS: [i64; 10] = [
    -2_208_988_800_001,
    -2_208_988_800_000,
    -2_500_000_000_000,
    -12_219_292_800_000,
    -62_135_596_800_000,
    0,
    DATE_BASE_MS,
    1_595_980_800_000,
    253_402_300_799_000,
    253_402_300_799_999,
];

/// Reads the zone-id list, accepting the encodings PowerShell's redirection may
/// produce: UTF-8 (with or without BOM) and UTF-16 with a BOM.
fn read_zone_list(path: &Path) -> Result<String> {
    let raw = std::fs::read(path)?;
    if raw.len() >= 2 && raw[0] == 0xFF && raw[1] == 0xFE {
        let units: Vec<u16> = raw[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        return String::from_utf16(&units)
            .map_err(|_| Error::Jackcess("invalid UTF-16 zone list".to_string()));
    }
    if raw.len() >= 2 && raw[0] == 0xFE && raw[1] == 0xFF {
        let units: Vec<u16> = raw[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
            .collect();
        return String::from_utf16(&units)
            .map_err(|_| Error::Jackcess("invalid UTF-16 zone list".to_string()));
    }
    let raw = if raw.starts_with(&[0xEF, 0xBB, 0xBF]) {
        &raw[3..]
    } else {
        &raw[..]
    };
    String::from_utf8(raw.to_vec())
        .map_err(|_| Error::Jackcess("invalid UTF-8 zone list".to_string()))
}

fn zone_all(zones_file: &Path) -> Result<()> {
    let text = read_zone_list(zones_file)?;
    for line in text.lines() {
        let id = line.trim();
        if id.is_empty() || id.starts_with('#') {
            continue;
        }
        let Some(tz) = TimeZone::from_zone_id(id) else {
            println!("zone={id} unavailable");
            continue;
        };
        let mut parts = Vec::with_capacity(ZONEALL_INSTANTS.len() * 2);
        for ms in ZONEALL_INSTANTS {
            parts.push(format!("{}:{}", tz.get_offset(ms), tz.zone_offset(ms)));
        }
        println!(
            "zone={} raw={} offs={}",
            id,
            tz.get_raw_offset(),
            parts.join(",")
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// write: the byte-identical fixture
// ---------------------------------------------------------------------------

/// The date rows of the fixture, as (D1, D2, D3, label) values.  A
/// [`Value::DateTime`] is a `java.util.Date`-style wall clock value and a
/// [`Value::BigInt`] is a `java.util.Date` instant, exactly the two Java value
/// kinds the oracle writes.
fn fixture_rows(tz: &TimeZone) -> Vec<Vec<Value>> {
    let ldt = |y, mo, d, h, mi, s, n| Value::DateTime(LocalDateTime::new(y, mo, d, h, mi, s, n));
    let instant = |ms: i64| Value::BigInt(ms);
    vec![
        vec![
            Value::Null,
            ldt(2020, 3, 15, 12, 30, 45, 0),
            instant(DATE_BASE_MS),
            ldt(1999, 12, 31, 23, 59, 59, 123_000_000),
            Value::Text("clean".to_string()),
        ],
        vec![
            Value::Null,
            // a local time in the New York spring-forward gap / London gap
            ldt(2020, 3, 8, 2, 30, 0, 0),
            instant(0),
            // a local time repeated by the New York fall-back transition
            ldt(2020, 11, 1, 1, 30, 0, 0),
            Value::Text("dst".to_string()),
        ],
        vec![
            Value::Null,
            // the LMT era of Asia/Shanghai / pre-1900 America/New_York
            ldt(1899, 6, 1, 0, 0, 0, 0),
            instant(-2_208_988_800_000),
            ldt(1582, 10, 15, 0, 0, 0, 0),
            Value::Text("historic".to_string()),
        ],
        vec![
            Value::Null,
            ldt(9999, 12, 31, 23, 59, 59, 999_000_000),
            instant(-1),
            ldt(1900, 1, 1, 0, 0, 0, 0),
            Value::Text("edges".to_string()),
        ],
        vec![
            Value::Null,
            ldt(2020, 3, 15, 12, 30, 45, 123_456_789),
            instant(1_604_210_400_000),
            Value::Null,
            Value::Text("subms".to_string()),
        ],
        vec![
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Text("nulls".to_string()),
        ],
    ]
    .into_iter()
    .inspect(|_row| {
        // the zone is only consulted by the conversions, so the row builder does
        // not need it; it is kept in the signature so both sides build the same
        // rows from the same zone aware code
        let _ = tz;
    })
    .collect()
}

fn write(
    path: &Path,
    format: FileFormat,
    date_time_type: DateTimeType,
    zone: &str,
    now: Option<LocalDateTime>,
) -> Result<()> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let mut db = Database::create(format, path)?;
    db.set_zone_id(zone)?;
    db.set_date_time_type(date_time_type);
    if let Some(now) = now {
        db.now_ldt_queue.push(now);
    }
    let tz = db.tz;

    let mut columns = vec![
        ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
        ColumnSpec::new("D1", DataType::ShortDateTime),
        ColumnSpec::new("D2", DataType::ShortDateTime),
        ColumnSpec::new("D3", DataType::ShortDateTime).put_property(
            jackcess::property::DEFAULT_VALUE_PROP,
            Value::Text("#2020-03-15 12:30:45#".to_string()),
        )?,
        ColumnSpec::new("Label", DataType::Text).with_length(20),
    ];
    if matches!(format, FileFormat::V2010 | FileFormat::V2016) {
        columns.push(ColumnSpec::new("CD", DataType::ShortDateTime).set_calculated_info("[D1]"));
    }
    let spec = TableSpec::new("DT", columns)
        .with_index(IndexSpec::new("D1Index", vec![("D1".to_string(), true)]));
    db.create_table(&spec)?;

    // date valued properties: a wall clock time in a DST gap, a
    // java.util.Date instant and a historical time, both table and column level
    {
        let mut maps = db.get_table_properties_by_name("DT")?;
        maps.get_default().put_value(
            "ZonedLdt",
            Value::DateTime(LocalDateTime::new(2020, 3, 8, 2, 30, 0, 0)),
        )?;
        maps.get_default().put_value(
            "ZonedInstant",
            Value::DateTime(tz.local_date_time_from_millis(DATE_BASE_MS)),
        )?;
        maps.get_default().put_value(
            "Historic",
            Value::DateTime(LocalDateTime::new(1899, 6, 1, 0, 0, 0, 0)),
        )?;
        maps.get_mut("D1").put_value(
            "ColZonedInstant",
            Value::DateTime(tz.local_date_time_from_millis(DATE_BASE_MS + 1)),
        )?;
        maps.save(&mut db)?;
    }

    let rows = fixture_rows(&tz);
    for row in &rows {
        db.add_rows("DT", std::slice::from_ref(row))?;
    }
    // read the rows back and write the values again: in DATE mode they are
    // DateExt values which keep their stored bits
    let page = table_page(&mut db, "DT")?;
    let read_back = db.read_table_rows(page)?;
    let mut reread = Vec::new();
    for row in read_back.into_iter().flatten() {
        if row.len() >= 5 {
            reread.push(vec![
                Value::Null,
                row[1].clone(),
                row[2].clone(),
                Value::Null,
                Value::Text("reread".to_string()),
            ]);
        }
    }
    for row in &reread {
        db.add_rows("DT", std::slice::from_ref(row))?;
    }

    // index lookups with zoned keys, in the fixture's own mode
    let probes = lookup_probes(&tz);
    println!(
        "write table=DT type={} zone={} raw={} rows={} cols={}",
        date_time_type.name(),
        zone,
        tz.get_raw_offset(),
        rows.len() + reread.len(),
        db.tables.get(&page).map(|t| t.columns.len()).unwrap_or(0)
    );
    print_rows(&mut db, page, &tz)?;
    let table_name = "DT";
    for (label, value) in &probes {
        println!(
            "find={},{}",
            label,
            join_ids(&lookup_ids(&mut db, table_name, value)?)
        );
    }
    // the date valued properties are checked by `dump`, which always reads the
    // stored blob (Java's creating instance still holds the objects it put)
    let table = db
        .tables
        .get(&page)
        .ok_or_else(|| Error::Jackcess("table DT is not loaded".into()))?;
    println!(
        "nowldt={}",
        table
            .get_created_date()
            .map(|d| ldt_text(&d))
            .unwrap_or_else(|| "-".to_string())
    );
    println!(
        "nowargs={}",
        table
            .get_created_date()
            .map(ldt_args)
            .unwrap_or_else(|| "-".to_string())
    );
    db.flush()
}

// ---------------------------------------------------------------------------
// dump: the cross-implementation read
// ---------------------------------------------------------------------------

fn dump(path: &Path, date_time_type: DateTimeType, zone: &str, label: &str) -> Result<()> {
    let mut db = Database::open_path(path)?;
    // the zone of the file on disk is the one Java wrote it with; the fixture
    // is created and read with the same zone
    let tz = zone_of(zone)?;
    db.set_time_zone(tz);
    db.set_date_time_type(date_time_type);
    println!(
        "dump label={} type={} zone={} raw={}",
        label,
        date_time_type.name(),
        zone,
        tz.get_raw_offset()
    );
    for name in ["DT", "DTL"] {
        let page = match table_page(&mut db, name) {
            Ok(page) => page,
            Err(_) => continue,
        };
        let (table_name, created, updated) = {
            let table = db
                .tables
                .get(&page)
                .ok_or_else(|| Error::Jackcess(format!("table {name} is not loaded")))?;
            (
                table.name.clone(),
                table.get_created_date(),
                table.get_updated_date(),
            )
        };
        println!(
            "table={} created={} updated={}",
            escape(&table_name),
            created
                .map(|d| ldt_text(&d))
                .unwrap_or_else(|| "-".to_string()),
            updated
                .map(|d| ldt_text(&d))
                .unwrap_or_else(|| "-".to_string())
        );
        print_rows(&mut db, page, &tz)?;
        if name == "DTL" {
            let probes = lookup_probes(&tz);
            for (label, value) in &probes {
                println!(
                    "find={},{}",
                    label,
                    join_ids(&lookup_ids(&mut db, "DTL", value)?)
                );
            }
        }
        print_table_properties(&mut db, name, &tz)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// lookup: the indexed date lookup fixture
// ---------------------------------------------------------------------------

fn lookup_probes(tz: &TimeZone) -> Vec<(String, Value)> {
    let _ = tz;
    vec![
        (
            "subms".to_string(),
            Value::DateTime(LocalDateTime::new(2020, 3, 15, 12, 30, 45, 123_456_789)),
        ),
        (
            "whole123".to_string(),
            Value::DateTime(LocalDateTime::new(2020, 3, 15, 12, 30, 45, 123_000_000)),
        ),
        (
            "gap".to_string(),
            Value::DateTime(LocalDateTime::new(2020, 3, 8, 2, 30, 0, 0)),
        ),
        (
            "overlap".to_string(),
            Value::DateTime(LocalDateTime::new(2020, 11, 1, 1, 30, 0, 0)),
        ),
        ("instant".to_string(), Value::BigInt(DATE_BASE_MS)),
        ("instant1".to_string(), Value::BigInt(DATE_BASE_MS + 1)),
        (
            "historic".to_string(),
            Value::DateTime(LocalDateTime::new(1899, 6, 1, 0, 0, 0, 0)),
        ),
        (
            "year9999".to_string(),
            Value::DateTime(LocalDateTime::new(9999, 12, 31, 23, 59, 59, 999_000_000)),
        ),
    ]
}

fn lookup(
    path: &Path,
    format: FileFormat,
    date_time_type: DateTimeType,
    zone: &str,
    now: Option<LocalDateTime>,
) -> Result<()> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let mut db = Database::create(format, path)?;
    db.set_zone_id(zone)?;
    db.set_date_time_type(date_time_type);
    if let Some(now) = now {
        db.now_ldt_queue.push(now);
    }
    let tz = db.tz;
    let spec = TableSpec::new(
        "DTL",
        vec![
            ColumnSpec::new("ID", DataType::Long),
            ColumnSpec::new("D1", DataType::ShortDateTime),
        ],
    )
    .with_index(IndexSpec::new("D1Index", vec![("D1".to_string(), true)]));
    let page = db.create_table(&spec)?;

    let rows = vec![
        vec![
            Value::Long(1),
            Value::DateTime(LocalDateTime::new(2020, 3, 15, 12, 30, 45, 123_456_789)),
        ],
        vec![
            Value::Long(2),
            Value::DateTime(LocalDateTime::new(2020, 3, 15, 12, 30, 45, 123_000_000)),
        ],
        vec![
            Value::Long(3),
            Value::DateTime(LocalDateTime::new(2020, 3, 15, 12, 30, 45, 124_000_000)),
        ],
        // a local time inside the New York/London gap and one in the overlap
        vec![
            Value::Long(4),
            Value::DateTime(LocalDateTime::new(2020, 3, 8, 2, 30, 0, 0)),
        ],
        vec![
            Value::Long(5),
            Value::DateTime(LocalDateTime::new(2020, 11, 1, 1, 30, 0, 0)),
        ],
        // a java.util.Date instant and a historical LMT time
        vec![Value::Long(6), Value::BigInt(DATE_BASE_MS)],
        vec![
            Value::Long(7),
            Value::DateTime(LocalDateTime::new(1899, 6, 1, 0, 0, 0, 0)),
        ],
        vec![
            Value::Long(8),
            Value::DateTime(LocalDateTime::new(9999, 12, 31, 23, 59, 59, 999_000_000)),
        ],
        vec![Value::Long(9), Value::Null],
    ];
    for row in &rows {
        db.add_rows("DTL", std::slice::from_ref(row))?;
    }
    // re-write the values read back from the table (DateExt bits in DATE mode)
    let reread = {
        let read = db.read_table_rows(page)?;
        let mut values = Vec::new();
        for row in read.into_iter().flatten() {
            let id = row.first().and_then(|v| v.as_i64()).unwrap_or(-1);
            if (1..=8).contains(&id) {
                values.push(vec![
                    Value::Long(id as i32 + 100),
                    row.get(1).cloned().unwrap_or(Value::Null),
                ]);
            }
        }
        values
    };
    for row in &reread {
        db.add_rows("DTL", std::slice::from_ref(row))?;
    }

    println!(
        "lookup table=DTL type={} zone={} raw={}",
        date_time_type.name(),
        zone,
        tz.get_raw_offset()
    );
    print_rows(&mut db, page, &tz)?;
    // both DateTimeType settings: the mode of the database at search time does
    // not change which rows a zoned key finds
    let other = match date_time_type {
        DateTimeType::Date => DateTimeType::LocalDateTime,
        DateTimeType::LocalDateTime => DateTimeType::Date,
    };
    for search_type in [date_time_type, other] {
        db.set_date_time_type(search_type);
        let mut probes = lookup_probes(&tz);
        let reread_probe = {
            let read = db.read_table_rows(page)?;
            read.into_iter()
                .flatten()
                .find(|row| row.first().and_then(|v| v.as_i64()) == Some(1))
                .and_then(|row| row.get(1).cloned())
                .unwrap_or(Value::Null)
        };
        probes.push(("reread".to_string(), reread_probe));
        for (label, value) in &probes {
            println!(
                "find={},{},{}",
                search_type.name(),
                label,
                join_ids(&lookup_ids(&mut db, "DTL", value)?)
            );
        }
    }
    let table = db
        .tables
        .get(&page)
        .ok_or_else(|| Error::Jackcess("table DTL is not loaded".into()))?;
    println!(
        "nowldt={}",
        table
            .get_created_date()
            .map(|d| ldt_text(&d))
            .unwrap_or_else(|| "-".to_string())
    );
    println!(
        "nowargs={}",
        table
            .get_created_date()
            .map(ldt_args)
            .unwrap_or_else(|| "-".to_string())
    );
    db.flush()
}

/// The ids of the rows of the given index range, in index order
/// (`CursorBuilder.setStartRow`/`setEndRow` on the single column index).
fn lookup_ids(db: &mut Database, table: &str, key: &Value) -> Result<Vec<i64>> {
    let mut row = vec![Value::Null; 2];
    row[1] = key.clone();
    let mut range = IndexRange::new();
    range.set_start_row(row.clone());
    range.set_end_row(row);
    db.find_table(table)?;
    let mut cursor = IndexCursor::new_with_range(db, table, "D1Index", &range)?;
    cursor.before_first();
    let mut ids = Vec::new();
    while let Some(values) = cursor.get_next_row(db)? {
        if let Some(id) = values.first().and_then(|v| v.as_i64()) {
            ids.push(id);
        }
    }
    ids.sort_unstable();
    Ok(ids)
}

fn join_ids(ids: &[i64]) -> String {
    if ids.is_empty() {
        return "-".to_string();
    }
    ids.iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

// ---------------------------------------------------------------------------
// shared dumping helpers
// ---------------------------------------------------------------------------

fn print_rows(db: &mut Database, page: i32, tz: &TimeZone) -> Result<()> {
    let rows = db.read_table_rows(page)?;
    let columns = {
        let table = db
            .tables
            .get(&page)
            .ok_or_else(|| Error::Jackcess("table is not loaded".into()))?;
        table
            .columns
            .iter()
            .map(|c| c.name.clone())
            .collect::<Vec<_>>()
    };
    for (number, row) in rows.into_iter().flatten().enumerate() {
        println!("row={number}");
        for (index, name) in columns.iter().enumerate() {
            let value = row.get(index).cloned().unwrap_or(Value::Null);
            println!("value={},{}", escape(name), describe(&value, tz));
        }
    }
    Ok(())
}

fn print_table_properties(db: &mut Database, table: &str, tz: &TimeZone) -> Result<()> {
    let maps = db.get_table_properties_by_name(table)?;
    for map in maps.iter() {
        let kind = map_kind(map.map_type());
        for prop in map.iter() {
            if matches!(
                prop.data_type,
                DataType::ShortDateTime | DataType::ExtDateTime
            ) {
                println!(
                    "prop={}|{}|{}|{}|{}|{}",
                    table,
                    kind,
                    escape(map.name()),
                    escape(&prop.name),
                    prop.data_type.get_name(),
                    describe(&prop.value, tz)
                );
            }
        }
    }
    Ok(())
}

fn map_kind(map_type: i16) -> &'static str {
    match map_type {
        0 => "default",
        1 => "column",
        2 => "index",
        _ => "other",
    }
}

fn describe(value: &Value, tz: &TimeZone) -> String {
    match value {
        Value::Null => "kind=null,class=-,text=null".to_string(),
        Value::DateBits(bits) => format!(
            "kind=date,class=java.util.Date,text=date(millis={};bits={})",
            column::date_ext_millis(*bits, tz),
            bits_hex(bits.to_bits())
        ),
        Value::DateTime(ldt) => format!(
            "kind=ldt,class=java.time.LocalDateTime,text={}",
            ldt_text(ldt)
        ),
        Value::Text(text) => format!(
            "kind=string,class=java.lang.String,text=\"{}\"",
            escape(text)
        ),
        Value::Bool(b) => format!("kind=bool,class=java.lang.Boolean,text={b}"),
        Value::Decimal { unscaled, scale } => {
            format!("kind=decimal,class=java.math.BigDecimal,text=dec({unscaled},{scale})")
        }
        Value::Byte(v) => format!("kind=number,class=java.lang.Byte,text={}", *v as i8),
        Value::Int(v) => format!("kind=number,class=java.lang.Short,text={v}"),
        Value::Long(v) => format!("kind=number,class=java.lang.Integer,text={v}"),
        Value::BigInt(v) => format!("kind=number,class=java.lang.Long,text={v}"),
        Value::Float(v) => format!("kind=number,class=java.lang.Float,text={v}"),
        Value::Double(v) => format!("kind=number,class=java.lang.Double,text={v}"),
        Value::Bytes(data) => format!("kind=bytes,class=[B,text=bytes({})", hex(data)),
        Value::Uuid(uuid) => {
            let hex: String = uuid.iter().map(|b| format!("{b:02x}")).collect();
            format!(
                "kind=uuid,class=java.util.UUID,text=uuid({}-{}-{}-{}-{})",
                &hex[0..8],
                &hex[8..12],
                &hex[12..16],
                &hex[16..20],
                &hex[20..32]
            )
        }
        Value::Keep => "kind=other,class=KEEP_VALUE,text=-".to_string(),
    }
}

fn hex(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2);
    for b in data {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// `ZoneDateGen.escape`: printable ASCII, every other character as `\uXXXX`.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\u{20}'..='\u{7e}' => out.push(c),
            other => out.push_str(&format!("\\u{:04x}", other as u32)),
        }
    }
    out
}

/// Keeps the `datenums` import obviously used when the conversions are all
/// routed through [`column`]/[`TimeZone`].
#[allow(dead_code)]
fn _unused(_: datenums::LocalDateTime) {}
