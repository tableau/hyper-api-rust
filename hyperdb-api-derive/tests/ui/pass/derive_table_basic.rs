use hyperdb_api::Table;
use hyperdb_api_derive::Table;

#[derive(Table)]
struct User {
    id: i64,
    name: String,
    score: Option<f64>,
}

fn main() {
    // Default table name is lower_snake_case of struct ident: "User" → "user"
    assert!(User::CREATE_SQL.contains("user"));
    assert_eq!(User::NAME, "user");
}
