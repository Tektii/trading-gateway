//! Thin wiremock helpers to reduce per-test boilerplate.

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Start a [`MockServer`] and return it alongside its base URL string.
///
/// The base URL is in the form `http://127.0.0.1:{port}` — ready to pass
/// directly to adapter credential constructors.
pub async fn start_mock_server() -> (MockServer, String) {
    let server = MockServer::start().await;
    let base_url = server.uri();
    (server, base_url)
}

/// Mount a JSON response for a given HTTP method + path.
///
/// ```ignore
/// mount_json(&server, "GET", "/v2/account", 200, json!({ "balance": "100000" })).await;
/// ```
pub async fn mount_json(
    server: &MockServer,
    http_method: &str,
    url_path: &str,
    status: u16,
    body: serde_json::Value,
) {
    Mock::given(method(http_method))
        .and(path(url_path))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(server)
        .await;
}

/// Mount an empty response (e.g., 204 No Content for DELETE).
pub async fn mount_empty(server: &MockServer, http_method: &str, url_path: &str, status: u16) {
    Mock::given(method(http_method))
        .and(path(url_path))
        .respond_with(ResponseTemplate::new(status))
        .mount(server)
        .await;
}

/// Shallow-merge `overrides` keys into `base` (top-level only).
///
/// Used by adapter test helpers to customise JSON response fixtures:
///
/// ```ignore
/// let mut json = default_order_json();
/// merge_json(&mut json, &json!({ "status": "filled" }));
/// ```
pub fn merge_json(base: &mut serde_json::Value, overrides: &serde_json::Value) {
    if let (Some(base_obj), Some(over_obj)) = (base.as_object_mut(), overrides.as_object()) {
        for (k, v) in over_obj {
            base_obj.insert(k.clone(), v.clone());
        }
    }
}

/// Mount a plain-text response (e.g., for testing malformed JSON handling).
pub async fn mount_text(
    server: &MockServer,
    http_method: &str,
    url_path: &str,
    status: u16,
    body: &str,
) {
    Mock::given(method(http_method))
        .and(path(url_path))
        .respond_with(ResponseTemplate::new(status).set_body_string(body))
        .mount(server)
        .await;
}

/// Mount a response with custom headers (e.g., `Retry-After` for 429).
pub async fn mount_with_headers(
    server: &MockServer,
    http_method: &str,
    url_path: &str,
    status: u16,
    body: serde_json::Value,
    headers: &[(&str, &str)],
) {
    let mut template = ResponseTemplate::new(status).set_body_json(body);
    for &(name, value) in headers {
        template = template.append_header(name, value);
    }
    Mock::given(method(http_method))
        .and(path(url_path))
        .respond_with(template)
        .mount(server)
        .await;
}
