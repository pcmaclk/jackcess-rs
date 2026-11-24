//! Port of the Jackcess text index collation subsystem: `GeneralLegacyIndexCodes`
//! (the `GENERAL_LEGACY` family), `GeneralIndexCodes` (the `GENERAL` family) and
//! `General97IndexCodes` (the `GENERAL_97` family), plus the parts of
//! `IndexCodes` (entry flags, collation selection) and `TailoredIndexCodes`
//! (the weight-table skeleton every collation shares) which they need.
//!
//! Every collation is a weight table read from a pair of resource files (one
//! line per UTF-16 code unit) plus the encoder which turns a text value into an
//! index entry.  The encoder writes:
//!
//! * the "inline" weights of the characters,
//! * the end-of-text flag,
//! * the "extra" (diacritic) codes, the "unprintable" codes and the "crazy"
//!   (kana) codes,
//! * the trailing flags,
//! * and, for a descending index, the whole entry is bit-flipped.
//!
//! The entry itself is preceded by the start-entry flag which
//! `IndexData.ColumnDescriptor#writeValue` writes before calling
//! `writeNonNullIndexTextValue`, so that a single function produces the bytes
//! of a complete index key.
//!
//! # Embedded resources
//!
//! The weight tables are embedded at compile time from `resources/` (copied
//! verbatim from the jackcess jar, package `com.healthmarketscience.jackcess`):
//!
//! * `index_codes_genleg.txt` / `index_codes_ext_genleg.txt` -- the general
//!   legacy table (one byte wide entries, mixed widths),
//! * `index_codes_gen.txt` / `index_codes_ext_gen.txt` /
//!   `index_codes_tert_gen.txt` -- the general table (two byte wide entries)
//!   and its tertiary weights,
//! * `index_codes_gen_97.txt` / `index_mappings_ext_gen_97.txt` -- the general
//!   97 table and the extended-char mappings which fold back into it.
//!
//! # Scope
//!
//! The three collations requested by [`write_index_text_value`] are ported
//! faithfully, including the kana / repeat-mark / surrogate / overrun handling.
//! Not ported, because they belong to other collations and are unreachable from
//! this entry point:
//!
//! * [`TailoredIndexCodes`] moves (`czech`, `hungarian`, ... ) and the
//!   contractions they can declare: `getContraction` is always empty here,
//!   which is exactly the behaviour of the untailored tables,
//! * `FrenchIndexCodes.isDiacriticsReversed()` (always `false` here),
//! * the tertiary weights of the general legacy / general 97 tables (both
//!   return `null` in Java as well; only the general table has a file).
//!
//! Sort orders which do not name one of the three supported collations fall
//! back to `GENERAL_LEGACY` (Java returns `null` for those, i.e. refuses to
//! write an index), see [`collation_for`].
//!
//! `charset` is accepted for signature compatibility only: the collation is
//! defined over Java `char`s (UTF-16 code units) and the weight tables are
//! independent of the column encoding, so the value is converted to UTF-16 code
//! units and encoded exactly as Java does.  A Rust `&str` cannot hold lone
//! surrogates, so a malformed UTF-16 value can only reach this code as its
//! replacement/decoded form; paired surrogates (astral characters) are handled
//! exactly as Java handles them.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use crate::error::{Error, Result};
use crate::format::{Charset, SortOrder};

// ---------------------------------------------------------------------------
// constants (GeneralLegacyIndexCodes)

/// `MAX_TEXT_INDEX_CHAR_LENGTH`: text index entries are limited to the number
/// of chars of a `VARCHAR` column (255 in Jet 4).
const MAX_TEXT_INDEX_CHAR_LENGTH: usize =
    crate::format::TEXT_FIELD_MAX_LENGTH as usize / crate::format::TEXT_FIELD_UNIT_SIZE as usize;

const END_TEXT: u8 = 0x01;
const END_EXTRA_TEXT: u8 = 0x00;

const UNPRINTABLE_COUNT_START: i32 = 7;
const UNPRINTABLE_COUNT_MULTIPLIER: i32 = 4;
const UNPRINTABLE_OFFSET_FLAGS: i32 = 0x8000;
const UNPRINTABLE_MIDFIX: u8 = 0x06;

/// a weight table whose entries are not all the same number of bytes wide
const VARIABLE_UNIT_WIDTH: u8 = 0;
/// what stands in for a character whose weight is shorter than a whole entry
const SHORT_WEIGHT_EXTRA_PLACEHOLDER: u8 = 0x00;

/// what a repeat mark writes when no weight has been written before it
const NO_REPEATED_WEIGHT: [u8; 2] = [0xFF, 0xFF];

/// what fills the gaps in the diacritic (extra) codes
const INTERNATIONAL_EXTRA_PLACEHOLDER: u8 = 0x02;

const CRAZY_CODE_START: u8 = 0x80;
const CRAZY_CODE_1: u8 = 0x02;
const CRAZY_CODE_2: u8 = 0x03;
const CRAZY_CODES_SUFFIX_START: u8 = 0xFF;
const CRAZY_CODES_SUFFIX_MIDFIX: u8 = 0x02;
const CRAZY_CODES_SUFFIX_SEPARATOR: u8 = 0xFF;
const CRAZY_CODES_SUFFIX_REPEAT: u8 = 0x80;
const CRAZY_CODES_PER_SUFFIX_REPEAT: i32 = 7;
const LONG_REPEATED_WEIGHT_PREFIX: u8 = 0x05;
const CJK_WEIGHT_LEAD: [u8; 2] = [0xFD, 0xFF];
const WEIGHT_ENTRIES_PER_CHAR: i32 = 3;
const TERTIARY_WEIGHTS_PER_CHAR: i32 = 3;
const DEFAULT_TERTIARY_WEIGHT: u8 = 0x01;
const CRAZY_COPY_CODE_CHAR: u8 = 0x01;
const CRAZY_COPY_CODE_REPEAT: u8 = 0x02;
const CRAZY_COPY_CODE_PROLONGED: u8 = 0x03;
const CRAZY_CODES_UNPRINT_SUFFIX: u8 = 0xFF;

/// the first byte of the two byte inline code of a kana
const KANA_CODE_PREFIX: u8 = 0x7F;

/// `IndexCodes.getStartEntryFlag`
const ASC_START_FLAG: u8 = 0x7F;
const DESC_START_FLAG: u8 = 0x80;

/// the inline code of the vowel a kana ends on, keyed by the second byte of the
/// kana's own inline code, with 0 where the mark copies instead
const KANA_VOWEL_CODES: [u8; 79] = [
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x03, 0x04, 0x05, 0x06, 0x00,
    0x00, 0x00, 0x02, 0x03, 0x04, 0x05, 0x06, 0x00, 0x00, 0x00, 0x02, 0x03, 0x04, 0x05, 0x06, 0x00,
    0x00, 0x00, 0x02, 0x03, 0x04, 0x05, 0x06, 0x00, 0x00, 0x00, 0x02, 0x03, 0x04, 0x05, 0x06, 0x00,
    0x00, 0x00, 0x02, 0x03, 0x04, 0x05, 0x06, 0x00, 0x00, 0x00, 0x02, 0x00, 0x04, 0x00, 0x06, 0x00,
    0x00, 0x00, 0x02, 0x03, 0x04, 0x05, 0x06, 0x00, 0x00, 0x00, 0x02, 0x03, 0x00, 0x05, 0x06,
];

// the embedded weight tables

const CODES_GENLEG: &str = include_str!("../resources/index_codes_genleg.txt");
const EXT_CODES_GENLEG: &str = include_str!("../resources/index_codes_ext_genleg.txt");
const CODES_GEN: &str = include_str!("../resources/index_codes_gen.txt");
const EXT_CODES_GEN: &str = include_str!("../resources/index_codes_ext_gen.txt");
const TERTIARY_WEIGHTS_GEN: &str = include_str!("../resources/index_codes_tert_gen.txt");
const CODES_GEN_97: &str = include_str!("../resources/index_codes_gen_97.txt");
const EXT_MAPPINGS_GEN_97: &str = include_str!("../resources/index_mappings_ext_gen_97.txt");

