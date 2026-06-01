use hyperdb_api_derive::query_as;

fn main() {
    let _ = query_as!(User); // missing SQL argument
}
