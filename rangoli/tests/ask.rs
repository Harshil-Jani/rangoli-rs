//! The admin's "Ask your data": the model sees the schema, never rows; its query is checked and
//! run read-only with the asker's permissions.
#![cfg(feature = "admin")]

use rangoli::admin::ModelAdmin;
use rangoli::orm::Model;
use rangoli::{auth, testing, App};
use std::sync::{Arc, Mutex};

#[derive(rangoli::Model, Clone, Debug)]
#[model(table = "ask_ticket", display = "subject")]
struct Ticket {
    id: Option<i64>,
    #[field(max_length = 100)]
    subject: String,
    #[field(max_length = 10)]
    status: String,
    /// Kept from the model and from Ask: the admin excludes it.
    #[field(max_length = 50)]
    secret: String,
}

#[tokio::test(flavor = "multi_thread")]
async fn ask_runs_a_checked_read_only_query() {
    let seen = Arc::new(Mutex::new(String::new()));
    let answer = Arc::new(Mutex::new(String::new()));
    let (s, a) = (seen.clone(), answer.clone());
    let app = App::new().admin_with::<Ticket>(ModelAdmin::new().exclude([&Ticket::SECRET])).llm(move |prompt| {
        *s.lock().unwrap() = prompt;
        let reply = a.lock().unwrap().clone();
        async move { Ok(reply) }
    });
    testing::database(&app).await.unwrap();
    for (subject, status) in [("Refund stuck", "open"), ("Login loop", "open"), ("Thanks!", "closed")] {
        Ticket { id: None, subject: subject.into(), status: status.into(), secret: "hunter2".into() }.save().await.unwrap();
    }
    let boss = auth::create_user(&format!("ask_boss_{}", std::process::id()), "correct horse", true).await.unwrap();
    let mut c = testing::Client::new(&app);
    c.login(&boss).await;
    let ask = |reply: &str| {
        *answer.lock().unwrap() = reply.to_string();
        let c = &c;
        async move { c.post_form("/admin/ask/", &[("q", "how are tickets doing?")]).await.body }
    };

    let page = ask(r#"Here you go: {"table": "ask_ticket", "group_by": "status"}"#).await;
    assert!(page.contains("&quot;open&quot;") && page.contains("&quot;count&quot;: 2"), "{page}");
    let prompt = seen.lock().unwrap().clone();
    assert!(prompt.contains("table ask_ticket") && prompt.contains("status: text"), "{prompt}");
    assert!(!prompt.contains("Refund stuck") && !prompt.contains("secret"), "no rows and no excluded fields reach the model");

    let page = ask(r#"{"table": "ask_ticket", "filters": {"status": "open"}, "ordering": "subject"}"#).await;
    assert!(page.contains("2 Tickets") && page.contains("Login loop") && !page.contains("Thanks!"), "{page}");
    assert!(ask(r#"{"table": "ask_ticket", "filters": {"secret": "hunter2"}}"#).await.contains("hides"), "hidden fields refused");
    assert!(ask(r#"{"table": "rangoli_api_token"}"#).await.contains("can&#x27;t view"), "framework tables refused");
    assert!(ask("I'd rather chat").await.contains("wasn&#x27;t a query"));
    assert_eq!(Ticket::objects().count().await.unwrap(), 3, "nothing changed");
    sqlx::query("DROP TABLE ask_ticket").execute(&rangoli::orm::db().pool).await.unwrap();
}
