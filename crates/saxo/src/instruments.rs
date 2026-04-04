//! Saxo Bank instrument resolution.
//!
//! Maps between Saxo symbols (e.g., `EURUSD:FxSpot`) and Saxo's UIC-based
//! identification (e.g., `Uic: 21, AssetType: "FxSpot"`).
#![allow(dead_code)]
//!
//! The [`SaxoInstrumentMap`] is built once at startup by loading instruments
//! from the Saxo reference data API, then used immutably for bidirectional lookups.

use std::collections::HashMap;

use serde::Deserialize;
use tracing::warn;

use super::error::SaxoError;
use super::http::SaxoHttpClient;

// --- DTOs for Saxo API response ---

/// Response from `GET /ref/v1/instruments`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SaxoInstrumentsResponse {
    /// Instrument data entries.
    pub data: Vec<SaxoInstrumentData>,
    /// Pagination URL. Present when more pages are available.
    #[serde(rename = "__next")]
    pub next: Option<String>,
}

/// A single instrument from the Saxo reference data API.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct SaxoInstrumentData {
    /// Unique instrument identifier (Saxo-specific).
    /// The `/ref/v1/instruments` endpoint returns this as `Identifier`,
    /// while other Saxo endpoints use `Uic` — accept both.
    #[serde(alias = "Identifier")]
    pub uic: u32,
    /// Asset type (e.g., "FxSpot", "CfdOnIndex", "CfdOnStock", "CfdOnFutures").
    pub asset_type: String,
    /// Human-readable description.
    pub description: String,
    /// Trading symbol (e.g., "EURUSD"). May be absent for some instruments.
    #[serde(default)]
    pub symbol: Option<String>,
    /// Settlement currency code.
    #[serde(default)]
    pub currency_code: Option<String>,
    /// Number of decimal places for prices.
    #[serde(default)]
    pub price_decimals: Option<u8>,
}

// --- Instrument info stored in the map ---

/// Resolved Saxo instrument info for a symbol.
#[derive(Debug, Clone)]
pub struct SaxoInstrumentInfo {
    /// Saxo unique instrument identifier.
    pub uic: u32,
    /// Saxo asset type string.
    pub asset_type: String,
    /// Human-readable description.
    pub description: String,
    /// Price decimal places.
    pub price_decimals: Option<u8>,
}

// --- Immutable instrument map ---

/// Bidirectional map between symbols and Saxo UICs.
///
/// Built once at startup via [`load`](Self::load), then used immutably.
pub struct SaxoInstrumentMap {
    pub(crate) symbol_to_saxo: HashMap<String, SaxoInstrumentInfo>,
    pub(crate) uic_to_symbol: HashMap<(u32, String), String>,
}

impl SaxoInstrumentMap {
    /// Load instruments from the Saxo reference data API.
    ///
    /// Fetches all instruments for the given asset types, handling pagination
    /// automatically. Returns an immutable map ready for bidirectional lookups.
    pub async fn load(
        http_client: &SaxoHttpClient,
        asset_types: &[&str],
    ) -> Result<Self, SaxoError> {
        let mut symbol_to_saxo = HashMap::new();
        let mut uic_to_symbol = HashMap::new();

        for asset_type in asset_types {
            let mut skip = 0;
            let top = 1000;

            loop {
                let path =
                    format!("/ref/v1/instruments?AssetTypes={asset_type}&$top={top}&$skip={skip}");
                let response: SaxoInstrumentsResponse = http_client.get(&path).await?;
                let page_count = response.data.len();

                for instrument in &response.data {
                    if let Some(tektii_symbol) = derive_symbol(instrument) {
                        if symbol_to_saxo.contains_key(&tektii_symbol) {
                            warn!(
                                symbol = %tektii_symbol,
                                uic = instrument.uic,
                                "Duplicate instrument symbol, keeping first"
                            );
                            continue;
                        }

                        let info = SaxoInstrumentInfo {
                            uic: instrument.uic,
                            asset_type: instrument.asset_type.clone(),
                            description: instrument.description.clone(),
                            price_decimals: instrument.price_decimals,
                        };

                        uic_to_symbol.insert(
                            (instrument.uic, instrument.asset_type.clone()),
                            tektii_symbol.clone(),
                        );
                        symbol_to_saxo.insert(tektii_symbol, info);
                    }
                }

                if response.next.is_none() || page_count == 0 {
                    break;
                }
                skip += page_count;
            }
        }

        Ok(Self {
            symbol_to_saxo,
            uic_to_symbol,
        })
    }

