//! API models for the gateway.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::models::{Position, Trade};

/// Response containing trades and positions.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TradesResponse {
    /// List of trades.
    pub trades: Vec<Trade>,
    /// Positions by symbol.
    pub positions: HashMap<String, Position>,
}
