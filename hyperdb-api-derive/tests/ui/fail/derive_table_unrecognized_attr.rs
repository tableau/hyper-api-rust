use hyperdb_api_derive::Table;

#[derive(Table)]
#[hyperdb(unknown_attr)]
struct User {
    id: i64,
}

fn main() {}
