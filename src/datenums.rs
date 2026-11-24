//! Date/time handling which mirrors the Java library's use of
//! `java.time.LocalDateTime`, `java.util.Date` and Access "OLE" date doubles.
//!
//! Access stores dates as a double holding the number of days since
//! 1899-12-30, with the time of day as the (always positive) fraction.

/// Proleptic Gregorian date/time with nanosecond precision, mirroring
/// `java.time.LocalDateTime` closely enough for jackcess' needs.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct LocalDateTime {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub nano: u32,
}

pub const MILLISECONDS_PER_DAY: i64 = 24 * 60 * 60 * 1000;
pub const SECONDS_PER_DAY: i64 = 24 * 60 * 60;
pub const NANOS_PER_SECOND: i64 = 1_000_000_000;
pub const MILLIS_BETWEEN_EPOCH_AND_1900: i64 = 25569 * MILLISECONDS_PER_DAY;

/// Days from 1970-01-01 to 1899-12-30 (negative because it is earlier).
pub const EPOCH_DAYS_TO_1899_12_30: i64 = -25569;

/// `TIME_ROUNDING_NANOS` from `ColumnImpl`: the rounding units tried for the
/// fractional second of a date read from a double, coarsest first.  The first
/// one which converts back to the same double is used, so a value this library
/// wrote (clean to the millisecond) keeps the coarse rounding while a value
/// Access wrote keeps as much of the sub-millisecond part as the double holds.
const TIME_ROUNDING_NANOS: [i64; 7] = [1_000_000, 100_000, 10_000, 1_000, 100, 10, 1];

impl LocalDateTime {
    pub fn new(
        year: i32,
        month: u32,
        day: u32,
        hour: u32,
        minute: u32,
        second: u32,
        nano: u32,
    ) -> LocalDateTime {
        LocalDateTime {
            year,
            month,
            day,
            hour,
            minute,
            second,
            nano,
        }
    }

    pub fn of_date(year: i32, month: u32, day: u32) -> LocalDateTime {
        LocalDateTime::new(year, month, day, 0, 0, 0, 0)
    }

    /// Days since 1970-01-01 (the Unix epoch day).
    pub fn to_epoch_day(self) -> i64 {
        days_from_civil(self.year, self.month, self.day)
    }

    pub fn from_epoch_day(days: i64) -> LocalDateTime {
        let (y, m, d) = civil_from_days(days);
        LocalDateTime::new(y, m, d, 0, 0, 0, 0)
    }

    /// Milliseconds since the Unix epoch, truncated (like `Date.getTime()`).
    pub fn to_epoch_millis(self) -> i64 {
        let days = self.to_epoch_day();
        let secs = self.hour as i64 * 3600 + self.minute as i64 * 60 + self.second as i64;
        days * MILLISECONDS_PER_DAY + secs * 1000 + (self.nano as i64) / 1_000_000
    }

    pub fn from_epoch_millis(millis: i64) -> LocalDateTime {
        let mut days = millis.div_euclid(MILLISECONDS_PER_DAY);
        let mut ms_of_day = millis.rem_euclid(MILLISECONDS_PER_DAY);
        // Java's Date is a UTC instant; callers apply the zone themselves.
        let _ = &mut days;
        let hour = (ms_of_day / 3_600_000) as u32;
        ms_of_day %= 3_600_000;
        let minute = (ms_of_day / 60_000) as u32;
        ms_of_day %= 60_000;
        let second = (ms_of_day / 1000) as u32;
        let milli = (ms_of_day % 1000) as u32;
        let (y, m, d) = civil_from_days(days);
        LocalDateTime::new(y, m, d, hour, minute, second, milli * 1_000_000)
    }

    pub fn with_nano(self, nano: u32) -> LocalDateTime {
        LocalDateTime { nano, ..self }
    }

    /// `LocalDate.of(...).until(other)` in days.
    pub fn days_until(self, other: LocalDateTime) -> i64 {
        other.to_epoch_day() - self.to_epoch_day()
    }

    /// Seconds of the time-of-day part.
    pub fn second_of_day(self) -> i64 {
        self.hour as i64 * 3600 + self.minute as i64 * 60 + self.second as i64
    }
}

/// Days from 1970-01-01 for the given proleptic Gregorian date.
/// (Howard Hinnant's `days_from_civil`.)
pub fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = y as i64 - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = ((m as i64) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + (d as i64) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Inverse of [`days_from_civil`].
pub fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

/// The instant at which `java.util.TimeZone` stops using the zone's raw offset
/// for history and starts following the transition table: JDK's `ZoneInfoFile`
/// drops every transition before `UTC1900` and prepends one entry at
/// `1900-01-01T00:00:00Z` (`ZoneInfoFile.UTC1900`), so `TimeZone.getOffset`
/// returns `getRawOffset()` for every earlier instant.  `java.time.ZoneId` has
/// no such cut-off and keeps the historical LMT offset there, which is why the
/// port models both (`TimeZone::get_offset` vs
/// [`TimeZone::zone_offset`]).
pub const TIME_ZONE_HISTORY_START_MILLIS: i64 = -2_208_988_800_000;

/// The concrete zone of a [`TimeZone`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ZoneKind {
    /// A fixed offset zone such as `UTC` or `GMT-05:00` (`java.time.ZoneOffset`,
    /// and the `TimeZone` Java derives from it); it observes no daylight saving
    /// time, so the offset is the same for every instant.
    Fixed { offset_ms: i64 },
    /// A named IANA zone from the tz database (`Asia/Shanghai`,
    /// `America/New_York`), with its full historical and daylight saving rules.
    Named(chrono_tz::Tz),
}

