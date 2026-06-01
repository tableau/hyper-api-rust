use hyperdb_api::Table;
use hyperdb_api_derive::Table;

#[derive(Table)]
#[hyperdb(table = "my_users")]
struct User {
    id: i64,
    name: String,
}

fn main() {
    assert_eq!(User::NAME, "my_users");
    assert!(User::CREATE_SQL.contains("my_users"));
}
