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
    pub features: Option<FeaturesSettings>,
    pub listeners: Listeners,
    pub retention: Retention,
    pub sessions: SessionsSettings,
    pub storage: Storage,
    /// The subscription cost model's envelope as `GET /admin/pricing` answers it, its `unit` once at the top.
    pub subscription_costs: crate::pricing::SubscriptionCostsEnvelope,
    pub usage: UsageSettings,
}

#[derive(serde::Serialize, ToSchema)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(deny_unknown_fields)]
pub(crate) struct About {
    /// The full commit the router was built from, or `unknown` for a build that had none, as every answer carries it
    /// in `x-phoebis-build`.
    pub build: String,
    pub profile: String,
    pub version: String,
}

#[derive(serde::Serialize, ToSchema)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(deny_unknown_fields)]
pub(crate) struct Access {
    pub admin_configured: bool,
}

/// The feature listener and its CA. Null on a router without the listener.
#[derive(serde::Serialize, ToSchema)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(deny_unknown_fields)]
pub(crate) struct FeaturesSettings {
    /// The CA's end, in Unix milliseconds.
    pub ca_expires: i64,
    /// The SHA-256 of the bytes `GET /v0/features/ca.pem` serves, as 64 lowercase hex characters.
    pub ca_fingerprint: String,
    /// The CA's certificate as PEM text.
    pub ca_pem: String,
    pub listen: String,
}

#[derive(serde::Serialize, ToSchema)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(deny_unknown_fields)]
pub(crate) struct Listeners {
    pub metrics: String,
    pub proxy: String,
    /// Whether the proxy listener terminates TLS.
    pub proxy_tls: bool,
}

/// The retention bounds the router runs under: file settings of the `stats` section the console shows and never
/// edits. The two day counts read 0 for forever.
#[derive(serde::Serialize, ToSchema)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(deny_unknown_fields)]
pub(crate) struct Retention {
    pub body_capture_max_hours: u32,
    /// `stats.retention_days`: the requests with their attempts, the kept-warm marks, the events, the decisions,
    /// the quota and extra-usage samples, the registry's settled refresh attempts and its change log.
    pub retention_days: u32,
    /// `stats.rollup_retention_days`: the hourly rollups and the account rollups.
    pub rollup_retention_days: u32,
}

#[derive(serde::Serialize, ToSchema)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionsSettings {
    pub expire_after_seconds: u64,
    /// `sessions.non_companion_idle_seconds`: how long a session no companion reports stays listed past its newest pin
    /// use, its last request or its companion's end, and how long a line's pin stays listed on such a row past its last
    /// use. The lesser of the timeout and the session expiry applies.
    pub non_companion_idle_seconds: u64,
}

/// The two table paths null when their key is unset.
#[derive(serde::Serialize, ToSchema)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(deny_unknown_fields)]
pub(crate) struct Storage {
    pub api_prices_path: Option<String>,
    pub ledger_path: String,
    pub path: String,
    pub subscription_costs_path: Option<String>,
}

/// The `usage` section's file settings, which the console shows and never edits.
#[derive(serde::Serialize, ToSchema)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[serde(deny_unknown_fields)]
pub(crate) struct UsageSettings {
    /// `usage.limit_reset_warning_days`: how many days before an unused limit reset ends the Dashboard and the Quota
    /// card warn, 0 for never. A file setting the console shows and never edits.
    pub limit_reset_warning_days: u32,
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
            profile: state.profile.name.into(),
            version: env!("CARGO_PKG_VERSION").into(),
        },
        access: Access {
            admin_configured: true,
        },
        api_prices,
        features: s.features.as_ref().map(|f| FeaturesSettings {
            ca_expires: f.ca_expires,
            ca_fingerprint: f.ca_fingerprint.clone(),
            ca_pem: f.ca_pem.clone(),
            listen: f.listen.clone(),
        }),
        listeners: Listeners {
            metrics: s.metrics_listen.clone(),
            proxy: s.proxy_listen.clone(),
            proxy_tls: s.proxy_tls,
        },
        retention: Retention {
            body_capture_max_hours: state.body_capture_max_hours,
            retention_days: s.retention_days,
            rollup_retention_days: s.rollup_retention_days,
        },
        sessions: SessionsSettings {
            expire_after_seconds: state.session_expiry.as_secs(),
            non_companion_idle_seconds: state.non_companion_idle.as_secs(),
        },
        storage: Storage {
            api_prices_path: s.api_prices_path.clone(),
            ledger_path: s.ledger_path.clone(),
            path: s.storage_path.clone(),
            subscription_costs_path: s.subscription_costs_path.clone(),
        },
        subscription_costs,
        usage: UsageSettings {
            limit_reset_warning_days: s.limit_reset_warning_days,
        },
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
        features: Option<FeaturesSettings>,
        listeners: Listeners,
        retention: Retention,
        sessions: SessionsSettings,
        storage: Storage,
        subscription_costs: serde_json::Value,
        usage: UsageSettings,
    }

    #[test]
    fn the_settings_fixture_round_trips_through_its_declared_type_and_the_pricing_mirrors() {
        round_trips::<SettingsTwin>("settings");
        round_trips_each::<crate::openapi::ModelRowView>(
            "settings",
            &["api_prices", "content", "models", "*"],
        );
        round_trips_each::<crate::openapi::UnitView>("settings", &["subscription_costs", "unit"]);
        round_trips_each::<crate::openapi::SubscriptionCostsView>(
            "settings",
            &["subscription_costs", "content"],
        );
    }
}
