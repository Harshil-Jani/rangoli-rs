# Testing your app

```rust
use rangoli::{testing, auth};

#[tokio::test]
async fn only_owners_see_their_drafts() {
    let app = my_app();                                  // the same App your main() runs
    testing::database(&app).await.unwrap();              // a fresh database with your tables
    let alice = auth::create_user("alice", "pw", false).await.unwrap();

    let mut client = testing::Client::new(&app);
    assert_eq!(client.get("/api/draft/").await.status, 401);
    client.login(&alice).await;                          // or client.token(&api_token_or_jwt)
    let made = client.post_json("/api/draft/", &json!({ "title": "Plan" })).await;
    assert_eq!(made.status, 201);
    assert_eq!(client.get("/api/draft/").await.json()["count"], 1);
}
```

- **`testing::database`** creates a new SQLite file per run. Set `RANGOLI_TEST_DATABASE_URL` to test against Postgres or MySQL; point it at a throwaway database, because your app's tables are dropped and recreated each run. Your `migrations/` folder isn't touched.
- **`Client`** sends requests straight to your app, with no server and no port. It has `get`, `post_json`, `patch_json`, `put_json`, `delete` and `post_form` for the admin, plus `login`, `token` and `logout`. Responses give you `.status`, `.headers`, `.body` and `.json()`.
- **Counting queries,** like Django's `assertNumQueries`: `let (rows, n) = orm::count_queries(load()).await; assert_eq!(n, 2);`