// the surrogate ranges of the general table (GeneralIndexCodes)

const HIGH_SURROGATE_PLANE2_START: u16 = 0xD840;
const HIGH_SURROGATE_UNWEIGHTED_START: u16 = 0xD880;
const HIGH_SURROGATE_HIGH_PLANES_START: u16 = 0xDB80;
const HIGH_SURROGATE_PLANE1_OFFSET: i32 = -10238;
const HIGH_SURROGATE_PLANE2_OFFSET: i32 = 9666;
const HIGH_SURROGATE_HIGH_PLANES_OFFSET: i32 = 9090;
const SURROGATE_EXTRA_BYTE: u8 = 0x3f;
const HIGH_SURROGATE_EXTRA_BYTE: u8 = 0x3e;

// the range of extended chars which the general 97 table maps back into the
// single byte chars (General97IndexCodes)

const FIRST_MAP_CHAR: u32 = 338;
const LAST_MAP_CHAR: u32 = 8482;
const EXT_CODES_BOUNDS_NIBBLE: u8 = 0x00;

// ---------------------------------------------------------------------------
// public entry point

/// Appends the encoded bytes of a text index key.
///
/// This is the whole index entry as `IndexData` writes it: the leading
/// start-entry flag (`0x7F` ascending, `0x80` descending) followed by
/// `GeneralLegacyIndexCodes.writeNonNullIndexTextValue(value, bout,
/// isAscending)` for the collation selected by `sort_order`, or
/// `General97IndexCodes.writeNonNullIndexTextValue` for `GENERAL_97`.
///
/// `charset` is unused (kept for signature compatibility); see the module docs.
pub fn write_index_text_value(
    value: &str,
    sort_order: &SortOrder,
    charset: Charset,
    out: &mut Vec<u8>,
    is_ascending: bool,
) -> Result<()> {
    let _ = charset;
    let coll = collation_for(sort_order);

    out.push(if is_ascending {
        ASC_START_FLAG
    } else {
        DESC_START_FLAG
    });

    let units = to_index_char_sequence(value);
    match coll.kind {
        Kind::Gen97 => write_general_97(coll, &units, out, is_ascending),
        Kind::Legacy | Kind::General => write_general(coll, &units, out, is_ascending),
    }
    Ok(())
}

/// `IndexCodes.getTextIndexCodes` restricted to the collations this module
/// implements.
///
/// `GENERAL_LEGACY` (1033/0/0), `GENERAL` (1033/0/1) and `GENERAL_97`
/// (1033/0/-1) are mapped to their weight tables.  Java returns `null` for
/// every other sort order (jackcess refuses to write an index in a collation it
/// has no measured data for); this port follows the requirement to fall back to
/// `GENERAL_LEGACY` instead, which is a documented deviation.
fn collation_for(sort_order: &SortOrder) -> &'static Collation {
    let so = *sort_order;
    if so.value == SortOrder::GENERAL_97.value && so.variant == 0 {
        match so.version {
            -1 => return gen97_collation(),
            0 => return legacy_collation(),
            1 => return general_collation(),
            _ => {}
        }
    }
    legacy_collation()
}

// ---------------------------------------------------------------------------
// weight table model

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Legacy,
    General,
    Gen97,
}

/// `GeneralLegacyIndexCodes.Type`
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Type {
    Simple,
    International,
    Unprintable,
    UnprintableExt,
    InternationalExt,
    Significant,
    Surrogate,
    Ignored,
    Repeat,
}

/// which computed handler a surrogate char takes (GeneralIndexCodes only)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SurrogateKind {
    Plane1,
    Plane2,
    HighPlanes,
    Low,
}

/// `GeneralLegacyIndexCodes.CharHandler`
#[derive(Clone, Debug)]
struct CharHandler {
    typ: Type,
    inline: Option<Box<[u8]>>,
    extra: Option<Box<[u8]>>,
    unprintable: Option<Box<[u8]>>,
    extra_modifier: u8,
    crazy_flag: u8,
    num_char_units: i32,
    num_primary_units: i32,
    extra_placeholder: u8,
    /// the prolonged sound mark, whose handler reports `Type::Repeat`
    prolonged: bool,
    surrogate: Option<SurrogateKind>,
}

impl CharHandler {
    fn base(typ: Type) -> Self {
        CharHandler {
            typ,
            inline: None,
            extra: None,
            unprintable: None,
            extra_modifier: 0,
            crazy_flag: 0,
            num_char_units: 1,
            num_primary_units: 1,
            extra_placeholder: INTERNATIONAL_EXTRA_PLACEHOLDER,
            prolonged: false,
            surrogate: None,
        }
    }

    fn simple(bytes: Option<Box<[u8]>>) -> Self {
        let mut h = Self::base(Type::Simple);
        h.inline = bytes;
        h
    }

    fn international(bytes: Option<Box<[u8]>>, extra: Option<Box<[u8]>>) -> Self {
        let mut h = Self::base(Type::International);
        h.inline = bytes;
        h.extra = extra;
        h
    }

    fn unprintable(bytes: Option<Box<[u8]>>) -> Self {
        let mut h = Self::base(Type::Unprintable);
        h.unprintable = bytes;
        h
    }

    fn unprintable_ext(modifier: u8) -> Self {
        let mut h = Self::base(Type::UnprintableExt);
        h.extra_modifier = modifier;
        h
    }

    fn international_ext(
        bytes: Option<Box<[u8]>>,
        extra: Option<Box<[u8]>>,
        crazy_flag: u8,
    ) -> Self {
        let mut h = Self::base(Type::InternationalExt);
        h.inline = bytes;
        h.extra = extra;
        h.crazy_flag = crazy_flag;
        h
    }

    fn significant(bytes: Option<Box<[u8]>>) -> Self {
        let mut h = Self::base(Type::Significant);
        h.inline = bytes;
        h
    }

    fn ignored() -> Self {
        Self::base(Type::Ignored)
    }

    fn repeat(extra: Option<Box<[u8]>>) -> Self {
        let mut h = Self::base(Type::Repeat);
        h.extra = extra;
        h
    }

    fn prolonged() -> Self {
        let mut h = Self::base(Type::Repeat);
        h.prolonged = true;
        h
    }

    fn surrogate(kind: SurrogateKind, extra_byte: u8) -> Self {
        let mut h = Self::base(Type::Surrogate);
        h.surrogate = Some(kind);
        h.extra = Some(Box::new([extra_byte]));
        h
    }

    /// the inline codes as Java's `getInlineBytes(char)` returns them.  A
    /// surrogate handler computes them from the char it is given, so the
    /// scratch buffer is supplied by the caller.
    fn inline_bytes<'a>(&'a self, c: u16, tmp: &'a mut [u8; 2]) -> Option<&'a [u8]> {
        if self.typ != Type::Surrogate {
            return self.inline.as_deref();
        }
        let kind = self.surrogate?;
        let idx = match kind {
            SurrogateKind::Plane1 => c as i32 + HIGH_SURROGATE_PLANE1_OFFSET,
            SurrogateKind::Plane2 => c as i32 + HIGH_SURROGATE_PLANE2_OFFSET,
            SurrogateKind::HighPlanes => c as i32 + HIGH_SURROGATE_HIGH_PLANES_OFFSET,
            SurrogateKind::Low => {
                let char_offset = (c as i32 - 0xdc00) % 1024;
                let idx_offset = if char_offset < 8 {
                    9992
                } else if char_offset < (8 + 254) {
                    9990
                } else if char_offset < (8 + 254 + 254) {
                    9988
                } else if char_offset < (8 + 254 + 254 + 254) {
                    9986
                } else {
                    9984
                };
                c as i32 - idx_offset
            }
        };
        // Java's toInlineBytes writes the low two bytes, big endian
        tmp[0] = ((idx >> 8) & 0xFF) as u8;
        tmp[1] = (idx & 0xFF) as u8;
        Some(&tmp[..])
    }

