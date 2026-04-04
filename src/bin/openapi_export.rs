/// Prints the gateway's OpenAPI spec as pretty JSON to stdout.
///
/// Usage: `cargo run --bin openapi-export > openapi.json`
fn main() {
    let (_router, api) = tektii_gateway_core::create_gateway_router().split_for_parts();
    println!(
        "{}",
        serde_json::to_string_pretty(&api).expect("failed to serialize OpenAPI spec")
    );
}
