use hyperdb_api_derive::FromRow;

#[derive(FromRow)]
enum Color {
    Red,
    Green,
}

fn main() {}
