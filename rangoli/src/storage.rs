//! Files in S3-compatible storage (AWS S3, Cloudflare R2, MinIO, Backblaze B2...), without the
//! bytes ever passing through your app.
//!
//! The server hands out short-lived signed URLs: browsers upload straight to the bucket with a
//! presigned POST whose policy the storage service enforces (size range, content type, exact
//! key), and download private files through presigned GET links. A 5 GB upload costs your app
//! one tiny JSON response. Signing is AWS Signature Version 4 with the `ring`/`sha2` already
//! compiled in; nothing else is added.
//!
//! ```ignore
//! let s3 = S3::from_env().expect("RANGOLI_S3_BUCKET is set");
//! let form = s3.presigned_post("avatars/42.png", 0..=5_000_000, "image/", 600);   // send to the browser
//! let link = s3.presign_get("avatars/42.png", 300);                               // a 5-minute download link
//! ```

use ring::hmac;
use sha2::{Digest, Sha256};

/// One bucket and the credentials to sign for it.
#[derive(Clone)]
pub struct S3 {
    pub bucket: String,
    pub region: String,
    /// `https://s3.<region>.amazonaws.com` by default; set for R2, MinIO, B2.
    pub endpoint: String,
    /// `bucket.host/key` (AWS's default) or `host/bucket/key` (MinIO and most self-hosted).
    pub path_style: bool,
    access_key: String,
    secret_key: String,
}

impl std::fmt::Debug for S3 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3").field("bucket", &self.bucket).field("region", &self.region).field("endpoint", &self.endpoint).finish()
    }
}

/// What a browser posts to upload one file: `url`, plus `fields` as form fields before the
/// file field (named `file`).
#[derive(Clone, Debug, serde::Serialize)]
pub struct PostForm {
    pub url: String,
    pub fields: Vec<(String, String)>,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn mac(key: &[u8], msg: &str) -> Vec<u8> {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), msg.as_bytes()).as_ref().to_vec()
}

