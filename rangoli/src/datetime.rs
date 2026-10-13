//! UTC timestamps, stored as Unix seconds (`BIGINT`) on every database, so the
//! same value round-trips through Postgres, MySQL and SQLite without timezone
//! surprises and still sorts and compares in SQL.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DateTime(i64);

impl DateTime {
    pub fn now() -> Self {
        DateTime(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64))
    }

    pub const fn from_unix(secs: i64) -> Self {
        DateTime(secs)
    }

    pub fn unix(self) -> i64 {
        self.0
    }

    /// `(year, month, day, hour, minute, second)` in UTC.
    pub fn parts(self) -> (i64, u32, u32, u32, u32, u32) {
        let (days, rem) = (self.0.div_euclid(86_400), self.0.rem_euclid(86_400) as u32);
        let (y, m, d) = civil_from_days(days);
        (y, m, d, rem / 3600, rem % 3600 / 60, rem % 60)
    }

    pub fn from_parts(y: i64, m: u32, d: u32, hh: u32, mm: u32, ss: u32) -> Option<Self> {
        let valid = (1..=9999).contains(&y) && (1..=12).contains(&m) && d >= 1 && d <= days_in_month(y, m) && hh < 24 && mm < 60 && ss < 60;
        valid.then(|| DateTime(days_from_civil(y, m, d) * 86_400 + i64::from(hh * 3600 + mm * 60 + ss)))
    }

    /// Accepts `YYYY-MM-DD`, `YYYY-MM-DDTHH:MM`, `YYYY-MM-DD HH:MM:SS`, with an optional trailing `Z`.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().trim_end_matches('Z');
        let (date, time) = s.split_once(['T', ' ']).unwrap_or((s, "00:00"));
        let mut d = date.splitn(3, '-').map(str::parse::<i64>);
        let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
        let mut t = time.splitn(3, ':').map(str::parse::<u32>);
        let (hh, mm) = (t.next()?.ok()?, t.next()?.ok()?);
        let ss = t.next().transpose().ok()?.unwrap_or(0);
        Self::from_parts(y, u32::try_from(m).ok()?, u32::try_from(day).ok()?, hh, mm, ss)
    }

    pub fn start_of_day(self) -> Self {
        DateTime(self.0 - self.0.rem_euclid(86_400))
    }

    pub fn start_of_month(self) -> Self {
        let (y, m, ..) = self.parts();
        Self::from_parts(y, m, 1, 0, 0, 0).unwrap()
    }

    pub fn start_of_year(self) -> Self {
        Self::from_parts(self.parts().0, 1, 1, 0, 0, 0).unwrap()
    }

    /// The value an `<input type="datetime-local">` expects.
    pub fn input_value(self) -> String {
        let (y, m, d, hh, mm, _) = self.parts();
        format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}")
    }

    /// `2026-10-07 02:15 UTC`, for people.
    pub fn human(self) -> String {
        let (y, m, d, hh, mm, _) = self.parts();
        format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02} UTC")
    }
}

/// ISO 8601: `2026-10-07T02:15:00Z`.
impl fmt::Display for DateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (y, m, d, hh, mm, ss) = self.parts();
        write!(f, "{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
    }
}

impl Serialize for DateTime {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for DateTime {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        DateTime::parse(&s).ok_or_else(|| serde::de::Error::custom(format!("invalid datetime `{s}`")))
    }
}

/// A calendar date with no time or timezone, stored as days since 1970-01-01 (`BIGINT`),
/// so it sorts and compares in SQL and round-trips identically on every database.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date(i64);

impl Date {
    /// Today in UTC.
    pub fn today() -> Self {
        Date(DateTime::now().unix().div_euclid(86_400))
    }

    pub const fn from_days(days: i64) -> Self {
        Date(days)
    }

    /// Days since 1970-01-01.
    pub fn days(self) -> i64 {
        self.0
    }

    pub fn from_ymd(y: i64, m: u32, d: u32) -> Option<Self> {
        // Years 1 to 9999, like ISO 8601 and Python's datetime; huge years overflowed the day count.
        ((1..=9999).contains(&y) && (1..=12).contains(&m) && d >= 1 && d <= days_in_month(y, m)).then(|| Date(days_from_civil(y, m, d)))
    }

    pub fn ymd(self) -> (i64, u32, u32) {
        civil_from_days(self.0)
    }

    /// Exactly `YYYY-MM-DD`.
    pub fn parse(s: &str) -> Option<Self> {
        let mut d = s.trim().splitn(3, '-').map(str::parse::<i64>);
        let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
        Self::from_ymd(y, u32::try_from(m).ok()?, u32::try_from(day).ok()?)
    }
}

/// ISO 8601: `2026-10-07`.
impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (y, m, d) = self.ymd();
        write!(f, "{y:04}-{m:02}-{d:02}")
    }
}

impl Serialize for Date {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Date {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Date::parse(&s).ok_or_else(|| serde::de::Error::custom(format!("invalid date `{s}`")))
    }
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Howard Hinnant's civil-from-days.
pub(crate) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Howard Hinnant's days-from-civil.
pub(crate) fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let t = DateTime::from_unix(1_700_000_000);
        assert_eq!(t.to_string(), "2023-11-14T22:13:20Z");
        assert_eq!(DateTime::parse("2023-11-14T22:13:20Z"), Some(t));
        assert_eq!(DateTime::parse(&t.input_value()), Some(DateTime::from_unix(1_700_000_000 - 20)));
        assert_eq!(DateTime::parse("2000-02-29"), Some(DateTime::from_unix(951_782_400)));
        assert_eq!(DateTime::parse("1969-12-31 23:59:59"), Some(DateTime::from_unix(-1)));
        assert_eq!(t.human(), "2023-11-14 22:13 UTC");
        for bad in ["2023-02-29", "2023-13-01", "2023-11-14T24:00", "yesterday", "2023-11", "99999999999999999-01-01T00:00"] {
            assert_eq!(DateTime::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn dates() {
        let d = Date::parse("2024-02-29").unwrap();
        assert_eq!((d.to_string(), d.ymd()), ("2024-02-29".into(), (2024, 2, 29)));
        assert_eq!(Date::parse("1970-01-01"), Some(Date::from_days(0)));
        assert_eq!(Date::parse("1969-12-31"), Some(Date::from_days(-1)));
        assert!(Date::parse("2024-12-31").unwrap() > d, "orders like the calendar");
        for bad in
            ["2023-02-29", "2023-00-10", "2023-11-14T10:00", "2023-11", "", "x-y-z", "99999999999999999-01-01", "10000-01-01", "0000-01-01"]
        {
            assert_eq!(Date::parse(bad), None, "{bad}");
        }
        assert_eq!(serde_json::to_string(&d).unwrap(), "\"2024-02-29\"");
    }

    #[test]
    fn calendar_boundaries() {
        let t = DateTime::parse("2026-10-07T13:45:10").unwrap();
        assert_eq!(t.start_of_day().to_string(), "2026-10-07T00:00:00Z");
        assert_eq!(t.start_of_month().to_string(), "2026-10-01T00:00:00Z");
        assert_eq!(t.start_of_year().to_string(), "2026-01-01T00:00:00Z");
        for days in [-800_000, -1, 0, 59, 60, 11_016, 2_932_896] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
    }
}