/// The two faces of a time zone, matching the pair of values Java's
/// `DatabaseImpl.setZoneInfo` keeps for a database:
///
/// * a `java.util.TimeZone` (`Database.getTimeZone`), whose `getOffset` returns
///   the raw offset before 1900 (`ZoneInfoFile` drops the earlier history) and
///   which `ColumnImpl` uses for the `DateTimeType.DATE` conversions;
/// * a `java.time.ZoneId` (`Database.getZoneId`), which keeps the historical
///   LMT offset for any instant and which the `DateTimeType.LOCAL_DATE_TIME`
///   conversions use.
///
/// A fixed offset zone (`UTC`, `GMT-05:00`) has the same offset in both roles.
/// Named zones come from the bundled IANA database (`chrono-tz`), so
/// [`TimeZone::get_offset`] follows the zone's historical and daylight saving
/// transitions exactly like `TimeZone.getOffset` does.
///
/// # tzdata version
///
/// `chrono-tz` 0.10.4 bundles IANA tzdb **2025b**, while the JDK used for the
/// oracle ships its own copy (currently **2026f**).  The rules of the zones the
/// time-zone harness compares (`UTC`, `GMT-05:00`, `America/New_York`,
/// `Europe/Berlin`, `Asia/Shanghai`, `Australia/Lord_Howe`, `Pacific/Apia`)
/// are unchanged between those releases, so their offsets, LMT history and DST
/// transitions match Java exactly for every tested instant (1874 through
/// 9999).  A zone whose rules *did* change between the two releases (for
/// example the 2026f changes to `Africa/Casablanca`, `America/Vancouver` or the
/// `CST6CDT`-style legacy ids) can differ from that JDK; `tools/verify-zonedates.ps1`
/// enumerates those known differences, and the time-zone harness records both
/// versions instead of asserting they are equal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeZone {
    /// offset east of UTC in milliseconds
    pub raw_offset_ms: i64,
    kind: ZoneKind,
}

impl Default for TimeZone {
    fn default() -> Self {
        // allow overriding for tests; otherwise +08:00
        if let Ok(id) = std::env::var("JACKCESS_TZ") {
            if let Some(tz) = TimeZone::from_zone_id(id.trim()) {
                return tz;
            }
        }
        let offset_minutes = std::env::var("JACKCESS_TZ_OFFSET_MINUTES")
            .ok()
            .and_then(|v| v.trim().parse::<i64>().ok())
            .unwrap_or(480);
        TimeZone::from_offset_minutes(offset_minutes as i32)
    }
}

impl TimeZone {
    /// A fixed offset zone (`java.util.TimeZone.getTimeZone("GMT-05:00")` /
    /// `"UTC"`): neither of those zones observes daylight saving time, so the
    /// offset is the same for every instant.
    pub fn from_offset_minutes(minutes: i32) -> TimeZone {
        TimeZone::from_raw_offset_ms(minutes as i64 * 60_000)
    }

    /// A fixed offset zone from a raw offset in milliseconds (Java's
    /// `TimeZone.setRawOffset`).
    pub fn from_raw_offset_ms(millis: i64) -> TimeZone {
        TimeZone {
            raw_offset_ms: millis,
            kind: ZoneKind::Fixed { offset_ms: millis },
        }
    }

    /// `TimeZone.getTimeZone("UTC")`.
    pub fn utc() -> TimeZone {
        TimeZone::from_offset_minutes(0)
    }

    /// Builds the zone Java's `ZoneId.of(id)` / `TimeZone.getTimeZone(id)` pair
    /// describes: a named IANA zone when the tz database knows the id, otherwise
    /// a fixed offset zone for the `UTC` / `GMT` / `GMT+05:30` / `+05:30` forms.
    /// `None` when Java would throw `ZoneRulesException` /
    /// `DateTimeException`.
    pub fn from_zone_id(id: &str) -> Option<TimeZone> {
        let trimmed = id.trim();
        if let Ok(tz) = trimmed.parse::<chrono_tz::Tz>() {
            return Some(TimeZone::named(tz));
        }
        parse_fixed_offset_zone_id(trimmed).map(TimeZone::from_raw_offset_ms)
    }

    /// A named IANA zone, like `ZoneId.of("America/New_York")` combined with
    /// the `TimeZone` Java derives from it.
    pub fn from_tz(tz: chrono_tz::Tz) -> TimeZone {
        TimeZone::named(tz)
    }

    fn named(tz: chrono_tz::Tz) -> TimeZone {
        TimeZone {
            raw_offset_ms: java_raw_offset(tz),
            kind: ZoneKind::Named(tz),
        }
    }

    /// The canonical id of the zone: the IANA id for a named zone, and the
    /// `UTC` / `GMT±HH:MM` form Java's `TimeZone.getTimeZone(...).toZoneId()`
    /// reports for a fixed offset zone.
    pub fn zone_id(&self) -> String {
        match self.kind {
            ZoneKind::Named(tz) => tz.name().to_string(),
            ZoneKind::Fixed { .. } => fixed_offset_zone_id(self.raw_offset_ms),
        }
    }

    /// `true` for an IANA zone with a transition table, `false` for a fixed
    /// offset zone.
    pub fn is_named(&self) -> bool {
        matches!(self.kind, ZoneKind::Named(_))
    }

    /// `TimeZone.getOffset(millis)`: the offset (including DST) Java's
    /// `java.util.TimeZone` reports.  For a named zone the offset is the raw
    /// offset for any instant before 1900 (see
    /// [`TIME_ZONE_HISTORY_START_MILLIS`]) and the tz database offset
    /// afterwards.
    pub fn get_offset(&self, millis: i64) -> i64 {
        match self.kind {
            ZoneKind::Fixed { offset_ms } => offset_ms,
            ZoneKind::Named(_) if millis < TIME_ZONE_HISTORY_START_MILLIS => self.raw_offset_ms,
            ZoneKind::Named(tz) => named_offset_at(tz, millis),
        }
    }

    /// `ZoneId.getRules().getOffset(instant)`: the historical offset of the
    /// zone, including the LMT offset before the first transition, which
    /// `java.time` keeps for any instant.
    pub fn zone_offset(&self, millis: i64) -> i64 {
        match self.kind {
            ZoneKind::Fixed { offset_ms } => offset_ms,
            ZoneKind::Named(tz) => named_offset_at(tz, millis),
        }
    }

    /// The offset to use for a *local* wall clock time, like
    /// `LocalDateTime.atZone(zoneId)`: the single valid offset when the local
    /// time exists once, the earlier offset when it is repeated by a fall-back
    /// transition, and the offset before the gap when the local time does not
    /// exist at all (Java moves such a value forward by the gap length, which
    /// yields `local - offsetBefore`).
    pub fn offset_for_local(&self, ldt: LocalDateTime) -> i64 {
        let tz = match self.kind {
            ZoneKind::Fixed { offset_ms } => return offset_ms,
            ZoneKind::Named(tz) => tz,
        };
        let naive = match naive_from_ldt(ldt) {
            Some(naive) => naive,
            // outside the range chrono can represent: fall back to the raw
            // offset, like the pre-history part of the zone
            None => return self.raw_offset_ms,
        };
        match chrono::TimeZone::from_local_datetime(&tz, &naive) {
            chrono::LocalResult::Single(dt) => fixed_offset_ms(dt.offset()),
            // the `Ambiguous` results are ordered earliest first, and
            // `ZonedDateTime.ofLocal` picks the earlier offset, i.e. the first
            chrono::LocalResult::Ambiguous(earlier, _later) => fixed_offset_ms(earlier.offset()),
            chrono::LocalResult::None => match chrono_tz::GapInfo::new(&naive, &tz) {
                Some(info) => info
                    .begin
                    .map(|(_, before)| fixed_offset_ms(&before))
                    .unwrap_or(self.raw_offset_ms),
                None => self.raw_offset_ms,
            },
        }
    }