/// RFC 3986 encoding as SigV4 wants it; `/` kept in paths.
fn enc(s: &str, keep_slash: bool) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            b'/' if keep_slash => "/".into(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// `20130524T000000Z` and `20130524` for a unix time.
fn stamps(now: i64) -> (String, String) {
    let t = crate::DateTime::from_unix(now).to_string(); // 2013-05-24T00:00:00Z
    let amz = t.replace(['-', ':'], "");
    (amz.clone(), amz[..8].to_string())
}

impl S3 {
    pub fn new(bucket: &str, region: &str, access_key: &str, secret_key: &str) -> S3 {
        S3 {
            bucket: bucket.into(),
            region: region.into(),
            // us-east-1 is also served at the global host, which AWS's own examples sign against.
            endpoint: if region == "us-east-1" { "https://s3.amazonaws.com".into() } else { format!("https://s3.{region}.amazonaws.com") },
            path_style: false,
            access_key: access_key.into(),
            secret_key: secret_key.into(),
        }
    }

    /// A custom endpoint (R2, MinIO, B2), addressed path-style.
    pub fn endpoint(mut self, url: &str) -> S3 {
        self.endpoint = url.trim_end_matches('/').into();
        self.path_style = true;
        self
    }

    /// From `RANGOLI_S3_BUCKET`, `RANGOLI_S3_REGION` (default `us-east-1`), optional
    /// `RANGOLI_S3_ENDPOINT`, and `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`.
    pub fn from_env() -> Option<S3> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let region = var("RANGOLI_S3_REGION").unwrap_or_else(|| "us-east-1".into());
        let s3 = S3::new(&var("RANGOLI_S3_BUCKET")?, &region, &var("AWS_ACCESS_KEY_ID")?, &var("AWS_SECRET_ACCESS_KEY")?);
        Some(match var("RANGOLI_S3_ENDPOINT") {
            Some(e) => s3.endpoint(&e),
            None => s3,
        })
    }

    fn host_and_path(&self, key: &str) -> (String, String, String) {
        let (scheme, host) = self.endpoint.split_once("://").unwrap_or(("https", &self.endpoint));
        let key = enc(key.trim_start_matches('/'), true);
        if self.path_style {
            (format!("{scheme}://{host}"), host.to_string(), format!("/{}/{key}", enc(&self.bucket, false)))
        } else {
            let host = format!("{}.{host}", self.bucket);
            (format!("{scheme}://{host}"), host, format!("/{key}"))
        }
    }

    fn signing_key(&self, date: &str) -> Vec<u8> {
        let k = mac(format!("AWS4{}", self.secret_key).as_bytes(), date);
        let k = mac(&k, &self.region);
        let k = mac(&k, "s3");
        mac(&k, "aws4_request")
    }

    fn presign(&self, method: &str, key: &str, expires_secs: u64, now: i64) -> String {
        let (base, host, path) = self.host_and_path(key);
        let (amz, date) = stamps(now);
        let scope = format!("{date}/{}/s3/aws4_request", self.region);
        let mut q = [
            ("X-Amz-Algorithm", "AWS4-HMAC-SHA256".to_string()),
            ("X-Amz-Credential", format!("{}/{scope}", self.access_key)),
            ("X-Amz-Date", amz.clone()),
            ("X-Amz-Expires", expires_secs.clamp(1, 604_800).to_string()),
            ("X-Amz-SignedHeaders", "host".to_string()),
        ]
        .iter()
        .map(|(k, v)| format!("{}={}", enc(k, false), enc(v, false)))
        .collect::<Vec<_>>();
        q.sort();
        let query = q.join("&");
        let canonical = format!("{method}\n{path}\n{query}\nhost:{host}\n\nhost\nUNSIGNED-PAYLOAD");
        let to_sign = format!("AWS4-HMAC-SHA256\n{amz}\n{scope}\n{}", hex(&Sha256::digest(canonical.as_bytes())));
        let sig = hex(&mac(&self.signing_key(&date), &to_sign));
        format!("{base}{path}?{query}&X-Amz-Signature={sig}")
    }

    /// A link that downloads `key` for the next `expires_secs` seconds (max 7 days).
    pub fn presign_get(&self, key: &str, expires_secs: u64) -> String {
        self.presign("GET", key, expires_secs, crate::auth::now())
    }

    /// A URL a client can `PUT` the file to for `expires_secs`. No size limit: prefer
    /// [`S3::presigned_post`] for uploads from browsers.
    pub fn presign_put(&self, key: &str, expires_secs: u64) -> String {
        self.presign("PUT", key, expires_secs, crate::auth::now())
    }

    /// A browser upload of exactly `key`, between `size` bytes, with a `Content-Type` starting
    /// with `type_prefix` (`"image/"`, or `""` for any), valid for `expires_secs`. The storage
    /// service checks all three and refuses anything else.
    pub fn presigned_post(&self, key: &str, size: std::ops::RangeInclusive<u64>, type_prefix: &str, expires_secs: u64) -> PostForm {
        self.post_at(key, size, type_prefix, expires_secs, crate::auth::now())
    }

    fn post_at(&self, key: &str, size: std::ops::RangeInclusive<u64>, type_prefix: &str, expires_secs: u64, now: i64) -> PostForm {
        let (amz, date) = stamps(now);
        let credential = format!("{}/{date}/{}/s3/aws4_request", self.access_key, self.region);
        let expiration = crate::DateTime::from_unix(now + expires_secs.clamp(1, 604_800) as i64).to_string();
        let policy = serde_json::json!({
            "expiration": expiration.replace('Z', ".000Z"),
            "conditions": [
                { "bucket": self.bucket },
                { "key": key },
                ["content-length-range", size.start(), size.end()],
                ["starts-with", "$Content-Type", type_prefix],
                { "x-amz-algorithm": "AWS4-HMAC-SHA256" },
                { "x-amz-credential": credential },
                { "x-amz-date": amz },
            ]
        });
        let policy = crate::django::base64(policy.to_string().as_bytes());
        let signature = hex(&mac(&self.signing_key(&date), &policy));
        let (base, _, path) = self.host_and_path("");
        let url = if self.path_style { format!("{base}{}", path.trim_end_matches('/')) } else { base };
        let fields = [
            ("key", key.to_string()),
            ("x-amz-algorithm", "AWS4-HMAC-SHA256".into()),
            ("x-amz-credential", credential),
            ("x-amz-date", amz),
            ("policy", policy),
            ("x-amz-signature", signature),
        ];
        PostForm { url, fields: fields.into_iter().map(|(k, v)| (k.to_string(), v)).collect() }
    }
}

#[cfg(test)]
mod tests {
    use super::S3;

    #[test]
    fn aws_documented_presigned_url() {
        // From AWS's SigV4 query-string example ("Authenticating Requests: Using Query
        // Parameters"): GET test.txt from examplebucket, 2013-05-24, valid 86400 s.
        let s3 = S3::new("examplebucket", "us-east-1", "AKIAIOSFODNN7EXAMPLE", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY");
        let url = s3.presign("GET", "test.txt", 86_400, 1_369_353_600);
        assert!(url.starts_with("https://examplebucket.s3.amazonaws.com/test.txt?X-Amz-Algorithm=AWS4-HMAC-SHA256"), "{url}");
        assert!(url.ends_with("&X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"), "{url}");
    }

    #[test]
    fn keys_are_encoded_and_path_style_works() {
        let s3 = S3::new("b", "auto", "k", "s").endpoint("http://127.0.0.1:9000/");
        let url = s3.presign("GET", "a dir/ü file?.txt", 60, 1_800_000_000);
        assert!(url.starts_with("http://127.0.0.1:9000/b/a%20dir/%C3%BC%20file%3F.txt?"), "{url}");
        let form = s3.post_at("up/1.png", 0..=1000, "image/", 60, 1_800_000_000);
        assert_eq!(form.url, "http://127.0.0.1:9000/b");
        assert_eq!(form.fields[0], ("key".to_string(), "up/1.png".to_string()));
    }
}
