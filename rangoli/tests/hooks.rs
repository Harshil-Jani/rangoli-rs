//! App::on_change: durable hooks on changes made through the API (and admin, MCP, imports).

use rangoli::api::{Access, Api};
use rangoli::hooks::{Action, Change};
use rangoli::orm::Model;
use rangoli::tasks::{self, TaskRecord};
use rangoli::{auth, testing, App, DateTime};
use serde_json::json;
use std::sync::Mutex;

#[derive(rangoli::Model, Clone, Debug)]
#[model(table = "hk_order", display = "item")]
struct Order {
    id: Option<i64>,
    #[field(max_length = 100)]
    item: String,
    qty: i64,
}

static SEEN: Mutex<Vec<(String, Action, i64)>> = Mutex::new(Vec::new());
static FLAKY_CALLS: Mutex<usize> = Mutex::new(0);

fn seen() -> Vec<(String, Action, i64)> {
    SEEN.lock().unwrap().clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn hooks_run_after_changes_reliably() {
    let app = App::new()
        .api::<Order>(Api::new().read(Access::Authenticated).write(Access::Authenticated))
        .on_change::<Order, _, _>(|c: Change| async move {
            SEEN.lock().unwrap().push((c.object, c.action, c.user_id));
            Ok(())
        })
        .on_change::<Order, _, _>(|_c| async move {
            let mut n = FLAKY_CALLS.lock().unwrap();
            *n += 1;
            if *n == 1 {
                return Err(rangoli::Error::Task("the mail server is down".into()));
            }
            Ok(())
        });
    testing::database(&app).await.unwrap();
    let alice = auth::create_user(&format!("hk_alice_{}", std::process::id()), "correct horse", false).await.unwrap();
    let mut c = testing::Client::new(&app);
    c.login(&alice).await;

    let made = c.post_json("/api/hk_order/", &json!({ "item": "Tea", "qty": 2 })).await;
    assert_eq!(made.status, 201);
    let id = made.json()["id"].as_i64().unwrap();
    assert!(seen().is_empty(), "the request doesn't wait for hooks");
    tasks::run_due(10).await.unwrap();
    assert_eq!(seen(), [("Tea".to_string(), Action::Added, alice.id.unwrap())], "the hook got the change and who made it");

    c.patch_json(&format!("/api/hk_order/{id}"), &json!({ "qty": 3 })).await;
    c.delete(&format!("/api/hk_order/{id}")).await;
    tasks::run_due(10).await.unwrap();
    let actions: Vec<Action> = seen().into_iter().map(|(_, a, _)| a).collect();
    assert_eq!(actions, [Action::Added, Action::Changed, Action::Deleted]);

    let queued = || async { TaskRecord::objects().filter(TaskRecord::NAME.eq("rangoli.on_change")).count().await.unwrap() };
    let before = queued().await;
    assert_eq!(c.post_json("/api/hk_order/", &json!({ "item": "", "qty": "lots" })).await.status, 400);
    assert_eq!(queued().await, before, "a refused write queues nothing");

    // The flaky hook failed once and is retried on its own; the first hook ran once per change.
    assert_eq!(*FLAKY_CALLS.lock().unwrap(), 3, "added (failed), changed, deleted");
    tasks::run_due_at(DateTime::from_unix(DateTime::now().unix() + 3600), 10).await.unwrap();
    assert_eq!(*FLAKY_CALLS.lock().unwrap(), 4, "its retry ran");
    assert_eq!(seen().len(), 3, "and the other hook didn't run again");
    sqlx::query("DROP TABLE hk_order").execute(&rangoli::orm::db().pool).await.unwrap();
}
