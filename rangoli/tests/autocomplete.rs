//! Foreign keys to big tables become search boxes in admin forms.
#![cfg(feature = "admin")]

use rangoli::admin::ModelAdmin;
use rangoli::orm::Model;
use rangoli::{auth, testing, App};

#[derive(rangoli::Model, Clone, Debug)]
#[model(table = "ac_customer", display = "name")]
struct Customer {
    id: Option<i64>,
    #[field(max_length = 100)]
    name: String,
}

#[derive(rangoli::Model, Clone, Debug)]
#[model(table = "ac_order", display = "item")]
struct Order {
    id: Option<i64>,
    #[field(max_length = 100)]
    item: String,
    #[field(fk = Customer)]
    customer_id: i64,
}

#[tokio::test(flavor = "multi_thread")]
async fn big_foreign_keys_search_instead_of_listing() {
    let app = App::new().admin_with::<Customer>(ModelAdmin::new()).admin_with::<Order>(ModelAdmin::new());
    testing::database(&app).await.unwrap();
    let mut people: Vec<Customer> = (0..1_500).map(|i| Customer { id: None, name: format!("Customer {i:04}") }).collect();
    for chunk in people.chunks_mut(500) {
        Customer::bulk_create(chunk).await.unwrap();
    }
    let target = people[1234].id.unwrap();
    let mut boss = auth::create_user(&format!("ac_boss_{}", std::process::id()), "correct horse", true).await.unwrap();
    boss.is_superuser = true;
    boss.save().await.unwrap();
    let mut c = testing::Client::new(&app);
    c.login(&boss).await;

    let form = c.get("/admin/ac_order/add").await.body;
    assert!(
        form.contains("data-fk-for=\"id_customer_id\"") && !form.contains("Customer 1499</option>"),
        "1,500 customers: a search box, not a 1,500-item list"
    );
    let found = c.get("/admin/ac_customer/autocomplete?q=Customer+1234").await.body;
    assert!(found.contains(&format!("data-id=\"{target}\"")) && found.matches("<option").count() == 1, "{found}");
    let by_id = c.get(&format!("/admin/ac_customer/autocomplete?q={target}")).await.body;
    assert!(by_id.contains("Customer 1234"), "an id finds its row");
    let saved = c.post_form("/admin/ac_order/add", &[("item", "Tea"), ("customer_id", &target.to_string())]).await;
    assert_eq!(saved.status, 303, "the hidden id is what's saved");
    let order = Order::objects().filter(Order::ITEM.eq("Tea")).first().await.unwrap().unwrap();
    let page = c.get(&format!("/admin/ac_order/{}/", order.id.unwrap())).await.body;
    assert!(page.contains("Customer 1234 (#"), "the saved choice shows by name");
    for t in ["ac_order", "ac_customer"] {
        sqlx::query(&format!("DROP TABLE {t}")).execute(&rangoli::orm::db().pool).await.unwrap();
    }
}