    /// the inline codes a handler holds on its own, which is what the character
    /// unit counting reads (a surrogate handler has none)
    fn static_inline(&self) -> Option<&[u8]> {
        if self.typ == Type::Surrogate {
            return None;
        }
        self.inline.as_deref()
    }
}

/// a weight table plus the metadata which belongs to the family it comes from
struct Collation {
    kind: Kind,
    /// the handler of every UTF-16 code unit, `0x0000 ..= 0xFFFF`
    handlers: Vec<CharHandler>,
    /// how many bytes a weight entry takes, or [`VARIABLE_UNIT_WIDTH`]
    primary_width: u8,
    /// the tertiary weight of each unit of the characters which do not take the
    /// default (the general table only)
    tertiary: HashMap<u16, Box<[u8]>>,
}

impl Collation {
    fn handler(&self, c: u16) -> &CharHandler {
        &self.handlers[c as usize]
    }

    /// `GeneralLegacyIndexCodes.getTertiaryWeights`
    fn tertiary_weights(&self, c: u16) -> Option<&[u8]> {
        self.tertiary.get(&c).map(|b| &b[..])
    }
}

fn legacy_collation() -> &'static Collation {
    static CELL: OnceLock<Collation> = OnceLock::new();
    CELL.get_or_init(|| build_legacy().expect("general legacy index codes resource is valid"))
}

fn general_collation() -> &'static Collation {
    static CELL: OnceLock<Collation> = OnceLock::new();
    CELL.get_or_init(|| build_general().expect("general index codes resource is valid"))
}

fn gen97_collation() -> &'static Collation {
    static CELL: OnceLock<Collation> = OnceLock::new();
    CELL.get_or_init(|| build_general_97().expect("general 97 index codes resource is valid"))
}

// ---------------------------------------------------------------------------
// loading

fn build_legacy() -> Result<Collation> {
    let codes = load_codes(
        CODES_GENLEG,
        0x0000,
        0x00FF,
        SurrogateSource::Ignored,
        None,
        VARIABLE_UNIT_WIDTH,
    )?;
    let ext = load_codes(
        EXT_CODES_GENLEG,
        0x0100,
        0xFFFF,
        SurrogateSource::Ignored,
        Some(&codes),
        VARIABLE_UNIT_WIDTH,
    )?;
    let mut handlers = codes;
    handlers.extend(ext);
    Ok(Collation {
        kind: Kind::Legacy,
        handlers,
        primary_width: VARIABLE_UNIT_WIDTH,
        tertiary: HashMap::new(),
    })
}

fn build_general() -> Result<Collation> {
    let codes = load_codes(
        CODES_GEN,
        0x0000,
        0x00FF,
        SurrogateSource::Ignored,
        None,
        GENERAL_PRIMARY_UNIT_WIDTH,
    )?;
    let ext = load_codes(
        EXT_CODES_GEN,
        0x0100,
        0xFFFF,
        SurrogateSource::General,
        Some(&codes),
        GENERAL_PRIMARY_UNIT_WIDTH,
    )?;
    let mut handlers = codes;
    handlers.extend(ext);
    Ok(Collation {
        kind: Kind::General,
        handlers,
        primary_width: GENERAL_PRIMARY_UNIT_WIDTH,
        tertiary: load_tertiary_weights(TERTIARY_WEIGHTS_GEN)?,
    })
}

fn build_general_97() -> Result<Collation> {
    let codes = load_codes(
        CODES_GEN_97,
        0x0000,
        0x00FF,
        SurrogateSource::Ignored,
        None,
        VARIABLE_UNIT_WIDTH,
    )?;
    let mappings = load_mappings(EXT_MAPPINGS_GEN_97)?;

    let mut handlers: Vec<CharHandler> = Vec::with_capacity(0x10000);
    handlers.extend(codes.iter().cloned());
    for c in 0x0100u32..=0xFFFF {
        let ch = if !(FIRST_MAP_CHAR..=LAST_MAP_CHAR).contains(&c) {
            CharHandler::ignored()
        } else {
            // 0 means "no mapping", not the char U+0000, which has a weight of
            // its own
            let mapped = mappings[(c - FIRST_MAP_CHAR) as usize];
            if mapped == 0 {
                CharHandler::ignored()
            } else {
                codes[mapped as usize].clone()
            }
        };
        handlers.push(ch);
    }
    Ok(Collation {
        kind: Kind::Gen97,
        handlers,
        primary_width: VARIABLE_UNIT_WIDTH,
        tertiary: HashMap::new(),
    })
}

/// `GeneralIndexCodes.PRIMARY_UNIT_WIDTH`
const GENERAL_PRIMARY_UNIT_WIDTH: u8 = 2;

/// where the surrogate handlers come from
#[derive(Clone, Copy)]
enum SurrogateSource {
    /// the general legacy / general 97 table gives a surrogate no weight
    Ignored,
    /// the general table computes one, except for the third run of high
    /// surrogates which is unweighted
    General,
}

fn is_surrogate(c: u16) -> bool {
    (0xD800..=0xDFFF).contains(&c)
}

fn gen_surrogate_handler(c: u16) -> CharHandler {
    if (0xDC00..=0xDFFF).contains(&c) {
        CharHandler::surrogate(SurrogateKind::Low, SURROGATE_EXTRA_BYTE)
    } else if c < HIGH_SURROGATE_PLANE2_START {
        CharHandler::surrogate(SurrogateKind::Plane1, SURROGATE_EXTRA_BYTE)
    } else if c < HIGH_SURROGATE_UNWEIGHTED_START {
        CharHandler::surrogate(SurrogateKind::Plane2, HIGH_SURROGATE_EXTRA_BYTE)
    } else if c < HIGH_SURROGATE_HIGH_PLANES_START {
        CharHandler::ignored()
    } else {
        CharHandler::surrogate(SurrogateKind::HighPlanes, HIGH_SURROGATE_EXTRA_BYTE)
    }
}

/// `GeneralLegacyIndexCodes.loadCodes`: one handler per char of the range, read
/// from the resource file with one line per non-surrogate char.  `rest` is the
/// part of the weight table a file which holds only a part of it does not have
/// on its own; its one byte codes take part in the entry counting.
fn load_codes(
    text: &str,
    first: u32,
    last: u32,
    surrogates: SurrogateSource,
    rest: Option<&[CharHandler]>,
    primary_width: u8,
) -> Result<Vec<CharHandler>> {
    let mut lines = text.split('\n');
    let mut values = Vec::with_capacity((last - first + 1) as usize);
    for i in first..=last {
        let c = i as u16;
        let ch = if is_surrogate(c) {
            match surrogates {
                SurrogateSource::Ignored => CharHandler::ignored(),
                SurrogateSource::General => gen_surrogate_handler(c),
            }
        } else {
            let line = lines
                .next()
                .ok_or_else(|| Error::IllegalState("Invalid codes file".into()))?;
            parse_codes_line(line.strip_suffix('\r').unwrap_or(line))?
        };
        values.push(ch);
    }
    count_char_units(&mut values, rest, primary_width);
    Ok(values)
}

