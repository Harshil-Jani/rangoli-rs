# A multi-tenant SaaS: every customer sees only their own data

Put a `tenant_id` on your tables and write one rule. Every door enforces it: the admin, the JSON API and AI agents.

```rust
#[derive(Model, Clone, Debug)]
#[model(table = "app_project", unique(tenant_id, slug))]   // slugs are unique per customer
pub struct Project {
    pub id: Option<i64>,
    #[field(fk = Tenant)]
    pub tenant_id: i64,                                     // indexed automatically, as a foreign key
    #[field(max_length = 50)]
    pub slug: String,
}

fn same_tenant(user: Option<&User>) -> Option<Expr<Project>> {
    match user {
        Some(u) if u.is_superuser => None,                  // your own staff see everything
        Some(u) => Some(Project::TENANT_ID.eq(tenant_of(u))),
        None => Some(Project::TENANT_ID.eq(-1)),            // signed out: nothing
    }
}

App::new()
    .api::<Project>(Api::new().read(Access::Authenticated).write(Access::Authenticated).rows(same_tenant))
    .admin_with::<Project>(ModelAdmin::new().rows(same_tenant))
```

**What this gives you:**

- **Other customers' rows don't exist** for a user, through any door: lists, search, filters, ids, bulk actions, CSV export, vector search and MCP tools all apply the rule.
- **No moving rows between customers.** Creating a row in another customer, or changing one so it would leave the user's own, is refused and rolled back.
- **Tested door by door,** with the check removed to make sure the tests notice ([`tests/e2e.rs`](../../rangoli/tests/e2e.rs), "row-level permissions").

**Why not a Postgres schema per customer (Django Trac #6148)?** With a schema per customer, every migration runs once per customer and every connection has to switch schemas. One table with a `tenant_id` and an index scales to millions of rows per customer and keeps migrations to one run. If you need hard isolation, for example for regulatory reasons, give that customer their own database and their own Rangoli process.
