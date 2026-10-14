# A live dashboard with Server-Sent Events

```rust
use rangoli::realtime;

async fn feed() -> impl IntoResponse {
    realtime::events("orders")                   // a stream browsers reconnect to on their own
}

// anywhere: a handler, a background job, an admin action
realtime::publish("orders", r#"{"new_order": 42}"#);
```

```html
<script>
  new EventSource("/feed").onmessage = (e) => console.log(JSON.parse(e.data));
</script>
```

On Postgres, a publish from any process reaches subscribers on every instance, through `LISTEN/NOTIFY`, with no Redis. A background worker publishing "job finished" reaches browsers connected to your web servers. WebSockets work the same way with `realtime::serve`. Messages crossing instances must be under 7,900 bytes (a Postgres NOTIFY limit); send an id and let the browser fetch the rest.
