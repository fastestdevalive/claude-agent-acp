//! AIR client-capability parsing (Decision 11 / Item 2.4, research X-1).
//!
//! JetBrains AIR extensions apply only to a client that declares
//! `initialize.clientCapabilities._meta.jetbrains.air` (`dist/air-extension.js:90-154`).
//! When that is absent or malformed (not an object), the client is a plain
//! (non-AIR) client and gets none of the AIR advertise/behaviour (no
//! `_meta.jetbrains`, no goal, no async-task updates).
//!
//! Mirrors Node exactly (Opus review on Phase 2): `is_air()` answers "did the
//! client declare the `air` object at all" (`isAirClient` only checks that
//! `_meta.jetbrains.air` is an OBJECT); the version>=1 AND array checks belong
//! to `supports(cap)` (`clientSupportsAirCapability`), which decides whether a
//! specific capability is advertised.
//!
//! Parse-once at `initialize` (Decision 11): the session registry stores an
//! `Option<AirClient>`; each session inherits it (threaded in Phase 3 for async
//! tasks). The parsed object is inert for everything Rust does not implement —
//! each unimplemented advertised capability is a `pending` PARITY row.

use serde_json::Value;

/// A parsed AIR client declaration.
///
/// Exists only for a client that declared `_meta.jetbrains.air` as an object.
/// `is_air()` is therefore always `true` for a constructed value — the Option
/// around it carries "is this an AIR client at all". The version / capabilities
/// are parsed as options and consulted only by `supports(cap)`, exactly like
/// Node's `clientSupportsAirCapability`.
#[derive(Debug, Clone)]
pub struct AirClient {
    /// The declared `_meta.jetbrains.air.version`, if it is an integer.
    version: Option<u64>,
    /// The declared `capabilities` array verbatim, if it is an array.
    capabilities: Option<Vec<String>>,
}

