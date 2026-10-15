# Rules about a whole row

Field types, lengths and choices are checked for you. For rules that involve more than one field, like Django's `Model.clean()`:

```rust
App::new().validate::<Booking>(|b| {
    if b.ends_at <= b.starts_at {
        return Err(vec![("ends_at", "Must be after the start.".into())]);
    }
    Ok(())
})
```

- **Where it runs:** admin adds and changes, API creates and updates, MCP writes and CSV imports.
- **Partial updates** (`PATCH`) are merged with the stored row first, so the rule always sees the whole row.
- **Errors** look like field errors: `{"ends_at": ["Must be after the start."]}` from the API, next to the field in the admin, on the row's line in an import preview. Use `"__all__"` for errors about the row as a whole.
