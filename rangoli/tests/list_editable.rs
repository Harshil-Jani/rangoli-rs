//! `ModelAdmin::list_editable` (edit columns in the changelist, one Save, all or nothing) and
//! `ModelAdmin::save_as`.
#![cfg(feature = "admin")]

use rangoli::admin::ModelAdmin;
use rangoli::orm::Model;
use rangoli::{auth, testing, App};

#[derive(rangoli::Model, Clone, Debug)]
#[model(table = "le_task", display = "title")]
struct Task {
    id: Option<i64>,
    #[field(max_length = 50)]
    title: String,
    done: bool,
    points: i64,
}

#[tokio::test(flavor = "multi_thread")]
async fn edit_in_the_list() {
    let app = App::new()
        .admin_with::<Task>(
            ModelAdmin::new()
                .list_display([&Task::TITLE, &Task::DONE, &Task::POINTS])
                .list_editable([&Task::DONE, &Task::POINTS])
                .save_as(),
        )
        .validate::<Task>(|t| if t.points > 100 { Err(vec![("points", "Too many points.".into())]) } else { Ok(()) });
    testing::database(&app).await.unwrap();
    let mut ids = vec![];
    for (title, points) in [("a", 1), ("b", 2)] {
        let mut t = Task { id: None, title: title.into(), done: false, points };
        t.save().await.unwrap();
        ids.push(t.id.unwrap());
    }
    let mut boss = auth::create_user(&format!("le_boss_{}", std::process::id()), "correct horse", true).await.unwrap();
    boss.save().await.unwrap();
    let mut c = testing::Client::new(&app);
    c.login(&boss).await;

    let page = c.get("/admin/le_task/").await;
    assert!(page.body.contains(&format!("name=\"e{}_points\"", ids[0])) && page.body.contains("_save_list"), "inputs in the list");

    let (a, b) = (ids[0].to_string(), ids[1].to_string());
    let (pa, pb, da) = (format!("e{a}_points"), format!("e{b}_points"), format!("e{a}_done"));
    let save = vec![
        ("_save_list", "1"),
        ("_list_row", a.as_str()),
        ("_list_row", b.as_str()),
        (pa.as_str(), "7"),
        (da.as_str(), "on"),
        (pb.as_str(), "2"),
    ];
    assert_eq!(c.post_form("/admin/le_task/", &save).await.status, 303);
    let rows = Task::objects().order_by(Task::ID.asc()).all().await.unwrap();
    assert!(rows[0].points == 7 && rows[0].done && rows[1].points == 2 && !rows[1].done, "{rows:?}");

    let bad = vec![("_save_list", "1"), ("_list_row", a.as_str()), ("_list_row", b.as_str()), (pa.as_str(), "8"), (pb.as_str(), "101")];
    let refused = c.post_form("/admin/le_task/", &bad).await;
    assert!(refused.status == 422 && refused.body.contains("Too many points."), "row rules apply");
    assert_eq!(Task::objects().order_by(Task::ID.asc()).all().await.unwrap()[0].points, 7, "and nothing was saved");
    let change = c.get(&format!("/admin/le_task/{}/", ids[0])).await;
    assert!(change.body.contains("formaction=\"/admin/le_task/add\""), "save_as shows its button");
    assert_eq!(
        c.post_form("/admin/le_task/add", &[("title", "a copy"), ("points", "7")]).await.status,
        303,
        "and what it posts makes a new row"
    );
    assert_eq!(Task::objects().count().await.unwrap(), 3);
    sqlx::query("DROP TABLE le_task").execute(&rangoli::orm::db().pool).await.unwrap();
}
