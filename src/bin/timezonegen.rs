//! Oracle-facing generator for the IANA time-zone parity harness
//! (`tools/verify-timezone.ps1`).
//!
//! Every command mirrors a command of `java-src/TimeZoneGen.java` and prints
//! the same lines from the same arguments, so the harness compares the two
//! outputs line by line (and compares the database files byte for byte).
//!
//! Commands:
//!
//! * `tzdb`: the IANA database version this implementation uses (recorded, not
//!   compared: the JVM ships its own tzdata).
//! * `conv <zone> <DATE|LOCAL_DATE_TIME>`: the instant matrix.  For each epoch
//!   millisecond it prints the `TimeZone` and `ZoneId` offsets, the local wall
//!   clock time of the instant and the stored `SHORT_DATE_TIME` bits produced
//!   by writing the instant as a `java.util.Date` and as its `LocalDateTime`,
//!   plus the values read back from both (`DateExt` millis and the exact
//!   `LocalDateTime` text).
//! * `wall <zone> <DATE|LOCAL_DATE_TIME>`: the wall-clock matrix.  For each
//!   `LocalDateTime` it prints the valid offsets (none in a gap, two in an
//!   overlap), the instant Java's `atZone` picks (a gap moves forward, an
//!   overlap uses the earlier offset), the wall clock of that instant and the
//!   `DATE`/`LOCAL_DATE_TIME` write bits and read-back values.
//! * `write <out> <FORMAT> <MODE> <zone> [--now-ms=N]`: creates the fixture
//!   database (an indexed date column, a date `DefaultValue`, a calculated date
//!   column on V2010+, and date valued table/column properties), fills it with
//!   wall clock times around the zone's DST transitions, `java.util.Date`
//!   instants, the LMT era and the year 9999 range, and prints the read-back
//!   values plus `nowms=`, the catalog timestamp, so the Java file's timestamp
//!   can be fed back to this generator.  Both files are compared byte for byte.
//! * `dump <file> <MODE> <zone>`: prints the catalog created/updated dates, the
//!   rows and every date valued property, used to compare the two files across
//!   implementations.
//!
//! The zone id is one of the harness' zones: `UTC`, `GMT-05:00`,
//! `America/New_York`, `Europe/Berlin`, `Asia/Shanghai`,
//! `Australia/Lord_Howe` (30 minute DST) and `Pacific/Apia` (the 2011 date-line
//! change).

use std::path::Path;

use chrono::Offset;
use jackcess::column::{self, ColumnSpec};
use jackcess::database::{Database, DateTimeType};
use jackcess::datatype::DataType;
use jackcess::datenums::{self, LocalDateTime, TimeZone};
use jackcess::error::{Error, Result};
use jackcess::format::FileFormat;
use jackcess::tablecreator::{IndexSpec, TableSpec};
use jackcess::value::Value;

/// 2020-03-15T10:30:45Z, the instant the other harnesses use.
const DATE_BASE_MS: i64 = 1_584_246_645_000;