/// `GeneralLegacyIndexCodes.parseCodes`
fn parse_codes_line(line: &str) -> Result<CharHandler> {
    if line.is_empty() {
        return Err(Error::IllegalState("Invalid codes file".into()));
    }
    let prefix = line.as_bytes()[0] as char;
    let suffix = if line.len() > 1 { &line[1..] } else { "" };
    let parts: Vec<&str> = suffix.split(',').collect();
    match prefix {
        'S' => {
            expect_parts(&parts, 1)?;
            Ok(CharHandler::simple(codes_to_bytes(parts[0], true)?))
        }
        'I' => {
            expect_parts(&parts, 2)?;
            Ok(CharHandler::international(
                codes_to_bytes(parts[0], true)?,
                codes_to_bytes(parts[1], true)?,
            ))
        }
        'U' => {
            expect_parts(&parts, 1)?;
            Ok(CharHandler::unprintable(codes_to_bytes(parts[0], true)?))
        }
        'P' => {
            expect_parts(&parts, 1)?;
            let bytes = codes_to_bytes(parts[0], true)?;
            let bytes = bytes.ok_or_else(|| unexpected(&parts))?;
            if bytes.len() != 1 {
                return Err(unexpected(&parts));
            }
            Ok(CharHandler::unprintable_ext(bytes[0]))
        }
        'Z' => {
            expect_parts(&parts, 3)?;
            let crazy_flag = if parts[2] == "1" {
                CRAZY_CODE_1
            } else {
                CRAZY_CODE_2
            };
            Ok(CharHandler::international_ext(
                codes_to_bytes(parts[0], true)?,
                codes_to_bytes(parts[1], false)?,
                crazy_flag,
            ))
        }
        'G' => {
            expect_parts(&parts, 1)?;
            Ok(CharHandler::significant(codes_to_bytes(parts[0], true)?))
        }
        'X' => Ok(CharHandler::ignored()),
        'R' => {
            expect_parts(&parts, 1)?;
            if parts[0].is_empty() {
                Ok(CharHandler::repeat(None))
            } else {
                Ok(CharHandler::repeat(codes_to_bytes(parts[0], true)?))
            }
        }
        'V' => {
            // the mark has no codes of its own, so a line which gives it any
            // fails the load
            if parts.len() != 1 || !parts[0].is_empty() {
                return Err(unexpected(&parts));
            }
            Ok(CharHandler::prolonged())
        }
        _ => Err(Error::IllegalState("Invalid codes file".into())),
    }
}

fn expect_parts(parts: &[&str], count: usize) -> Result<()> {
    if parts.len() != count {
        return Err(unexpected(parts));
    }
    Ok(())
}

fn unexpected(parts: &[&str]) -> Error {
    Error::IllegalState(format!("Unexpected code strings {:?}", parts))
}

fn hex_val(b: u8) -> Result<u8> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(Error::IllegalState("Invalid codes file".into())),
    }
}

/// `GeneralLegacyIndexCodes.codesToBytes`
fn codes_to_bytes(codes: &str, required: bool) -> Result<Option<Box<[u8]>>> {
    if codes.is_empty() {
        if required {
            return Err(Error::IllegalState("empty code bytes".into()));
        }
        return Ok(None);
    }
    let padded;
    let codes = if !codes.len().is_multiple_of(2) {
        // stripped a leading 0
        padded = format!("0{}", codes);
        padded.as_str()
    } else {
        codes
    };
    let raw = codes.as_bytes();
    let mut bytes = Vec::with_capacity(raw.len() / 2);
    for i in (0..raw.len()).step_by(2) {
        bytes.push((hex_val(raw[i])? << 4) | hex_val(raw[i + 1])?);
    }
    Ok(Some(bytes.into_boxed_slice()))
}

/// `GeneralLegacyIndexCodes.asUnsignedChar` (kept for parity with the Java
/// source; the port uses plain `u16`/`u32` arithmetic)
#[allow(dead_code)]
fn as_unsigned_char(c: u16) -> u32 {
    c as u32
}

// ---------------------------------------------------------------------------
// character unit counting

/// `GeneralLegacyIndexCodes.countCharUnits`
fn count_char_units(values: &mut [CharHandler], rest: Option<&[CharHandler]>, primary_width: u8) {
    let lead_bytes = if primary_width == VARIABLE_UNIT_WIDTH {
        let mut set: HashSet<u8> = HashSet::new();
        for ch in values.iter() {
            add_one_byte_code(&mut set, ch);
        }
        if let Some(rest) = rest {
            for ch in rest.iter() {
                add_one_byte_code(&mut set, ch);
            }
        }
        Some(set)
    } else {
        None
    };

    for ch in values.iter_mut() {
        count_handler_units(ch, lead_bytes.as_ref(), primary_width);
    }
}

/// `GeneralLegacyIndexCodes.addOneByteCode`
fn add_one_byte_code(lead_bytes: &mut HashSet<u8>, ch: &CharHandler) {
    if let Some(bytes) = ch.static_inline() {
        if bytes.len() == 1 {
            lead_bytes.insert(bytes[0]);
        }
    }
}

/// `GeneralLegacyIndexCodes.countCharUnits(CharHandler, Set, int)`
fn count_handler_units(ch: &mut CharHandler, lead_bytes: Option<&HashSet<u8>>, primary_width: u8) {
    let (num_entries, cjk_lead, byte_len) = match ch.static_inline() {
        None => return,
        Some(bytes) => (
            count_entries(bytes, lead_bytes, primary_width),
            has_cjk_lead(bytes),
            bytes.len(),
        ),
    };

    // a weight entry is one character, in both tables
    let mut num_units = num_entries;
    if cjk_lead {
        // a cjk ideograph is the lead entry and then its own, two entries which
        // stand for one character
        num_units = 1;
    }

    if let Some(extra_bytes) = ch.extra.as_deref() {
        // a character carries at most one diacritic weight for each character
        // it stands for
        num_units = num_units.max(extra_bytes.len() as i32);
    }

    ch.num_char_units = num_units;
    ch.num_primary_units = num_entries;
    if primary_width != VARIABLE_UNIT_WIDTH && (byte_len as i32) < primary_width as i32 {
        ch.extra_placeholder = SHORT_WEIGHT_EXTRA_PLACEHOLDER;
    }
}

/// `GeneralLegacyIndexCodes.countEntries`
fn count_entries(bytes: &[u8], lead_bytes: Option<&HashSet<u8>>, primary_width: u8) -> i32 {
    if primary_width != VARIABLE_UNIT_WIDTH {
        return (bytes.len() + primary_width as usize - 1) as i32 / primary_width as i32;
    }
    let lead_bytes = lead_bytes.expect("variable width counting needs the lead bytes");
    let mut num_entries = 0;
    let mut i = 0usize;
    while i < bytes.len() {
        num_entries += 1;
        i += if lead_bytes.contains(&bytes[i]) { 1 } else { 2 };
    }
    num_entries
}

/// `GeneralLegacyIndexCodes.hasCjkLead`
fn has_cjk_lead(bytes: &[u8]) -> bool {
    bytes.len() == CJK_WEIGHT_LEAD.len() * 2
        && bytes[0] == CJK_WEIGHT_LEAD[0]
        && bytes[1] == CJK_WEIGHT_LEAD[1]
}

/// `GeneralLegacyIndexCodes.loadTertiaryWeights`
fn load_tertiary_weights(text: &str) -> Result<HashMap<u16, Box<[u8]>>> {
    let mut weights: HashMap<u16, Box<[u8]>> = HashMap::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let sep = line
            .find(',')
            .ok_or_else(|| Error::IllegalState("Invalid tertiary weights file".into()))?;
        let c = u16::from_str_radix(&line[..sep], 16)
            .map_err(|_| Error::IllegalState("Invalid tertiary weights file".into()))?;
        let digits = &line[sep + 1..];
        let units: Vec<u8> = digits.bytes().map(|b| b - b'0').collect();
        weights.insert(c, units.into_boxed_slice());
    }
    Ok(weights)
}