impl AirClient {
    /// Parse `clientCapabilities` from an `initialize` request.
    ///
    /// Mirrors Node's `isAirClient`: returns `Some` whenever
    /// `_meta.jetbrains.air` is an OBJECT (even an empty one, even without a
    /// version or capabilities). Returns `None` when it is absent or not an
    /// object. The version/array gates live in `supports(cap)`.
    pub fn from_client_capabilities(caps: &Value) -> Option<AirClient> {
        let air = caps
            .get("_meta")
            .and_then(|m| m.get("jetbrains"))
            .and_then(|j| j.get("air"))?;
        // Node's `airExtensionMeta`: air && typeof air === "object" && !Array.isArray(air).
        if air.is_null() || !air.is_object() {
            return None;
        }
        // Node's `Number.isInteger`: a JSON float such as `1.0` is still an
        // integer version. `as_u64` alone would reject the float spelling, so
        // read the number and accept any finite, integral, non-negative value.
        let version = air.get("version").and_then(|value| {
            if let Some(integer) = value.as_u64() {
                Some(integer)
            } else {
                value
                    .as_f64()
                    .filter(|number| number.is_finite() && number.fract() == 0.0 && *number >= 0.0)
                    .map(|number| number as u64)
            }
        });
        let capabilities = air
            .get("capabilities")
            .and_then(serde_json::Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(String::from)
                    .collect()
            });
        Some(AirClient {
            version,
            capabilities,
        })
    }

    /// Whether this is an AIR client. Always `true` for a constructed value —
    /// the surrounding `Option` carries whether the client is AIR at all.
    pub fn is_air(&self) -> bool {
        true
    }

    /// Whether the client advertised support for `cap` (X-1: a capability is
    /// honoured only if listed, and the version must be an integer >= 1).
    ///
    /// Mirrors Node's `clientSupportsAirCapability`: version integer >= 1 AND
    /// the capabilities array contains `cap`.
    pub fn supports(&self, cap: &str) -> bool {
        let ok_version = self.version.is_some_and(|v| v >= 1);
        let advertised = self
            .capabilities
            .as_deref()
            .is_some_and(|caps| caps.iter().any(|c| c == cap));
        ok_version && advertised
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_air_client_capabilities() {
        let caps = json!({
            "fs": {"readTextFile": false},
            "_meta": {"jetbrains": {"air": {
                "version": 1,
                "capabilities": ["asyncTasks", "diffPatch"]
            }}}
        });
        let air = AirClient::from_client_capabilities(&caps).expect("air client");
        assert!(air.is_air());
        assert!(air.supports("asyncTasks"));
        assert!(air.supports("diffPatch"));
        assert!(!air.supports("planFile"));
    }

    #[test]
    fn empty_air_object_is_air_but_supports_nothing() {
        // Node's isAirClient: `air:{}` is AIR, but clientSupportsAirCapability
        // fails because there is no version / capabilities array.
        let caps = json!({"_meta": {"jetbrains": {"air": {}}}});
        let air = AirClient::from_client_capabilities(&caps).expect("air:{} is an air client");
        assert!(air.is_air());
        assert!(!air.supports("asyncTasks"));
    }

    #[test]
    fn air_with_version_only_is_air_but_supports_nothing() {
        // `{version:1}` is AIR (object present) but no capabilities array.
        let caps = json!({"_meta": {"jetbrains": {"air": {"version": 1}}}});
        let air = AirClient::from_client_capabilities(&caps).expect("air client");
        assert!(air.is_air());
        assert!(!air.supports("asyncTasks"));
    }

    #[test]
    fn air_with_version_and_capabilities_supports_listed() {
        let caps = json!({"_meta": {"jetbrains": {"air": {
            "version": 1, "capabilities": ["asyncTasks"]
        }}}});
        let air = AirClient::from_client_capabilities(&caps).expect("air client");
        assert!(air.is_air());
        assert!(air.supports("asyncTasks"));
        assert!(!air.supports("diffPatch"));
    }

    #[test]
    fn missing_or_non_object_air_returns_none() {
        // No _meta at all (the recorder's initialize, all corpus scripts).
        assert!(AirClient::from_client_capabilities(&json!({})).is_none());
        assert!(
            AirClient::from_client_capabilities(&json!({"fs": {"readTextFile": false}})).is_none()
        );
        // air is not an object (string, array, null) -> not AIR.
        assert!(AirClient::from_client_capabilities(&json!({
            "_meta": {"jetbrains": {"air": "asyncTasks"}}
        }))
        .is_none());
        assert!(AirClient::from_client_capabilities(&json!({
            "_meta": {"jetbrains": {"air": ["asyncTasks"]}}
        }))
        .is_none());
        assert!(AirClient::from_client_capabilities(&json!({
            "_meta": {"jetbrains": {"air": null}}
        }))
        .is_none());
    }

    #[test]
    fn zero_version_is_air_but_supports_nothing() {
        // Node: version 0 is still an object -> isAir true; but
        // clientSupportsAirCapability requires version >= 1 -> supports false.
        let caps = json!({"_meta": {"jetbrains": {"air": {
            "version": 0, "capabilities": ["asyncTasks"]
        }}}});
        let air = AirClient::from_client_capabilities(&caps).expect("air client");
        assert!(air.is_air());
        assert!(!air.supports("asyncTasks"));
    }

    #[test]
    fn non_numeric_version_is_air_but_supports_nothing() {
        let caps = json!({"_meta": {"jetbrains": {"air": {
            "version": "1", "capabilities": ["asyncTasks"]
        }}}});
        let air = AirClient::from_client_capabilities(&caps).expect("air client");
        assert!(air.is_air());
        assert!(!air.supports("asyncTasks"));
    }

    #[test]
    fn float_integer_version_is_accepted_like_number_is_integer() {
        // Node's `Number.isInteger(1.0)` is true; a JSON float version must
        // therefore pass the >= 1 gate.
        let caps = json!({"_meta": {"jetbrains": {"air": {
            "version": 1.0, "capabilities": ["asyncTasks"]
        }}}});
        let air = AirClient::from_client_capabilities(&caps).expect("air client");
        assert!(air.supports("asyncTasks"));
    }

    #[test]
    fn fractional_or_negative_version_is_not_an_integer() {
        for version in [json!(1.5), json!(-1), json!(-1.0)] {
            let caps = json!({"_meta": {"jetbrains": {"air": {
                "version": version, "capabilities": ["asyncTasks"]
            }}}});
            let air = AirClient::from_client_capabilities(&caps).expect("air client");
            assert!(!air.supports("asyncTasks"));
        }
    }
}