/// The instant matrix shared with `TimeZoneGen.INSTANTS`:
///
/// * 1901-01-01T00:00:00Z, the 1900 `java.util.TimeZone` history cut-off and
///   the Shanghai LMT transition just after it;
/// * 1970-01-01 (0, -1, 1) and the 32-bit `time_t` edge
///   (2038-01-19T03:14:07Z / :08Z);
/// * DST transitions before/after each zone's gap and overlap: New York and
///   Berlin 2020, Lord Howe 2020/2021, Shanghai 1986, the Apia date-line change
///   of 2011-12-30 and the Apia DST end of 2012;
/// * pre-1901 LMT instants (1874/1890/1899) and the year 1 / year 9999 ends.
const INSTANTS: [i64; 36] = [
    // the epoch and the 1901 boundary
    0,
    1,
    -1,
    -86_400_000,
    -2_177_452_800_000, // 1901-01-01T00:00:00Z
    -2_177_481_943_000, // Shanghai +08:05:43 -> +08:00
    -2_208_988_800_001,
    -2_208_988_800_000, // the TimeZone history cut-off
    -2_208_988_799_999,
    // the 2038 boundary
    2_147_483_647_000,
    2_147_483_648_000,
    // New York 2020-03-08T07:00Z (gap) and 2020-11-01T06:00Z (overlap)
    1_583_650_799_000,
    1_583_650_800_000,
    1_604_210_399_000,
    1_604_210_400_000,
    // Berlin 2020-03-29T01:00Z and 2020-10-25T01:00Z
    1_585_443_599_000,
    1_585_443_600_000,
    1_603_587_599_000,
    1_603_587_600_000,
    // Lord Howe 2020-10-03T15:30Z (gap) and 2021-04-03T15:00Z (overlap)
    1_601_738_999_000,
    1_601_739_000_000,
    1_617_461_999_000,
    1_617_462_000_000,
    // Shanghai 1986-05-03T18:00Z (gap) and 1986-09-13T17:00Z (overlap)
    515_527_199_000,
    515_527_200_000,
    527_014_799_000,
    527_014_800_000,
    // Apia 2011-12-30T10:00Z (the date-line change) and 2012-03-31T14:00Z
    1_325_239_199_000,
    1_325_239_200_000,
    1_333_202_399_000,
    1_333_202_400_000,
    // pre-1901 LMT, the OLE epoch, the common harness instant and year ends
    -3_000_000_000_000,
    -2_500_000_000_000,
    DATE_BASE_MS,
    -12_219_292_800_000,
    253_402_300_799_999,
];

/// The wall clock matrix shared with `TimeZoneGen.localTimes()`: the ordinary
/// wall clock time, the gap/overlap local times of every harness zone (New York
/// and Berlin one hour, Lord Howe 30 minutes, Shanghai one hour, Apia a whole
/// skipped day and a one hour overlap), the LMT era, the 1901/1970/2038
/// boundaries, year 1 / year 9999 and a sub-millisecond value.
fn local_times() -> Vec<LocalDateTime> {
    vec![
        LocalDateTime::new(2020, 3, 15, 12, 30, 45, 0),
        LocalDateTime::new(2020, 3, 15, 12, 30, 45, 123_456_789),
        // New York 2020: gap 02:00-03:00, overlap 01:00-02:00
        LocalDateTime::new(2020, 3, 8, 1, 59, 59, 0),
        LocalDateTime::new(2020, 3, 8, 2, 0, 0, 0),
        LocalDateTime::new(2020, 3, 8, 2, 30, 0, 0),
        LocalDateTime::new(2020, 3, 8, 3, 0, 0, 0),
        LocalDateTime::new(2020, 11, 1, 1, 0, 0, 0),
        LocalDateTime::new(2020, 11, 1, 1, 30, 0, 0),
        LocalDateTime::new(2020, 11, 1, 2, 0, 0, 0),
        // Berlin 2020: gap 02:00-03:00, overlap 02:00-03:00
        LocalDateTime::new(2020, 3, 29, 1, 59, 59, 0),
        LocalDateTime::new(2020, 3, 29, 2, 30, 0, 0),
        LocalDateTime::new(2020, 10, 25, 2, 30, 0, 0),
        // Lord Howe 2020-10-04: gap 02:00-02:30 (30 minutes)
        LocalDateTime::new(2020, 10, 4, 1, 59, 59, 0),
        LocalDateTime::new(2020, 10, 4, 2, 15, 0, 0),
        LocalDateTime::new(2020, 10, 4, 2, 45, 0, 0),
        // Lord Howe 2021-04-04: overlap 01:30-02:00 (30 minutes)
        LocalDateTime::new(2021, 4, 4, 1, 30, 0, 0),
        LocalDateTime::new(2021, 4, 4, 1, 45, 0, 0),
        LocalDateTime::new(2021, 4, 4, 2, 0, 0, 0),
        // Apia 2011-12-30 does not exist at all
        LocalDateTime::new(2011, 12, 29, 23, 59, 59, 0),
        LocalDateTime::new(2011, 12, 30, 0, 0, 0, 0),
        LocalDateTime::new(2011, 12, 30, 12, 0, 0, 0),
        LocalDateTime::new(2011, 12, 31, 0, 0, 0, 0),
        // Apia 2012-04-01: overlap 03:00-04:00
        LocalDateTime::new(2012, 4, 1, 3, 30, 0, 0),
        // Shanghai 1986: gap 02:00-03:00, overlap 01:00-02:00
        LocalDateTime::new(1986, 5, 4, 2, 30, 0, 0),
        LocalDateTime::new(1986, 9, 14, 1, 30, 0, 0),
        // the 1901 boundary, the LMT era and the OLE epoch
        LocalDateTime::new(1901, 1, 1, 0, 0, 0, 0),
        LocalDateTime::new(1900, 12, 31, 23, 59, 59, 0),
        LocalDateTime::new(1900, 1, 1, 0, 0, 0, 0),
        LocalDateTime::new(1890, 6, 1, 0, 0, 0, 0),
        LocalDateTime::new(1899, 12, 30, 0, 0, 0, 0),
        // 1970, 2038, year 1 and year 9999
        LocalDateTime::new(1970, 1, 1, 0, 0, 0, 0),
        LocalDateTime::new(2038, 1, 19, 3, 14, 8, 0),
        LocalDateTime::new(1, 1, 1, 0, 0, 0, 0),
        LocalDateTime::new(9999, 12, 31, 23, 59, 59, 999_000_000),
    ]
}

