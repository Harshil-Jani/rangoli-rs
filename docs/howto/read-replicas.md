# Read replicas

```sh
export RANGOLI_DATABASE_URL=postgres://.../app                    # the primary
export RANGOLI_DATABASE_REPLICA_URL=postgres://...replica.../app  # a read replica
```

Plain `SELECT`s go to the replica, and everything else goes to the primary: writes, transactions, and the reads that follow a write.

- **You always read your own writes.** Once a request or background job has written, its later reads use the primary, so replica lag never shows a user stale data right after they saved.
- **Forcing the primary:** `orm::on_primary(async { ... })` sends every query in it to the primary, for reads that mustn't lag, such as balance checks.
- **Same database kind:** the replica must be the same kind of database as the primary.
