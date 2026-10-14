# Keep personal data encrypted at rest

```rust
#[field(encrypted)]
pub national_id: String,
```

```sh
export RANGOLI_ENCRYPTION_KEY=$(openssl rand -hex 32)
```

The column holds `enc:v1:...` (AES-256-GCM, with a fresh nonce for every write), so a database dump or a backup shows nothing. Your code, the admin, the API and MCP see plain values. An app with encrypted fields won't start without a key.

- **Rotating:** move the old key to `RANGOLI_ENCRYPTION_OLD_KEYS` and set a new one. Old values still read, and each row is re-encrypted on its next save.
- **Turning it on for an existing column:** rows written before read as they are; no data migration is needed.
- **The trade-off:** encrypted fields can't be searched or filtered (the API answers 400), because the stored bytes change on every write.
- **Undo snapshots:** History's snapshots of encrypted fields stay encrypted.
