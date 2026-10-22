//! The bootstrap values the console reads, redacted: a setting's value, never a secret and never the name of the
//! variable that holds one.

use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use utoipa::ToSchema;

use crate::AdminState;

/// The bootstrap values the console reads, redacted: a setting's value, never a secret and never the name of the
/// variable that holds one.
#[derive(serde::Serialize, ToSchema)]
pub(crate) struct Settings {
    pub about: About,
    pub access: Access,
    /// The price table's envelope as `GET /admin/pricing` answers it.
    pub api_prices: crate::pricing::Envelope<crate::pricing::PriceTableContent>,
    pub listeners: Listeners,
    pub retention: Retention,
    pub sessions: SessionsSettings,
    pub storage: Storage,
    /// The subscription cost model's envelope as `GET /admin/pricing` answers it.
    pub subscription_costs: crate::pricing::Envelope<crate::pricing::SubscriptionCostsContent>,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct About {
    /// The full commit the router was built from, or `unknown` for a build that had none, as every answer carries it
    /// in `x-phoebis-build`.
    pub build: String,
    pub profile: String,
    pub version: String,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Access {
    pub admin_configured: bool,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Listeners {
    pub metrics: String,
    pub proxy: String,
    /// Whether the proxy listener terminates TLS.
    pub proxy_tls: bool,
}

/// The retention bounds the router runs under: file settings of the `stats` section the console shows and never
/// edits. The two day counts read 0 for forever.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Retention {
    pub body_capture_max_hours: u32,
    /// `stats.retention_days`: the requests with their attempts, the kept-warm marks, the events, the decisions,
    /// the quota and extra-usage samples, the registry's settled refresh attempts and its change log.
    pub retention_days: u32,
    /// `stats.rollup_retention_days`: the hourly rollups and the account rollups.
    pub rollup_retention_days: u32,
}

#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionsSettings {
    pub expire_after_seconds: u64,
}

/// The two table paths null when their key is unset.
#[derive(serde::Serialize, serde::Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Storage {
    pub api_prices_path: Option<String>,
    pub ledger_path: String,
    pub path: String,
    pub subscription_costs_path: Option<String>,
}

/// Read the settings.
#[utoipa::path(
    get,
    path = "/admin/settings",
    operation_id = "settings_show",
    tag = "Settings",
    responses(
        (status = 200, description = "The running configuration, redacted", body = Settings),
    ),
)]
pub(crate) async fn show(State(state): State<Arc<AdminState>>) -> Response {
    let s = &state.settings;
    let crate::pricing::Envelopes {
        api_prices,
        subscription_costs,
    } = crate::pricing::table_envelopes(&state.pricing.revisions(), &state.tables, (state.clock)());
    Json(Settings {
        about: About {
            build: state.build.into(),
            profile: s.profile.clone(),
            version: env!("CARGO_PKG_VERSION").into(),
        },
        access: Access {
            admin_configured: true,
        },
        api_prices,
        listeners: Listeners {
            metrics: s.metrics_listen.clone(),
            proxy: s.proxy_listen.clone(),
            proxy_tls: s.proxy_tls,
        },
        retention: Retention {
            body_capture_max_hours: s.body_capture_max_hours,
            retention_days: s.retention_days,
            rollup_retention_days: s.rollup_retention_days,
        },
        sessions: SessionsSettings {
            expire_after_seconds: s.session_expire_after_seconds,
        },
        storage: Storage {
            api_prices_path: s.api_prices_path.clone(),
            ledger_path: s.ledger_path.clone(),
            path: s.storage_path.clone(),
            subscription_costs_path: s.subscription_costs_path.clone(),
        },
        subscription_costs,
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openapi::round_trip::{round_trips, round_trips_each};

    #[derive(serde::Serialize, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct SettingsTwin {
        about: About,
        access: Access,
        api_prices: serde_json::Value,
        listeners: Listeners,
        retention: Retention,
        sessions: SessionsSettings,
        storage: Storage,
        subscription_costs: serde_json::Value,
    }

    #[test]
    fn the_settings_fixture_round_trips_through_its_declared_type_and_the_pricing_mirrors() {
        round_trips::<SettingsTwin>("settings");
        round_trips_each::<crate::openapi::ModelRowView>(
            "settings",
            &["api_prices", "content", "models", "*"],
        );
        round_trips_each::<crate::openapi::SubscriptionCostsView>(
            "settings",
            &["subscription_costs", "content"],
        );
    }
}
