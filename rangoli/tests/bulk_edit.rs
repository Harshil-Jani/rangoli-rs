//! "Edit selected": set fields on many rows, each validated, all or nothing.
#![cfg(feature = "admin")]

use rangoli::admin::ModelAdmin;
use rangoli::orm::Model;
use rangoli::{auth, testing, App};

#[derive(rangoli::Model, Clone, Debug)]
#[model(table = "be_task", display = "title")]
struct Task {
    id: Option<i64>,
    #[field(max_length = 50)]
    title: String,
    done: bool,
    points: i64,
}

#[tokio::test(flavor = "multi_thread")]
async fn edit_many_rows_at_once() {
    let app = App::new()
        .admin_with::<Task>(ModelAdmin::new())
        .validate::<Task>(|t| if t.points > 100 { Err(vec![("points", "Too many points.".into())]) } else { Ok(()) });
    testing::database(&app).await.unwrap();
    let mut ids = vec![];
    for (title, points) in [("a", 1), ("b", 2), ("c", 99)] {
        let mut t = Task { id: None, title: title.into(), done: false, points };
        t.save().await.unwrap();
        ids.push(t.id.unwrap().to_string());
    }
    let mut boss = auth::create_user(&format!("be_boss_{}", std::process::id()), "correct horse", true).await.unwrap();
    boss.save().await.unwrap();
    let mut c = testing::Client::new(&app);
    c.login(&boss).await;
    let sel: Vec<(&str, &str)> = ids.iter().map(|i| ("_selected_action", i.as_str())).collect();

    let mut form = vec![("action", "edit_selected")];
    form.extend(&sel);
    let page = c.post_form("/admin/be_task/", &form).await;
    assert!(page.body.contains("Edit 3") && page.body.contains("set_done"), "the edit form");

    let mut set_done = vec![("action", "edit_selected"), ("post", "yes"), ("set_done", "on"), ("done", "on")];
    set_done.extend(&sel);
    assert_eq!(c.post_form("/admin/be_task/", &set_done).await.status, 303);
    assert!(Task::objects().all().await.unwrap().iter().all(|t| t.done), "all three done");
    assert!(Task::objects().all().await.unwrap().iter().map(|t| t.title.as_str()).eq(["a", "b", "c"]), "other fields untouched");

    let mut too_many = vec![("action", "edit_selected"), ("post", "yes"), ("set_points", "on"), ("points", "101")];
    too_many.extend(&sel);
    let refused = c.post_form("/admin/be_task/", &too_many).await;
    assert!(refused.status == 422 && refused.body.contains("Too many points."), "row rules apply to each row");
    assert!(Task::objects().all().await.unwrap().iter().all(|t| t.points < 100), "and nothing was saved");
    sqlx::query("DROP TABLE be_task").execute(&rangoli::orm::db().pool).await.unwrap();
}
