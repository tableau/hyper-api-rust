use hyperdb_api::FromRow;
use hyperdb_api_derive::FromRow;

#[derive(FromRow)]
struct User {
    id: i32,
    name: String,
    score: Option<f64>,
}

fn main() {}
