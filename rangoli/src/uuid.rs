//! UUIDs (RFC 9562), stored as canonical lowercase text (`VARCHAR(36)`) so they read the same in
//! every database and in logs. `#[field(auto)]` fills a v7 on insert: v7 starts with the
//! millisecond timestamp, so new keys sort after old ones and B-tree inserts stay at the
//! right edge of the index instead of landing on random pages like v4.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Uuid([u8; 16]);

impl Uuid {
    pub const NIL: Uuid = Uuid([0; 16]);

    pub const fn from_bytes(b: [u8; 16]) -> Self {
        Uuid(b)
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Random (version 4).
    pub fn new_v4() -> Self {
        let mut b = random();
        b[6] = (b[6] & 0x0f) | 0x40;
        b[8] = (b[8] & 0x3f) | 0x80;
        Uuid(b)
    }

    /// Time-ordered (version 7): 48-bit Unix milliseconds, then random bits.
    pub fn new_v7() -> Self {
        let ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let mut b = random();
        b[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
        b[6] = (b[6] & 0x0f) | 0x70;
        b[8] = (b[8] & 0x3f) | 0x80;
        Uuid(b)
    }

    pub fn version(&self) -> u8 {
        self.0[6] >> 4
    }

    /// The hyphenated form, any case (`67e55044-10b1-426f-9247-bb680e5fe0c8`).
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().as_bytes();
        let dash = |i: usize| matches!(i, 8 | 13 | 18 | 23);
        // Dashes exactly at 8, 13, 18 and 23 and hex digits everywhere else: 32 hex digits.
        if s.len() != 36 || s.iter().enumerate().any(|(i, &c)| if dash(i) { c != b'-' } else { !c.is_ascii_hexdigit() }) {
            return None;
        }
        let hex: Vec<u8> = s.iter().copied().filter(|&c| c != b'-').collect();
        let mut b = [0u8; 16];
        for (i, pair) in hex.chunks(2).enumerate() {
            b[i] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
        }
        // from_str_radix accepts a leading '+'; a canonical UUID has only hex digits.
        hex.iter().all(u8::is_ascii_hexdigit).then_some(Uuid(b))
    }
}

fn random() -> [u8; 16] {
    let mut b = [0u8; 16];
    // ponytail: the OS RNG failing is unrecoverable for a process that issues ids and sessions.
    getrandom::fill(&mut b).expect("OS random number generator unavailable");
    b
}

impl fmt::Display for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, byte) in self.0.iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                f.write_str("-")?;
            }
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl std::str::FromStr for Uuid {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        Uuid::parse(s).ok_or_else(|| format!("`{s}` is not a valid UUID"))
    }
}

impl Serialize for Uuid {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Uuid {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_rejects() {
        let s = "67e55044-10b1-426f-9247-bb680e5fe0c8";
        let u = Uuid::parse(s).unwrap();
        assert_eq!((u.to_string(), u.version()), (s.to_string(), 4));
        assert_eq!(Uuid::parse(&s.to_uppercase()), Some(u));
        assert_eq!(Uuid::NIL.to_string(), "00000000-0000-0000-0000-000000000000");
        for bad in [
            "00000000-0000-0000-0000-00000000000-",
            "67e55044-10b1-426f-9247-bb680e5f-e0c",
            "",
            "67e55044-10b1-426f-9247-bb680e5fe0c",
            "67e5504410b1426f9247bb680e5fe0c8xxxx",
            "67e55044+10b1-426f-9247-bb680e5fe0c8",
            "g7e55044-10b1-426f-9247-bb680e5fe0c8",
            "+7e55044-10b1-426f-9247-bb680e5fe0c8",
        ] {
            assert_eq!(Uuid::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn versions_and_order() {
        let a = Uuid::new_v4();
        assert_eq!((a.version(), a.0[8] >> 6), (4, 0b10), "version and RFC variant bits");
        assert_ne!(a, Uuid::new_v4());
        let mut prev = Uuid::new_v7();
        assert_eq!(prev.version(), 7);
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(1));
            let next = Uuid::new_v7();
            assert!(next.to_string() > prev.to_string(), "v7 text sorts in creation order");
            prev = next;
        }
    }
}
