//! Email over SMTP against a real server (Mailpit). Runs when RANGOLI_TEST_MAILPIT is set to
//! Mailpit's address, e.g. `127.0.0.1:1025,127.0.0.1:8025` (SMTP, then its web API).

use rangoli::mail::{deliver, Mail};
use std::process::Command;

fn messages(api: &str) -> serde_json::Value {
    let out = Command::new("curl").args(["-s", &format!("http://{api}/api/v1/messages")]).output().unwrap();
    serde_json::from_slice(&out.stdout).unwrap_or_default()
}

#[tokio::test]
async fn delivers_over_smtp() {
    let Ok(cfg) = std::env::var("RANGOLI_TEST_MAILPIT") else {
        eprintln!("skipped: set RANGOLI_TEST_MAILPIT=127.0.0.1:1025,127.0.0.1:8025");
        return;
    };
    let (smtp, api) = cfg.split_once(',').unwrap();
    Command::new("curl").args(["-s", "-X", "DELETE", &format!("http://{api}/api/v1/messages")]).output().unwrap();
    // SAFETY: this test is the only thread reading these variables.
    unsafe {
        std::env::set_var("RANGOLI_SMTP_URL", format!("smtp://app%40acme.com:s3cr%3At@{smtp}"));
        std::env::set_var("RANGOLI_EMAIL_FROM", "Acme <no-reply@acme.com>");
    }
    let mail = Mail::new("Bob <bob@example.com>", "Your résumé was received", "Hello Bob,\n.this line starts with a dot\nBye")
        .html("<p>Hello <b>Bob</b></p>");
    deliver(&mail).await.expect("delivered");

    let list = messages(api);
    assert_eq!(list["total"], 1, "{list}");
    let m = &list["messages"][0];
    assert_eq!(m["Subject"], "Your résumé was received", "non-ASCII subjects survive");
    assert_eq!(m["From"]["Address"], "no-reply@acme.com");
    assert_eq!(m["To"][0]["Address"], "bob@example.com");
    let id = m["ID"].as_str().unwrap();
    let out = Command::new("curl").args(["-s", &format!("http://{api}/api/v1/message/{id}")]).output().unwrap();
    let full: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        full["Text"].as_str().unwrap().replace("\r\n", "\n").trim(),
        "Hello Bob,\n.this line starts with a dot\nBye",
        "dot-stuffing undone by the server: {full}"
    );
    assert!(full["HTML"].as_str().unwrap().contains("<b>Bob</b>"), "the HTML part arrives too");
}
