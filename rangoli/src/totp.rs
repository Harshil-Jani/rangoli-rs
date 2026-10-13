//! Two-factor sign-in with time-based one-time codes (RFC 6238, the six digits from Google
//! Authenticator, 1Password, Authy...). Staff turn it on in the admin; from then on the admin
//! login asks for the current code too. Django needs django-otp or allauth for this.

use ring::hmac;

const STEP: i64 = 30;
const DIGITS: u32 = 6;
const B32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

pub(crate) fn base32(bytes: &[u8]) -> String {
    let mut out = String::new();
    let (mut buf, mut bits) = (0u32, 0);
    for &b in bytes {
        buf = (buf << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(B32[((buf >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(B32[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

pub(crate) fn unbase32(s: &str) -> Option<Vec<u8>> {
    let mut out = vec![];
    let (mut buf, mut bits) = (0u32, 0);
    for c in s.bytes().filter(|c| *c != b'=' && *c != b' ') {
        let v = B32.iter().position(|x| *x == c.to_ascii_uppercase())? as u32;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

/// A new random secret, as the base32 text authenticator apps take.
pub fn new_secret() -> String {
    let mut b = [0u8; 20];
    getrandom::fill(&mut b).expect("OS random number generator unavailable");
    base32(&b)
}

/// The code for one 30-second step.
fn code_at(secret: &[u8], step: i64) -> u32 {
    let key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, secret);
    let mac = hmac::sign(&key, &(step as u64).to_be_bytes());
    let h = mac.as_ref();
    let at = (h[19] & 0x0f) as usize;
    let n = (u32::from(h[at]) & 0x7f) << 24 | u32::from(h[at + 1]) << 16 | u32::from(h[at + 2]) << 8 | u32::from(h[at + 3]);
    n % 10u32.pow(DIGITS)
}

/// The current code, for tests and for showing what an app should display.
pub fn current_code(secret: &str, now: i64) -> Option<String> {
    Some(format!("{:06}", code_at(&unbase32(secret)?, now.div_euclid(STEP))))
}

/// The step `code` belongs to if it matches now, or one step either side (clock drift), and is
/// newer than `last_used`: a code works once.
pub fn verify(secret: &str, code: &str, now: i64, last_used: i64) -> Option<i64> {
    let code: u32 = code.trim().replace(' ', "").parse().ok()?;
    let key = unbase32(secret)?;
    let step = now.div_euclid(STEP);
    (step - 1..=step + 1).filter(|s| *s > last_used).find(|s| code_at(&key, *s) == code)
}

/// The `otpauth://` link authenticator apps read (as a QR code or pasted).
pub fn uri(issuer: &str, account: &str, secret: &str) -> String {
    let enc = |s: &str| {
        s.bytes().map(|b| if b.is_ascii_alphanumeric() { (b as char).to_string() } else { format!("%{b:02X}") }).collect::<String>()
    };
    format!("otpauth://totp/{}:{}?secret={secret}&issuer={}", enc(issuer), enc(account), enc(issuer))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_6238_vectors() {
        // RFC 6238 appendix B, SHA-1, secret "12345678901234567890", 8 digits truncated to 6.
        let secret = base32(b"12345678901234567890");
        assert_eq!(secret, "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ");
        assert_eq!(current_code(&secret, 59).unwrap(), "287082");
        assert_eq!(current_code(&secret, 1_111_111_109).unwrap(), "081804");
        assert_eq!(current_code(&secret, 2_000_000_000).unwrap(), "279037");
        assert_eq!(unbase32(&secret).unwrap(), b"12345678901234567890");
    }

    #[test]
    fn codes_work_once_and_only_near_now() {
        let s = new_secret();
        let now = 1_800_000_000;
        let code = current_code(&s, now).unwrap();
        let step = verify(&s, &code, now, 0).expect("current code");
        assert!(verify(&s, &code, now + 30, 0).is_some(), "one step of drift is fine");
        assert!(verify(&s, &code, now + 120, 0).is_none(), "old codes stop working");
        assert!(verify(&s, &code, now, step).is_none(), "a code can't be replayed");
        assert!(verify(&s, "000000x", now, 0).is_none());
    }
}
