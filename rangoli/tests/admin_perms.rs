//! App::admin_permissions: staff only do what their groups allow, at every door of the admin.
#![cfg(feature = "admin")]

use rangoli::admin::ModelAdmin;
use rangoli::auth::{self, Group};
use rangoli::orm::Model;
use rangoli::{testing, App};

#[derive(rangoli::Model, Clone, Debug)]
#[model(table = "ap_note", display = "title")]
struct Note {
    id: Option<i64>,
    #[field(max_length = 100)]
    title: String,
}

#[derive(rangoli::Model, Clone, Debug)]
#[model(table = "ap_payroll", display = "who")]
struct Payroll {
    id: Option<i64>,
    #[field(max_length = 100)]
    who: String,
}

async fn staff_in(name: &str, permissions: &str, stamp: u32) -> rangoli::auth::User {
    let mut u = auth::create_user(&format!("{name}_{stamp}"), "correct horse", true).await.unwrap();
    u.is_superuser = false;
    u.save().await.unwrap();
    if !permissions.is_empty() {
        let mut g = Group { id: None, name: format!("{name}s_{stamp}"), permissions: permissions.into() };
        g.save().await.unwrap();
        u.groups().add(&[&g]).await.unwrap();
    }
    u
}

#[tokio::test(flavor = "multi_thread")]
async fn staff_do_only_what_their_groups_allow() {
    let app = App::new().admin_with::<Note>(ModelAdmin::new()).admin_with::<Payroll>(ModelAdmin::new()).admin_permissions();
    testing::database(&app).await.unwrap();
    let stamp = std::process::id();
    let mut note = Note { id: None, title: "Agenda".into() };
    note.save().await.unwrap();
    let mut pay = Payroll { id: None, who: "Everyone's salary".into() };
    pay.save().await.unwrap();
    let (n, p) = (note.id.unwrap(), pay.id.unwrap());

    let editor = staff_in("editor", "ap_note.view\nap_note.change", stamp).await;
    let viewer = staff_in("viewer", "*.view", stamp).await;
    let nobody = staff_in("nobody", "", stamp).await;
    let mut boss = auth::create_user(&format!("boss_{stamp}"), "correct horse", true).await.unwrap();
    boss.is_superuser = true;
    boss.save().await.unwrap();

    let mut c = testing::Client::new(&app);
    c.login(&editor).await;
    let home = c.get("/admin/").await.body;
    assert!(home.contains("/admin/ap_note/") && !home.contains("/admin/ap_payroll/"), "the sidebar shows what you may see");
    assert_eq!(c.get("/admin/ap_payroll/").await.status, 403, "another model's list");
    assert_eq!(c.get(&format!("/admin/ap_payroll/{p}/")).await.status, 403, "or its rows");
    assert_eq!(c.get("/admin/ap_payroll/export.csv").await.status, 403, "or its CSV");
    assert_eq!(c.get("/admin/ap_note/").await.status, 200);
    assert_eq!(c.post_form(&format!("/admin/ap_note/{n}/"), &[("title", "Agenda v2")]).await.status, 303, "change is granted");
    assert_eq!(c.get("/admin/ap_note/add").await.status, 403, "add isn't");
    assert_eq!(c.post_form("/admin/ap_note/add", &[("title", "sneaky")]).await.status, 403);
    assert_eq!(c.get(&format!("/admin/ap_note/{n}/delete")).await.status, 403, "nor delete");
    let bulk = n.to_string();
    assert_eq!(
        c.post_form("/admin/ap_note/", &[("action", "delete_selected"), ("_selected_action", &bulk), ("post", "yes")]).await.status,
        403
    );
    assert!(Note::get(n).await.is_ok(), "the note survived");

    c.login(&viewer).await;
    assert_eq!(c.get(&format!("/admin/ap_payroll/{p}/")).await.status, 200, "*.view sees every model");
    assert_eq!(c.post_form(&format!("/admin/ap_payroll/{p}/"), &[("who", "changed")]).await.status, 403, "but changes nothing");
    assert_eq!(Payroll::get(p).await.unwrap().who, "Everyone's salary");

    c.login(&nobody).await;
    assert_eq!(c.get("/admin/ap_note/").await.status, 403, "staff without groups see nothing");
    assert!(!c.get("/admin/").await.body.contains("/admin/ap_note/"));

    c.login(&boss).await;
    assert_eq!(c.get(&format!("/admin/ap_note/{n}/delete")).await.status, 200, "superusers can do anything");
    let form = c.get(&format!("/admin/rangoli_user/{}/", editor.id.unwrap())).await.body;
    assert!(form.contains("name=\"groups\""), "groups are assigned on the user's page");
    sqlx::query("DELETE FROM rangoli_group WHERE name LIKE '%s\\_%'").execute(&rangoli::orm::db().pool).await.ok();
    for t in ["ap_note", "ap_payroll"] {
        sqlx::query(&format!("DROP TABLE {t}")).execute(&rangoli::orm::db().pool).await.unwrap();
    }
}