    /// Look up Saxo instrument info for a symbol.
    pub fn resolve_symbol(&self, symbol: &str) -> Result<&SaxoInstrumentInfo, SaxoError> {
        self.symbol_to_saxo
            .get(symbol)
            .ok_or_else(|| SaxoError::InstrumentNotFound(symbol.to_string()))
    }

    /// Look up symbol for a Saxo UIC and asset type.
    pub fn resolve_uic(&self, uic: u32, asset_type: &str) -> Result<&str, SaxoError> {
        self.uic_to_symbol
            .get(&(uic, asset_type.to_string()))
            .map(String::as_str)
            .ok_or_else(|| SaxoError::InstrumentNotFound(format!("UIC {uic} ({asset_type})")))
    }

    /// Number of instruments loaded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.symbol_to_saxo.len()
    }

    /// Whether the map is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.symbol_to_saxo.is_empty()
    }
}

/// Derive a symbol key from Saxo instrument data.
///
/// Combines the raw symbol with the asset type using Saxo's native format:
/// `{Symbol}:{AssetType}` (e.g., `EURUSD:FxSpot`, `US500:CfdOnIndex`).
///
/// Returns `None` if the symbol field is missing.
fn derive_symbol(instrument: &SaxoInstrumentData) -> Option<String> {
    let symbol = instrument.symbol.as_deref()?;
    Some(format!("{symbol}:{}", instrument.asset_type))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_instrument(
        uic: u32,
        asset_type: &str,
        description: &str,
        symbol: Option<&str>,
    ) -> SaxoInstrumentData {
        SaxoInstrumentData {
            uic,
            asset_type: asset_type.to_string(),
            description: description.to_string(),
            symbol: symbol.map(|s| s.to_string()),
            currency_code: Some("USD".to_string()),
            price_decimals: Some(5),
        }
    }

    #[test]
    fn derive_symbol_fx_spot() {
        let inst = make_instrument(21, "FxSpot", "Euro/US Dollar", Some("EURUSD"));
        assert_eq!(derive_symbol(&inst), Some("EURUSD:FxSpot".to_string()));
    }

    #[test]
    fn derive_symbol_cfd_on_index() {
        let inst = make_instrument(100, "CfdOnIndex", "US 500", Some("US500"));
        assert_eq!(derive_symbol(&inst), Some("US500:CfdOnIndex".to_string()));
    }

    #[test]
    fn derive_symbol_cfd_on_stock() {
        let inst = make_instrument(200, "CfdOnStock", "Apple Inc", Some("AAPL"));
        assert_eq!(derive_symbol(&inst), Some("AAPL:CfdOnStock".to_string()));
    }

    #[test]
    fn derive_symbol_cfd_on_futures() {
        let inst = make_instrument(300, "CfdOnFutures", "Crude Oil", Some("OIL"));
        assert_eq!(derive_symbol(&inst), Some("OIL:CfdOnFutures".to_string()));
    }

    #[test]
    fn derive_symbol_preserves_full_symbol() {
        let inst = make_instrument(21, "FxSpot", "Euro/US Dollar", Some("EURUSD:xOANDA"));
        assert_eq!(
            derive_symbol(&inst),
            Some("EURUSD:xOANDA:FxSpot".to_string())
        );
    }

    #[test]
    fn derive_symbol_any_asset_type() {
        let inst = make_instrument(999, "Bond", "Some Bond", Some("BOND1"));
        assert_eq!(derive_symbol(&inst), Some("BOND1:Bond".to_string()));
    }

    #[test]
    fn derive_symbol_missing_symbol_field_returns_none() {
        let inst = make_instrument(21, "FxSpot", "Euro/US Dollar", None);
        assert_eq!(derive_symbol(&inst), None);
    }

    #[test]
    fn resolve_symbol_not_found() {
        let map = SaxoInstrumentMap {
            symbol_to_saxo: HashMap::new(),
            uic_to_symbol: HashMap::new(),
        };
        let err = map.resolve_symbol("NOPE:FxSpot").unwrap_err();
        assert!(matches!(err, SaxoError::InstrumentNotFound(s) if s == "NOPE:FxSpot"));
    }

    #[test]
    fn resolve_uic_not_found() {
        let map = SaxoInstrumentMap {
            symbol_to_saxo: HashMap::new(),
            uic_to_symbol: HashMap::new(),
        };
        let err = map.resolve_uic(999, "FxSpot").unwrap_err();
        assert!(matches!(err, SaxoError::InstrumentNotFound(s) if s.contains("999")));
    }

    #[test]
    fn empty_map_is_empty() {
        let map = SaxoInstrumentMap {
            symbol_to_saxo: HashMap::new(),
            uic_to_symbol: HashMap::new(),
        };
        assert!(map.is_empty());
        assert_eq!(map.len(), 0);
    }
}
