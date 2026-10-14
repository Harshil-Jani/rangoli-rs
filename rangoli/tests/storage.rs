//! Presigned uploads and downloads against a real S3-compatible server. Runs when
//! RANGOLI_TEST_S3_ENDPOINT is set (with RANGOLI_TEST_S3_BUCKET, _KEY and _SECRET), e.g. a local
//! SeaweedFS or MinIO; skipped otherwise. Uses curl as the "browser".

use rangoli::storage::S3;
use std::process::Command;

fn curl(args: &[&str]) -> (u16, String) {
    let out = Command::new("curl").args(["-s", "-o", "/tmp/rangoli-s3-body", "-w", "%{http_code}"]).args(args).output().expect("curl");
    let code = String::from_utf8_lossy(&out.stdout).parse().unwrap_or(0);
    (code, std::fs::read_to_string("/tmp/rangoli-s3-body").unwrap_or_default())
}

fn post(form: &rangoli::storage::PostForm, content_type: &str, file: &str, key_override: Option<&str>) -> u16 {
    let mut args: Vec<String> = vec![];
    for (k, v) in &form.fields {
        let v = if k == "key" { key_override.unwrap_or(v) } else { v };
        args.extend(["-F".into(), format!("{k}={v}")]);
    }
    args.extend(["-F".into(), format!("Content-Type={content_type}"), "-F".into(), format!("file=@{file}"), form.url.clone()]);
    curl(&args.iter().map(String::as_str).collect::<Vec<_>>()).0
}

#[test]
fn presigned_uploads_and_downloads() {
    let Ok(endpoint) = std::env::var("RANGOLI_TEST_S3_ENDPOINT") else {
        eprintln!("skipped: set RANGOLI_TEST_S3_ENDPOINT");
        return;
    };
    let var = |k: &str| std::env::var(k).unwrap();
    let s3 = S3::new(&var("RANGOLI_TEST_S3_BUCKET"), "us-east-1", &var("RANGOLI_TEST_S3_KEY"), &var("RANGOLI_TEST_S3_SECRET"))
        .endpoint(&endpoint);
    let dir = std::env::temp_dir();
    let (small, big) = (dir.join("rangoli-s3-small.png"), dir.join("rangoli-s3-big.png"));
    std::fs::write(&small, vec![b'x'; 1_000]).unwrap();
    std::fs::write(&big, vec![b'y'; 2_000]).unwrap();
    let key = format!("avatars/{}.png", std::process::id());

    let form = s3.presigned_post(&key, 1..=1_500, "image/", 600);
    let small_path = small.to_str().unwrap();
    assert!(matches!(post(&form, "image/png", small_path, None), 200 | 201 | 204), "an upload inside the policy is stored");
    let (code, body) = curl(&[&s3.presign_get(&key, 60)]);
    assert_eq!((code, body.len()), (200, 1_000), "and downloads through a presigned link");

    assert_eq!(post(&form, "image/png", big.to_str().unwrap(), None) / 100, 4, "too big: refused by the storage service");
    assert_eq!(post(&form, "text/html", small_path, None) / 100, 4, "wrong content type: refused");
    assert_eq!(post(&form, "image/png", small_path, Some("avatars/someone-else.png")) / 100, 4, "another key: refused");

    let unsigned = s3.presign_get(&key, 60).split('?').next().unwrap().to_string();
    assert_eq!(curl(&[&unsigned]).0, 403, "the bucket is private without a signature");
    let tampered = s3.presign_get(&key, 60).replace("X-Amz-Expires=60", "X-Amz-Expires=604800");
    assert_eq!(curl(&[&tampered]).0, 403, "changing the link breaks its signature");
    let short = s3.presign_get(&key, 1);
    std::thread::sleep(std::time::Duration::from_secs(3));
    assert_eq!(curl(&[&short]).0, 403, "an expired link stops working");

    let put = s3.presign_put(&format!("{key}.put"), 60);
    assert_eq!(curl(&["-X", "PUT", "--data-binary", &format!("@{small_path}"), &put]).0, 200, "presigned PUT works too");
}
