# A JSON API where each user sees only their own data

The DRF way is a permission class, a `get_queryset` override and a `perform_create` override, repeated for every viewset. Here it's one call.

```rust
#[derive(Model, Clone, Debug)]
#[model(table = "notes_note", display = "title")]
pub struct Note {
    pub id: Option<i64>,
    #[field(max_length = 200)]
    pub title: String,
    #[field(fk = rangoli::auth::User, cascade)]
    pub owner_id: i64,
}

App::new()
    .api::<Note>(Api::new().read(Access::Authenticated).write(Access::Authenticated).owned_by(&Note::OWNER_ID))
    .cors(["https://app.example.com"])      // your front end on another domain
    .rate_limit(600)                        // per user (or IP) per minute
```

- **Own rows only:** each user lists, reads, updates and deletes only their own notes. Anyone else's note is a 404.
- **Owner can't be forged:** the owner is always the caller, whatever the request body says.
- **Staff:** they see everything.

## Tokens for apps and scripts

```sh
cargo run -- createtoken alice mobile-app --days 90
curl -H "Authorization: Bearer rgl_..." https://api.example.com/api/notes_note/
```

Only a hash of each token is stored. To revoke one, delete it under **API tokens** in the admin, or set an expiry there.

## Big lists

`?cursor=` pages by id with no `COUNT(*)`. On a million rows, page 10,000 measured 103× faster than offset paging. Lists also answer `QUERY` with the options as a JSON body.
