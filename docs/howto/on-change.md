# Run code when data changes (signals that don't get lost)

```rust
App::new().on_change::<Order, _, _>(|change| async move {
    if change.action == Action::Added {
        mail::send(Mail::new("sales@acme.com", &format!("New order: {}", change.object), "")).await?;
    }
    Ok(())
})
```

The hook runs for each change made through the admin, the JSON API, AI agents (MCP), approvals and CSV imports. It gets the table, the row's id and name, *added*, *changed* or *deleted*, History's message, and who made the change.

**How it differs from Django's signals:**

- **Queued with the change, in the same transaction.** A change that's rolled back never fires its hook, and a committed one always does, even if the server restarts. (By construction; the rollback case isn't separately tested.)
- **Runs on the background worker,** so a slow hook (sending email, calling an API) never slows the request, and a failing one never breaks it.
- **Retried with backoff** when it fails, on its own: other hooks on the same change don't run again. Delivery is *at least once*, so make hooks safe to run twice.

Your own code's writes (`order.save()`) don't fire hooks. Call your function directly there.
