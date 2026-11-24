//! The value model, mirroring the Java objects jackcess stores in rows.

use crate::datenums::LocalDateTime;

#[derive(Clone, Debug)]
pub enum Value {
    Null,
    Bool(bool),
    /// java.lang.Byte
    Byte(u8),
    /// java.lang.Short
    Int(i16),
    /// java.lang.Integer
    Long(i32),
    /// java.lang.Long
    BigInt(i64),
    /// java.math.BigDecimal used by MONEY/NUMERIC (unscaled value + scale)
    Decimal {
        unscaled: i128,
        scale: i32,
    },
    /// java.lang.Float
    Float(f32),
    /// java.lang.Double
    Double(f64),
    /// java.lang.String
    Text(String),
    /// byte[]
    Bytes(Vec<u8>),
    /// java.time.LocalDateTime
    DateTime(LocalDateTime),
    /// a date value read from the database, kept as its original bits
    DateBits(f64),
    /// java.util.UUID
    Uuid([u8; 16]),
    /// `Column.KEEP_VALUE`: in a partial row update the column keeps the value
    /// (and, for a variable length column, the raw on-page bytes) it already
    /// holds.  It is never a real column value: writing it outside the
    /// `update_row`/`update_value`/`update_row_from_map` paths is an error.
    Keep,
}

/// The `java.util.Date.getTime()` an Access date double represents, up to the
/// fixed offset of the zone: [`crate::datenums::from_local_date_double`] rounds
/// the fraction of the day to a whole millisecond and the zone offset is the
/// same for both values (the port's zones have a fixed offset), so comparing
/// this value is equivalent to comparing `Date.getTime()`.
fn date_bits_time(bits: f64) -> i64 {
    crate::datenums::from_local_date_double(bits)
}

impl PartialEq for Value {
    /// The equality of the Java objects the values stand for.  In particular a
    /// [`Value::DateBits`] is jackcess' `ColumnImpl.DateExt` (a
    /// `java.util.Date` which also keeps the stored date bits), and
    /// `java.util.Date.equals` compares `getTime()`, not the raw bits: two date
    /// doubles which differ below the millisecond are equal when they denote
    /// the same instant.  The stored bits are still kept (they are what a
    /// rewrite has to store) and [`Value::DateBits`] never equals a
    /// [`Value::DateTime`], exactly like `java.util.Date` never equals a
    /// `java.time.LocalDateTime`.
    fn eq(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Byte(a), Value::Byte(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Long(a), Value::Long(b)) => a == b,
            (Value::BigInt(a), Value::BigInt(b)) => a == b,
            (
                Value::Decimal {
                    unscaled: a,
                    scale: a_scale,
                },
                Value::Decimal {
                    unscaled: b,
                    scale: b_scale,
                },
            ) => a == b && a_scale == b_scale,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Double(a), Value::Double(b)) => a == b,
            (Value::Text(a), Value::Text(b)) => a == b,
            (Value::Bytes(a), Value::Bytes(b)) => a == b,
            (Value::DateTime(a), Value::DateTime(b)) => a == b,
            (Value::DateBits(a), Value::DateBits(b)) => date_bits_time(*a) == date_bits_time(*b),
            (Value::Uuid(a), Value::Uuid(b)) => a == b,
            (Value::Keep, Value::Keep) => true,
            _ => false,
        }
    }
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// `true` for [`Value::Keep`] (`Column.KEEP_VALUE`).
    pub fn is_keep(&self) -> bool {
        matches!(self, Value::Keep)
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Bool(b) => Some(if *b { 1 } else { 0 }),
            Value::Byte(v) => Some(*v as i64),
            Value::Int(v) => Some(*v as i64),
            Value::Long(v) => Some(*v as i64),
            Value::BigInt(v) => Some(*v),
            Value::Float(v) => Some(*v as i64),
            Value::Double(v) => Some(*v as i64),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            Value::Byte(v) => Some(*v as f64),
            Value::Int(v) => Some(*v as f64),
            Value::Long(v) => Some(*v as f64),
            Value::BigInt(v) => Some(*v as f64),
            Value::Float(v) => Some(*v as f64),
            Value::Double(v) => Some(*v),
            Value::Decimal { unscaled, scale } => Some(*unscaled as f64 / 10f64.powi(*scale)),
            _ => None,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datenums::to_local_date_double;

    #[test]
    fn date_bits_equality_compares_the_java_date_time() {
        // two doubles which differ below the millisecond but denote the same
        // `java.util.Date.getTime()` (a `DateExt` keeps whatever bits it read)
        let a = to_local_date_double(1_584_246_645_123);
        // the next representable double is still the same whole millisecond
        let b = f64::from_bits(a.to_bits() + 1);
        assert_ne!(a.to_bits(), b.to_bits());
        assert_eq!(Value::DateBits(a), Value::DateBits(b));
        // a whole millisecond apart is a different Date
        let c = to_local_date_double(1_584_246_645_124);
        assert_ne!(Value::DateBits(a), Value::DateBits(c));
        // DateExt is not a LocalDateTime
        assert_ne!(
            Value::DateBits(a),
            Value::DateTime(LocalDateTime::new(2020, 3, 15, 12, 30, 45, 123_000_000))
        );
        // the stored bits survive: a clone still has the exact double
        match (Value::DateBits(a).clone(), a) {
            (Value::DateBits(bits), expected) => assert_eq!(bits.to_bits(), expected.to_bits()),
            _ => unreachable!(),
        }
    }

    #[test]
    fn non_date_values_keep_structural_equality() {
        assert_eq!(Value::Null, Value::Null);
        assert_eq!(Value::Text("a".into()), Value::Text("a".into()));
        assert_ne!(Value::Text("a".into()), Value::Text("b".into()));
        assert_eq!(Value::Long(1), Value::Long(1));
        // different kinds are never equal (derived discrimination)
        assert_ne!(Value::Long(1), Value::BigInt(1));
        assert_ne!(Value::Int(1), Value::Long(1));
        assert_eq!(
            Value::Decimal {
                unscaled: 1234,
                scale: 2
            },
            Value::Decimal {
                unscaled: 1234,
                scale: 2
            }
        );
        assert_ne!(Value::Double(f64::NAN), Value::Double(f64::NAN));
    }
}
