//! The per-pool cache TTL policy: which marker TTL a request's turns take, if any, and the per-model overrides
//! and in-overage widening that decide it.

/// The two TTLs the API accepts. Serialized as the wire spells them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum CacheTtl {
    #[serde(rename = "5m")]
    FiveMinutes,
    #[serde(rename = "1h")]
    OneHour,
}

impl CacheTtl {
    /// The `ttl` member's value.
    pub fn as_str(self) -> &'static str {
        match self {
            CacheTtl::FiveMinutes => "5m",
            CacheTtl::OneHour => "1h",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelTtl {
    pub main: CacheTtl,
    pub model: String,
    pub subagent: CacheTtl,
}

/// A pool's cache TTL policy, stored as JSON in `pools.cache_ttl_json`. Every field has a default so a stored
/// object written before a field existed still reads, and a later strategy joins as one more defaulted field.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct CacheTtlPolicy {
    pub enabled: bool,
    /// Taken over the rules below while a write costs money: the serving account is in overage, or is an
    /// `api_key` account. `None` keeps the rules below.
    pub in_overage: Option<CacheTtl>,
    pub main: CacheTtl,
    /// Per-model overrides on the canonical model id; a model absent here takes `main` and `subagent`.
    pub models: Vec<ModelTtl>,
    pub subagent: CacheTtl,
}

impl Default for CacheTtlPolicy {
    fn default() -> Self {
        CacheTtlPolicy {
            enabled: false,
            in_overage: None,
            main: CacheTtl::OneHour,
            models: Vec::new(),
            subagent: CacheTtl::FiveMinutes,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CacheTtlError {
    #[error("cache_ttl.models lists {0} twice")]
    DuplicateModel(String),
    #[error("cache_ttl.models[].model must be a model id")]
    EmptyModel,
    #[error("cache_ttl is not a policy object: {0}")]
    Shape(String),
}

impl CacheTtlPolicy {
    /// What the admin API accepts: an object, known fields only, canonical model ids listed once each.
    pub fn parse(raw: &serde_json::Value) -> Result<CacheTtlPolicy, CacheTtlError> {
        // As `KeepalivePolicy::parse`: the container-level default lets an array deserialize element by element.
        if !raw.is_object() {
            return Err(CacheTtlError::Shape(
                "cache_ttl policy must be a JSON object".into(),
            ));
        }
        let policy: CacheTtlPolicy =
            serde_json::from_value(raw.clone()).map_err(|e| CacheTtlError::Shape(e.to_string()))?;
        let mut seen = std::collections::BTreeSet::new();
        for entry in &policy.models {
            if entry.model.is_empty() {
                return Err(CacheTtlError::EmptyModel);
            }
            if !seen.insert(entry.model.as_str()) {
                return Err(CacheTtlError::DuplicateModel(entry.model.clone()));
            }
        }
        Ok(policy)
    }

    /// What a stored row reads as; anything unreadable reads as the default (off).
    pub fn from_json(raw: &str) -> CacheTtlPolicy {
        serde_json::from_str(raw).unwrap_or_default()
    }

    /// The TTL every marker is rewritten to, `None` when the policy is off. `writes_cost_money` is the widened
    /// in-overage rule; it wins over the per-model overrides because it names the price, not the model.
    pub fn ttl_for(
        &self,
        canonical_model: &str,
        main_agent: bool,
        writes_cost_money: bool,
    ) -> Option<CacheTtl> {
        if !self.enabled {
            return None;
        }
        if let (true, Some(ttl)) = (writes_cost_money, self.in_overage) {
            return Some(ttl);
        }
        let (main, subagent) = self
            .models
            .iter()
            .find(|m| m.model == canonical_model)
            .or_else(|| {
                phoebis_routing::listing::undated(canonical_model)
                    .and_then(|base| self.models.iter().find(|m| m.model == base))
            })
            .map_or((self.main, self.subagent), |m| (m.main, m.subagent));
        Some(if main_agent { main } else { subagent })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_policy_parses_with_defaults_and_refuses_an_unknown_field_and_a_duplicate_model() {
        let policy = CacheTtlPolicy::parse(&serde_json::json!({ "enabled": true })).unwrap();
        assert_eq!(
            (policy.main, policy.subagent, policy.in_overage),
            (CacheTtl::OneHour, CacheTtl::FiveMinutes, None)
        );
        assert!(matches!(
            CacheTtlPolicy::parse(&serde_json::json!({ "main": "2h" })),
            Err(CacheTtlError::Shape(_))
        ));
        assert!(
            matches!(
                CacheTtlPolicy::parse(&serde_json::json!({ "strategy": "x" })),
                Err(CacheTtlError::Shape(_))
            ),
            "a field this build does not know is refused, so a later strategy cannot be silently dropped"
        );
        let twice = serde_json::json!({ "models": [{ "model": "claude-haiku-4-5-20251001", "main": "5m", "subagent": "5m" }, { "model": "claude-haiku-4-5-20251001", "main": "1h", "subagent": "1h" }] });
        assert_eq!(
            CacheTtlPolicy::parse(&twice),
            Err(CacheTtlError::DuplicateModel(
                "claude-haiku-4-5-20251001".into()
            ))
        );
        assert_eq!(
            CacheTtlPolicy::parse(
                &serde_json::json!({ "models": [{ "model": "", "main": "5m", "subagent": "5m" }] })
            ),
            Err(CacheTtlError::EmptyModel)
        );
        assert_eq!(
            CacheTtlPolicy::from_json("not json"),
            CacheTtlPolicy::default()
        );
    }

    #[test]
    fn the_ttl_follows_the_agent_the_model_override_and_the_money_rule_in_that_order() {
        let policy = CacheTtlPolicy::parse(&serde_json::json!({ "enabled": true, "main": "1h", "subagent": "5m", "in_overage": "5m", "models": [{ "model": "claude-haiku-4-5-20251001", "main": "5m", "subagent": "1h" }] })).unwrap();
        assert_eq!(
            policy.ttl_for("claude-opus-5", true, false),
            Some(CacheTtl::OneHour)
        );
        assert_eq!(
            policy.ttl_for("claude-opus-5", false, false),
            Some(CacheTtl::FiveMinutes)
        );
        assert_eq!(
            policy.ttl_for("claude-haiku-4-5-20251001", true, false),
            Some(CacheTtl::FiveMinutes)
        );
        assert_eq!(
            policy.ttl_for("claude-haiku-4-5-20251001", false, false),
            Some(CacheTtl::OneHour)
        );
        assert_eq!(
            policy.ttl_for("claude-haiku-4-5-20251001", false, true),
            Some(CacheTtl::FiveMinutes),
            "money beats the model override"
        );
        let off = CacheTtlPolicy::parse(
            &serde_json::json!({ "enabled": true, "in_overage": null, "main": "1h" }),
        )
        .unwrap();
        assert_eq!(
            off.ttl_for("claude-opus-5", true, true),
            Some(CacheTtl::OneHour),
            "no in-overage value keeps the rules"
        );
        assert_eq!(
            CacheTtlPolicy::default().ttl_for("claude-opus-5", true, true),
            None
        );
    }

    #[test]
    fn a_models_override_written_as_the_undated_id_keeps_applying_once_the_canonical_id_is_dated() {
        let policy = CacheTtlPolicy::parse(&serde_json::json!({ "enabled": true, "models": [{ "model": "claude-sonnet-4-5", "main": "5m", "subagent": "5m" }] })).unwrap();
        assert_eq!(
            policy.ttl_for("claude-sonnet-4-5-20250929", true, false),
            Some(CacheTtl::FiveMinutes),
            "the override written as the undated id still finds the dated one"
        );
        assert_eq!(
            policy.ttl_for("claude-sonnet-4-5", true, false),
            Some(CacheTtl::FiveMinutes),
            "the undated spelling itself still matches directly"
        );
        assert_eq!(
            policy.ttl_for("claude-x-2025", true, false),
            Some(CacheTtl::OneHour),
            "a four-digit suffix is not a date, so no fallback and no match"
        );
    }
}
