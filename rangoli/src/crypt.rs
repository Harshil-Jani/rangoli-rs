//! Encrypted fields: `#[field(encrypted)] tax_id: String` is stored as AES-256-GCM ciphertext
//! (a fresh random nonce for every write) and decrypted when read, so a database dump or a
//! backup shows nothing. Code, the admin, the API and MCP see plain values.
//!
//! The key is `RANGOLI_ENCRYPTION_KEY`: 32 bytes as 64 hex characters (`openssl rand -hex 32`).
//! To rotate, move the old key to `RANGOLI_ENCRYPTION_OLD_KEYS` (comma-separated) and set a new
//! one: old values still decrypt, and each is re-encrypted with the new key the next time its
//! row is saved. Encrypted columns can't be searched or filtered: the stored bytes differ on
//! every write by design.

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use sha2::Digest;
use std::sync::OnceLock;

const PREFIX: &str = "enc:v1:";

struct Key {
    id: String,
    key: LessSafeKey,
}

fn parse_key(hex: &str) -> Option<Key> {
    let hex = hex.trim();
    if hex.len() != 64 {
        return None;
    }
    let bytes: Vec<u8> = (0..32).map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16)).collect::<Result<_, _>>().ok()?;
    let id = sha2::Sha256::digest(&bytes).iter().take(4).map(|b| format!("{b:02x}")).collect();
    Some(Key { id, key: LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &bytes).ok()?) })
}

/// The current key first, then old ones (for reading only).
fn keys() -> &'static [Key] {
    static KEYS: OnceLock<Vec<Key>> = OnceLock::new();
    KEYS.get_or_init(|| {
        let current = std::env::var("RANGOLI_ENCRYPTION_KEY").ok();
        let old = std::env::var("RANGOLI_ENCRYPTION_OLD_KEYS").unwrap_or_default();
        current.iter().map(String::as_str).chain(old.split(',').filter(|k| !k.trim().is_empty())).filter_map(parse_key).collect()
    })
}

/// Is a usable `RANGOLI_ENCRYPTION_KEY` set? Apps with encrypted fields refuse to start without one.
pub fn configured() -> bool {
    std::env::var("RANGOLI_ENCRYPTION_KEY").ok().and_then(|k| parse_key(&k)).is_some()
}

fn b64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            s.push(if i <= c.len() { A[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    s
}

fn unb64(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let s = s.trim_end_matches('=').as_bytes();
    let mut out = vec![];
    for chunk in s.chunks(4) {
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            n |= u32::from(val(*c)?) << (18 - 6 * i);
        }
        for i in 0..chunk.len().saturating_sub(1) {
            out.push((n >> (16 - 8 * i)) as u8);
        }
    }
    Some(out)
}

/// `enc:v1:<key id>:<base64 nonce + ciphertext + tag>`. Panics without a key: apps with
/// encrypted fields check for one at startup.
pub(crate) fn encrypt(plain: &str) -> String {
    let k = keys().first().expect("RANGOLI_ENCRYPTION_KEY is not set");
    let mut nonce = [0u8; 12];
    getrandom::fill(&mut nonce).expect("OS random number generator unavailable");
    let mut buf = plain.as_bytes().to_vec();
    k.key.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut buf).expect("AES-GCM seal");
    let mut out = nonce.to_vec();
    out.extend(buf);
    format!("{PREFIX}{}:{}", k.id, b64(&out))
}

/// The plain value, by whichever configured key wrote it. Values written before the field was
/// encrypted (no prefix) read as they are, so turning encryption on needs no data migration.
pub(crate) fn decrypt(stored: &str) -> crate::Result<String> {
    let Some(rest) = stored.strip_prefix(PREFIX) else { return Ok(stored.to_string()) };
    let bad = || crate::Error::Decode("an encrypted value can't be decrypted with the configured keys".into());
    let (id, data) = rest.split_once(':').ok_or_else(bad)?;
    let k = keys().iter().find(|k| k.id == id).ok_or_else(bad)?;
    let raw = unb64(data).ok_or_else(bad)?;
    if raw.len() < 12 + 16 {
        return Err(bad());
    }
    let (nonce, sealed) = raw.split_at(12);
    let mut buf = sealed.to_vec();
    let nonce = Nonce::try_assume_unique_for_key(nonce).map_err(|_| bad())?;
    let plain = k.key.open_in_place(nonce, Aad::empty(), &mut buf).map_err(|_| bad())?;
    String::from_utf8(plain.to_vec()).map_err(|_| bad())
}

#[cfg(test)]
mod tests {
    #[test]
    fn round_trips_and_detects_tampering() {
        std::env::set_var("RANGOLI_ENCRYPTION_KEY", "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff");
        let a = super::encrypt("123-45-6789");
        let b = super::encrypt("123-45-6789");
        assert!(a.starts_with("enc:v1:") && a != b, "a fresh nonce every time: {a}");
        assert!(!a.contains("6789"));
        assert_eq!(super::decrypt(&a).unwrap(), "123-45-6789");
        assert_eq!(super::decrypt("plain old value").unwrap(), "plain old value", "pre-encryption rows still read");
        let mut tampered: Vec<char> = a.chars().collect();
        let i = tampered.len() - 12; // inside the ciphertext, not trailing padding bits
        tampered[i] = if tampered[i] == 'A' { 'B' } else { 'A' };
        let tampered: String = tampered.into_iter().collect();
        assert!(super::decrypt(&tampered).is_err(), "tampering is detected, not silently accepted");
        assert_eq!(super::unb64(&super::b64(b"any bytes \x00\xff")).unwrap(), b"any bytes \x00\xff");
    }
}
