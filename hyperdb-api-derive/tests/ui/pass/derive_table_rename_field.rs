use hyperdb_api::Table;
use hyperdb_api_derive::Table;

#[derive(Table)]
struct User {
    id: i64,
    #[hyperdb(rename = "email_address")]
    email: String,
}

fn main() {
    assert!(User::CREATE_SQL.contains("email_address"));
    // The original field name "email" must not appear as a standalone column identifier
    assert!(!User::CREATE_SQL.contains("email BIGINT"));
    // "email_address" should be the column name for the renamed field
    assert!(User::CREATE_SQL.contains("email_address TEXT NOT NULL"));
}
