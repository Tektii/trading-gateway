/// Ensures the committed `openapi.json` stays in sync with the utoipa-generated spec.
///
/// If this test fails, run:
///   cargo run --bin openapi-export > openapi.json
#[test]
fn committed_openapi_spec_matches_generated() {
    let (_router, api) = tektii_gateway_core::create_gateway_router().split_for_parts();
    let generated: serde_json::Value =
        serde_json::to_value(&api).expect("failed to serialize generated spec");

    let committed_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("openapi.json");
    let committed_bytes = std::fs::read(&committed_path)
        .expect("failed to read openapi.json — run: cargo run --bin openapi-export > openapi.json");
    let committed: serde_json::Value =
        serde_json::from_slice(&committed_bytes).expect("openapi.json is not valid JSON");

    assert_eq!(
        generated, committed,
        "\nopenapi.json is stale. Update it with:\n  cargo run --bin openapi-export > openapi.json\n"
    );
}
