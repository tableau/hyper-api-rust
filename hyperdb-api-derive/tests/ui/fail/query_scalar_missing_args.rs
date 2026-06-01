use hyperdb_api_derive::query_scalar;

fn main() {
    let _ = query_scalar!(i64); // missing SQL argument
}