/// `General97IndexCodes.loadMappings`
fn load_mappings(text: &str) -> Result<Vec<u16>> {
    let mut values = vec![0u16; (LAST_MAP_CHAR - FIRST_MAP_CHAR + 1) as usize];
    for line in text.split('\n') {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split(',');
        let from = parts
            .next()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .ok_or_else(|| Error::IllegalState("Invalid index mappings file".into()))?;
        let to = parts
            .next()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .ok_or_else(|| Error::IllegalState("Invalid index mappings file".into()))?;
        if !(FIRST_MAP_CHAR..=LAST_MAP_CHAR).contains(&from) {
            return Err(Error::IllegalState("Invalid index mappings file".into()));
        }
        values[(from - FIRST_MAP_CHAR) as usize] = to as u16;
    }
    Ok(values)
}

// ---------------------------------------------------------------------------
// byte streams (ByteUtil.ByteStream and the two subclasses it gains)

/// `ByteUtil.ByteStream`
#[derive(Default)]
struct Stream {
    bytes: Vec<u8>,
}

impl Stream {
    fn new() -> Self {
        Stream { bytes: Vec::new() }
    }

    fn with_capacity(capacity: usize) -> Self {
        Stream {
            bytes: Vec::with_capacity(capacity),
        }
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }

    fn get(&self, offset: usize) -> u8 {
        self.bytes[offset]
    }

    fn set(&mut self, offset: usize, b: u8) {
        self.bytes[offset] = b;
    }

    fn set_bits(&mut self, offset: usize, b: u8) {
        self.bytes[offset] |= b;
    }

    fn write(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    fn write_byte(&mut self, b: u8) {
        self.bytes.push(b);
    }

    fn write_fill(&mut self, length: usize, b: u8) {
        self.bytes.resize(self.bytes.len() + length, b);
    }

    fn get_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// `ByteStream.trimTrailing(minTrimCode, maxTrimCode, minLength)`
    fn trim_trailing(&mut self, min_trim_code: u8, max_trim_code: u8, min_length: usize) {
        let mut idx = self.bytes.len() as i32 - 1;
        while idx >= min_length as i32 {
            let val = self.bytes[idx as usize];
            if val >= min_trim_code && val <= max_trim_code {
                idx -= 1;
            } else {
                break;
            }
        }
        self.bytes.truncate((idx + 1) as usize);
    }
}

/// `GeneralLegacyIndexCodes.ExtraCodesStream`
struct ExtraCodes {
    stream: Stream,
    num_chars: i32,
}

impl ExtraCodes {
    fn new(length: usize) -> Self {
        ExtraCodes {
            stream: Stream::with_capacity(length),
            num_chars: 0,
        }
    }
}

/// `General97IndexCodes.NibbleStream`
struct NibbleStream {
    stream: Stream,
    nibble_len: usize,
}

impl NibbleStream {
    fn new(length: usize) -> Self {
        NibbleStream {
            stream: Stream::with_capacity(length),
            nibble_len: 0,
        }
    }

    fn next_is_hi(&self) -> bool {
        self.nibble_len.is_multiple_of(2)
    }

    fn write_low_nibble(&mut self, b: u8) {
        let byte_off = self.nibble_len / 2;
        self.stream.set_bits(byte_off, b & 0x0F);
    }

    fn write_nibble(&mut self, b: u8) {
        if self.next_is_hi() {
            self.stream.write_byte((b << 4) & 0xF0);
        } else {
            self.write_low_nibble(b);
        }
        self.nibble_len += 1;
    }

