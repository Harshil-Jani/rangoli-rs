# Background jobs and schedules without Celery

Django's usual answer is Celery plus Redis or RabbitMQ, plus celery-beat for schedules, run as extra processes. Rangoli keeps jobs in your database and runs them in the same binary.

```rust
#[derive(Serialize, Deserialize)]
struct SendReceipt { order_id: i64 }

impl Task for SendReceipt {
    const NAME: &'static str = "send_receipt";
    async fn run(self) -> rangoli::Result<()> { /* email it */ Ok(()) }
}

#[derive(Serialize, Deserialize)]
struct PurgeOldCarts;
impl Task for PurgeOldCarts {
    const NAME: &'static str = "purge_old_carts";
    async fn run(self) -> rangoli::Result<()> { /* ... */ Ok(()) }
}

App::new()
    .task::<SendReceipt>()
    .schedule("0 3 * * *", PurgeOldCarts)            // 03:00 UTC daily

SendReceipt { order_id: 42 }.enqueue().await?;       // or .enqueue_in(Duration::from_secs(60))
```

- **Where jobs run:** `runserver` runs workers in-process (`RANGOLI_WORKERS`); `cargo run -- worker` runs a dedicated one.
- **Failures:** they retry with backoff, panics are caught, and a crashed worker's jobs are picked up again.
- **Schedules:** each tick runs once across any number of workers and servers. In the test, 20 racing workers queued a tick once; without the unique key it was queued 20 times.
- **Queue once:** `ReindexProduct { id: 7 }.enqueue_unique("product-7")` returns `None` while that key is still waiting or running, so a double click or two servers can't queue it twice. In the test, 10 racing enqueues queued 1 run; the key frees when the run ends. (Celery's top complaints include "same task runs multiple times", celery#4400.)
- **Seeing and retrying them:** every job is in the admin under **Task records**. Select failed runs and choose **Retry selected failed runs now**.
