# Vector search (semantic search) on your own database

```rust
#[derive(Model, Clone, Debug)]
#[model(table = "kb_article")]
pub struct Article {
    pub id: Option<i64>,
    #[field(max_length = 200)]
    pub title: String,
    #[field(vector = 1536, index)]          // your embedding model's size
    pub embedding: Option<Vector>,
}

// Store an embedding from any provider (OpenAI, Voyage, a local model)
article.embedding = Some(Vector(embed(&article.title).await?));
article.save().await?;

// The 10 most similar articles, with their cosine distance; filters still apply
let similar = Article::objects().filter(Article::PUBLISHED.eq(true)).nearest(Article::EMBEDDING, &query, 10).await?;
```

- **Postgres:** a pgvector column, with an HNSW index when the field is `index`ed. `makemigrations` enables the extension and builds the index, concurrently on a live table. Use the `pgvector/pgvector` image or a host that has the extension.
- **MySQL and SQLite:** vectors are stored as JSON, and `nearest` compares them in your app. That's fine for tens of thousands of rows; use Postgres beyond that.

**Measured (tests/vector.rs):** on 20,000 random 64-dimension vectors, the index's top 10 matched the exact top 10 for 97.6% of results (recall@10), with a median query of 8 ms. Random vectors are the hard case; real embeddings cluster and do better. To get there, Rangoli widens pgvector's default search for each query, which on its own scored 0.69.

## Let agents search by meaning

```rust
App::new()
    .api::<Article>(Api::new().read(Access::Authenticated))
    .embed(|text| async move { my_embedding_model(&text).await })   // returns Vec<f32>
```

Every model with a vector field then answers `GET /api/kb_article/similar?text=refund+policy&k=5`, and AI agents get a `kb_article_similar` tool. Both go through the same checks as a list: access level, `owned_by`, row permissions, and the token's tables. An agent finds the closest rows *it is allowed to see*, never another user's (tested, with the check removed to make sure the test notices).

Vectors stay out of the JSON API and MCP output (agents don't need 1,536 numbers) and out of admin forms and CSV files.
