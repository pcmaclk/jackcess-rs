//! Port of Jackcess' expression engine (`impl.expr.Expressionator` and the
//! value/function support it evaluates against), used by calculated columns,
//! `=...` default values and validation rules.
//!
//! The module is self contained: it does not read or write databases.  Column
//! values are supplied through [`EvalContext`], and results can be coerced to
//! the internal representation of a target [`DataType`] exactly like
//! `ColumnImpl.toInternalValue`.
//!
//! # Supported language
//!
//! * literals: integers (32 bit, widening to a 38 digit decimal when they do
//!   not fit), decimal numbers, `"..."` and `'...'` text (doubled quote is an
//!   escaped quote), `TRUE`/`FALSE`/`YES`/`NO`/`ON`/`OFF` (as `-1`/`0`, the
//!   Access "boolean" longs), `NULL`, and ISO date literals `#yyyy-mm-dd#`,
//!   `#hh:mm:ss#` and `#yyyy-mm-dd hh:mm:ss#` (see *Limitations*).
//! * operators, with Jackcess' precedence (tightest first): unary `-`/`+`
//!   directly before a number, `^`, unary `-`/`+`, `*` `/`, `\`, `MOD`,
//!   `+` `-`, `&`, comparisons `=` `<>` `<` `<=` `>` `>=` and `LIKE` /
//!   `IS [NOT] NULL`, `NOT`, `AND`, `OR`, `XOR`, `EQV`, `IMP`, `[NOT] IN` and
//!   `[NOT] BETWEEN`.
//! * functions: `IIf`, `Nz`, `IsNull`, `IsDate`, `IsNumeric`, `TypeName`,
//!   `VarType`, `Abs`, `Int`, `Fix`, `Sgn`, `Round`, `Sqr`, `Sin`, `Cos`,
//!   `Tan`, `Atn`, `Log`, `Exp`, `Hex`, `Oct`, `Str`, `Val`, `Space`,
//!   `String`, `Replace`, `StrReverse`, `InStr`, `InStrRev`, `StrComp`, `Len`,
//!   `Left`, `Right`, `Mid`, `LCase`, `UCase`, `Trim`, `LTrim`, `RTrim`,
//!   `Chr`, `ChrW`, `Asc`, `AscW`, `CStr`, `CLng`, `CDbl`, `CInt`, `CByte`,
//!   `CBool`, `CDec`, `CCur`, `CSng`, `CDate`, `CVar`, `Year`, `Month`, `Day`,
//!   `Hour`, `Minute`, `Second`, `DateSerial`, `TimeSerial`, `DateValue`,
//!   `TimeValue`, `Weekday`, `WeekdayName`, `MonthName`, `Choose`, `Switch`,
//!   `DatePart`, `DateAdd`, `DateDiff`, `Date`, `Now`, `Time`, `Timer`, `Rnd`,
//!   `GenGUID`, `GenUniqueID`, `StrConv`, `FormatDateTime`, `FormatNumber`,
//!   `FormatPercent`, `FormatCurrency`, `Format` (its predefined names, custom
//!   patterns and the empty format)
//!   and the financial functions `FV`, `PV`, `PMT`, `IPMT`, `PPMT`, `NPER`,
//!   `DDB`, `SLN`, `SYD` and `RATE`.
//!   The impure functions (`Date`, `Now`, `Time`, `Timer`, `Rnd`, `GenGUID`
//!   and `GenUniqueID`) read an injectable clock
//!   ([`EvalContext::set_clock`]); with no clock they use the system UTC time.
//!   Functions which Jackcess does not provide, or which need a database, are
//!   reported as [`ExprError::Unsupported`]: `Partition`, the
//!   `DLookup`/`DCount`/`DSum`/`DMax`/`DMin`/`DAvg` aggregates, `IRR`, `MIRR`
//!   and `NPV`.
//! * null propagation and three valued logic exactly as `BuiltinOperators`.
//! * numeric promotion, `Long` (Java `Integer`) wrapping, integer division and
//!   `MOD` sign rules, `BigDecimal` decimal arithmetic (38 digit coefficient,
//!   28 significant digit division and `MathContext` rounding, `HALF_EVEN`).
//! * coercion of a result to a target column type through
//!   [`Expression::eval_column`] / [`ExprValue::coerce_to`] for `LONG`, `TEXT`,
//!   `MEMO`, `GUID`, `DOUBLE`, `BOOLEAN`, `SHORT_DATE_TIME` and `NUMERIC`
//!   (plus the `BYTE`/`INT`/`BIG_INT` integer conversions).
//! * validation rules: [`ExprMode::FieldValidator`] gives `[ThisColumn]` as the
//!   implicit left value and [`Expression::eval_condition`] implements the
//!   condition conversion.
//! * `=...` defaults: [`ExprMode::DefaultValue`] strips a leading `=` and
//!   implements the literal default rule for text columns.
//!
//! # Limitations
//!
//! * Date literals support the ISO subset only (`yyyy-mm-dd[ hh:mm:ss]` and
//!   `hh:mm:ss`).  Jackcess also reads locale formats such as `1/2/2020`;
//!   those are rejected as [`ExprError::Unsupported`] rather than guessed.
//! * The `LIKE` implementation supports `*`, `?`, `#` and `[...]` / `[!...]`
//!   character classes with ranges.  Java regex constructs inside a class
//!   which Access passes through (`\d`, `&&`, `[:alpha:]`) are rejected as
//!   unsupported instead of being approximated.
//! * Decimal coefficients are 128 bit (38 digits).  Values which need more
//!   precision produce [`ExprError::Unsupported`] instead of silently losing
//!   digits.  Jackcess uses `BigDecimal` and has no such limit.  `Format` is
//!   the exception: its number path evaluates Java `BigDecimal`s (see below),
//!   so every double/float and any length of decimal coefficient formats
//!   exactly like Java.
//! * Text functions `Left`, `Right`, `Mid` and `Len` use UTF-16 code unit
//!   offsets like Java; a boundary which splits a surrogate pair is rejected
//!   as unsupported instead of producing Java's unpaired surrogate strings.
//!   `Replace`, `InStr`, `InStrRev`, `StrComp` and case-insensitive comparison
//!   reject non-BMP text as unsupported (for BMP text they are exact).
//!   Case conversion uses Rust's Unicode tables, which agree with Java for
//!   ASCII (the corpus and Access field names are ASCII).  `StrConv`'s proper
//!   case uses the same word rules as Java's `BreakIterator` for ASCII.
//! * `Format` supports the predefined format names (`"General Date"`,
//!   `"Currency"`, `"Scientific"`, ...), the empty format and custom patterns
//!   (`FormatUtil.parseCustomFormat`):
//!   * number patterns are `java.text.DecimalFormat` patterns: `0` `#` `.`
//!     `,` `%` `E`/`e` with an exponent sign, up to four `;` separated
//!     subpatterns (positive, negative, zero, null), quoted literals,
//!     `\`-escapes, `[...]` colour codes, the `!` alignment marker, grouping,
//!     minimum/maximum integer and fraction digits, the percent multiplier and
//!     the `NumberFormatter.ScientificFormat` `E+`/`e-` handling.  Rounding is
//!     `HALF_EVEN` like `DecimalFormat.setRoundingMode` and the Access
//!     quirks are reproduced (minimum integer digits forced to 0 when the
//!     pattern has no `0`, the zero subpattern's literalized and
//!     zero-stripped form, the "fall back to the zero format when rounding
//!     produces zero" rule).
//!   * date/time patterns use Jackcess' Access field codes, which are *not*
//!     `DateTimeFormatter` codes: `c` (general date/time), `d`/`dd` (day),
//!     `ddd`/`dddd` (weekday), `ddddd`/`dddddd` (predefined short/long date),
//!     `w`/`ww` (weekday number / week of year with the first day and first
//!     week arguments), `m`..`mmmm` (month), `q` (quarter), `y` (day of
//!     year), `yy`/`yyyy`, `h`/`hh` (12 hour only when an am/pm code is
//!     present, otherwise 24 hour), `n`/`nn` (minute), `s`/`ss` (second),
//!     `ttttt` (long time), `AM/PM`, `am/pm`, `A/P`, `a/p`, `AMPM`, plus
//!     `:`/`/` and every other character as a literal (`"..."` quoting,
//!     `\`-escapes and `[...]` colour codes included).
//!   * text patterns (`@`, `&`, `<`, `>`, `!`, `"..."`) and the literal choice
//!     ("yes/no") patterns are supported as well; values are coerced exactly
//!     like `FormatUtil.Args` does (strings which look like numbers or dates,
//!     booleans as `-1`/`0`, dates as their date double) and values which
//!     cannot be coerced are returned unchanged.
//!
//!   * the number path evaluates Java `BigDecimal`s like Jackcess does, with
//!     arbitrary precision: doubles use `BigDecimal.valueOf` (Java's
//!     `Double.toString` digits, reproduced exactly including the smallest
//!     subnormals and exact ties), `"General Number"` uses
//!     `new BigDecimal(double, MathContext(15))`'s exact binary expansion for
//!     `Double` and `MathContext(28)` for `BigDecimal` values, the percent
//!     multiplier and `HALF_EVEN` rounding are exact, and coefficients with
//!     any number of digits (a 40 digit number string, a 38 digit number
//!     formatted with `%`) format exactly.
//!
//!   The remaining `Format` limitations are the cases where Jackcess 5.0.4
//!   itself throws a non-`EvalException`, which the port reports as
//!   [`ExprError::Unsupported`]: the date codes `M`, `P` and `p` have no
//!   `DATE_FMT_BUILDERS` entry (Java `NullPointerException`; note that the
//!   usual `DateTimeFormatter` codes `M`, `E`, `H`, `S` and `u` are therefore
//!   *literals* here, exactly as in Jackcess), malformed `DecimalFormat`
//!   patterns are Java `IllegalArgumentException`, NaN/infinity cannot be
//!   converted to a `BigDecimal` (Java `NumberFormatException`), and the
//!   per-mille (`‰`) multiplier is unreachable because `‰` is a literal
//!   character in an Access format.  An unterminated `"`/`[` in a pattern is
//!   the `ParseException` of `createFormat`, which the function call layer
//!   wraps into an `EvalException` exactly like `FuncVar.evalVar`.
//!   `StrConv` accepts only the US locale id (1033); other locale ids change
//!   the case mapping and are reported as unsupported.
//! * `Date`, `Now`, `Time`, `Timer` and `Rnd` use the clock injected with
//!   [`EvalContext::set_clock`] (the system UTC time by default).  Jackcess
//!   reads the current local time in the database's time zone and seeds `Rnd`
//!   from `System.currentTimeMillis`; the port has no time zone database, so
//!   an unseeded `Rnd` is only reproducible through the injected clock.
//! * `Rnd` reproduces `java.util.Random` and the `RandomContext` state machine
//!   (seeded sequences, the seed reset of a negative seed and the last value
//!   of a zero seed).  The generator state lives in [`EvalContext`], so one
//!   context behaves like one Jackcess database.  `GenUniqueID` and `GenGUID`
//!   use their own generators seeded from the injected clock; their values are
//!   random by definition and cannot be compared with the Java oracle, so they
//!   are covered by structural tests instead.
//! * A bare column name is accepted as a column reference (Jackcess requires
//!   `[Name]`); use the bracketed form for byte-for-byte compatibility with
//!   Access expressions.
//!
//! The module only depends on [`crate::datenums`] (date doubles/`LocalDateTime`)
//! and the pure [`crate::datatype::DataType`] enum.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt;
use std::num::NonZeroU64;
use std::rc::Rc;
use std::str::FromStr;

use bigdecimal::num_bigint::{BigInt, Sign};
use bigdecimal::{BigDecimal, RoundingMode};

use crate::datatype::DataType;
use crate::datenums::{self, LocalDateTime};

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------

/// Error returned by parsing or evaluating an expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExprError {
    /// `ParseException`: the expression string is malformed.
    Parse(String),
    /// `EvalException`: the expression is valid but cannot be evaluated with
    /// these values (bad conversion, out of range, invalid date, ...).
    Eval(String),
    /// `ArithmeticException`: division/modulo by zero or integer overflow
    /// while rounding.
    Arithmetic(String),
    /// A Jackcess construct this port deliberately does not implement.  The
    /// message always names the construct.
    Unsupported(String),
}

impl ExprError {
    /// The category name used by the Java oracle (`Parse`, `Eval`,
    /// `Arithmetic`, `Unsupported`).
    pub fn kind_name(&self) -> &'static str {
        match self {
            ExprError::Parse(_) => "Parse",
            ExprError::Eval(_) => "Eval",
            ExprError::Arithmetic(_) => "Arithmetic",
            ExprError::Unsupported(_) => "Unsupported",
        }
    }
}

impl fmt::Display for ExprError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExprError::Parse(m) => write!(f, "parse error: {m}"),
            ExprError::Eval(m) => write!(f, "evaluation error: {m}"),
            ExprError::Arithmetic(m) => write!(f, "arithmetic error: {m}"),
            ExprError::Unsupported(m) => write!(f, "unsupported: {m}"),
        }
    }
}

impl std::error::Error for ExprError {}

/// Convenience result type for the expression engine.
pub type ExprResult<T> = std::result::Result<T, ExprError>;

fn parse_err<T>(msg: impl Into<String>) -> ExprResult<T> {
    Err(ExprError::Parse(msg.into()))
}

fn eval_err<T>(msg: impl Into<String>) -> ExprResult<T> {
    Err(ExprError::Eval(msg.into()))
}

fn arith_err<T>(msg: impl Into<String>) -> ExprResult<T> {
    Err(ExprError::Arithmetic(msg.into()))
}

fn unsupported<T>(msg: impl Into<String>) -> ExprResult<T> {
    Err(ExprError::Unsupported(msg.into()))
}

// ---------------------------------------------------------------------------
// decimal (java.math.BigDecimal subset)
// ---------------------------------------------------------------------------

/// Minimal arbitrary precision decimal with the semantics of the subset of
/// `java.math.BigDecimal` used by the expression engine.
///
/// A value is `unscaled * 10^-scale` with a normalised representation: the
/// coefficient has no trailing zeros and `scale >= 0`.  Parsing and the basic
/// operations (`+ - * /`, `pow`, `setScale`) round with `HALF_EVEN` where
/// `BigDecimal`'s `MathContext(28, HALF_EVEN)` does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decimal {
    unscaled: i128,
    scale: i32,
}

/// `NumberFormatter.DEC_SIG_DIGITS`.
const DEC_SIG_DIGITS: u32 = 28;

/// The largest number of decimal digits which fit in the coefficient.
const MAX_DEC_DIGITS: usize = 38;

fn pow10(power: u32) -> ExprResult<i128> {
    let mut result: i128 = 1;
    let mut i = 0;
    while i < power {
        result = result.checked_mul(10).ok_or_else(|| {
            ExprError::Unsupported("decimal value exceeds the 38 digit coefficient".into())
        })?;
        i += 1;
    }
    Ok(result)
}

fn digits_i128(value: i128) -> i32 {
    let mut v = value.unsigned_abs();
    if v == 0 {
        return 1;
    }
    let mut digits = 0;
    while v > 0 {
        v /= 10;
        digits += 1;
    }
    digits
}

fn digits_u128(mut value: u128) -> i32 {
    if value == 0 {
        return 1;
    }
    let mut digits = 0;
    while value > 0 {
        value /= 10;
        digits += 1;
    }
    digits
}

/// Divide, rounding to nearest with ties to even (positive denominator).
fn div_round_half_even(num: i128, den: i128) -> ExprResult<i128> {
    if den == 0 {
        return arith_err("/ by zero");
    }
    let q = num / den;
    let r = num % den;
    if r == 0 {
        return Ok(q);
    }
    let negative = (num < 0) != (den < 0);
    let abs_r = r.unsigned_abs();
    let abs_d = den.unsigned_abs();
    let half = abs_d / 2;
    let round_away = if abs_d.is_multiple_of(2) && abs_r == half {
        (q & 1) != 0
    } else {
        abs_r.saturating_mul(2) >= abs_d
    };
    if round_away {
        Ok(if negative { q - 1 } else { q + 1 })
    } else {
        Ok(q)
    }
}

impl Decimal {
    /// Zero.
    pub fn zero() -> Decimal {
        Decimal {
            unscaled: 0,
            scale: 0,
        }
    }

    /// The coefficient (value = `unscaled * 10^-scale`).
    pub fn unscaled(&self) -> i128 {
        self.unscaled
    }

    /// The scale (number of fractional digits).
    pub fn scale(&self) -> i32 {
        self.scale
    }

    /// `new BigDecimal(value)` after `ValueSupport.normalize`.
    pub fn from_i64(value: i64) -> Decimal {
        Decimal {
            unscaled: value as i128,
            scale: 0,
        }
    }

    /// The decimal with the given coefficient and scale, normalised the way
    /// `ValueSupport.normalize` normalises a `BigDecimal` (no trailing zeros,
    /// no negative scale).
    pub fn from_unscaled(unscaled: i128, scale: i32) -> ExprResult<Decimal> {
        Decimal::normalized(unscaled, scale)
    }

    fn normalized(mut unscaled: i128, mut scale: i32) -> ExprResult<Decimal> {
        if unscaled == 0 {
            return Ok(Decimal::zero());
        }
        while scale > 0 && (unscaled % 10) == 0 {
            unscaled /= 10;
            scale -= 1;
        }
        if scale < 0 {
            let factor = pow10(scale.unsigned_abs())?;
            unscaled = unscaled.checked_mul(factor).ok_or_else(|| {
                ExprError::Unsupported("decimal value exceeds the 38 digit coefficient".into())
            })?;
            scale = 0;
        }
        Ok(Decimal { unscaled, scale })
    }

    /// `new BigDecimal(String)` (also used for exponent forms such as `1e3`).
    pub fn parse(text: &str) -> ExprResult<Decimal> {
        let bytes = text.as_bytes();
        if bytes.is_empty() {
            return parse_err(format!("Invalid number literal '{text}'"));
        }
        let mut pos = 0;
        let mut negative = false;
        if bytes[pos] == b'+' || bytes[pos] == b'-' {
            negative = bytes[pos] == b'-';
            pos += 1;
        }
        let mut digits = String::new();
        let mut fraction = 0i32;
        let mut seen_digit = false;
        let mut seen_dot = false;
        while pos < bytes.len() {
            let c = bytes[pos];
            if c.is_ascii_digit() {
                digits.push(c as char);
                seen_digit = true;
                if seen_dot {
                    fraction += 1;
                }
                pos += 1;
            } else if c == b'.' && !seen_dot {
                seen_dot = true;
                pos += 1;
            } else {
                break;
            }
        }
        if !seen_digit {
            return parse_err(format!("Invalid number literal '{text}'"));
        }
        let mut exponent: i32 = 0;
        if pos < bytes.len() && (bytes[pos] == b'e' || bytes[pos] == b'E') {
            pos += 1;
            let mut exp_negative = false;
            if pos < bytes.len() && (bytes[pos] == b'+' || bytes[pos] == b'-') {
                exp_negative = bytes[pos] == b'-';
                pos += 1;
            }
            let mut exp_digits = String::new();
            while pos < bytes.len() && bytes[pos].is_ascii_digit() {
                exp_digits.push(bytes[pos] as char);
                pos += 1;
            }
            if exp_digits.is_empty() {
                return parse_err(format!("Invalid number literal '{text}'"));
            }
            let parsed: i64 = exp_digits.parse().map_err(|_| {
                ExprError::Unsupported(format!("exponent of '{text}' is too large"))
            })?;
            let signed = if exp_negative { -parsed } else { parsed };
            exponent = i32::try_from(signed).map_err(|_| {
                ExprError::Unsupported(format!("exponent of '{text}' is too large"))
            })?;
        }
        if pos != bytes.len() {
            return parse_err(format!("Invalid number literal '{text}'"));
        }
        if digits.len() > MAX_DEC_DIGITS {
            return Err(ExprError::Unsupported(format!(
                "number literal '{text}' has more than {MAX_DEC_DIGITS} digits"
            )));
        }
        let unscaled: i128 = digits
            .parse()
            .map_err(|_| ExprError::Parse(format!("Invalid number literal '{text}'")))?;
        let scale = fraction
            .checked_sub(exponent)
            .ok_or_else(|| ExprError::Unsupported(format!("exponent of '{text}' is too large")))?;
        Decimal::normalized(if negative { -unscaled } else { unscaled }, scale)
    }

    /// `BigDecimal.valueOf(double)`: the shortest decimal which round trips.
    pub fn from_f64(value: f64) -> ExprResult<Decimal> {
        if !value.is_finite() {
            return eval_err("NaN/Infinity cannot be converted to a decimal");
        }
        if value == 0.0 {
            return Ok(Decimal::zero());
        }
        let text = format!("{value:e}");
        Decimal::parse(&text)
    }

    /// The exact value of an integer valued `double` as a decimal, like
    /// `new BigDecimal(double)` for an integral argument.  This is what
    /// `ColumnImpl.toBigDecimal(Number)` uses for BIG_INT column values.
    fn from_f64_exact_integer(value: f64) -> ExprResult<Decimal> {
        if !value.is_finite() {
            return eval_err("NaN/Infinity cannot be converted to a decimal");
        }
        let text = format!("{value:.0}");
        Decimal::parse(&text)
    }

    fn checked_scale(scale: i64) -> ExprResult<i32> {
        i32::try_from(scale).map_err(|_| {
            ExprError::Unsupported("decimal exponent exceeds the supported range".into())
        })
    }

    fn round_drop(self, drop: u32) -> ExprResult<Decimal> {
        if drop == 0 {
            return Ok(self);
        }
        let factor = pow10(drop)?;
        let q = div_round_half_even(self.unscaled, factor)?;
        let scale = (self.scale as i64) - (drop as i64);
        Decimal::normalized(q, Decimal::checked_scale(scale)?)
    }

    /// Round to `prec` significant digits (`HALF_EVEN`).
    pub fn round_to_sig(self, prec: u32) -> ExprResult<Decimal> {
        let digits = digits_i128(self.unscaled) as u32;
        if digits <= prec {
            Ok(self)
        } else {
            self.round_drop(digits - prec)
        }
    }

    /// `setScale(target, HALF_EVEN)`.
    pub fn round_to_scale(self, target: i32) -> ExprResult<Decimal> {
        if target == self.scale {
            return Ok(self);
        }
        if target > self.scale {
            let factor = pow10((target - self.scale) as u32)?;
            let unscaled = self.unscaled.checked_mul(factor).ok_or_else(|| {
                ExprError::Unsupported("decimal value exceeds the 38 digit coefficient".into())
            })?;
            Decimal::normalized(unscaled, target)
        } else {
            self.round_drop((self.scale - target) as u32)
        }
    }

    /// `setScale(0, HALF_EVEN).intValueExact()`.
    pub fn to_i32_round(self) -> ExprResult<i32> {
        let rounded = self.round_to_scale(0)?;
        let value = rounded.unscaled;
        if value < i32::MIN as i128 || value > i32::MAX as i128 {
            return arith_err("integer overflow");
        }
        Ok(value as i32)
    }

    fn div_round_half_even_i128(self, other: Decimal) -> ExprResult<Decimal> {
        if other.unscaled == 0 {
            return arith_err("/ by zero");
        }
        let negative = (self.unscaled < 0) != (other.unscaled < 0);
        let ua = self.unscaled.unsigned_abs();
        let ub = other.unscaled.unsigned_abs();
        let da = digits_u128(ua);
        let db = digits_u128(ub);
        let t = DEC_SIG_DIGITS as i32 - (da - db);
        let (numerator, denominator) = if t >= 0 {
            let factor = pow10(t as u32)? as i128;
            let n = i128::try_from(ua)
                .map_err(|_| ExprError::Unsupported("decimal division overflow".into()))?
                .checked_mul(factor)
                .ok_or_else(|| ExprError::Unsupported("decimal division overflow".into()))?;
            (n, ub as i128)
        } else {
            let factor = pow10((-t) as u32)? as i128;
            let d = (ub as i128)
                .checked_mul(factor)
                .ok_or_else(|| ExprError::Unsupported("decimal division overflow".into()))?;
            (
                i128::try_from(ua)
                    .map_err(|_| ExprError::Unsupported("decimal division overflow".into()))?,
                d,
            )
        };
        let mut q = div_round_half_even(numerator, denominator)?;
        let mut scale = (self.scale as i64) - (other.scale as i64) + (t as i64);
        if digits_i128(q) > DEC_SIG_DIGITS as i32 {
            let dropped = q % 10;
            let rest = q / 10;
            let remainder = numerator % denominator;
            let round_away = if dropped > 5 {
                true
            } else if dropped < 5 {
                false
            } else if remainder != 0 {
                true
            } else {
                (rest & 1) != 0
            };
            q = if round_away { rest + 1 } else { rest };
            scale -= 1;
        }
        Decimal::normalized(
            if negative { -q } else { q },
            Decimal::checked_scale(scale)?,
        )
    }

    /// Exact addition, rounded to 28 significant digits.
    // Named `add` to mirror the Java `BigDecimal`-style API of `Decimal`;
    // implementing `std::ops::Add` would hide the fallible result.
    #[expect(
        clippy::should_implement_trait,
        reason = "mirrors the Java Decimal API; the operation is fallible"
    )]
    pub fn add(self, other: Decimal) -> ExprResult<Decimal> {
        let scale = self.scale.max(other.scale);
        let aligned = (|| -> Option<(i128, i128)> {
            let factor_a = pow10((scale - self.scale) as u32).ok()?;
            let factor_b = pow10((scale - other.scale) as u32).ok()?;
            Some((
                self.unscaled.checked_mul(factor_a)?,
                other.unscaled.checked_mul(factor_b)?,
            ))
        })();
        let (a, b) =
            match aligned {
                Some(pair) => pair,
                None => {
                    let a = self.round_to_sig(DEC_SIG_DIGITS)?;
                    let b = other.round_to_sig(DEC_SIG_DIGITS)?;
                    let scale = a.scale.max(b.scale);
                    let factor_a = pow10((scale - a.scale) as u32)?;
                    let factor_b = pow10((scale - b.scale) as u32)?;
                    let av = a.unscaled.checked_mul(factor_a).ok_or_else(|| {
                        ExprError::Unsupported("decimal addition overflow".into())
                    })?;
                    let bv = b.unscaled.checked_mul(factor_b).ok_or_else(|| {
                        ExprError::Unsupported("decimal addition overflow".into())
                    })?;
                    let sum = av.checked_add(bv).ok_or_else(|| {
                        ExprError::Unsupported("decimal addition overflow".into())
                    })?;
                    return Decimal::normalized(sum, scale)?.round_to_sig(DEC_SIG_DIGITS);
                }
            };
        let sum = a
            .checked_add(b)
            .ok_or_else(|| ExprError::Unsupported("decimal addition overflow".into()))?;
        Decimal::normalized(sum, scale)?.round_to_sig(DEC_SIG_DIGITS)
    }

    /// Exact subtraction, rounded to 28 significant digits.
    pub fn subtract(self, other: Decimal) -> ExprResult<Decimal> {
        self.add(other.negate())
    }

    /// Exact multiplication, rounded to 28 significant digits.
    pub fn multiply(self, other: Decimal) -> ExprResult<Decimal> {
        let scale = (self.scale as i64) + (other.scale as i64);
        let scale = Decimal::checked_scale(scale)?;
        let product = match self.unscaled.checked_mul(other.unscaled) {
            Some(v) => v,
            None => {
                let a = self.round_to_sig(DEC_SIG_DIGITS)?;
                let b = other.round_to_sig(DEC_SIG_DIGITS)?;
                a.unscaled.checked_mul(b.unscaled).ok_or_else(|| {
                    ExprError::Unsupported("decimal multiplication overflow".into())
                })?
            }
        };
        Decimal::normalized(product, scale)?.round_to_sig(DEC_SIG_DIGITS)
    }

    /// `BigDecimal.divide(other, MathContext(28, HALF_EVEN))`.
    pub fn divide(self, other: Decimal) -> ExprResult<Decimal> {
        self.div_round_half_even_i128(other)
    }

    /// `BigDecimal.pow(exponent, MathContext(28, HALF_EVEN))` for a
    /// non-negative `i32` exponent.
    pub fn pow(self, exponent: i32) -> ExprResult<Decimal> {
        if exponent < 0 {
            return eval_err("negative exponent");
        }
        let mut result = Decimal::from_i64(1);
        let mut base = self;
        let mut exp = exponent as u32;
        while exp > 0 {
            if (exp & 1) != 0 {
                result = result.multiply(base)?;
            }
            exp >>= 1;
            if exp > 0 {
                base = base.multiply(base)?;
            }
        }
        Ok(result)
    }

    /// Negation (`BigDecimal.negate`).
    pub fn negate(self) -> Decimal {
        Decimal {
            unscaled: -self.unscaled,
            scale: self.scale,
        }
    }

    /// `BigDecimal.abs`.
    pub fn abs(self) -> Decimal {
        Decimal {
            unscaled: self.unscaled.abs(),
            scale: self.scale,
        }
    }

    /// `BigDecimal.signum`.
    pub fn signum(self) -> i32 {
        match self.unscaled.cmp(&0) {
            Ordering::Less => -1,
            Ordering::Equal => 0,
            Ordering::Greater => 1,
        }
    }

    /// `BigDecimal.compareTo`.
    pub fn compare(self, other: Decimal) -> Ordering {
        let scale = self.scale.max(other.scale);
        let a = (self.scale, self.unscaled);
        let b = (other.scale, other.unscaled);
        // Align without overflow: both coefficients are at most 38 digits, so
        // any alignment padding beyond a few digits can be decided by scale.
        let mut factor_a: i128 = 1;
        let mut factor_b: i128 = 1;
        let mut overflow = false;
        if a.0 < scale {
            match pow10((scale - a.0) as u32) {
                Ok(f) => factor_a = f,
                Err(_) => overflow = true,
            }
        }
        if b.0 < scale {
            match pow10((scale - b.0) as u32) {
                Ok(f) => factor_b = f,
                Err(_) => overflow = true,
            }
        }
        if overflow {
            // Compare via the exact 30 significant digit decimal strings.
            let sa = self.to_plain_string();
            let sb = other.to_plain_string();
            return compare_decimal_strings(&sa, &sb);
        }
        let av = self.unscaled.saturating_mul(factor_a);
        let bv = other.unscaled.saturating_mul(factor_b);
        av.cmp(&bv)
    }

    /// The value as a plain decimal string, never using exponent notation.
    pub fn to_plain_string(self) -> String {
        let negative = self.unscaled < 0;
        let digits = self.unscaled.unsigned_abs().to_string();
        let mut out = String::new();
        if negative {
            out.push('-');
        }
        if self.scale <= 0 {
            out.push_str(&digits);
            for _ in 0..(-self.scale) {
                out.push('0');
            }
        } else if (digits.len() as i32) > self.scale {
            let split = digits.len() - self.scale as usize;
            out.push_str(&digits[..split]);
            out.push('.');
            out.push_str(&digits[split..]);
        } else {
            out.push_str("0.");
            for _ in 0..(self.scale as usize - digits.len()) {
                out.push('0');
            }
            out.push_str(&digits);
        }
        out
    }

    /// `BigDecimal.doubleValue`.
    /// `BigDecimal.longValue()`: truncate toward zero, keeping the low 64
    /// bits.
    pub fn to_i64_trunc(self) -> i64 {
        let mut value = self.unscaled;
        if self.scale > 0 {
            let mut drop = self.scale as u32;
            while drop > 0 {
                value /= 10;
                drop -= 1;
            }
        } else {
            let mut pad = (-self.scale) as u32;
            while pad > 0 {
                match value.checked_mul(10) {
                    Some(next) => value = next,
                    None => return value as i64,
                }
                pad -= 1;
            }
        }
        value as i64
    }

    pub fn to_f64(self) -> f64 {
        let text = self.to_plain_string();
        match text.parse::<f64>() {
            Ok(v) => v,
            Err(_) => {
                if self.unscaled < 0 {
                    f64::NEG_INFINITY
                } else {
                    f64::INFINITY
                }
            }
        }
    }

    /// Remove trailing zeros, allowing a negative scale, like
    /// `BigDecimal.stripTrailingZeros`.
    fn stripped(self) -> Decimal {
        let mut unscaled = self.unscaled;
        let mut scale = self.scale;
        if unscaled == 0 {
            return Decimal::zero();
        }
        while (unscaled % 10) == 0 {
            unscaled /= 10;
            scale -= 1;
        }
        Decimal { unscaled, scale }
    }

    fn scientific_string(self) -> String {
        let negative = self.unscaled < 0;
        let digits = self.unscaled.unsigned_abs().to_string();
        let exponent = digits.len() as i32 - self.scale - 1;
        let mut out = String::new();
        if negative {
            out.push('-');
        }
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('E');
        out.push(if exponent < 0 { '-' } else { '+' });
        let abs_exp = exponent.unsigned_abs();
        if abs_exp < 10 {
            out.push('0');
        }
        out.push_str(&abs_exp.to_string());
        out
    }

    /// `NumberFormatter`'s decimal formatting (`DecimalFormat("0.#")` with
    /// `prec` significant digits, falling back to scientific notation).
    fn format_with_prec(self, prec: u32) -> ExprResult<String> {
        let rounded = self.round_to_sig(prec)?;
        let stripped = rounded.stripped();
        if stripped.unscaled == 0 {
            return Ok("0".to_string());
        }
        let precision = digits_i128(stripped.unscaled);
        let mut sig_digits = precision;
        if stripped.scale < 0 {
            sig_digits -= stripped.scale;
        } else if stripped.scale > precision {
            sig_digits += stripped.scale - precision;
        }
        if sig_digits > prec as i32 {
            Ok(stripped.scientific_string())
        } else {
            Ok(stripped.to_plain_string())
        }
    }

    /// Formatting used when a decimal is converted to a string
    /// (`Value.Type.BIG_DEC`).
    pub fn format(self) -> String {
        self.format_with_prec(DEC_SIG_DIGITS)
            .unwrap_or_else(|_| self.to_plain_string())
    }
}

fn compare_decimal_strings(a: &str, b: &str) -> Ordering {
    fn split(s: &str) -> (bool, String, String) {
        let (neg, s) = match s.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, s),
        };
        let mut parts = s.splitn(2, '.');
        let int = parts
            .next()
            .unwrap_or("0")
            .trim_start_matches('0')
            .to_string();
        let frac = parts.next().unwrap_or("").to_string();
        (neg, int, frac)
    }
    let (na, ia, fa) = split(a);
    let (nb, ib, fb) = split(b);
    if na != nb {
        return if na {
            Ordering::Less
        } else {
            Ordering::Greater
        };
    }
    let mag = {
        let ia = if ia.is_empty() { "0" } else { &ia };
        let ib = if ib.is_empty() { "0" } else { &ib };
        let int_cmp = ia.len().cmp(&ib.len()).then_with(|| ia.cmp(ib));
        if int_cmp != Ordering::Equal {
            int_cmp
        } else {
            let max = fa.len().max(fb.len());
            let mut fa2 = fa.clone();
            let mut fb2 = fb.clone();
            while fa2.len() < max {
                fa2.push('0');
            }
            while fb2.len() < max {
                fb2.push('0');
            }
            fa2.cmp(&fb2)
        }
    };
    if na {
        mag.reverse()
    } else {
        mag
    }
}

// ---------------------------------------------------------------------------
// values
// ---------------------------------------------------------------------------

/// The date/time variants of `Value.Type` (`DATE`, `TIME`, `DATE_TIME`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DateKind {
    /// `Value.Type.DATE`
    Date,
    /// `Value.Type.TIME`
    Time,
    /// `Value.Type.DATE_TIME`
    DateTime,
}

/// A value produced by evaluating an expression.
///
/// The variants follow Jackcess' `Value.Type`: Access has no boolean value
/// type, `TRUE`/`FALSE` are the `Long` values `-1`/`0`.  [`ExprValue::Bool`]
/// only appears as the internal representation of a `BOOLEAN` column value /
/// coercion result; [`EvalContext::set_column`] normalises it to `Long` (like
/// `BaseEvalContext.toValue` does).
#[derive(Clone, Debug, PartialEq)]
pub enum ExprValue {
    /// `Value.Type.NULL`
    Null,
    /// A Java `Boolean`; normalised to `Long(-1/0)` inside the evaluator.
    Bool(bool),
    /// A Java `Byte` (an 8 bit column value / coercion result).
    Byte(i8),
    /// A Java `Short` (a 16 bit column value / coercion result).
    Int(i16),
    /// `Value.Type.LONG` (a Java `Integer`, Access' native 32 bit integer).
    Long(i32),
    /// A Java `Long` (a 64 bit `BIG_INT` column value / coercion result).
    BigInt(i64),
    /// `Value.Type.DOUBLE` (a Java `Double`).
    Double(f64),
    /// `Value.Type.BIG_DEC` (a Java `BigDecimal`).
    Decimal(Decimal),
    /// `Value.Type.STRING`.
    Text(String),
    /// `Value.Type.DATE`, `TIME` or `DATE_TIME`.
    DateTime {
        kind: DateKind,
        value: LocalDateTime,
    },
}

impl ExprValue {
    /// `ValueSupport.toValue(boolean)`: Access booleans are `-1`/`0`.
    pub fn bool(value: bool) -> ExprValue {
        ExprValue::Long(if value { -1 } else { 0 })
    }

    /// A `DATE` value (`ValueSupport.toValue(LocalDate)`).
    pub fn date(value: LocalDateTime) -> ExprValue {
        ExprValue::DateTime {
            kind: DateKind::Date,
            value,
        }
    }

    /// A `TIME` value (`ValueSupport.toValue(LocalTime)`).
    pub fn time(value: LocalDateTime) -> ExprValue {
        ExprValue::DateTime {
            kind: DateKind::Time,
            value,
        }
    }

    /// A `DATE_TIME` value (`ValueSupport.toValue(DATE_TIME, ldt)`).
    pub fn date_time(value: LocalDateTime) -> ExprValue {
        ExprValue::DateTime {
            kind: DateKind::DateTime,
            value,
        }
    }

    /// A date/time value with the type derived from its fields
    /// (`ValueSupport.toValue(LocalDateTime)` / `getDateTimeType`).
    pub fn from_local_date_time(value: LocalDateTime) -> ExprValue {
        ExprValue::DateTime {
            kind: date_kind_of(value),
            value,
        }
    }

    /// `Value.isNull()`.
    pub fn is_null(&self) -> bool {
        matches!(self, ExprValue::Null)
    }

    /// The `Value.Type` of this value.
    pub fn kind(&self) -> ValueKind {
        match self {
            ExprValue::Null => ValueKind::Null,
            ExprValue::Bool(_)
            | ExprValue::Byte(_)
            | ExprValue::Int(_)
            | ExprValue::Long(_)
            | ExprValue::BigInt(_) => ValueKind::Long,
            ExprValue::Double(_) => ValueKind::Double,
            ExprValue::Decimal(_) => ValueKind::Decimal,
            ExprValue::Text(_) => ValueKind::Text,
            ExprValue::DateTime { kind, .. } => match kind {
                DateKind::Date => ValueKind::Date,
                DateKind::Time => ValueKind::Time,
                DateKind::DateTime => ValueKind::DateTime,
            },
        }
    }

    /// `Value.getAsBoolean` (only called for non-null values by operators).
    pub fn get_as_boolean(&self) -> ExprResult<bool> {
        match self {
            ExprValue::Null => eval_err("Null cannot be converted to a boolean"),
            ExprValue::Bool(b) => Ok(*b),
            ExprValue::Byte(v) => Ok(*v != 0),
            ExprValue::Int(v) => Ok(*v != 0),
            ExprValue::Long(v) => Ok(*v != 0),
            ExprValue::BigInt(v) => Ok(*v != 0),
            ExprValue::Double(v) => Ok(*v != 0.0),
            ExprValue::Decimal(v) => Ok(!v.unscaled_is_zero()),
            // ms access seems to treat strings as "true"
            ExprValue::Text(_) => Ok(true),
            // ms access seems to treat dates/times as "true"
            ExprValue::DateTime { .. } => Ok(true),
        }
    }

    /// `Value.getAsString`.
    pub fn get_as_string(&self) -> ExprResult<String> {
        match self {
            ExprValue::Null => eval_err("Null cannot be converted to STRING"),
            ExprValue::Bool(b) => Ok(if *b { "-1" } else { "0" }.to_string()),
            ExprValue::Byte(v) => Ok(v.to_string()),
            ExprValue::Int(v) => Ok(v.to_string()),
            ExprValue::Long(v) => Ok(v.to_string()),
            ExprValue::BigInt(v) => Ok(v.to_string()),
            ExprValue::Double(v) => Ok(format_double(*v)),
            ExprValue::Decimal(v) => Ok(v.format()),
            ExprValue::Text(s) => Ok(s.clone()),
            ExprValue::DateTime { kind, value } => Ok(format_date_time(*kind, *value)),
        }
    }

    /// `Value.getAsLongInt` (`Integer`), rounding with `HALF_EVEN`.
    pub fn get_as_long(&self) -> ExprResult<i32> {
        match self {
            ExprValue::Null => eval_err("Null cannot be converted to LONG"),
            ExprValue::Bool(b) => Ok(if *b { -1 } else { 0 }),
            ExprValue::Byte(v) => Ok(*v as i32),
            ExprValue::Int(v) => Ok(*v as i32),
            ExprValue::Long(v) => Ok(*v),
            ExprValue::BigInt(v) => Decimal::from_i64(*v).to_i32_round(),
            ExprValue::Double(v) => Decimal::from_f64(*v)?.to_i32_round(),
            ExprValue::Decimal(v) => v.to_i32_round(),
            ExprValue::Text(s) => parse_string_decimal(s)?.to_i32_round(),
            ExprValue::DateTime { value, .. } => {
                Decimal::from_f64(datenums::to_date_double(*value))?.to_i32_round()
            }
        }
    }

    /// `Value.getAsDouble`.
    pub fn get_as_double(&self) -> ExprResult<f64> {
        match self {
            ExprValue::Null => eval_err("Null cannot be converted to DOUBLE"),
            ExprValue::Bool(b) => Ok(if *b { -1.0 } else { 0.0 }),
            ExprValue::Byte(v) => Ok(*v as f64),
            ExprValue::Int(v) => Ok(*v as f64),
            ExprValue::Long(v) => Ok(*v as f64),
            ExprValue::BigInt(v) => Ok(*v as f64),
            ExprValue::Double(v) => Ok(*v),
            ExprValue::Decimal(v) => Ok(v.to_f64()),
            ExprValue::Text(s) => Ok(parse_string_decimal(s)?.to_f64()),
            ExprValue::DateTime { value, .. } => Ok(datenums::to_date_double(*value)),
        }
    }

    /// `Value.getAsBigDecimal`.
    pub fn get_as_decimal(&self) -> ExprResult<Decimal> {
        match self {
            ExprValue::Null => eval_err("Null cannot be converted to BIG_DEC"),
            ExprValue::Bool(b) => Ok(Decimal::from_i64(if *b { -1 } else { 0 })),
            ExprValue::Byte(v) => Ok(Decimal::from_i64(*v as i64)),
            ExprValue::Int(v) => Ok(Decimal::from_i64(*v as i64)),
            ExprValue::Long(v) => Ok(Decimal::from_i64(*v as i64)),
            ExprValue::BigInt(v) => Ok(Decimal::from_i64(*v)),
            ExprValue::Double(v) => Decimal::from_f64(*v),
            ExprValue::Decimal(v) => Ok(*v),
            ExprValue::Text(s) => parse_string_decimal(s),
            ExprValue::DateTime { value, .. } => {
                Decimal::from_f64(datenums::to_date_double(*value))
            }
        }
    }

    /// `Value.getAsDateTimeValue` / `getAsLocalDateTime`, with the same
    /// string -> number -> date fallback as `StringValue`.
    pub fn get_as_local_date_time(&self) -> ExprResult<LocalDateTime> {
        match self {
            ExprValue::Null => eval_err("Null cannot be converted to DATE_TIME"),
            ExprValue::DateTime { value, .. } => Ok(*value),
            ExprValue::Byte(_)
            | ExprValue::Int(_)
            | ExprValue::Long(_)
            | ExprValue::BigInt(_)
            | ExprValue::Double(_)
            | ExprValue::Decimal(_) => {
                let dd = self.get_as_double()?;
                number_to_date_value(dd)
            }
            ExprValue::Text(s) => {
                if let Some(value) = string_to_date_value(s)? {
                    return value.get_as_local_date_time();
                }
                match parse_string_decimal(s) {
                    Ok(dec) => number_to_date_value(dec.to_f64()),
                    Err(_) => eval_err(format!("'{s}' cannot be converted to DATE_TIME")),
                }
            }
            ExprValue::Bool(b) => {
                let dd = if *b { -1.0 } else { 0.0 };
                number_to_date_value(dd)
            }
        }
    }

    /// Coerce this value to the internal representation of a column type,
    /// mirroring `ColumnImpl.toInternalValue` (including the result type
    /// conversion `CalcColEvalContext` applies first).
    ///
    /// Supported types: `BOOLEAN`, `BYTE`, `INT`, `LONG`, `BIG_INT`, `DOUBLE`,
    /// `TEXT`, `MEMO`, `GUID`, `NUMERIC` and `SHORT_DATE_TIME`.
    /// `MONEY`, `FLOAT`, `EXT_DATE_TIME` and the binary types return
    /// [`ExprError::Unsupported`] (`MONEY` would need the exact
    /// `new BigDecimal(double)` conversion of Java).
    pub fn coerce_to(&self, target: DataType) -> ExprResult<ExprValue> {
        if self.is_null() {
            return Ok(ExprValue::Null);
        }
        use DataType::*;
        match target {
            Boolean => {
                // CalcColEvalContext result type is LONG, which rounds first
                let value = self.get_as_long()?;
                Ok(ExprValue::Bool(value != 0))
            }
            Byte => {
                let value = self.get_as_long()?;
                Ok(ExprValue::Byte(value as i8))
            }
            Int => {
                let value = self.get_as_long()?;
                Ok(ExprValue::Int(value as i16))
            }
            Long => Ok(ExprValue::Long(self.get_as_long()?)),
            BigInt => Ok(ExprValue::BigInt(self.get_as_decimal()?.to_i64_trunc())),
            Numeric => Ok(ExprValue::Decimal(self.get_as_decimal()?)),
            Double => Ok(ExprValue::Double(self.get_as_double()?)),
            // `BaseEvalContext.TYPE_MAP` maps MONEY/FLOAT to Value.Type.DOUBLE, so
            // calculated columns of those result types evaluate to a double (the column
            // write then applies its own currency/float conversion).
            Money | Float => Ok(ExprValue::Double(self.get_as_double()?)),
            Text | Memo | Guid => Ok(ExprValue::Text(self.get_as_string()?)),
            ShortDateTime => Ok(ExprValue::date_time(self.get_as_local_date_time()?)),
            other => unsupported(format!(
                "coercion to column type {other:?} is not implemented"
            )),
        }
    }
}

impl Decimal {
    fn unscaled_is_zero(self) -> bool {
        self.unscaled == 0
    }
}

/// The `Value.Type` of an expression value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ValueKind {
    Null,
    Text,
    Date,
    Time,
    DateTime,
    Long,
    Double,
    Decimal,
}

impl ValueKind {
    fn is_numeric(self) -> bool {
        matches!(
            self,
            ValueKind::Long | ValueKind::Double | ValueKind::Decimal
        )
    }

    fn is_integral(self) -> bool {
        self == ValueKind::Long
    }

    fn is_temporal(self) -> bool {
        matches!(
            self,
            ValueKind::Date | ValueKind::Time | ValueKind::DateTime
        )
    }

    fn is_string(self) -> bool {
        self == ValueKind::Text
    }

    fn preferred_fp_type(self) -> ValueKind {
        if self <= ValueKind::Double {
            ValueKind::Double
        } else {
            ValueKind::Decimal
        }
    }

    fn preferred_numeric_type(self) -> Option<ValueKind> {
        if self.is_numeric() {
            return Some(self);
        }
        if self.is_temporal() {
            return Some(if self == ValueKind::Date {
                ValueKind::Long
            } else {
                ValueKind::Double
            });
        }
        None
    }
}

fn date_kind_of(value: LocalDateTime) -> DateKind {
    let has_date = !(value.year == 1899 && value.month == 12 && value.day == 30);
    let has_time = !(value.hour == 0 && value.minute == 0 && value.second == 0 && value.nano == 0);
    if has_date {
        if has_time {
            DateKind::DateTime
        } else {
            DateKind::Date
        }
    } else {
        DateKind::Time
    }
}

/// `BaseEvalContext.toValueType(DataType)`.
pub fn value_kind_for(target: DataType) -> ValueKind {
    use DataType::*;
    match target {
        Boolean | Byte | Int | Long => ValueKind::Long,
        Money | Float | Double => ValueKind::Double,
        ShortDateTime => ValueKind::DateTime,
        Numeric | BigInt => ValueKind::Decimal,
        _ => ValueKind::Text,
    }
}

// ---------------------------------------------------------------------------
// date helpers
// ---------------------------------------------------------------------------

/// `DefaultDateFunctions.MIN_DATE`.
const MIN_DATE: f64 = -657434.0;
/// `DefaultDateFunctions.MAX_DATE`.
const MAX_DATE: f64 = 2958465.999988426;

const BASE_YEAR: i32 = 1899;
const BASE_MONTH: u32 = 12;
const BASE_DAY: u32 = 30;

fn base_ldt() -> LocalDateTime {
    LocalDateTime::of_date(BASE_YEAR, BASE_MONTH, BASE_DAY)
}

fn valid_date_double(value: f64) -> bool {
    (MIN_DATE..=MAX_DATE).contains(&value)
}

/// `ColumnImpl.ldtFromLocalDateDouble`.
fn ldt_from_local_date_double(value: f64) -> LocalDateTime {
    let (seconds, nanos) = datenums::duration_from_local_date_double(value);
    let base_seconds = datenums::days_from_civil(BASE_YEAR, BASE_MONTH, BASE_DAY) * 86_400;
    let total = base_seconds + seconds;
    let days = total.div_euclid(86_400);
    let sec = total.rem_euclid(86_400);
    let mut ldt = LocalDateTime::from_epoch_day(days);
    ldt.hour = (sec / 3600) as u32;
    ldt.minute = ((sec % 3600) / 60) as u32;
    ldt.second = (sec % 60) as u32;
    ldt.nano = nanos as u32;
    ldt
}

fn number_to_date_value(value: f64) -> ExprResult<LocalDateTime> {
    if !valid_date_double(value) {
        return eval_err("date/time value is outside the valid range");
    }
    Ok(ldt_from_local_date_double(value))
}

/// `ValueSupport.toDateValueIfPossible`.
fn to_date_value_if_possible(kind: DateKind, value: f64) -> ExprValue {
    if valid_date_double(value) {
        ExprValue::DateTime {
            kind,
            value: ldt_from_local_date_double(value),
        }
    } else {
        ExprValue::Double(value)
    }
}

/// Java's `uuuu` year pattern (min width 4, `SignStyle.EXCEEDS_PAD`): a
/// negative year gets a `-` and zero padding, a positive year with more than
/// four digits gets an explicit `+`.
fn format_year(year: i32) -> String {
    if year < 0 {
        format!("-{:04}", -year)
    } else if year > 9999 {
        format!("+{year}")
    } else {
        format!("{year:04}")
    }
}

/// `ValueSupport.getDateFormatForType` using `TemporalConfig.US_TEMPORAL_CONFIG`.
fn format_date_time(kind: DateKind, value: LocalDateTime) -> String {
    format_general_date_us(kind, value)
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

enum IsoDate {
    NotIso,
    Invalid,
    Value(LocalDateTime),
}

/// Parse the supported ISO subset of a Jackcess date literal / date string.
fn parse_iso_date(text: &str) -> IsoDate {
    let text = text.trim();
    if text.is_empty() {
        return IsoDate::NotIso;
    }
    let parts: Vec<&str> = text.split_ascii_whitespace().collect();
    if parts.len() > 2 {
        return IsoDate::NotIso;
    }
    let (date_part, time_part) = match parts.len() {
        1 => {
            if parts[0].contains(':') {
                (None, Some(parts[0]))
            } else {
                (Some(parts[0]), None)
            }
        }
        2 => (Some(parts[0]), Some(parts[1])),
        _ => return IsoDate::NotIso,
    };

    let mut year = BASE_YEAR;
    let mut month = BASE_MONTH;
    let mut day = BASE_DAY;
    if let Some(date) = date_part {
        let separator = if date.contains('-') {
            '-'
        } else if date.contains('/') {
            '/'
        } else {
            return IsoDate::NotIso;
        };
        let fields: Vec<&str> = date.split(separator).collect();
        if fields.len() != 3 {
            return IsoDate::NotIso;
        }
        if fields[0].len() != 4
            || !fields[0].bytes().all(|b| b.is_ascii_digit())
            || fields[1].is_empty()
            || fields[1].len() > 2
            || !fields[1].bytes().all(|b| b.is_ascii_digit())
            || fields[2].is_empty()
            || fields[2].len() > 2
            || !fields[2].bytes().all(|b| b.is_ascii_digit())
        {
            return IsoDate::NotIso;
        }
        year = match fields[0].parse() {
            Ok(v) => v,
            Err(_) => return IsoDate::NotIso,
        };
        month = match fields[1].parse() {
            Ok(v) => v,
            Err(_) => return IsoDate::Invalid,
        };
        day = match fields[2].parse() {
            Ok(v) => v,
            Err(_) => return IsoDate::Invalid,
        };
        if month == 0 || month > 12 || day == 0 || day > days_in_month(year, month) {
            return IsoDate::Invalid;
        }
    }

    let mut hour = 0u32;
    let mut minute = 0u32;
    let mut second = 0u32;
    if let Some(time) = time_part {
        if time.contains('.') {
            return IsoDate::NotIso;
        }
        let fields: Vec<&str> = time.split(':').collect();
        if fields.len() < 2 || fields.len() > 3 {
            return IsoDate::NotIso;
        }
        let mut values = [0u32; 3];
        for (idx, field) in fields.iter().enumerate() {
            if field.is_empty() || field.len() > 2 || !field.bytes().all(|b| b.is_ascii_digit()) {
                return IsoDate::NotIso;
            }
            values[idx] = match field.parse() {
                Ok(v) => v,
                Err(_) => return IsoDate::Invalid,
            };
        }
        hour = values[0];
        minute = values[1];
        second = values[2];
        if hour > 23 || minute > 59 || second > 59 {
            return IsoDate::Invalid;
        }
    }

    IsoDate::Value(LocalDateTime::new(
        year, month, day, hour, minute, second, 0,
    ))
}

fn parse_date_literal(text: &str) -> ExprResult<ExprValue> {
    match parse_iso_date(text) {
        IsoDate::Value(ldt) => Ok(ExprValue::from_local_date_time(ldt)),
        IsoDate::Invalid => parse_err(format!("Invalid date/time literal '{text}'")),
        IsoDate::NotIso => {
            if looks_like_date(text) {
                // a locale date/time which Jackcess would read, but this port
                // deliberately does not guess
                unsupported(format!(
                    "date/time literal '{text}' (only the ISO forms yyyy-mm-dd, \
                     hh:mm:ss and yyyy-mm-dd hh:mm:ss are supported)"
                ))
            } else {
                // not a date at all, which Jackcess also rejects while parsing
                parse_err(format!("Invalid date/time literal '{text}'"))
            }
        }
    }
}

fn string_to_date_value(text: &str) -> ExprResult<Option<ExprValue>> {
    match parse_iso_date(text) {
        IsoDate::Value(ldt) => Ok(Some(ExprValue::from_local_date_time(ldt))),
        // Jackcess' DateTimeParser throws for a date/time whose fields are out
        // of range; stringToDateValue turns that into "not a date".
        IsoDate::Invalid => Ok(None),
        IsoDate::NotIso => {
            if looks_like_date(text) {
                Err(ExprError::Unsupported(format!(
                    "date/time string '{text}' (only the ISO forms yyyy-mm-dd, \
                     hh:mm:ss and yyyy-mm-dd hh:mm:ss are supported)"
                )))
            } else {
                Ok(None)
            }
        }
    }
}

/// Whether `text` uses a Jackcess date/time shape which this port does not
/// parse (locale dates such as `1/2/2020`, times such as `1.5`, month names,
/// am/pm, ...).  Plain numbers (including negative ones) are not date shapes.
fn looks_like_date(text: &str) -> bool {
    let bytes: Vec<char> = text.chars().collect();
    let has_digit = bytes.iter().any(|c| c.is_ascii_digit());
    if !has_digit {
        return false;
    }
    let between_digits = |sep: char| {
        bytes
            .windows(3)
            .any(|w| w[0].is_ascii_digit() && w[1] == sep && w[2].is_ascii_digit())
    };
    if between_digits('/')
        || between_digits('-')
        || between_digits(',')
        || between_digits(':')
        || between_digits('.')
    {
        return true;
    }
    let lower = text.to_ascii_lowercase();
    const MARKERS: [&str; 16] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec", "am",
        "pm", "a.m", "p.m",
    ];
    MARKERS.iter().any(|marker| lower.contains(marker))
}

// ---------------------------------------------------------------------------
// number/string conversions and formatting
// ---------------------------------------------------------------------------

fn parse_string_decimal(text: &str) -> ExprResult<Decimal> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return eval_err(format!("'{text}' cannot be converted to a number"));
    }
    if let Some(rest) = trimmed.strip_prefix('&') {
        if let Some(hex) = rest.strip_prefix(['h', 'H']) {
            if !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                let value = i128::from_str_radix(hex, 16).map_err(|_| {
                    ExprError::Unsupported(format!(
                        "hex literal '{text}' exceeds the 38 digit coefficient"
                    ))
                })?;
                return Ok(Decimal {
                    unscaled: value,
                    scale: 0,
                });
            }
        }
        if let Some(octal) = rest.strip_prefix(['o', 'O']) {
            if !octal.is_empty() && octal.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
                let value = i128::from_str_radix(octal, 8).map_err(|_| {
                    ExprError::Unsupported(format!(
                        "octal literal '{text}' exceeds the 38 digit coefficient"
                    ))
                })?;
                return Ok(Decimal {
                    unscaled: value,
                    scale: 0,
                });
            }
        }
        return eval_err(format!("'{text}' cannot be converted to a number"));
    }
    // US numeric config: grouping separator ',' is discarded
    let canonical: String = trimmed.chars().filter(|c| *c != ',').collect();
    match Decimal::parse(&canonical) {
        Ok(value) => Ok(value),
        Err(ExprError::Unsupported(message)) => Err(ExprError::Unsupported(message)),
        Err(_) => eval_err(format!("'{text}' cannot be converted to a number")),
    }
}

/// `Value.getAsString` for a `Double` (`NumberFormatter.format(double)`):
/// `new BigDecimal(d, MathContext(15, HALF_EVEN))` through the type formatter,
/// so the exact binary value of `d` is rounded to 15 significant digits (see
/// [`format_general_number_double`]).
fn format_double(value: f64) -> String {
    format_general_number_double(value)
}

fn java_double_cmp(a: f64, b: f64) -> Ordering {
    if a.is_nan() {
        return if b.is_nan() {
            Ordering::Equal
        } else {
            Ordering::Greater
        };
    }
    if b.is_nan() {
        return Ordering::Less;
    }
    match a.partial_cmp(&b) {
        Some(Ordering::Less) => Ordering::Less,
        Some(Ordering::Greater) => Ordering::Greater,
        _ => (a.to_bits() as i64).cmp(&(b.to_bits() as i64)),
    }
}

/// Java `String.compareToIgnoreCase`.
fn compare_ignore_case(a: &str, b: &str) -> Ordering {
    let mut left = a.chars();
    let mut right = b.chars();
    loop {
        match (left.next(), right.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(c1), Some(c2)) => {
                if c1 == c2 {
                    continue;
                }
                let u1 = c1.to_uppercase().next().unwrap_or(c1);
                let u2 = c2.to_uppercase().next().unwrap_or(c2);
                if u1 == u2 {
                    let l1 = c1.to_lowercase().next().unwrap_or(c1);
                    let l2 = c2.to_lowercase().next().unwrap_or(c2);
                    return l1.cmp(&l2);
                }
                return u1.cmp(&u2);
            }
        }
    }
}

fn compare_case_sensitive(a: &str, b: &str) -> Ordering {
    a.cmp(b)
}

/// Java's text functions index UTF-16 code units.  For BMP text a Rust
/// `char` is one code unit, so the port is exact; non-BMP text is rejected
/// rather than approximated.
fn has_non_bmp(text: &str) -> bool {
    text.chars().any(|c| (c as u32) > 0xFFFF)
}

fn ci_char_eq(a: char, b: char) -> bool {
    a == b || a.to_uppercase().eq(b.to_uppercase()) || a.to_lowercase().eq(b.to_lowercase())
}

// ---------------------------------------------------------------------------
// LIKE
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum LikeToken {
    Star,
    Any,
    Digit,
    Class {
        negated: bool,
        ranges: Vec<(char, char)>,
    },
    Literal(char),
}

fn compile_like(pattern: &str) -> ExprResult<Vec<LikeToken>> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut tokens = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' => {
                tokens.push(LikeToken::Star);
                i += 1;
            }
            '?' => {
                tokens.push(LikeToken::Any);
                i += 1;
            }
            '#' => {
                tokens.push(LikeToken::Digit);
                i += 1;
            }
            '[' => {
                let start = i + 1;
                let mut end = None;
                for (j, c) in chars.iter().enumerate().skip(start) {
                    if *c == ']' {
                        end = Some(j);
                        break;
                    }
                }
                let end = match end {
                    Some(end) => end,
                    // Access treats an unterminated class as unmatchable
                    None => return Ok(vec![LikeToken::Literal('\u{0}')]),
                };
                let mut class: Vec<char> = chars[start..end].to_vec();
                let mut negated = false;
                if let Some(first) = class.first().copied() {
                    if first == '!' || first == '^' {
                        negated = true;
                        class.remove(0);
                    }
                }
                if class.is_empty() {
                    // Java's regex "[]"/"[^]" is invalid -> never matches
                    return Ok(vec![LikeToken::Literal('\u{0}')]);
                }
                if class.contains(&'\\') {
                    return unsupported(
                        "LIKE character class escapes are not implemented".to_string(),
                    );
                }
                // Java regex also gives these constructs a special meaning
                // inside a character class; do not quietly treat them as
                // literals.
                if class.contains(&'[') || class.windows(2).any(|pair| pair == ['&', '&']) {
                    return unsupported(
                        "nested/intersected LIKE character classes are not implemented".to_string(),
                    );
                }
                let mut ranges = Vec::new();
                let mut k = 0;
                while k < class.len() {
                    if k + 2 < class.len() && class[k + 1] == '-' {
                        if class[k] > class[k + 2] {
                            // invalid java regex range -> unmatchable
                            return Ok(vec![LikeToken::Literal('\u{0}')]);
                        }
                        ranges.push((class[k], class[k + 2]));
                        k += 3;
                    } else {
                        ranges.push((class[k], class[k]));
                        k += 1;
                    }
                }
                tokens.push(LikeToken::Class { negated, ranges });
                i = end + 1;
            }
            c => {
                tokens.push(LikeToken::Literal(c));
                i += 1;
            }
        }
    }
    Ok(tokens)
}

fn like_class_matches(negated: bool, ranges: &[(char, char)], c: char) -> bool {
    let mut variants = vec![c];
    if let Some(u) = c.to_uppercase().next() {
        variants.push(u);
    }
    if let Some(l) = c.to_lowercase().next() {
        variants.push(l);
    }
    let found = variants
        .iter()
        .any(|v| ranges.iter().any(|(lo, hi)| v >= lo && v <= hi));
    found != negated
}

fn like_matches_tokens(tokens: &[LikeToken], text: &[char]) -> bool {
    // dynamic programming over (token index, text index)
    let mut memo = vec![vec![None; text.len() + 1]; tokens.len() + 1];
    fn go(
        tokens: &[LikeToken],
        text: &[char],
        ti: usize,
        si: usize,
        memo: &mut Vec<Vec<Option<bool>>>,
    ) -> bool {
        if let Some(value) = memo[ti][si] {
            return value;
        }
        let result = if ti == tokens.len() {
            si == text.len()
        } else {
            match &tokens[ti] {
                LikeToken::Star => {
                    // try consuming zero or more characters
                    let mut found = go(tokens, text, ti + 1, si, memo);
                    let mut idx = si;
                    while !found && idx < text.len() {
                        idx += 1;
                        found = go(tokens, text, ti + 1, idx, memo);
                    }
                    found
                }
                LikeToken::Any => si < text.len() && go(tokens, text, ti + 1, si + 1, memo),
                LikeToken::Digit => {
                    si < text.len()
                        && text[si].is_ascii_digit()
                        && go(tokens, text, ti + 1, si + 1, memo)
                }
                LikeToken::Class { negated, ranges } => {
                    si < text.len()
                        && like_class_matches(*negated, ranges, text[si])
                        && go(tokens, text, ti + 1, si + 1, memo)
                }
                LikeToken::Literal(expected) => {
                    si < text.len()
                        && ci_char_eq(*expected, text[si])
                        && go(tokens, text, ti + 1, si + 1, memo)
                }
            }
        };
        memo[ti][si] = Some(result);
        result
    }
    go(tokens, text, 0, 0, &mut memo)
}

/// Match an Access `LIKE` pattern the way `Expressionator.likePatternToRegex`
/// plus `Pattern.matches()` does for the supported subset.
pub fn like_match(pattern: &str, text: &str) -> ExprResult<bool> {
    let tokens = compile_like(pattern)?;
    // The `'\0'` sentinel used for "unmatchable" can never match real text.
    if tokens.len() == 1 && matches!(tokens[0], LikeToken::Literal('\u{0}')) {
        return Ok(text.contains('\u{0}'));
    }
    let chars: Vec<char> = text.chars().collect();
    Ok(like_matches_tokens(&tokens, &chars))
}

// ---------------------------------------------------------------------------
// tokenizer
// ---------------------------------------------------------------------------

/// `Expressionator.Type`: the context an expression is parsed in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExprMode {
    /// A calculated column / ordinary expression.
    Expression,
    /// A `=...` default value (leading `=` and literal defaults allowed).
    DefaultValue,
    /// A field validation rule (`[ThisColumn]` implicit left value).
    FieldValidator,
    /// A record validation rule (condition).
    RowValidator,
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    /// `+ - * / \ ^ &` and the comparison operators
    Op(String),
    /// `. ! , ( )`
    Delim(char),
    /// A bare word, including keywords and column names.
    Word(String),
    /// `[Object Name]`
    ObjName(String),
    /// A parsed literal.
    Literal(ExprValue),
}

fn is_op_char(c: char) -> bool {
    matches!(c, '+' | '-' | '*' | '/' | '\\' | '^' | '&')
}

fn is_comp_char(c: char) -> bool {
    matches!(c, '<' | '>' | '=')
}

fn is_delim_char(c: char) -> bool {
    matches!(c, '.' | '!' | ',' | '(' | ')')
}

fn is_space_char(c: char) -> bool {
    matches!(c, ' ' | '\n' | '\r' | '\t')
}

fn is_special_char(c: char) -> bool {
    is_op_char(c)
        || is_comp_char(c)
        || is_delim_char(c)
        || is_space_char(c)
        || matches!(c, '"' | '\'' | '#' | '[' | ']')
}

fn tokenize(text: &str, mode: ExprMode) -> ExprResult<Vec<Token>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return parse_err("null/empty expression");
    }
    let chars: Vec<char> = trimmed.chars().collect();
    let mut tokens = Vec::new();
    let mut pos = 0;
    while pos < chars.len() {
        let c = chars[pos];
        if is_op_char(c) {
            tokens.push(Token::Op(c.to_string()));
            pos += 1;
        } else if is_comp_char(c) {
            // only <, <=, <>, >, >=, = are recognized; other combinations are
            // two separate operators like Jackcess' two-char lookup
            let mut op = c.to_string();
            if pos + 1 < chars.len() {
                let next = chars[pos + 1];
                let combo = format!("{c}{next}");
                if matches!(combo.as_str(), "<=" | ">=" | "<>") {
                    op = combo;
                    pos += 1;
                }
            }
            tokens.push(Token::Op(op));
            pos += 1;
        } else if is_delim_char(c) {
            tokens.push(Token::Delim(c));
            pos += 1;
        } else if is_space_char(c) {
            while pos < chars.len() && is_space_char(chars[pos]) {
                pos += 1;
            }
        } else if c == '"' || c == '\'' {
            pos += 1;
            let mut value = String::new();
            let mut closed = false;
            while pos < chars.len() {
                let ch = chars[pos];
                if ch == c {
                    if pos + 1 < chars.len() && chars[pos + 1] == c {
                        value.push(c);
                        pos += 2;
                    } else {
                        pos += 1;
                        closed = true;
                        break;
                    }
                } else {
                    value.push(ch);
                    pos += 1;
                }
            }
            if !closed {
                return parse_err(format!("Missing closing '{c}' for quoted string"));
            }
            tokens.push(Token::Literal(ExprValue::Text(value)));
        } else if c == '#' {
            pos += 1;
            let mut value = String::new();
            let mut closed = false;
            while pos < chars.len() {
                let ch = chars[pos];
                pos += 1;
                if ch == '#' {
                    closed = true;
                    break;
                }
                value.push(ch);
            }
            if !closed {
                return parse_err("Missing closing '#' for date literal".to_string());
            }
            tokens.push(Token::Literal(parse_date_literal(&value)?));
        } else if c == '[' {
            pos += 1;
            let mut value = String::new();
            let mut closed = false;
            while pos < chars.len() {
                let ch = chars[pos];
                pos += 1;
                if ch == ']' {
                    closed = true;
                    break;
                }
                if ch == '[' {
                    return parse_err("Missing closing ']' for quoted string");
                }
                value.push(ch);
            }
            if !closed {
                return parse_err("Missing closing ']' for quoted string");
            }
            tokens.push(Token::ObjName(value));
        } else if c == ']' {
            return parse_err("Invalid leading quote character ]");
        } else if c.is_ascii_digit() {
            if let Some(token) = try_number_literal(&chars, &mut pos)? {
                tokens.push(token);
            } else {
                tokens.push(Token::Word(parse_bare_word(&chars, &mut pos, mode)));
            }
        } else {
            tokens.push(Token::Word(parse_bare_word(&chars, &mut pos, mode)));
        }
    }
    Ok(tokens)
}

/// Try to read a number literal starting at `pos`; on success advance `pos`,
/// otherwise leave it unchanged so the caller can read a bare word.
fn try_number_literal(chars: &[char], pos: &mut usize) -> ExprResult<Option<Token>> {
    let start = *pos;
    let mut i = start;
    let mut digits = String::new();
    let mut is_fp = false;
    let mut has_digit = false;
    let mut exp_pos: Option<usize> = None;
    while i < chars.len() {
        let c = chars[i];
        if c.is_ascii_digit() {
            has_digit = true;
            digits.push(c);
            i += 1;
        } else if c == '.' {
            is_fp = true;
            digits.push('.');
            i += 1;
        } else if has_digit && exp_pos.is_none() && (c == 'e' || c == 'E') {
            is_fp = true;
            digits.push(c);
            exp_pos = Some(digits.len());
            i += 1;
        } else if exp_pos == Some(digits.len()) && (c == '-' || c == '+') {
            digits.push(c);
            i += 1;
        } else if is_special_char(c) {
            break;
        } else {
            // a non-number, non-special character: not a numeric literal
            return Ok(None);
        }
    }
    if !has_digit {
        return Ok(None);
    }
    let text = digits;
    if !is_fp {
        if let Ok(value) = text.parse::<i32>() {
            *pos = i;
            return Ok(Some(Token::Literal(ExprValue::Long(value))));
        }
    }
    let value = Decimal::parse(&text)?;
    *pos = i;
    Ok(Some(Token::Literal(ExprValue::Decimal(value))))
}

fn parse_bare_word(chars: &[char], pos: &mut usize, mode: ExprMode) -> String {
    let mut word = String::new();
    while *pos < chars.len() {
        let c = chars[*pos];
        let stop = is_special_char(c) || (mode == ExprMode::FieldValidator && is_comp_char(c));
        if stop {
            break;
        }
        word.push(c);
        *pos += 1;
    }
    word
}

// ---------------------------------------------------------------------------
// AST
// ---------------------------------------------------------------------------

/// An object reference (`[Table].[Column]` etc.).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identifier {
    /// The optional collection/table name.
    pub collection: Option<String>,
    /// The column name.
    pub object: String,
    /// The optional property name.
    pub property: Option<String>,
}

impl fmt::Display for Identifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(collection) = &self.collection {
            write!(f, "[{collection}].")?;
        }
        write!(f, "[{}]", self.object)?;
        if let Some(property) = &self.property {
            write!(f, ".[{property}]")?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UnaryOp {
    Neg,
    Pos,
    NegNum,
    PosNum,
    Not,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BinaryOp {
    Plus,
    Minus,
    Multiply,
    Divide,
    IntDivide,
    Exp,
    Concat,
    Mod,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CompareOp {
    Lt,
    Lte,
    Gt,
    Gte,
    Eq,
    Ne,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LogicalOp {
    And,
    Or,
    Xor,
    Eqv,
    Imp,
}

#[derive(Clone, Debug)]
enum Expr {
    Literal(ExprValue),
    Column(Identifier),
    ThisColumn,
    Unary(UnaryOp, Box<Expr>),
    Binary(BinaryOp, Box<Expr>, Box<Expr>),
    Compare(CompareOp, Box<Expr>, Box<Expr>),
    Logical(LogicalOp, Box<Expr>, Box<Expr>),
    IsNull {
        value: Box<Expr>,
        negated: bool,
    },
    Like {
        value: Box<Expr>,
        pattern: Box<Expr>,
        negated: bool,
    },
    Between {
        value: Box<Expr>,
        start: Box<Expr>,
        end: Box<Expr>,
        negated: bool,
    },
    In {
        value: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    Func {
        name: String,
        args: Vec<Expr>,
        non_variant: bool,
    },
    Paren(Box<Expr>),
    ImplicitCompare(Box<Expr>),
}

impl Expr {
    fn is_validation_expression(&self) -> bool {
        match self {
            Expr::Compare(..)
            | Expr::Logical(..)
            | Expr::IsNull { .. }
            | Expr::Like { .. }
            | Expr::Between { .. }
            | Expr::In { .. }
            | Expr::ImplicitCompare(_) => true,
            Expr::Paren(inner) => inner.is_validation_expression(),
            _ => false,
        }
    }

    fn is_constant(&self) -> bool {
        match self {
            Expr::Literal(_) => true,
            Expr::ThisColumn => false,
            Expr::Column(_) => false,
            Expr::Unary(_, value) => value.is_constant(),
            Expr::Binary(_, left, right)
            | Expr::Compare(_, left, right)
            | Expr::Logical(_, left, right) => left.is_constant() && right.is_constant(),
            Expr::IsNull { value, .. } => value.is_constant(),
            Expr::Like { value, pattern, .. } => value.is_constant() && pattern.is_constant(),
            Expr::Between {
                value, start, end, ..
            } => value.is_constant() && start.is_constant() && end.is_constant(),
            Expr::In { value, list, .. } => {
                value.is_constant() && list.iter().all(Expr::is_constant)
            }
            Expr::Func { name, args, .. } => {
                !is_impure_function(name) && args.iter().all(Expr::is_constant)
            }
            Expr::Paren(inner) | Expr::ImplicitCompare(inner) => inner.is_constant(),
        }
    }

    fn collect_identifiers(&self, out: &mut Vec<Identifier>) {
        match self {
            Expr::Column(identifier) => out.push(identifier.clone()),
            Expr::Unary(_, value) | Expr::IsNull { value, .. } => value.collect_identifiers(out),
            Expr::Binary(_, left, right)
            | Expr::Compare(_, left, right)
            | Expr::Logical(_, left, right) => {
                left.collect_identifiers(out);
                right.collect_identifiers(out);
            }
            Expr::Like { value, pattern, .. } => {
                value.collect_identifiers(out);
                pattern.collect_identifiers(out);
            }
            Expr::Between {
                value, start, end, ..
            } => {
                value.collect_identifiers(out);
                start.collect_identifiers(out);
                end.collect_identifiers(out);
            }
            Expr::In { value, list, .. } => {
                value.collect_identifiers(out);
                for item in list {
                    item.collect_identifiers(out);
                }
            }
            Expr::Func { args, .. } => {
                for arg in args {
                    arg.collect_identifiers(out);
                }
            }
            Expr::Paren(inner) | Expr::ImplicitCompare(inner) => inner.collect_identifiers(out),
            Expr::Literal(_) | Expr::ThisColumn => {}
        }
    }
}

// ---------------------------------------------------------------------------
// parser
// ---------------------------------------------------------------------------

const P_NEG_NUM: i32 = 0;
const P_EXP: i32 = 1;
const P_UNARY: i32 = 2;
const P_MUL: i32 = 3;
const P_INT_DIV: i32 = 4;
const P_MOD: i32 = 5;
const P_ADD: i32 = 6;
const P_CONCAT: i32 = 7;
const P_COMP: i32 = 8;
const P_NOT: i32 = 9;
const P_AND: i32 = 10;
const P_OR: i32 = 11;
const P_XOR: i32 = 12;
const P_EQV: i32 = 13;
const P_IMP: i32 = 14;
const P_SPECIAL: i32 = 15;

#[derive(Clone, Copy)]
struct InfixOp {
    precedence: i32,
    kind: InfixKind,
}

#[derive(Clone, Copy)]
enum InfixKind {
    Binary(BinaryOp),
    Compare(CompareOp),
    Logical(LogicalOp),
    IsNull,
    Like,
    NotLike,
    Between,
    NotBetween,
    In,
    NotIn,
}

fn lower(word: &str) -> String {
    word.to_ascii_lowercase()
}

fn is_constant_word(word: &str) -> bool {
    matches!(
        lower(word).as_str(),
        "true" | "false" | "yes" | "no" | "on" | "off" | "null"
    )
}

fn constant_value(word: &str) -> ExprValue {
    match lower(word).as_str() {
        "true" | "yes" | "on" => ExprValue::Long(-1),
        "false" | "no" | "off" => ExprValue::Long(0),
        _ => ExprValue::Null,
    }
}

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
    mode: ExprMode,
}

impl<'a> Parser<'a> {
    fn new(tokens: &'a [Token], mode: ExprMode) -> Parser<'a> {
        Parser {
            tokens,
            pos: 0,
            mode,
        }
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn peek_at(&self, offset: usize) -> Option<&Token> {
        self.tokens.get(self.pos + offset)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.pos).cloned();
        if token.is_some() {
            self.pos += 1;
        }
        token
    }

    fn peek_word(&self, word: &str) -> bool {
        matches!(self.peek(), Some(Token::Word(w)) if w.eq_ignore_ascii_case(word))
    }

    fn eat_delim(&mut self, delim: char) -> bool {
        if matches!(self.peek(), Some(Token::Delim(c)) if *c == delim) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_delim(&mut self, delim: char) -> ExprResult<()> {
        if self.eat_delim(delim) {
            Ok(())
        } else {
            parse_err(format!("Missing closing '{delim}'"))
        }
    }

    fn expect_word(&mut self, word: &str) -> ExprResult<()> {
        if self.peek_word(word) {
            self.pos += 1;
            Ok(())
        } else {
            parse_err(format!("Expected '{word}'"))
        }
    }

    fn parse_expression(&mut self) -> ExprResult<Expr> {
        let expr =
            if self.mode == ExprMode::FieldValidator && self.starts_with_implicit_this_operator() {
                self.parse_binary_tail(Expr::ThisColumn, i32::MAX, false)?
            } else {
                self.parse_expr(i32::MAX, false)?
            };
        if let Some(token) = self.peek() {
            return parse_err(format!(
                "Unexpected token '{}' at end of expression",
                token_text(token)
            ));
        }
        if self.mode == ExprMode::FieldValidator && !expr.is_validation_expression() {
            return Ok(Expr::ImplicitCompare(Box::new(expr)));
        }
        Ok(expr)
    }

    fn starts_with_implicit_this_operator(&self) -> bool {
        match self.peek() {
            Some(Token::Op(op)) => matches!(op.as_str(), "<" | "<=" | ">" | ">=" | "=" | "<>"),
            Some(Token::Word(word)) => {
                let word = lower(word);
                if matches!(word.as_str(), "like" | "between" | "in" | "is") {
                    return true;
                }
                if word == "not" {
                    if let Some(Token::Word(next)) = self.peek_at(1) {
                        return matches!(lower(next).as_str(), "like" | "between" | "in");
                    }
                }
                false
            }
            _ => false,
        }
    }

    fn parse_expr(&mut self, min_precedence: i32, stop_at_and: bool) -> ExprResult<Expr> {
        let left = self.parse_prefix()?;
        self.parse_binary_tail(left, min_precedence, stop_at_and)
    }

    fn parse_binary_tail(
        &mut self,
        mut left: Expr,
        min_precedence: i32,
        stop_at_and: bool,
    ) -> ExprResult<Expr> {
        loop {
            if self.peek().is_none() {
                break;
            }
            if stop_at_and && self.peek_word("and") {
                break;
            }
            let infix = match self.peek_infix() {
                Some(infix) => infix,
                None => break,
            };
            if infix.precedence >= min_precedence {
                break;
            }
            left = self.parse_infix(left, infix)?;
        }
        Ok(left)
    }

    fn parse_prefix(&mut self) -> ExprResult<Expr> {
        let token = match self.next() {
            Some(token) => token,
            None => return parse_err("Unexpected end of expression"),
        };
        match token {
            Token::Literal(value) => Ok(Expr::Literal(value)),
            Token::Delim('(') => {
                let inner = self.parse_expr(i32::MAX, false)?;
                self.expect_delim(')')?;
                Ok(Expr::Paren(Box::new(inner)))
            }
            Token::Delim(other) => parse_err(format!("Unexpected delimiter '{other}'")),
            Token::ObjName(name) => self.parse_object_ref(name),
            Token::Op(op) if op == "-" || op == "+" => {
                let numeric_literal = matches!(
                    self.peek(),
                    Some(Token::Literal(value)) if value.kind().is_numeric()
                );
                let (op, precedence) = if op == "-" {
                    if numeric_literal {
                        (UnaryOp::NegNum, P_NEG_NUM)
                    } else {
                        (UnaryOp::Neg, P_UNARY)
                    }
                } else if numeric_literal {
                    (UnaryOp::PosNum, P_NEG_NUM)
                } else {
                    (UnaryOp::Pos, P_UNARY)
                };
                let value = self.parse_expr(precedence, false)?;
                Ok(Expr::Unary(op, Box::new(value)))
            }
            Token::Op(op) => parse_err(format!("Unexpected operator '{op}'")),
            Token::Word(word) => self.parse_word_prefix(word),
        }
    }

    fn parse_word_prefix(&mut self, word: String) -> ExprResult<Expr> {
        let lowered = lower(&word);
        if is_constant_word(&word) {
            return Ok(Expr::Literal(constant_value(&word)));
        }
        if lowered == "not" {
            match self.peek() {
                Some(Token::Word(next))
                    if matches!(lower(next).as_str(), "like" | "between" | "in") =>
                {
                    return parse_err("Missing left expression for comparison operator 'Not'");
                }
                _ => {}
            }
            let value = self.parse_expr(P_NOT, false)?;
            return Ok(Expr::Unary(UnaryOp::Not, Box::new(value)));
        }
        if matches!(
            lowered.as_str(),
            "mod" | "and" | "or" | "xor" | "eqv" | "imp"
        ) {
            return parse_err(format!("Missing left expression for operator '{}'", word));
        }
        if matches!(lowered.as_str(), "is" | "like" | "between" | "in") {
            return parse_err(format!(
                "Missing left expression for comparison operator '{}'",
                word
            ));
        }
        if matches!(self.peek(), Some(Token::Delim('('))) {
            return self.parse_function_call(word);
        }
        self.parse_object_ref(word)
    }

    fn parse_function_call(&mut self, name: String) -> ExprResult<Expr> {
        let (base_name, non_variant) = match name.strip_suffix('$') {
            Some(base) => (base.to_string(), true),
            None => (name.clone(), false),
        };
        let upper = base_name.to_ascii_uppercase();
        if non_variant && !has_string_variant(&upper) {
            return parse_err(format!("Could not find function '{name}'"));
        }
        if !is_known_function(&upper) {
            return parse_err(format!("Could not find function '{name}'"));
        }
        self.expect_delim('(')?;
        let mut args = Vec::new();
        if !self.eat_delim(')') {
            loop {
                args.push(self.parse_expr(i32::MAX, false)?);
                if self.eat_delim(',') {
                    continue;
                }
                if self.eat_delim(')') {
                    break;
                }
                return parse_err("Missing closing ')' for function call");
            }
        }
        Ok(Expr::Func {
            name: upper,
            args,
            non_variant,
        })
    }

    fn parse_object_ref(&mut self, first: String) -> ExprResult<Expr> {
        let mut names = vec![first];
        loop {
            let is_separator = matches!(
                self.peek(),
                Some(Token::Delim('.')) | Some(Token::Delim('!'))
            );
            if !is_separator {
                break;
            }
            self.pos += 1;
            match self.next() {
                Some(Token::Word(name)) | Some(Token::ObjName(name)) => names.push(name),
                _ => return parse_err("Invalid object reference"),
            }
        }
        if names.len() > 3 {
            return parse_err("Invalid object reference");
        }
        let (collection, object, property) = match names.len() {
            1 => (None, names.remove(0), None),
            2 => {
                let object = names.remove(1);
                (Some(names.remove(0)), object, None)
            }
            3 => {
                let property = names.remove(2);
                let object = names.remove(1);
                (Some(names.remove(0)), object, Some(property))
            }
            _ => unreachable!(),
        };
        Ok(Expr::Column(Identifier {
            collection,
            object,
            property,
        }))
    }

    fn peek_infix(&self) -> Option<InfixOp> {
        let token = self.peek()?;
        match token {
            Token::Op(op) => {
                let kind = match op.as_str() {
                    "+" => InfixKind::Binary(BinaryOp::Plus),
                    "-" => InfixKind::Binary(BinaryOp::Minus),
                    "*" => InfixKind::Binary(BinaryOp::Multiply),
                    "/" => InfixKind::Binary(BinaryOp::Divide),
                    "\\" => InfixKind::Binary(BinaryOp::IntDivide),
                    "^" => InfixKind::Binary(BinaryOp::Exp),
                    "&" => InfixKind::Binary(BinaryOp::Concat),
                    "<" => InfixKind::Compare(CompareOp::Lt),
                    "<=" => InfixKind::Compare(CompareOp::Lte),
                    ">" => InfixKind::Compare(CompareOp::Gt),
                    ">=" => InfixKind::Compare(CompareOp::Gte),
                    "=" => InfixKind::Compare(CompareOp::Eq),
                    "<>" => InfixKind::Compare(CompareOp::Ne),
                    _ => return None,
                };
                Some(InfixOp {
                    precedence: precedence_of(kind),
                    kind,
                })
            }
            Token::Word(word) => {
                let word = lower(word);
                let kind = match word.as_str() {
                    "mod" => InfixKind::Binary(BinaryOp::Mod),
                    "and" => InfixKind::Logical(LogicalOp::And),
                    "or" => InfixKind::Logical(LogicalOp::Or),
                    "xor" => InfixKind::Logical(LogicalOp::Xor),
                    "eqv" => InfixKind::Logical(LogicalOp::Eqv),
                    "imp" => InfixKind::Logical(LogicalOp::Imp),
                    "like" => InfixKind::Like,
                    "between" => InfixKind::Between,
                    "in" => InfixKind::In,
                    "is" => InfixKind::IsNull,
                    "not" => {
                        let next = self.peek_at(1)?;
                        match next {
                            Token::Word(next) => match lower(next).as_str() {
                                "like" => InfixKind::NotLike,
                                "between" => InfixKind::NotBetween,
                                "in" => InfixKind::NotIn,
                                _ => return None,
                            },
                            _ => return None,
                        }
                    }
                    _ => return None,
                };
                Some(InfixOp {
                    precedence: precedence_of(kind),
                    kind,
                })
            }
            _ => None,
        }
    }

    fn parse_infix(&mut self, left: Expr, infix: InfixOp) -> ExprResult<Expr> {
        match infix.kind {
            InfixKind::Binary(op) => {
                self.pos += 1;
                let right = self.parse_expr(infix.precedence, false)?;
                Ok(Expr::Binary(op, Box::new(left), Box::new(right)))
            }
            InfixKind::Compare(op) => {
                self.pos += 1;
                let right = self.parse_expr(infix.precedence, false)?;
                Ok(Expr::Compare(op, Box::new(left), Box::new(right)))
            }
            InfixKind::Logical(op) => {
                self.pos += 1;
                let right = self.parse_expr(infix.precedence, false)?;
                Ok(Expr::Logical(op, Box::new(left), Box::new(right)))
            }
            InfixKind::IsNull => {
                self.pos += 1; // is
                let negated = if self.peek_word("not") {
                    self.pos += 1;
                    true
                } else {
                    false
                };
                self.expect_word("null")?;
                Ok(Expr::IsNull {
                    value: Box::new(left),
                    negated,
                })
            }
            InfixKind::Like | InfixKind::NotLike => {
                let negated = matches!(infix.kind, InfixKind::NotLike);
                if negated {
                    self.pos += 1; // not
                }
                self.expect_word("like")?;
                let pattern = self.parse_expr(infix.precedence, false)?;
                Ok(Expr::Like {
                    value: Box::new(left),
                    pattern: Box::new(pattern),
                    negated,
                })
            }
            InfixKind::Between | InfixKind::NotBetween => {
                let negated = matches!(infix.kind, InfixKind::NotBetween);
                if negated {
                    self.pos += 1; // not
                }
                self.expect_word("between")?;
                let start = self.parse_expr(i32::MAX, true)?;
                self.expect_word("and")?;
                let end = self.parse_expr(P_SPECIAL, false)?;
                Ok(Expr::Between {
                    value: Box::new(left),
                    start: Box::new(start),
                    end: Box::new(end),
                    negated,
                })
            }
            InfixKind::In | InfixKind::NotIn => {
                let negated = matches!(infix.kind, InfixKind::NotIn);
                if negated {
                    self.pos += 1; // not
                }
                self.expect_word("in")?;
                if !self.eat_delim('(') {
                    return parse_err("Malformed 'In' expression");
                }
                let mut list = Vec::new();
                if !self.eat_delim(')') {
                    loop {
                        list.push(self.parse_expr(i32::MAX, false)?);
                        if self.eat_delim(',') {
                            continue;
                        }
                        if self.eat_delim(')') {
                            break;
                        }
                        return parse_err("Malformed 'In' expression");
                    }
                }
                Ok(Expr::In {
                    value: Box::new(left),
                    list,
                    negated,
                })
            }
        }
    }
}

fn precedence_of(kind: InfixKind) -> i32 {
    match kind {
        InfixKind::Binary(BinaryOp::Exp) => P_EXP,
        InfixKind::Binary(BinaryOp::Multiply) | InfixKind::Binary(BinaryOp::Divide) => P_MUL,
        InfixKind::Binary(BinaryOp::IntDivide) => P_INT_DIV,
        InfixKind::Binary(BinaryOp::Mod) => P_MOD,
        InfixKind::Binary(BinaryOp::Plus) | InfixKind::Binary(BinaryOp::Minus) => P_ADD,
        InfixKind::Binary(BinaryOp::Concat) => P_CONCAT,
        InfixKind::Compare(_) | InfixKind::Like | InfixKind::NotLike | InfixKind::IsNull => P_COMP,
        InfixKind::Logical(LogicalOp::And) => P_AND,
        InfixKind::Logical(LogicalOp::Or) => P_OR,
        InfixKind::Logical(LogicalOp::Xor) => P_XOR,
        InfixKind::Logical(LogicalOp::Eqv) => P_EQV,
        InfixKind::Logical(LogicalOp::Imp) => P_IMP,
        InfixKind::In | InfixKind::NotIn | InfixKind::Between | InfixKind::NotBetween => P_SPECIAL,
    }
}

fn token_text(token: &Token) -> String {
    match token {
        Token::Op(op) => op.clone(),
        Token::Delim(c) => c.to_string(),
        Token::Word(word) => word.clone(),
        Token::ObjName(name) => format!("[{name}]"),
        Token::Literal(value) => match value {
            ExprValue::Null => "Null".to_string(),
            ExprValue::Text(text) => format!("\"{text}\""),
            ExprValue::Byte(v) => v.to_string(),
            ExprValue::Int(v) => v.to_string(),
            ExprValue::Long(v) => v.to_string(),
            ExprValue::BigInt(v) => v.to_string(),
            ExprValue::Double(v) => v.to_string(),
            ExprValue::Decimal(v) => v.to_plain_string(),
            ExprValue::Bool(b) => b.to_string(),
            ExprValue::DateTime { .. } => "#date#".to_string(),
        },
    }
}

// ---------------------------------------------------------------------------
// operators
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Coercion {
    Simple,
    General,
    Compare,
}

impl Coercion {
    fn prefer_temporal(self) -> bool {
        matches!(self, Coercion::Simple)
    }

    fn allow_string_to_num(self) -> bool {
        !matches!(self, Coercion::Compare)
    }
}

fn any_null2(a: &ExprValue, b: &ExprValue) -> bool {
    a.is_null() || b.is_null()
}

fn preferred_numeric_type(a: ValueKind, b: ValueKind) -> ValueKind {
    if a.is_integral() && b.is_integral() {
        return a.max(b);
    }
    a.preferred_fp_type().max(b.preferred_fp_type())
}

fn coerce_string_to_numeric(
    a: &ExprValue,
    b: &ExprValue,
    coercion: Coercion,
) -> ExprResult<Option<ValueKind>> {
    let t1 = a.kind();
    let t2 = b.kind();
    let (pref_type, string_value) = if t1.is_numeric() {
        (t1, b)
    } else if t2.is_numeric() {
        (t2, a)
    } else if t1.is_temporal() {
        (
            if coercion.prefer_temporal() {
                t1
            } else {
                t1.preferred_numeric_type().unwrap_or(t1)
            },
            b,
        )
    } else if t2.is_temporal() {
        (
            if coercion.prefer_temporal() {
                t2
            } else {
                t2.preferred_numeric_type().unwrap_or(t2)
            },
            a,
        )
    } else {
        return Ok(None);
    };
    if string_value.get_as_decimal().is_err() {
        return Ok(None);
    }
    let kind = if pref_type.is_numeric() {
        if pref_type == ValueKind::Decimal {
            ValueKind::Decimal
        } else {
            ValueKind::Double
        }
    } else {
        pref_type
    };
    Ok(Some(kind))
}

fn get_math_type(a: &ExprValue, b: &ExprValue, coercion: Coercion) -> ExprResult<ValueKind> {
    let t1 = a.kind();
    let t2 = b.kind();
    if t1 == t2 {
        if !coercion.prefer_temporal() && t1.is_temporal() {
            return t1
                .preferred_numeric_type()
                .ok_or_else(|| ExprError::Eval("unexpected temporal type".into()));
        }
        return Ok(t1);
    }
    if t1.is_string() || t2.is_string() {
        if coercion.allow_string_to_num() {
            if let Some(kind) = coerce_string_to_numeric(a, b, coercion)? {
                return Ok(kind);
            }
        }
        // string always wins
        return Ok(ValueKind::Text);
    }
    if coercion.prefer_temporal() && (t1.is_temporal() || t2.is_temporal()) {
        return Ok(if t1.is_temporal() {
            if t2.is_temporal() {
                ValueKind::DateTime
            } else {
                t1
            }
        } else {
            t2
        });
    }
    let p1 = t1
        .preferred_numeric_type()
        .ok_or_else(|| ExprError::Eval(format!("Unexpected type {:?}", t1)))?;
    let p2 = t2
        .preferred_numeric_type()
        .ok_or_else(|| ExprError::Eval(format!("Unexpected type {:?}", t2)))?;
    Ok(preferred_numeric_type(p1, p2))
}

fn date_kind_of_value(kind: ValueKind) -> Option<DateKind> {
    match kind {
        ValueKind::Date => Some(DateKind::Date),
        ValueKind::Time => Some(DateKind::Time),
        ValueKind::DateTime => Some(DateKind::DateTime),
        _ => None,
    }
}

fn negate(value: &ExprValue) -> ExprResult<ExprValue> {
    if value.is_null() {
        return Ok(ExprValue::Null);
    }
    match value.kind() {
        ValueKind::Date | ValueKind::Time | ValueKind::DateTime => {
            let kind = date_kind_of_value(value.kind()).unwrap();
            let result = -value.get_as_double()?;
            Ok(to_date_value_if_possible(kind, result))
        }
        ValueKind::Long => Ok(ExprValue::Long(value.get_as_long()?.wrapping_neg())),
        ValueKind::Double => Ok(ExprValue::Double(-value.get_as_double()?)),
        ValueKind::Decimal | ValueKind::Text => {
            Ok(ExprValue::Decimal(value.get_as_decimal()?.negate()))
        }
        _ => eval_err("Unexpected type for unary '-'"),
    }
}

fn add_values(a: &ExprValue, b: &ExprValue) -> ExprResult<ExprValue> {
    if any_null2(a, b) {
        return Ok(ExprValue::Null);
    }
    match get_math_type(a, b, Coercion::Simple)? {
        ValueKind::Text => concat_non_null(a, b),
        kind @ (ValueKind::Date | ValueKind::Time | ValueKind::DateTime) => {
            let date_kind = date_kind_of_value(kind).unwrap();
            let result = a.get_as_double()? + b.get_as_double()?;
            Ok(to_date_value_if_possible(date_kind, result))
        }
        ValueKind::Long => Ok(ExprValue::Long(
            a.get_as_long()?.wrapping_add(b.get_as_long()?),
        )),
        ValueKind::Double => Ok(ExprValue::Double(a.get_as_double()? + b.get_as_double()?)),
        ValueKind::Decimal => Ok(ExprValue::Decimal(
            a.get_as_decimal()?.add(b.get_as_decimal()?)?,
        )),
        _ => eval_err("Unexpected type for '+'"),
    }
}

fn subtract_values(a: &ExprValue, b: &ExprValue) -> ExprResult<ExprValue> {
    if any_null2(a, b) {
        return Ok(ExprValue::Null);
    }
    match get_math_type(a, b, Coercion::Simple)? {
        kind @ (ValueKind::Date | ValueKind::Time | ValueKind::DateTime) => {
            let date_kind = date_kind_of_value(kind).unwrap();
            let result = a.get_as_double()? - b.get_as_double()?;
            Ok(to_date_value_if_possible(date_kind, result))
        }
        ValueKind::Long => Ok(ExprValue::Long(
            a.get_as_long()?.wrapping_sub(b.get_as_long()?),
        )),
        ValueKind::Double => Ok(ExprValue::Double(a.get_as_double()? - b.get_as_double()?)),
        ValueKind::Decimal => Ok(ExprValue::Decimal(
            a.get_as_decimal()?.subtract(b.get_as_decimal()?)?,
        )),
        ValueKind::Text => eval_err("Unexpected type STRING"),
        _ => eval_err("Unexpected type for '-'"),
    }
}

fn multiply_values(a: &ExprValue, b: &ExprValue) -> ExprResult<ExprValue> {
    if any_null2(a, b) {
        return Ok(ExprValue::Null);
    }
    match get_math_type(a, b, Coercion::General)? {
        ValueKind::Long => Ok(ExprValue::Long(
            a.get_as_long()?.wrapping_mul(b.get_as_long()?),
        )),
        ValueKind::Double => Ok(ExprValue::Double(a.get_as_double()? * b.get_as_double()?)),
        ValueKind::Decimal => Ok(ExprValue::Decimal(
            a.get_as_decimal()?.multiply(b.get_as_decimal()?)?,
        )),
        ValueKind::Text => eval_err("Unexpected type STRING"),
        _ => eval_err("Unexpected type for '*'"),
    }
}

fn divide_values(a: &ExprValue, b: &ExprValue) -> ExprResult<ExprValue> {
    if any_null2(a, b) {
        return Ok(ExprValue::Null);
    }
    match get_math_type(a, b, Coercion::General)? {
        ValueKind::Long => {
            let left = a.get_as_long()?;
            let right = b.get_as_long()?;
            if right == 0 {
                return arith_err("/ by zero");
            }
            if (left % right) == 0 {
                Ok(ExprValue::Long(left / right))
            } else {
                Ok(ExprValue::Double(left as f64 / right as f64))
            }
        }
        ValueKind::Double => {
            let right = b.get_as_double()?;
            if right == 0.0 {
                return arith_err("/ by zero");
            }
            Ok(ExprValue::Double(a.get_as_double()? / right))
        }
        ValueKind::Decimal => Ok(ExprValue::Decimal(
            a.get_as_decimal()?.divide(b.get_as_decimal()?)?,
        )),
        ValueKind::Text => eval_err("Unexpected type STRING"),
        _ => eval_err("Unexpected type for '/'"),
    }
}

fn int_divide_values(a: &ExprValue, b: &ExprValue) -> ExprResult<ExprValue> {
    if any_null2(a, b) {
        return Ok(ExprValue::Null);
    }
    if get_math_type(a, b, Coercion::General)?.is_string() {
        return eval_err("Unexpected type STRING");
    }
    let left = a.get_as_long()?;
    let right = b.get_as_long()?;
    if right == 0 {
        return arith_err("/ by zero");
    }
    Ok(ExprValue::Long(left / right))
}

fn modulo_values(a: &ExprValue, b: &ExprValue) -> ExprResult<ExprValue> {
    if any_null2(a, b) {
        return Ok(ExprValue::Null);
    }
    if get_math_type(a, b, Coercion::General)?.is_string() {
        return eval_err("Unexpected type STRING");
    }
    let left = a.get_as_long()?;
    let right = b.get_as_long()?;
    if right == 0 {
        return arith_err("/ by zero");
    }
    Ok(ExprValue::Long(left % right))
}

fn is_integral(value: f64) -> bool {
    let rounded = value.round_ties_even();
    value == rounded && value >= i32::MIN as f64 && value <= i32::MAX as f64 && value.is_finite()
}

fn exp_values(a: &ExprValue, b: &ExprValue) -> ExprResult<ExprValue> {
    if any_null2(a, b) {
        return Ok(ExprValue::Null);
    }
    let math_type = get_math_type(a, b, Coercion::General)?;
    if math_type == ValueKind::Decimal {
        if let Some(exponent) = b.get_as_decimal()?.to_i32_exact() {
            match a.get_as_decimal()?.pow(exponent) {
                Ok(result) => return Ok(ExprValue::Decimal(result)),
                // a coefficient/scale overflow is a port limitation, not a
                // reason to silently fall back to a double (which is only
                // what Jackcess does when BigDecimal.pow itself throws)
                Err(error @ ExprError::Unsupported(_)) => return Err(error),
                Err(_) => {}
            }
        }
    }
    let result = a.get_as_double()?.powf(b.get_as_double()?);
    if math_type == ValueKind::Long && is_integral(result) {
        return Ok(ExprValue::Long(result as i32));
    }
    Ok(ExprValue::Double(result))
}

fn concat_non_null(a: &ExprValue, b: &ExprValue) -> ExprResult<ExprValue> {
    Ok(ExprValue::Text(format!(
        "{}{}",
        a.get_as_string()?,
        b.get_as_string()?
    )))
}

fn concat_values(a: &ExprValue, b: &ExprValue) -> ExprResult<ExprValue> {
    let empty = ExprValue::Text(String::new());
    let left = if a.is_null() { &empty } else { a };
    let right = if b.is_null() { &empty } else { b };
    concat_non_null(left, right)
}

fn not_value(value: &ExprValue) -> ExprResult<ExprValue> {
    if value.is_null() {
        return Ok(ExprValue::Null);
    }
    Ok(ExprValue::bool(!value.get_as_boolean()?))
}

fn non_null_compare(a: &ExprValue, b: &ExprValue) -> ExprResult<Ordering> {
    match get_math_type(a, b, Coercion::Compare)? {
        ValueKind::Text => {
            if a.kind() != b.kind() {
                return eval_err("Unexpected type STRING");
            }
            let left = a.get_as_string()?;
            let right = b.get_as_string()?;
            if has_non_bmp(&left) || has_non_bmp(&right) {
                return unsupported(
                    "case-insensitive comparison of non-BMP text is not implemented".to_string(),
                );
            }
            Ok(compare_ignore_case(&left, &right))
        }
        ValueKind::Long => Ok(a.get_as_long()?.cmp(&b.get_as_long()?)),
        ValueKind::Double => Ok(java_double_cmp(a.get_as_double()?, b.get_as_double()?)),
        ValueKind::Decimal => Ok(a.get_as_decimal()?.compare(b.get_as_decimal()?)),
        kind => eval_err(format!("Unexpected type {:?}", kind)),
    }
}

fn compare_values(op: CompareOp, a: &ExprValue, b: &ExprValue) -> ExprResult<ExprValue> {
    if any_null2(a, b) {
        return Ok(ExprValue::Null);
    }
    let ordering = non_null_compare(a, b)?;
    let result = match op {
        CompareOp::Lt => ordering == Ordering::Less,
        CompareOp::Lte => ordering != Ordering::Greater,
        CompareOp::Gt => ordering == Ordering::Greater,
        CompareOp::Gte => ordering != Ordering::Less,
        CompareOp::Eq => ordering == Ordering::Equal,
        CompareOp::Ne => ordering != Ordering::Equal,
    };
    Ok(ExprValue::bool(result))
}

impl Decimal {
    fn to_i32_exact(self) -> Option<i32> {
        if self.scale == 0 {
            i32::try_from(self.unscaled).ok()
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// evaluator
// ---------------------------------------------------------------------------

struct EvalEnv<'a> {
    ctx: &'a EvalContext,
    result_kind: Option<ValueKind>,
}

impl Expr {
    fn eval(&self, env: &EvalEnv<'_>) -> ExprResult<ExprValue> {
        match self {
            Expr::Literal(value) => Ok(value.clone()),
            Expr::Column(identifier) => env.ctx.lookup_identifier(identifier),
            Expr::ThisColumn => env
                .ctx
                .this_value
                .clone()
                .ok_or_else(|| ExprError::Eval("No current column value available".into())),
            Expr::Paren(inner) => inner.eval(env),
            Expr::ImplicitCompare(value) => {
                let right = value.eval(env)?;
                let left =
                    env.ctx.this_value.clone().ok_or_else(|| {
                        ExprError::Eval("No current column value available".into())
                    })?;
                compare_values(CompareOp::Eq, &left, &right)
            }
            Expr::Unary(op, value) => {
                let value = value.eval(env)?;
                match op {
                    UnaryOp::Neg | UnaryOp::NegNum => negate(&value),
                    UnaryOp::Pos | UnaryOp::PosNum => Ok(value),
                    UnaryOp::Not => not_value(&value),
                }
            }
            Expr::Binary(op, left, right) => {
                let a = left.eval(env)?;
                let b = right.eval(env)?;
                match op {
                    BinaryOp::Plus => add_values(&a, &b),
                    BinaryOp::Minus => subtract_values(&a, &b),
                    BinaryOp::Multiply => multiply_values(&a, &b),
                    BinaryOp::Divide => divide_values(&a, &b),
                    BinaryOp::IntDivide => int_divide_values(&a, &b),
                    BinaryOp::Mod => modulo_values(&a, &b),
                    BinaryOp::Exp => exp_values(&a, &b),
                    BinaryOp::Concat => concat_values(&a, &b),
                }
            }
            Expr::Compare(op, left, right) => {
                let a = left.eval(env)?;
                let b = right.eval(env)?;
                compare_values(*op, &a, &b)
            }
            Expr::Logical(op, left, right) => eval_logical(*op, left, right, env),
            Expr::IsNull { value, negated } => {
                let value = value.eval(env)?;
                Ok(ExprValue::bool(value.is_null() != *negated))
            }
            Expr::Like {
                value,
                pattern,
                negated,
            } => {
                let value = value.eval(env)?;
                if value.is_null() {
                    return Ok(ExprValue::Null);
                }
                let pattern = pattern.eval(env)?;
                if pattern.is_null() {
                    return Ok(ExprValue::Null);
                }
                let matched = like_match(&pattern.get_as_string()?, &value.get_as_string()?)?;
                Ok(ExprValue::bool(matched != *negated))
            }
            Expr::Between {
                value,
                start,
                end,
                negated,
            } => {
                let value = value.eval(env)?;
                let start = start.eval(env)?;
                let end = end.eval(env)?;
                if value.is_null() || start.is_null() || end.is_null() {
                    return Ok(ExprValue::Null);
                }
                // the between values can be in either order
                let (min, max) = if non_null_compare(&start, &end)? == Ordering::Greater {
                    (&end, &start)
                } else {
                    (&start, &end)
                };
                let above = non_null_compare(&value, min)? != Ordering::Less;
                let below = non_null_compare(&value, max)? != Ordering::Greater;
                let result = above && below;
                Ok(ExprValue::bool(result != *negated))
            }
            Expr::In {
                value,
                list,
                negated,
            } => {
                let value = value.eval(env)?;
                if value.is_null() {
                    return Ok(ExprValue::Null);
                }
                let mut found = false;
                for item in list {
                    let candidate = item.eval(env)?;
                    if candidate.is_null() {
                        continue;
                    }
                    if non_null_compare(&value, &candidate)? == Ordering::Equal {
                        found = true;
                        break;
                    }
                }
                Ok(ExprValue::bool(found != *negated))
            }
            Expr::Func {
                name,
                args,
                non_variant,
            } => {
                let mut values = Vec::with_capacity(args.len());
                for arg in args {
                    values.push(arg.eval(env)?);
                }
                let result = eval_function(name, &values, env.result_kind, env.ctx)?;
                if *non_variant && result.is_null() {
                    return eval_err(format!("Invalid function call {{{name}(Null)}}"));
                }
                Ok(result)
            }
        }
    }
}

fn eval_logical(
    op: LogicalOp,
    left: &Expr,
    right: &Expr,
    env: &EvalEnv<'_>,
) -> ExprResult<ExprValue> {
    let left_value = left.eval(env)?;
    match op {
        LogicalOp::And => {
            if left_value.is_null() {
                return Ok(ExprValue::Null);
            }
            if !left_value.get_as_boolean()? {
                return Ok(ExprValue::Long(0));
            }
            let right_value = right.eval(env)?;
            if right_value.is_null() {
                return Ok(ExprValue::Null);
            }
            Ok(ExprValue::bool(right_value.get_as_boolean()?))
        }
        LogicalOp::Or => {
            if left_value.is_null() {
                return Ok(ExprValue::Null);
            }
            if left_value.get_as_boolean()? {
                return Ok(ExprValue::Long(-1));
            }
            let right_value = right.eval(env)?;
            if right_value.is_null() {
                return Ok(ExprValue::Null);
            }
            Ok(ExprValue::bool(right_value.get_as_boolean()?))
        }
        LogicalOp::Imp => {
            if left_value.is_null() {
                let right_value = right.eval(env)?;
                if right_value.is_null() || !right_value.get_as_boolean()? {
                    return Ok(ExprValue::Null);
                }
                return Ok(ExprValue::Long(-1));
            }
            if !left_value.get_as_boolean()? {
                return Ok(ExprValue::Long(-1));
            }
            let right_value = right.eval(env)?;
            if right_value.is_null() {
                return Ok(ExprValue::Null);
            }
            Ok(ExprValue::bool(right_value.get_as_boolean()?))
        }
        LogicalOp::Xor | LogicalOp::Eqv => {
            let right_value = right.eval(env)?;
            if left_value.is_null() || right_value.is_null() {
                return Ok(ExprValue::Null);
            }
            let a = left_value.get_as_boolean()?;
            let b = right_value.get_as_boolean()?;
            Ok(ExprValue::bool(if matches!(op, LogicalOp::Xor) {
                a ^ b
            } else {
                a == b
            }))
        }
    }
}

// ---------------------------------------------------------------------------
// functions
// ---------------------------------------------------------------------------

const IMPLEMENTED_FUNCTIONS: &[&str] = &[
    "IIF",
    "NZ",
    "ISNULL",
    "ISDATE",
    "ISNUMERIC",
    "TYPENAME",
    "VARTYPE",
    "ABS",
    "INT",
    "FIX",
    "SGN",
    "ROUND",
    "SQR",
    "SIN",
    "COS",
    "TAN",
    "ATN",
    "LOG",
    "EXP",
    "HEX",
    "OCT",
    "STR",
    "VAL",
    "SPACE",
    "STRING",
    "REPLACE",
    "STRREVERSE",
    "INSTR",
    "INSTRREV",
    "STRCOMP",
    "LEN",
    "LEFT",
    "RIGHT",
    "MID",
    "LCASE",
    "UCASE",
    "TRIM",
    "LTRIM",
    "RTRIM",
    "CHR",
    "CHRW",
    "ASC",
    "ASCW",
    "CSTR",
    "CLNG",
    "CDBL",
    "CINT",
    "CBYTE",
    "CBOOL",
    "CDEC",
    "CCUR",
    "CSNG",
    "CDATE",
    "CVAR",
    "CVDATE",
    "YEAR",
    "MONTH",
    "DAY",
    "HOUR",
    "MINUTE",
    "SECOND",
    "DATESERIAL",
    "TIMESERIAL",
    "DATEVALUE",
    "TIMEVALUE",
    "WEEKDAY",
    "WEEKDAYNAME",
    "MONTHNAME",
    "CHOOSE",
    "SWITCH",
    "DATEPART",
    "DATEADD",
    "DATEDIFF",
    "DATE",
    "NOW",
    "TIME",
    "TIMER",
    "RND",
    "GENUNIQUEID",
    "GENGUID",
    "FV",
    "PV",
    "PMT",
    "IPMT",
    "PPMT",
    "NPER",
    "DDB",
    "SLN",
    "SYD",
    "RATE",
    "STRCONV",
    "FORMATNUMBER",
    "FORMATPERCENT",
    "FORMATCURRENCY",
    "FORMATDATETIME",
    "FORMAT",
];

/// Functions which Jackcess itself does not provide (an expression using one is
/// a parse error in Jackcess) and the database level aggregate functions
/// (`DLookup`, `DCount`, ...) which need a database to evaluate.  The port
/// reports these as [`ExprError::Unsupported`] instead of failing at parse
/// time, so that an Access expression which uses them fails with a clear
/// message.
const UNSUPPORTED_FUNCTIONS: &[&str] = &[
    "PARTITION",
    "DLOOKUP",
    "DCOUNT",
    "DSUM",
    "DMAX",
    "DMIN",
    "DAVG",
    "IRR",
    "MIRR",
    "NPV",
];

fn is_known_function(name: &str) -> bool {
    IMPLEMENTED_FUNCTIONS.contains(&name) || UNSUPPORTED_FUNCTIONS.contains(&name)
}

/// Functions registered with `registerStringFunc` have a `Name$` non-variant
/// alias in Jackcess; all other names do not.
fn has_string_variant(name: &str) -> bool {
    matches!(
        name,
        "HEX"
            | "OCT"
            | "CHR"
            | "CHRW"
            | "STR"
            | "VAL"
            | "LCASE"
            | "UCASE"
            | "LEFT"
            | "RIGHT"
            | "MID"
            | "LTRIM"
            | "RTRIM"
            | "TRIM"
            | "REPLACE"
            | "SPACE"
            | "STRING"
            | "STRCONV"
            | "FORMAT"
    )
}

fn is_unsupported_function(name: &str) -> bool {
    UNSUPPORTED_FUNCTIONS.contains(&name)
}

fn is_impure_function(name: &str) -> bool {
    matches!(
        name,
        "RND" | "DATE" | "NOW" | "TIME" | "TIMER" | "GENGUID" | "GENUNIQUEID"
    )
}

fn expect_arity(args: &[ExprValue], min: usize, max: usize) -> ExprResult<()> {
    if args.len() < min || args.len() > max {
        let expected = if min == max {
            min.to_string()
        } else {
            format!("{min} to {max}")
        };
        return eval_err(format!(
            "Invalid number of parameters {} passed, expected {}",
            args.len(),
            expected
        ));
    }
    Ok(())
}

fn optional_int(args: &[ExprValue], idx: usize, default: i32, use_def: i32) -> ExprResult<i32> {
    if args.len() > idx {
        let value = args[idx].get_as_long()?;
        Ok(if value == use_def { default } else { value })
    } else {
        Ok(default)
    }
}

fn optional_bool(args: &[ExprValue], idx: usize) -> ExprResult<bool> {
    if args.len() > idx {
        args[idx].get_as_boolean()
    } else {
        Ok(false)
    }
}

fn optional_ignore_case(args: &[ExprValue], idx: usize) -> ExprResult<bool> {
    if args.len() > idx {
        match args[idx].get_as_long()? {
            -1 | 0 => Ok(false),
            1 => Ok(true),
            other => eval_err(format!("Unsupported compare type {other}")),
        }
    } else {
        Ok(true)
    }
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn utf16_slice(text: &str, start: usize, end: usize) -> ExprResult<String> {
    let units: Vec<u16> = text.encode_utf16().collect();
    if start > end || end > units.len() {
        return eval_err("string index out of range");
    }
    String::from_utf16(&units[start..end]).map_err(|_| {
        ExprError::Unsupported("text function selected half of a surrogate pair".into())
    })
}

fn first_utf16_unit(text: &str) -> Option<u16> {
    text.encode_utf16().next()
}

fn trim_spaces(text: &str, left: bool, right: bool) -> String {
    let mut start = 0;
    let mut end = text.len();
    if left {
        while start < end && text.as_bytes()[start] == b' ' {
            start += 1;
        }
    }
    if right {
        while start < end && text.as_bytes()[end - 1] == b' ' {
            end -= 1;
        }
    }
    text[start..end].to_string()
}

fn region_matches(text: &[char], start: usize, needle: &[char], ignore_case: bool) -> bool {
    if start + needle.len() > text.len() {
        return false;
    }
    for (idx, expected) in needle.iter().enumerate() {
        let found = text[start + idx];
        let equal = if ignore_case {
            ci_char_eq(*expected, found)
        } else {
            *expected == found
        };
        if !equal {
            return false;
        }
    }
    true
}

fn expand_short_year(year: i32) -> i32 {
    if year >= 100 {
        year
    } else if year < 50 {
        year + 2000
    } else {
        year + 1900
    }
}

fn date_from_serial(year: i32, month: i32, day: i32) -> ExprResult<LocalDateTime> {
    let year = expand_short_year(year) as i64;
    let total_months = year * 12 + (month as i64 - 1);
    let y = total_months.div_euclid(12);
    let m = total_months.rem_euclid(12) + 1;
    if !(-999_999_999..=999_999_999).contains(&y) {
        return eval_err("date/time value is outside the valid range");
    }
    let base = datenums::days_from_civil(y as i32, m as u32, 1);
    let days = base + (day as i64 - 1);
    let (ry, rm, rd) = datenums::civil_from_days(days);
    if !(-999_999_999..=999_999_999).contains(&(ry as i64)) {
        return eval_err("date/time value is outside the valid range");
    }
    Ok(LocalDateTime::of_date(ry, rm, rd))
}

fn time_from_serial(hour: i32, minute: i32, second: i32) -> LocalDateTime {
    let total = (hour as i64) * 3600 + (minute as i64) * 60 + second as i64;
    let seconds = total.rem_euclid(86_400);
    let mut ldt = base_ldt();
    ldt.hour = (seconds / 3600) as u32;
    ldt.minute = ((seconds % 3600) / 60) as u32;
    ldt.second = (seconds % 60) as u32;
    ldt
}

fn weekday_from_epoch_day(days: i64) -> i32 {
    ((days + 4).rem_euclid(7) + 1) as i32
}

fn day_of_week_to_weekday(day: i32, first_day: i32) -> i32 {
    ((day - 1) - (first_day - 1)).rem_euclid(7) + 1
}

fn weekday_to_day_of_week(weekday: i32, first_day: i32) -> i32 {
    ((first_day - 1) + (weekday - 1)).rem_euclid(7) + 1
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// `LocalDate.getDayOfYear`.
fn day_of_year(value: LocalDateTime) -> i32 {
    (value.to_epoch_day() - datenums::days_from_civil(value.year, 1, 1) + 1) as i32
}

/// `LocalDateTime.plusDays`.
fn ldt_plus_days(value: LocalDateTime, days: i64) -> LocalDateTime {
    LocalDateTime {
        ..LocalDateTime::from_epoch_day(value.to_epoch_day() + days)
    }
    .with_time_of(value)
}

impl LocalDateTime {
    fn with_time_of(self, other: LocalDateTime) -> LocalDateTime {
        LocalDateTime {
            hour: other.hour,
            minute: other.minute,
            second: other.second,
            nano: other.nano,
            ..self
        }
    }
}
/// `LocalDateTime.plusMonths`.
fn ldt_plus_months(value: LocalDateTime, months: i64) -> LocalDateTime {
    let total = value.year as i64 * 12 + (value.month as i64 - 1) + months;
    let year = total.div_euclid(12);
    let month = total.rem_euclid(12) + 1;
    // Java clamps an invalid day to the last day of the target month
    let day = value
        .day
        .min(days_in_month(year as i32, month as u32))
        .max(1);
    LocalDateTime {
        year: year as i32,
        month: month as u32,
        day,
        ..value
    }
}

/// `LocalDateTime.plusYears`.
fn ldt_plus_years(value: LocalDateTime, years: i64) -> LocalDateTime {
    ldt_plus_months(value, years * 12)
}

/// `LocalDateTime.plusHours` (and the minute/second variants).
fn ldt_plus_seconds(value: LocalDateTime, seconds: i64) -> LocalDateTime {
    let total = value.second_of_day() + seconds;
    let days = total.div_euclid(86_400);
    let secs = total.rem_euclid(86_400);
    let mut result = LocalDateTime::from_epoch_day(value.to_epoch_day() + days);
    result.hour = (secs / 3600) as u32;
    result.minute = ((secs % 3600) / 60) as u32;
    result.second = (secs % 60) as u32;
    result.nano = value.nano;
    result
}

/// Sunday based day of week (`WeekFields.SUNDAY_FIRST.dayOfWeek()`): 1 is
/// Sunday, 7 is Saturday.
fn sunday_day_of_week(value: LocalDateTime) -> i32 {
    weekday_from_epoch_day(value.to_epoch_day())
}

/// `DefaultDateFunctions.weekFields` (`WeekFields.of(firstDay, minDays)`).
#[derive(Clone, Copy, Debug)]
struct WeekFields {
    first_day: i32,
    min_days: i32,
}

fn week_fields(first_day: i32, first_week_type: i32) -> ExprResult<WeekFields> {
    let min_days = match first_week_type {
        1 => 1,
        2 => 4,
        3 => 7,
        other => {
            return eval_err(format!("Invalid first week of year type {other}"));
        }
    };
    // `DayOfWeek.SUNDAY.plus(firstDay - 1)` wraps around for any integer;
    // the result is a Sunday based day number (1 = Sunday)
    let first_day = (first_day - 1).rem_euclid(7) + 1;
    Ok(WeekFields {
        first_day,
        min_days,
    })
}

impl WeekFields {
    /// The epoch day of the first day of week 1 of the given week based year.
    fn week1_start(self, year: i32) -> i64 {
        let jan1 = datenums::days_from_civil(year, 1, 1);
        let jan1_dow = weekday_from_epoch_day(jan1);
        let offset = (jan1_dow - self.first_day).rem_euclid(7);
        let candidate = jan1 - offset as i64;
        if 7 - offset >= self.min_days {
            candidate
        } else {
            candidate + 7
        }
    }

    /// `WeekFields.weekOfWeekBasedYear()`: `(week, week_based_year)`.
    fn week_of_week_based_year(self, value: LocalDateTime) -> (i32, i32) {
        let day = value.to_epoch_day();
        let offset = (sunday_day_of_week(value) - self.first_day).rem_euclid(7);
        let week_start = day - offset as i64;
        let mut year = value.year;
        if week_start < self.week1_start(year) {
            year -= 1;
        } else if week_start >= self.week1_start(year + 1) {
            year += 1;
        }
        let week = (week_start - self.week1_start(year)) / 7 + 1;
        (week as i32, year)
    }

    /// `LocalDate.of(year, 2, 1).range(weekOfWeekBasedYear).getMaximum()`.
    fn weeks_in_year(self, year: i32) -> i32 {
        ((self.week1_start(year + 1) - self.week1_start(year)) / 7) as i32
    }
}

const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const MONTH_ABBREVIATIONS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const WEEKDAY_NAMES: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
const WEEKDAY_ABBREVIATIONS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

fn weekday_index(value: LocalDateTime) -> usize {
    (sunday_day_of_week(value) - 1) as usize
}

fn hour12(value: LocalDateTime) -> u32 {
    if value.hour.is_multiple_of(12) {
        12
    } else {
        value.hour % 12
    }
}

fn am_pm(value: LocalDateTime) -> &'static str {
    if value.hour < 12 {
        "AM"
    } else {
        "PM"
    }
}

/// `TemporalConfig.US_DATE_FORMAT` (`"M/d[/uuuu]"`); the year is always
/// present for a `LocalDateTime`, so the optional section is always printed.
fn format_short_date_us(value: LocalDateTime) -> String {
    format!("{}/{}/{}", value.month, value.day, format_year(value.year))
}

/// `TemporalConfig.US_LONG_DATE_FORMAT` (`"EEEE, MMMM dd, uuuu"`).
fn format_long_date_us(value: LocalDateTime) -> String {
    format!(
        "{}, {} {:02}, {}",
        WEEKDAY_NAMES[weekday_index(value)],
        MONTH_NAMES[(value.month - 1) as usize],
        value.day,
        format_year(value.year)
    )
}

/// `TemporalConfig.MEDIUM_DATE_FORMAT` (`"dd-MMM-uu"`).
fn format_medium_date_us(value: LocalDateTime) -> String {
    format!(
        "{:02}-{}-{:02}",
        value.day,
        MONTH_ABBREVIATIONS[(value.month - 1) as usize],
        (value.year % 100).abs()
    )
}

/// `TemporalConfig.US_TIME_FORMAT_12_FORMAT` (`"h:mm:ss a"`).
fn format_long_time_us(value: LocalDateTime) -> String {
    format!(
        "{}:{:02}:{:02} {}",
        hour12(value),
        value.minute,
        value.second,
        am_pm(value)
    )
}

/// `TemporalConfig.MEDIUM_TIME_FORMAT` (`"hh:mm a"`).
fn format_medium_time_us(value: LocalDateTime) -> String {
    format!("{:02}:{:02} {}", hour12(value), value.minute, am_pm(value))
}

/// `TemporalConfig.SHORT_TIME_FORMAT` (`"HH:mm"`).
fn format_short_time_us(value: LocalDateTime) -> String {
    format!("{:02}:{:02}", value.hour, value.minute)
}

/// The default formatting of a date/time value (`getDateFormatForType` for
/// `DATE`, `TIME` and `DATE_TIME`).
fn format_general_date_us(kind: DateKind, value: LocalDateTime) -> String {
    match kind {
        DateKind::Date => format_short_date_us(value),
        DateKind::Time => format_long_time_us(value),
        DateKind::DateTime => format!(
            "{} {}",
            format_short_date_us(value),
            format_long_time_us(value)
        ),
    }
}

/// Scan the `ValueSupport.NUMBER_PAT` prefix of `text`.
fn scan_number_prefix(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    if i < chars.len() && (chars[i] == '+' || chars[i] == '-') {
        i += 1;
    }
    let int_start = i;
    while i < chars.len() && chars[i].is_ascii_digit() {
        i += 1;
    }
    let int_digits = i - int_start;
    let mut frac_digits = 0;
    if i < chars.len() && chars[i] == '.' {
        i += 1;
        let frac_start = i;
        while i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
        }
        frac_digits = i - frac_start;
    }
    if int_digits == 0 && frac_digits == 0 {
        return None;
    }
    if i < chars.len() && (chars[i] == 'e' || chars[i] == 'E') {
        let exp_mark = i;
        i += 1;
        if i < chars.len() && (chars[i] == '+' || chars[i] == '-') {
            i += 1;
        }
        let exp_start = i;
        while i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
        }
        if i == exp_start {
            i = exp_mark;
        }
    }
    Some(chars[..i].iter().collect())
}

fn val_value(text: &str) -> ExprResult<ExprValue> {
    let stripped: String = text
        .chars()
        .filter(|c| !matches!(c, ' ' | '\t' | '\r' | '\n'))
        .collect();
    if stripped.is_empty() {
        return Ok(ExprValue::Double(0.0));
    }
    if let Some(rest) = stripped.strip_prefix('&') {
        let parse_radix = |digits: &str, radix: u32| -> Option<ExprValue> {
            let value = u128::from_str_radix(digits, radix).ok()?;
            let bit_length = 128 - value.leading_zeros();
            let as_double = if bit_length <= 16 {
                (value as u16 as i16) as f64
            } else {
                (value as u32 as i32) as f64
            };
            Some(ExprValue::Double(as_double))
        };
        if let Some(hex) = rest.strip_prefix(['h', 'H']) {
            let digits: String = hex.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
            if !digits.is_empty() {
                return parse_radix(&digits, 16)
                    .ok_or_else(|| ExprError::Unsupported("Val hex literal is too large".into()));
            }
        }
        if let Some(octal) = rest.strip_prefix(['o', 'O']) {
            let digits: String = octal
                .chars()
                .take_while(|c| ('0'..='7').contains(c))
                .collect();
            if !digits.is_empty() {
                return parse_radix(&digits, 8).ok_or_else(|| {
                    ExprError::Unsupported("Val octal literal is too large".into())
                });
            }
        }
        return Ok(ExprValue::Double(0.0));
    }
    match scan_number_prefix(&stripped) {
        Some(number) => {
            let decimal = Decimal::parse(&number)?;
            Ok(ExprValue::Double(decimal.to_f64()))
        }
        None => Ok(ExprValue::Double(0.0)),
    }
}

fn eval_function(
    name: &str,
    args: &[ExprValue],
    result_kind: Option<ValueKind>,
    ctx: &EvalContext,
) -> ExprResult<ExprValue> {
    if is_unsupported_function(name) {
        return unsupported(format!(
            "function {name} is not implemented by the expression engine port"
        ));
    }
    match evaluate_builtin(name, args, result_kind, ctx) {
        Ok(value) => Ok(value),
        Err(ExprError::Eval(message)) => Err(ExprError::Eval(message)),
        // our own explicit "not implemented" must stay Unsupported, even
        // though Jackcess' FunctionSupport would wrap it in an EvalException
        Err(error @ ExprError::Unsupported(_)) => Err(error),
        Err(error) => Err(ExprError::Eval(format!(
            "Invalid function call {{{name}(...)}}: {error}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// DatePart / DateAdd / DateDiff
// ---------------------------------------------------------------------------

/// Java `String.trim()`: strips every char `<= ' '`.
fn java_trim(text: &str) -> &str {
    text.trim_matches(|c: char| c <= ' ')
}

fn date_part_function(args: &[ExprValue]) -> ExprResult<ExprValue> {
    expect_arity(args, 2, 4)?;
    // DefaultDateFunctions.DatePart: the date is checked for null before the
    // first day / week type parameters and the interval are read
    if args[1].is_null() {
        return Ok(ExprValue::Null);
    }
    let first_day = optional_int(args, 2, 1, 0)?;
    let first_week_type = optional_int(args, 3, 1, 0)?;

    let interval = java_trim(&args[0].get_as_string()?).to_string();
    // Jackcess converts the date/time inside each interval branch, so an
    // invalid interval is reported before a bad date/time value
    let ldt = || args[1].get_as_local_date_time();
    let result = if interval.eq_ignore_ascii_case("yyyy") {
        ldt()?.year
    } else if interval.eq_ignore_ascii_case("q") {
        quarter_of(ldt()?)
    } else if interval.eq_ignore_ascii_case("m") {
        ldt()?.month as i32
    } else if interval.eq_ignore_ascii_case("y") {
        day_of_year(ldt()?)
    } else if interval.eq_ignore_ascii_case("d") {
        ldt()?.day as i32
    } else if interval.eq_ignore_ascii_case("w") {
        day_of_week_to_weekday(sunday_day_of_week(ldt()?), first_day)
    } else if interval.eq_ignore_ascii_case("ww") {
        let value = ldt()?;
        let fields = week_fields(first_day, first_week_type)?;
        fields.week_of_week_based_year(value).0
    } else if interval.eq_ignore_ascii_case("h") {
        ldt()?.hour as i32
    } else if interval.eq_ignore_ascii_case("n") {
        ldt()?.minute as i32
    } else if interval.eq_ignore_ascii_case("s") {
        ldt()?.second as i32
    } else {
        return eval_err(format!("Invalid interval {interval}"));
    };
    Ok(ExprValue::Long(result))
}

fn quarter_of(value: LocalDateTime) -> i32 {
    (value.month as i32 - 1) / 3 + 1
}

/// Java's `LocalDateTime` supports the years -999999999 to 999999999; a value
/// outside that range throws (`DateTimeException`), which Jackcess reports as
/// an `EvalException`.
fn check_date_range(value: LocalDateTime) -> ExprResult<LocalDateTime> {
    if !(-999_999_999..=999_999_999).contains(&value.year) {
        return eval_err("date/time value is outside the valid range");
    }
    Ok(value)
}

fn date_add_function(args: &[ExprValue]) -> ExprResult<ExprValue> {
    expect_arity(args, 3, 3)?;
    if args[2].is_null() {
        return Ok(ExprValue::Null);
    }
    let interval = java_trim(&args[0].get_as_string()?).to_string();
    let amount = args[1].get_as_long()?;
    let ldt = args[2].get_as_local_date_time()?;

    let result = if interval.eq_ignore_ascii_case("yyyy") {
        ldt_plus_years(ldt, amount as i64)
    } else if interval.eq_ignore_ascii_case("q") {
        // Jackcess multiplies the int parameter and lets it wrap
        ldt_plus_months(ldt, amount.wrapping_mul(3) as i64)
    } else if interval.eq_ignore_ascii_case("m") {
        ldt_plus_months(ldt, amount as i64)
    } else if interval.eq_ignore_ascii_case("y")
        || interval.eq_ignore_ascii_case("d")
        || interval.eq_ignore_ascii_case("w")
    {
        ldt_plus_days(ldt, amount as i64)
    } else if interval.eq_ignore_ascii_case("ww") {
        ldt_plus_days(ldt, amount as i64 * 7)
    } else if interval.eq_ignore_ascii_case("h") {
        ldt_plus_seconds(ldt, amount as i64 * 3600)
    } else if interval.eq_ignore_ascii_case("n") {
        ldt_plus_seconds(ldt, amount as i64 * 60)
    } else if interval.eq_ignore_ascii_case("s") {
        ldt_plus_seconds(ldt, amount as i64)
    } else {
        return eval_err(format!("Invalid interval {interval}"));
    };
    let result = check_date_range(result)?;
    Ok(ExprValue::from_local_date_time(result))
}

fn date_diff_function(args: &[ExprValue]) -> ExprResult<ExprValue> {
    expect_arity(args, 3, 5)?;
    if args[1].is_null() || args[2].is_null() {
        return Ok(ExprValue::Null);
    }
    let first_day = optional_int(args, 3, 1, 0)?;
    let first_week_type = optional_int(args, 4, 1, 0)?;

    let interval = java_trim(&args[0].get_as_string()?).to_string();
    let mut ldt1 = args[1].get_as_local_date_time()?;
    let mut ldt2 = args[2].get_as_local_date_time()?;
    let mut sign = 1;
    if ldt1 > ldt2 {
        std::mem::swap(&mut ldt1, &mut ldt2);
        sign = -1;
    }

    // NOTE: Jackcess' DateDiff understands leap years but not daylight
    // savings time, so every day is 24 hours.
    let result = if interval.eq_ignore_ascii_case("yyyy") {
        ldt2.year - ldt1.year
    } else if interval.eq_ignore_ascii_case("q") {
        let mut q2 = quarter_of(ldt2);
        let mut y2 = ldt2.year;
        let q1 = quarter_of(ldt1);
        while y2 > ldt1.year {
            q2 += 4;
            y2 -= 1;
        }
        q2 - q1
    } else if interval.eq_ignore_ascii_case("m") {
        let mut m2 = ldt2.month as i32;
        let mut y2 = ldt2.year;
        let m1 = ldt1.month as i32;
        while y2 > ldt1.year {
            m2 += 12;
            y2 -= 1;
        }
        m2 - m1
    } else if interval.eq_ignore_ascii_case("y") || interval.eq_ignore_ascii_case("d") {
        day_difference(ldt1, ldt2)
    } else if interval.eq_ignore_ascii_case("w") {
        day_difference(ldt1, ldt2) / 7
    } else if interval.eq_ignore_ascii_case("ww") {
        let fields = week_fields(first_day, first_week_type)?;
        let (w1, wy1) = fields.week_of_week_based_year(ldt1);
        let (mut w2, mut wy2) = fields.week_of_week_based_year(ldt2);
        while wy2 > wy1 {
            wy2 -= 1;
            w2 += fields.weeks_in_year(wy2);
        }
        w2 - w1
    } else if interval.eq_ignore_ascii_case("h") {
        ldt2.hour as i32 + 24 * day_difference(ldt1, ldt2) - ldt1.hour as i32
    } else if interval.eq_ignore_ascii_case("n") {
        let hour_diff = ldt2.hour as i32 + 24 * day_difference(ldt1, ldt2) - ldt1.hour as i32;
        ldt2.minute as i32 + 60 * hour_diff - ldt1.minute as i32
    } else if interval.eq_ignore_ascii_case("s") {
        let hour_diff = ldt2.hour as i32 + 24 * day_difference(ldt1, ldt2) - ldt1.hour as i32;
        let minute_diff = ldt2.minute as i32 + 60 * hour_diff - ldt1.minute as i32;
        ldt2.second as i32 + 60 * minute_diff - ldt1.second as i32
    } else {
        return eval_err(format!("Invalid interval {interval}"));
    };

    Ok(ExprValue::Long(result * sign))
}

/// `DefaultDateFunctions.getDayDiff`: the day-of-year difference counting whole
/// intervening years (equivalent to the epoch day difference of the dates).
fn day_difference(ldt1: LocalDateTime, ldt2: LocalDateTime) -> i32 {
    (ldt2.to_epoch_day() - ldt1.to_epoch_day()) as i32
}

// ---------------------------------------------------------------------------
// financial functions
// ---------------------------------------------------------------------------

fn optional_double(args: &[ExprValue], idx: usize, default: f64) -> ExprResult<f64> {
    if args.len() > idx {
        args[idx].get_as_double()
    } else {
        Ok(default)
    }
}

/// `DefaultFinancialFunctions.getPaymentType`: anything but 0 is "beginning
/// of month".
fn payment_type(args: &[ExprValue], idx: usize) -> ExprResult<i32> {
    if args.len() > idx {
        Ok(if args[idx].get_as_long()? != 0 { 1 } else { 0 })
    } else {
        Ok(0)
    }
}

fn financial_future_value(rate: f64, nper: f64, pmt: f64, pv: f64, pmt_type: i32) -> f64 {
    if rate == 0.0 {
        return -(pv + (nper * pmt));
    }
    let r1 = if pmt_type == 1 { rate + 1.0 } else { 1.0 };
    let p1 = (rate + 1.0).powf(nper);
    (((1.0 - p1) * r1 * pmt) / rate) - (pv * p1)
}

fn financial_present_value(rate: f64, nper: f64, pmt: f64, fv: f64, pmt_type: i32) -> f64 {
    if rate == 0.0 {
        return -((nper * pmt) + fv);
    }
    let r1 = if pmt_type == 1 { rate + 1.0 } else { 1.0 };
    let p1 = (rate + 1.0).powf(nper);
    ((((1.0 - p1) / rate) * r1 * pmt) - fv) / p1
}

fn financial_loan_payment(rate: f64, nper: f64, pv: f64, fv: f64, pmt_type: i32) -> f64 {
    if rate == 0.0 {
        return -(fv + pv) / nper;
    }
    let r1 = if pmt_type == 1 { rate + 1.0 } else { 1.0 };
    let p1 = (rate + 1.0).powf(nper);
    (fv + (pv * p1)) * rate / (r1 * (1.0 - p1))
}

fn financial_interest_payment(
    rate: f64,
    per: f64,
    nper: f64,
    pv: f64,
    fv: f64,
    pmt_type: i32,
) -> f64 {
    if per == 1.0 && pmt_type == 1 {
        // no interest for a payment at the beginning of the first period
        return 0.0;
    }
    let pmt = financial_loan_payment(rate, nper, pv, fv, pmt_type);
    let mut result = financial_future_value(rate, per - 1.0, pmt, pv, pmt_type) * rate;
    if pmt_type == 1 {
        result /= 1.0 + rate;
    }
    result
}

fn financial_principal_payment(
    rate: f64,
    per: f64,
    nper: f64,
    pv: f64,
    fv: f64,
    pmt_type: i32,
) -> f64 {
    let pmt = financial_loan_payment(rate, nper, pv, fv, pmt_type);
    let ipmt = financial_interest_payment(rate, per, nper, pv, fv, pmt_type);
    pmt - ipmt
}

fn financial_double_declining_balance(
    cost: f64,
    salvage: f64,
    life: f64,
    period: f64,
    factor: f64,
) -> f64 {
    if cost < 0.0 || (life == 2.0 && period > 1.0) {
        return 0.0;
    }
    if life < 2.0 || (life == 2.0 && period <= 1.0) {
        return cost - salvage;
    }
    let v1 = (factor * cost) / life;
    if period <= 1.0 {
        return v1.min(cost - salvage);
    }
    let v2 = (life - factor) / life;
    let v3 = (salvage - (cost * v2.powf(period))).max(0.0);
    let result = (v1 * v2.powf(period - 1.0)) - v3;
    result.max(0.0)
}

fn financial_loan_payment_periods(rate: f64, pmt: f64, pv: f64, fv: f64, pmt_type: i32) -> f64 {
    if rate == 0.0 {
        return -(fv + pv) / pmt;
    }
    let cr = (if pmt_type == 1 { 1.0 + rate } else { 1.0 }) * pmt / rate;
    let (v1, v2) = if cr - fv < 0.0 {
        ((fv - cr).ln(), (-pv - cr).ln())
    } else {
        ((cr - fv).ln(), (pv + cr).ln())
    };
    (v1 - v2) / (1.0 + rate).ln()
}

fn financial_rate(nper: f64, pmt: f64, pv: f64, fv: f64, pmt_type: i32, guess: f64) -> f64 {
    const MAX_ITERATIONS: i32 = 20;
    const PRECISION: f64 = 0.0000001;
    let mut rate = guess;
    let mut f = 0.0;
    let mut y;
    if rate.abs() < PRECISION {
        y = pv * (1.0 + nper * rate) + pmt * (1.0 + rate * pmt_type as f64) * nper + fv;
    } else {
        f = (nper * (1.0 + rate).ln()).exp();
        y = pv * f + pmt * (1.0 / rate + pmt_type as f64) * (f - 1.0) + fv;
    }
    let _ = y;
    let mut y0 = pv + pmt * nper + fv;
    let mut y1 = pv * f + pmt * (1.0 / rate + pmt_type as f64) * (f - 1.0) + fv;

    let mut i = 0;
    let mut x0 = 0.0;
    let mut x1 = rate;
    while (y0 - y1).abs() > PRECISION && i < MAX_ITERATIONS {
        rate = (y1 * x0 - y0 * x1) / (y1 - y0);
        x0 = x1;
        x1 = rate;
        if rate.abs() < PRECISION {
            y = pv * (1.0 + nper * rate) + pmt * (1.0 + rate * pmt_type as f64) * nper + fv;
        } else {
            f = (nper * (1.0 + rate).ln()).exp();
            y = pv * f + pmt * (1.0 / rate + pmt_type as f64) * (f - 1.0) + fv;
        }
        y0 = y1;
        y1 = y;
        i += 1;
    }
    rate
}

fn financial_function(name: &str, args: &[ExprValue]) -> ExprResult<ExprValue> {
    let value = match name {
        "NPER" => {
            expect_arity(args, 3, 5)?;
            let rate = args[0].get_as_double()?;
            let pmt = args[1].get_as_double()?;
            let pv = args[2].get_as_double()?;
            let fv = optional_double(args, 3, 0.0)?;
            let pmt_type = payment_type(args, 4)?;
            financial_loan_payment_periods(rate, pmt, pv, fv, pmt_type)
        }
        "FV" => {
            expect_arity(args, 3, 5)?;
            let rate = args[0].get_as_double()?;
            let nper = args[1].get_as_double()?;
            let pmt = args[2].get_as_double()?;
            let pv = optional_double(args, 3, 0.0)?;
            let pmt_type = payment_type(args, 4)?;
            financial_future_value(rate, nper, pmt, pv, pmt_type)
        }
        "PV" => {
            expect_arity(args, 3, 5)?;
            let rate = args[0].get_as_double()?;
            let nper = args[1].get_as_double()?;
            let pmt = args[2].get_as_double()?;
            let fv = optional_double(args, 3, 0.0)?;
            let pmt_type = payment_type(args, 4)?;
            financial_present_value(rate, nper, pmt, fv, pmt_type)
        }
        "PMT" => {
            expect_arity(args, 3, 5)?;
            let rate = args[0].get_as_double()?;
            let nper = args[1].get_as_double()?;
            let pv = args[2].get_as_double()?;
            let fv = optional_double(args, 3, 0.0)?;
            let pmt_type = payment_type(args, 4)?;
            financial_loan_payment(rate, nper, pv, fv, pmt_type)
        }
        "IPMT" => {
            expect_arity(args, 4, 6)?;
            let rate = args[0].get_as_double()?;
            let per = args[1].get_as_double()?;
            let nper = args[2].get_as_double()?;
            let pv = args[3].get_as_double()?;
            let fv = optional_double(args, 4, 0.0)?;
            let pmt_type = payment_type(args, 5)?;
            financial_interest_payment(rate, per, nper, pv, fv, pmt_type)
        }
        "PPMT" => {
            expect_arity(args, 4, 6)?;
            let rate = args[0].get_as_double()?;
            let per = args[1].get_as_double()?;
            let nper = args[2].get_as_double()?;
            let pv = args[3].get_as_double()?;
            let fv = optional_double(args, 4, 0.0)?;
            let pmt_type = payment_type(args, 5)?;
            financial_principal_payment(rate, per, nper, pv, fv, pmt_type)
        }
        "DDB" => {
            expect_arity(args, 4, 5)?;
            let cost = args[0].get_as_double()?;
            let salvage = args[1].get_as_double()?;
            let life = args[2].get_as_double()?;
            let period = args[3].get_as_double()?;
            let factor = optional_double(args, 4, 2.0)?;
            financial_double_declining_balance(cost, salvage, life, period, factor)
        }
        "SLN" => {
            expect_arity(args, 3, 3)?;
            let cost = args[0].get_as_double()?;
            let salvage = args[1].get_as_double()?;
            let life = args[2].get_as_double()?;
            (cost - salvage) / life
        }
        "SYD" => {
            expect_arity(args, 4, 4)?;
            let cost = args[0].get_as_double()?;
            let salvage = args[1].get_as_double()?;
            let life = args[2].get_as_double()?;
            let period = args[3].get_as_double()?;
            ((cost - salvage) * (life - period + 1.0) * 2.0) / (life * (life + 1.0))
        }
        "RATE" => {
            expect_arity(args, 3, 6)?;
            let nper = args[0].get_as_double()?;
            let pmt = args[1].get_as_double()?;
            let pv = args[2].get_as_double()?;
            let fv = optional_double(args, 3, 0.0)?;
            let pmt_type = payment_type(args, 4)?;
            let guess = optional_double(args, 5, 0.1)?;
            financial_rate(nper, pmt, pv, fv, pmt_type, guess)
        }
        other => {
            return unsupported(format!("financial function {other} is not implemented"));
        }
    };
    Ok(ExprValue::Double(value))
}

// ---------------------------------------------------------------------------
// text case conversion (StrConv)
// ---------------------------------------------------------------------------

/// `StringUtil.capitalizeFully` for the US locale: the first character of every
/// word is title cased and the rest is lower cased.  The word boundaries follow
/// Java's `BreakIterator` rules for ASCII text: letters and digits form words,
/// and a single apostrophe, period, underscore or hyphen between two word
/// characters keeps the word together (`don't`, `1.5`, `co-op`).  Characters
/// which do not start a word are copied unchanged.
fn capitalize_fully(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_alphanumeric() {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        while i < chars.len() {
            if chars[i].is_alphanumeric() {
                i += 1;
            } else if matches!(chars[i], '\'' | '.' | '_' | '-')
                && i + 1 < chars.len()
                && chars[i + 1].is_alphanumeric()
            {
                // a single joining character inside a word
                i += 2;
            } else {
                break;
            }
        }
        let word: String = chars[start..i].iter().collect();
        let mut word_chars = word.chars();
        if let Some(first) = word_chars.next() {
            let upper: String = first.to_uppercase().collect();
            out.push_str(&upper);
            out.push_str(&word_chars.as_str().to_lowercase());
        }
    }
    out
}

// ---------------------------------------------------------------------------
// number formatting (the Format* functions and the "Format" predefs)
// ---------------------------------------------------------------------------

/// `FormatUtil.NumPatternType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NumPatternType {
    General,
    Currency,
    Euro,
    Percent,
}

// --- java.math.BigDecimal (the `Format` number path) ------------------------
//
// Java evaluates `Format` with a `java.math.BigDecimal` (`Value.getAsBigDecimal`),
// which has no precision limit: a 40 digit number string, a pattern multiplier
// applied to a long coefficient, or `new BigDecimal(double)`'s exact binary
// expansion are all formatted exactly.  The rest of the expression engine keeps
// the fixed width [`Decimal`]; only the code below uses `BigDecimal`.

/// `BigDecimal.ZERO`.
fn bd_zero() -> BigDecimal {
    BigDecimal::from(0i64)
}

/// `BigDecimal.valueOf(long)`.
fn bd_from_i64(value: i64) -> BigDecimal {
    BigDecimal::from(value)
}

/// The `BigDecimal` with the same coefficient and scale as a [`Decimal`].
fn bd_from_decimal(value: Decimal) -> BigDecimal {
    BigDecimal::new(BigInt::from(value.unscaled()), value.scale() as i64)
}

/// `10^exp` as a `BigInt`.
fn pow10_big(exp: i64) -> BigInt {
    BigInt::from(10u32).pow(exp as u32)
}

/// The number of decimal digits of a positive `BigInt`.
fn big_int_digits(value: &BigInt) -> i64 {
    value.magnitude().to_string().len() as i64
}

/// `floor(log10(num / den))` for positive integers.
fn floor_log10_ratio(num: &BigInt, den: &BigInt) -> i64 {
    let mut k = big_int_digits(num) - big_int_digits(den);
    loop {
        let ge = if k >= 0 {
            num >= &(den * pow10_big(k))
        } else {
            &(num * pow10_big(-k)) >= den
        };
        if !ge {
            k -= 1;
            continue;
        }
        let lt = if k + 1 >= 0 {
            num < &(den * pow10_big(k + 1))
        } else {
            &(num * pow10_big(-(k + 1))) < den
        };
        if lt {
            return k;
        }
        k += 1;
    }
}

/// `2^q` as an exact `num / den` pair.
fn pow2_ratio(q: i64) -> (BigInt, BigInt) {
    if q >= 0 {
        (BigInt::from(1) << q as usize, BigInt::from(1))
    } else {
        (BigInt::from(1), BigInt::from(1) << (-q) as usize)
    }
}

/// The significant digits and exponent of `Double.toString(value)`: the
/// Schubfach "shortest, closest, ties to even" digits (see `DoubleToDecimal`).
///
/// Java renders 16 or 17 digit candidates at the scale `10^k` (where
/// `k = floor(log10(2^q))`) and takes the one which lies in the value's
/// rounding interval, trying one shorter (multiple of 10) candidate first.
fn java_double_digits(magnitude: f64) -> (BigInt, i64) {
    let bits = magnitude.to_bits();
    let biased = ((bits >> 52) & 0x7ff) as i32;
    let fraction = bits & 0x000f_ffff_ffff_ffff;
    let (mut c, q) = if biased == 0 {
        // subnormal
        (fraction, -1074i64)
    } else {
        (fraction | (1u64 << 52), biased as i64 - 1075)
    };
    // the smallest normal value has a half sized step below it
    let irregular = c == (1u64 << 52) && q != -1074;
    // "tiny" subnormals: Java shifts the significand by 10 and the decimal
    // exponent by -1 (`DoubleToDecimal` section 8.2.1)
    let dk = if biased == 0 && c < 3 {
        c *= 10;
        -1
    } else {
        0
    };
    let (num, den) = if q >= 0 {
        (BigInt::from(c) << q as usize, BigInt::from(1))
    } else {
        (BigInt::from(c), BigInt::from(1) << (-q) as usize)
    };
    // `k = floor(log10(2^q))` (or `floor(log10(3/4 * 2^q))` for the smallest
    // normal value)
    let k = if irregular {
        let (mut n, d) = pow2_ratio(q);
        n *= 3;
        floor_log10_ratio(&n, &(d * 4))
    } else {
        let (n, d) = pow2_ratio(q);
        floor_log10_ratio(&n, &d)
    };
    // r = value / 10^k
    let (mut r_num, mut r_den) = (num, den);
    if k >= 0 {
        r_den *= pow10_big(k);
    } else {
        r_num *= pow10_big(-k);
    }
    let s = &r_num / &r_den;
    let even = c % 2 == 0;
    let half = pow2_ratio(q - 1);
    let lower = if irregular {
        pow2_ratio(q - 2)
    } else {
        half.clone()
    };
    // Is `x * 10^k` within the value's rounding interval?  The interval is
    // `[v - half, v + half]` (the lower half step is quarter sized for a power
    // of two, whose predecessor is closer), with both endpoints belonging to
    // an even significand.
    let in_rv = |x: &BigInt| -> bool {
        let mut diff = x * &r_den - &r_num;
        let below = diff.sign() == Sign::Minus;
        if below {
            diff = -diff;
        }
        let (u_num, u_den) = if below { &lower } else { &half };
        let (left, right) = if k >= 0 {
            (diff * u_den * pow10_big(k), u_num * &r_den)
        } else {
            (diff * u_den, u_num * &r_den * pow10_big(-k))
        };
        match left.cmp(&right) {
            Ordering::Less => true,
            Ordering::Equal => even,
            Ordering::Greater => false,
        }
    };
    // try the one digit shorter candidates first
    if s >= BigInt::from(100) {
        let sp = (&s / 10) * 10;
        let tp = &sp + 10;
        let upin = in_rv(&sp);
        let wpin = in_rv(&tp);
        if upin != wpin {
            let chosen = if upin { sp } else { tp };
            return (chosen, k);
        }
    }
    let t = &s + 1;
    let uin = in_rv(&s);
    let win = in_rv(&t);
    if uin != win {
        let chosen = if uin { s } else { t };
        return (chosen, k + dk);
    }
    // both (or neither) lie in the interval: take the closest, ties to even
    let cmp = &r_num * 2 - (&s + &t) * &r_den;
    let s_even = (&s % 2) == BigInt::from(0);
    let chosen = if cmp < BigInt::from(0) || (cmp == BigInt::from(0) && s_even) {
        s
    } else {
        t
    };
    (chosen, k + dk)
}

/// `Double.toString(double)`: Java's decimal rendering (`computerized
/// scientific notation` outside `[1e-3, 1e7)`, at least one fractional digit
/// and a `0` after a single significant digit).  `None` for NaN / infinity.
fn java_double_string(value: f64) -> Option<String> {
    if !value.is_finite() {
        return None;
    }
    if value == 0.0 {
        return Some(if value.is_sign_negative() {
            "-0.0".to_string()
        } else {
            "0.0".to_string()
        });
    }
    let negative = value < 0.0;
    let (digits, mut exponent) = java_double_digits(value.abs());
    let mut digits = digits.to_string();
    while digits.len() > 1 && digits.ends_with('0') {
        digits.pop();
        exponent += 1;
    }
    let exponent = exponent + digits.len() as i64 - 1;
    let mut java_text = String::new();
    if !(-3..7).contains(&exponent) {
        // computerized scientific notation, e.g. `1.0E300`, `4.9E-324`
        java_text.push_str(&digits[..1]);
        java_text.push('.');
        if digits.len() == 1 {
            java_text.push('0');
        } else {
            java_text.push_str(&digits[1..]);
        }
        java_text.push('E');
        java_text.push_str(&exponent.to_string());
    } else if exponent >= 0 {
        let int_len = exponent as usize + 1;
        if int_len >= digits.len() {
            java_text.push_str(&digits);
            java_text.push_str(&"0".repeat(int_len - digits.len()));
            java_text.push_str(".0");
        } else {
            java_text.push_str(&digits[..int_len]);
            java_text.push('.');
            java_text.push_str(&digits[int_len..]);
        }
    } else {
        java_text.push_str("0.");
        java_text.push_str(&"0".repeat((-exponent - 1) as usize));
        java_text.push_str(&digits);
    }
    Some(if negative {
        format!("-{java_text}")
    } else {
        java_text
    })
}

/// Java `BigDecimal.valueOf(double)` (`new BigDecimal(Double.toString(d))`).
///
/// The coefficient/scale *representation* matters (`CustomNumberFmt.
/// formatMaybeZero` compares `bd.scale()`), so Java's `Double.toString` form is
/// reproduced.  `None` is Java's `NumberFormatException` (NaN / infinity).
fn bd_value_of(value: f64) -> Option<BigDecimal> {
    BigDecimal::from_str(&java_double_string(value)?).ok()
}

/// Java `new BigDecimal(double)`: the exact decimal value of the binary
/// floating point number (including subnormals), normalised like the JDK
/// constructor (an odd significand with the smallest scale).  `None` for
/// NaN / infinity.
fn bd_exact_from_f64(value: f64) -> Option<BigDecimal> {
    if !value.is_finite() {
        return None;
    }
    let bits = value.to_bits();
    let negative = (bits >> 63) != 0;
    let biased = ((bits >> 52) & 0x7ff) as i32;
    let fraction = bits & 0x000f_ffff_ffff_ffff;
    let mut significand = if biased == 0 {
        // subnormal (the JDK doubles the significand and lowers the exponent)
        fraction << 1
    } else {
        fraction | (1u64 << 52)
    };
    let mut exponent = biased as i64 - 1075;
    if significand == 0 {
        return Some(bd_zero());
    }
    while significand & 1 == 0 {
        significand >>= 1;
        exponent += 1;
    }
    let mut coefficient = BigInt::from(significand);
    if negative {
        coefficient = -coefficient;
    }
    Some(if exponent < 0 {
        // coefficient * 2^exponent == coefficient * 5^-exponent * 10^exponent
        let k = (-exponent) as u64;
        BigDecimal::new(coefficient * BigInt::from(5u32).pow(k as u32), k as i64)
    } else if exponent > 0 {
        BigDecimal::new(coefficient << exponent as usize, 0)
    } else {
        BigDecimal::new(coefficient, 0)
    })
}

/// `BigDecimal.round(MathContext(prec, HALF_EVEN))`.
fn bd_round_sig(value: &BigDecimal, prec: u32) -> BigDecimal {
    match NonZeroU64::new(prec as u64) {
        Some(prec) => value.with_precision_round(prec, RoundingMode::HalfEven),
        None => value.clone(),
    }
}

/// `ValueSupport.normalize(BigDecimal)`: strip trailing zeros and force a
/// scale of at least 0.
fn normalize_big_decimal(value: BigDecimal) -> BigDecimal {
    if value.fractional_digit_count() == 0 {
        return value;
    }
    if value.sign() == Sign::NoSign {
        return bd_zero();
    }
    let value = value.normalized();
    if value.fractional_digit_count() < 0 {
        value.with_scale(0)
    } else {
        value
    }
}

/// The syntax of `new BigDecimal(String)`, as validated by [`Decimal::parse`],
/// returning the coefficient digits and the scale.  There is no digit count
/// limit (unlike [`Decimal::parse`]).
fn parse_decimal_literal_parts(text: &str) -> ExprResult<(bool, String, i32)> {
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return parse_err(format!("Invalid number literal '{text}'"));
    }
    let mut pos = 0;
    let mut negative = false;
    if bytes[pos] == b'+' || bytes[pos] == b'-' {
        negative = bytes[pos] == b'-';
        pos += 1;
    }
    let mut digits = String::new();
    let mut fraction = 0i32;
    let mut seen_digit = false;
    let mut seen_dot = false;
    while pos < bytes.len() {
        let c = bytes[pos];
        if c.is_ascii_digit() {
            digits.push(c as char);
            seen_digit = true;
            if seen_dot {
                fraction += 1;
            }
            pos += 1;
        } else if c == b'.' && !seen_dot {
            seen_dot = true;
            pos += 1;
        } else {
            break;
        }
    }
    if !seen_digit {
        return parse_err(format!("Invalid number literal '{text}'"));
    }
    let mut exponent: i32 = 0;
    if pos < bytes.len() && (bytes[pos] == b'e' || bytes[pos] == b'E') {
        pos += 1;
        let mut exp_negative = false;
        if pos < bytes.len() && (bytes[pos] == b'+' || bytes[pos] == b'-') {
            exp_negative = bytes[pos] == b'-';
            pos += 1;
        }
        let mut exp_digits = String::new();
        while pos < bytes.len() && bytes[pos].is_ascii_digit() {
            exp_digits.push(bytes[pos] as char);
            pos += 1;
        }
        if exp_digits.is_empty() {
            return parse_err(format!("Invalid number literal '{text}'"));
        }
        let parsed: i64 = exp_digits
            .parse()
            .map_err(|_| ExprError::Unsupported(format!("exponent of '{text}' is too large")))?;
        let signed = if exp_negative { -parsed } else { parsed };
        exponent = i32::try_from(signed)
            .map_err(|_| ExprError::Unsupported(format!("exponent of '{text}' is too large")))?;
    }
    if pos != bytes.len() {
        return parse_err(format!("Invalid number literal '{text}'"));
    }
    let scale = fraction
        .checked_sub(exponent)
        .ok_or_else(|| ExprError::Unsupported(format!("exponent of '{text}' is too large")))?;
    Ok((negative, digits, scale))
}

/// `StringValue.getNumber`: the number a text value coerces to (arbitrary
/// precision, `ValueSupport.normalize`d).
fn parse_big_decimal(text: &str) -> ExprResult<BigDecimal> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return eval_err(format!("'{text}' cannot be converted to a number"));
    }
    if let Some(rest) = trimmed.strip_prefix('&') {
        if let Some(hex) = rest.strip_prefix(['h', 'H']) {
            if !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                let value = BigInt::parse_bytes(hex.as_bytes(), 16).ok_or_else(|| {
                    ExprError::Eval(format!("'{text}' cannot be converted to a number"))
                })?;
                return Ok(normalize_big_decimal(BigDecimal::from(value)));
            }
        }
        if let Some(octal) = rest.strip_prefix(['o', 'O']) {
            if !octal.is_empty() && octal.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
                let value = BigInt::parse_bytes(octal.as_bytes(), 8).ok_or_else(|| {
                    ExprError::Eval(format!("'{text}' cannot be converted to a number"))
                })?;
                return Ok(normalize_big_decimal(BigDecimal::from(value)));
            }
        }
        return eval_err(format!("'{text}' cannot be converted to a number"));
    }
    // US numeric config: grouping separator ',' is discarded
    let canonical: String = trimmed.chars().filter(|c| *c != ',').collect();
    let (negative, digits, scale) =
        parse_decimal_literal_parts(&canonical).map_err(|error| match error {
            ExprError::Unsupported(message) => ExprError::Unsupported(message),
            _ => ExprError::Eval(format!("'{text}' cannot be converted to a number")),
        })?;
    let coefficient = BigInt::parse_bytes(digits.as_bytes(), 10)
        .ok_or_else(|| ExprError::Eval(format!("'{text}' cannot be converted to a number")))?;
    let coefficient = if negative { -coefficient } else { coefficient };
    Ok(normalize_big_decimal(BigDecimal::new(
        coefficient,
        scale as i64,
    )))
}

/// `Value.getAsBigDecimal` with Java `BigDecimal` semantics (arbitrary
/// precision; doubles use `BigDecimal.valueOf`, *not* `new BigDecimal(double)`).
fn as_big_decimal(value: &ExprValue) -> ExprResult<BigDecimal> {
    match value {
        ExprValue::Null => eval_err("Null cannot be converted to BIG_DEC"),
        ExprValue::Bool(value) => Ok(bd_from_i64(if *value { -1 } else { 0 })),
        ExprValue::Byte(value) => Ok(bd_from_i64(*value as i64)),
        ExprValue::Int(value) => Ok(bd_from_i64(*value as i64)),
        ExprValue::Long(value) => Ok(bd_from_i64(*value as i64)),
        ExprValue::BigInt(value) => Ok(bd_from_i64(*value)),
        ExprValue::Double(value) => bd_value_of(*value).ok_or_else(|| {
            ExprError::Unsupported(
                "Java throws NumberFormatException (NaN/Infinity cannot be converted to a \
                 BigDecimal); the expression port cannot reproduce that failure"
                    .into(),
            )
        }),
        ExprValue::Decimal(value) => Ok(bd_from_decimal(*value)),
        ExprValue::Text(text) => parse_big_decimal(text),
        ExprValue::DateTime { value, .. } => bd_value_of(datenums::to_date_double(*value))
            .ok_or_else(|| {
                ExprError::Unsupported(
                    "Java throws NumberFormatException (NaN/Infinity cannot be converted to a \
                     BigDecimal); the expression port cannot reproduce that failure"
                        .into(),
                )
            }),
    }
}

/// `FormatUtil.Args.coerceToNumberValue` followed by `Value.getAsBigDecimal`
/// (the `Format` path; unlike [`as_big_decimal`] it coerces `"true"`/`"false"`
/// text and dates).
fn coerce_to_big_decimal(value: &ExprValue) -> ExprResult<BigDecimal> {
    if value.kind().is_numeric() {
        return as_big_decimal(value);
    }
    if let ExprValue::Text(text) = value {
        if text.eq_ignore_ascii_case("true") {
            return Ok(bd_from_i64(-1));
        }
        if text.eq_ignore_ascii_case("false") {
            return Ok(bd_from_i64(0));
        }
        if let Ok(decimal) = parse_big_decimal(text) {
            return Ok(decimal);
        }
        if let Ok(date_time) = value.get_as_local_date_time() {
            return Ok(bd_value_of(datenums::to_date_double(date_time)).unwrap_or_else(bd_zero));
        }
        return eval_err("invalid number value");
    }
    Ok(bd_value_of(value.get_as_double()?).unwrap_or_else(bd_zero))
}

/// `NumberFormatter.TypeFormatter.format`: format a `BigDecimal` with at most
/// `prec` significant digits, using `DecimalFormat("0.#")` and falling back to
/// `DecimalFormat("0.#E00")` for `prec` significant digits when the plain
/// notation would need more.
fn format_type_formatter(value: &BigDecimal, prec: u32) -> ExprResult<String> {
    let value = value.normalized();
    let digits = value.digits() as i64;
    let scale = value.fractional_digit_count();
    let sig_digits = if scale < 0 {
        digits - scale
    } else if scale > digits {
        scale
    } else {
        digits
    };
    if sig_digits > prec as i64 {
        // `createScientificFormat`: `DecimalFormat("0.#E00")` with one integer
        // digit and `prec` fraction digits, wrapped in the `E+` scientific
        // notation
        let mut df = JavaDecimalFormat::parse("0.#E00")?;
        df.max_frac = prec as i32;
        df.decimal_always = false;
        return format_scientific_df(&df, ExpNotation::UpperPlus, &value);
    }
    let mut df = JavaDecimalFormat::parse("0.#")?;
    df.max_int = prec as i32;
    df.max_frac = prec as i32;
    // `DecimalFormat` only shows the decimal separator when there are fraction
    // digits (Jackcess does not force it for these formatters)
    df.decimal_always = false;
    format_df(&df, &value)
}

/// `NumberFormatter.format(double)`: `new BigDecimal(d, MathContext(15,
/// HALF_EVEN))` through the type formatter, i.e. the exact binary value of
/// `d` rounded to 15 significant digits.
fn format_general_number_double(value: f64) -> String {
    if value.is_nan() {
        return "1.#QNAN".to_string();
    }
    if value.is_infinite() {
        return if value < 0.0 {
            "-1.#INF".to_string()
        } else {
            "1.#INF".to_string()
        };
    }
    if value == 0.0 {
        return "0".to_string();
    }
    let Some(exact) = bd_exact_from_f64(value) else {
        return value.to_string();
    };
    let rounded = bd_round_sig(&exact, 15);
    format_type_formatter(&rounded, 15).unwrap_or_else(|_| value.to_string())
}

/// `Format`'s `"General Number"` predef: `coerceToNumberValue().getAsString()`.
fn format_general_number(value: &ExprValue) -> ExprResult<String> {
    match value {
        ExprValue::Null => eval_err("Null cannot be converted to STRING"),
        ExprValue::Bool(value) => Ok(if *value { "-1" } else { "0" }.to_string()),
        ExprValue::Byte(value) => Ok(value.to_string()),
        ExprValue::Int(value) => Ok(value.to_string()),
        ExprValue::Long(value) => Ok(value.to_string()),
        ExprValue::BigInt(value) => Ok(value.to_string()),
        ExprValue::Double(value) => Ok(format_general_number_double(*value)),
        ExprValue::Decimal(value) => Ok(value.format()),
        ExprValue::Text(text) => {
            if text.eq_ignore_ascii_case("true") {
                return Ok("-1".to_string());
            }
            if text.eq_ignore_ascii_case("false") {
                return Ok("0".to_string());
            }
            match parse_big_decimal(text) {
                // `NumberFormatter.format(BigDecimal)` rounds its argument to
                // `DEC_MATH_CONTEXT` (28 significant digits) before the type
                // formatter formats it
                Ok(decimal) => {
                    let rounded = bd_round_sig(&decimal, DEC_SIG_DIGITS);
                    format_type_formatter(&rounded, DEC_SIG_DIGITS)
                }
                Err(_) => {
                    let date_time = value.get_as_local_date_time()?;
                    Ok(format_general_number_double(datenums::to_date_double(
                        date_time,
                    )))
                }
            }
        }
        ExprValue::DateTime { .. } => Ok(format_general_number_double(value.get_as_double()?)),
    }
}

/// Split a non-negative `BigDecimal` with `num_dec` fraction digits into its
/// integer and fraction digit strings.
fn split_scaled_digits(value: &BigDecimal, num_dec: u32) -> (String, String) {
    let digits = value.abs().as_bigint_and_exponent().0.to_string();
    let num_dec = num_dec as usize;
    let (int_part, frac_part) = if num_dec >= digits.len() {
        (
            "0".to_string(),
            "0".repeat(num_dec - digits.len()) + &digits,
        )
    } else {
        let split = digits.len() - num_dec;
        (digits[..split].to_string(), digits[split..].to_string())
    };
    let trimmed = int_part.trim_start_matches('0');
    let int_part = if trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    };
    let mut frac_part = frac_part;
    while frac_part.len() < num_dec {
        frac_part.push('0');
    }
    (int_part, frac_part)
}

/// Insert grouping separators every `group` digits from the right.
fn group_integer_digits(digits: &str, group: usize) -> String {
    let len = digits.len();
    let mut out = String::with_capacity(len + len / group);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (len - i).is_multiple_of(group) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Format a value exactly like the `DecimalFormat` pattern built by
/// `FormatUtil.createNumberFormatPattern` for a `NumericConfig` with US
/// symbols (grouping 3, `$` currency, `%` percent, `HALF_EVEN` rounding).
///
/// `value` is the (already coerced) `Value.getAsBigDecimal` of the argument, so
/// the multiply and the rounding are exact `BigDecimal` operations.
fn format_number_pattern(
    value: &BigDecimal,
    pattern: NumPatternType,
    num_dec_digits: i32,
    inc_lead_digit: bool,
    neg_parens: bool,
    group_digits: i32,
) -> ExprResult<String> {
    let scaled = if pattern == NumPatternType::Percent {
        value * BigDecimal::from(100i64)
    } else {
        value.clone()
    };
    // DecimalFormat derives the sign from the value before rounding, so a
    // small negative number formats as "-0"
    let negative = scaled.sign() == Sign::Minus;
    let num_dec = num_dec_digits.max(0) as u32;
    let rounded = scaled
        .abs()
        .with_scale_round(num_dec as i64, RoundingMode::HalfEven);
    let (int_digits, frac_digits) = split_scaled_digits(&rounded, num_dec);

    let mut int_out = int_digits;
    if !inc_lead_digit && int_out == "0" {
        int_out.clear();
    }
    if group_digits > 0 && int_out.len() > group_digits as usize {
        int_out = group_integer_digits(&int_out, group_digits as usize);
    }

    let (prefix, suffix) = match pattern {
        NumPatternType::Currency => ("$", ""),
        NumPatternType::Euro => ("\u{20AC}", ""),
        NumPatternType::Percent => ("", "%"),
        NumPatternType::General => ("", ""),
    };

    let mut text = String::new();
    if negative && neg_parens {
        text.push('(');
        text.push_str(prefix);
    } else {
        if negative {
            text.push('-');
        }
        text.push_str(prefix);
    }
    text.push_str(&int_out);
    if num_dec > 0 {
        text.push('.');
        text.push_str(&frac_digits);
    }
    text.push_str(suffix);
    if negative && neg_parens {
        text.push(')');
    }
    Ok(text)
}

/// Format a value in the scientific pattern `0.<num_dec zeros>E0` used by the
/// `NumericConfig` "Scientific" format, including the explicit exponent sign
/// `NumberFormatter.ScientificFormat` adds.
fn format_scientific_pattern(value: &BigDecimal, num_dec: u32) -> ExprResult<String> {
    let pattern = if num_dec > 0 {
        format!("0.{}E0", "0".repeat(num_dec as usize))
    } else {
        "0E0".to_string()
    };
    let mut df = JavaDecimalFormat::parse(&pattern)?;
    df.decimal_always = false;
    format_scientific_df(&df, ExpNotation::UpperPlus, value)
}

// ---------------------------------------------------------------------------
// custom Format patterns (`FormatUtil.parseCustomFormat`)
// ---------------------------------------------------------------------------
//
// `Format` first looks the format string up in `PREDEF_FMTS`; anything else
// (apart from the empty format, which is a no-op) is a custom pattern.  The
// pattern is classified by its first significant character and then parsed as
// a `java.text.DecimalFormat` pattern (numbers), a `DateTimeFormatter` pattern
// (dates, after Jackcess' own Access field code mapping), a text format
// (`@ & < >`) or a general "yes/no" format (literal choices only).
//
// Every parser below follows the corresponding `FormatUtil` method step by
// step, including its quirks (min integer digits forced to 0 when the pattern
// has no `0`, the literal zero format with its `0`-stripping loop, quoting of
// literals with single quotes and the `E`/`e` exponent marker handling).

/// `FormatUtil`'s `FCT_*` character classes (`FORMAT_CODE_TYPES`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FormatCodeType {
    Unknown,
    Literal,
    General,
    Date,
    Number,
    Text,
}

/// `FormatUtil.getFormatCodeType`.
fn format_code_type(c: char) -> FormatCodeType {
    match c {
        // setFormatCodeTypes(" $+-()", FCT_LITERAL)
        ' ' | '$' | '+' | '-' | '(' | ')' => FormatCodeType::Literal,
        // setFormatCodeTypes("\"!*\\[];", FCT_GENERAL)
        '"' | '!' | '*' | '\\' | '[' | ']' | ';' => FormatCodeType::General,
        // setFormatCodeTypes(":/cdwmqyhnstampmAMPM", FCT_DATE)
        ':' | '/' | 'c' | 'd' | 'w' | 'm' | 'q' | 'y' | 'h' | 'n' | 's' | 't' | 'a' | 'p' | 'A'
        | 'M' | 'P' => FormatCodeType::Date,
        // setFormatCodeTypes(".,0#%Ee", FCT_NUMBER)
        '.' | ',' | '0' | '#' | '%' | 'E' | 'e' => FormatCodeType::Number,
        // setFormatCodeTypes("@&<>", FCT_TEXT)
        '@' | '&' | '<' | '>' => FormatCodeType::Text,
        _ => FormatCodeType::Unknown,
    }
}

/// The `ExpressionTokenizer.ExprBuf` cursor the format parsers use.
struct FormatBuf {
    chars: Vec<char>,
    pos: usize,
}

impl FormatBuf {
    fn new(text: &str) -> FormatBuf {
        FormatBuf {
            chars: text.chars().collect(),
            pos: 0,
        }
    }

    fn has_next(&self) -> bool {
        self.pos < self.chars.len()
    }

    fn next(&mut self) -> char {
        let c = self.chars[self.pos];
        self.pos += 1;
        c
    }

    fn peek_next(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn cur_pos(&self) -> usize {
        self.pos
    }

    fn reset(&mut self, pos: usize) {
        self.pos = pos;
    }
}

/// `ExpressionTokenizer.parseStringUntil`: scan until `end_char`, optionally
/// treating a doubled `end_char` as an escaped literal.  A missing closing
/// character is a `ParseException`.
fn parse_format_string_until(
    buf: &mut FormatBuf,
    start_char: Option<char>,
    end_char: char,
    allow_doubled_escape: bool,
) -> ExprResult<String> {
    let mut out = String::new();
    let mut complete = false;
    while buf.has_next() {
        let c = buf.next();
        if c == end_char {
            if allow_doubled_escape && buf.peek_next() == Some(end_char) {
                buf.next();
            } else {
                complete = true;
                break;
            }
        } else if start_char == Some(c) {
            return parse_err(format!(
                "Missing closing '{end_char}' for quoted string in format pattern"
            ));
        }
        out.push(c);
    }
    if !complete {
        return parse_err(format!(
            "Missing closing '{end_char}' for quoted string in format pattern"
        ));
    }
    Ok(out)
}

/// `FormatUtil.determineFormatType`: skip literals / quoted strings / colour
/// codes / escapes and return the first date, number or text code.
fn determine_format_type(text: &str) -> ExprResult<FormatCodeType> {
    let mut buf = FormatBuf::new(text);
    while buf.has_next() {
        let c = buf.next();
        match format_code_type(c) {
            FormatCodeType::Unknown | FormatCodeType::Literal => {}
            FormatCodeType::General => match c {
                '"' => {
                    parse_format_string_until(&mut buf, None, '"', true)?;
                }
                '[' => {
                    parse_format_string_until(&mut buf, Some('['), ']', false)?;
                }
                '\\' | '*' if buf.has_next() => {
                    buf.next();
                }
                _ => {}
            },
            other => return Ok(other),
        }
    }
    Ok(FormatCodeType::General)
}

/// A parsed custom format (`FormatUtil.Fmt`).
#[derive(Clone, Debug)]
enum CustomFormat {
    Number(CustomNumberFormat),
    Date(CustomDateFormat),
    Text(CustomTextFormat),
    General(CustomGeneralFormat),
}

impl CustomFormat {
    /// `FormatUtil.createFormat` + `Args.format`: `Err(Eval)` means "the value
    /// cannot be formatted as this type, return it as is" (the caller maps it
    /// to the original value).
    fn apply(&self, value: &ExprValue) -> ExprResult<ExprValue> {
        match self {
            CustomFormat::Number(fmt) => fmt.format(value),
            CustomFormat::General(fmt) => fmt.format(value),
            CustomFormat::Date(fmt) => {
                // `CustomFmt`: null / empty string use the `NULL_FMT`
                if is_null_or_empty(value) {
                    return Ok(ExprValue::Text(String::new()));
                }
                fmt.format(value)
            }
            CustomFormat::Text(fmt) => fmt.format(value),
        }
    }
}

fn is_null_or_empty(value: &ExprValue) -> bool {
    match value {
        ExprValue::Null => true,
        ExprValue::Text(text) => text.is_empty(),
        _ => false,
    }
}

/// `FormatUtil.parseCustomFormat`.
fn parse_custom_format(
    text: &str,
    first_day: i32,
    first_week_type: i32,
) -> ExprResult<CustomFormat> {
    match determine_format_type(text)? {
        FormatCodeType::General => Ok(CustomFormat::General(parse_custom_general_format(text)?)),
        FormatCodeType::Date => Ok(CustomFormat::Date(parse_custom_date_format(
            text,
            first_day,
            first_week_type,
        )?)),
        FormatCodeType::Number => Ok(CustomFormat::Number(parse_custom_number_format(text)?)),
        FormatCodeType::Text => Ok(CustomFormat::Text(parse_custom_text_format(text)?)),
        other => eval_err(format!("Invalid format type {other:?}")),
    }
}

/// Report a case where Java itself throws a non-`EvalException` (and which
/// therefore cannot be reproduced as a `Value` result).
fn java_throws<T>(exception: &str, detail: &str) -> ExprResult<T> {
    unsupported(format!(
        "Java throws {exception} ({detail}); the expression port cannot reproduce that failure"
    ))
}

// --- java.text.DecimalFormat ------------------------------------------------

/// `NumberFormatter.NotationType` (the exponent marker and sign handling).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExpNotation {
    UpperMinus,
    UpperPlus,
    LowerMinus,
    LowerPlus,
}

/// The subset of `java.text.DecimalFormat` state the custom number formats
/// use (US symbols, HALF_EVEN rounding, no currency).
#[derive(Clone, Debug)]
struct JavaDecimalFormat {
    prefix: String,
    suffix: String,
    min_int: i32,
    max_int: i32,
    min_frac: i32,
    max_frac: i32,
    grouping_size: i32,
    grouping_used: bool,
    multiplier: i32,
    use_exp: bool,
    min_exp_digits: i32,
    decimal_always: bool,
}

impl JavaDecimalFormat {
    /// `DecimalFormat.applyPattern` for a pattern with at most one subpattern
    /// (Jackcess splits the Access pattern on `;` before this is called).
    fn parse(pattern: &str) -> ExprResult<JavaDecimalFormat> {
        let chars: Vec<char> = pattern.chars().collect();
        let len = chars.len();
        let mut prefix = String::new();
        let mut suffix = String::new();
        // 0 = prefix, 1 = number, 2 = suffix
        let mut phase = 0u8;
        let mut in_quote = false;
        let mut decimal_pos: i32 = -1;
        let mut multiplier = 1i32;
        let mut digit_left = 0i32;
        let mut zero_digit = 0i32;
        let mut digit_right = 0i32;
        let mut grouping_count: i32 = -1;
        let mut use_exp = false;
        let mut min_exp_digits = 0i32;
        let mut pos = 0usize;
        while pos < len {
            let ch = chars[pos];
            match phase {
                0 | 2 => {
                    let affix = if phase == 0 { &mut prefix } else { &mut suffix };
                    if in_quote {
                        if ch == '\'' {
                            if pos + 1 < len && chars[pos + 1] == '\'' {
                                pos += 1;
                                affix.push_str("''");
                            } else {
                                in_quote = false;
                            }
                            pos += 1;
                            continue;
                        }
                        affix.push(ch);
                        pos += 1;
                        continue;
                    }
                    match ch {
                        '#' | '0' | ',' | '.' => {
                            phase = 1;
                            continue;
                        }
                        '\'' => {
                            if pos + 1 < len && chars[pos + 1] == '\'' {
                                pos += 1;
                                affix.push_str("''");
                            } else {
                                in_quote = true;
                            }
                            pos += 1;
                            continue;
                        }
                        ';' => {
                            // a second subpattern is never produced by
                            // Jackcess (it splits on `;` itself)
                            return java_throws(
                                "IllegalArgumentException",
                                "unquoted ';' in a DecimalFormat pattern",
                            );
                        }
                        '%' => {
                            if multiplier != 1 {
                                return java_throws(
                                    "IllegalArgumentException",
                                    "too many percent characters in pattern",
                                );
                            }
                            multiplier = 100;
                            affix.push_str("'%");
                            pos += 1;
                            continue;
                        }
                        _ => {
                            affix.push(ch);
                            pos += 1;
                            continue;
                        }
                    }
                }
                _ => {
                    match ch {
                        '#' => {
                            if zero_digit > 0 {
                                digit_right += 1;
                            } else {
                                digit_left += 1;
                            }
                            if grouping_count >= 0 && decimal_pos < 0 {
                                grouping_count += 1;
                            }
                        }
                        '0' => {
                            if digit_right > 0 {
                                return java_throws(
                                    "IllegalArgumentException",
                                    "unexpected '0' in the fraction of a pattern",
                                );
                            }
                            zero_digit += 1;
                            if grouping_count >= 0 && decimal_pos < 0 {
                                grouping_count += 1;
                            }
                        }
                        ',' => {
                            grouping_count = 0;
                        }
                        '.' => {
                            if decimal_pos >= 0 {
                                return java_throws(
                                    "IllegalArgumentException",
                                    "multiple decimal separators in pattern",
                                );
                            }
                            decimal_pos = digit_left + zero_digit + digit_right;
                        }
                        'E' => {
                            if use_exp {
                                return java_throws(
                                    "IllegalArgumentException",
                                    "multiple exponential symbols in pattern",
                                );
                            }
                            use_exp = true;
                            min_exp_digits = 0;
                            let mut next = pos + 1;
                            while next < len && chars[next] == '0' {
                                min_exp_digits += 1;
                                next += 1;
                            }
                            if digit_left + zero_digit < 1 || min_exp_digits < 1 {
                                return java_throws(
                                    "IllegalArgumentException",
                                    "malformed exponential pattern",
                                );
                            }
                            phase = 2;
                            pos = next;
                            continue;
                        }
                        _ => {
                            phase = 2;
                            continue;
                        }
                    }
                    pos += 1;
                }
            }
        }

        // `###.###` and `.###` get an implicit zero digit
        if zero_digit == 0 && digit_left > 0 && decimal_pos >= 0 {
            let mut n = decimal_pos;
            if n == 0 {
                n += 1;
            }
            digit_right = digit_left - n;
            digit_left = n - 1;
            zero_digit = 1;
        }

        if (decimal_pos < 0 && digit_right > 0)
            || (decimal_pos >= 0
                && (decimal_pos < digit_left || decimal_pos > digit_left + zero_digit))
            || grouping_count == 0
            || in_quote
        {
            return java_throws("IllegalArgumentException", "malformed pattern");
        }

        let total = digit_left + zero_digit + digit_right;
        let effective_decimal_pos = if decimal_pos >= 0 { decimal_pos } else { total };
        let min_int = effective_decimal_pos - digit_left;
        let max_int = if use_exp {
            digit_left + min_int
        } else {
            i32::MAX
        };
        let max_frac = if decimal_pos >= 0 {
            total - decimal_pos
        } else {
            0
        };
        let min_frac = if decimal_pos >= 0 {
            digit_left + zero_digit - decimal_pos
        } else {
            0
        };
        let mut df = JavaDecimalFormat {
            prefix: expand_affix(&prefix),
            suffix: expand_affix(&suffix),
            min_int,
            max_int,
            min_frac,
            max_frac,
            grouping_size: if grouping_count > 0 {
                grouping_count
            } else {
                0
            },
            grouping_used: grouping_count > 0,
            multiplier,
            use_exp,
            min_exp_digits,
            decimal_always: decimal_pos == 0 || decimal_pos == total,
        };
        if df.max_frac > 0 {
            // `createCustomNumberFormat`: if the decimal is included in the
            // format, Access always shows it
            df.decimal_always = true;
        }
        Ok(df)
    }

    /// `DecimalBDFormat.getMaxDecimalDigits`.
    fn max_dec_digits(&self) -> i32 {
        let mut digits = self.max_frac;
        let mut mult = self.multiplier;
        while mult > 1 {
            digits += 1;
            mult /= 10;
        }
        digits
    }
}

/// `DecimalFormat.expandAffix`: drop the quote characters and map the quoted
/// percent / permille / minus / currency markers.
fn expand_affix(pattern: &str) -> String {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        if c == '\'' {
            if i >= chars.len() {
                // `DecimalFormat` would throw StringIndexOutOfBoundsException;
                // Jackcess never produces an unterminated affix quote
                break;
            }
            let quoted = chars[i];
            i += 1;
            match quoted {
                '%' => out.push('%'),
                '\u{2030}' => out.push('\u{2030}'),
                '-' => out.push('-'),
                '\u{00A4}' => {
                    // only reachable for a raw currency sign, which the
                    // Jackcess pattern builder always quotes itself
                    if i < chars.len() && chars[i] == '\u{00A4}' {
                        i += 1;
                        out.push_str("USD");
                    } else {
                        out.push('$');
                    }
                }
                other => out.push(other),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The significant digits and decimal point position of a non-negative
/// `BigDecimal`, like `DigitList.set(BigDecimal, ..., fixedPoint)` (which uses
/// `BigDecimal.toString()` and then drops the trailing zeros).
fn decimal_digit_list(value: &BigDecimal) -> (Vec<char>, i64) {
    let (coefficient, scale) = value.as_bigint_and_exponent();
    if coefficient.sign() == Sign::NoSign {
        return (Vec::new(), 0);
    }
    let text = coefficient.to_string();
    let mut digits: Vec<char> = text.trim_start_matches('-').chars().collect();
    let decimal_at = digits.len() as i64 - scale;
    while digits.len() > 1 && digits.last() == Some(&'0') {
        digits.pop();
    }
    (digits, decimal_at)
}

/// `DigitList.shouldRoundUp` for `HALF_EVEN` with an exact decimal value.
fn should_round_up(digits: &[char], keep: usize) -> bool {
    if keep >= digits.len() {
        return false;
    }
    match digits[keep] {
        d if d > '5' => true,
        '5' => {
            if keep == digits.len() - 1 {
                // exact tie: round to even
                keep > 0 && !(digits[keep - 1] as u8 - b'0').is_multiple_of(2)
            } else {
                digits[keep + 1..].iter().any(|c| *c != '0')
            }
        }
        _ => false,
    }
}

/// `DigitList.round`: keep `maximum` digits, rounding HALF_EVEN.
fn round_digits(digits: &mut Vec<char>, decimal_at: &mut i64, maximum: i64) {
    let count = digits.len() as i64;
    if maximum < 0 || maximum >= count {
        return;
    }
    let keep = maximum as usize;
    if should_round_up(digits, keep) {
        let mut i = keep as i64 - 1;
        loop {
            if i < 0 {
                // all nines: 1 followed by zeros, exponent bumped
                digits.clear();
                digits.push('1');
                *decimal_at += 1;
                return;
            }
            let d = digits[i as usize];
            if d == '9' {
                digits[i as usize] = '0';
                i -= 1;
            } else {
                digits[i as usize] = (d as u8 + 1) as char;
                break;
            }
        }
        digits.truncate(keep.max(1));
    } else {
        digits.truncate(keep);
    }
    while digits.len() > 1 && digits.last() == Some(&'0') {
        digits.pop();
    }
}

/// `BigDecimal.multiply` for the `DecimalFormat` multiplier: exact, like Java's
/// `number.multiply(getBigDecimalMultiplier())`.
fn multiply_by_multiplier(value: &BigDecimal, multiplier: i32) -> BigDecimal {
    if multiplier == 1 {
        return value.clone();
    }
    value * BigDecimal::from(multiplier as i64)
}

/// Render a non-negative `BigDecimal` with the given `DecimalFormat` (the
/// positive affixes are always used: Jackcess formats the negated value with
/// the negative subpattern's own format).
fn format_df(df: &JavaDecimalFormat, value: &BigDecimal) -> ExprResult<String> {
    let scaled = multiply_by_multiplier(value, df.multiplier);
    // `DecimalFormat.format(BigDecimal)` derives the sign from the (multiplied)
    // value and formats the negation with the negative affixes (a pattern
    // without a `;` subpattern has the default `-` negative prefix)
    let negative = scaled.sign() == Sign::Minus;
    let scaled = if negative { scaled.abs() } else { scaled };
    let (mut digits, mut decimal_at) = decimal_digit_list(&scaled);
    if digits.is_empty() {
        // `DigitList.isZero()` → `decimalAt = 0`
        decimal_at = 0;
    }
    let zero = digits.is_empty();

    if df.use_exp {
        let maximum = df.max_int as i64 + df.max_frac as i64;
        round_digits(&mut digits, &mut decimal_at, maximum);
    } else if -decimal_at > df.max_frac as i64 {
        // underflow to zero
        digits.clear();
    } else if -decimal_at == df.max_frac as i64 {
        // the value is just below 10^-max_frac: round the leading digit
        let up = if let Some(first) = digits.first().copied() {
            if first > '5' {
                true
            } else if first == '5' {
                if digits.len() == 1 {
                    false
                } else {
                    digits[1..].iter().any(|c| *c != '0')
                }
            } else {
                false
            }
        } else {
            false
        };
        if up {
            digits.clear();
            digits.push('1');
            decimal_at += 1;
        } else {
            digits.clear();
        }
    } else {
        let maximum = df.max_frac as i64 + decimal_at;
        round_digits(&mut digits, &mut decimal_at, maximum);
    }

    let digit_count = digits.len() as i64;
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    out.push_str(&df.prefix);
    if df.use_exp {
        let mut exponent = decimal_at;
        let repeat = df.max_int as i64;
        let mut minimum_integer_digits = df.min_int as i64;
        if repeat > 1 && repeat > df.min_int as i64 {
            // engineering notation: the exponent is a multiple of `repeat`
            if exponent >= 1 {
                exponent = ((exponent - 1) / repeat) * repeat;
            } else {
                exponent = ((exponent - repeat) / repeat) * repeat;
            }
            minimum_integer_digits = 1;
        } else {
            exponent -= minimum_integer_digits;
        }
        let mut minimum_digits = df.min_int as i64 + df.min_frac as i64;
        let integer_digits = if zero {
            minimum_integer_digits
        } else {
            decimal_at - exponent
        };
        if minimum_digits < integer_digits {
            minimum_digits = integer_digits;
        }
        let mut total_digits = digit_count;
        if minimum_digits > total_digits {
            total_digits = minimum_digits;
        }
        for i in 0..total_digits {
            if i == integer_digits {
                out.push('.');
            }
            if (i as usize) < digits.len() {
                out.push(digits[i as usize]);
            } else {
                out.push('0');
            }
        }
        if df.decimal_always && total_digits == integer_digits {
            out.push('.');
        }
        if zero {
            exponent = 0;
        }
        out.push('E');
        let negative_exponent = exponent < 0;
        if negative_exponent {
            exponent = -exponent;
            out.push('-');
        }
        let exp_text = exponent.to_string();
        let mut pad = df.min_exp_digits as i64 - exp_text.chars().count() as i64;
        while pad > 0 {
            out.push('0');
            pad -= 1;
        }
        out.push_str(&exp_text);
    } else {
        let mut count = df.min_int as i64;
        let mut digit_index: i64 = 0;
        if decimal_at > 0 && count < decimal_at {
            count = decimal_at;
        }
        if count > df.max_int as i64 {
            count = df.max_int as i64;
            digit_index = decimal_at - count;
        }
        let size_before = out.chars().count();
        let mut i = count - 1;
        while i >= 0 {
            if i < decimal_at && digit_index < digit_count {
                out.push(digits[digit_index as usize]);
                digit_index += 1;
            } else {
                out.push('0');
            }
            if df.grouping_used
                && i > 0
                && df.grouping_size != 0
                && i % df.grouping_size as i64 == 0
            {
                out.push(',');
            }
            i -= 1;
        }
        let fraction_present = df.min_frac > 0 || digit_index < digit_count;
        if !fraction_present && out.chars().count() == size_before {
            out.push('0');
        }
        if df.decimal_always || fraction_present {
            out.push('.');
        }
        let mut i = 0i64;
        while i < df.max_frac as i64 {
            if i >= df.min_frac as i64 && digit_index >= digit_count {
                break;
            }
            if -1 - i > decimal_at - 1 {
                out.push('0');
                i += 1;
                continue;
            }
            if digit_index < digit_count {
                out.push(digits[digit_index as usize]);
                digit_index += 1;
            } else {
                out.push('0');
            }
            i += 1;
        }
    }
    out.push_str(&df.suffix);
    Ok(out)
}

/// `NumberFormatter.ScientificFormat`: post-process the `E` of the formatted
/// number (the `+`/`e` handling Jackcess does outside `DecimalFormat`).
fn format_scientific_df(
    df: &JavaDecimalFormat,
    exp: ExpNotation,
    value: &BigDecimal,
) -> ExprResult<String> {
    let text = format_df(df, value)?;
    let chars: Vec<char> = text.chars().collect();
    let Some(index) = chars.iter().rposition(|c| *c == 'E') else {
        return Ok(text);
    };
    let plus = matches!(exp, ExpNotation::UpperPlus | ExpNotation::LowerPlus);
    let lower = matches!(exp, ExpNotation::LowerMinus | ExpNotation::LowerPlus);
    if plus && index + 1 >= chars.len() {
        // `maybeInsertExpPlus` reads the character after the `E`
        return java_throws(
            "StringIndexOutOfBoundsException",
            "an exponent 'E' as the last character of the formatted value",
        );
    }
    let mut out = String::new();
    for (i, c) in chars.iter().enumerate() {
        if i == index {
            out.push(if lower { 'e' } else { 'E' });
            if plus && chars.get(index + 1) != Some(&'-') {
                out.push('+');
            }
        } else {
            out.push(*c);
        }
    }
    Ok(out)
}

/// The `BDFormat` variants of `FormatUtil` (literal, `DecimalFormat`, and the
/// scientific wrapper).
#[derive(Clone, Debug)]
enum BDFormat {
    Literal(String),
    Decimal(JavaDecimalFormat),
    Scientific(JavaDecimalFormat, ExpNotation),
}

impl BDFormat {
    fn max_dec_digits(&self) -> i32 {
        match self {
            BDFormat::Decimal(df) => df.max_dec_digits(),
            BDFormat::Literal(_) | BDFormat::Scientific(..) => i32::MAX,
        }
    }

    fn format(&self, value: &BigDecimal) -> ExprResult<String> {
        match self {
            BDFormat::Literal(text) => Ok(text.clone()),
            BDFormat::Decimal(df) => format_df(df, value),
            BDFormat::Scientific(df, exp) => format_scientific_df(df, *exp, value),
        }
    }
}

/// One subpattern of an Access number/yes-no format.
#[derive(Clone, Debug, Default)]
struct NumberSubPattern {
    pattern: String,
    exp: Option<ExpNotation>,
    has_fmts: bool,
    has_req_digit: bool,
}

/// `FormatUtil.flushPendingNumberLiteral`: wrap a literal run in the single
/// quotes `DecimalFormat` uses for quoted affixes.
fn flush_pending_number_literal(pending: &mut String, sb: &mut String) {
    if pending.is_empty() {
        return;
    }
    let text = std::mem::take(pending);
    if text == "'" {
        // a standalone single quote is written as a doubled quote
        sb.push_str("''");
        return;
    }
    sb.push('\'');
    for c in text.chars() {
        if c == '\'' {
            sb.push('\'');
        }
        sb.push(c);
    }
    sb.push('\'');
}

/// `FormatUtil.addCustomNumberFormat`: store the current subpattern, filling
/// in the Access defaults (negative and zero reuse the positive pattern, the
/// null format is empty).
fn push_number_subpattern(
    subs: &mut Vec<NumberSubPattern>,
    idx: usize,
    sb: &mut String,
    exp: Option<ExpNotation>,
    has_fmts: bool,
    has_req_digit: bool,
) {
    let mut exp = exp;
    let mut has_fmts = has_fmts;
    let mut has_req_digit = has_req_digit;
    if sb.is_empty() {
        match idx {
            1 => {
                let pos = &subs[0];
                sb.push('-');
                sb.push_str(&pos.pattern);
                exp = pos.exp;
                has_fmts = pos.has_fmts;
                has_req_digit = pos.has_req_digit;
            }
            2 => {
                let pos = &subs[0];
                sb.push_str(&pos.pattern);
                exp = pos.exp;
                has_fmts = pos.has_fmts;
                has_req_digit = pos.has_req_digit;
            }
            _ => {}
        }
    }
    subs.push(NumberSubPattern {
        pattern: std::mem::take(sb),
        exp,
        has_fmts,
        has_req_digit,
    });
}

/// `FormatUtil.createCustomNumberFormat` for a literal-only subpattern: strip
/// the quoting that `flushPendingNumberLiteral` added.
fn strip_literal_pattern(pattern: &str) -> ExprResult<String> {
    if pattern.is_empty() {
        return Ok(String::new());
    }
    let mut chars: Vec<char> = pattern.chars().collect();
    if chars.len() < 2 {
        return java_throws(
            "StringIndexOutOfBoundsException",
            "literal subpattern quoting",
        );
    }
    chars.pop();
    chars.remove(0);
    if chars.is_empty() {
        // this was a single, literal single quote
        return Ok("'".to_string());
    }
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\'' {
            i += 1;
            if i >= chars.len() {
                return java_throws(
                    "StringIndexOutOfBoundsException",
                    "literal subpattern quoting",
                );
            }
            chars.remove(i);
        }
        i += 1;
    }
    Ok(chars.into_iter().collect())
}

/// The literal zero format of a number pattern (`createCustomNumberFormat` with
/// `isZeroFmt`): format zero, then delete the `0` characters Java inserts
/// which Access does not show.
fn strip_zero_digits(df: &JavaDecimalFormat, zero_val: String) -> String {
    let pref_len = df.prefix.chars().count() as i64;
    let suffix_len = df.suffix.chars().count();
    let mut chars: Vec<char> = zero_val.chars().collect();
    if chars.len() < suffix_len {
        return zero_val;
    }
    let mut i = chars.len() as i64 - suffix_len as i64 - 1;
    while i >= pref_len {
        if chars[i as usize] == '0' {
            chars.remove(i as usize);
        }
        i -= 1;
    }
    chars.into_iter().collect()
}

/// `FormatUtil.createCustomNumberFormat`.
fn build_bd_format(sub: &NumberSubPattern, is_zero_fmt: bool) -> ExprResult<BDFormat> {
    if !sub.has_fmts {
        return Ok(BDFormat::Literal(strip_literal_pattern(&sub.pattern)?));
    }
    let mut df = JavaDecimalFormat::parse(&sub.pattern)?;
    if let Some(exp) = sub.exp {
        if is_zero_fmt {
            let text = format_scientific_df(&df, exp, &bd_zero())?;
            return Ok(BDFormat::Literal(text));
        }
        return Ok(BDFormat::Scientific(df, exp));
    }
    if !sub.has_req_digit {
        // java likes to force extra 0's while access doesn't
        df.min_int = 0;
    }
    if is_zero_fmt {
        let zero_val = format_df(&df, &bd_zero())?;
        if !sub.has_req_digit {
            return Ok(BDFormat::Literal(strip_zero_digits(&df, zero_val)));
        }
        return Ok(BDFormat::Literal(zero_val));
    }
    Ok(BDFormat::Decimal(df))
}

/// The custom number format (`FormatUtil.CustomNumberFmt`).
#[derive(Clone, Debug)]
struct CustomNumberFormat {
    pos: BDFormat,
    neg: BDFormat,
    zero: BDFormat,
    null: BDFormat,
}

impl CustomNumberFormat {
    /// `CustomNumberFmt.formatMaybeZero`: if rounding to the format's precision
    /// produces zero, use the zero subpattern for the result.
    fn format_maybe_zero(&self, value: BigDecimal, fmt: &BDFormat) -> ExprResult<ExprValue> {
        let mut value = value;
        let max_digits = fmt.max_dec_digits() as i64;
        if max_digits < value.fractional_digit_count() {
            value = value.with_scale_round(max_digits, RoundingMode::HalfEven);
            if value.sign() == Sign::NoSign {
                return Ok(ExprValue::Text(self.zero.format(&value)?));
            }
        }
        Ok(ExprValue::Text(fmt.format(&value)?))
    }

    fn format(&self, value: &ExprValue) -> ExprResult<ExprValue> {
        if value.is_null() {
            return Ok(ExprValue::Text(self.null.format(&bd_zero())?));
        }
        // `Args.getAsBigDecimal` coerces and then uses `Value.getAsBigDecimal`.
        // `DoubleValue.getAsBigDecimal` is `BigDecimal.valueOf`, which throws
        // `NumberFormatException` for NaN / infinity.  `Args.format` only
        // catches `EvalException`, so Java propagates that failure.
        let bd = coerce_to_big_decimal(value)?;
        match bd.sign() {
            Sign::Plus => self.format_maybe_zero(bd, &self.pos),
            Sign::Minus => self.format_maybe_zero(-bd, &self.neg),
            Sign::NoSign => Ok(ExprValue::Text(self.zero.format(&bd)?)),
        }
    }
}

/// `FormatUtil.parseCustomNumberFormat`.
fn parse_custom_number_format(text: &str) -> ExprResult<CustomNumberFormat> {
    let mut buf = FormatBuf::new(text);
    let mut subs: Vec<NumberSubPattern> = Vec::new();
    let mut pending = String::new();
    let mut sb = String::new();
    let mut exp: Option<ExpNotation> = None;
    let mut has_fmts = false;
    let mut has_req_digit = false;
    let mut idx = 0usize;

    while buf.has_next() {
        let c = buf.next();
        match format_code_type(c) {
            FormatCodeType::General => match c {
                '!' => {}
                '"' => {
                    let quoted = parse_format_string_until(&mut buf, None, '"', true)?;
                    pending.push_str(&quoted);
                }
                '[' => {
                    parse_format_string_until(&mut buf, Some('['), ']', false)?;
                }
                '\\' => {
                    if buf.has_next() {
                        pending.push(buf.next());
                    }
                }
                '*' => {
                    if buf.has_next() {
                        buf.next();
                    }
                }
                ';' => {
                    if idx == 3 {
                        break;
                    }
                    flush_pending_number_literal(&mut pending, &mut sb);
                    push_number_subpattern(&mut subs, idx, &mut sb, exp, has_fmts, has_req_digit);
                    idx += 1;
                    exp = None;
                    has_fmts = false;
                    has_req_digit = false;
                }
                other => pending.push(other),
            },
            FormatCodeType::Number => {
                has_fmts = true;
                match c {
                    'E' | 'e' => {
                        let sign = buf.peek_next();
                        if sign == Some('+') || sign == Some('-') {
                            buf.next();
                            exp = Some(match (c, sign) {
                                ('E', Some('+')) => ExpNotation::UpperPlus,
                                ('E', _) => ExpNotation::UpperMinus,
                                (_, Some('+')) => ExpNotation::LowerPlus,
                                (_, _) => ExpNotation::LowerMinus,
                            });
                            flush_pending_number_literal(&mut pending, &mut sb);
                            sb.push('E');
                        } else {
                            pending.push(c);
                        }
                    }
                    '0' => {
                        has_req_digit = true;
                        flush_pending_number_literal(&mut pending, &mut sb);
                        sb.push('0');
                    }
                    other => {
                        flush_pending_number_literal(&mut pending, &mut sb);
                        sb.push(other);
                    }
                }
            }
            _ => pending.push(c),
        }
    }

    while idx < 4 {
        flush_pending_number_literal(&mut pending, &mut sb);
        push_number_subpattern(&mut subs, idx, &mut sb, exp, has_fmts, has_req_digit);
        idx += 1;
        exp = None;
        has_fmts = false;
        has_req_digit = false;
    }

    Ok(CustomNumberFormat {
        pos: build_bd_format(&subs[0], false)?,
        neg: build_bd_format(&subs[1], false)?,
        zero: build_bd_format(&subs[2], true)?,
        null: build_bd_format(&subs[3], true)?,
    })
}

// --- date/time formats ------------------------------------------------------

/// The `DATE_FMT_BUILDERS` keys, used to recognise partial patterns.
const DATE_FORMAT_KEYS: [&str; 29] = [
    "c", "d", "dd", "ddd", "dddd", "ddddd", "dddddd", "w", "ww", "m", "mm", "mmm", "mmmm", "q",
    "y", "yy", "yyyy", "h", "hh", "n", "nn", "s", "ss", "ttttt", "AM/PM", "am/pm", "A/P", "a/p",
    "AMPM",
];

/// A `FormatUtil.DateFormatBuilder` match: a complete pattern or a prefix of
/// a longer one.
enum DfbMatch {
    Full(DatePart),
    Partial,
}

/// One date/time format component.
#[derive(Clone, Debug)]
enum DatePart {
    Literal(String),
    LitChar(char),
    General,
    Day,
    Day2,
    WeekdayShort,
    WeekdayFull,
    ShortDate,
    LongDate,
    Month,
    Month2,
    MonthShort,
    MonthFull,
    Quarter,
    DayOfYear,
    Year2,
    Year4,
    Hour { pad: bool },
    Minute,
    Minute2,
    Second,
    Second2,
    LongTime,
    AmPm(AmPmCase),
    WeekDay(WeekFields),
    WeekOfYear(WeekFields),
}

/// `FormatUtil.AmPmDFB`'s am/pm strings.
#[derive(Clone, Copy, Debug)]
enum AmPmCase {
    Upper,
    Lower,
    UpperSingle,
    LowerSingle,
}

/// `FormatUtil.DATE_FMT_BUILDERS.get`.
fn date_part_lookup(
    pattern: &str,
    week_fields: &ExprResult<WeekFields>,
) -> ExprResult<Option<DfbMatch>> {
    let full = match pattern {
        "c" => Some(DatePart::General),
        "d" => Some(DatePart::Day),
        "dd" => Some(DatePart::Day2),
        "ddd" => Some(DatePart::WeekdayShort),
        "dddd" => Some(DatePart::WeekdayFull),
        "ddddd" => Some(DatePart::ShortDate),
        "dddddd" => Some(DatePart::LongDate),
        "w" => Some(DatePart::WeekDay(
            *week_fields.as_ref().map_err(|e| e.clone())?,
        )),
        "ww" => Some(DatePart::WeekOfYear(
            *week_fields.as_ref().map_err(|e| e.clone())?,
        )),
        "m" => Some(DatePart::Month),
        "mm" => Some(DatePart::Month2),
        "mmm" => Some(DatePart::MonthShort),
        "mmmm" => Some(DatePart::MonthFull),
        "q" => Some(DatePart::Quarter),
        "y" => Some(DatePart::DayOfYear),
        "yy" => Some(DatePart::Year2),
        "yyyy" => Some(DatePart::Year4),
        "h" => Some(DatePart::Hour { pad: false }),
        "hh" => Some(DatePart::Hour { pad: true }),
        "n" => Some(DatePart::Minute),
        "nn" => Some(DatePart::Minute2),
        "s" => Some(DatePart::Second),
        "ss" => Some(DatePart::Second2),
        "ttttt" => Some(DatePart::LongTime),
        "AM/PM" => Some(DatePart::AmPm(AmPmCase::Upper)),
        "am/pm" => Some(DatePart::AmPm(AmPmCase::Lower)),
        "A/P" => Some(DatePart::AmPm(AmPmCase::UpperSingle)),
        "a/p" => Some(DatePart::AmPm(AmPmCase::LowerSingle)),
        "AMPM" => Some(DatePart::AmPm(AmPmCase::Upper)),
        _ => None,
    };
    if let Some(part) = full {
        return Ok(Some(DfbMatch::Full(part)));
    }
    // `fillInPartialPrefixes` registers every proper prefix of a key
    if DATE_FORMAT_KEYS
        .iter()
        .any(|key| key.len() > pattern.len() && key.starts_with(pattern))
    {
        return Ok(Some(DfbMatch::Partial));
    }
    Ok(None)
}

/// The custom date/time format (`FormatUtil.CustomFmt` over a set of
/// `DateTimeFormatter` builders).
#[derive(Clone, Debug)]
struct CustomDateFormat {
    parts: Vec<DatePart>,
    has_ampm: bool,
}

impl CustomDateFormat {
    fn format(&self, value: &ExprValue) -> ExprResult<ExprValue> {
        let coerced = coerce_to_date_time_value(value)?;
        let ExprValue::DateTime { kind, value } = &coerced else {
            return Ok(coerced);
        };
        let text = render_date_parts(&self.parts, self.has_ampm, *value, *kind);
        Ok(ExprValue::Text(text))
    }
}

/// `FormatUtil.parseCustomDateFormat`.
fn parse_custom_date_format(
    text: &str,
    first_day: i32,
    first_week_type: i32,
) -> ExprResult<CustomDateFormat> {
    // `DefaultDateFunctions.weekFields` is only consulted by `w` / `ww`
    let week_fields = week_fields(first_day, first_week_type);
    let mut buf = FormatBuf::new(text);
    let mut parts: Vec<DatePart> = Vec::new();
    let mut has_ampm = false;

    while buf.has_next() {
        let c = buf.next();
        match format_code_type(c) {
            FormatCodeType::General => match c {
                '"' => {
                    let quoted = parse_format_string_until(&mut buf, None, '"', true)?;
                    parts.push(DatePart::Literal(quoted));
                }
                '[' => {
                    parse_format_string_until(&mut buf, Some('['), ']', false)?;
                }
                '\\' => {
                    if buf.has_next() {
                        parts.push(DatePart::LitChar(buf.next()));
                    }
                }
                '*' => {
                    if buf.has_next() {
                        buf.next();
                    }
                }
                ';' => break,
                // in a date format `!` is a literal (unlike the other types)
                other => parts.push(DatePart::LitChar(other)),
            },
            FormatCodeType::Date => {
                if c == ':' || c == '/' {
                    parts.push(DatePart::LitChar(c));
                    continue;
                }
                let first_pos = buf.cur_pos();
                let mut candidate = String::new();
                candidate.push(c);
                let mut best: Option<DatePart> = None;
                let mut best_pos = first_pos;
                match date_part_lookup(&candidate, &week_fields)? {
                    Some(DfbMatch::Full(part)) => best = Some(part),
                    Some(DfbMatch::Partial) => {}
                    None => {
                        // `DATE_FMT_BUILDERS.get` returns null and the null
                        // builder is invoked: NullPointerException in Java
                        return java_throws(
                            "NullPointerException",
                            &format!("date/time format code '{c}' has no builder"),
                        );
                    }
                }
                while buf.has_next() {
                    candidate.push(buf.next());
                    match date_part_lookup(&candidate, &week_fields)? {
                        None => break,
                        Some(DfbMatch::Partial) => {}
                        Some(DfbMatch::Full(part)) => {
                            best = Some(part);
                            best_pos = buf.cur_pos();
                        }
                    }
                }
                match best {
                    Some(part) => {
                        buf.reset(best_pos);
                        // the hour codes switch to the 12 hour clock when an
                        // am/pm code is present (`HourlyDFB`); the general `c`
                        // code picks its component from the value's own type
                        // when it is rendered
                        if matches!(c, 'a' | 'A') {
                            has_ampm = true;
                        }
                        parts.push(part);
                    }
                    None => {
                        // only a partial prefix: the first character is a
                        // literal, the rest is parsed again
                        buf.reset(first_pos);
                        parts.push(DatePart::LitChar(c));
                    }
                }
            }
            _ => parts.push(DatePart::LitChar(c)),
        }
    }

    Ok(CustomDateFormat { parts, has_ampm })
}

/// Format the date/time parts (the three `DateTimeFormatter` variants differ
/// only in the `c` component, which is picked from the value's own type).
fn render_date_parts(
    parts: &[DatePart],
    has_ampm: bool,
    value: LocalDateTime,
    kind: DateKind,
) -> String {
    let mut out = String::new();
    for part in parts {
        match part {
            DatePart::Literal(text) => out.push_str(text),
            DatePart::LitChar(c) => out.push(*c),
            DatePart::General => {
                out.push_str(&format_general_date_us(kind, value));
            }
            DatePart::Day => out.push_str(&value.day.to_string()),
            DatePart::Day2 => out.push_str(&format!("{:02}", value.day)),
            DatePart::WeekdayShort => out.push_str(WEEKDAY_ABBREVIATIONS[weekday_index(value)]),
            DatePart::WeekdayFull => out.push_str(WEEKDAY_NAMES[weekday_index(value)]),
            DatePart::ShortDate => out.push_str(&format_short_date_us(value)),
            DatePart::LongDate => out.push_str(&format_long_date_us(value)),
            DatePart::Month => out.push_str(&value.month.to_string()),
            DatePart::Month2 => out.push_str(&format!("{:02}", value.month)),
            DatePart::MonthShort => out.push_str(MONTH_ABBREVIATIONS[(value.month - 1) as usize]),
            DatePart::MonthFull => out.push_str(MONTH_NAMES[(value.month - 1) as usize]),
            DatePart::Quarter => out.push_str(&(((value.month - 1) / 3 + 1).to_string())),
            DatePart::DayOfYear => {
                let day = value.to_epoch_day() - datenums::days_from_civil(value.year, 1, 1) + 1;
                out.push_str(&day.to_string());
            }
            DatePart::Year2 => out.push_str(&format!("{:02}", value.year.rem_euclid(100))),
            DatePart::Year4 => out.push_str(&format_year(value.year)),
            DatePart::Hour { pad } => {
                let hour = if has_ampm { hour12(value) } else { value.hour };
                if *pad {
                    out.push_str(&format!("{hour:02}"));
                } else {
                    out.push_str(&hour.to_string());
                }
            }
            DatePart::Minute => out.push_str(&value.minute.to_string()),
            DatePart::Minute2 => out.push_str(&format!("{:02}", value.minute)),
            DatePart::Second => out.push_str(&value.second.to_string()),
            DatePart::Second2 => out.push_str(&format!("{:02}", value.second)),
            DatePart::LongTime => out.push_str(&format_long_time_us(value)),
            DatePart::AmPm(case) => {
                let (am, pm) = match case {
                    AmPmCase::Upper => ("AM", "PM"),
                    AmPmCase::Lower => ("am", "pm"),
                    AmPmCase::UpperSingle => ("A", "P"),
                    AmPmCase::LowerSingle => ("a", "p"),
                };
                out.push_str(if value.hour < 12 { am } else { pm });
            }
            DatePart::WeekDay(fields) => {
                let day = (sunday_day_of_week(value) - fields.first_day).rem_euclid(7) + 1;
                out.push_str(&day.to_string());
            }
            DatePart::WeekOfYear(fields) => {
                let (week, _year) = fields.week_of_week_based_year(value);
                out.push_str(&week.to_string());
            }
        }
    }
    out
}

// --- text formats -----------------------------------------------------------

/// One `FormatUtil.CharSourceFmt` action.
#[derive(Clone, Debug)]
enum TextAction {
    Literal(String),
    RequiredPlaceholder,
    OptionalPlaceholder,
}

/// `FormatUtil.TextCase`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TextCase {
    None,
    Upper,
    Lower,
}

impl TextCase {
    fn apply(self, c: char) -> char {
        match self {
            TextCase::None => c,
            TextCase::Upper => c.to_uppercase().next().unwrap_or(c),
            TextCase::Lower => c.to_lowercase().next().unwrap_or(c),
        }
    }
}

/// `FormatUtil.CharSource`.
struct TextCharSource {
    pref_len: usize,
    chars: Vec<char>,
    pos: usize,
    case: TextCase,
}

impl TextCharSource {
    fn new(text: &str, len: usize, right_aligned: bool, case: TextCase) -> TextCharSource {
        let chars: Vec<char> = text.chars().collect();
        let str_len = chars.len();
        let mut pref_len = 0;
        let mut pos = 0;
        if len > str_len {
            if right_aligned {
                pref_len = len - str_len;
            }
        } else if len < str_len {
            // the meaning of "right aligned" seems to flip when the string is
            // longer than the format length
            if !right_aligned {
                pos = str_len - len;
            }
        }
        TextCharSource {
            pref_len,
            chars,
            pos,
            case,
        }
    }

    fn next(&mut self) -> Option<char> {
        if self.pref_len > 0 {
            self.pref_len -= 1;
            return None;
        }
        if self.pos < self.chars.len() {
            let c = self.chars[self.pos];
            self.pos += 1;
            return Some(self.case.apply(c));
        }
        None
    }

    fn append_remaining(&mut self, out: &mut String) {
        while self.pos < self.chars.len() {
            let c = self.chars[self.pos];
            self.pos += 1;
            out.push(self.case.apply(c));
        }
    }
}

/// `FormatUtil.CharSourceFmt`.
#[derive(Clone, Debug)]
struct TextFmt {
    actions: Vec<TextAction>,
    num_placeholders: usize,
    right_aligned: bool,
    case: TextCase,
}

impl TextFmt {
    fn format(&self, text: &str) -> String {
        let mut source =
            TextCharSource::new(text, self.num_placeholders, self.right_aligned, self.case);
        let mut out = String::new();
        for action in &self.actions {
            match action {
                TextAction::Literal(literal) => out.push_str(literal),
                TextAction::RequiredPlaceholder => out.push(source.next().unwrap_or(' ')),
                TextAction::OptionalPlaceholder => {
                    if let Some(c) = source.next() {
                        out.push(c);
                    }
                }
            }
        }
        source.append_remaining(&mut out);
        out
    }
}

/// The custom text format (`FormatUtil.parseCustomTextFormat`).
#[derive(Clone, Debug)]
struct CustomTextFormat {
    main: TextFmt,
    empty: Option<TextFmt>,
}

impl CustomTextFormat {
    fn format(&self, value: &ExprValue) -> ExprResult<ExprValue> {
        if is_null_or_empty(value) {
            return Ok(ExprValue::Text(match &self.empty {
                Some(fmt) => fmt.format(""),
                None => String::new(),
            }));
        }
        let text = value.get_as_string()?;
        Ok(ExprValue::Text(self.main.format(&text)))
    }
}

fn flush_pending_text_literal(pending: &mut String, actions: &mut Vec<TextAction>) {
    if pending.is_empty() {
        return;
    }
    actions.push(TextAction::Literal(std::mem::take(pending)));
}

/// `FormatUtil.parseCustomTextFormat`.
fn parse_custom_text_format(text: &str) -> ExprResult<CustomTextFormat> {
    let mut buf = FormatBuf::new(text);
    let mut first: Option<TextFmt> = None;
    let mut actions: Vec<TextAction> = Vec::new();
    let mut num_placeholders = 0usize;
    let mut right_aligned = true;
    let mut case = TextCase::None;
    let mut pending = String::new();
    let mut has_fmt_chars = false;

    loop {
        if !buf.has_next() {
            break;
        }
        let c = buf.next();
        has_fmt_chars = true;
        match format_code_type(c) {
            FormatCodeType::General => match c {
                '!' => right_aligned = false,
                '"' => {
                    let quoted = parse_format_string_until(&mut buf, None, '"', true)?;
                    pending.push_str(&quoted);
                }
                '[' => {
                    parse_format_string_until(&mut buf, Some('['), ']', false)?;
                }
                '\\' => {
                    if buf.has_next() {
                        pending.push(buf.next());
                    }
                }
                '*' => {
                    if buf.has_next() {
                        buf.next();
                    }
                }
                ';' => {
                    if first.is_some() {
                        break;
                    }
                    flush_pending_text_literal(&mut pending, &mut actions);
                    first = Some(TextFmt {
                        actions: std::mem::take(&mut actions),
                        num_placeholders,
                        right_aligned,
                        case,
                    });
                    num_placeholders = 0;
                    right_aligned = true;
                    case = TextCase::None;
                    has_fmt_chars = false;
                }
                other => pending.push(other),
            },
            FormatCodeType::Text => match c {
                '@' => {
                    flush_pending_text_literal(&mut pending, &mut actions);
                    num_placeholders += 1;
                    actions.push(TextAction::RequiredPlaceholder);
                }
                '&' => {
                    flush_pending_text_literal(&mut pending, &mut actions);
                    num_placeholders += 1;
                    actions.push(TextAction::OptionalPlaceholder);
                }
                '>' => {
                    case = if case == TextCase::Lower {
                        TextCase::None
                    } else {
                        TextCase::Upper
                    };
                }
                '<' => {
                    case = if case == TextCase::Upper {
                        TextCase::None
                    } else {
                        TextCase::Lower
                    };
                }
                other => pending.push(other),
            },
            _ => pending.push(c),
        }
    }

    flush_pending_text_literal(&mut pending, &mut actions);
    match first {
        None => Ok(CustomTextFormat {
            main: TextFmt {
                actions,
                num_placeholders,
                right_aligned,
                case,
            },
            empty: None,
        }),
        Some(main) => Ok(CustomTextFormat {
            main,
            empty: if has_fmt_chars {
                Some(TextFmt {
                    actions,
                    num_placeholders,
                    right_aligned,
                    case,
                })
            } else {
                None
            },
        }),
    }
}

// --- general ("yes/no") formats ---------------------------------------------

/// The literal-choice format (`FormatUtil.CustomGeneralFmt`).
#[derive(Clone, Debug)]
struct CustomGeneralFormat {
    pos: ExprValue,
    neg: ExprValue,
    zero: ExprValue,
    null: ExprValue,
}

impl CustomGeneralFormat {
    fn format(&self, value: &ExprValue) -> ExprResult<ExprValue> {
        if value.is_null() {
            return Ok(self.null.clone());
        }
        let bd = coerce_to_big_decimal(value)?;
        Ok(match bd.sign() {
            Sign::Plus => self.pos.clone(),
            Sign::Minus => self.neg.clone(),
            Sign::NoSign => self.zero.clone(),
        })
    }
}

/// `FormatUtil.addCustomGeneralFormat` / `addCustomNumberFormat`: store a
/// literal-only subpattern.
fn push_general_subpattern(strs: &mut Vec<String>, idx: usize, sb: &mut String) {
    if sb.is_empty() {
        match idx {
            1 => {
                sb.push('-');
                sb.push_str(&strs[0]);
            }
            2 => {
                sb.push_str(&strs[0]);
            }
            _ => {}
        }
    }
    strs.push(std::mem::take(sb));
}

/// `FormatUtil.parseCustomGeneralFormat`.
fn parse_custom_general_format(text: &str) -> ExprResult<CustomGeneralFormat> {
    let mut buf = FormatBuf::new(text);
    let mut strs: Vec<String> = Vec::new();
    let mut sb = String::new();
    let mut idx = 0usize;

    while buf.has_next() {
        let c = buf.next();
        match format_code_type(c) {
            FormatCodeType::General => match c {
                '!' => {}
                '"' => {
                    let quoted = parse_format_string_until(&mut buf, None, '"', true)?;
                    sb.push_str(&quoted);
                }
                '[' => {
                    parse_format_string_until(&mut buf, Some('['), ']', false)?;
                }
                '\\' => {
                    if buf.has_next() {
                        sb.push(buf.next());
                    }
                }
                '*' => {
                    if buf.has_next() {
                        buf.next();
                    }
                }
                ';' => {
                    if idx == 3 {
                        break;
                    }
                    push_general_subpattern(&mut strs, idx, &mut sb);
                    idx += 1;
                }
                other => sb.push(other),
            },
            _ => sb.push(c),
        }
    }

    while idx < 4 {
        push_general_subpattern(&mut strs, idx, &mut sb);
        idx += 1;
    }

    Ok(CustomGeneralFormat {
        // `parseCustomGeneralFormat` stores the raw subpattern text (there is
        // no `DecimalFormat` quoting in this path)
        pos: ExprValue::Text(std::mem::take(&mut strs[0])),
        neg: ExprValue::Text(std::mem::take(&mut strs[1])),
        zero: ExprValue::Text(std::mem::take(&mut strs[2])),
        null: ExprValue::Text(std::mem::take(&mut strs[3])),
    })
}

/// `DefaultFunctions.getOptionalTriStateBoolean`: 0 = false, -1 = true,
/// -2 = use the default.
fn optional_tri_state(args: &[ExprValue], idx: usize, default: bool) -> ExprResult<bool> {
    if args.len() <= idx {
        return Ok(default);
    }
    match args[idx].get_as_long()? {
        0 => Ok(false),
        -1 => Ok(true),
        -2 => Ok(default),
        other => eval_err(format!("Unsupported tri-state boolean value {other}")),
    }
}

fn format_number_function(args: &[ExprValue], pattern: NumPatternType) -> ExprResult<ExprValue> {
    expect_arity(args, 1, 6)?;
    if args[0].is_null() {
        return Ok(ExprValue::Null);
    }
    let num_dec_digits = optional_int(args, 1, 2, -1)?;
    let inc_lead_digit = optional_tri_state(args, 2, true)?;
    // Currency patterns default to parentheses for negative values
    // (`NumericConfig.useParensForCurrencyNegatives`), the other patterns do
    // not (`useParensForNegatives`)
    let neg_parens = optional_tri_state(args, 3, pattern == NumPatternType::Currency)?;
    let group_digits = if optional_tri_state(args, 4, true)? {
        3
    } else {
        0
    };
    let bd = as_big_decimal(&args[0])?;
    let text = format_number_pattern(
        &bd,
        pattern,
        num_dec_digits,
        inc_lead_digit,
        neg_parens,
        group_digits,
    )?;
    Ok(ExprValue::Text(text))
}

/// `Args.coerceToDateTimeValue`: temporal values are returned unchanged,
/// everything else is converted through the date/time coercion (with the
/// special "true"/"false" string handling of the Format family).
fn coerce_to_date_time_value(value: &ExprValue) -> ExprResult<ExprValue> {
    if value.kind().is_temporal() {
        return Ok(value.clone());
    }
    let mut coerced = value.clone();
    if let ExprValue::Text(text) = &coerced {
        if text.eq_ignore_ascii_case("true") {
            coerced = ExprValue::Long(-1);
        } else if text.eq_ignore_ascii_case("false") {
            coerced = ExprValue::Long(0);
        }
    }
    Ok(ExprValue::from_local_date_time(
        coerced.get_as_local_date_time()?,
    ))
}

/// `Args.coerceToNumberValue`.
fn coerce_to_number_value(value: &ExprValue) -> ExprResult<ExprValue> {
    if value.kind().is_numeric() {
        return Ok(value.clone());
    }
    if let ExprValue::Text(text) = value {
        if text.eq_ignore_ascii_case("true") {
            return Ok(ExprValue::Long(-1));
        }
        if text.eq_ignore_ascii_case("false") {
            return Ok(ExprValue::Long(0));
        }
        if let Ok(decimal) = parse_string_decimal(text) {
            return Ok(ExprValue::Decimal(decimal));
        }
        if let Ok(date_time) = value.get_as_local_date_time() {
            return Ok(ExprValue::Double(datenums::to_date_double(date_time)));
        }
        return eval_err("invalid number value");
    }
    Ok(ExprValue::Double(value.get_as_double()?))
}

/// A predefined `Format` name.  The caller has already handled the null /
/// empty string case (every predef returns the empty string for those).
fn format_predef_function(name: &str, value: &ExprValue) -> ExprResult<Option<ExprValue>> {
    let result = match name {
        "General Date" => {
            let coerced = coerce_to_date_time_value(value)?;
            let ExprValue::DateTime { kind, value } = &coerced else {
                return Ok(Some(coerced));
            };
            ExprValue::Text(format_general_date_us(*kind, *value))
        }
        "Long Date" => {
            let coerced = coerce_to_date_time_value(value)?;
            ExprValue::Text(format_long_date_us(coerced.get_as_local_date_time()?))
        }
        "Medium Date" => {
            let coerced = coerce_to_date_time_value(value)?;
            ExprValue::Text(format_medium_date_us(coerced.get_as_local_date_time()?))
        }
        "Short Date" => {
            let coerced = coerce_to_date_time_value(value)?;
            ExprValue::Text(format_short_date_us(coerced.get_as_local_date_time()?))
        }
        "Long Time" => {
            let coerced = coerce_to_date_time_value(value)?;
            ExprValue::Text(format_long_time_us(coerced.get_as_local_date_time()?))
        }
        "Medium Time" => {
            let coerced = coerce_to_date_time_value(value)?;
            ExprValue::Text(format_medium_time_us(coerced.get_as_local_date_time()?))
        }
        "Short Time" => {
            let coerced = coerce_to_date_time_value(value)?;
            ExprValue::Text(format_short_time_us(coerced.get_as_local_date_time()?))
        }
        "General Number" => ExprValue::Text(format_general_number(value)?),
        "Currency" | "Euro" | "Fixed" | "Standard" | "Percent" => {
            let bd = coerce_to_big_decimal(value)?;
            let (pattern, group_digits) = match name {
                "Currency" => (NumPatternType::Currency, 3),
                "Euro" => (NumPatternType::Euro, 3),
                "Fixed" => (NumPatternType::General, 0),
                "Percent" => (NumPatternType::Percent, 0),
                _ => (NumPatternType::General, 3),
            };
            ExprValue::Text(format_number_pattern(
                &bd,
                pattern,
                2,
                true,
                pattern == NumPatternType::Currency || pattern == NumPatternType::Euro,
                group_digits,
            )?)
        }
        "Scientific" => {
            let bd = coerce_to_big_decimal(value)?;
            ExprValue::Text(format_scientific_pattern(&bd, 2)?)
        }
        "True/False" | "Yes/No" | "On/Off" => {
            let coerced = coerce_to_number_value(value)?;
            let true_value = coerced.get_as_boolean()?;
            let (yes, no) = match name {
                "True/False" => ("True", "False"),
                "Yes/No" => ("Yes", "No"),
                _ => ("On", "Off"),
            };
            ExprValue::Text(if true_value { yes } else { no }.to_string())
        }
        _ => return Ok(None),
    };
    Ok(Some(result))
}

/// The names registered in `FormatUtil.PREDEF_FMTS`.
fn is_predef_format(name: &str) -> bool {
    matches!(
        name,
        "General Date"
            | "Long Date"
            | "Medium Date"
            | "Short Date"
            | "Long Time"
            | "Medium Time"
            | "Short Time"
            | "General Number"
            | "Currency"
            | "Euro"
            | "Fixed"
            | "Standard"
            | "Percent"
            | "Scientific"
            | "True/False"
            | "Yes/No"
            | "On/Off"
    )
}

fn format_function(args: &[ExprValue], _ctx: &EvalContext) -> ExprResult<ExprValue> {
    expect_arity(args, 1, 4)?;
    if args.len() < 2 {
        // no formatting, do simple string conversion
        if args[0].is_null() {
            return Ok(ExprValue::Null);
        }
        return Ok(ExprValue::Text(args[0].get_as_string()?));
    }
    let fmt_str = args[1].get_as_string()?;
    // Jackcess reads these before it looks at the format string; only the
    // custom formats use them
    let first_day = optional_int(args, 2, 1, 0)?;
    let first_week_type = optional_int(args, 3, 1, 0)?;

    if !is_predef_format(&fmt_str) {
        if fmt_str.is_empty() {
            // FormatUtil.DUMMY_FMT
            return Ok(if args[0].is_null() {
                ExprValue::Text(String::new())
            } else {
                args[0].clone()
            });
        }
        // `createFormat` parses the pattern before `Args.format` runs, so a
        // pattern error is not turned into the "return the value as is" case
        let fmt = parse_custom_format(&fmt_str, first_day, first_week_type)?;
        return match fmt.apply(&args[0]) {
            Ok(value) => Ok(value),
            // FormatUtil.Args.format: a value which cannot be formatted as
            // the target type is returned as is
            Err(ExprError::Eval(_)) => Ok(args[0].clone()),
            Err(error) => Err(error),
        };
    }

    // predefined formats return the empty string for null/empty expressions
    let is_empty =
        args[0].is_null() || matches!(&args[0], ExprValue::Text(text) if text.is_empty());
    if is_empty {
        return Ok(ExprValue::Text(String::new()));
    }

    match format_predef_function(&fmt_str, &args[0]) {
        Ok(Some(value)) => Ok(value),
        Ok(None) => unsupported(format!(
            "Format format '{fmt_str}' is not implemented by the expression engine port"
        )),
        // FormatUtil.Args.format: a value which cannot be converted to the
        // format's target type is returned as is
        Err(ExprError::Eval(_)) => Ok(args[0].clone()),
        Err(error) => Err(error),
    }
}

/// `FormatUtil.createStandaloneFormatter(...).format(value)` /
/// `FormatUtil.format(ctx, expr, fmtStr, firstDay, firstWeekType)`: applies a
/// `Format` pattern to a single value outside the expression engine.
///
/// This is what [`crate::columnfmt::ColumnFormatter`] uses; it has exactly the
/// semantics of the `Format(value, format, firstDay, firstWeekType)` builtin,
/// including the "value which cannot be formatted is returned as is" rule and
/// the empty/null format handling (`FormatUtil.DUMMY_FMT`).
pub fn format_value(
    fmt_str: &str,
    value: &ExprValue,
    first_day: i32,
    first_week_type: i32,
) -> ExprResult<ExprValue> {
    format_function(
        &[
            value.clone(),
            ExprValue::Text(fmt_str.to_string()),
            ExprValue::Long(first_day),
            ExprValue::Long(first_week_type),
        ],
        &EvalContext::new(),
    )
}

fn format_date_time_function(args: &[ExprValue]) -> ExprResult<ExprValue> {
    expect_arity(args, 1, 2)?;
    if args[0].is_null() {
        return Ok(ExprValue::Null);
    }
    let ldt = args[0].get_as_local_date_time()?;
    let format_type = optional_int(args, 1, 0, 0)?;
    let text = match format_type {
        0 => match date_kind_of(ldt) {
            // vbGeneralDate
            DateKind::Date => format_short_date_us(ldt),
            DateKind::Time => format_long_time_us(ldt),
            DateKind::DateTime => format_general_date_us(DateKind::DateTime, ldt),
        },
        1 => format_long_date_us(ldt),
        2 => format_short_date_us(ldt),
        3 => format_long_time_us(ldt),
        4 => format_short_time_us(ldt),
        other => return eval_err(format!("Unknown format {other}")),
    };
    Ok(ExprValue::Text(text))
}

fn evaluate_builtin(
    name: &str,
    args: &[ExprValue],
    result_kind: Option<ValueKind>,
    ctx: &EvalContext,
) -> ExprResult<ExprValue> {
    match name {
        "IIF" => {
            expect_arity(args, 3, 3)?;
            let condition = &args[0];
            if !condition.is_null() && condition.get_as_boolean()? {
                Ok(args[1].clone())
            } else {
                Ok(args[2].clone())
            }
        }
        "NZ" => {
            expect_arity(args, 1, 2)?;
            if !args[0].is_null() {
                return Ok(args[0].clone());
            }
            if args.len() > 1 {
                return Ok(args[1].clone());
            }
            Ok(match result_kind {
                None | Some(ValueKind::Text) => ExprValue::Text(String::new()),
                _ => ExprValue::Long(0),
            })
        }
        "ISNULL" => {
            expect_arity(args, 1, 1)?;
            Ok(ExprValue::bool(args[0].is_null()))
        }
        "ISDATE" => {
            expect_arity(args, 1, 1)?;
            if args[0].kind().is_temporal() {
                return Ok(ExprValue::Long(-1));
            }
            if args[0].kind().is_string() {
                let text = args[0].get_as_string()?;
                if string_to_date_value(&text)?.is_some() {
                    return Ok(ExprValue::Long(-1));
                }
            }
            Ok(ExprValue::Long(0))
        }
        "ISNUMERIC" => {
            expect_arity(args, 1, 1)?;
            if args[0].kind().is_numeric() {
                return Ok(ExprValue::Long(-1));
            }
            if args[0].kind().is_string() {
                let text = args[0].get_as_string()?;
                if parse_string_decimal(&text).is_ok() {
                    return Ok(ExprValue::Long(-1));
                }
            }
            Ok(ExprValue::Long(0))
        }
        "TYPENAME" => {
            expect_arity(args, 1, 1)?;
            let name = match args[0].kind() {
                ValueKind::Null => "Null",
                ValueKind::Text => "String",
                ValueKind::Date | ValueKind::Time | ValueKind::DateTime => "Date",
                ValueKind::Long => "Long",
                ValueKind::Double => "Double",
                ValueKind::Decimal => "Decimal",
            };
            Ok(ExprValue::Text(name.to_string()))
        }
        "VARTYPE" => {
            expect_arity(args, 1, 1)?;
            let value = match args[0].kind() {
                ValueKind::Null => 1,
                ValueKind::Text => 8,
                ValueKind::Date | ValueKind::Time | ValueKind::DateTime => 7,
                ValueKind::Long => 3,
                ValueKind::Double => 5,
                ValueKind::Decimal => 14,
            };
            Ok(ExprValue::Long(value))
        }
        "ABS" => {
            expect_arity(args, 1, 1)?;
            let value = &args[0];
            if value.is_null() {
                return Ok(ExprValue::Null);
            }
            match value.kind() {
                kind @ (ValueKind::Date | ValueKind::Time | ValueKind::DateTime) => {
                    let date_kind = date_kind_of_value(kind).unwrap();
                    let result = value.get_as_double()?.abs();
                    Ok(to_date_value_if_possible(date_kind, result))
                }
                ValueKind::Long => Ok(ExprValue::Long(value.get_as_long()?.wrapping_abs())),
                ValueKind::Double => Ok(ExprValue::Double(value.get_as_double()?.abs())),
                ValueKind::Text | ValueKind::Decimal => {
                    Ok(ExprValue::Decimal(value.get_as_decimal()?.abs()))
                }
                _ => eval_err("Unexpected type for Abs"),
            }
        }
        "INT" => {
            expect_arity(args, 1, 1)?;
            let value = &args[0];
            if value.is_null() {
                return Ok(ExprValue::Null);
            }
            if value.kind().is_integral() {
                return Ok(value.clone());
            }
            Ok(ExprValue::Long(value.get_as_double()?.floor() as i32))
        }
        "FIX" => {
            expect_arity(args, 1, 1)?;
            let value = &args[0];
            if value.is_null() {
                return Ok(ExprValue::Null);
            }
            if value.kind().is_integral() {
                return Ok(value.clone());
            }
            Ok(ExprValue::Long(value.get_as_double()? as i32))
        }
        "SGN" => {
            expect_arity(args, 1, 1)?;
            let value = &args[0];
            if value.is_null() {
                return Ok(ExprValue::Null);
            }
            let signum = if value.kind().is_integral() {
                value.get_as_long()?.signum()
            } else {
                value.get_as_decimal()?.signum()
            };
            Ok(ExprValue::Long(signum))
        }
        "ROUND" => {
            expect_arity(args, 1, 2)?;
            let value = &args[0];
            if value.is_null() {
                return Ok(ExprValue::Null);
            }
            if value.kind().is_integral() {
                return Ok(value.clone());
            }
            let scale = if args.len() > 1 {
                args[1].get_as_long()?
            } else {
                0
            };
            Ok(ExprValue::Decimal(
                value.get_as_decimal()?.round_to_scale(scale)?,
            ))
        }
        "SQR" => {
            expect_arity(args, 1, 1)?;
            let value = args[0].get_as_double()?;
            if value < 0.0 {
                return eval_err(format!("Invalid value '{value}'"));
            }
            Ok(ExprValue::Double(value.sqrt()))
        }
        "SIN" => unary_double(args, f64::sin),
        "COS" => unary_double(args, f64::cos),
        "TAN" => unary_double(args, f64::tan),
        "ATN" => unary_double(args, f64::atan),
        "LOG" => unary_double(args, f64::ln),
        "EXP" => unary_double(args, f64::exp),
        "HEX" => {
            expect_arity(args, 1, 1)?;
            let value = &args[0];
            if value.is_null() {
                return Ok(ExprValue::Null);
            }
            if value.kind().is_string() && value.get_as_string()?.is_empty() {
                return Ok(ExprValue::Long(0));
            }
            let lv = value.get_as_long()?;
            Ok(ExprValue::Text(format!("{:X}", lv as u32)))
        }
        "OCT" => {
            expect_arity(args, 1, 1)?;
            let value = &args[0];
            if value.is_null() {
                return Ok(ExprValue::Null);
            }
            if value.kind().is_string() && value.get_as_string()?.is_empty() {
                return Ok(ExprValue::Long(0));
            }
            let lv = value.get_as_long()?;
            Ok(ExprValue::Text(format!("{:o}", lv as u32)))
        }
        "STR" => {
            expect_arity(args, 1, 1)?;
            let value = &args[0];
            if value.is_null() {
                return Ok(ExprValue::Null);
            }
            let decimal = value.get_as_decimal()?;
            let text = decimal.to_plain_string();
            Ok(ExprValue::Text(if decimal.signum() >= 0 {
                format!(" {text}")
            } else {
                text
            }))
        }
        "VAL" => {
            expect_arity(args, 1, 1)?;
            let value = &args[0];
            if value.is_null() {
                return Ok(ExprValue::Null);
            }
            val_value(&value.get_as_string()?)
        }
        "SPACE" => {
            expect_arity(args, 1, 1)?;
            let count = args[0].get_as_long()?;
            if count < 0 {
                return eval_err(format!("Invalid value '{count}'"));
            }
            Ok(ExprValue::Text(" ".repeat(count as usize)))
        }
        "STRING" => {
            expect_arity(args, 2, 2)?;
            if args[0].is_null() || args[1].is_null() {
                return Ok(ExprValue::Null);
            }
            let count = args[0].get_as_long()?;
            if count < 0 {
                return eval_err(format!("Invalid value '{count}'"));
            }
            let text = args[1].get_as_string()?;
            let unit = first_utf16_unit(&text)
                .ok_or_else(|| ExprError::Eval("No characters in string".into()))?;
            let c = char::from_u32((unit % 256) as u32)
                .ok_or_else(|| ExprError::Eval("Invalid character".into()))?;
            Ok(ExprValue::Text(
                std::iter::repeat_n(c, count as usize).collect(),
            ))
        }
        "REPLACE" => {
            expect_arity(args, 3, 6)?;
            let text_str = args[0].get_as_string()?;
            let search_str = args[1].get_as_string()?;
            let replacement = args[2].get_as_string()?;
            if has_non_bmp(&text_str) || has_non_bmp(&search_str) || has_non_bmp(&replacement) {
                return unsupported("Replace of non-BMP text is not implemented".to_string());
            }
            let text: Vec<char> = text_str.chars().collect();
            let search: Vec<char> = search_str.chars().collect();
            let start = optional_int(args, 3, 1, 1)? - 1;
            let mut count = optional_int(args, 4, -1, -1)?;
            let ignore_case = optional_ignore_case(args, 5)?;
            let text_len = text.len();
            if start < 0 {
                return eval_err("string index out of range");
            }
            if start as usize >= text_len {
                return Ok(ExprValue::Text(String::new()));
            }
            let start = start as usize;
            if search.is_empty() || count == 0 {
                let result: String = text[if start > 0 { start } else { 0 }..].iter().collect();
                return Ok(ExprValue::Text(result));
            }
            if count < 0 {
                count = text_len as i32;
            }
            let mut result = String::new();
            let mut matches = 0;
            let mut i = start;
            while i < text_len {
                if matches < count && region_matches(&text, i, &search, ignore_case) {
                    result.push_str(&replacement);
                    matches += 1;
                    i += search.len();
                } else {
                    result.push(text[i]);
                    i += 1;
                }
            }
            Ok(ExprValue::Text(result))
        }
        "STRREVERSE" => {
            expect_arity(args, 1, 1)?;
            let text = args[0].get_as_string()?;
            Ok(ExprValue::Text(text.chars().rev().collect()))
        }
        "INSTR" => {
            expect_arity(args, 2, 4)?;
            let mut idx = 0;
            let mut start: i32 = 0;
            if args.len() > 2 {
                start = args[0].get_as_long()? - 1;
                idx = 1;
            }
            let haystack_value = &args[idx];
            if haystack_value.is_null() {
                return Ok(haystack_value.clone());
            }
            let haystack_str = haystack_value.get_as_string()?;
            if has_non_bmp(&haystack_str) {
                return unsupported("InStr of non-BMP text is not implemented".to_string());
            }
            let haystack: Vec<char> = haystack_str.chars().collect();
            if haystack.is_empty() {
                return Ok(ExprValue::Long(0));
            }
            let needle_value = &args[idx + 1];
            if needle_value.is_null() {
                return Ok(needle_value.clone());
            }
            let needle_str = needle_value.get_as_string()?;
            if has_non_bmp(&needle_str) {
                return unsupported("InStr of non-BMP text is not implemented".to_string());
            }
            let needle: Vec<char> = needle_str.chars().collect();
            if needle.is_empty() {
                return Ok(ExprValue::Long(start + 1));
            }
            let ignore_case = optional_ignore_case(args, idx + 2)?;
            let end = haystack.len().saturating_sub(needle.len());
            let mut pos = start;
            // Jackcess' loop is `while(start < end)`, so a match at the very
            // end of the haystack is not found, and a start before the string
            // simply never matches until it walks into range
            while (pos as i64) < end as i64 {
                if pos >= 0 && region_matches(&haystack, pos as usize, &needle, ignore_case) {
                    return Ok(ExprValue::Long(pos + 1));
                }
                pos += 1;
            }
            Ok(ExprValue::Long(0))
        }
        "INSTRREV" => {
            expect_arity(args, 2, 4)?;
            let haystack_value = &args[0];
            if haystack_value.is_null() {
                return Ok(haystack_value.clone());
            }
            let haystack_str = haystack_value.get_as_string()?;
            if has_non_bmp(&haystack_str) {
                return unsupported("InStrRev of non-BMP text is not implemented".to_string());
            }
            let haystack: Vec<char> = haystack_str.chars().collect();
            if haystack.is_empty() {
                return Ok(ExprValue::Long(0));
            }
            let needle_value = &args[1];
            if needle_value.is_null() {
                return Ok(needle_value.clone());
            }
            let needle_str = needle_value.get_as_string()?;
            if has_non_bmp(&needle_str) {
                return unsupported("InStrRev of non-BMP text is not implemented".to_string());
            }
            let needle: Vec<char> = needle_str.chars().collect();
            let mut start = haystack.len() as i32 - 1;
            if needle.is_empty() {
                return Ok(ExprValue::Long(start + 1));
            }
            if args.len() > 2 {
                start = args[2].get_as_long()?;
                if start == -1 {
                    start = haystack.len() as i32;
                }
                start -= 1;
            }
            let ignore_case = optional_ignore_case(args, 3)?;
            let limit =
                (haystack.len() as i32 - needle.len() as i32).min(start - needle.len() as i32 + 1);
            let mut pos = limit;
            while pos >= 0 {
                if region_matches(&haystack, pos as usize, &needle, ignore_case) {
                    return Ok(ExprValue::Long(pos + 1));
                }
                pos -= 1;
            }
            Ok(ExprValue::Long(0))
        }
        "STRCOMP" => {
            expect_arity(args, 2, 3)?;
            if args[0].is_null() || args[1].is_null() {
                return Ok(ExprValue::Null);
            }
            let ignore_case = optional_ignore_case(args, 2)?;
            let left = args[0].get_as_string()?;
            let right = args[1].get_as_string()?;
            if has_non_bmp(&left) || has_non_bmp(&right) {
                return unsupported("StrComp of non-BMP text is not implemented".to_string());
            }
            let ordering = if ignore_case {
                compare_ignore_case(&left, &right)
            } else {
                compare_case_sensitive(&left, &right)
            };
            Ok(ExprValue::Long(match ordering {
                Ordering::Less => -1,
                Ordering::Equal => 0,
                Ordering::Greater => 1,
            }))
        }
        "LEN" => {
            expect_arity(args, 1, 1)?;
            if args[0].is_null() {
                return Ok(ExprValue::Null);
            }
            Ok(ExprValue::Long(utf16_len(&args[0].get_as_string()?) as i32))
        }
        "LEFT" => {
            expect_arity(args, 2, 2)?;
            if args[0].is_null() {
                return Ok(ExprValue::Null);
            }
            let text = args[0].get_as_string()?;
            let len = (utf16_len(&text) as i64).min(args[1].get_as_long()? as i64);
            if len < 0 {
                return eval_err("string index out of range");
            }
            Ok(ExprValue::Text(utf16_slice(&text, 0, len as usize)?))
        }
        "RIGHT" => {
            expect_arity(args, 2, 2)?;
            if args[0].is_null() {
                return Ok(ExprValue::Null);
            }
            let text = args[0].get_as_string()?;
            let text_len = utf16_len(&text);
            let len = (text_len as i64).min(args[1].get_as_long()? as i64);
            if len < 0 {
                return eval_err("string index out of range");
            }
            Ok(ExprValue::Text(utf16_slice(
                &text,
                text_len - len as usize,
                text_len,
            )?))
        }
        "MID" => {
            expect_arity(args, 2, 3)?;
            if args[0].is_null() {
                return Ok(ExprValue::Null);
            }
            let text = args[0].get_as_string()?;
            let text_len = utf16_len(&text) as i64;
            let start = text_len.min(args[1].get_as_long()? as i64 - 1);
            let len = if args.len() > 2 {
                (args[2].get_as_long()? as i64).min(text_len - start)
            } else {
                text_len - start
            };
            if start < 0 || len < 0 {
                return eval_err("string index out of range");
            }
            Ok(ExprValue::Text(utf16_slice(
                &text,
                start as usize,
                (start + len) as usize,
            )?))
        }
        "LCASE" => {
            expect_arity(args, 1, 1)?;
            if args[0].is_null() {
                return Ok(ExprValue::Null);
            }
            Ok(ExprValue::Text(args[0].get_as_string()?.to_lowercase()))
        }
        "UCASE" => {
            expect_arity(args, 1, 1)?;
            if args[0].is_null() {
                return Ok(ExprValue::Null);
            }
            Ok(ExprValue::Text(args[0].get_as_string()?.to_uppercase()))
        }
        "TRIM" => {
            expect_arity(args, 1, 1)?;
            if args[0].is_null() {
                return Ok(ExprValue::Null);
            }
            Ok(ExprValue::Text(trim_spaces(
                &args[0].get_as_string()?,
                true,
                true,
            )))
        }
        "LTRIM" => {
            expect_arity(args, 1, 1)?;
            if args[0].is_null() {
                return Ok(ExprValue::Null);
            }
            Ok(ExprValue::Text(trim_spaces(
                &args[0].get_as_string()?,
                true,
                false,
            )))
        }
        "RTRIM" => {
            expect_arity(args, 1, 1)?;
            if args[0].is_null() {
                return Ok(ExprValue::Null);
            }
            Ok(ExprValue::Text(trim_spaces(
                &args[0].get_as_string()?,
                false,
                true,
            )))
        }
        "CHR" => {
            expect_arity(args, 1, 1)?;
            if args[0].is_null() {
                return Ok(ExprValue::Null);
            }
            let code = args[0].get_as_long()?;
            if !(0..=255).contains(&code) {
                return eval_err(format!("Character code '{code}' out of range"));
            }
            let c = char::from_u32(code as u32)
                .ok_or_else(|| ExprError::Eval("Invalid character".into()))?;
            Ok(ExprValue::Text(c.to_string()))
        }
        "CHRW" => {
            expect_arity(args, 1, 1)?;
            if args[0].is_null() {
                return Ok(ExprValue::Null);
            }
            let code = args[0].get_as_long()?;
            let c = u32::try_from(code)
                .ok()
                .and_then(char::from_u32)
                .ok_or_else(|| ExprError::Eval(format!("Invalid character code '{code}'")))?;
            Ok(ExprValue::Text(c.to_string()))
        }
        "ASC" | "ASCW" => {
            expect_arity(args, 1, 1)?;
            let text = args[0].get_as_string()?;
            let unit = first_utf16_unit(&text)
                .ok_or_else(|| ExprError::Eval("No characters in string".into()))?;
            if name == "ASC" && unit > 255 {
                return eval_err(format!("Character code '{unit}' out of range"));
            }
            Ok(ExprValue::Long(unit as i32))
        }
        "CSTR" => {
            expect_arity(args, 1, 1)?;
            Ok(ExprValue::Text(args[0].get_as_string()?))
        }
        "CLNG" => {
            expect_arity(args, 1, 1)?;
            Ok(ExprValue::Long(args[0].get_as_long()?))
        }
        "CDBL" => {
            expect_arity(args, 1, 1)?;
            Ok(ExprValue::Double(args[0].get_as_double()?))
        }
        "CINT" => {
            expect_arity(args, 1, 1)?;
            let value = args[0].get_as_long()?;
            if !(i16::MIN as i32..=i16::MAX as i32).contains(&value) {
                return eval_err(format!("Int value '{value}' out of range"));
            }
            Ok(ExprValue::Long(value))
        }
        "CBYTE" => {
            expect_arity(args, 1, 1)?;
            let value = args[0].get_as_long()?;
            if !(0..=255).contains(&value) {
                return eval_err(format!("Byte code '{value}' out of range"));
            }
            Ok(ExprValue::Long(value))
        }
        "CBOOL" => {
            expect_arity(args, 1, 1)?;
            Ok(ExprValue::bool(args[0].get_as_boolean()?))
        }
        "CDEC" => {
            expect_arity(args, 1, 1)?;
            Ok(ExprValue::Decimal(args[0].get_as_decimal()?))
        }
        "CCUR" => {
            expect_arity(args, 1, 1)?;
            Ok(ExprValue::Decimal(
                args[0].get_as_decimal()?.round_to_scale(4)?,
            ))
        }
        "CSNG" => {
            expect_arity(args, 1, 1)?;
            let value = args[0].get_as_double()?;
            const FLOAT_MIN: f64 = 1.401_298_464_324_817e-45;
            const FLOAT_MAX: f64 = 3.402_823_466_385_288_6e38;
            if value < FLOAT_MIN || value > FLOAT_MAX {
                return eval_err(format!("Single value '{value}' out of range"));
            }
            Ok(ExprValue::Double((value as f32) as f64))
        }
        "CDATE" | "CVDATE" => {
            expect_arity(args, 1, 1)?;
            match &args[0] {
                value @ ExprValue::DateTime { .. } => Ok(value.clone()),
                value => Ok(ExprValue::from_local_date_time(
                    value.get_as_local_date_time()?,
                )),
            }
        }
        "CVAR" => {
            expect_arity(args, 1, 1)?;
            Ok(args[0].clone())
        }
        "YEAR" => date_part(args, |ldt| ldt.year),
        "MONTH" => date_part(args, |ldt| ldt.month as i32),
        "DAY" => date_part(args, |ldt| ldt.day as i32),
        "HOUR" => date_part(args, |ldt| ldt.hour as i32),
        "MINUTE" => date_part(args, |ldt| ldt.minute as i32),
        "SECOND" => date_part(args, |ldt| ldt.second as i32),
        "DATESERIAL" => {
            expect_arity(args, 3, 3)?;
            let year = args[0].get_as_long()?;
            let month = args[1].get_as_long()?;
            let day = args[2].get_as_long()?;
            Ok(ExprValue::date(date_from_serial(year, month, day)?))
        }
        "TIMESERIAL" => {
            expect_arity(args, 3, 3)?;
            let hour = args[0].get_as_long()?;
            let minute = args[1].get_as_long()?;
            let second = args[2].get_as_long()?;
            Ok(ExprValue::time(time_from_serial(hour, minute, second)))
        }
        "DATEVALUE" => {
            expect_arity(args, 1, 1)?;
            if args[0].is_null() {
                return Ok(ExprValue::Null);
            }
            if let ExprValue::DateTime {
                kind: DateKind::Date,
                ..
            } = &args[0]
            {
                return Ok(args[0].clone());
            }
            let ldt = args[0].get_as_local_date_time()?;
            Ok(ExprValue::date(LocalDateTime::of_date(
                ldt.year, ldt.month, ldt.day,
            )))
        }
        "TIMEVALUE" => {
            expect_arity(args, 1, 1)?;
            if args[0].is_null() {
                return Ok(ExprValue::Null);
            }
            if let ExprValue::DateTime {
                kind: DateKind::Time,
                ..
            } = &args[0]
            {
                return Ok(args[0].clone());
            }
            let ldt = args[0].get_as_local_date_time()?;
            let mut value = base_ldt();
            value.hour = ldt.hour;
            value.minute = ldt.minute;
            value.second = ldt.second;
            value.nano = ldt.nano;
            Ok(ExprValue::time(value))
        }
        "WEEKDAY" => {
            expect_arity(args, 1, 2)?;
            if args[0].is_null() {
                return Ok(ExprValue::Null);
            }
            let ldt = args[0].get_as_local_date_time()?;
            let day = weekday_from_epoch_day(ldt.to_epoch_day());
            let first_day = optional_int(args, 1, 1, 0)?;
            Ok(ExprValue::Long(day_of_week_to_weekday(day, first_day)))
        }
        "WEEKDAYNAME" => {
            expect_arity(args, 1, 3)?;
            if args[0].is_null() {
                return Ok(ExprValue::Null);
            }
            let weekday = args[0].get_as_long()?;
            let abbreviate = optional_bool(args, 1)?;
            let first_day = optional_int(args, 2, 1, 0)?;
            let day = weekday_to_day_of_week(weekday, first_day);
            let index = (day - 1).rem_euclid(7) as usize;
            Ok(ExprValue::Text(if abbreviate {
                WEEKDAY_ABBREVIATIONS[index].to_string()
            } else {
                WEEKDAY_NAMES[index].to_string()
            }))
        }
        "MONTHNAME" => {
            expect_arity(args, 1, 2)?;
            if args[0].is_null() {
                return Ok(ExprValue::Null);
            }
            let month = args[0].get_as_long()?;
            if !(1..=12).contains(&month) {
                return eval_err(format!("Invalid value '{month}'"));
            }
            let abbreviate = optional_bool(args, 1)?;
            let index = (month - 1) as usize;
            Ok(ExprValue::Text(if abbreviate {
                MONTH_ABBREVIATIONS[index].to_string()
            } else {
                MONTH_NAMES[index].to_string()
            }))
        }
        "CHOOSE" => {
            expect_arity(args, 1, usize::MAX)?;
            let index = args[0].get_as_long()?;
            if index < 1 || index as usize >= args.len() {
                return Ok(ExprValue::Null);
            }
            Ok(args[index as usize].clone())
        }
        "SWITCH" => {
            expect_arity(args, 0, usize::MAX)?;
            if !args.len().is_multiple_of(2) {
                return eval_err("Odd number of parameters");
            }
            let mut i = 0;
            while i < args.len() {
                if args[i].get_as_boolean()? {
                    return Ok(args[i + 1].clone());
                }
                i += 2;
            }
            Ok(ExprValue::Null)
        }
        "DATEPART" => date_part_function(args),
        "DATEADD" => date_add_function(args),
        "DATEDIFF" => date_diff_function(args),
        "DATE" => {
            expect_arity(args, 0, 0)?;
            let now = ctx.clock();
            Ok(ExprValue::date(LocalDateTime::of_date(
                now.year, now.month, now.day,
            )))
        }
        "NOW" => {
            expect_arity(args, 0, 0)?;
            Ok(ExprValue::date_time(ctx.clock()))
        }
        "TIME" => {
            expect_arity(args, 0, 0)?;
            let now = ctx.clock();
            let mut value = base_ldt();
            value.hour = now.hour;
            value.minute = now.minute;
            value.second = now.second;
            value.nano = now.nano;
            Ok(ExprValue::time(value))
        }
        "TIMER" => {
            expect_arity(args, 0, 0)?;
            let now = ctx.clock();
            let millis = now.second_of_day() * 1000 + (now.nano / 1_000_000) as i64;
            Ok(ExprValue::Double(millis as f64 / 1000.0))
        }
        "RND" => {
            expect_arity(args, 0, 1)?;
            let seed = if args.is_empty() {
                None
            } else {
                Some(args[0].get_as_long()?)
            };
            Ok(ExprValue::Double(ctx.get_random(seed) as f64))
        }
        "GENUNIQUEID" => {
            expect_arity(args, 0, 0)?;
            Ok(ExprValue::Long(ctx.get_unique_id()))
        }
        "GENGUID" => {
            expect_arity(args, 0, 0)?;
            Ok(ExprValue::Text(ctx.get_guid()))
        }
        "FV" | "PV" | "PMT" | "IPMT" | "PPMT" | "NPER" | "DDB" | "SLN" | "SYD" | "RATE" => {
            financial_function(name, args)
        }
        "STRCONV" => {
            expect_arity(args, 2, 3)?;
            if args[0].is_null() {
                return Ok(ExprValue::Null);
            }
            let text = args[0].get_as_string()?;
            let conversion = args[1].get_as_long()?;
            if args.len() > 2 {
                let locale_id = args[2].get_as_long()?;
                // LocaleUtil knows every Access locale id, but a locale other
                // than the US one changes the case mapping; that is not
                // reproduced here
                if locale_id != 1033 {
                    return unsupported(format!(
                        "StrConv locale id {locale_id} is not implemented by the expression engine port"
                    ));
                }
            }
            let case_conversion = conversion & 0x03;
            let char_conversion = conversion & !0x03;
            let text = match case_conversion {
                1 => text.to_uppercase(),
                2 => text.to_lowercase(),
                3 => capitalize_fully(&text),
                _ => text,
            };
            if char_conversion != 0 && char_conversion != 64 {
                return eval_err(format!(
                    "Unsupported character conversion {char_conversion}"
                ));
            }
            Ok(ExprValue::Text(text))
        }
        "FORMATNUMBER" => format_number_function(args, NumPatternType::General),
        "FORMATPERCENT" => format_number_function(args, NumPatternType::Percent),
        "FORMATCURRENCY" => format_number_function(args, NumPatternType::Currency),
        "FORMATDATETIME" => format_date_time_function(args),
        "FORMAT" => format_function(args, ctx),
        other => unsupported(format!(
            "function {other} is not implemented by the expression engine port"
        )),
    }
}

fn unary_double(args: &[ExprValue], f: fn(f64) -> f64) -> ExprResult<ExprValue> {
    expect_arity(args, 1, 1)?;
    Ok(ExprValue::Double(f(args[0].get_as_double()?)))
}

fn date_part(args: &[ExprValue], f: fn(LocalDateTime) -> i32) -> ExprResult<ExprValue> {
    expect_arity(args, 1, 1)?;
    if args[0].is_null() {
        return Ok(ExprValue::Null);
    }
    let ldt = args[0].get_as_local_date_time()?;
    Ok(ExprValue::Long(f(ldt)))
}

// ---------------------------------------------------------------------------
// evaluation context
// ---------------------------------------------------------------------------

/// Column values available to an expression, keyed by name (case
/// insensitively, like Access identifiers).
///
/// The context is deliberately independent of the database layer: the caller
/// (a calculated column, a default value or a validation rule) supplies the
/// already-converted values.
#[derive(Clone, Debug, Default)]
pub struct EvalContext {
    columns: Vec<(String, ExprValue)>,
    table_name: Option<String>,
    this_value: Option<ExprValue>,
    /// The result type of the expression (`CalcColEvalContext.getResultType`),
    /// used by `Nz` when it has no replacement value.
    pub result_type: Option<DataType>,
    /// Injected wall clock for the impure functions (`Date`, `Now`, `Time`,
    /// `Timer`, and the seed of an unseeded `Rnd`).  Jackcess reads the current
    /// time in the database's zone; this port has no zone database, so callers
    /// inject the local time they want (see [`EvalContext::set_clock`]).  When
    /// nothing is injected the system time in UTC is used.
    clock: Option<LocalDateTime>,
    /// The state of the `Rnd` function (`impl.expr.RandomContext`).
    random: Rc<RefCell<RandomState>>,
}

/// `java.util.Random`, reproduced bit for bit (48 bit linear congruential
/// generator), because `Rnd(seed)` returns `nextFloat()` values.
#[derive(Clone, Debug)]
struct JavaRandom {
    seed: u64,
}

impl JavaRandom {
    const MULTIPLIER: u64 = 0x5DEECE66D;
    const ADDEND: u64 = 0xB;
    const MASK: u64 = (1u64 << 48) - 1;

    fn new(seed: i64) -> JavaRandom {
        JavaRandom {
            seed: ((seed as u64) ^ JavaRandom::MULTIPLIER) & JavaRandom::MASK,
        }
    }

    fn next(&mut self, bits: u32) -> u32 {
        self.seed = (self
            .seed
            .wrapping_mul(JavaRandom::MULTIPLIER)
            .wrapping_add(JavaRandom::ADDEND))
            & JavaRandom::MASK;
        (self.seed >> (48 - bits)) as u32
    }

    /// `Random.nextFloat()`: `next(24) / (float)(1 << 24)`.
    fn next_float(&mut self) -> f32 {
        (self.next(24) as f32) / ((1u32 << 24) as f32)
    }
}

/// One entry of `RandomContext._rnds`.
#[derive(Clone, Debug)]
enum RndSource {
    /// `SimpleSource`: successive values of a seeded generator.
    Simple(JavaRandom),
}

/// `impl.expr.RandomContext`: the stateful logic behind `Rnd`.
#[derive(Clone, Debug)]
struct RandomState {
    def_rnd: Option<JavaRandom>,
    rnds: HashMap<i32, RndSource>,
    last_val: f32,
    /// The generator behind `GenGUID` (`UUID.randomUUID` in Jackcess).
    guid_rnd: Option<JavaRandom>,
    /// The generator behind `GenUniqueID` (`ThreadLocalRandom` in Jackcess).
    unique_rnd: Option<JavaRandom>,
}

impl Default for RandomState {
    fn default() -> RandomState {
        RandomState {
            def_rnd: None,
            rnds: HashMap::new(),
            // the value access uses for "last val" before any value was
            // returned (RandomContext._lastVal)
            last_val: 1.953_125E-02,
            guid_rnd: None,
            unique_rnd: None,
        }
    }
}

impl RandomState {
    fn set_last(&mut self, value: f32) -> f32 {
        self.last_val = value;
        value
    }

    /// `RandomContext.getRandom(Integer)`; `now` seeds the unseeded generator
    /// (Java uses `System.currentTimeMillis()`).
    fn get_random(&mut self, seed: Option<i32>, now: LocalDateTime) -> f32 {
        match seed {
            None => {
                if self.def_rnd.is_none() {
                    self.def_rnd = Some(JavaRandom::new(now.to_epoch_millis()));
                }
                let value = self.def_rnd.as_mut().unwrap().next_float();
                self.set_last(value)
            }
            Some(0) => {
                // LastValSource: return the last random value returned
                self.last_val
            }
            Some(seed) if seed < 0 => {
                // ResetSource: returns the same value every time and resets
                // all the other randoms
                self.rnds.clear();
                let value = JavaRandom::new(seed as i64).next_float();
                self.set_last(value)
            }
            Some(seed) => {
                self.rnds
                    .entry(seed)
                    .or_insert_with(|| RndSource::Simple(JavaRandom::new(seed as i64)));
                let value = match self.rnds.get_mut(&seed) {
                    Some(RndSource::Simple(rnd)) => rnd.next_float(),
                    None => unreachable!(),
                };
                self.set_last(value)
            }
        }
    }

    /// `GenUniqueID`: a random value over the whole 32 bit range
    /// (`ThreadLocalRandom.current().nextInt()`).
    fn next_unique_id(&mut self, now: LocalDateTime) -> i32 {
        if self.unique_rnd.is_none() {
            self.unique_rnd = Some(JavaRandom::new(
                now.to_epoch_millis() ^ 0x4745_4E55_4E49_5149,
            ));
        }
        self.unique_rnd.as_mut().unwrap().next(32) as i32
    }

    /// `GenGUID`: a random version 4 UUID formatted the way
    /// `ColumnImpl.toGUIDString` does.
    fn next_guid(&mut self, now: LocalDateTime) -> String {
        if self.guid_rnd.is_none() {
            self.guid_rnd = Some(JavaRandom::new(
                now.to_epoch_millis() ^ 0x4745_4E47_5549_4431,
            ));
        }
        let rnd = self.guid_rnd.as_mut().unwrap();
        let mut bytes = [0u8; 16];
        for chunk in bytes.chunks_mut(4) {
            chunk.copy_from_slice(&rnd.next(32).to_be_bytes());
        }
        // version 4 ("random") and the IETF variant bits of UUID.randomUUID
        bytes[6] = (bytes[6] & 0x0F) | 0x40;
        bytes[8] = (bytes[8] & 0x3F) | 0x80;
        format!(
            "{{{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
            bytes[0],
            bytes[1],
            bytes[2],
            bytes[3],
            bytes[4],
            bytes[5],
            bytes[6],
            bytes[7],
            bytes[8],
            bytes[9],
            bytes[10],
            bytes[11],
            bytes[12],
            bytes[13],
            bytes[14],
            bytes[15]
        )
    }
}

/// The current UTC wall clock as a `LocalDateTime` (the fallback for an
/// uninjected [`EvalContext::clock`]).
fn system_utc_date_time() -> LocalDateTime {
    let millis = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => duration.as_millis() as i64,
        Err(error) => -(error.duration().as_millis() as i64),
    };
    LocalDateTime::from_epoch_millis(millis)
}

impl EvalContext {
    /// An empty context.
    pub fn new() -> EvalContext {
        EvalContext::default()
    }

    /// Set a column value; `Bool` is normalised to Access' `-1`/`0` long.
    pub fn set_column(&mut self, name: &str, value: ExprValue) -> &mut EvalContext {
        let value = normalize_context_value(value);
        if let Some(entry) = self
            .columns
            .iter_mut()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
        {
            entry.0 = name.to_string();
            entry.1 = value;
        } else {
            self.columns.push((name.to_string(), value));
        }
        self
    }

    /// Builder version of [`EvalContext::set_column`].
    pub fn with_column(mut self, name: &str, value: ExprValue) -> EvalContext {
        self.set_column(name, value);
        self
    }

    /// Remove a column value.
    pub fn remove_column(&mut self, name: &str) -> Option<ExprValue> {
        let index = self
            .columns
            .iter()
            .position(|(existing, _)| existing.eq_ignore_ascii_case(name))?;
        Some(self.columns.remove(index).1)
    }

    /// Look up a column value by name (case insensitively).
    pub fn get_column(&self, name: &str) -> Option<&ExprValue> {
        self.columns
            .iter()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
            .map(|(_, value)| value)
    }

    /// The table name used to resolve `[Table].[Column]` references.
    pub fn set_table_name(&mut self, name: &str) -> &mut EvalContext {
        self.table_name = Some(name.to_string());
        self
    }

    /// The current field value for a validation rule (`ColValidatorEvalContext`
    /// / `getThisColumnValue`).
    pub fn set_this_value(&mut self, value: ExprValue) -> &mut EvalContext {
        self.this_value = Some(normalize_context_value(value));
        self
    }

    /// Inject the wall clock used by the impure functions (`Date`, `Now`,
    /// `Time`, `Timer` and the seed of an unseeded `Rnd`), which is what makes
    /// them testable.  Jackcess reads the current local time in the database's
    /// time zone; the expression engine here is time zone naive, so the caller
    /// supplies the local date/time.  With no injected clock the system UTC
    /// time is used.
    pub fn set_clock(&mut self, value: LocalDateTime) -> &mut EvalContext {
        self.clock = Some(value);
        self
    }

    /// Builder version of [`EvalContext::set_clock`].
    pub fn with_clock(mut self, value: LocalDateTime) -> EvalContext {
        self.clock = Some(value);
        self
    }

    /// The wall clock used by `Date`/`Now`/`Time`/`Timer`.
    pub fn clock(&self) -> LocalDateTime {
        self.clock.unwrap_or_else(system_utc_date_time)
    }

    /// `EvalContext.getRandom(Integer)`.
    fn get_random(&self, seed: Option<i32>) -> f32 {
        self.random.borrow_mut().get_random(seed, self.clock())
    }

    /// `GenUniqueID` (`ThreadLocalRandom.current().nextInt()`).
    fn get_unique_id(&self) -> i32 {
        self.random.borrow_mut().next_unique_id(self.clock())
    }

    /// `GenGUID` (`ColumnImpl.toGUIDString(UUID.randomUUID())`).
    fn get_guid(&self) -> String {
        self.random.borrow_mut().next_guid(self.clock())
    }

    fn lookup_identifier(&self, identifier: &Identifier) -> ExprResult<ExprValue> {
        if identifier.property.is_some() {
            return eval_err(format!("Cannot access field properties for {identifier}"));
        }
        if let Some(collection) = &identifier.collection {
            match &self.table_name {
                Some(table) if collection.eq_ignore_ascii_case(table) => {}
                Some(_) => {
                    return eval_err(format!(
                        "Cannot access fields outside this table for {identifier}"
                    ))
                }
                None => {
                    return eval_err(format!(
                        "Cannot resolve collection '{}' for {identifier}",
                        collection
                    ))
                }
            }
        }
        self.get_column(&identifier.object)
            .cloned()
            .ok_or_else(|| ExprError::Eval(format!("Unknown column '{identifier}'")))
    }
}

fn normalize_context_value(value: ExprValue) -> ExprValue {
    match value {
        // BaseEvalContext.toValue maps BOOLEAN/BYTE/INT columns to LONG ...
        ExprValue::Bool(b) => ExprValue::Long(if b { -1 } else { 0 }),
        ExprValue::Byte(v) => ExprValue::Long(v as i32),
        ExprValue::Int(v) => ExprValue::Long(v as i32),
        // ... and BIG_INT to BIG_DEC, through the double conversion
        // ColumnImpl.toBigDecimal(Number) uses (so Jackcess' precision loss
        // above 2^53 is reproduced)
        ExprValue::BigInt(v) => ExprValue::Decimal(
            Decimal::from_f64_exact_integer(v as f64).unwrap_or_else(|_| Decimal::from_i64(v)),
        ),
        other => other,
    }
}

// ---------------------------------------------------------------------------
// public expression API
// ---------------------------------------------------------------------------

/// Options controlling [`parse_with`].
#[derive(Clone, Copy, Debug)]
pub struct ParseOptions {
    /// The kind of expression (calculated column, default, validator).
    pub mode: ExprMode,
    /// The target column type, if known.
    pub result_type: Option<DataType>,
    /// Whether a text default value which is not an expression may be taken
    /// literally (`ColDefaultValueEvalContext.allowLiteralDefaultValue`).
    pub allow_literal_default: bool,
}

impl Default for ParseOptions {
    fn default() -> ParseOptions {
        ParseOptions {
            mode: ExprMode::Expression,
            result_type: None,
            allow_literal_default: false,
        }
    }
}

impl ParseOptions {
    /// A calculated column / ordinary expression.
    pub fn expression() -> ParseOptions {
        ParseOptions::default()
    }

    /// A `=...` default value for a column of `result_type`.
    pub fn default_value(result_type: DataType, allow_literal_default: bool) -> ParseOptions {
        ParseOptions {
            mode: ExprMode::DefaultValue,
            result_type: Some(result_type),
            allow_literal_default,
        }
    }

    /// A field validation rule.
    pub fn field_validator() -> ParseOptions {
        ParseOptions {
            mode: ExprMode::FieldValidator,
            ..ParseOptions::default()
        }
    }

    /// A record validation rule.
    pub fn row_validator() -> ParseOptions {
        ParseOptions {
            mode: ExprMode::RowValidator,
            ..ParseOptions::default()
        }
    }
}

/// Parse an ordinary (calculated column style) expression.
pub fn parse(expression: &str) -> ExprResult<Expression> {
    parse_with(expression, ParseOptions::default())
}

/// Parse an expression with the given mode/options.
pub fn parse_with(expression: &str, options: ParseOptions) -> ExprResult<Expression> {
    let mut tokens = tokenize(expression, options.mode)?;
    if options.mode == ExprMode::DefaultValue {
        let leading_equals = matches!(tokens.first(), Some(Token::Op(op)) if op == "=");
        if leading_equals {
            tokens.remove(0);
            if tokens.is_empty() {
                return parse_err("No expression found after '='");
            }
        } else if options.allow_literal_default
            && options
                .result_type
                .map(|target| value_kind_for(target) == ValueKind::Text)
                .unwrap_or(false)
            && !expression.starts_with('"')
        {
            // a default value which does not start with '=' (or a '"') is a
            // literal string for a text column
            return Ok(Expression {
                raw: expression.to_string(),
                expr: Expr::Literal(ExprValue::Text(expression.to_string())),
                result_type: options.result_type,
                mode: options.mode,
            });
        }
    }
    let mut parser = Parser::new(&tokens, options.mode);
    let expr = parser.parse_expression()?;
    Ok(Expression {
        raw: expression.to_string(),
        expr,
        result_type: options.result_type,
        mode: options.mode,
    })
}

/// A parsed expression.
#[derive(Clone, Debug)]
pub struct Expression {
    raw: String,
    expr: Expr,
    result_type: Option<DataType>,
    mode: ExprMode,
}

impl Expression {
    /// The raw expression string.
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// The parsing mode.
    pub fn mode(&self) -> ExprMode {
        self.mode
    }

    /// The configured result type, if any.
    pub fn result_type(&self) -> Option<DataType> {
        self.result_type
    }

    /// Whether the expression contains no column references and only pure
    /// functions (`Expressionator` memoizes such expressions).
    pub fn is_constant(&self) -> bool {
        self.expr.is_constant()
    }

    /// The column references used by the expression
    /// (`Expression.collectIdentifiers`).
    pub fn identifiers(&self) -> Vec<Identifier> {
        let mut out = Vec::new();
        self.expr.collect_identifiers(&mut out);
        out
    }

    /// Evaluate the expression.  If a result type was configured, the value is
    /// converted to it like `ExprWrapper.evalValue`.
    pub fn eval(&self, context: &EvalContext) -> ExprResult<ExprValue> {
        let result_kind = self
            .result_type
            .map(value_kind_for)
            .or(context.result_type.map(value_kind_for));
        let env = EvalEnv {
            ctx: context,
            result_kind,
        };
        let value = self.expr.eval(&env)?;
        match self.result_type {
            Some(target) => apply_result_type(value, value_kind_for(target)),
            None => Ok(value),
        }
    }

    /// Evaluate the expression and coerce the result to a column's internal
    /// representation, exactly like the calculated column path
    /// (`CalcColEvalContext` + `ColumnImpl.toInternalValue`).
    pub fn eval_column(&self, context: &EvalContext, target: DataType) -> ExprResult<ExprValue> {
        let result_kind = value_kind_for(target);
        let env = EvalEnv {
            ctx: context,
            result_kind: Some(result_kind),
        };
        let value = self.expr.eval(&env)?;
        let value = apply_result_type(value, result_kind)?;
        value.coerce_to(target)
    }

    /// Evaluate the expression as a validation condition
    /// (`BaseExprWrapper.evalCondition`): `Null` is an error, everything else
    /// uses `Value.getAsBoolean`.
    pub fn eval_condition(&self, context: &EvalContext) -> ExprResult<bool> {
        let value = self.eval(context)?;
        if value.is_null() {
            return eval_err("Condition evaluated to Null");
        }
        value.get_as_boolean()
    }
}

fn apply_result_type(value: ExprValue, kind: ValueKind) -> ExprResult<ExprValue> {
    if value.is_null() {
        return Ok(ExprValue::Null);
    }
    match kind {
        ValueKind::Null => Ok(ExprValue::Null),
        ValueKind::Text => Ok(ExprValue::Text(value.get_as_string()?)),
        ValueKind::Long => Ok(ExprValue::Long(value.get_as_long()?)),
        ValueKind::Double => Ok(ExprValue::Double(value.get_as_double()?)),
        ValueKind::Decimal => Ok(ExprValue::Decimal(value.get_as_decimal()?)),
        ValueKind::Date => Ok(ExprValue::date(value.get_as_local_date_time()?)),
        ValueKind::Time => Ok(ExprValue::time(value.get_as_local_date_time()?)),
        ValueKind::DateTime => Ok(ExprValue::date_time(value.get_as_local_date_time()?)),
    }
}

// ---------------------------------------------------------------------------
// evaluation configuration (`com.healthmarketscience.jackcess.expr`)
// ---------------------------------------------------------------------------

/// `TemporalConfig.Type`: the date/time format variants.
///
/// Java nests this enum in `TemporalConfig`; Rust has no nested types, so the
/// module level [`TemporalType`] is the equivalent (`TemporalConfig::Type` in
/// the Java documentation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TemporalType {
    Date,
    Time,
    DateTime,
    Time12,
    Time24,
    DateTime12,
    DateTime24,
    GeneralDate,
    LongDate,
    MediumDate,
    ShortDate,
    LongTime,
    MediumTime,
    ShortTime,
}

/// The Java enum constant name (`TemporalConfig.Type.<name>`).
pub type TemporalConfigType = TemporalType;

impl TemporalType {
    /// `TemporalConfig.Type.getDefaultType()`.
    pub fn default_type(self) -> TemporalType {
        match self {
            TemporalType::Date
            | TemporalType::LongDate
            | TemporalType::MediumDate
            | TemporalType::ShortDate => TemporalType::Date,
            TemporalType::Time
            | TemporalType::Time12
            | TemporalType::Time24
            | TemporalType::LongTime
            | TemporalType::MediumTime
            | TemporalType::ShortTime => TemporalType::Time,
            TemporalType::DateTime
            | TemporalType::DateTime12
            | TemporalType::DateTime24
            | TemporalType::GeneralDate => TemporalType::DateTime,
        }
    }

    /// `TemporalConfig.Type.getValueType()`.
    pub fn value_type(self) -> ValueKind {
        match self.default_type() {
            TemporalType::Date => ValueKind::Date,
            TemporalType::Time => ValueKind::Time,
            _ => ValueKind::DateTime,
        }
    }

    /// `TemporalConfig.Type.includesTime()`.
    pub fn includes_time(self) -> bool {
        !self.is_date_only()
    }

    /// `TemporalConfig.Type.includesDate()`.
    pub fn includes_date(self) -> bool {
        !self.is_time_only()
    }

    /// `TemporalConfig.Type.isDateOnly()`.
    pub fn is_date_only(self) -> bool {
        matches!(
            self,
            TemporalType::Date
                | TemporalType::LongDate
                | TemporalType::MediumDate
                | TemporalType::ShortDate
        )
    }

    /// `TemporalConfig.Type.isTimeOnly()`.
    pub fn is_time_only(self) -> bool {
        matches!(
            self,
            TemporalType::Time
                | TemporalType::Time12
                | TemporalType::Time24
                | TemporalType::LongTime
                | TemporalType::MediumTime
                | TemporalType::ShortTime
        )
    }

    /// The Java enum constant name (`DATE`, `DATE_TIME_12`, ...).
    pub fn name(self) -> &'static str {
        match self {
            TemporalType::Date => "DATE",
            TemporalType::Time => "TIME",
            TemporalType::DateTime => "DATE_TIME",
            TemporalType::Time12 => "TIME_12",
            TemporalType::Time24 => "TIME_24",
            TemporalType::DateTime12 => "DATE_TIME_12",
            TemporalType::DateTime24 => "DATE_TIME_24",
            TemporalType::GeneralDate => "GENERAL_DATE",
            TemporalType::LongDate => "LONG_DATE",
            TemporalType::MediumDate => "MEDIUM_DATE",
            TemporalType::ShortDate => "SHORT_DATE",
            TemporalType::LongTime => "LONG_TIME",
            TemporalType::MediumTime => "MEDIUM_TIME",
            TemporalType::ShortTime => "SHORT_TIME",
        }
    }
}

/// Port of `com.healthmarketscience.jackcess.expr.TemporalConfig`: the
/// date/time formats used by expression evaluation.
///
/// The default is [`TemporalConfig::us`] (`US_TEMPORAL_CONFIG`).  The Rust
/// port stores the locale as its `Locale.toString()` text (`"en_US"`) and keeps
/// the `AM`/`PM` strings; other `DateFormatSymbols` data of the JDK is not
/// reproduced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TemporalConfig {
    locale: String,
    date_format: String,
    long_date_format: String,
    time_format_12: String,
    time_format_24: String,
    date_separator: char,
    time_separator: char,
    date_time_format_12: String,
    date_time_format_24: String,
    am_pm_strings: [String; 2],
}

impl TemporalConfig {
    /// `TemporalConfig.US_DATE_FORMAT`.
    pub const US_DATE_FORMAT: &'static str = "M/d[/uuuu]";
    /// `TemporalConfig.US_TIME_FORMAT_12_FORMAT`.
    pub const US_TIME_FORMAT_12_FORMAT: &'static str = "h:mm:ss a";
    /// `TemporalConfig.US_TIME_FORMAT_24_FORMAT`.
    pub const US_TIME_FORMAT_24_FORMAT: &'static str = "H:mm:ss";
    /// `TemporalConfig.US_LONG_DATE_FORMAT`.
    pub const US_LONG_DATE_FORMAT: &'static str = "EEEE, MMMM dd, uuuu";
    /// `TemporalConfig.MEDIUM_DATE_FORMAT`.
    pub const MEDIUM_DATE_FORMAT: &'static str = "dd-MMM-uu";
    /// `TemporalConfig.MEDIUM_TIME_FORMAT`.
    pub const MEDIUM_TIME_FORMAT: &'static str = "hh:mm a";
    /// `TemporalConfig.SHORT_TIME_FORMAT`.
    pub const SHORT_TIME_FORMAT: &'static str = "HH:mm";

    /// `TemporalConfig.US_TEMPORAL_CONFIG` (the Java constant is a singleton;
    /// the Rust equivalent is this constructor).
    pub fn us() -> TemporalConfig {
        TemporalConfig::new(
            Self::US_DATE_FORMAT,
            Self::US_LONG_DATE_FORMAT,
            Self::US_TIME_FORMAT_12_FORMAT,
            Self::US_TIME_FORMAT_24_FORMAT,
            '/',
            ':',
            US_LOCALE,
        )
    }

    /// `TemporalConfig(String dateFormat, String longDateFormat, String
    /// timeFormat12, String timeFormat24, char dateSeparator, char
    /// timeSeparator, Locale locale)`.
    pub fn new(
        date_format: impl Into<String>,
        long_date_format: impl Into<String>,
        time_format_12: impl Into<String>,
        time_format_24: impl Into<String>,
        date_separator: char,
        time_separator: char,
        locale: impl Into<String>,
    ) -> TemporalConfig {
        let date_format = date_format.into();
        let time_format_12 = time_format_12.into();
        let time_format_24 = time_format_24.into();
        TemporalConfig {
            locale: locale.into(),
            date_time_format_12: to_date_time_format(&date_format, &time_format_12),
            date_time_format_24: to_date_time_format(&date_format, &time_format_24),
            date_format,
            long_date_format: long_date_format.into(),
            time_format_12,
            time_format_24,
            date_separator,
            time_separator,
            am_pm_strings: ["AM".to_string(), "PM".to_string()],
        }
    }

    /// Overrides the `DateFormatSymbols.getAmPmStrings()` values (Java derives
    /// them from the locale; the port cannot).
    pub fn with_am_pm_strings(
        mut self,
        am: impl Into<String>,
        pm: impl Into<String>,
    ) -> TemporalConfig {
        self.am_pm_strings = [am.into(), pm.into()];
        self
    }

    pub fn locale(&self) -> &str {
        &self.locale
    }

    pub fn date_format(&self) -> &str {
        &self.date_format
    }

    pub fn long_date_format(&self) -> &str {
        &self.long_date_format
    }

    pub fn time_format_12(&self) -> &str {
        &self.time_format_12
    }

    pub fn time_format_24(&self) -> &str {
        &self.time_format_24
    }

    pub fn date_time_format_12(&self) -> &str {
        &self.date_time_format_12
    }

    pub fn date_time_format_24(&self) -> &str {
        &self.date_time_format_24
    }

    /// `TemporalConfig.getDefaultDateFormat()`.
    pub fn default_date_format(&self) -> &str {
        self.date_format()
    }

    /// `TemporalConfig.getDefaultTimeFormat()`.
    pub fn default_time_format(&self) -> &str {
        self.time_format_12()
    }

    /// `TemporalConfig.getDefaultDateTimeFormat()`.
    pub fn default_date_time_format(&self) -> &str {
        self.date_time_format_12()
    }

    pub fn date_separator(&self) -> char {
        self.date_separator
    }

    pub fn time_separator(&self) -> char {
        self.time_separator
    }

    /// `TemporalConfig.getAmPmStrings()`.
    pub fn am_pm_strings(&self) -> [&str; 2] {
        [&self.am_pm_strings[0], &self.am_pm_strings[1]]
    }

    /// `TemporalConfig.getDateTimeFormat(Type)`.
    pub fn get_date_time_format(&self, type_: TemporalType) -> &str {
        match type_ {
            TemporalType::Date | TemporalType::ShortDate => self.default_date_format(),
            TemporalType::Time => self.default_time_format(),
            TemporalType::DateTime | TemporalType::GeneralDate => self.default_date_time_format(),
            TemporalType::Time12 | TemporalType::LongTime => self.time_format_12(),
            TemporalType::Time24 => self.time_format_24(),
            TemporalType::DateTime12 => self.date_time_format_12(),
            TemporalType::DateTime24 => self.date_time_format_24(),
            TemporalType::LongDate => self.long_date_format(),
            TemporalType::MediumDate => Self::MEDIUM_DATE_FORMAT,
            TemporalType::MediumTime => Self::MEDIUM_TIME_FORMAT,
            TemporalType::ShortTime => Self::SHORT_TIME_FORMAT,
        }
    }
}

impl Default for TemporalConfig {
    /// `TemporalConfig.US_TEMPORAL_CONFIG`.
    fn default() -> TemporalConfig {
        TemporalConfig::us()
    }
}

fn to_date_time_format(date_format: &str, time_format: &str) -> String {
    format!("{date_format} {time_format}")
}

/// `FormatUtil.NumPatternType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumericType {
    Currency,
    Fixed,
    Standard,
    Percent,
    Scientific,
    Euro,
}

impl NumericType {
    /// The Java enum constant name.
    pub fn name(self) -> &'static str {
        match self {
            NumericType::Currency => "CURRENCY",
            NumericType::Fixed => "FIXED",
            NumericType::Standard => "STANDARD",
            NumericType::Percent => "PERCENT",
            NumericType::Scientific => "SCIENTIFIC",
            NumericType::Euro => "EURO",
        }
    }

    fn append_prefix(self) -> &'static str {
        match self {
            NumericType::Currency => "\u{00A4}",
            NumericType::Euro => "\u{20AC}",
            _ => "",
        }
    }

    fn append_suffix(self) -> &'static str {
        match self {
            NumericType::Percent => "%",
            NumericType::Scientific => "E0",
            _ => "",
        }
    }
}

/// Port of `com.healthmarketscience.jackcess.expr.NumericConfig`: the number
/// formatting options used by expression evaluation.
///
/// The default is [`NumericConfig::us`] (`US_NUMERIC_CONFIG`).  The locale is
/// kept as its `Locale.toString()` text; the `DecimalFormatSymbols` of the JDK
/// are not reproduced, so [`NumericConfig::get_number_format`] returns the
/// `FormatUtil.createNumberFormatPattern` pattern with US symbols (`$`, `%`)
/// and the formatting helpers use the US symbol set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NumericConfig {
    num_dec_digits: i32,
    inc_leading_digit: bool,
    use_neg_parens: bool,
    use_neg_currency_parens: bool,
    num_group_digits: i32,
    locale: String,
}

impl NumericConfig {
    /// `NumericConfig.US_NUMERIC_CONFIG`.
    pub fn us() -> NumericConfig {
        NumericConfig::new(2, true, false, true, 3, US_LOCALE)
    }

    /// `NumericConfig(int numDecDigits, boolean incLeadingDigit, boolean
    /// useNegParens, boolean useNegCurrencyParens, int numGroupDigits, Locale
    /// locale)`.
    pub fn new(
        num_dec_digits: i32,
        inc_leading_digit: bool,
        use_neg_parens: bool,
        use_neg_currency_parens: bool,
        num_group_digits: i32,
        locale: impl Into<String>,
    ) -> NumericConfig {
        NumericConfig {
            num_dec_digits,
            inc_leading_digit,
            use_neg_parens,
            use_neg_currency_parens,
            num_group_digits,
            locale: locale.into(),
        }
    }

    /// `NumericConfig.getNumDecimalDigits()`.
    pub fn num_decimal_digits(&self) -> i32 {
        self.num_dec_digits
    }

    /// `NumericConfig.includeLeadingDigit()`.
    pub fn include_leading_digit(&self) -> bool {
        self.inc_leading_digit
    }

    /// `NumericConfig.useParensForNegatives()`.
    pub fn use_parens_for_negatives(&self) -> bool {
        self.use_neg_parens
    }

    /// `NumericConfig.useParensForCurrencyNegatives()`.
    pub fn use_parens_for_currency_negatives(&self) -> bool {
        self.use_neg_currency_parens
    }

    /// `NumericConfig.getNumGroupingDigits()`.
    pub fn num_grouping_digits(&self) -> i32 {
        self.num_group_digits
    }

    pub fn locale(&self) -> &str {
        &self.locale
    }

    /// `NumericConfig.getNumberFormat(Type)`:
    /// `FormatUtil.createNumberFormatPattern` for this configuration.
    pub fn get_number_format(&self, type_: NumericType) -> String {
        // `NumericConfig`'s constructor passes different options per format
        let (inc_lead_digit, neg_parens, group_digits) = match type_ {
            NumericType::Currency | NumericType::Euro => (
                self.inc_leading_digit,
                self.use_neg_currency_parens,
                self.num_group_digits,
            ),
            NumericType::Fixed => (true, self.use_neg_parens, 0),
            NumericType::Standard => (
                self.inc_leading_digit,
                self.use_neg_parens,
                self.num_group_digits,
            ),
            NumericType::Percent => (self.inc_leading_digit, self.use_neg_parens, 0),
            NumericType::Scientific => (true, false, 0),
        };
        number_format_pattern(
            type_,
            self.num_dec_digits,
            inc_lead_digit,
            neg_parens,
            group_digits,
        )
    }
}

impl Default for NumericConfig {
    /// `NumericConfig.US_NUMERIC_CONFIG`.
    fn default() -> NumericConfig {
        NumericConfig::us()
    }
}

/// `FormatUtil.createNumberFormatPattern`.
fn number_format_pattern(
    num_pat_type: NumericType,
    num_dec_digits: i32,
    inc_lead_digit: bool,
    neg_parens: bool,
    num_group_digits: i32,
) -> String {
    let mut fmt = String::new();
    fmt.push_str(num_pat_type.append_prefix());
    if num_group_digits > 0 {
        fmt.push_str("#,");
        for _ in 0..(num_group_digits - 1) {
            fmt.push('#');
        }
    }
    fmt.push(if inc_lead_digit { '0' } else { '#' });
    if num_dec_digits > 0 {
        fmt.push('.');
        for _ in 0..num_dec_digits {
            fmt.push('0');
        }
    }
    fmt.push_str(num_pat_type.append_suffix());
    if neg_parens {
        let main = fmt.clone();
        fmt.push_str(";(");
        fmt.push_str(&main);
        fmt.push(')');
    }
    fmt
}

/// Java's `Locale.toString()` of `Locale.US`.
pub const US_LOCALE: &str = "en_US";

/// The `Function` description a [`FunctionLookup`] resolves a name to.
///
/// Java returns a `com.healthmarketscience.jackcess.expr.Function` instance.
/// The port evaluates its built-in functions directly from the expression
/// engine, so the lookup exposes the resolved name (and whether it is the
/// default implementation) instead of a callable object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionInfo {
    name: String,
    is_default: bool,
}

impl FunctionInfo {
    pub fn new(name: impl Into<String>) -> FunctionInfo {
        FunctionInfo {
            name: name.into(),
            is_default: true,
        }
    }

    /// `Function.getName()`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// `true` for the built-in functions of the default lookup.
    pub fn is_default(&self) -> bool {
        self.is_default
    }
}

/// `com.healthmarketscience.jackcess.expr.FunctionLookup`.
pub trait FunctionLookup: std::fmt::Debug {
    /// `FunctionLookup.getFunction(String name)`: the function with the given
    /// name (case insensitive, like Access), or `None`.
    fn get_function(&self, name: &str) -> Option<FunctionInfo>;
}

/// `DefaultFunctions.LOOKUP`: the built-in functions of the Java library.
///
/// The names are the ones `DefaultTextFunctions`, `DefaultNumberFunctions`,
/// `DefaultDateFunctions` and `DefaultFinancialFunctions` register (including
/// the `$` variants of the string conversion functions, which
/// `StringFuncWrapper` adds).
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultFunctionLookup;

impl DefaultFunctionLookup {
    /// The shared instance (`DefaultFunctions.LOOKUP`).
    pub fn instance() -> DefaultFunctionLookup {
        DefaultFunctionLookup
    }

    /// Every function name of the default lookup, in alphabetical order.
    pub fn function_names() -> &'static [&'static str] {
        DEFAULT_FUNCTION_NAMES
    }

    /// `true` when `name` is one of the default functions.
    pub fn is_known(name: &str) -> bool {
        DEFAULT_FUNCTION_NAMES
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(name))
    }
}

impl FunctionLookup for DefaultFunctionLookup {
    fn get_function(&self, name: &str) -> Option<FunctionInfo> {
        // `DatabaseImpl.toLookupName` upper cases the requested name; the
        // resolved function keeps the registered spelling
        let registered = DEFAULT_FUNCTION_NAMES
            .iter()
            .find(|candidate| candidate.eq_ignore_ascii_case(name))?;
        Some(FunctionInfo::new(*registered))
    }
}

/// The function names of `DefaultFunctions.LOOKUP`, generated from
/// `DefaultFunctions` / `DefaultTextFunctions` / `DefaultNumberFunctions` /
/// `DefaultDateFunctions` / `DefaultFinancialFunctions` (every `registerFunc`
/// name plus the `$` variants `StringFuncWrapper` adds for the 19
/// `registerStringFunc` names).
pub static DEFAULT_FUNCTION_NAMES: &[&str] = &[
    "Abs",
    "Asc",
    "AscW",
    "Atn",
    "CBool",
    "CByte",
    "CCur",
    "CDate",
    "CDbl",
    "CDec",
    "Choose",
    "Chr",
    "Chr$",
    "ChrW",
    "ChrW$",
    "CInt",
    "CLng",
    "Cos",
    "CSng",
    "CStr",
    "CVar",
    "CVDate",
    "Date",
    "DateAdd",
    "DateDiff",
    "DatePart",
    "DateSerial",
    "DateValue",
    "Day",
    "DDB",
    "Exp",
    "Fix",
    "Format",
    "Format$",
    "FormatCurrency",
    "FormatDateTime",
    "FormatNumber",
    "FormatPercent",
    "FV",
    "GenGUID",
    "GenUniqueID",
    "Hex",
    "Hex$",
    "Hour",
    "IIf",
    "InStr",
    "InStrRev",
    "Int",
    "IPmt",
    "IsDate",
    "IsNull",
    "IsNumeric",
    "LCase",
    "LCase$",
    "Left",
    "Left$",
    "Len",
    "Log",
    "LTrim",
    "LTrim$",
    "Mid",
    "Mid$",
    "Minute",
    "Month",
    "MonthName",
    "Now",
    "NPer",
    "Nz",
    "Oct",
    "Oct$",
    "Pmt",
    "PPmt",
    "PV",
    "Rate",
    "Replace",
    "Replace$",
    "Right",
    "Right$",
    "Rnd",
    "Round",
    "RTrim",
    "RTrim$",
    "Second",
    "Sgn",
    "Sin",
    "SLN",
    "Space",
    "Space$",
    "Sqr",
    "Str",
    "Str$",
    "StrComp",
    "StrConv",
    "StrConv$",
    "String",
    "String$",
    "StrReverse",
    "Switch",
    "SYD",
    "Tan",
    "Time",
    "Timer",
    "TimeSerial",
    "TimeValue",
    "Trim",
    "Trim$",
    "TypeName",
    "UCase",
    "UCase$",
    "Val",
    "Val$",
    "VarType",
    "Weekday",
    "WeekdayName",
    "Year",
];

/// `javax.script.Bindings` of an [`EvalConfig`]: custom information passed into
/// expression evaluation.  The Java interface is a `Map<String,Object>`; the
/// Rust port stores [`ExprValue`]s.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Bindings {
    values: HashMap<String, ExprValue>,
}

impl Bindings {
    pub fn new() -> Bindings {
        Bindings::default()
    }

    /// `Bindings.get(Object key)`.
    pub fn get(&self, key: &str) -> Option<&ExprValue> {
        self.values.get(key)
    }

    /// `Bindings.put(String name, Object value)`.
    pub fn put(&mut self, key: impl Into<String>, value: ExprValue) -> Option<ExprValue> {
        self.values.insert(key.into(), value)
    }

    /// `Bindings.remove(Object key)`.
    pub fn remove(&mut self, key: &str) -> Option<ExprValue> {
        self.values.remove(key)
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// The bindings in iteration order (the port uses a `HashMap`, so the
    /// order is unspecified, like Java's `SimpleBindings`; sort the keys when
    /// an order is needed).
    pub fn iter(&self) -> impl Iterator<Item = (&String, &ExprValue)> {
        self.values.iter()
    }
}

/// Port of `com.healthmarketscience.jackcess.expr.EvalConfig` (implemented by
/// `impl.DBEvalContext` in the Java library): the expression evaluation
/// configuration of a database.
///
/// # Java name mapping
///
/// | Java `EvalConfig` | Rust |
/// |---|---|
/// | `getLocale()` / `setLocale(Locale)` | [`EvalConfig::locale`] / [`EvalConfig::set_locale`] |
/// | `getTemporalConfig()` / `setTemporalConfig(...)` | [`EvalConfig::temporal_config`] / [`EvalConfig::set_temporal_config`] |
/// | `getNumericConfig()` / `setNumericConfig(...)` | [`EvalConfig::numeric_config`] / [`EvalConfig::set_numeric_config`] |
/// | `getFunctionLookup()` / `setFunctionLookup(...)` | [`EvalConfig::function_lookup`] / [`EvalConfig::set_function_lookup`] |
/// | `getBindings()` / `setBindings(Bindings)` | [`EvalConfig::bindings`] / [`EvalConfig::bindings_mut`] / [`EvalConfig::set_bindings`] |
/// | (no equivalent) | [`EvalConfig::zone`] / [`EvalConfig::set_zone`] |
///
/// The zone is a port extension: Java's `EvalConfig` has no time zone; its
/// implementation `DBEvalContext` answers `getZoneId()` from the owning
/// `DatabaseImpl`.  The Rust [`EvalConfig`] carries the database's
/// [`crate::datenums::TimeZone`] so a database can return a fully populated
/// config from `Database::get_eval_config`.
///
/// As in the Java library the default configuration is US based:
/// `Locale.US`, [`TemporalConfig::us`], [`NumericConfig::us`], the default
/// function lookup and empty bindings.
#[derive(Clone, Debug)]
pub struct EvalConfig {
    locale: String,
    temporal_config: TemporalConfig,
    numeric_config: NumericConfig,
    function_lookup: std::sync::Arc<dyn FunctionLookup>,
    bindings: Bindings,
    zone: Option<crate::datenums::TimeZone>,
}

impl Default for EvalConfig {
    /// `new DBEvalContext(db)`'s initial state (`DBEvalContext`'s field
    /// initializers): US locale, US temporal/numeric config, the default
    /// function lookup, empty bindings and no explicit zone.
    fn default() -> EvalConfig {
        EvalConfig {
            locale: US_LOCALE.to_string(),
            temporal_config: TemporalConfig::us(),
            numeric_config: NumericConfig::us(),
            function_lookup: std::sync::Arc::new(DefaultFunctionLookup::instance()),
            bindings: Bindings::new(),
            zone: None,
        }
    }
}

impl EvalConfig {
    pub fn new() -> EvalConfig {
        EvalConfig::default()
    }

    /// `EvalConfig.getLocale()` (as `Locale.toString()`).
    pub fn locale(&self) -> &str {
        &self.locale
    }

    /// `EvalConfig.setLocale(Locale)`.  The port stores the locale text
    /// (`"en_US"`); a change does not rebuild the temporal/numeric config,
    /// matching `DBEvalContext.setLocale` (which only stores the locale).
    pub fn set_locale(&mut self, locale: impl Into<String>) {
        self.locale = locale.into();
    }

    /// Builder form of [`EvalConfig::set_locale`].
    pub fn with_locale(mut self, locale: impl Into<String>) -> EvalConfig {
        self.set_locale(locale);
        self
    }

    /// `EvalConfig.getTemporalConfig()`.
    pub fn temporal_config(&self) -> &TemporalConfig {
        &self.temporal_config
    }

    /// `EvalConfig.setTemporalConfig(TemporalConfig)`.
    pub fn set_temporal_config(&mut self, temporal_config: TemporalConfig) {
        self.temporal_config = temporal_config;
    }

    /// Builder form of [`EvalConfig::set_temporal_config`].
    pub fn with_temporal_config(mut self, temporal_config: TemporalConfig) -> EvalConfig {
        self.set_temporal_config(temporal_config);
        self
    }

    /// `EvalConfig.getNumericConfig()`.
    pub fn numeric_config(&self) -> &NumericConfig {
        &self.numeric_config
    }

    /// `EvalConfig.setNumericConfig(NumericConfig)`.
    pub fn set_numeric_config(&mut self, numeric_config: NumericConfig) {
        self.numeric_config = numeric_config;
    }

    /// Builder form of [`EvalConfig::set_numeric_config`].
    pub fn with_numeric_config(mut self, numeric_config: NumericConfig) -> EvalConfig {
        self.set_numeric_config(numeric_config);
        self
    }

    /// `EvalConfig.getFunctionLookup()`.
    pub fn function_lookup(&self) -> &dyn FunctionLookup {
        self.function_lookup.as_ref()
    }

    /// `EvalConfig.setFunctionLookup(FunctionLookup)`.
    pub fn set_function_lookup(&mut self, lookup: std::sync::Arc<dyn FunctionLookup>) {
        self.function_lookup = lookup;
    }

    /// Builder form of [`EvalConfig::set_function_lookup`].
    pub fn with_function_lookup(
        mut self,
        lookup: std::sync::Arc<dyn FunctionLookup>,
    ) -> EvalConfig {
        self.set_function_lookup(lookup);
        self
    }

    /// `EvalConfig.getBindings()`.
    pub fn bindings(&self) -> &Bindings {
        &self.bindings
    }

    /// Mutable access to the current bindings (`EvalConfig.getBindings()`
    /// returns the live `SimpleBindings` instance in Java).
    pub fn bindings_mut(&mut self) -> &mut Bindings {
        &mut self.bindings
    }

    /// `EvalConfig.setBindings(Bindings)`.
    pub fn set_bindings(&mut self, bindings: Bindings) {
        self.bindings = bindings;
    }

    /// Builder form of [`EvalConfig::set_bindings`].
    pub fn with_bindings(mut self, bindings: Bindings) -> EvalConfig {
        self.set_bindings(bindings);
        self
    }

    /// The database time zone the config belongs to (port extension; see the
    /// type documentation).  `None` means "not resolved yet".
    pub fn zone(&self) -> Option<&crate::datenums::TimeZone> {
        self.zone.as_ref()
    }

    /// Sets the database time zone (port extension; Java's `DBEvalContext`
    /// reads it from the owning database instead).
    pub fn set_zone(&mut self, zone: Option<crate::datenums::TimeZone>) {
        self.zone = zone;
    }

    /// Builder form of [`EvalConfig::set_zone`].
    pub fn with_zone(mut self, zone: Option<crate::datenums::TimeZone>) -> EvalConfig {
        self.set_zone(zone);
        self
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(all(test, feature = "fixtures"))]
mod tests {
    use super::*;

    fn ldt(year: i32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> LocalDateTime {
        LocalDateTime::new(year, month, day, hour, minute, second, 0)
    }

    fn fixtures() -> Vec<(&'static str, EvalContext)> {
        let mut r1 = EvalContext::new();
        r1.set_column("A", ExprValue::Long(5))
            .set_column("B", ExprValue::Double(2.5))
            .set_column("S", ExprValue::Text("Hello".into()))
            .set_column("D", ExprValue::date_time(ldt(2020, 1, 2, 3, 4, 5)))
            .set_column("BOOL", ExprValue::Bool(true));
        let mut r2 = EvalContext::new();
        r2.set_column("A", ExprValue::Long(-3))
            .set_column("B", ExprValue::Double(-1.5))
            .set_column("S", ExprValue::Text("  Mixed Case  ".into()))
            .set_column("D", ExprValue::date_time(ldt(1899, 12, 30, 12, 34, 56)))
            .set_column("BOOL", ExprValue::Bool(false));
        let mut r3 = EvalContext::new();
        r3.set_column("A", ExprValue::Null)
            .set_column("B", ExprValue::Null)
            .set_column("S", ExprValue::Null)
            .set_column("D", ExprValue::Null)
            .set_column("BOOL", ExprValue::Null);
        let mut r4 = EvalContext::new();
        r4.set_column("A", ExprValue::Long(0))
            .set_column("B", ExprValue::Double(0.0))
            .set_column("S", ExprValue::Text(String::new()))
            .set_column("D", ExprValue::date_time(ldt(2021, 12, 31, 23, 59, 59)))
            .set_column("BOOL", ExprValue::Bool(true));
        let mut r5 = EvalContext::new();
        r5.set_column("A", ExprValue::Long(i32::MAX))
            .set_column("B", ExprValue::Double(-0.5))
            .set_column("S", ExprValue::Text("abc".into()))
            .set_column("D", ExprValue::date_time(ldt(2000, 2, 29, 0, 0, 0)))
            .set_column("BOOL", ExprValue::Bool(false));
        let mut r6 = EvalContext::new();
        r6.set_column("A", ExprValue::Long(i32::MIN))
            .set_column("B", ExprValue::Double(12345.6789))
            .set_column("S", ExprValue::Text("MiXeD".into()))
            .set_column("D", ExprValue::date_time(ldt(1999, 12, 31, 23, 59, 59)))
            .set_column("BOOL", ExprValue::Bool(true));
        let mut r7 = EvalContext::new();
        r7.set_column("A", ExprValue::Long(7))
            .set_column("B", ExprValue::Double(0.1))
            .set_column("S", ExprValue::Text("a,b".into()))
            .set_column("D", ExprValue::date_time(ldt(2038, 1, 19, 3, 14, 7)))
            .set_column("BOOL", ExprValue::Bool(false));
        let mut r8 = EvalContext::new();
        r8.set_column("A", ExprValue::Long(1))
            .set_column("B", ExprValue::Double(1.2345678901234567))
            .set_column("S", ExprValue::Text("O'Brien".into()))
            .set_column("D", ExprValue::date_time(ldt(1900, 2, 28, 12, 0, 0)))
            .set_column("BOOL", ExprValue::Bool(true));
        let mut r9 = EvalContext::new();
        r9.set_column("A", ExprValue::Long(-1))
            .set_column("B", ExprValue::Double(-1.0E-10))
            .set_column("S", ExprValue::Text("  spaced  out  ".into()))
            .set_column("D", ExprValue::date_time(ldt(2000, 12, 31, 23, 59, 59)))
            .set_column("BOOL", ExprValue::Bool(false));
        let mut r10 = EvalContext::new();
        r10.set_column("A", ExprValue::Long(123456789))
            .set_column("B", ExprValue::Double(1.0E15))
            .set_column("S", ExprValue::Text("ABC".into()))
            .set_column("D", ExprValue::date_time(ldt(2024, 2, 29, 6, 30, 0)))
            .set_column("BOOL", ExprValue::Bool(true));
        // fixtures for the custom Format pattern corpus (R11/R12 mirror
        // java-src/ExprOracle.java)
        let mut r11 = EvalContext::new();
        r11.set_column("A", ExprValue::Long(100000000))
            .set_column("B", ExprValue::Double(0.0000001))
            .set_column("S", ExprValue::Text("0.00".into()))
            .set_column("D", ExprValue::date_time(ldt(1900, 3, 1, 0, 0, 0)))
            .set_column("BOOL", ExprValue::Bool(true));
        let mut r12 = EvalContext::new();
        r12.set_column("A", ExprValue::Long(-7))
            .set_column("B", ExprValue::Double(1234567.891234))
            .set_column("S", ExprValue::Text("--".into()))
            .set_column("D", ExprValue::date_time(ldt(2019, 6, 30, 12, 0, 0)))
            .set_column("BOOL", ExprValue::Bool(false));
        // fixtures for the extreme / high precision Format corpus (R13..R20
        // mirror java-src/ExprOracle.java: subnormals, MIN/MAX, extreme
        // exponents and number strings needing more than 38 coefficient
        // digits)
        let mut r13 = EvalContext::new();
        r13.set_column("A", ExprValue::Long(1))
            .set_column("B", ExprValue::Double(f64::from_bits(1)))
            .set_column(
                "S",
                ExprValue::Text("1234567890123456789012345678901234567890".into()),
            )
            .set_column("D", ExprValue::date_time(ldt(2001, 1, 1, 0, 0, 0)))
            .set_column("BOOL", ExprValue::Bool(true));
        let mut r14 = EvalContext::new();
        r14.set_column("A", ExprValue::Long(-1))
            .set_column("B", ExprValue::Double(f64::MAX))
            .set_column(
                "S",
                ExprValue::Text("0.123456789012345678901234567890123456789012345".into()),
            )
            .set_column("D", ExprValue::date_time(ldt(2001, 1, 2, 0, 0, 0)))
            .set_column("BOOL", ExprValue::Bool(false));
        let mut r15 = EvalContext::new();
        r15.set_column("A", ExprValue::Long(0))
            .set_column("B", ExprValue::Double(1.0E300))
            .set_column(
                "S",
                ExprValue::Text(
                    "&hFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF".into(),
                ),
            )
            .set_column("D", ExprValue::date_time(ldt(2001, 1, 3, 0, 0, 0)))
            .set_column("BOOL", ExprValue::Bool(true));
        let mut r16 = EvalContext::new();
        r16.set_column("A", ExprValue::Long(0))
            .set_column("B", ExprValue::Double(1.0E-300))
            .set_column("S", ExprValue::Text("1E+400".into()))
            .set_column("D", ExprValue::date_time(ldt(2001, 1, 4, 0, 0, 0)))
            .set_column("BOOL", ExprValue::Bool(false));
        let mut r17 = EvalContext::new();
        r17.set_column("A", ExprValue::Long(0))
            .set_column("B", ExprValue::Double(f64::MIN_POSITIVE))
            .set_column(
                "S",
                ExprValue::Text("&o777777777777777777777777777777777777777777777777777".into()),
            )
            .set_column("D", ExprValue::date_time(ldt(2001, 1, 5, 0, 0, 0)))
            .set_column("BOOL", ExprValue::Bool(true));
        let mut r18 = EvalContext::new();
        r18.set_column("A", ExprValue::Long(0))
            .set_column("B", ExprValue::Double(f64::from_bits(2)))
            .set_column("S", ExprValue::Text("-1,234.5678".into()))
            .set_column("D", ExprValue::date_time(ldt(2001, 1, 6, 0, 0, 0)))
            .set_column("BOOL", ExprValue::Bool(false));
        let mut r19 = EvalContext::new();
        r19.set_column("A", ExprValue::Long(0))
            .set_column("B", ExprValue::Double(1.0E-320))
            .set_column(
                "S",
                ExprValue::Text(
                    "0.1234567890123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890".into(),
                ),
            )
            .set_column("D", ExprValue::date_time(ldt(2001, 1, 7, 0, 0, 0)))
            .set_column("BOOL", ExprValue::Bool(true));
        let mut r20 = EvalContext::new();
        r20.set_column("A", ExprValue::Long(0))
            .set_column("B", ExprValue::Double(-f64::MAX))
            .set_column("S", ExprValue::Text("1e-400".into()))
            .set_column("D", ExprValue::date_time(ldt(2001, 1, 8, 0, 0, 0)))
            .set_column("BOOL", ExprValue::Bool(false));
        let mut r21 = EvalContext::new();
        r21.set_column("A", ExprValue::Long(0))
            .set_column("B", ExprValue::Double(12345.6789))
            .set_column(
                "S",
                ExprValue::Text("-1234567890123456789012345678901234567890".into()),
            )
            .set_column("D", ExprValue::date_time(ldt(2001, 1, 9, 0, 0, 0)))
            .set_column("BOOL", ExprValue::Bool(true));
        vec![
            ("R1", r1),
            ("R2", r2),
            ("R3", r3),
            ("R4", r4),
            ("R5", r5),
            ("R6", r6),
            ("R7", r7),
            ("R8", r8),
            ("R9", r9),
            ("R10", r10),
            ("R11", r11),
            ("R12", r12),
            ("R13", r13),
            ("R14", r14),
            ("R15", r15),
            ("R16", r16),
            ("R17", r17),
            ("R18", r18),
            ("R19", r19),
            ("R20", r20),
            ("R21", r21),
        ]
    }

    fn escape(text: &str) -> String {
        let mut out = String::new();
        for c in text.chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '\t' => out.push_str("\\t"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                other => out.push(other),
            }
        }
        out
    }

    fn canonical(value: &ExprValue) -> String {
        match value {
            ExprValue::Null => "Null".to_string(),
            ExprValue::Bool(value) => format!("Bool:{value}"),
            ExprValue::Byte(value) => format!("Byte:{value}"),
            ExprValue::Int(value) => format!("Int:{value}"),
            ExprValue::Long(value) => format!("Long:{value}"),
            ExprValue::BigInt(value) => format!("BigInt:{value}"),
            ExprValue::Double(value) => format!("Double:0x{:016x}", value.to_bits()),
            ExprValue::Decimal(value) => format!("Decimal:{}", value.to_plain_string()),
            ExprValue::Text(text) => format!("Text:{}", escape(text)),
            ExprValue::DateTime { value, .. } => format!(
                "DateTime:{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09}",
                value.year,
                value.month,
                value.day,
                value.hour,
                value.minute,
                value.second,
                value.nano
            ),
        }
    }

    fn target_type(name: &str) -> DataType {
        match name {
            "LONG" => DataType::Long,
            "TEXT" => DataType::Text,
            "DOUBLE" => DataType::Double,
            "BOOLEAN" => DataType::Boolean,
            "DATE" => DataType::ShortDateTime,
            "BYTE" => DataType::Byte,
            "INT" => DataType::Int,
            "BIGINT" => DataType::BigInt,
            other => panic!("unknown corpus target {other}"),
        }
    }

    /// The full Java oracle corpus: every case is a real calculated column
    /// evaluated by Jackcess 5.0.4 (see `java-src/ExprOracle.java`).
    ///
    /// The Java oracle evaluates each target column type in its own database,
    /// so stateful functions (`Rnd`) restart per target; the fixture contexts
    /// are rebuilt per target for the same reason.
    #[test]
    fn java_oracle_corpus() {
        let corpus = include_str!("../resources/expr_ref/corpus.tsv");
        let mut cases: Vec<(String, DataType, String, String)> = Vec::new();
        for line in corpus.lines() {
            let line = line.trim_end_matches('\r');
            if line.is_empty() {
                continue;
            }
            if line.starts_with("# ") || line.starts_with("##") {
                continue;
            }
            // comments start with '#' and have no tab; a date literal
            // expression such as #2020-01-02# does have one
            if line.starts_with('#') && !line.contains('\t') {
                continue;
            }
            let parts: Vec<&str> = line.split('\t').collect();
            assert_eq!(parts.len(), 4, "malformed corpus line: {line}");
            cases.push((
                parts[0].to_string(),
                target_type(parts[1]),
                parts[2].to_string(),
                parts[3].to_string(),
            ));
        }

        let mut target_order: Vec<DataType> = Vec::new();
        for (_, target, _, _) in &cases {
            if !target_order.contains(target) {
                target_order.push(*target);
            }
        }

        let mut total = 0usize;
        let mut failures = Vec::new();
        for target in target_order {
            let fixtures = fixtures();
            for (expression, case_target, row, expected) in &cases {
                if *case_target != target {
                    continue;
                }
                let context = fixtures
                    .iter()
                    .find(|(id, _)| id == row)
                    .map(|(_, context)| context)
                    .unwrap_or_else(|| panic!("unknown fixture row {row}"));
                let actual = match parse(expression)
                    .and_then(|parsed| parsed.eval_column(context, target))
                {
                    Ok(value) => canonical(&value),
                    Err(error) => format!("ERROR:{}", error.kind_name()),
                };
                total += 1;
                if actual != *expected {
                    failures.push(format!(
                        "{expression} [{row}, {target:?}]: expected {expected}, got {actual}"
                    ));
                }
            }
        }
        assert!(total >= 300, "corpus unexpectedly small: {total}");
        println!(
            "expr java-oracle corpus: {total} cases, {} failed",
            failures.len()
        );
        if !failures.is_empty() {
            let shown: Vec<String> = failures.iter().take(50).cloned().collect();
            panic!(
                "{} of {} corpus cases failed:\n{}",
                failures.len(),
                total,
                shown.join("\n")
            );
        }
    }

    #[test]
    fn unsupported_constructs_are_reported() {
        let context = EvalContext::new();
        // known but unimplemented function
        let expr = parse("DLookup(\"x\", \"y\")").unwrap();
        match expr.eval_column(&context, DataType::Text) {
            Err(ExprError::Unsupported(message)) => {
                assert!(message.contains("DLOOKUP"), "{message}");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
        // `Format` patterns which make Jackcess 5.0.4 itself throw a
        // non-EvalException are reported as unsupported (see the custom
        // format section): the date codes `M` / `P` / `p` have no
        // `DATE_FMT_BUILDERS` entry and produce a NullPointerException.
        for expression in [
            "Format(#2020-01-02#, \"MM\")",
            "Format(#2020-01-02#, \"yyyy-MM-dd\")",
            "Format(#2020-01-02#, \"p\")",
        ] {
            let expr = parse(expression).unwrap();
            match expr.eval_column(&context, DataType::Text) {
                Err(ExprError::Unsupported(message)) => {
                    assert!(message.contains("NullPointerException"), "{message}");
                }
                other => panic!("{expression}: expected Unsupported, got {other:?}"),
            }
        }
        // malformed DecimalFormat patterns are IllegalArgumentException in Java
        for expression in [
            "Format(5, \"0.0.0\")",
            "Format(5, \"0,\")",
            "Format(5, \"0%%\")",
        ] {
            let expr = parse(expression).unwrap();
            match expr.eval_column(&context, DataType::Text) {
                Err(ExprError::Unsupported(message)) => {
                    assert!(message.contains("IllegalArgumentException"), "{message}");
                }
                other => panic!("{expression}: expected Unsupported, got {other:?}"),
            }
        }
        // NaN / Infinity cannot be converted to a BigDecimal in Java
        // (NumberFormatException); the port reports it as unsupported
        for expression in ["Format(Log(-1), \"0.00\")", "Format(Exp(1000), \"0.00\")"] {
            let expr = parse(expression).unwrap();
            match expr.eval_column(&context, DataType::Text) {
                Err(ExprError::Unsupported(message)) => {
                    assert!(message.contains("NumberFormatException"), "{message}");
                }
                other => panic!("{expression}: expected Unsupported, got {other:?}"),
            }
        }
        // StrConv with a non-US locale id is not implemented
        let expr = parse("StrConv(\"abc\", 1, 1031)").unwrap();
        match expr.eval_column(&context, DataType::Text) {
            Err(ExprError::Unsupported(message)) => {
                assert!(message.contains("locale"), "{message}");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
        // non-ISO date literal
        match parse("#1/2/2020#") {
            Err(ExprError::Unsupported(message)) => {
                assert!(message.contains("ISO"), "{message}");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
        // MONEY/FLOAT result types map to Value.Type.DOUBLE in
        // `BaseEvalContext.TYPE_MAP`, so their coercion yields a double (the
        // column write applies the currency/float conversion)
        let expr = parse("1").unwrap();
        match expr.eval_column(&context, DataType::Money) {
            Ok(ExprValue::Double(v)) => assert_eq!(v, 1.0),
            other => panic!("expected a double for MONEY, got {other:?}"),
        }
        match expr.eval_column(&context, DataType::Float) {
            Ok(ExprValue::Double(v)) => assert_eq!(v, 1.0),
            other => panic!("expected a double for FLOAT, got {other:?}"),
        }
        // an unsupported target column type is still reported
        match expr.eval_column(&context, DataType::ExtDateTime) {
            Err(ExprError::Unsupported(message)) => {
                assert!(
                    message.contains("ExtDateTime") || message.contains("EXT_DATE_TIME"),
                    "{message}"
                );
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
        // LIKE class escape
        let expr = parse("[S] Like \"[\\\\d]*\"").unwrap();
        let mut ctx = EvalContext::new();
        ctx.set_column("S", ExprValue::Text("1".into()));
        match expr.eval_column(&ctx, DataType::Long) {
            Err(ExprError::Unsupported(message)) => {
                assert!(message.contains("LIKE"), "{message}");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
        // Java regex constructs inside a LIKE class are not approximated
        for pattern in ["[a&&b]", "[a[b]]"] {
            let expression = format!("[S] Like \"{pattern}\"");
            let expr = parse(&expression).unwrap();
            match expr.eval_column(&ctx, DataType::Long) {
                Err(ExprError::Unsupported(message)) => {
                    assert!(message.contains("class"), "{message}");
                }
                other => panic!("{pattern}: expected Unsupported, got {other:?}"),
            }
        }
        // locale date strings are not silently treated as "not a date"
        for expression in [
            "CDate(\"1/2/2020\")",
            "IsDate(\"1.5\")",
            "DateValue(\"Jan 2 2020\")",
        ] {
            let expr = parse(expression).unwrap();
            match expr.eval(&ctx) {
                Err(ExprError::Unsupported(message)) => {
                    assert!(message.contains("ISO"), "{message}");
                }
                other => panic!("{expression}: expected Unsupported, got {other:?}"),
            }
        }
        // ... but a word is simply not a date
        match parse("CDate(\"abc\")").unwrap().eval(&ctx) {
            Err(ExprError::Eval(_)) => {}
            other => panic!("expected Eval, got {other:?}"),
        }
        // non-BMP text is rejected where Java's UTF-16 indexing would differ
        let mut non_bmp = EvalContext::new();
        non_bmp.set_column("S", ExprValue::Text("\u{1F600}".into()));
        for expression in ["InStr([S], \"x\")", "[S] = \"\u{1F600}\""] {
            let expr = parse(expression).unwrap();
            match expr.eval_column(&non_bmp, DataType::Long) {
                Err(ExprError::Unsupported(message)) => {
                    assert!(message.contains("non-BMP"), "{message}");
                }
                other => panic!("{expression}: expected Unsupported, got {other:?}"),
            }
        }
    }

    #[test]
    fn parse_errors() {
        assert!(matches!(parse("1 +"), Err(ExprError::Parse(_))));
        assert!(matches!(parse("(1 + 2"), Err(ExprError::Parse(_))));
        assert!(matches!(parse("1 2"), Err(ExprError::Parse(_))));
        assert!(matches!(parse(""), Err(ExprError::Parse(_))));
        assert!(matches!(
            parse("NoSuchFunction(1)"),
            Err(ExprError::Parse(_))
        ));
        // a value which is not a date at all is a parse error, like Jackcess
        assert!(matches!(parse("#abc#"), Err(ExprError::Parse(_))));
        assert!(matches!(parse("#2020-13-01#"), Err(ExprError::Parse(_))));
    }

    #[test]
    fn default_value_semantics() {
        let context = EvalContext::new();
        // a leading '=' forces expression handling
        let expr = parse_with("=1+2", ParseOptions::default_value(DataType::Long, false)).unwrap();
        assert_eq!(
            expr.eval_column(&context, DataType::Long).unwrap(),
            ExprValue::Long(3)
        );
        // a text default without '=' is a literal, spaces included
        let expr = parse_with(" abc ", ParseOptions::default_value(DataType::Text, true)).unwrap();
        assert_eq!(
            expr.eval_column(&context, DataType::Text).unwrap(),
            ExprValue::Text(" abc ".into())
        );
        // a double quote starts a real string literal
        let expr =
            parse_with("\"abc\"", ParseOptions::default_value(DataType::Text, true)).unwrap();
        assert_eq!(
            expr.eval_column(&context, DataType::Text).unwrap(),
            ExprValue::Text("abc".into())
        );
        // the literal rule only applies to text columns
        assert!(
            parse_with(" abc ", ParseOptions::default_value(DataType::Long, true))
                .unwrap()
                .eval_column(&context, DataType::Long)
                .is_err()
        );
    }

    #[test]
    fn field_validator_semantics() {
        let mut context = EvalContext::new();
        context.set_this_value(ExprValue::Long(5));
        let expr = parse_with("> 3", ParseOptions::field_validator()).unwrap();
        assert!(expr.eval_condition(&context).unwrap());
        let expr = parse_with("Between 1 And 10", ParseOptions::field_validator()).unwrap();
        assert!(expr.eval_condition(&context).unwrap());
        // a plain expression is compared with the current field value
        let expr = parse_with("5", ParseOptions::field_validator()).unwrap();
        assert!(expr.eval_condition(&context).unwrap());
        let expr = parse_with("4", ParseOptions::field_validator()).unwrap();
        assert!(!expr.eval_condition(&context).unwrap());
        // a null condition is an error
        context.set_this_value(ExprValue::Null);
        let expr = parse_with("> 3", ParseOptions::field_validator()).unwrap();
        assert!(matches!(
            expr.eval_condition(&context),
            Err(ExprError::Eval(_))
        ));
    }

    #[test]
    fn native_values_and_identifiers() {
        let mut context = EvalContext::new();
        context
            .set_table_name("T")
            .set_column("Alpha", ExprValue::Long(3))
            .set_column("Beta", ExprValue::Double(2.0))
            .set_column("Flag", ExprValue::Bool(true));
        // case insensitive lookup, bracket and bare names, table qualifier
        let expr = parse("[ALPHA] + [T].[alpha] + beta + [Flag]").unwrap();
        assert_eq!(expr.eval(&context).unwrap(), ExprValue::Double(7.0));
        // booleans are normalised to -1
        let expr = parse("[Flag] + 1").unwrap();
        assert_eq!(expr.eval(&context).unwrap(), ExprValue::Long(0));
        // decimal literals stay decimals
        let expr = parse("1.5 + 2").unwrap();
        assert_eq!(
            expr.eval(&context).unwrap(),
            ExprValue::Decimal(Decimal::parse("3.5").unwrap())
        );
        // integer division and modulo follow Java
        assert_eq!(
            parse("-10 \\ 4").unwrap().eval(&context).unwrap(),
            ExprValue::Long(-2)
        );
        assert_eq!(
            parse("-10 Mod 4").unwrap().eval(&context).unwrap(),
            ExprValue::Long(-2)
        );
        // '^' binds tighter than a non-literal unary minus
        assert_eq!(
            parse("-[Alpha] ^ 2").unwrap().eval(&context).unwrap(),
            ExprValue::Long(-9)
        );
        // ... but not when the minus is directly before a number literal
        assert_eq!(
            parse("-2 ^ 2").unwrap().eval(&context).unwrap(),
            ExprValue::Long(4)
        );
        // identifiers() reports column references
        let identifiers = parse("[T].[Alpha]").unwrap().identifiers();
        assert_eq!(identifiers.len(), 1);
        assert_eq!(identifiers[0].object, "Alpha");
    }

    #[test]
    fn integer_column_coercions() {
        let context = EvalContext::new();
        // BYTE/INT first round to a Java Integer, then truncate to the width
        assert_eq!(
            parse("300")
                .unwrap()
                .eval_column(&context, DataType::Byte)
                .unwrap(),
            ExprValue::Byte(44)
        );
        assert_eq!(
            parse("-1")
                .unwrap()
                .eval_column(&context, DataType::Byte)
                .unwrap(),
            ExprValue::Byte(-1)
        );
        assert_eq!(
            parse("40000")
                .unwrap()
                .eval_column(&context, DataType::Int)
                .unwrap(),
            ExprValue::Int(-25536)
        );
        assert_eq!(
            parse("1.9")
                .unwrap()
                .eval_column(&context, DataType::Int)
                .unwrap(),
            ExprValue::Int(2)
        );
        // BIG_INT uses BigDecimal.longValue(): truncation toward zero
        assert_eq!(
            parse("2.9")
                .unwrap()
                .eval_column(&context, DataType::BigInt)
                .unwrap(),
            ExprValue::BigInt(2)
        );
        assert_eq!(
            parse("-2.9")
                .unwrap()
                .eval_column(&context, DataType::BigInt)
                .unwrap(),
            ExprValue::BigInt(-2)
        );
        assert_eq!(
            parse("1.5")
                .unwrap()
                .eval_column(&context, DataType::Numeric)
                .unwrap(),
            ExprValue::Decimal(Decimal::parse("1.5").unwrap())
        );
        // BOOLEAN rounds to a long first, so 0.5 is false and 1.5 is true
        assert_eq!(
            parse("0.5")
                .unwrap()
                .eval_column(&context, DataType::Boolean)
                .unwrap(),
            ExprValue::Bool(false)
        );
        assert_eq!(
            parse("1.5")
                .unwrap()
                .eval_column(&context, DataType::Boolean)
                .unwrap(),
            ExprValue::Bool(true)
        );
        assert_eq!(
            parse("Null")
                .unwrap()
                .eval_column(&context, DataType::Boolean)
                .unwrap(),
            ExprValue::Null
        );
    }

    #[test]
    fn byte_int_and_bigint_context_values_normalise_like_jackcess() {
        let mut context = EvalContext::new();
        context
            .set_column("BT", ExprValue::Byte(5))
            .set_column("SH", ExprValue::Int(-7))
            .set_column("BG", ExprValue::BigInt(1_000_000_000_000))
            .set_column("BIG", ExprValue::BigInt(9_007_199_254_740_993));
        // BYTE and INT become LONG, BIG_INT becomes BIG_DEC
        assert_eq!(
            parse("[BT]").unwrap().eval(&context).unwrap(),
            ExprValue::Long(5)
        );
        assert_eq!(
            parse("[SH] * 2").unwrap().eval(&context).unwrap(),
            ExprValue::Long(-14)
        );
        assert_eq!(
            parse("[BG] + 1").unwrap().eval(&context).unwrap(),
            ExprValue::Decimal(Decimal::parse("1000000000001").unwrap())
        );
        // Jackcess converts BIG_INT through a double, so 2^53 + 1 loses its
        // low bit; the port reproduces that
        assert_eq!(
            parse("[BIG]").unwrap().eval(&context).unwrap(),
            ExprValue::Decimal(Decimal::parse("9007199254740992").unwrap())
        );
    }

    #[test]
    fn constant_and_identifier_analysis() {
        assert!(parse("1 + 2 * 3").unwrap().is_constant());
        assert!(!parse("[A] + 1").unwrap().is_constant());
        assert!(!parse("Now()").unwrap().is_constant());
        assert!(parse("IIf(True, 1, 2)").unwrap().is_constant());
        let identifiers = parse("IIf([A] > 0, [B], [T].[C])").unwrap().identifiers();
        let names: Vec<&str> = identifiers.iter().map(|id| id.object.as_str()).collect();
        assert_eq!(names, vec!["A", "B", "C"]);
    }

    // -- tests for the functions added after the first oracle corpus ---------

    fn eval_value(expression: &str, context: &EvalContext) -> ExprValue {
        parse(expression)
            .unwrap_or_else(|error| panic!("{expression}: {error}"))
            .eval(context)
            .unwrap_or_else(|error| panic!("{expression}: {error}"))
    }

    fn eval_error(expression: &str, context: &EvalContext) -> ExprError {
        match parse(expression).unwrap().eval(context) {
            Ok(value) => panic!("{expression}: expected an error, got {value:?}"),
            Err(error) => error,
        }
    }

    #[test]
    fn datepart_dateadd_datediff_semantics() {
        let context = EvalContext::new();
        assert_eq!(
            eval_value("DatePart(\"yyyy\", #2020-01-02 03:04:05#)", &context),
            ExprValue::Long(2020)
        );
        assert_eq!(
            eval_value("DatePart(\"q\", #2020-12-31#)", &context),
            ExprValue::Long(4)
        );
        assert_eq!(
            eval_value("DatePart(\"y\", #2020-02-29#)", &context),
            ExprValue::Long(60)
        );
        // Weekday is shifted by the first day parameter (vbSunday default)
        assert_eq!(
            eval_value("DatePart(\"w\", #2020-01-05#)", &context),
            ExprValue::Long(1)
        );
        assert_eq!(
            eval_value("DatePart(\"w\", #2020-01-05#, 2)", &context),
            ExprValue::Long(7)
        );
        // firstDay 1 (Sunday) with firstWeekType 2 (first four day week)
        assert_eq!(
            eval_value("DatePart(\"ww\", #2020-01-05#, 1, 2)", &context),
            ExprValue::Long(2)
        );
        assert_eq!(
            eval_value("DatePart(\"ww\", #2020-01-01#, 1, 1)", &context),
            ExprValue::Long(1)
        );
        assert_eq!(
            eval_value("DatePart(\"ww\", #2021-06-07#, 1, 2)", &context),
            ExprValue::Long(23)
        );
        // null propagation and error cases
        assert_eq!(
            eval_value("DatePart(\"m\", Null)", &context),
            ExprValue::Null
        );
        assert!(matches!(
            eval_error("DatePart(\"bad\", #2020-01-02#)", &context),
            ExprError::Eval(_)
        ));
        assert!(matches!(
            eval_error("DatePart(\"ww\", #2020-01-02#, 1, 4)", &context),
            ExprError::Eval(_)
        ));

        // DateAdd clamps to the end of the target month
        assert_eq!(
            canonical(&eval_value("DateAdd(\"m\", 1, #2020-01-31#)", &context)),
            "DateTime:2020-02-29T00:00:00.000000000"
        );
        assert_eq!(
            canonical(&eval_value("DateAdd(\"yyyy\", 1, #2020-02-29#)", &context)),
            "DateTime:2021-02-28T00:00:00.000000000"
        );
        assert_eq!(
            canonical(&eval_value(
                "DateAdd(\"h\", 25, #2020-01-02 23:30:00#)",
                &context
            )),
            "DateTime:2020-01-04T00:30:00.000000000"
        );
        assert_eq!(
            eval_value("DateAdd(\"d\", 1, Null)", &context),
            ExprValue::Null
        );
        assert!(matches!(
            eval_error("DateAdd(\"bad\", 1, #2020-01-02#)", &context),
            ExprError::Eval(_)
        ));
        // a year outside java.time's range is an EvalException in Jackcess
        assert!(matches!(
            eval_error("DateAdd(\"yyyy\", 1000000000, #2020-01-02#)", &context),
            ExprError::Eval(_)
        ));

        assert_eq!(
            eval_value("DateDiff(\"d\", #2020-01-01#, #2021-01-01#)", &context),
            ExprValue::Long(366)
        );
        assert_eq!(
            eval_value("DateDiff(\"d\", #2021-01-01#, #2020-01-01#)", &context),
            ExprValue::Long(-366)
        );
        assert_eq!(
            eval_value("DateDiff(\"m\", #2019-12-31#, #2020-02-01#)", &context),
            ExprValue::Long(2)
        );
        assert_eq!(
            eval_value(
                "DateDiff(\"h\", #2020-01-01 00:30:00#, #2020-01-01 23:30:00#)",
                &context
            ),
            ExprValue::Long(23)
        );
        assert_eq!(
            eval_value(
                "DateDiff(\"s\", #2020-01-01 01:00:30#, #2020-01-01 02:00:15#)",
                &context
            ),
            ExprValue::Long(3585)
        );
        assert_eq!(
            eval_value("DateDiff(\"m\", Null, #2020-01-02#)", &context),
            ExprValue::Null
        );
        assert!(matches!(
            eval_error(
                "DateDiff(\"ww\", #2020-01-01#, #2020-01-02#, 1, 9)",
                &context
            ),
            ExprError::Eval(_)
        ));
    }

    #[test]
    fn injected_clock_drives_the_impure_date_functions() {
        let clock = ldt(2021, 6, 7, 15, 4, 5);
        let context = EvalContext::new().with_clock(clock);
        assert_eq!(
            canonical(&eval_value("Now()", &context)),
            "DateTime:2021-06-07T15:04:05.000000000"
        );
        assert_eq!(
            canonical(&eval_value("Date()", &context)),
            "DateTime:2021-06-07T00:00:00.000000000"
        );
        assert_eq!(
            canonical(&eval_value("Time()", &context)),
            "DateTime:1899-12-30T15:04:05.000000000"
        );
        assert_eq!(eval_value("Timer()", &context), ExprValue::Double(54245.0));
        // Date() has a DATE value type, Now() a DATE_TIME one, Time() a TIME one
        assert_eq!(
            eval_value("TypeName(Date())", &context),
            ExprValue::Text("Date".to_string())
        );
        assert_eq!(
            eval_value("TypeName(Time())", &context),
            ExprValue::Text("Date".to_string())
        );
        // without an injected clock the system UTC time is used
        let default_context = EvalContext::new();
        assert!(matches!(
            eval_value("Now()", &default_context),
            ExprValue::DateTime { .. }
        ));
        assert!(matches!(
            eval_value("Timer()", &default_context),
            ExprValue::Double(value) if (0.0..86400.0).contains(&value)
        ));
    }

    #[test]
    fn rnd_matches_java_util_random() {
        let clock = ldt(2021, 6, 7, 15, 4, 5);
        let context = EvalContext::new().with_clock(clock);
        // first values of java.util.Random(seed).nextFloat() (Java prints the
        // shortest float representation, which Rust parses to the same f32)
        assert_eq!(
            eval_value("Rnd(42)", &context),
            ExprValue::Double(0.7275637f32 as f64)
        );
        assert_eq!(
            eval_value("Rnd(42)", &context),
            ExprValue::Double(0.054665208f32 as f64)
        );
        assert_eq!(
            eval_value("Rnd(42)", &context),
            ExprValue::Double(0.6832234f32 as f64)
        );
        // Rnd(0) returns the last generated value
        assert_eq!(
            eval_value("Rnd(0)", &context),
            ExprValue::Double(0.6832234f32 as f64)
        );
        // a negative seed always returns the same value and resets the rest
        assert_eq!(
            eval_value("Rnd(-1)", &context),
            ExprValue::Double(0.2689426f32 as f64)
        );
        assert_eq!(
            eval_value("Rnd(-1)", &context),
            ExprValue::Double(0.2689426f32 as f64)
        );
        // the reset generator for the seed starts over
        assert_eq!(
            eval_value("Rnd(42)", &context),
            ExprValue::Double(0.7275637f32 as f64)
        );
        // before any value is generated Rnd(0) is Access' initial 0.01953125
        let fresh = EvalContext::new().with_clock(clock);
        assert_eq!(eval_value("Rnd(0)", &fresh), ExprValue::Double(0.01953125));
        // the unseeded generator is seeded from the (injectable) clock
        let unseeded_a = EvalContext::new().with_clock(clock);
        let unseeded_b = EvalContext::new().with_clock(clock);
        let a = eval_value("Rnd()", &unseeded_a);
        assert_eq!(a, eval_value("Rnd()", &unseeded_b));
        let other = EvalContext::new().with_clock(ldt(2021, 6, 7, 15, 4, 6));
        assert_eq!(eval_value("Rnd(0)", &other), ExprValue::Double(0.01953125));
        assert!(matches!(a, ExprValue::Double(value) if (0.0..1.0).contains(&value)));
        assert_eq!(
            eval_value("Rnd(42)", &context),
            ExprValue::Double(0.054665208f32 as f64)
        );

        // GenUniqueID is a random 32 bit value: the clock seeded generator
        // makes it reproducible per context
        let first = eval_value("GenUniqueID()", &unseeded_a);
        assert!(matches!(first, ExprValue::Long(_)));
        assert_ne!(first, eval_value("GenUniqueID()", &unseeded_a));
        let fresh_ids = EvalContext::new().with_clock(clock);
        assert_eq!(first, eval_value("GenUniqueID()", &fresh_ids));

        // GenGUID is a random version 4 UUID in Jackcess' upper case, braced
        // string form
        let guid = match eval_value("GenGUID()", &fresh_ids) {
            ExprValue::Text(text) => text,
            other => panic!("expected text GUID, got {other:?}"),
        };
        assert_eq!(guid.len(), 38, "{guid}");
        assert!(guid.starts_with('{') && guid.ends_with('}'), "{guid}");
        assert_eq!(&guid[9..10], "-");
        assert_eq!(&guid[14..15], "-");
        assert_eq!(&guid[15..16], "4", "version nibble: {guid}");
        assert_eq!(&guid[19..20], "-");
        assert!(
            matches!(&guid[20..21], "8" | "9" | "A" | "B"),
            "variant nibble: {guid}"
        );
        assert_eq!(&guid[24..25], "-");
        assert!(guid[1..37]
            .chars()
            .all(|c| c == '-' || c.is_ascii_hexdigit()));
        let fresh_guid = EvalContext::new().with_clock(clock);
        assert_eq!(
            guid,
            match eval_value("GenGUID()", &fresh_guid) {
                ExprValue::Text(text) => text,
                other => panic!("expected text GUID, got {other:?}"),
            }
        );
        assert!(matches!(
            eval_error("GenGUID(1)", &context),
            ExprError::Eval(_)
        ));
    }

    #[test]
    fn format_function_semantics() {
        let context = EvalContext::new();
        let text = |expression: &str| match eval_value(expression, &context) {
            ExprValue::Text(text) => text,
            other => panic!("{expression}: expected text, got {other:?}"),
        };
        assert_eq!(text("FormatNumber(1234.5)"), "1,234.50");
        assert_eq!(text("FormatNumber(-1234.5)"), "-1,234.50");
        assert_eq!(text("FormatNumber(2.345)"), "2.34");
        assert_eq!(text("FormatNumber(2.355)"), "2.36");
        assert_eq!(text("FormatNumber(0.5, 2, 0)"), ".50");
        assert_eq!(text("FormatNumber(-1234.5, 2, -1, -1)"), "(1,234.50)");
        assert_eq!(text("FormatNumber(-0.5, 0)"), "-0");
        // the currency patterns use parentheses for negative values
        assert_eq!(text("FormatCurrency(-1234.5)"), "($1,234.50)");
        assert_eq!(text("FormatCurrency(1234.5)"), "$1,234.50");
        // FormatPercent groups by default, the "Percent" predef does not
        assert_eq!(text("FormatPercent(0.125)"), "12.50%");
        assert_eq!(text("FormatPercent(12345.6789)"), "1,234,567.89%");
        assert_eq!(text("Format(1234.5, \"Percent\")"), "123450.00%");
        assert_eq!(text("Format(1234.5, \"Fixed\")"), "1234.50");
        assert_eq!(text("Format(1234.5, \"Standard\")"), "1,234.50");
        assert_eq!(text("Format(-1234.5, \"Currency\")"), "($1,234.50)");
        assert_eq!(text("Format(1234.5, \"Scientific\")"), "1.23E+3");
        assert_eq!(text("Format(0, \"Scientific\")"), "0.00E+0");
        assert_eq!(text("Format(0.001234, \"Scientific\")"), "1.23E-3");
        assert_eq!(
            text("Format(#2020-01-02 03:04:05#, \"General Date\")"),
            "1/2/2020 3:04:05 AM"
        );
        assert_eq!(
            text("Format(#2020-01-02 03:04:05#, \"Long Date\")"),
            "Thursday, January 02, 2020"
        );
        assert_eq!(
            text("Format(#2020-01-02 03:04:05#, \"Medium Date\")"),
            "02-Jan-20"
        );
        assert_eq!(
            text("Format(#2020-01-02 03:04:05#, \"Short Date\")"),
            "1/2/2020"
        );
        assert_eq!(
            text("Format(#2020-01-02 03:04:05#, \"Long Time\")"),
            "3:04:05 AM"
        );
        assert_eq!(
            text("Format(#2020-01-02 03:04:05#, \"Medium Time\")"),
            "03:04 AM"
        );
        assert_eq!(
            text("Format(#2020-01-02 03:04:05#, \"Short Time\")"),
            "03:04"
        );
        assert_eq!(text("Format(-1, \"Yes/No\")"), "Yes");
        assert_eq!(text("Format(0, \"On/Off\")"), "Off");
        assert_eq!(text("Format(1234.5, \"General Number\")"), "1234.5");
        // predefs return the empty string for null and empty strings
        assert_eq!(text("Format(Null, \"Fixed\")"), "");
        assert_eq!(text("Format(\"\", \"Fixed\")"), "");
        // ... and uncoercible values are returned unchanged
        assert_eq!(text("Format(\"abc\", \"Fixed\")"), "abc");
        // an empty format is a no-op conversion
        assert_eq!(eval_value("Format(5, \"\")", &context), ExprValue::Long(5));
        assert_eq!(text("Format(5)"), "5");
        assert!(matches!(
            eval_error("FormatNumber(1234.5, 2, 5)", &context),
            ExprError::Eval(_)
        ));
        assert!(matches!(
            eval_error("FormatNumber(\"\")", &context),
            ExprError::Eval(_)
        ));
        assert!(matches!(
            eval_error("FormatDateTime(#2020-01-02#, 5)", &context),
            ExprError::Eval(_)
        ));
        assert!(matches!(
            eval_error("FormatDateTime(\"abc\")", &context),
            ExprError::Eval(_)
        ));
    }

    #[test]
    fn custom_format_number_semantics() {
        let context = EvalContext::new();
        let text = |expression: &str| match eval_value(expression, &context) {
            ExprValue::Text(text) => text,
            other => panic!("{expression}: expected text, got {other:?}"),
        };
        // fixed / optional fraction digits, minimum integer digits
        assert_eq!(text("Format(1234.5, \"0.00\")"), "1234.50");
        assert_eq!(text("Format(1234.5, \"0.0000\")"), "1234.5000");
        assert_eq!(text("Format(1234.5, \"0.00##\")"), "1234.50");
        assert_eq!(text("Format(12345.6789, \"0.00##\")"), "12345.6789");
        assert_eq!(text("Format(1, \"0.##\")"), "1.");
        assert_eq!(text("Format(0.5, \"0.##\")"), "0.5");
        assert_eq!(text("Format(0.5, \"#.##\")"), ".5");
        assert_eq!(text("Format(0, \"#.##\")"), ".");
        assert_eq!(text("Format(0, \"0.00\")"), "0.00");
        assert_eq!(text("Format(1, \"00\")"), "01");
        assert_eq!(text("Format(1234, \"000000\")"), "001234");
        assert_eq!(text("Format(5, \"#\")"), "5");
        // HALF_EVEN rounding like DecimalFormat
        assert_eq!(text("Format(2.5, \"0\")"), "2");
        assert_eq!(text("Format(3.5, \"0\")"), "4");
        assert_eq!(text("Format(-2.5, \"0\")"), "-2");
        assert_eq!(text("Format(2.675, \"0.00\")"), "2.68");
        assert_eq!(text("Format(1.005, \"0.00\")"), "1.00");
        assert_eq!(text("Format(0.125, \"0.00\")"), "0.12");
        // grouping from the pattern, not from the value
        assert_eq!(text("Format(1234.5, \"#,##0.00\")"), "1,234.50");
        assert_eq!(text("Format(1234567.891, \"#,##0.00\")"), "1,234,567.89");
        assert_eq!(text("Format(1234, \"0,000\")"), "1,234");
        assert_eq!(text("Format(1, \"0,000\")"), "0,001");
        assert_eq!(text("Format(1234.5, \"0.00\")"), "1234.50");
        // percent multiplier, including its effect on the rounding fallback
        assert_eq!(text("Format(0.125, \"0.00%\")"), "12.50%");
        assert_eq!(text("Format(0.125, \"0%\")"), "12%");
        assert_eq!(text("Format(0.0001, \"0.00%\")"), "0.01%");
        assert_eq!(text("Format(0.000001, \"0.00%\")"), "0.00%");
        // quoted literals in the prefix / suffix
        assert_eq!(text("Format(1234.5, \"$#,##0.00\")"), "$1,234.50");
        assert_eq!(text("Format(1234.5, \"0.00 kg\")"), "1234.50 kg");
        assert_eq!(text("Format(1234.5, \"\"\"x\"\"0.00\")"), "x1234.50");
        assert_eq!(text("Format(1234.5, \"0.00 x\")"), "1234.50 x");
        // negative subpatterns and the Access defaults for missing ones
        assert_eq!(text("Format(-1234.5, \"0.00;(0.00)\")"), "(1234.50)");
        assert_eq!(text("Format(-1234.5, \"0.00\")"), "-1234.50");
        assert_eq!(
            text("Format(-1234.5, \"0.00;\"\"neg\"\"0.00\")"),
            "neg1234.50"
        );
        assert_eq!(text("Format(1234.5, \"0.00;0.00\")"), "1234.50");
        assert_eq!(text("Format(-1234.5, \"$0.00;($0.00)\")"), "($1234.50)");
        // single quotes are literal characters in an Access format (double
        // quotes are the literal-text quoting)
        assert_eq!(text("Format(-1234.5, \"0.00;'neg'0.00\")"), "'neg'1234.50");
        // zero / null subpatterns
        assert_eq!(text("Format(0, \"0.00;0.00;\"\"zero\"\"\")"), "zero");
        assert_eq!(
            text("Format(0, \"0.00;0.00;\"\"zero\"\";\"\"null\"\"\")"),
            "zero"
        );
        assert_eq!(
            text("Format(Null, \"0.00;0.00;\"\"zero\"\";\"\"null\"\"\")"),
            "null"
        );
        assert_eq!(text("Format(0, \"0.00;0.00;'zero'\")"), "'zero'");
        assert_eq!(text("Format(Null, \"0.00\")"), "");
        // the Access zero format strips the zeros Java inserts
        assert_eq!(text("Format(0.0001, \"#.##\")"), ".");
        assert_eq!(text("Format(0.5, \"#\")"), "");
        // scientific notation with the E/e and +/- handling
        assert_eq!(text("Format(1234.5, \"0.00E+00\")"), "1.23E+03");
        assert_eq!(text("Format(1234.5, \"0.00E-00\")"), "1.23E03");
        assert_eq!(text("Format(1234.5, \"0.00e+00\")"), "1.23e+03");
        assert_eq!(text("Format(1234.5, \"0.00e-00\")"), "1.23e03");
        assert_eq!(text("Format(0.0001234, \"0.0E+0\")"), "1.2E-4");
        assert_eq!(text("Format(0, \"0.000E+000\")"), "0.000E+000");
        assert_eq!(text("Format(-1234.5, \"0.00E+00\")"), "-1.23E+03");
        // values which cannot be converted are returned as is
        assert_eq!(text("Format(\"abc\", \"0.00\")"), "abc");
        assert_eq!(text("Format(\"\", \"0.00\")"), "");
        // booleans are the Access -1/0 longs
        assert_eq!(text("Format(True, \"0.00\")"), "-1.00");
        assert_eq!(text("Format(False, \"0.00\")"), "0.00");
        // dates are formatted as their date double
        assert_eq!(text("Format(#2020-01-02#, \"0.00\")"), "43832.00");
        assert_eq!(text("Format(\"2.5\", \"0.00\")"), "2.50");

        // invalid patterns: Java's non-Eval exceptions become Unsupported
        for expression in [
            "Format(5, \"0.0.0\")",
            "Format(5, \"0,\")",
            "Format(5, \"0%%\")",
            "Format(5, \"0.0#0\")",
        ] {
            assert!(matches!(
                eval_error(expression, &context),
                ExprError::Unsupported(_)
            ));
        }
        // ... while an unterminated quote / colour code is a ParseException
        // in `createFormat`, which the function call machinery wraps into the
        // `EvalException` (`FuncVar.evalVar` does the same in Jackcess)
        for expression in ["Format(5, Chr(34) & \"abc\")", "Format(5, \"[Red\")"] {
            match eval_error(expression, &context) {
                ExprError::Eval(message) => {
                    assert!(message.contains("Missing closing"), "{message}");
                }
                other => panic!("{expression}: expected Eval, got {other:?}"),
            }
        }

        // a 38 digit coefficient is formatted exactly ...
        assert_eq!(
            text("Format(92345678901234567890123456789012345678, \"0.00\")"),
            "92345678901234567890123456789012345678.00"
        );
        assert_eq!(
            text("Format(1234567890123456789012345678901234567, \"0.00E+00\")"),
            "1.23E+36"
        );
        // ... and the percent multiplier keeps every digit (the Format number
        // path is arbitrary precision `BigDecimal`)
        assert_eq!(
            text("Format(92345678901234567890123456789012345678, \"0.00%\")"),
            "9234567890123456789012345678901234567800.00%"
        );
        // `ScientificFormat` reads the character after the exponent `E`
        assert!(matches!(
            eval_error("Format(1234.5, \"0.00E+00E\")", &context),
            ExprError::Unsupported(_)
        ));
    }

    #[test]
    fn custom_format_date_semantics() {
        let context = EvalContext::new();
        let text = |expression: &str| match eval_value(expression, &context) {
            ExprValue::Text(text) => text,
            other => panic!("{expression}: expected text, got {other:?}"),
        };
        assert_eq!(text("Format(#2020-01-02#, \"yyyy-mm-dd\")"), "2020-01-02");
        assert_eq!(text("Format(#2020-01-02#, \"yyyy/mm/dd\")"), "2020/01/02");
        assert_eq!(text("Format(#2020-01-02#, \"m/d/yyyy\")"), "1/2/2020");
        assert_eq!(text("Format(#2020-01-02#, \"mm/dd/yy\")"), "01/02/20");
        assert_eq!(text("Format(#2020-01-02#, \"d mmm yyyy\")"), "2 Jan 2020");
        assert_eq!(
            text("Format(#2020-01-02#, \"mmmm d, yyyy\")"),
            "January 2, 2020"
        );
        assert_eq!(text("Format(#2020-01-02#, \"ddd\")"), "Thu");
        assert_eq!(text("Format(#2020-01-02#, \"dddd\")"), "Thursday");
        assert_eq!(text("Format(#2020-01-02#, \"ddddd\")"), "1/2/2020");
        assert_eq!(
            text("Format(#2020-01-02#, \"dddddd\")"),
            "Thursday, January 02, 2020"
        );
        assert_eq!(
            text("Format(#2020-01-02 03:04:05#, \"ttttt\")"),
            "3:04:05 AM"
        );
        // hour fields switch to the 12 hour clock only with an am/pm pattern
        assert_eq!(
            text("Format(#2020-01-02 15:04:05#, \"h:nn:ss\")"),
            "15:04:05"
        );
        assert_eq!(
            text("Format(#2020-01-02 15:04:05#, \"h:nn:ss AM/PM\")"),
            "3:04:05 PM"
        );
        assert_eq!(
            text("Format(#2020-01-02 03:04:05#, \"hh:nn:ss am/pm\")"),
            "03:04:05 am"
        );
        assert_eq!(
            text("Format(#2020-01-02 03:04:05#, \"hh:nn A/P\")"),
            "03:04 A"
        );
        assert_eq!(
            text("Format(#2020-01-02 15:04:05#, \"hh:nn a/p\")"),
            "03:04 p"
        );
        // 24 hour clock when there is no am/pm pattern ('H' is a literal)
        assert_eq!(text("Format(#2020-01-02 15:04:05#, \"h:nn\")"), "15:04");
        assert_eq!(text("Format(#2020-01-02 03:04:05#, \"h:nn\")"), "3:04");
        assert_eq!(text("Format(#2020-01-02 03:04:05#, \"HH:nn\")"), "HH:04");
        // week based fields (first day / first week type arguments)
        assert_eq!(text("Format(#2020-01-02#, \"q y w ww\")"), "1 2 5 1");
        assert_eq!(text("Format(#1899-12-30#, \"q y w ww\")"), "4 364 7 52");
        assert_eq!(text("Format(#2020-01-05#, \"w ww\")"), "1 2");
        assert_eq!(text("Format(#2020-01-05#, \"w ww\", 2, 2)"), "7 1");
        // the 'c' general format depends on the value's date/time type
        assert_eq!(
            text("Format(#2020-01-02 03:04:05#, \"c\")"),
            "1/2/2020 3:04:05 AM"
        );
        assert_eq!(text("Format(#2020-01-02#, \"c\")"), "1/2/2020");
        assert_eq!(text("Format(#03:04:05#, \"c\")"), "3:04:05 AM");
        assert_eq!(text("Format(43832, \"c\")"), "1/2/2020");
        // quoted literals, colour codes and escapes
        assert_eq!(
            text("Format(#2020-01-02#, \"\"\"Date: \"\"yyyy-mm-dd\")"),
            "Date: 2020-01-02"
        );
        assert_eq!(text("Format(#2020-01-02#, \"[Red]yyyy\")"), "2020");
        assert_eq!(text("Format(#2020-01-02#, \"!yyyy\")"), "!2020");
        // a pattern which is only a literal still formats
        assert_eq!(text("Format(#2020-01-02#, \":\")"), ":");
        // non-date values are coerced through the date/number conversions
        assert_eq!(text("Format(\"2020-01-02\", \"yyyy\")"), "2020");
        assert_eq!(text("Format(43832, \"yyyy-mm-dd\")"), "2020-01-02");
        assert_eq!(text("Format(\"abc\", \"yyyy\")"), "abc");
        // date codes without a builder are a Java NullPointerException
        for pattern in ["MM", "yyyy-MM-dd", "p", "P"] {
            let expression = format!("Format(#2020-01-02#, \"{pattern}\")");
            match eval_error(&expression, &context) {
                ExprError::Unsupported(message) => {
                    assert!(message.contains("NullPointerException"), "{message}");
                }
                other => panic!("{pattern}: expected Unsupported, got {other:?}"),
            }
        }
        // an invalid first week type is an EvalException (only for `w`/`ww`)
        assert!(matches!(
            eval_error("Format(#2020-01-02#, \"ww\", 1, 4)", &context),
            ExprError::Eval(_)
        ));
        assert_eq!(text("Format(#2020-01-02#, \"yyyy\", 1, 4)"), "2020");
    }

    #[test]
    fn custom_format_text_and_general_semantics() {
        let context = EvalContext::new();
        let text = |expression: &str| match eval_value(expression, &context) {
            ExprValue::Text(text) => text,
            other => panic!("{expression}: expected text, got {other:?}"),
        };
        // `@` fills characters, `&` is an optional placeholder
        assert_eq!(text("Format(\"Hello\", \"@@@@@@@\")"), "  Hello");
        assert_eq!(text("Format(\"Hello\", \"!@@@@@@@\")"), "Hello  ");
        assert_eq!(text("Format(\"Hello\", \"!@@@\")"), "llo");
        assert_eq!(text("Format(\"Hello\", \"@@@\")"), "Hello");
        assert_eq!(text("Format(\"Hello\", \"@-@\")"), "H-ello");
        assert_eq!(text("Format(\"abc\", \"@@@@@\")"), "  abc");
        assert_eq!(text("Format(\"abc\", \"@\")"), "abc");
        assert_eq!(text("Format(\"abc\", \"&\")"), "abc");
        assert_eq!(text("Format(\"abc\", \"@@@@;empty\")"), " abc");
        assert_eq!(text("Format(\"abc\", \"@@@;empty\")"), "abc");
        assert_eq!(text("Format(\"\", \"@@@@;empty\")"), "empty");
        assert_eq!(text("Format(Null, \"@@@@;empty\")"), "empty");
        assert_eq!(text("Format(\"\", \"@@@@\")"), "");
        // case conversion (a pair of < and > cancels out)
        assert_eq!(text("Format(\"abc\", \">@@@@\")"), " ABC");
        assert_eq!(text("Format(\"ABC\", \"<@@@@\")"), " abc");
        assert_eq!(text("Format(\"MiXeD\", \">@\")"), "MIXED");
        assert_eq!(text("Format(\"MiXeD\", \">@<@>@\")"), "MIXED");
        assert_eq!(text("Format(\"MiXeD\", \">@@@<\")"), "MiXeD");
        assert_eq!(text("Format(\"a,b\", \"\"\"x\"\"@\")"), "xa,b");
        // a plain literal-only format is the "general" (choice) format
        assert_eq!(text("Format(5, \"Hi;;Lo\")"), "Hi");
        assert_eq!(text("Format(-5, \"Hi;;Lo\")"), "-Hi");
        assert_eq!(text("Format(0, \"Hi;;Lo\")"), "Lo");
        assert_eq!(text("Format(Null, \"Hi;;Lo\")"), "");
        assert_eq!(text("Format(True, \"Hi;;Lo\")"), "-Hi");
        assert_eq!(text("Format(-1, \"Y;;N\")"), "-Y");
        assert_eq!(text("Format(0, \"Y;;N\")"), "N");
        assert_eq!(text("Format(1, \"Y;;N\")"), "Y");
        assert_eq!(text("Format(1, \"Hi;Lo;Zx;Nx\")"), "Hi");
        assert_eq!(text("Format(-1, \"Hi;Lo;Zx;Nx\")"), "Lo");
        assert_eq!(text("Format(0, \"Hi;Lo;Zx;Nx\")"), "Zx");
        assert_eq!(text("Format(Null, \"Hi;Lo;Zx;Nx\")"), "Nx");
        assert_eq!(text("Format(\"abc\", \"Hi;;Lo\")"), "abc");
        // quoted literals in a choice format
        assert_eq!(text("Format(1, \"\"\"a\"\";\"\"b\"\"\")"), "a");
        assert_eq!(text("Format(-1, \"\"\"a\"\";\"\"b\"\"\")"), "b");
    }

    #[test]
    fn decimal_format_pattern_properties() {
        let parse = |pattern: &str| JavaDecimalFormat::parse(pattern).unwrap();
        let df = parse("#,##0.00");
        assert_eq!(df.min_int, 1);
        assert_eq!(df.max_frac, 2);
        assert_eq!(df.min_frac, 2);
        assert_eq!(df.grouping_size, 3);
        assert!(df.grouping_used);
        assert!(df.decimal_always);
        // `#.##` has no `0` pattern character; DecimalFormat derives
        // min integer digits 1 which Jackcess then forces to 0
        let df = parse("#.##");
        assert_eq!(df.min_int, 1);
        assert_eq!(df.max_frac, 2);
        assert_eq!(df.min_frac, 0);
        assert_eq!(df.grouping_size, 0);
        assert!(!df.grouping_used);
        let df = parse("0,000");
        assert_eq!(df.min_int, 4);
        assert_eq!(df.grouping_size, 3);
        let df = parse("0.00%");
        assert_eq!(df.multiplier, 100);
        assert_eq!(df.max_dec_digits(), 4);
        let df = parse("0.00E00");
        // Jackcess consumes the exponent sign: the DecimalFormat pattern is
        // `0.00E00`
        assert!(df.use_exp);
        assert_eq!(df.min_exp_digits, 2);
        assert_eq!(df.max_int, 1);
        assert_eq!(df.min_int, 1);
        // quoted literals become affixes
        let df = parse("'$'#,##0.00");
        assert_eq!(df.prefix, "$");
        let df = parse("'part '0.00");
        assert_eq!(df.prefix, "part ");
        // `.` at the start/end of the pattern keeps the decimal separator
        let df = parse(".00");
        assert_eq!(df.min_int, 0);
        assert!(df.decimal_always);
        // an unexpected 0 after the fraction is malformed
        assert!(JavaDecimalFormat::parse("0.0#0").is_err());
    }

    #[test]
    fn formatdatetime_semantics() {
        let context = EvalContext::new();
        let text = |expression: &str| match eval_value(expression, &context) {
            ExprValue::Text(text) => text,
            other => panic!("{expression}: expected text, got {other:?}"),
        };
        assert_eq!(
            text("FormatDateTime(#2020-01-02 03:04:05#)"),
            "1/2/2020 3:04:05 AM"
        );
        assert_eq!(
            text("FormatDateTime(#2020-01-02 03:04:05#, 1)"),
            "Thursday, January 02, 2020"
        );
        assert_eq!(text("FormatDateTime(#2020-01-02 03:04:05#, 2)"), "1/2/2020");
        assert_eq!(
            text("FormatDateTime(#2020-01-02 03:04:05#, 3)"),
            "3:04:05 AM"
        );
        assert_eq!(text("FormatDateTime(#2020-01-02 03:04:05#, 4)"), "03:04");
        assert_eq!(text("FormatDateTime(#12:34:56#)"), "12:34:56 PM");
        assert_eq!(text("FormatDateTime(#2020-01-02#)"), "1/2/2020");
        assert_eq!(text("FormatDateTime(0, 0)"), "12:00:00 AM");
        assert_eq!(text("FormatDateTime(TimeSerial(15, 4, 5), 4)"), "15:04");
        assert_eq!(
            eval_value("FormatDateTime(Null)", &context),
            ExprValue::Null
        );
    }

    #[test]
    fn financial_function_semantics() {
        let context = EvalContext::new();
        let double = |expression: &str| match eval_value(expression, &context) {
            ExprValue::Double(value) => value,
            other => panic!("{expression}: expected double, got {other:?}"),
        };
        assert_eq!(double("SLN(10000, 1000, 5)"), 1800.0);
        assert_eq!(double("SYD(10000, 1000, 5, 2)"), 2400.0);
        assert_eq!(double("DDB(1000, 100, 5, 1)"), 400.0);
        assert_eq!(double("DDB(1000, 100, 2, 2)"), 0.0);
        assert_eq!(double("PMT(0, 10, 1000)"), -100.0);
        assert_eq!(double("FV(0, 10, -100)"), 1000.0);
        assert_eq!(double("PV(0, 10, -100)"), 1000.0);
        assert_eq!(double("NPER(0, -100, 1000)"), 10.0);
        assert_eq!(double("IPMT(0.05, 1, 10, 10000, 0, 1)"), 0.0);
        assert!((double("PMT(0.05, 10, 10000)") + 1295.0457).abs() < 0.001);
        assert!((double("RATE(10, -100, 1000)")).abs() < 1e-6);
        assert!(double("SLN(1000, 100, 0)").is_infinite());
        // a missing argument is a null conversion error
        assert!(matches!(
            eval_error("FV(Null, 10, -100)", &context),
            ExprError::Eval(_)
        ));
        assert!(matches!(
            eval_error("FV(1, 2)", &context),
            ExprError::Eval(_)
        ));
    }

    #[test]
    fn strconv_semantics() {
        let context = EvalContext::new();
        let text = |expression: &str| match eval_value(expression, &context) {
            ExprValue::Text(text) => text,
            other => panic!("{expression}: expected text, got {other:?}"),
        };
        assert_eq!(text("StrConv(\"Hello World\", 1)"), "HELLO WORLD");
        assert_eq!(text("StrConv(\"Hello World\", 2)"), "hello world");
        assert_eq!(text("StrConv(\"HELLO WORLD\", 3)"), "Hello World");
        assert_eq!(text("StrConv(\"  Mixed Case  \", 3)"), "  Mixed Case  ");
        assert_eq!(text("StrConv(\"don't stop\", 3)"), "Don't Stop");
        assert_eq!(text("StrConv(\"a-b c\", 3)"), "A-b C");
        assert_eq!(text("StrConv(\"abc123def\", 3)"), "Abc123def");
        // vbUnicode (64) is a no-op for Java strings
        assert_eq!(text("StrConv(\"abc\", 64)"), "abc");
        assert_eq!(text("StrConv(\"abc\", 65)"), "ABC");
        assert_eq!(eval_value("StrConv(Null, 1)", &context), ExprValue::Null);
        // unsupported character conversion is an EvalException
        assert!(matches!(
            eval_error("StrConv(\"abc\", 5)", &context),
            ExprError::Eval(_)
        ));
        // ... while a non-US locale id is reported as unsupported
        assert!(matches!(
            eval_error("StrConv(\"abc\", 1, 1031)", &context),
            ExprError::Unsupported(_)
        ));
    }

    // -- arbitrary precision `Format` number path ---------------------------

    /// Java `new BigDecimal(double)`, `BigDecimal.valueOf(double)` and Java's
    /// `Double.toString` digits.
    #[test]
    fn big_decimal_conversion_matches_java() {
        // `new BigDecimal(double)` keeps the exact binary value
        assert_eq!(
            bd_exact_from_f64(0.1).unwrap().to_plain_string(),
            "0.1000000000000000055511151231257827021181583404541015625"
        );
        assert_eq!(bd_exact_from_f64(0.5).unwrap().to_plain_string(), "0.5");
        assert_eq!(bd_exact_from_f64(1.5).unwrap().to_plain_string(), "1.5");
        assert_eq!(bd_exact_from_f64(-1.5).unwrap().to_plain_string(), "-1.5");
        assert_eq!(bd_exact_from_f64(0.0).unwrap().to_plain_string(), "0");
        assert!(bd_exact_from_f64(f64::NAN).is_none());
        assert!(bd_exact_from_f64(f64::INFINITY).is_none());
        // precision/scale of the exact expansions (Java BigDecimal)
        let min_value = bd_exact_from_f64(f64::from_bits(1)).unwrap();
        assert_eq!(
            (min_value.digits(), min_value.fractional_digit_count()),
            (751, 1074)
        );
        let max = bd_exact_from_f64(f64::MAX).unwrap();
        assert_eq!((max.digits(), max.fractional_digit_count()), (309, 0));
        let min_normal = bd_exact_from_f64(f64::MIN_POSITIVE).unwrap();
        assert_eq!(
            (min_normal.digits(), min_normal.fractional_digit_count()),
            (715, 1022)
        );
        let big = bd_exact_from_f64(1.0E300).unwrap();
        assert_eq!((big.digits(), big.fractional_digit_count()), (301, 0));

        // `Double.toString` digits (Schubfach, including its "tiny" and exact
        // tie handling); `BigDecimal.valueOf` reuses them
        for (value, expected) in [
            (f64::from_bits(1), "4.9E-324"),
            (f64::from_bits(2), "9.9E-324"),
            (f64::from_bits(0x10), "7.9E-323"),
            (
                f64::from_bits(0x0070_0000_0000_0000),
                "1.424047269444609E-306",
            ),
            (
                f64::from_bits(0x4312_c164_037c_3969),
                "1.3197962200387462E15",
            ),
            (f64::MAX, "1.7976931348623157E308"),
            (f64::MIN_POSITIVE, "2.2250738585072014E-308"),
            (1.0E300, "1.0E300"),
            (1.0E16, "1.0E16"),
            (1.0E-320, "1.0E-320"),
            (0.1, "0.1"),
            (2.675, "2.675"),
            (100.0, "100.0"),
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (1.0E-4, "1.0E-4"),
            (0.001, "0.001"),
            (123456789012345.0, "1.23456789012345E14"),
            (1.0E7, "1.0E7"),
        ] {
            assert_eq!(
                java_double_string(value).unwrap(),
                expected,
                "bits {:016x}",
                value.to_bits()
            );
        }
        assert!(java_double_string(f64::NAN).is_none());
        assert!(java_double_string(f64::INFINITY).is_none());
        // `BigDecimal.valueOf` keeps Java's coefficient/scale (which the
        // custom zero-subpattern fallback compares against)
        let tiny = bd_value_of(f64::from_bits(1)).unwrap();
        assert_eq!(tiny.fractional_digit_count(), 325);
        assert_eq!(tiny.to_plain_string(), format!("0.{}49", "0".repeat(323)));
        assert_eq!(bd_value_of(100.0).unwrap().to_plain_string(), "100.0");
        assert!(bd_value_of(f64::NAN).is_none());
    }

    #[test]
    fn arbitrary_precision_decimal_parsing() {
        assert_eq!(
            parse_big_decimal("1234567890123456789012345678901234567890")
                .unwrap()
                .to_plain_string(),
            "1234567890123456789012345678901234567890"
        );
        assert_eq!(parse_big_decimal("&hFF").unwrap().to_plain_string(), "255");
        assert_eq!(parse_big_decimal("&o777").unwrap().to_plain_string(), "511");
        // `ValueSupport.normalize`: no trailing zeros, no negative scale
        assert_eq!(
            parse_big_decimal("-1,234.500").unwrap().to_plain_string(),
            "-1234.5"
        );
        assert_eq!(
            parse_big_decimal("1E+400").unwrap().to_plain_string().len(),
            401
        );
        assert_eq!(
            parse_big_decimal("1e-400")
                .unwrap()
                .fractional_digit_count(),
            400
        );
        assert!(parse_big_decimal("abc").is_err());
        assert!(parse_big_decimal("").is_err());
        assert!(parse_big_decimal("&h").is_err());
        // > 38 digit coefficients round to 28 significant digits for the
        // "General Number" predef (`NumberFormatter.DEC_SIG_DIGITS`)
        let big = parse_big_decimal("1234567890123456789012345678901234567890").unwrap();
        assert_eq!(
            format_type_formatter(&bd_round_sig(&big, DEC_SIG_DIGITS), DEC_SIG_DIGITS).unwrap(),
            "1.234567890123456789012345679E+39"
        );
    }

    #[test]
    fn extreme_number_formatting_matches_java() {
        let format = |fmt: &str, value: ExprValue| -> String {
            match format_value(fmt, &value, 0, 0) {
                Ok(ExprValue::Text(text)) => text,
                other => panic!("{fmt} {value:?}: {other:?}"),
            }
        };
        let double = |value: f64| ExprValue::Double(value);
        let text = |value: &str| ExprValue::Text(value.to_string());
        // `NumberFormatter.format(double)`: the exact value to 15 digits
        assert_eq!(
            format("General Number", double(f64::MAX)),
            "1.79769313486232E+308"
        );
        assert_eq!(
            format("General Number", double(f64::from_bits(1))),
            "4.94065645841247E-324"
        );
        assert_eq!(
            format("General Number", double(f64::from_bits(2))),
            "9.88131291682493E-324"
        );
        assert_eq!(
            format("General Number", double(f64::MIN_POSITIVE)),
            "2.2250738585072E-308"
        );
        assert_eq!(format("General Number", double(1.0E300)), "1E+300");
        assert_eq!(format("General Number", double(1.0E-300)), "1E-300");
        assert_eq!(format("General Number", double(1.0E16)), "1E+16");
        assert_eq!(
            format("General Number", double(1.0E-320)),
            "9.99988867182683E-321"
        );
        assert_eq!(
            format("General Number", double(1.2345678901234567E300)),
            "1.23456789012346E+300"
        );
        assert_eq!(format("General Number", double(0.000123)), "0.000123");
        assert_eq!(format("General Number", double(100.0)), "100");
        // patterns use `BigDecimal.valueOf` (Java's `Double.toString` digits)
        assert_eq!(format("Scientific", double(f64::from_bits(1))), "4.90E-324");
        assert_eq!(format("Scientific", double(f64::from_bits(2))), "9.90E-324");
        assert_eq!(format("Scientific", double(f64::MAX)), "1.80E+308");
        assert_eq!(format("Scientific", double(1.0E-320)), "1.00E-320");
        assert_eq!(
            format("Standard", double(1.0E300)),
            format!("1{}.00", ",000".repeat(100))
        );
        assert_eq!(
            format("Fixed", double(1.0E300)),
            format!("1{}.00", "0".repeat(300))
        );
        assert_eq!(format("Fixed", double(1.0E-300)), "0.00");
        assert_eq!(format("Percent", double(1.0E-300)), "0.00%");
        assert_eq!(format("Currency", double(1.0E-300)), "$0.00");
        // arbitrary precision text coercion (> 38 digit coefficients)
        let digits = "1234567890123456789012345678901234567890";
        assert_eq!(
            format("Fixed", text(digits)),
            "1234567890123456789012345678901234567890.00"
        );
        assert_eq!(
            format("Percent", text(digits)),
            "123456789012345678901234567890123456789000.00%"
        );
        assert_eq!(
            format(
                "Currency",
                text("-1234567890123456789012345678901234567890")
            ),
            "($1,234,567,890,123,456,789,012,345,678,901,234,567,890.00)"
        );
        assert_eq!(
            format(
                "0.00",
                text("0.123456789012345678901234567890123456789012345")
            ),
            "0.12"
        );
        assert_eq!(format("Scientific", text("1E+400")), "1.00E+400");
        assert_eq!(
            format("General Number", text(digits)),
            "1.234567890123456789012345679E+39"
        );
        assert_eq!(
            format(
                "General Number",
                text("0.123456789012345678901234567890123456789012345")
            ),
            "0.1234567890123456789012345679"
        );
        // a value whose rounding produces zero falls back to the zero format
        assert_eq!(
            format("0.00;\"neg\"0.00;\"zero\"", double(f64::from_bits(1))),
            "zero"
        );
        assert_eq!(
            format(
                "0.00E+00;\"neg\"0.00E+00;\"zero\"",
                double(f64::from_bits(1))
            ),
            "4.90E-324"
        );
        // HALF_EVEN ties are unchanged
        assert_eq!(format("0.00", double(2.675)), "2.68");
        assert_eq!(format("0.00", double(1.005)), "1.00");
        assert_eq!(format("0.00", double(0.125)), "0.12");
        // non-finite values keep Java's special strings
        assert_eq!(format("General Number", double(f64::NAN)), "1.#QNAN");
        assert_eq!(format("General Number", double(f64::INFINITY)), "1.#INF");
        assert_eq!(
            format("General Number", double(f64::NEG_INFINITY)),
            "-1.#INF"
        );
        assert_eq!(format("@", double(f64::NAN)), "1.#QNAN");
        assert_eq!(format("@", double(f64::INFINITY)), "1.#INF");
    }
}

#[cfg(test)]
mod eval_config_tests {
    use super::*;

    /// `DBEvalContext`'s initial state (the Java `EvalConfig` defaults).
    #[test]
    fn eval_config_defaults_match_java() {
        let config = EvalConfig::default();
        assert_eq!(config.locale(), "en_US");
        assert_eq!(
            config.temporal_config().date_format(),
            TemporalConfig::US_DATE_FORMAT
        );
        assert_eq!(
            config.temporal_config().long_date_format(),
            TemporalConfig::US_LONG_DATE_FORMAT
        );
        assert_eq!(
            config.temporal_config().time_format_12(),
            TemporalConfig::US_TIME_FORMAT_12_FORMAT
        );
        assert_eq!(
            config.temporal_config().time_format_24(),
            TemporalConfig::US_TIME_FORMAT_24_FORMAT
        );
        assert_eq!(config.temporal_config().date_separator(), '/');
        assert_eq!(config.temporal_config().time_separator(), ':');
        assert_eq!(
            config.temporal_config().date_time_format_12(),
            "M/d[/uuuu] h:mm:ss a"
        );
        assert_eq!(
            config.temporal_config().date_time_format_24(),
            "M/d[/uuuu] H:mm:ss"
        );
        assert_eq!(config.temporal_config().am_pm_strings(), ["AM", "PM"]);
        assert_eq!(config.numeric_config().num_decimal_digits(), 2);
        assert!(config.numeric_config().include_leading_digit());
        assert!(!config.numeric_config().use_parens_for_negatives());
        assert!(config.numeric_config().use_parens_for_currency_negatives());
        assert_eq!(config.numeric_config().num_grouping_digits(), 3);
        assert!(config.bindings().is_empty());
        assert!(config.zone().is_none());
        assert!(config.function_lookup().get_function("Abs").is_some());
        assert!(config
            .function_lookup()
            .get_function("NoSuchFunction")
            .is_none());
    }

    /// `TemporalConfig.Type`'s helpers.
    #[test]
    fn temporal_type_helpers_match_java() {
        assert_eq!(TemporalType::LongDate.default_type(), TemporalType::Date);
        assert_eq!(TemporalType::LongTime.default_type(), TemporalType::Time);
        assert_eq!(
            TemporalType::GeneralDate.default_type(),
            TemporalType::DateTime
        );
        assert_eq!(TemporalType::Date.value_type(), ValueKind::Date);
        assert_eq!(TemporalType::Time24.value_type(), ValueKind::Time);
        assert_eq!(TemporalType::DateTime12.value_type(), ValueKind::DateTime);
        assert!(TemporalType::ShortDate.is_date_only());
        assert!(TemporalType::MediumTime.is_time_only());
        assert!(TemporalType::GeneralDate.includes_date());
        assert!(TemporalType::GeneralDate.includes_time());
        let config = TemporalConfig::us();
        assert_eq!(
            config.get_date_time_format(TemporalType::LongDate),
            TemporalConfig::US_LONG_DATE_FORMAT
        );
        assert_eq!(
            config.get_date_time_format(TemporalType::MediumDate),
            "dd-MMM-uu"
        );
        assert_eq!(
            config.get_date_time_format(TemporalType::MediumTime),
            "hh:mm a"
        );
        assert_eq!(
            config.get_date_time_format(TemporalType::ShortTime),
            "HH:mm"
        );
        assert_eq!(
            config.get_date_time_format(TemporalType::Time24),
            TemporalConfig::US_TIME_FORMAT_24_FORMAT
        );
    }

    /// `NumericConfig.getNumberFormat(Type)`'s
    /// `FormatUtil.createNumberFormatPattern` patterns.
    #[test]
    fn numeric_config_patterns_match_java() {
        let config = NumericConfig::us();
        assert_eq!(
            config.get_number_format(NumericType::Currency),
            "¤#,##0.00;(¤#,##0.00)"
        );
        assert_eq!(
            config.get_number_format(NumericType::Euro),
            "€#,##0.00;(€#,##0.00)"
        );
        assert_eq!(config.get_number_format(NumericType::Fixed), "0.00");
        assert_eq!(config.get_number_format(NumericType::Standard), "#,##0.00");
        assert_eq!(config.get_number_format(NumericType::Percent), "0.00%");
        assert_eq!(config.get_number_format(NumericType::Scientific), "0.00E0");
        let custom = NumericConfig::new(3, false, true, false, 0, "de_DE");
        assert_eq!(
            custom.get_number_format(NumericType::Standard),
            "#.000;(#.000)"
        );
        assert_eq!(custom.get_number_format(NumericType::Currency), "¤#.000");
    }

    /// The default lookup's function list (`DefaultFunctions.LOOKUP`).
    #[test]
    fn default_function_lookup_matches_java() {
        assert_eq!(DefaultFunctionLookup::function_names().len(), 115);
        for name in [
            "Abs", "cdate", "Format", "Format$", "GenGUID", "IIf", "NPer", "Val$",
        ] {
            assert!(
                DefaultFunctionLookup::is_known(name),
                "{name} should be known"
            );
            let info = DefaultFunctionLookup::instance()
                .get_function(name)
                .unwrap();
            assert!(info.is_default());
            assert_eq!(info.name().to_uppercase(), name.to_uppercase());
        }
        for name in ["DLookup", "IRR", "NPV", "Partition", "Bogus", ""] {
            assert!(!DefaultFunctionLookup::is_known(name), "{name}");
        }
    }

    /// The config setters/builders and the bindings container.
    #[test]
    fn eval_config_setters_and_bindings() {
        let mut bindings = Bindings::new();
        assert_eq!(bindings.put("x", ExprValue::Long(1)), None);
        assert_eq!(bindings.get("x"), Some(&ExprValue::Long(1)));
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings.remove("x"), Some(ExprValue::Long(1)));
        assert!(bindings.is_empty());

        let temporal = TemporalConfig::new(
            "dd/MM/uuuu",
            "dddd, dd MMMM uuuu",
            "hh:mm",
            "HH:mm",
            '/',
            ':',
            "en_GB",
        );
        let numeric = NumericConfig::new(3, false, true, false, 0, "en_GB");
        let mut config = EvalConfig::new()
            .with_locale("en_GB")
            .with_temporal_config(temporal.clone())
            .with_numeric_config(numeric.clone())
            .with_bindings(bindings)
            .with_zone(Some(crate::datenums::TimeZone::from_offset_minutes(-300)));
        assert_eq!(config.locale(), "en_GB");
        assert_eq!(config.temporal_config(), &temporal);
        assert_eq!(config.numeric_config(), &numeric);
        assert!(config.bindings().is_empty());
        assert_eq!(config.zone().unwrap().raw_offset_ms, -5 * 60 * 60 * 1000);
        config.bindings_mut().put("y", ExprValue::Long(2));
        assert!(config.function_lookup().get_function("Abs").is_some());
    }
}