    fn write_fill_nibbles(&mut self, mut length: i32, b: u8) {
        let new_nibble_len = self.nibble_len as i32 + length;

        if !self.next_is_hi() {
            self.write_low_nibble(b);
            length -= 1;
        }

        if length > 1 {
            let double_b = ((b << 4) & 0xF0) | (b & 0x0F);
            loop {
                self.stream.write_byte(double_b);
                length -= 2;
                if length <= 1 {
                    break;
                }
            }
        }

        if length == 1 {
            self.stream.write_byte((b << 4) & 0xF0);
        }

        self.nibble_len = new_nibble_len as usize;
    }
}

// ---------------------------------------------------------------------------
// the encoder

/// `GeneralLegacyIndexCodes.toIndexCharSequence`, including the truncation to
/// `MAX_TEXT_INDEX_CHAR_LENGTH` and the removal of trailing spaces.
fn to_index_char_sequence(value: &str) -> Vec<u16> {
    let mut units: Vec<u16> = value.encode_utf16().collect();
    if units.len() > MAX_TEXT_INDEX_CHAR_LENGTH {
        units.truncate(MAX_TEXT_INDEX_CHAR_LENGTH);
    }
    let mut len = units.len();
    if len > 0 && units[len - 1] == b' ' as u16 {
        loop {
            len -= 1;
            if len == 0 || units[len - 1] != b' ' as u16 {
                break;
            }
        }
        units.truncate(len);
    }
    units
}

/// `GeneralLegacyIndexCodes.writeNonNullIndexTextValue`
fn write_general(coll: &Collation, str: &[u16], out: &mut Vec<u8>, is_ascending: bool) {
    let prev_length = out.len();

    let mut extra_codes: Option<ExtraCodes> = None;
    let mut unprintable_codes: Option<Stream> = None;
    let mut crazy_codes: Option<Stream> = None;
    let mut crazy_copy_codes: Option<Stream> = None;
    let mut crazy_units: Vec<i32> = Vec::new();
    let tertiary_room: i32 = TERTIARY_WEIGHTS_PER_CHAR * str.len() as i32;
    let mut overrun_weights: Option<Stream> = None;
    let mut char_offset: i32 = 0;
    let mut unprint_offset: i32 = 0;
    let mut last_weight: Option<Vec<u8>> = None;
    let mut last_crazy_flag: u8 = 0;
    let mut last_tertiary_char: u16 = 0;
    let mut has_last_tertiary_char = false;
    let mut padded_weights: Option<Stream> = None;
    let primary_width = coll.primary_width;

    for i in 0..str.len() {
        let c = str[i];

        // a collation can weigh a sequence of characters as a unit, and such a
        // sequence writes what one or more characters write.  the three
        // collations implemented here weigh no sequence as a unit
        let ch = coll.handler(c);

        let cur_char_offset = char_offset;
        let cur_unprint_offset = unprint_offset;

        let mut tmp = [0u8; 2];
        let mut bytes = ch.inline_bytes(c, &mut tmp);
        let mut num_char_units = ch.num_char_units;
        let mut num_primary_units = ch.num_primary_units;
        let mut repeated_nothing = false;
        // the bytes a repeat mark writes, when it copies a weight rather than
        // using its own inline codes
        let repeated_bytes: Vec<u8>;
        if ch.typ == Type::Repeat {
            // a repeat mark stands for the character before it, so it writes
            // that character's weight again, and a prolonged sound mark writes
            // the weight of the vowel instead
            repeated_nothing = last_weight.is_none();
            let last = last_weight.as_deref();
            repeated_bytes = if repeated_nothing {
                NO_REPEATED_WEIGHT.to_vec()
            } else if ch.prolonged {
                prolonged_sound_bytes(last.unwrap())
            } else {
                last.unwrap().to_vec()
            };
            bytes = Some(&repeated_bytes);
            num_char_units = if repeated_nothing { 0 } else { 1 };
            num_primary_units = 1;
        }

        if let Some(inline) = bytes {
            // write the "inline" codes immediately
            out.extend_from_slice(inline);

            char_offset += num_char_units;
            unprint_offset += num_primary_units;

            if char_offset > tertiary_room {
                let overrun = overrun_weights.get_or_insert_with(Stream::new);
                // a repeat mark takes the weight of the character it stands
                // for.  no handler of the supported collations weighs a
                // sequence as a unit, so a character always carries its own
                let tertiary: Option<&[u8]> = if ch.typ == Type::Repeat {
                    if has_last_tertiary_char {
                        coll.tertiary_weights(last_tertiary_char)
                    } else {
                        None
                    }
                } else {
                    coll.tertiary_weights(c)
                };
                let start = 0.max(tertiary_room - cur_char_offset);
                for t in start..num_char_units {
                    overrun.write_byte(tertiary_weight(tertiary, t as usize));
                }
            }

            let padded_len = num_primary_units * primary_width as i32;
            if padded_weights.is_none() && padded_len > inline.len() as i32 {
                // the first weight which does not fill its entries
                let mut pw = Stream::with_capacity(out.len() + padded_len as usize);
                pw.write(&out[prev_length..out.len() - inline.len()]);
                padded_weights = Some(pw);
            }
            if let Some(pw) = padded_weights.as_mut() {
                pw.write(inline);
                pw.write_fill(
                    (padded_len - inline.len() as i32).max(0) as usize,
                    SHORT_WEIGHT_EXTRA_PLACEHOLDER,
                );
            }

            // only a character weighed as one leaves a weight a repeat mark can
            // copy, which also leaves nothing after NO_REPEATED_WEIGHT.  a
            // prolonged sound mark leaves the weight of the kana it lengthens
            if !ch.prolonged {
                last_weight = if num_char_units == 1 {
                    Some(repeatable_weight(inline, num_primary_units))
                } else {
                    None
                };
            }
            if ch.typ != Type::Repeat {
                last_crazy_flag = if num_char_units == 1 {
                    ch.crazy_flag
                } else {
                    0
                };
                last_tertiary_char = c;
                has_last_tertiary_char = true;
            }
        }

        if ch.typ == Type::Simple && ch.extra_placeholder == INTERNATIONAL_EXTRA_PLACEHOLDER {
            // common case, skip further code handling
            continue;
        }

        if repeated_nothing {
            // a mark with no weight to repeat gives no secondary weight either
            continue;
        }

        let placeholder_bytes: Vec<u8>;
        let mut extra_bytes: Option<&[u8]> = ch.extra.as_deref();
        let extra_code_modifier = ch.extra_modifier;
        if extra_bytes.is_none()
            && extra_code_modifier == 0
            && ch.extra_placeholder != INTERNATIONAL_EXTRA_PLACEHOLDER
        {
            // the placeholder of this character is not the one the gaps are
            // filled with, so it is written where it stands
            placeholder_bytes = vec![ch.extra_placeholder; num_char_units as usize];
            extra_bytes = Some(&placeholder_bytes);
        }
        if extra_bytes.is_some() || extra_code_modifier != 0 {
            let ec = extra_codes.get_or_insert_with(|| ExtraCodes::new(str.len()));
            // keep track of the extra codes for later
            write_extra_codes(cur_char_offset, extra_bytes, extra_code_modifier, ec);
        }

        if let Some(unprintable) = ch.unprintable.as_deref() {
            let up = unprintable_codes.get_or_insert_with(Stream::new);
            // keep track of the unprintable codes for later
            write_unprintable_codes(cur_unprint_offset, unprintable, up);

            // a repeat mark after one of these copies it in the shape it takes
            // in the unprintable codes, the midfix and then the code
            let mut weight = Vec::with_capacity(1 + unprintable.len());
            weight.push(UNPRINTABLE_MIDFIX);
            weight.extend_from_slice(unprintable);
            last_weight = Some(weight);
            last_crazy_flag = 0;
            has_last_tertiary_char = false;
        }

        // a repeat mark takes the flag of the character it stands for, which
        // the inline block leaves alone and the unprintable one clears
        let crazy_flag = if ch.typ == Type::Repeat {
            last_crazy_flag
        } else {
            ch.crazy_flag
        };
        if crazy_flag != 0 {
            if crazy_codes.is_none() {
                crazy_codes = Some(Stream::new());
                crazy_copy_codes = Some(Stream::new());
                crazy_units = vec![0; str.len()];
            }

            // keep track of the crazy flags for later
            let num_crazy_codes = crazy_codes.as_ref().unwrap().len();
            if num_crazy_codes == crazy_units.len() {
                crazy_units.resize(num_crazy_codes * 2, 0);
            }
            crazy_units[num_crazy_codes] = cur_char_offset;
            crazy_codes.as_mut().unwrap().write_byte(crazy_flag);
            let copy_code = if ch.prolonged {
                CRAZY_COPY_CODE_PROLONGED
            } else if ch.typ == Type::Repeat {
                CRAZY_COPY_CODE_REPEAT
            } else {
                CRAZY_COPY_CODE_CHAR
            };
            crazy_copy_codes.as_mut().unwrap().write_byte(copy_code);
        }
    }

    // a crazy code which the tertiary overrun wrote is a weight rather than a
    // flag
    let mut num_overrun_codes = 0;
    if let Some(overrun) = overrun_weights.as_ref() {
        num_overrun_codes =
            write_tertiary_overrun(crazy_codes.as_mut(), &crazy_units, overrun, tertiary_room);
    }

    // write end text flag
    out.push(END_TEXT);

    extra_codes = write_weight_overrun(
        out,
        padded_weights.as_ref(),
        extra_codes,
        char_offset,
        unprint_offset,
        str.len(),
    );

    let has_extra_codes =
        trim_extra_codes(extra_codes.as_mut(), 0, INTERNATIONAL_EXTRA_PLACEHOLDER);
    let has_unprintable_codes = unprintable_codes.is_some();
    let has_crazy_codes = crazy_codes.is_some();
    if has_extra_codes || has_unprintable_codes || has_crazy_codes {
        // we write all the international extra bytes first
        if has_extra_codes {
            out.extend_from_slice(extra_codes.as_ref().unwrap().stream.get_bytes());
        }

        if has_crazy_codes || has_unprintable_codes {
            // write 2 more end flags
            out.push(END_TEXT);
            out.push(END_TEXT);

            // next come the crazy flags
            if has_crazy_codes {
                write_crazy_codes(
                    crazy_codes.as_mut().unwrap(),
                    crazy_copy_codes.as_mut().unwrap(),
                    num_overrun_codes,
                    out,
                );

                // if we are writing unprintable codes after this, tack on
                // another code
                if has_unprintable_codes {
                    out.push(CRAZY_CODES_UNPRINT_SUFFIX);
                }
            }

            // then we write all the unprintable extra bytes
            if has_unprintable_codes {
                // write another end flag
                out.push(END_TEXT);
                out.extend_from_slice(unprintable_codes.as_ref().unwrap().get_bytes());
            }
        }
    }

    // handle descending order by inverting the bytes
    if !is_ascending {
        // we actually write the end byte before flipping the bytes, and write
        // another one after flipping
        out.push(END_EXTRA_TEXT);
        flip_bytes(&mut out[prev_length..]);
    }

    // write end extra text
    out.push(END_EXTRA_TEXT);
}

/// `General97IndexCodes.writeNonNullIndexTextValue`
fn write_general_97(coll: &Collation, str: &[u16], out: &mut Vec<u8>, is_ascending: bool) {
    let prev_length = out.len();

    let mut extra_codes: Option<NibbleStream> = None;
    let mut sig_char_count: i32 = 0;
    for i in 0..str.len() {
        let c = str[i];
        let ch = coll.handler(c);

        let mut tmp = [0u8; 2];
        if let Some(bytes) = ch.inline_bytes(c, &mut tmp) {
            out.extend_from_slice(bytes);
        }

        if ch.typ == Type::Simple {
            // common case, skip further code handling
            continue;
        }

        if ch.typ == Type::Significant {
            // a character weighed as two, such as the sharp s for "ss", takes a
            // placeholder for each character it stands for.  significant chars
            // never have extra bytes
            sig_char_count += ch.num_char_units;
            continue;
        }

        if let Some(bytes) = ch.extra.as_deref() {
            let ns = extra_codes.get_or_insert_with(|| {
                let mut ns = NibbleStream::new(str.len());
                ns.write_nibble(EXT_CODES_BOUNDS_NIBBLE);
                ns
            });
            // keep track of the extra code for later
            write_extra_codes_97(sig_char_count, bytes, ns);
            sig_char_count = 0;
        }
    }

    if let Some(ns) = extra_codes.as_mut() {
        // write the extra codes to the end
        ns.write_nibble(EXT_CODES_BOUNDS_NIBBLE);
        out.extend_from_slice(ns.stream.get_bytes());
    } else {
        // write end extra text
        out.push(END_EXTRA_TEXT);
    }

    // handle descending order by inverting the bytes
    if !is_ascending {
        flip_bytes(&mut out[prev_length..]);
    }
}

/// `IndexData.flipBytes(value, offset, length)`
fn flip_bytes(bytes: &mut [u8]) {
    for b in bytes.iter_mut() {
        *b = !*b;
    }
}

/// `GeneralLegacyIndexCodes.repeatableWeight`
fn repeatable_weight(bytes: &[u8], num_primary_units: i32) -> Vec<u8> {
    if num_primary_units <= 1 {
        return bytes.to_vec();
    }
    vec![LONG_REPEATED_WEIGHT_PREFIX, bytes[bytes.len() - 2]]
}

/// `GeneralLegacyIndexCodes.prolongedSoundBytes`
fn prolonged_sound_bytes(last_weight: &[u8]) -> Vec<u8> {
    if last_weight.len() != 2 || last_weight[0] != KANA_CODE_PREFIX {
        return last_weight.to_vec();
    }
    let code = last_weight[1];
    let vowel = if (code as usize) < KANA_VOWEL_CODES.len() {
        KANA_VOWEL_CODES[code as usize]
    } else {
        0
    };
    if vowel == 0 {
        last_weight.to_vec()
    } else {
        vec![KANA_CODE_PREFIX, vowel]
    }
}

/// `GeneralLegacyIndexCodes.writeExtraCodes`
fn write_extra_codes(
    char_offset: i32,
    bytes: Option<&[u8]>,
    extra_code_modifier: u8,
    extra_codes: &mut ExtraCodes,
) {
    // we fill in a placeholder value for any chars w/out extra codes
    let num_chars = extra_codes.num_chars;
    if num_chars < char_offset {
        let fill_chars = char_offset - num_chars;
        extra_codes
            .stream
            .write_fill(fill_chars as usize, INTERNATIONAL_EXTRA_PLACEHOLDER);
        extra_codes.num_chars += fill_chars;
    }

    if let Some(bytes) = bytes {
        // write the actual extra codes and move the count on by what was
        // written
        extra_codes.stream.write(bytes);
        extra_codes.num_chars += bytes.len() as i32;
    } else {
        // extra code modifiers modify the existing extra code bytes and do not
        // count as additional extra code chars
        let last_idx = extra_codes.stream.len() as i32 - 1;
        if last_idx >= 0 {
            // the extra code modifier is added to the last extra code written
            let last_byte = extra_codes
                .stream
                .get(last_idx as usize)
                .wrapping_add(extra_code_modifier);
            extra_codes.stream.set(last_idx as usize, last_byte);
        } else {
            // there is no previous extra code, so the modifier is the code
            extra_codes.stream.write_byte(extra_code_modifier);
        }
    }
}

/// `General97IndexCodes.writeExtraCodes`
fn write_extra_codes_97(num_sig_chars: i32, bytes: &[u8], extra_codes: &mut NibbleStream) {
    // need to fill in placeholder nibbles for any "significant" chars
    if num_sig_chars > 0 {
        extra_codes.write_fill_nibbles(num_sig_chars, INTERNATIONAL_EXTRA_PLACEHOLDER);
    }

    // there should only ever be a single "extra" byte
    extra_codes.write_nibble(bytes[0]);
}

/// `GeneralLegacyIndexCodes.writeWeightOverrun`
fn write_weight_overrun(
    out: &[u8],
    padded_weights: Option<&Stream>,
    extra_codes: Option<ExtraCodes>,
    char_offset: i32,
    num_entries: i32,
    num_chars: usize,
) -> Option<ExtraCodes> {
    // the room is counted over the characters of the value as it is given
    let mut overrun_len = 2 * (num_entries - (WEIGHT_ENTRIES_PER_CHAR * num_chars as i32));
    if overrun_len <= 0 {
        return extra_codes;
    }

    let had_extra_codes = extra_codes.is_some();
    let mut codes = extra_codes.unwrap_or_else(|| ExtraCodes::new(num_chars));
    // the weights land on places which may carry no diacritic weight yet, so
    // the placeholders they stand on have to be written first
    let fill_chars = char_offset - codes.num_chars;
    if fill_chars > 0 {
        codes
            .stream
            .write_fill(fill_chars as usize, INTERNATIONAL_EXTRA_PLACEHOLDER);
        codes.num_chars += fill_chars;
    }

    overrun_len = overrun_len.min(codes.stream.len() as i32);
    if overrun_len <= 0 {
        return if had_extra_codes { Some(codes) } else { None };
    }

    // the END_TEXT byte is written by now, and the weights are what comes
    // before it
    let (weights, weights_len) = match padded_weights {
        Some(padded) => (padded.get_bytes(), padded.len()),
        None => (out, out.len() - 1),
    };
    overrun_len = overrun_len.min(weights_len as i32);
    let from = weights_len as i32 - overrun_len;
    for i in 0..overrun_len {
        let b = weights[(from + i) as usize];
        codes.stream.set(i as usize, b);
    }
    Some(codes)
}

/// `GeneralLegacyIndexCodes.trimExtraCodes`
fn trim_extra_codes(
    extra_codes: Option<&mut ExtraCodes>,
    min_trim_code: u8,
    max_trim_code: u8,
) -> bool {
    let Some(codes) = extra_codes else {
        return false;
    };
    codes.stream.trim_trailing(min_trim_code, max_trim_code, 0);
    codes.stream.len() > 0
}

/// `GeneralLegacyIndexCodes.tertiaryWeight`
fn tertiary_weight(weights: Option<&[u8]>, unit: usize) -> u8 {
    match weights {
        Some(weights) if unit < weights.len() => weights[unit],
        _ => DEFAULT_TERTIARY_WEIGHT,
    }
}

/// `GeneralLegacyIndexCodes.writeTertiaryOverrun`
fn write_tertiary_overrun(
    crazy_codes: Option<&mut Stream>,
    crazy_units: &[i32],
    overrun_weights: &Stream,
    tertiary_room: i32,
) -> i32 {
    let Some(crazy_codes) = crazy_codes else {
        return 0;
    };
    let num_codes = overrun_weights.len().min(crazy_codes.len());
    let mut num_written = 0;
    for (j, &unit) in crazy_units.iter().enumerate().take(num_codes) {
        if (tertiary_room + j as i32) >= unit {
            crazy_codes.set(j, overrun_weights.get(j));
            num_written = j as i32 + 1;
        }
    }
    num_written
}

/// `GeneralLegacyIndexCodes.writeCrazyCodes`
fn write_crazy_codes(
    crazy_codes: &mut Stream,
    crazy_copy_codes: &mut Stream,
    num_overrun_codes: i32,
    out: &mut Vec<u8>,
) {
    // the suffix length depends on how many chars took the crazy path, which is
    // the code count before any trimming
    let num_crazy_chars = crazy_codes.len();

    // CRAZY_CODE_2 flags at the end are ignored, so ditch them
    crazy_codes.trim_trailing(CRAZY_CODE_2, CRAZY_CODE_2, num_overrun_codes as usize);
    pack_crazy_codes(crazy_codes, out);

    // write crazy code suffix (note, we write this even if all the codes are
    // trimmed)
    write_crazy_codes_suffix(num_crazy_chars, crazy_copy_codes, out);
}

/// `GeneralLegacyIndexCodes.packCrazyCodes`
fn pack_crazy_codes(crazy_codes: &Stream, out: &mut Vec<u8>) {
    if crazy_codes.len() == 0 {
        return;
    }

    let mut cur_byte = CRAZY_CODE_START;
    let mut idx = 0usize;
    for i in 0..crazy_codes.len() {
        let next_byte = crazy_codes.get(i).wrapping_shl(((2 - idx) * 2) as u32);
        cur_byte |= next_byte;

        idx += 1;
        if idx == 3 {
            // write current byte and reset
            out.push(cur_byte);
            cur_byte = CRAZY_CODE_START;
            idx = 0;
        }
    }

    // write last byte
    if idx > 0 {
        out.push(cur_byte);
    }
}

/// `GeneralLegacyIndexCodes.writeCrazyCodesSuffix`
fn write_crazy_codes_suffix(
    num_crazy_chars: usize,
    crazy_copy_codes: &mut Stream,
    out: &mut Vec<u8>,
) {
    let num_repeats = num_crazy_chars.div_ceil(CRAZY_CODES_PER_SUFFIX_REPEAT as usize);

    out.push(CRAZY_CODES_SUFFIX_START);
    // the second run says which of the crazy codes a repeat mark wrote
    crazy_copy_codes.trim_trailing(CRAZY_COPY_CODE_CHAR, CRAZY_COPY_CODE_CHAR, 0);
    pack_crazy_codes(crazy_copy_codes, out);
    out.push(CRAZY_CODES_SUFFIX_MIDFIX);
    for _ in 0..num_repeats {
        out.push(CRAZY_CODES_SUFFIX_REPEAT);
    }
    out.push(CRAZY_CODES_SUFFIX_SEPARATOR);
    for _ in 0..num_repeats {
        out.push(CRAZY_CODES_SUFFIX_REPEAT);
    }
}

/// `GeneralLegacyIndexCodes.writeUnprintableCodes`
fn write_unprintable_codes(unprint_offset: i32, bytes: &[u8], unprintable_codes: &mut Stream) {
    // the offset counts the weight entries written before this character
    let offset = (UNPRINTABLE_COUNT_START + (UNPRINTABLE_COUNT_MULTIPLIER * unprint_offset))
        | UNPRINTABLE_OFFSET_FLAGS;

    // write offset as big-endian short
    unprintable_codes.write_byte(((offset >> 8) & 0xFF) as u8);
    unprintable_codes.write_byte((offset & 0xFF) as u8);

    unprintable_codes.write_byte(UNPRINTABLE_MIDFIX);
    unprintable_codes.write(bytes);
}

// ---------------------------------------------------------------------------
// tests

#[cfg(all(test, feature = "fixtures"))]
mod tests {
    use super::*;