    /// `TimeZone.getRawOffset()`.
    pub fn get_raw_offset(&self) -> i64 {
        self.raw_offset_ms
    }

    /// The offset east of UTC in whole minutes (the inverse of
    /// [`TimeZone::from_offset_minutes`]).
    pub fn offset_minutes(&self) -> i32 {
        (self.raw_offset_ms / 60_000) as i32
    }

    /// Converts epoch milliseconds (a `java.util.Date` instant) to the local
    /// wall clock time of this zone, like
    /// `LocalDateTime.ofInstant(Instant.ofEpochMilli(millis), zoneId)` does
    /// (the `ZoneId` face of the zone, so historical offsets included).
    pub fn local_date_time_from_millis(&self, millis: i64) -> LocalDateTime {
        LocalDateTime::from_epoch_millis(millis + self.zone_offset(millis))
    }

    /// `LocalDateTime.atZone(getZoneId()).toInstant().toEpochMilli()`: the
    /// epoch milliseconds of a local wall clock time in this zone.  The
    /// sub-millisecond part is truncated like `Instant.toEpochMilli` (towards
    /// negative infinity for instants before the epoch).
    pub fn local_to_epoch_millis(&self, ldt: LocalDateTime) -> i64 {
        ldt.to_epoch_millis() - self.offset_for_local(ldt)
    }
}

/// The offset of a named zone at an instant, in milliseconds east of UTC.
fn named_offset_at(tz: chrono_tz::Tz, millis: i64) -> i64 {
    match chrono::TimeZone::timestamp_millis_opt(&tz, millis) {
        chrono::LocalResult::Single(dt) => fixed_offset_ms(dt.offset()),
        // outside the representable range: the zone's last rule still applies
        // (Java's ZoneRules extrapolates the final transition rule forever)
        _ => named_offset_far_future(tz),
    }
}

/// `chrono::FixedOffset` (from a `TzOffset`) in milliseconds east of UTC.
fn fixed_offset_ms<O: chrono::Offset>(offset: &O) -> i64 {
    offset.fix().local_minus_utc() as i64 * 1000
}

fn naive_from_ldt(ldt: LocalDateTime) -> Option<chrono::NaiveDateTime> {
    chrono::NaiveDate::from_ymd_opt(ldt.year, ldt.month, ldt.day)
        .and_then(|date| date.and_hms_nano_opt(ldt.hour, ldt.minute, ldt.second, ldt.nano))
}

