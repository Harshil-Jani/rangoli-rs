//! Exact decimals for money and quantities, with the scale in the type: `price: Decimal<2>`.
//! Stored as an integer count of the smallest unit (`BIGINT`; cents for `Decimal<2>`), so sums
//! never drift like floats, and SQL compares and sorts them exactly on every database.
//! ponytail: i64 units, so `Decimal<2>` holds up to ±92,233,720,368,547,758.07; a wider
//! backing integer (i128 or text) only if that ever runs out.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::ops::{Add, Neg, Sub};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Decimal<const P: u32>(i64);

impl<const P: u32> Decimal<P> {
    const SCALE_OK: () = assert!(P <= 18, "Decimal<P> supports at most 18 decimal places");

    /// From a count of the smallest unit: `Decimal::<2>::from_units(1999)` is 19.99.
    pub const fn from_units(units: i64) -> Self {
        let () = Self::SCALE_OK;
        Decimal(units)
    }

    pub const fn units(self) -> i64 {
        self.0
    }

    /// `"19.99"`, `"-0.5"`, `"7"`; `None` for more than `P` places, exponents, or overflow.
    pub fn parse(s: &str) -> Option<Self> {
        let () = Self::SCALE_OK;
        parse_units(s, P).map(Decimal)
    }

    pub fn checked_add(self, o: Self) -> Option<Self> {
        self.0.checked_add(o.0).map(Decimal)
    }

    pub fn checked_sub(self, o: Self) -> Option<Self> {
        self.0.checked_sub(o.0).map(Decimal)
    }

    /// Multiply by a whole number, e.g. a unit price by a quantity.
    pub fn checked_mul_int(self, n: i64) -> Option<Self> {
        self.0.checked_mul(n).map(Decimal)
    }
}

/// Integer arithmetic semantics: overflow panics in debug builds.
impl<const P: u32> Add for Decimal<P> {
    type Output = Self;
    fn add(self, o: Self) -> Self {
        Decimal(self.0 + o.0)
    }
}
impl<const P: u32> Sub for Decimal<P> {
    type Output = Self;
    fn sub(self, o: Self) -> Self {
        Decimal(self.0 - o.0)
    }
}
impl<const P: u32> Neg for Decimal<P> {
    type Output = Self;
    fn neg(self) -> Self {
        Decimal(-self.0)
    }
}
impl<const P: u32> std::iter::Sum for Decimal<P> {
    fn sum<I: Iterator<Item = Self>>(it: I) -> Self {
        it.fold(Decimal(0), Add::add)
    }
}

/// Always exactly `P` places, like Django: `19.90`.
impl<const P: u32> fmt::Display for Decimal<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&format_units(self.0, P))
    }
}

impl<const P: u32> std::str::FromStr for Decimal<P> {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        Self::parse(s).ok_or_else(|| format!("`{s}` is not a number with at most {P} decimal places"))
    }
}

/// A string, as DRF does by default: JSON numbers are floats in most clients.
impl<const P: u32> Serialize for Decimal<P> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

/// Accepts `"19.99"` or a whole number like `20`. A fractional JSON number is refused: it is a
/// float by the time it is read, so `19.999999999999999` would already be `20`.
impl<'de, const P: u32> Deserialize<'de> for Decimal<P> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let j = serde_json::Value::deserialize(d)?;
        let s = match &j {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) if !n.is_f64() => n.to_string(),
            _ => {
                return Err(serde::de::Error::custom(
                    "expected a decimal string, or a whole number (fractions as strings: floats lose digits)",
                ))
            }
        };
        s.parse().map_err(serde::de::Error::custom)
    }
}

pub(crate) fn parse_units(s: &str, places: u32) -> Option<i64> {
    let s = s.trim();
    let (neg, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
    let digits = |p: &str| p.bytes().all(|b| b.is_ascii_digit());
    if whole.len() + frac.len() == 0 || !digits(whole) || !digits(frac) || frac.len() > places as usize {
        return None;
    }
    let pad = std::iter::repeat_n(b'0', places as usize - frac.len());
    // Accumulate towards the sign, so i64::MIN (which Display can produce) parses back.
    let sign = if neg { -1 } else { 1 };
    let mut units: i64 = 0;
    for b in whole.bytes().chain(frac.bytes()).chain(pad) {
        units = units.checked_mul(10)?.checked_add(sign * i64::from(b - b'0'))?;
    }
    Some(units)
}

pub(crate) fn format_units(units: i64, places: u32) -> String {
    let p = places as usize;
    let digits = format!("{:0>width$}", units.unsigned_abs(), width = p + 1);
    let (whole, frac) = digits.split_at(digits.len() - p);
    let sign = if units < 0 { "-" } else { "" };
    if p == 0 {
        format!("{sign}{whole}")
    } else {
        format!("{sign}{whole}.{frac}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    type Money = Decimal<2>;

    #[test]
    fn exact_where_floats_drift() {
        let dime = Money::parse("0.10").unwrap();
        let total: Money = std::iter::repeat_n(dime, 10).sum();
        assert_eq!(total.to_string(), "1.00");
        assert_ne!((0..10).map(|_| 0.1_f64).sum::<f64>(), 1.0, "the float version drifts");
    }

    #[test]
    fn parses_and_formats() {
        for (s, units, shown) in [
            ("19.99", 1999, "19.99"),
            ("19.9", 1990, "19.90"),
            ("7", 700, "7.00"),
            ("-0.5", -50, "-0.50"),
            (".5", 50, "0.50"),
            ("+3.", 300, "3.00"),
            ("0", 0, "0.00"),
            ("-0", 0, "0.00"),
            ("000.01", 1, "0.01"),
        ] {
            let d = Money::parse(s).unwrap_or_else(|| panic!("{s}"));
            assert_eq!((d.units(), d.to_string().as_str()), (units, shown), "{s}");
        }
        for bad in ["", ".", "-", "1.999", "1e3", "1,5", "abc", "1.2.3", " 1 2", "NaN", "92233720368547758.08", "--1"] {
            assert_eq!(Money::parse(bad), None, "{bad}");
        }
        assert_eq!(Money::parse("92233720368547758.07").unwrap().units(), i64::MAX);
        let min = Money::from_units(i64::MIN);
        assert_eq!(Money::parse(&min.to_string()), Some(min), "the smallest value round-trips");
        assert!(serde_json::from_str::<Money>("19.999999999999999").is_err(), "fractional JSON numbers are floats: refused");
        assert_eq!(serde_json::from_str::<Money>("20").unwrap().to_string(), "20.00");
        assert_eq!(Decimal::<0>::parse("12").unwrap().to_string(), "12");
        assert!(Money::parse("2.50").unwrap() > Money::parse("2.4").unwrap());
        let j: Money = serde_json::from_str("\"12.5\"").unwrap();
        assert_eq!(serde_json::to_string(&j).unwrap(), "\"12.50\"");
    }
}