    fn sort_order_for(name: &str) -> SortOrder {
        match name {
            "LEGACY" => SortOrder::GENERAL_LEGACY,
            "GENERAL" => SortOrder::GENERAL,
            "GEN97" => SortOrder::GENERAL_97,
            other => panic!("unknown collation {other}"),
        }
    }

    fn hex(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            out.push_str(&format!("{:02x}", b));
        }
        out
    }

    /// undoes the escaping used by the test fixtures
    fn unescape(s: &str) -> Vec<u16> {
        let mut out = Vec::with_capacity(s.len());
        let mut it = s.chars();
        while let Some(c) = it.next() {
            if c != '\\' {
                out.push(c as u16);
                continue;
            }
            match it.next().expect("truncated escape") {
                'n' => out.push(0x0A),
                'r' => out.push(0x0D),
                't' => out.push(0x09),
                'p' => out.push(b'|' as u16),
                'u' => {
                    let digits: String = (0..4)
                        .map(|_| it.next().expect("truncated escape"))
                        .collect();
                    out.push(u16::from_str_radix(&digits, 16).expect("bad hex escape"));
                }
                other => out.push(other as u16),
            }
        }
        out
    }

    /// runs every fixture line and returns (checked, skipped, mismatches)
    fn check_fixtures(text: &str) -> (usize, usize, Vec<String>) {
        let mut checked = 0usize;
        let mut skipped = 0usize;
        let mut mismatches: Vec<String> = Vec::new();
        for (line_no, line) in text.lines().enumerate() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut fields = line.splitn(4, '|');
            let coll = fields.next().unwrap();
            let dir = fields.next().unwrap();
            let expected = fields.next().unwrap();
            let escaped = fields.next().unwrap();

            let units = unescape(escaped);
            // a Rust &str cannot hold unpaired surrogates, so those fixtures
            // cannot be expressed through the public API at all
            let value = match String::from_utf16(&units) {
                Ok(value) => value,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };

            let mut out = Vec::new();
            write_index_text_value(
                &value,
                &sort_order_for(coll),
                Charset::Utf16Le,
                &mut out,
                dir == "A",
            )
            .expect("encoding failed");

            let actual = hex(&out);
            if actual != expected && mismatches.len() < 10 {
                mismatches.push(format!(
                    "line {}: {coll}/{dir} value {:?}: expected {} got {}",
                    line_no + 1,
                    escaped,
                    expected,
                    actual
                ));
            }
            checked += 1;
        }
        (checked, skipped, mismatches)
    }

    #[test]
    fn curated_cases_match_java() {
        let text = include_str!("../tools/indexcodes_expected.txt");
        let (checked, skipped, mismatches) = check_fixtures(text);
        assert!(
            mismatches.is_empty(),
            "{} of {} cases differ from Java ({} skipped):\n{}",
            mismatches.len(),
            checked,
            skipped,
            mismatches.join("\n")
        );
        assert!(checked > 400, "only {checked} cases checked");
        eprintln!("curated: {checked} cases match Java, {skipped} unrepresentable");
    }

    #[test]
    #[ignore = "large sweep: run with `cargo test --lib -- --ignored indexcodes`"]
    fn sweep_cases_match_java() {
        // The sweep fixture is 16 MB of generated Java output and is not
        // committed; regenerate it with tools/gen_cases.py + the Java oracle.
        // The test is ignored anyway, so a clone without it just skips.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tools/indexcodes_sweep_expected.txt");
        let Ok(text) = std::fs::read_to_string(&path) else {
            eprintln!("sweep fixture not present at {} (skipped)", path.display());
            return;
        };
        let (checked, skipped, mismatches) = check_fixtures(&text);
        assert!(
            mismatches.is_empty(),
            "{} of {} cases differ from Java ({} skipped):\n{}",
            mismatches.len(),
            checked,
            skipped,
            mismatches.join("\n")
        );
        eprintln!("sweep: {checked} cases match Java, {skipped} unrepresentable");
    }

    #[test]
    fn sort_order_fallback_is_general_legacy() {
        let mut a = Vec::new();
        let mut b = Vec::new();
        write_index_text_value(
            "abc",
            &SortOrder {
                value: 1041,
                variant: 0,
                version: 0,
            },
            Charset::Utf16Le,
            &mut a,
            true,
        )
        .unwrap();
        write_index_text_value(
            "abc",
            &SortOrder::GENERAL_LEGACY,
            Charset::Utf16Le,
            &mut b,
            true,
        )
        .unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn truncation_and_trailing_spaces() {
        // 300 chars are cut to 255, and trailing spaces are ignored
        let long = "a".repeat(300);
        let mut a = Vec::new();
        write_index_text_value(
            &long,
            &SortOrder::GENERAL_LEGACY,
            Charset::Utf16Le,
            &mut a,
            true,
        )
        .unwrap();
        let mut b = Vec::new();
        write_index_text_value(
            &"a".repeat(255),
            &SortOrder::GENERAL_LEGACY,
            Charset::Utf16Le,
            &mut b,
            true,
        )
        .unwrap();
        assert_eq!(a, b);

        let mut c = Vec::new();
        write_index_text_value(
            "abc   ",
            &SortOrder::GENERAL_LEGACY,
            Charset::Utf16Le,
            &mut c,
            true,
        )
        .unwrap();
        let mut d = Vec::new();
        write_index_text_value(
            "abc",
            &SortOrder::GENERAL_LEGACY,
            Charset::Utf16Le,
            &mut d,
            true,
        )
        .unwrap();
        assert_eq!(c, d);
    }
}
