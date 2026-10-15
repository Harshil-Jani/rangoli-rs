//! ?expand= embeds related rows, never more than the caller could fetch directly.

use rangoli::api::{Access, Api};
use rangoli::orm::Model;
use rangoli::{auth, testing, App};

#[derive(rangoli::Model, Clone, Debug)]
#[model(table = "ex_author", display = "name")]
struct Author {
    id: Option<i64>,
    #[field(max_length = 100)]
    name: String,
    #[field(max_length = 100, private)]
    phone: Option<String>,
}

#[derive(rangoli::Model, Clone, Debug)]
#[model(table = "ex_vault", display = "label")]
struct Vault {
    id: Option<i64>,
    #[field(max_length = 100)]
    label: String,
}

#[derive(rangoli::Model, Clone, Debug)]
#[model(table = "ex_post", display = "title")]
struct Post {
    id: Option<i64>,
    #[field(max_length = 100)]
    title: String,
    #[field(fk = Author)]
    author_id: i64,
    #[field(fk = Vault)]
    vault_id: Option<i64>,
}

#[tokio::test(flavor = "multi_thread")]
async fn expand_embeds_only_what_you_could_read() {
    let app = App::new()
        .api::<Post>(Api::new().read(Access::Public))
        .api::<Author>(Api::new().read(Access::Public))
        .api::<Vault>(Api::new().read(Access::Staff));
    testing::database(&app).await.unwrap();
    let mut a = Author { id: None, name: "Ada".into(), phone: Some("555-0100".into()) };
    a.save().await.unwrap();
    let mut v = Vault { id: None, label: "keys".into() };
    v.save().await.unwrap();
    let mut p = Post { id: None, title: "Engines".into(), author_id: a.id.unwrap(), vault_id: v.id };
    p.save().await.unwrap();
    let c = testing::Client::new(&app);

    let list = c.get("/api/ex_post/?expand=author_id").await.json();
    let row = &list["results"][0];
    assert_eq!(row["author"]["name"], "Ada", "embedded: {row}");
    assert!(row["author"].get("phone").is_none(), "private fields stay private when embedded");
    let one = c.get(&format!("/api/ex_post/{}?expand=author_id", p.id.unwrap())).await.json();
    assert_eq!(one["author"]["name"], "Ada", "on single rows too");
    assert_eq!(c.get("/api/ex_post/?expand=vault_id").await.status, 400, "anonymous can't read vaults, so can't expand them");
    assert_eq!(c.get("/api/ex_post/?expand=title").await.status, 400, "only foreign keys");

    let mut staff = auth::create_user(&format!("ex_staff_{}", std::process::id()), "correct horse", true).await.unwrap();
    staff.save().await.unwrap();
    let mut s = testing::Client::new(&app);
    s.login(&staff).await;
    assert_eq!(s.get("/api/ex_post/?expand=vault_id,author_id").await.json()["results"][0]["vault"]["label"], "keys", "staff can");
    for t in ["ex_post", "ex_vault", "ex_author"] {
        sqlx::query(&format!("DROP TABLE {t}")).execute(&rangoli::orm::db().pool).await.unwrap();
    }
}
