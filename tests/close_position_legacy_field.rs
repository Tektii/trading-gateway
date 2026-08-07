//! `ClosePositionRequest` used to carry a `cancel_associated_orders` flag that
//! nothing ever read: no adapter branched on it, and the SL/TP cleanup it
//! appeared to control runs unconditionally on position close. It was removed
//! rather than implemented.
//!
//! That is a wire-format change, and released SDK clients still send the field.
//! These tests pin both halves: it is gone from the published schema, and a
//! client that still sends it is ignored rather than rejected.

use tektii_gateway_core::models::{OrderHandle, OrderStatus, TradingPlatform};
use tektii_gateway_test_support::harness::spawn_test_gateway;
use tektii_gateway_test_support::mock_adapter::MockTradingAdapter;

const PLATFORM: TradingPlatform = TradingPlatform::AlpacaPaper;
const POSITION_ID: &str = "pos-legacy";
const CLOSE_ORDER_ID: &str = "close-order-1";

/// The flag must not reappear in the OpenAPI surface.
#[test]
fn close_position_schema_omits_cancel_associated_orders() {
    let (_router, api) = tektii_gateway_core::create_gateway_router().split_for_parts();
    let spec = serde_json::to_value(&api).expect("failed to serialize generated spec");

    let properties = spec
        .pointer("/components/schemas/ClosePositionRequest/properties")
        .and_then(serde_json::Value::as_object)
        .expect("ClosePositionRequest schema has no properties");

    assert!(
        !properties.contains_key("cancel_associated_orders"),
        "cancel_associated_orders is documented again but still has no effect"
    );
}

/// A client built against the old schema keeps working: the removed field is
/// ignored, not rejected, and the close still goes through to the adapter.
#[tokio::test]
async fn close_position_accepts_legacy_cancel_associated_orders_field() {
    let adapter = MockTradingAdapter::new(PLATFORM).with_close_position_response(
        POSITION_ID,
        Ok(OrderHandle {
            id: CLOSE_ORDER_ID.to_string(),
            client_order_id: None,
            correlation_id: None,
            status: OrderStatus::Open,
        }),
    );
    let gw = spawn_test_gateway(adapter).await;

    let response = reqwest::Client::new()
        .delete(format!("{}/positions/{POSITION_ID}", gw.base_url()))
        .json(&serde_json::json!({ "cancel_associated_orders": true }))
        .send()
        .await
        .expect("close request failed");

    assert_eq!(
        response.status(),
        reqwest::StatusCode::OK,
        "legacy close body was rejected"
    );

    let handle: OrderHandle = response
        .json()
        .await
        .expect("response was not an OrderHandle");
    assert_eq!(handle.id, CLOSE_ORDER_ID, "close did not reach the adapter");
    assert_eq!(handle.status, OrderStatus::Open);
}
