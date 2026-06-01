// FromRow trait is in hyperdb_api; the derive macro is in hyperdb_api_derive.
// No need to import the trait here since we don't call any trait methods.
use hyperdb_api_derive::FromRow;

#[derive(FromRow)]
struct User {
    id: i32,
    name: String,
    score: Option<f64>,
}

fn main() {}
