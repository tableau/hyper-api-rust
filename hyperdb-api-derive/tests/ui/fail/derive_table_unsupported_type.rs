use hyperdb_api_derive::Table;

#[derive(Table)]
struct BadField {
    id: i64,
    tags: Vec<String>, // Vec<T> other than Vec<u8> is unsupported
}

fn main() {}