/// The zone offset at a reference instant well after the last transition of the
/// tz database, where only the zone's final rule applies.
fn named_offset_far_future(tz: chrono_tz::Tz) -> i64 {
    let naive = chrono::NaiveDate::from_ymd_opt(2200, 1, 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .expect("valid reference date");
    fixed_offset_ms(&chrono::TimeZone::offset_from_utc_datetime(&tz, &naive))
}

/// `TimeZone.getRawOffset()` of the `java.util.TimeZone` Java derives from a
/// `ZoneId` (`TimeZone.getTimeZone(zoneId)`).
///
/// The JDK's `ZoneInfoFile` records this as the last entry of the zone's
/// *standard* offsets, i.e. the offset the zone uses outside daylight saving
/// time in its current era (including zones which model their winter as a
/// negative daylight saving offset, such as `Europe/Dublin`, where the JDK
/// reports `0` rather than the `+01:00` base offset).  The port derives it as
/// the smallest offset over the last two years the bundled tz database
/// describes (2090 and 2099, the range `zic -r` emits), which is the standard
/// offset of every zone that observes daylight saving time; a zone without a
/// DST rule gets its constant offset.  (The bundled database freezes the last
/// transition's offset after 2099, so a later year would sample that frozen
/// offset instead of the standard one.)
fn java_raw_offset(tz: chrono_tz::Tz) -> i64 {
    let mut min = named_offset_far_future(tz);
    for year in [2090i32, 2099] {
        for month in 1..=12u32 {
            let Some(date) = chrono::NaiveDate::from_ymd_opt(year, month, 15) else {
                continue;
            };
            let naive = date.and_hms_opt(12, 0, 0).expect("valid reference time");
            let offset = fixed_offset_ms(&chrono::TimeZone::offset_from_utc_datetime(&tz, &naive));
            if offset < min {
                min = offset;
            }
        }
    }
    min
}

/// The `UTC` / `GMT±HH:MM` id Java's
/// `TimeZone.getTimeZone(...).toZoneId().getId()` reports for a fixed offset
/// zone (a zero offset is `UTC`, every other one is the `GMT` form).
fn fixed_offset_zone_id(offset_ms: i64) -> String {
    if offset_ms == 0 {
        return "UTC".to_string();
    }
    let minutes = (offset_ms / 60_000) as i32;
    let sign = if minutes < 0 { '-' } else { '+' };
    let abs = minutes.unsigned_abs();
    format!("GMT{}{:02}:{:02}", sign, abs / 60, abs % 60)
}

/// `ZoneId.of(id)` for the fixed offset forms: `Z`, `UTC`, `UT`, `GMT`,
/// `UTC+01:00`, `GMT-5`, `+05:30`, `-08:00:15`.  Returns the offset in
/// milliseconds east of UTC.
fn parse_fixed_offset_zone_id(id: &str) -> Option<i64> {
    if id.is_empty() {
        return None;
    }
    if id.eq_ignore_ascii_case("Z") {
        return Some(0);
    }
    // `UTC` and `GMT` are the same three character prefix, so they share a
    // branch; `UT` is the two character prefix both of them start with.
    let (sign_part, prefix_len) = if id.len() >= 3
        && (id[..3].eq_ignore_ascii_case("UTC") || id[..3].eq_ignore_ascii_case("GMT"))
    {
        (&id[3..], 3)
    } else if id.len() >= 2 && id[..2].eq_ignore_ascii_case("UT") {
        (&id[2..], 2)
    } else {
        (id, 0)
    };
    let _ = prefix_len;
    if sign_part.is_empty() {
        // a bare `UTC`, `UT` or `GMT`
        return Some(0);
    }
    let (sign, rest) = match sign_part.as_bytes()[0] {
        b'+' => (1i64, &sign_part[1..]),
        b'-' => (-1i64, &sign_part[1..]),
        _ => return None,
    };
    if rest.is_empty() {
        return None;
    }
    let mut parts = rest.split(':');
    let hours: i64 = parts.next()?.parse().ok()?;
    let minutes: i64 = match parts.next() {
        Some(part) => part.parse().ok()?,
        None => 0,
    };
    let seconds: i64 = match parts.next() {
        Some(part) => part.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() || hours > 23 || minutes > 59 || seconds > 59 {
        return None;
    }
    Some(sign * (hours * 3_600_000 + minutes * 60_000 + seconds * 1000))
}

/// `ColumnImpl.getToLocalTimeZoneOffset`: the offset (including DST) which
/// converts a UTC instant (epoch milliseconds) to the local wall clock time of
/// `tz`.
pub fn get_to_local_time_zone_offset(time: i64, tz: &TimeZone) -> i64 {
    tz.get_offset(time)
}

/// `ColumnImpl.getFromLocalTimeZoneOffset`: the offset which converts a local
/// wall clock time (epoch milliseconds) back to a UTC instant.  Java applies
/// the raw offset first to get closer to the original time, because a DST
/// aware zone's offset depends on the instant.
pub fn get_from_local_time_zone_offset(time: i64, tz: &TimeZone) -> i64 {
    tz.get_offset(time - tz.get_raw_offset())
}

/// `ColumnImpl.fromLocalDateDouble`
pub fn from_local_date_double(value: f64) -> i64 {
    let date_part = (value as i64) * MILLISECONDS_PER_DAY;
    let time_part = (value.abs() % 1.0 * MILLISECONDS_PER_DAY as f64).round() as i64;
    let time = date_part + time_part;
    time - MILLIS_BETWEEN_EPOCH_AND_1900
}

/// `ColumnImpl.fromDateDouble(value, tz)`: the `java.util.Date` instant
/// (`Date.getTime()`) of a stored OLE date double, in `tz`.
pub fn from_date_double(value: f64, tz: &TimeZone) -> i64 {
    let local_time = from_local_date_double(value);
    local_time - get_from_local_time_zone_offset(local_time, tz)
}

/// `ColumnImpl.toDateDouble(Object value, dtc)` for an epoch millisecond
/// (`java.util.Date`/`Calendar`/`Number`) value: access stores dates in the
/// local timezone, so the instant is shifted by the zone offset before the pure
/// local conversion.  This is the `DateTimeType.DATE` write conversion.
pub fn to_date_double_from_millis(time: i64, tz: &TimeZone) -> f64 {
    to_local_date_double(time + get_to_local_time_zone_offset(time, tz))
}

/// `ColumnImpl.DefaultDateTimeFactory.toDateDouble(Object value, dtc)` for a
/// `java.time.LocalDateTime` (a `TemporalAccessor`) value.
///
/// Java converts the wall clock time to an instant in the database `ZoneId`
/// (`toInstant` -> `localDateTime.atZone(zoneId).toInstant()`), then applies the
/// `TimeZone` offset of *that* instant, and finally the pure local conversion.
/// With a fixed offset zone the two offsets cancel and the stored value is the
/// plain wall clock double; with a named zone they can disagree around a DST
/// transition and before 1900 (where `TimeZone` reports the raw offset), which
/// is why the conversion is not just [`to_date_double`].
pub fn to_date_double_from_local_datetime(ldt: LocalDateTime, tz: &TimeZone) -> f64 {
    let time = tz.local_to_epoch_millis(ldt);
    to_local_date_double(time + get_to_local_time_zone_offset(time, tz))
}

/// `ColumnImpl.toLocalDateDouble(long time)`
pub fn to_local_date_double(mut time: i64) -> f64 {
    time += MILLIS_BETWEEN_EPOCH_AND_1900;
    if time < 0 {
        let time_part = -time % MILLISECONDS_PER_DAY;
        if time_part > 0 {
            time -= 2 * (MILLISECONDS_PER_DAY - time_part);
        }
    }
    time as f64 / MILLISECONDS_PER_DAY as f64
}

/// `ColumnImpl.toDateDouble(LocalDateTime ldt)`
pub fn to_date_double(ldt: LocalDateTime) -> f64 {
    // Duration.between(BASE_LDT, ldt) in seconds + nanos
    let base = LocalDateTime::of_date(1899, 12, 30);
    let secs = ldt.to_epoch_day() * SECONDS_PER_DAY + ldt.second_of_day()
        - (base.to_epoch_day() * SECONDS_PER_DAY + base.second_of_day());
    let nanos = ldt.nano as i64;

    let mut time_seconds = secs % SECONDS_PER_DAY;
    if time_seconds < 0 {
        time_seconds += SECONDS_PER_DAY;
    }
    let date_seconds = secs - time_seconds;

    let time_double =
        ((nanos as f64 / NANOS_PER_SECOND as f64) + time_seconds as f64) / SECONDS_PER_DAY as f64;
    let mut date_double = date_seconds as f64 / SECONDS_PER_DAY as f64;
    let mut time_double = time_double;
    if date_seconds < 0 {
        time_double = -time_double;
    }
    date_double += time_double;
    date_double
}

/// `ColumnImpl.durationFromLocalDateDouble` - the inverse of
/// `toLocalDateDouble(Duration)`, choosing the coarsest rounding which still
/// converts back to the original double.  Returns seconds since BASE_LDT plus
/// nanos.
pub fn duration_from_local_date_double(value: f64) -> (i64, i64) {
    let date_seconds = (value as i64) * SECONDS_PER_DAY;
    let seconds_double = (value.abs() % 1.0) * SECONDS_PER_DAY as f64;
    let time_seconds = seconds_double as i64;
    let time_nanos = (seconds_double % 1.0 * NANOS_PER_SECOND as f64).round() as i64;

    let mut result_nanos = time_nanos;
    for unit in TIME_ROUNDING_NANOS {
        let rounded = ((time_nanos as f64 / unit as f64).round() as i64) * unit;
        let secs = date_seconds + time_seconds;
        if to_local_date_double_from_seconds(secs, rounded).to_bits() == value.to_bits() {
            result_nanos = rounded;
            break;
        }
    }
    // `Duration.ofSeconds` carries a whole second of nanoseconds into the
    // seconds field, which the fractional-second rounding above can produce
    let mut secs = date_seconds + time_seconds;
    if result_nanos >= NANOS_PER_SECOND {
        secs += result_nanos / NANOS_PER_SECOND;
        result_nanos %= NANOS_PER_SECOND;
    }
    (secs, result_nanos)
}

fn to_local_date_double_from_seconds(date_time_seconds: i64, time_nanos: i64) -> f64 {
    let mut time_seconds = date_time_seconds % SECONDS_PER_DAY;
    if time_seconds < 0 {
        time_seconds += SECONDS_PER_DAY;
    }
    let date_seconds = date_time_seconds - time_seconds;
    let mut time_double = ((time_nanos as f64 / NANOS_PER_SECOND as f64) + time_seconds as f64)
        / SECONDS_PER_DAY as f64;
    if date_seconds < 0 {
        time_double = -time_double;
    }
    date_seconds as f64 / SECONDS_PER_DAY as f64 + time_double
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_offset_time_zones_have_no_dst() {
        let utc = TimeZone::utc();
        assert_eq!(utc.get_raw_offset(), 0);
        assert_eq!(utc.offset_minutes(), 0);
        let neg = TimeZone::from_offset_minutes(-300);
        assert_eq!(neg.get_raw_offset(), -5 * 3_600_000);
        assert_eq!(neg.offset_minutes(), -300);
        // both a winter and a summer instant keep the same offset
        assert_eq!(neg.get_offset(1_577_836_800_000), -5 * 3_600_000);
        assert_eq!(neg.get_offset(1_595_980_800_000), -5 * 3_600_000);
        assert_eq!(
            TimeZone::from_raw_offset_ms(90 * 60_000).offset_minutes(),
            90
        );
    }

    #[test]
    fn local_date_time_from_millis_uses_the_offset() {
        let instant = 1_584_246_645_000i64; // 2020-03-15T10:30:45Z
        let utc = TimeZone::utc();
        assert_eq!(
            utc.local_date_time_from_millis(instant),
            LocalDateTime::from_epoch_millis(instant)
        );
        let neg = TimeZone::from_offset_minutes(-300);
        assert_eq!(
            neg.local_date_time_from_millis(instant),
            LocalDateTime::from_epoch_millis(instant - 5 * 3_600_000)
        );
        // it is the exact inverse of reading the same OLE double back
        assert_eq!(
            from_date_double(
                to_date_double(neg.local_date_time_from_millis(instant)),
                &neg
            ),
            instant
        );
    }

    /// `ColumnImpl.getToLocalTimeZoneOffset` / `getFromLocalTimeZoneOffset` and
    /// `toDateDouble(Object, dtc)`: the `DateTimeType.DATE` write conversion
    /// stores the instant shifted into the local zone, and the read conversion
    /// is its inverse at millisecond precision.
    #[test]
    fn date_millis_conversion_round_trips() {
        for tz in [
            TimeZone::utc(),
            TimeZone::from_offset_minutes(480),
            TimeZone::from_offset_minutes(-300),
        ] {
            // a fixed offset zone has the same offset for every instant, so
            // both helpers agree with it
            for time in [0i64, -1, 1_584_246_645_000, -12_219_292_800_000] {
                assert_eq!(
                    get_to_local_time_zone_offset(time, &tz),
                    tz.get_raw_offset()
                );
                assert_eq!(
                    get_from_local_time_zone_offset(time, &tz),
                    tz.get_raw_offset()
                );
                let double = to_date_double_from_millis(time, &tz);
                assert_eq!(
                    from_date_double(double, &tz),
                    time,
                    "round trip of {time} in {:?}",
                    tz
                );
            }
            // a whole millisecond is exactly representable in the OLE double,
            // so the conversion is lossless
            assert_eq!(
                from_date_double(to_date_double_from_millis(1_584_246_645_123, &tz), &tz),
                1_584_246_645_123
            );
        }
    }

    // -- named IANA zones ---------------------------------------------------

    fn zone(id: &str) -> TimeZone {
        TimeZone::from_zone_id(id).unwrap_or_else(|| panic!("unknown zone {id}"))
    }

    /// `TimeZone.getTimeZone(zoneId)` vs `ZoneId.getRules()`: the port keeps
    /// both faces of the zone.  A named zone's `TimeZone` offset is the raw
    /// offset before the JDK's 1900 history cut-off and the historical offset
    /// afterwards, while the `ZoneId` offset keeps the LMT offset there.
    #[test]
    fn named_zone_offsets_match_the_two_java_views() {
        // New York: LMT -4:56:02 before 1883, EST/EDT afterwards
        let ny = zone("America/New_York");
        assert!(ny.is_named());
        assert_eq!(ny.zone_id(), "America/New_York");
        assert_eq!(ny.get_raw_offset(), -5 * 3_600_000);
        // an 1850 instant is before the TimeZone table but has a ZoneId offset
        let old = -3_786_912_000_000i64; // 1850-01-01T00:00:00Z
        assert_eq!(ny.get_offset(old), -5 * 3_600_000);
        assert_eq!(ny.zone_offset(old), -17_762_000);
        // 1900-01-01T00:00:00Z is after the cut-off: both agree
        assert_eq!(
            ny.get_offset(TIME_ZONE_HISTORY_START_MILLIS),
            -5 * 3_600_000
        );
        assert_eq!(
            ny.get_offset(TIME_ZONE_HISTORY_START_MILLIS - 1),
            -5 * 3_600_000
        );
        // winter and summer instants follow the DST rule
        assert_eq!(ny.get_offset(1_577_836_800_000), -5 * 3_600_000); // 2020-01-01
        assert_eq!(ny.get_offset(1_595_980_800_000), -4 * 3_600_000); // 2020-07-29

        // London: the LMT offset is -0:01:15 and the modern rule is DST
        let london = zone("Europe/London");
        assert_eq!(london.get_raw_offset(), 0);
        assert_eq!(london.zone_offset(-62_135_596_800_000), -75_000);
        assert_eq!(london.get_offset(-62_135_596_800_000), 0);
        assert_eq!(london.get_offset(1_577_836_800_000), 0);
        assert_eq!(london.get_offset(1_595_980_800_000), 3_600_000);

        // Asia/Shanghai: +08:05:43 LMT, +08:00 raw, +09:00 DST in 1986-1991
        let shanghai = zone("Asia/Shanghai");
        assert_eq!(shanghai.get_raw_offset(), 8 * 3_600_000);
        let lmt_instant = -2_500_000_000_000i64; // 1890-10-11
        assert_eq!(shanghai.get_offset(lmt_instant), 8 * 3_600_000);
        assert_eq!(shanghai.zone_offset(lmt_instant), 29_143_000);
        assert_eq!(shanghai.get_offset(1_584_246_645_000), 8 * 3_600_000);
        assert_eq!(shanghai.get_offset(646_000_000_000), 9 * 3_600_000); // 1990-06

        // UTC: the bundled database carries it as a named zone whose only
        // offset is zero, which behaves exactly like `ZoneId.of("UTC")`
        let utc = zone("UTC");
        assert_eq!(utc.zone_id(), "UTC");
        assert_eq!(utc.get_raw_offset(), 0);
        assert_eq!(utc.get_offset(-62_135_596_800_000), 0);
        assert_eq!(utc.zone_offset(-62_135_596_800_000), 0);
        // the fixed offset constructor is the zone `TimeZone.getTimeZone("UTC")`
        // gives for a zero offset, which reports the same id and offsets
        let fixed_utc = TimeZone::utc();
        assert!(!fixed_utc.is_named());
        assert_eq!(fixed_utc.zone_id(), "UTC");
        assert_eq!(fixed_utc.get_offset(-62_135_596_800_000), 0);
        assert_eq!(fixed_utc.zone_offset(-62_135_596_800_000), 0);

        // the fixed offset ids Java's `ZoneId.of` accepts
        assert_eq!(zone("GMT+05:30").get_raw_offset(), 330 * 60_000);
        assert_eq!(zone("GMT+05:30").zone_id(), "GMT+05:30");
        assert_eq!(zone("+05:30").get_raw_offset(), 330 * 60_000);
        assert_eq!(zone("GMT-05:00").get_raw_offset(), -5 * 3_600_000);
        assert_eq!(zone("GMT").get_raw_offset(), 0);
        assert_eq!(zone("Etc/GMT+5").get_raw_offset(), -5 * 3_600_000);
        assert!(TimeZone::from_zone_id("Nowhere/Special").is_none());
    }

    /// `LocalDateTime.atZone(zoneId)`: a local time in a DST gap moves forward
    /// by the gap length (equivalent to using the offset before the gap), and a
    /// local time repeated by a fall-back transition uses the earlier offset.
    #[test]
    fn local_to_epoch_millis_resolves_gaps_and_overlaps() {
        let ny = zone("America/New_York");
        // 2020-03-08T02:30 does not exist in New York (02:00 -> 03:00)
        let gap = LocalDateTime::new(2020, 3, 8, 2, 30, 0, 0);
        assert_eq!(ny.offset_for_local(gap), -5 * 3_600_000);
        assert_eq!(
            ny.local_to_epoch_millis(gap),
            LocalDateTime::new(2020, 3, 8, 7, 30, 0, 0).to_epoch_millis()
        );
        // 2020-11-01T01:30 happens twice; Java picks the earlier (EDT) offset
        let overlap = LocalDateTime::new(2020, 11, 1, 1, 30, 0, 0);
        assert_eq!(ny.offset_for_local(overlap), -4 * 3_600_000);
        assert_eq!(
            ny.local_to_epoch_millis(overlap),
            LocalDateTime::new(2020, 11, 1, 5, 30, 0, 0).to_epoch_millis()
        );
        // an unambiguous time and the LMT era
        assert_eq!(
            ny.local_to_epoch_millis(LocalDateTime::new(2020, 3, 15, 12, 30, 45, 0)),
            LocalDateTime::new(2020, 3, 15, 16, 30, 45, 0).to_epoch_millis()
        );
        assert_eq!(
            ny.offset_for_local(LocalDateTime::new(1582, 10, 15, 0, 0, 0, 0)),
            -17_762_000
        );
        // a sub-millisecond value is truncated like `Instant.toEpochMilli`
        assert_eq!(
            ny.local_to_epoch_millis(LocalDateTime::new(2020, 3, 15, 12, 30, 45, 123_456_789)),
            LocalDateTime::new(2020, 3, 15, 16, 30, 45, 123_000_000).to_epoch_millis()
        );

        // the London transitions (01:00 -> 02:00 and 02:00 -> 01:00)
        let london = zone("Europe/London");
        let london_gap = LocalDateTime::new(2020, 3, 29, 1, 30, 0, 0);
        assert_eq!(london.offset_for_local(london_gap), 0);
        assert_eq!(
            london.local_to_epoch_millis(london_gap),
            LocalDateTime::new(2020, 3, 29, 1, 30, 0, 0).to_epoch_millis()
        );
        let london_overlap = LocalDateTime::new(2020, 10, 25, 1, 30, 0, 0);
        assert_eq!(london.offset_for_local(london_overlap), 3_600_000);
    }

    /// The `DATE`-mode `LocalDateTime` write conversion
    /// (`ColumnImpl.DefaultDateTimeFactory.toDateDouble`): the wall clock time
    /// becomes an instant in the `ZoneId`, which the `TimeZone` offset of that
    /// instant then shifts.  This is the plain wall clock double only when the
    /// two offsets agree.
    #[test]
    fn date_mode_local_date_time_conversion_uses_the_zone() {
        let ny = zone("America/New_York");
        let clean = LocalDateTime::new(2020, 3, 15, 12, 30, 45, 0);
        // EST at the instant, so the two offsets cancel
        assert_eq!(
            to_date_double_from_local_datetime(clean, &ny).to_bits(),
            to_date_double(clean).to_bits()
        );
        // the gap moves the value to 03:30, exactly like Java's
        // `ldt.atZone(zone).toInstant()` + `TimeZone.getOffset`
        let gap = LocalDateTime::new(2020, 3, 8, 2, 30, 0, 0);
        let expected = LocalDateTime::new(2020, 3, 8, 3, 30, 0, 0);
        assert_eq!(
            to_date_double_from_local_datetime(gap, &ny).to_bits(),
            to_date_double(expected).to_bits()
        );
        // sub-millisecond values are truncated to the millisecond
        let sub_ms = LocalDateTime::new(2020, 3, 15, 12, 30, 45, 123_456_789);
        assert_eq!(
            to_date_double_from_local_datetime(sub_ms, &ny).to_bits(),
            to_date_double(LocalDateTime::new(2020, 3, 15, 12, 30, 45, 123_000_000)).to_bits()
        );
        // before 1900 the `TimeZone` offset is the raw one, so an LMT local
        // time is stored at `wall clock - (LMT - raw)`
        let historic = LocalDateTime::new(1582, 10, 15, 0, 0, 0, 0);
        let stored = to_date_double_from_local_datetime(historic, &ny);
        let instant = ny.local_to_epoch_millis(historic);
        // the wall clock value Java stores is `instant + TimeZone offset`, which
        // is 3:58 before the LMT wall clock
        assert_eq!(instant + ny.get_offset(instant), instant - 18_000_000);
        assert_ne!(stored.to_bits(), to_date_double(historic).to_bits());
        // the DATE read of the stored double is the original instant
        assert_eq!(from_date_double(stored, &ny), instant);
        // a fixed offset zone keeps the plain wall clock value
        let fixed = TimeZone::from_offset_minutes(480);
        assert_eq!(
            to_date_double_from_local_datetime(gap, &fixed).to_bits(),
            to_date_double(gap).to_bits()
        );
    }

    /// `LocalDateTime.atZone` for the zones with the less common rules, with
    /// the epoch milliseconds Java reports for the same wall clock times
    /// (`LocalDateTime.atZone(...).toInstant().toEpochMilli()`):
    ///
    /// * Europe/Berlin: the ordinary one-hour central-European transitions;
    /// * Australia/Lord_Howe: the 30 minute DST shift;
    /// * Pacific/Apia: the 2011 date-line change, which skips the whole of
    ///   2011-12-30, and the +14/+13 DST overlap of 2012.
    #[test]
    fn at_zone_resolution_for_uncommon_zone_rules() {
        // Berlin 2020-03-29T02:00 -> 03:00 (gap) and 2020-10-25T03:00 -> 02:00
        let berlin = zone("Europe/Berlin");
        assert_eq!(
            berlin.local_to_epoch_millis(LocalDateTime::new(2020, 3, 29, 2, 30, 0, 0)),
            1_585_445_400_000
        );
        assert_eq!(
            berlin.local_to_epoch_millis(LocalDateTime::new(2020, 10, 25, 2, 30, 0, 0)),
            1_603_585_800_000
        );

        // Lord Howe 2020-10-04T02:00 -> 02:30: the gap is only 30 minutes, so
        // the local time 02:15 moves forward to 02:45 (+11:00)
        let lord_howe = zone("Australia/Lord_Howe");
        assert_eq!(
            lord_howe.offset_for_local(LocalDateTime::new(2020, 10, 4, 2, 15, 0, 0)),
            37_800_000
        );
        assert_eq!(
            lord_howe.local_to_epoch_millis(LocalDateTime::new(2020, 10, 4, 2, 15, 0, 0)),
            1_601_739_900_000
        );
        // 2021-04-04T02:00 (+11:00) -> 01:30 (+10:30): 01:45 happens twice and
        // the earlier (+11:00) offset wins
        assert_eq!(
            lord_howe.offset_for_local(LocalDateTime::new(2021, 4, 4, 1, 45, 0, 0)),
            39_600_000
        );
        assert_eq!(
            lord_howe.local_to_epoch_millis(LocalDateTime::new(2021, 4, 4, 1, 45, 0, 0)),
            1_617_461_100_000
        );
        assert_eq!(lord_howe.get_offset(1_601_739_900_000), 39_600_000); // +11
        assert_eq!(lord_howe.get_offset(1_617_461_100_000), 39_600_000); // +11

        // Apia skipped 2011-12-30 entirely: the gap is a full day, so the local
        // noon of that day moves forward to noon of the 31st (+14:00)
        let apia = zone("Pacific/Apia");
        assert_eq!(
            apia.offset_for_local(LocalDateTime::new(2011, 12, 30, 12, 0, 0, 0)),
            -36_000_000
        );
        assert_eq!(
            apia.local_to_epoch_millis(LocalDateTime::new(2011, 12, 30, 12, 0, 0, 0)),
            1_325_282_400_000
        );
        assert_eq!(
            apia.local_date_time_from_millis(1_325_282_400_000),
            LocalDateTime::new(2011, 12, 31, 12, 0, 0, 0)
        );
        // the 2012-04-01T04:00 (+14) -> 03:00 (+13) overlap repeats 03:00-04:00
        assert_eq!(
            apia.offset_for_local(LocalDateTime::new(2012, 4, 1, 3, 30, 0, 0)),
            50_400_000
        );
        assert_eq!(
            apia.local_to_epoch_millis(LocalDateTime::new(2012, 4, 1, 3, 30, 0, 0)),
            1_333_200_600_000
        );
        // the date-line code change itself: -10:00 (DST) to +14:00
        let change = 1_325_239_200_000i64;
        assert_eq!(apia.get_offset(change - 1), -36_000_000);
        assert_eq!(apia.get_offset(change), 50_400_000);

        // Shanghai 1986-05-04T02:00 -> 03:00 and 1986-09-14T02:00 -> 01:00
        let shanghai = zone("Asia/Shanghai");
        assert_eq!(
            shanghai.local_to_epoch_millis(LocalDateTime::new(1986, 5, 4, 2, 30, 0, 0)),
            515_529_000_000
        );
        assert_eq!(
            shanghai.local_to_epoch_millis(LocalDateTime::new(1986, 9, 14, 1, 30, 0, 0)),
            527_013_000_000
        );
    }

    /// The historical LMT era: `ZoneId` keeps the local mean time offset
    /// before a zone's first transition (Java's `LocalDateTime.atZone` and
    /// `ofInstant` use it for any instant), while the `TimeZone` face of the
    /// same zone reports the raw offset before the JDK's 1900 cut-off.  The
    /// values are the ones Java produces for the same wall clock times.
    #[test]
    fn historical_lmt_offsets_and_the_1901_boundary() {
        // 1874-12-20T00:00:00Z and 1890-06-01T00:00 local: before every
        // transition of these zones
        let cases: [(&str, i64, i64, i64); 5] = [
            // id, wall clock instant (ms), ZoneId offset (ms), raw offset (ms)
            (
                "America/New_York",
                -2_998_944_000_000,
                -17_762_000,
                -18_000_000,
            ),
            ("Europe/Berlin", -2_998_944_000_000, 3_208_000, 3_600_000),
            ("Asia/Shanghai", -2_998_944_000_000, 29_143_000, 28_800_000),
            (
                "Australia/Lord_Howe",
                -2_998_944_000_000,
                38_180_000,
                37_800_000,
            ),
            // Samoa was east of the date line before its 1892 move
            ("Pacific/Apia", -2_998_944_000_000, 45_184_000, 46_800_000),
        ];
        for (id, instant, zone_offset, raw_offset) in cases {
            let tz = zone(id);
            assert_eq!(tz.zone_offset(instant), zone_offset, "{id} LMT offset");
            assert_eq!(
                tz.get_offset(instant),
                raw_offset,
                "{id} TimeZone history cut-off"
            );
            assert_eq!(tz.get_raw_offset(), raw_offset, "{id} raw offset");
            // the same offsets select the wall clock time of the instant
            assert_eq!(
                tz.local_date_time_from_millis(instant),
                LocalDateTime::from_epoch_millis(instant + zone_offset),
                "{id} local time"
            );
        }
        // Apia kept the +12:33:04 LMT until its 1892 move across the date
        // line, after which the LMT is -11:26:56
        let apia = zone("Pacific/Apia");
        assert_eq!(apia.zone_offset(-2_500_000_000_000), 45_184_000); // 1890
        assert_eq!(apia.zone_offset(-2_400_000_000_000), -41_216_000); // 1893

        // Shanghai switched from +08:05:43 LMT to +08:00 at
        // 1900-12-31T15:54:17Z (the instant just after the TimeZone cut-off)
        let shanghai = zone("Asia/Shanghai");
        let change = -2_177_481_943_000i64;
        assert_eq!(shanghai.zone_offset(change - 1), 29_143_000);
        assert_eq!(shanghai.zone_offset(change), 28_800_000);
        // before 1900 the TimeZone face reports the raw offset; from 1900 on it
        // follows the table, which is still the LMT offset just before the
        // transition and the standard offset from the transition instant
        assert_eq!(shanghai.get_offset(change - 1), 29_143_000);
        assert_eq!(shanghai.get_offset(-2_500_000_000_000), 28_800_000); // 1890
        assert_eq!(shanghai.get_offset(change), 28_800_000);
        // the cut-off instant itself already follows the table
        assert_eq!(
            shanghai.get_offset(TIME_ZONE_HISTORY_START_MILLIS),
            29_143_000
        );
        // the overlap of the transition repeats 1900-12-31T23:54:17-24:00;
        // LocalDateTime.atZone picks the earlier (+08:05:43) offset there
        assert_eq!(
            shanghai.local_to_epoch_millis(LocalDateTime::new(1900, 12, 31, 23, 59, 59, 0)),
            -2_177_481_944_000
        );
        // 1901-01-01T00:00 itself is only valid with the new +08:00 offset
        assert_eq!(
            shanghai.local_to_epoch_millis(LocalDateTime::new(1901, 1, 1, 0, 0, 0, 0)),
            -2_177_481_600_000
        );
        // and a pre-1901 LMT wall clock time round trips through the zone
        let lmt_wall = LocalDateTime::new(1890, 6, 1, 0, 0, 0, 0);
        let lmt_instant = shanghai.local_to_epoch_millis(lmt_wall);
        assert_eq!(lmt_instant, -2_511_504_343_000);
        assert_eq!(shanghai.local_date_time_from_millis(lmt_instant), lmt_wall);
    }

    /// The 1901 / 1970 / 2038 boundaries of the epoch millisecond range and the
    /// 32-bit `time_t` edge (2038-01-19T03:14:07Z / :08Z) convert through the
    /// named zones in both directions.
    #[test]
    fn epoch_boundaries_convert_in_named_zones() {
        let boundaries = [
            -2_177_452_800_000i64, // 1901-01-01T00:00:00Z
            0,                     // 1970-01-01T00:00:00Z
            -1,
            1,
            2_147_483_647_000,   // 2038-01-19T03:14:07Z
            2_147_483_648_000,   // 2038-01-19T03:14:08Z
            253_402_300_799_999, // 9999-12-31T23:59:59.999Z
        ];
        for id in [
            "UTC",
            "GMT-05:00",
            "America/New_York",
            "Europe/Berlin",
            "Asia/Shanghai",
            "Australia/Lord_Howe",
            "Pacific/Apia",
        ] {
            let tz = zone(id);
            for ms in boundaries {
                let ldt = tz.local_date_time_from_millis(ms);
                assert_eq!(
                    tz.local_to_epoch_millis(ldt),
                    ms,
                    "{id} instant {ms} round trip via {}",
                    ldt.year
                );
                // and the DATE-mode OLE double round trips the same instant
                let double = to_date_double_from_millis(ms, &tz);
                assert_eq!(from_date_double(double, &tz), ms, "{id} DATE {ms}");
            }
        }
        // Exactly at the 1900 `TimeZone` history cut-off the `DATE` conversion
        // is not invertible for a zone whose 1900 table offset differs from its
        // raw offset: the write uses the table offset (+10:00 for Lord Howe)
        // while the read's first approximation lands before the cut-off, where
        // Java reports the raw offset (+10:30).  Java produces the same
        // half-hour shift, so the port matches rather than "fixing" it.
        let lord_howe = zone("Australia/Lord_Howe");
        let cut_off = -2_208_988_800_000i64;
        assert_eq!(lord_howe.get_offset(cut_off), 36_000_000);
        assert_eq!(
            from_date_double(to_date_double_from_millis(cut_off, &lord_howe), &lord_howe),
            -2_208_990_600_000
        );
        // a millisecond before the cut-off still round trips (raw offset on
        // both sides)
        assert_eq!(
            from_date_double(
                to_date_double_from_millis(cut_off - 1, &lord_howe),
                &lord_howe
            ),
            cut_off - 1
        );

        // the cut-off instant itself round trips in the zones whose 1900 table
        // offset equals the offset the read's approximation lands on
        for id in ["UTC", "GMT-05:00", "America/New_York", "Asia/Shanghai"] {
            let tz = zone(id);
            let cut_off = -2_208_988_800_000i64;
            assert_eq!(
                tz.local_to_epoch_millis(tz.local_date_time_from_millis(cut_off)),
                cut_off
            );
            assert_eq!(
                from_date_double(to_date_double_from_millis(cut_off, &tz), &tz),
                cut_off,
                "{id} DATE cut-off"
            );
        }

        // a local time at the 32-bit epoch boundary in New York
        let ny = zone("America/New_York");
        assert_eq!(
            ny.local_to_epoch_millis(LocalDateTime::new(2038, 1, 19, 3, 14, 8, 0)),
            2_147_501_648_000
        );
        // 2038-01-19T03:14:08Z is still EST
        assert_eq!(ny.get_offset(2_147_483_648_000), -5 * 3_600_000);
    }

    /// The `DATE` read (`ColumnImpl.fromDateDouble`) round trips a whole
    /// millisecond instant in a named zone, including the LMT era.
    #[test]
    fn date_millis_round_trips_in_a_named_zone() {
        for id in ["UTC", "Asia/Shanghai", "America/New_York", "Europe/London"] {
            let tz = zone(id);
            for millis in [
                0i64,
                -1,
                1_584_246_645_000,
                1_584_246_645_123,
                -2_208_988_800_001,
                -12_219_292_800_000,
                253_402_300_799_999,
            ] {
                let double = to_date_double_from_millis(millis, &tz);
                assert_eq!(
                    from_date_double(double, &tz),
                    millis,
                    "round trip of {millis} in {id}"
                );
            }
        }
    }
}
