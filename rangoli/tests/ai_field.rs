//! `App::ai_field`: a field the model keeps filled, redone only when its sources change.

use rangoli::orm::Model;
use rangoli::{testing, App};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(rangoli::Choices, Clone, Copy, Debug, PartialEq)]
enum Team {
    Billing,
    Engineering,
}

#[derive(rangoli::Model, Clone, Debug)]
#[model(table = "af_ticket")]
struct Ticket {
    id: Option<i64>,
    #[field(max_length = 100)]
    subject: String,
    #[field(choices)]
    team: Option<Team>,
    views: i64,
}

#[tokio::test(flavor = "multi_thread")]
async fn the_model_fills_and_refreshes_a_field() {
    let calls = Arc::new(AtomicUsize::new(0));
    let n = calls.clone();
    let app = App::new()
        .llm(move |prompt: String| {
            n.fetch_add(1, Ordering::SeqCst);
            let reply = if prompt.contains("refund") {
                "Billing"
            } else if prompt.contains("crash") {
                "engineering"
            } else {
                "Marketing"
            };
            async move { Ok(reply.to_string()) }
        })
        .ai_field(&Ticket::TEAM, [&Ticket::SUBJECT], "Which team should handle this ticket?");
    testing::database(&app).await.unwrap();
    let get = |id| async move { Ticket::get(id).await.unwrap() };

    let mut t = Ticket { id: None, subject: "Where is my refund?".into(), team: None, views: 0 };
    t.save().await.unwrap();
    let id = t.id.unwrap();
    rangoli::tasks::run_due(10).await.unwrap();
    assert_eq!(get(id).await.team, Some(Team::Billing), "by label, stored as the value");

    let mut t = get(id).await;
    t.views = 5;
    t.save().await.unwrap();
    rangoli::tasks::run_due(10).await.unwrap();
    assert_eq!((get(id).await.team, calls.load(Ordering::SeqCst)), (Some(Team::Billing), 1), "other edits cost no call");

    let mut t = get(id).await;
    t.subject = "App crash on login".into();
    t.save().await.unwrap();
    assert_eq!(get(id).await.team, None, "cleared in the same write as the subject");
    rangoli::tasks::run_due(10).await.unwrap();
    assert_eq!(get(id).await.team, Some(Team::Engineering));

    let mut other = Ticket { id: None, subject: "Love the logo".into(), team: None, views: 0 };
    other.save().await.unwrap();
    rangoli::tasks::run_due(10).await.unwrap();
    let before = calls.load(Ordering::SeqCst);
    assert_eq!(get(other.id.unwrap()).await.team, None, "an answer outside the choices is left empty");
    // Another row's insert runs the fill again: it asks about the new row only.
    Ticket { id: None, subject: "refund please".into(), team: None, views: 0 }.save().await.unwrap();
    rangoli::tasks::run_due(10).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), before + 1, "the refused row isn't asked again");
    sqlx::query("DROP TABLE af_ticket").execute(&rangoli::orm::db().pool).await.unwrap();
}
