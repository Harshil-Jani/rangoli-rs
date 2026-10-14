# File uploads without the files touching your server

Browsers upload straight to S3, R2, MinIO or any S3-compatible bucket. Your app only signs a small form, and the storage service enforces the limits.

```sh
export RANGOLI_S3_BUCKET=my-bucket AWS_ACCESS_KEY_ID=... AWS_SECRET_ACCESS_KEY=...
export RANGOLI_S3_ENDPOINT=https://<account>.r2.cloudflarestorage.com   # leave out for AWS
```

```rust
use rangoli::storage::S3;

// POST /avatar-upload: a form the browser posts the file to directly
async fn avatar_upload(user: CurrentUser) -> Json<rangoli::storage::PostForm> {
    let s3 = S3::from_env().unwrap();
    let key = format!("avatars/{}.png", user.0.unwrap().id.unwrap());
    Json(s3.presigned_post(&key, 1..=5_000_000, "image/", 600))   // up to 5 MB, images only, 10 minutes
}
```

```js
const { url, fields } = await (await fetch("/avatar-upload", { method: "POST" })).json();
const body = new FormData();
for (const [k, v] of fields) body.append(k, v);
body.append("Content-Type", file.type);
body.append("file", file);              // the file goes last
await fetch(url, { method: "POST", body });
```

For downloads of private files, use `s3.presign_get(&key, 300)`, a link that works for 5 minutes.

**What the storage service refuses:**

- files outside the size range;
- other content types;
- any key other than the one you signed;
- changed or expired links.

[`tests/storage.rs`](../../rangoli/tests/storage.rs) checks each of these against a real S3 server in CI, and the signing matches AWS's published example.