fn usage() -> ! {
    eprintln!(
        "usage: timezonegen tzdb\n\
         \x20      timezonegen conv <zone> <DATE|LOCAL_DATE_TIME>\n\
         \x20      timezonegen wall <zone> <DATE|LOCAL_DATE_TIME>\n\
         \x20      timezonegen write <out> <FORMAT> <MODE> <zone> [--now-ms=N]\n\
         \x20      timezonegen dump <file> <MODE> <zone>"
    );
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage();
    }
    let result = match args[1].as_str() {
        "tzdb" => {
            println!("tzdb={}", chrono_tz::IANA_TZDB_VERSION);
            Ok(())
        }
        "conv" => {
            if args.len() < 4 {
                usage();
            }
            conv(&args[2], DateTimeType::from_name(&args[3]))
        }
        "wall" => {
            if args.len() < 4 {
                usage();
            }
            wall(&args[2], DateTimeType::from_name(&args[3]))
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
                now_ms(&args[6..]),
            )
        }
        "dump" => {
            if args.len() < 5 {
                usage();
            }
            dump(
                Path::new(&args[2]),
                DateTimeType::from_name(&args[3]),
                &args[4],
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

fn now_ms(args: &[String]) -> Option<i64> {
    args.iter()
        .find_map(|arg| arg.strip_prefix("--now-ms="))
        .and_then(|text| text.trim().parse::<i64>().ok())
}

fn zone_of(zone: &str) -> Result<TimeZone> {
    TimeZone::from_zone_id(zone)
        .ok_or_else(|| Error::IllegalArgument(format!("Unknown time-zone ID: '{}'", zone)))
}

fn table_page(db: &mut Database, name: &str) -> Result<i32> {
    match db.find_table(name)? {
        Some(table) => Ok(table.table_def_page_number),
        None => Err(Error::Jackcess(format!("table {name} does not exist"))),
    }
}

fn ldt_text(ldt: &LocalDateTime) -> String {
    column::java_local_date_time_text(ldt)
}

fn bits_hex(bits: u64) -> String {
    format!("{bits:016x}")
}

/// `java.time.ZoneOffset.toString()`: `Z`, or `±HH:MM[:SS]`.
fn java_offset(offset_ms: i64) -> String {
    if offset_ms == 0 {
        return "Z".to_string();
    }
    let sign = if offset_ms < 0 { '-' } else { '+' };
    let total = offset_ms.unsigned_abs() / 1000;
    let (hours, minutes, seconds) = (total / 3600, (total / 60) % 60, total % 60);
    if seconds == 0 {
        format!("{sign}{hours:02}:{minutes:02}")
    } else {
        format!("{sign}{hours:02}:{minutes:02}:{seconds:02}")
    }
}

/// `ZoneRules.getValidOffsets(local)` as Java prints the list: no offset for a
/// gap, two offsets (earlier first) for an overlap, one otherwise.
fn valid_offsets(zone: &TimeZone, ldt: LocalDateTime) -> Vec<i64> {
    let raw = zone.get_raw_offset();
    let Some(tz) = zone
        .zone_id()
        .parse::<chrono_tz::Tz>()
        .ok()
        .filter(|_| zone.is_named())
    else {
        // a fixed offset zone: the local time is always valid with the raw
        // offset
        return vec![raw];
    };
    let Some(naive) = chrono::NaiveDate::from_ymd_opt(ldt.year, ldt.month, ldt.day)
        .and_then(|d| d.and_hms_nano_opt(ldt.hour, ldt.minute, ldt.second, ldt.nano))
    else {
        return vec![raw];
    };
    match chrono::TimeZone::from_local_datetime(&tz, &naive) {
        chrono::LocalResult::Single(dt) => {
            vec![dt.offset().fix().local_minus_utc() as i64 * 1000]
        }
        chrono::LocalResult::Ambiguous(earlier, later) => vec![
            earlier.offset().fix().local_minus_utc() as i64 * 1000,
            later.offset().fix().local_minus_utc() as i64 * 1000,
        ],
        chrono::LocalResult::None => Vec::new(),
    }
}

fn zone_header(zone: &str, tz: &TimeZone, date_time_type: DateTimeType) -> String {
    format!(
        "zone={} mode={} raw={} zoneid={}",
        zone,
        date_time_type.name(),
        tz.get_raw_offset(),
        tz.zone_id()
    )
}

// ---------------------------------------------------------------------------
// conv: the instant matrix
// ---------------------------------------------------------------------------

fn conv(zone: &str, date_time_type: DateTimeType) -> Result<()> {
    let tz = zone_of(zone)?;
    println!("{}", zone_header(zone, &tz, date_time_type));
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

// ---------------------------------------------------------------------------
// wall: the local wall clock matrix
// ---------------------------------------------------------------------------

fn wall(zone: &str, date_time_type: DateTimeType) -> Result<()> {
    let tz = zone_of(zone)?;
    println!("{}", zone_header(zone, &tz, date_time_type));
    for ldt in local_times() {
        let valid = valid_offsets(&tz, ldt);
        let offsets = if valid.is_empty() {
            "[]".to_string()
        } else {
            format!(
                "[{}]",
                valid
                    .iter()
                    .map(|o| java_offset(*o))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        // LocalDateTime.atZone: a gap moves forward, an overlap picks the
        // earlier offset; `offset_for_local` is that rule
        let at_zone = tz.local_to_epoch_millis(ldt);
        // `LocalDateTime.ofInstant(ldt.atZone(zone).toInstant(), zone)`: the
        // instant keeps the nanoseconds, and the wall clock of the instant is
        // the original one (gap/overlap resolved) with the sub-millisecond part
        // unchanged
        let back = tz.local_date_time_from_millis(at_zone).with_nano(ldt.nano);
        let w = column::date_value_bits_with(&Value::DateTime(ldt), &tz, date_time_type)?;
        let r_date = column::date_ext_millis(f64::from_bits(w), &tz);
        let r_ldt = column::ldt_from_local_date_double(f64::from_bits(w));
        println!(
            "lt={} valid={} offsets={} atzone={} back={} w={} rdate={} rdateldt={}",
            ldt_text(&ldt),
            valid.len(),
            offsets,
            at_zone,
            ldt_text(&back),
            bits_hex(w),
            r_date,
            ldt_text(&r_ldt),
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// write: the byte-identical fixture
// ---------------------------------------------------------------------------

/// The zone specific wall clock times of the fixture: the spring-forward gap
/// and the autumn overlap of the zone being written.
fn zone_transition_times(zone: &str) -> (LocalDateTime, LocalDateTime) {
    match zone {
        "Australia/Lord_Howe" => (
            LocalDateTime::new(2020, 10, 4, 2, 15, 0, 0),
            LocalDateTime::new(2021, 4, 4, 1, 45, 0, 0),
        ),
        "Pacific/Apia" => (
            LocalDateTime::new(2011, 12, 30, 12, 0, 0, 0),
            LocalDateTime::new(2012, 4, 1, 3, 30, 0, 0),
        ),
        "Europe/Berlin" => (
            LocalDateTime::new(2020, 3, 29, 2, 30, 0, 0),
            LocalDateTime::new(2020, 10, 25, 2, 30, 0, 0),
        ),
        "Asia/Shanghai" => (
            LocalDateTime::new(1986, 5, 4, 2, 30, 0, 0),
            LocalDateTime::new(1986, 9, 14, 1, 30, 0, 0),
        ),
        // America/New_York, and the fixed offset zones which have no transition
        _ => (
            LocalDateTime::new(2020, 3, 8, 2, 30, 0, 0),
            LocalDateTime::new(2020, 11, 1, 1, 30, 0, 0),
        ),
    }
}

fn fixture_rows(zone: &str) -> Vec<Vec<Value>> {
    let (gap, overlap) = zone_transition_times(zone);
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
            Value::DateTime(gap),
            instant(0),
            Value::DateTime(overlap),
            Value::Text("dst".to_string()),
        ],
        vec![
            Value::Null,
            // the LMT era (pre-1901, before every zone's first transition)
            ldt(1890, 6, 1, 0, 0, 0, 0),
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
            instant(2_147_483_648_000),
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
}

fn write(
    path: &Path,
    format: FileFormat,
    date_time_type: DateTimeType,
    zone: &str,
    now: Option<i64>,
) -> Result<()> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let mut db = Database::create(format, path)?;
    db.set_time_zone_id(zone)?;
    db.set_date_time_type(date_time_type);
    if let Some(now) = now {
        // the catalog timestamps Java created: the same instant converted
        // through the database zone (Java's `new Date()` -> catalog Date)
        db.now_ms = Some(now);
        db.now_queue = vec![now];
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
    let page = db.create_table(&spec)?;

    // date valued properties: the zone's gap wall clock time, a java.util.Date
    // instant and a historical (pre-1901) time, at table and column level
    {
        let (gap, _) = zone_transition_times(zone);
        let mut maps = db.get_table_properties_by_name("DT")?;
        maps.get_default()
            .put_value("ZonedLdt", Value::DateTime(gap))?;
        maps.get_default().put_value(
            "ZonedInstant",
            Value::DateTime(tz.local_date_time_from_millis(DATE_BASE_MS)),
        )?;
        maps.get_default().put_value(
            "Historic",
            Value::DateTime(LocalDateTime::new(1890, 6, 1, 0, 0, 0, 0)),
        )?;
        maps.get_mut("D1").put_value(
            "ColZonedInstant",
            Value::DateTime(tz.local_date_time_from_millis(DATE_BASE_MS + 1)),
        )?;
        maps.save(&mut db)?;
    }

    let rows = fixture_rows(zone);
    for row in &rows {
        db.add_rows("DT", std::slice::from_ref(row))?;
    }
    // read the rows back and write the values again: in DATE mode they are
    // DateExt values which keep their stored bits
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

    println!(
        "write table=DT type={} zone={} raw={} rows={} cols={}",
        date_time_type.name(),
        zone,
        tz.get_raw_offset(),
        rows.len() + reread.len(),
        db.tables.get(&page).map(|t| t.columns.len()).unwrap_or(0)
    );
    print_rows(&mut db, page, &tz)?;
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
    println!("nowms={}", now.unwrap_or(-1));
    db.flush()
}

// ---------------------------------------------------------------------------
// dump: the cross-implementation read
// ---------------------------------------------------------------------------

fn dump(path: &Path, date_time_type: DateTimeType, zone: &str) -> Result<()> {
    let mut db = Database::open_path(path)?;
    let tz = zone_of(zone)?;
    db.set_time_zone(tz);
    db.set_date_time_type(date_time_type);
    println!(
        "dump type={} zone={} raw={}",
        date_time_type.name(),
        zone,
        tz.get_raw_offset()
    );
    let page = table_page(&mut db, "DT")?;
    let (table_name, created, updated) = {
        let table = db
            .tables
            .get(&page)
            .ok_or_else(|| Error::Jackcess("table DT is not loaded".into()))?;
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
    print_table_properties(&mut db, "DT", &tz)?;
    Ok(())
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
                    // a date valued property is read as the same `DateExt` a
                    // row read produces (Java keeps the stored bits), so the
                    // row description is the right one
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

/// `TimeZoneGen.escape`: printable ASCII, every other character as `\uXXXX`.
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

/// Keeps the `datenums` import used when the conversions are all routed through
/// [`column`]/[`TimeZone`].
#[allow(dead_code)]
fn _unused(_: datenums::LocalDateTime) {}
