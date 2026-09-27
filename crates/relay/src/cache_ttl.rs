//! The per-pool cache TTL policy: two rule sets, `plan` and `metered`, each carrying the TTL a keepalive probe,
//! a main turn and a subagent turn take, and the per-model overrides on the two turn kinds.

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

/// One rule set: the TTL by branch, with per-model overrides on `main` and `subagent`. Serialized whole; read
/// through `complete`, never through a derived `Deserialize` with one shared default, so a metered probe can
/// never inherit an hour meant for the plan set.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct TtlRules {
    pub keepalive: CacheTtl,
    pub main: CacheTtl,
    /// Per-model overrides on the canonical model id; a model absent here takes `main` and `subagent`.
    pub models: Vec<ModelTtl>,
    pub subagent: CacheTtl,
}

impl TtlRules {
    pub fn plan_default() -> TtlRules {
        TtlRules {
            keepalive: CacheTtl::OneHour,
            main: CacheTtl::OneHour,
            models: Vec::new(),
            subagent: CacheTtl::OneHour,
        }
    }

    pub fn metered_default() -> TtlRules {
        TtlRules {
            keepalive: CacheTtl::FiveMinutes,
            main: CacheTtl::FiveMinutes,
            models: Vec::new(),
            subagent: CacheTtl::FiveMinutes,
        }
    }

    /// A partial set (every field optional, unknown fields refused) completed against `base`.
    pub fn complete(partial: PartialTtlRules, base: &TtlRules) -> TtlRules {
        TtlRules {
            keepalive: partial.keepalive.unwrap_or(base.keepalive),
            main: partial.main.unwrap_or(base.main),
            models: partial.models.unwrap_or_else(|| base.models.clone()),
            subagent: partial.subagent.unwrap_or(base.subagent),
        }
    }

    /// The TTL for `branch`: the per-model override first for `Main` and `Subagent`, an override written under
    /// an undated spelling still found once the canonical id is dated.
    fn ttl(&self, canonical_model: &str, branch: TtlBranch) -> CacheTtl {
        if branch == TtlBranch::Keepalive {
            return self.keepalive;
        }
        let over = self
            .models
            .iter()
            .find(|m| m.model == canonical_model)
            .or_else(|| {
                phoebis_routing::listing::undated(canonical_model)
                    .and_then(|base| self.models.iter().find(|m| m.model == base))
            });
        match (branch, over) {
            (TtlBranch::Subagent, Some(m)) => m.subagent,
            (_, Some(m)) => m.main,
            (TtlBranch::Subagent, None) => self.subagent,
            (_, None) => self.main,
        }
    }
}

/// The wire shape of a rule set: what a stored row or a PATCH may omit.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PartialTtlRules {
    pub keepalive: Option<CacheTtl>,
    pub main: Option<CacheTtl>,
    pub models: Option<Vec<ModelTtl>>,
    pub subagent: Option<CacheTtl>,
}

/// Which TTL of a rule set a request takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TtlBranch {
    Keepalive,
    Main,
    Subagent,
}

#[derive(Default, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawPolicy {
    enabled: bool,
    inject_when_absent: bool,
    metered: Option<PartialTtlRules>,
    plan: Option<PartialTtlRules>,
}

/// A pool's cache TTL policy, stored as JSON in `pools.cache_ttl_json`. Deserialized by hand through `RawPolicy`
/// so each set is completed against its own baseline rather than a shared default that could hand a metered
/// probe an hour.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct CacheTtlPolicy {
    pub enabled: bool,
    /// Whether the rewrite adds a top-level `cache_control` to a body that carries no marker.
    pub inject_when_absent: bool,
    /// The rules while a write costs money: an api_key account, or a subscription account in overage. A
    /// one-hour write costs a metered account 2x base input against 1.25x for five minutes, and a plan-funded
    /// one the same plan tokens either way, so the metered default is five minutes while the plan side keeps
    /// the hour.
    pub metered: TtlRules,
    pub plan: TtlRules,
}

impl Default for CacheTtlPolicy {
    fn default() -> Self {
        CacheTtlPolicy {
            enabled: false,
            inject_when_absent: false,
            metered: TtlRules::metered_default(),
            plan: TtlRules::plan_default(),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CacheTtlError {
    #[error("{0} lists {1} twice")]
    DuplicateModel(String, String),
    #[error("{0}[].model must be a model id")]
    EmptyModel(String),
    #[error("cache_ttl is not a policy object: {0}")]
    Shape(String),
}

fn check_models(models: &[ModelTtl], set: &str) -> Result<(), CacheTtlError> {
    let mut seen = std::collections::BTreeSet::new();
    for entry in models {
        if entry.model.is_empty() {
            return Err(CacheTtlError::EmptyModel(set.to_string()));
        }
        if !seen.insert(entry.model.as_str()) {
            return Err(CacheTtlError::DuplicateModel(
                set.to_string(),
                entry.model.clone(),
            ));
        }
    }
    Ok(())
}

impl CacheTtlPolicy {
    /// An object, known fields only, each set completed against its own default, canonical model ids listed
    /// once per set.
    pub fn parse(raw: &serde_json::Value) -> Result<CacheTtlPolicy, CacheTtlError> {
        // As `KeepalivePolicy::parse`: the container-level default lets an array deserialize element by element.
        if !raw.is_object() {
            return Err(CacheTtlError::Shape(
                "cache_ttl policy must be a JSON object".into(),
            ));
        }
        let raw: RawPolicy =
            serde_json::from_value(raw.clone()).map_err(|e| CacheTtlError::Shape(e.to_string()))?;
        let plan = TtlRules::complete(raw.plan.unwrap_or_default(), &TtlRules::plan_default());
        let metered = TtlRules::complete(
            raw.metered.unwrap_or_default(),
            &TtlRules::metered_default(),
        );
        check_models(&plan.models, "cache_ttl.plan.models")?;
        check_models(&metered.models, "cache_ttl.metered.models")?;
        Ok(CacheTtlPolicy {
            enabled: raw.enabled,
            inject_when_absent: raw.inject_when_absent,
            metered,
            plan,
        })
    }

    /// What a stored row reads as, through the same completion `parse` uses; anything unreadable reads as the
    /// default (off).
    pub fn from_json(raw: &str) -> CacheTtlPolicy {
        serde_json::from_str::<serde_json::Value>(raw)
            .ok()
            .and_then(|v| CacheTtlPolicy::parse(&v).ok())
            .unwrap_or_default()
    }

    /// `None` while the policy is off; else the branch's TTL from the set `metered` selects, the per-model
    /// override first for `Main` and `Subagent`.
    pub fn ttl_for(
        &self,
        canonical_model: &str,
        branch: TtlBranch,
        metered: bool,
    ) -> Option<CacheTtl> {
        self.enabled.then(|| {
            let set = if metered { &self.metered } else { &self.plan };
            set.ttl(canonical_model, branch)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_policy_parses_with_defaults_and_refuses_an_unknown_field_and_a_duplicate_model() {
        let policy = CacheTtlPolicy::parse(&serde_json::json!({ "enabled": true })).unwrap();
        assert_eq!(policy.plan, TtlRules::plan_default());
        assert_eq!(policy.metered, TtlRules::metered_default());
        assert!(matches!(
            CacheTtlPolicy::parse(&serde_json::json!({ "plan": { "main": "2h" } })),
            Err(CacheTtlError::Shape(_))
        ));
        assert!(
            matches!(
                CacheTtlPolicy::parse(&serde_json::json!({ "strategy": "x" })),
                Err(CacheTtlError::Shape(_))
            ),
            "a field this build does not know is refused, so a later strategy cannot be silently dropped"
        );
        let twice = serde_json::json!({ "plan": { "models": [
            { "model": "claude-haiku-4-5-20251001", "main": "5m", "subagent": "5m" },
            { "model": "claude-haiku-4-5-20251001", "main": "1h", "subagent": "1h" },
        ] } });
        assert_eq!(
            CacheTtlPolicy::parse(&twice),
            Err(CacheTtlError::DuplicateModel(
                "cache_ttl.plan.models".into(),
                "claude-haiku-4-5-20251001".into()
            ))
        );
        assert_eq!(
            CacheTtlPolicy::parse(&serde_json::json!({
                "metered": { "models": [{ "model": "", "main": "5m", "subagent": "5m" }] }
            })),
            Err(CacheTtlError::EmptyModel("cache_ttl.metered.models".into()))
        );
        assert_eq!(
            CacheTtlPolicy::from_json("not json"),
            CacheTtlPolicy::default()
        );
    }

    #[test]
    fn a_partial_rule_set_fills_its_omitted_fields_from_its_own_defaults() {
        for reparse in [true, false] {
            let read = |v: &serde_json::Value| {
                if reparse {
                    CacheTtlPolicy::from_json(&v.to_string())
                } else {
                    CacheTtlPolicy::parse(v).unwrap()
                }
            };
            let metered_only = read(&serde_json::json!({ "metered": { "main": "5m" } }));
            assert_eq!(
                metered_only.metered,
                TtlRules {
                    keepalive: CacheTtl::FiveMinutes,
                    main: CacheTtl::FiveMinutes,
                    models: Vec::new(),
                    subagent: CacheTtl::FiveMinutes,
                },
                "reparse={reparse}"
            );
            assert_eq!(
                metered_only.plan,
                TtlRules::plan_default(),
                "reparse={reparse}"
            );

            let plan_only = read(&serde_json::json!({ "plan": { "subagent": "1h" } }));
            assert_eq!(
                plan_only.plan,
                TtlRules {
                    keepalive: CacheTtl::OneHour,
                    main: CacheTtl::OneHour,
                    models: Vec::new(),
                    subagent: CacheTtl::OneHour,
                },
                "reparse={reparse}"
            );
            assert_eq!(
                plan_only.metered,
                TtlRules::metered_default(),
                "reparse={reparse}"
            );
        }
    }

    #[test]
    fn the_metered_set_prices_a_money_write_and_the_plan_set_the_rest() {
        let policy = CacheTtlPolicy::parse(&serde_json::json!({
            "enabled": true,
            "plan": { "main": "1h", "subagent": "5m", "models": [{ "model": "claude-haiku-4-5-20251001", "main": "1h", "subagent": "1h" }] },
            "metered": { "main": "5m", "subagent": "1h" },
        }))
        .unwrap();
        assert_eq!(
            policy.ttl_for("claude-opus-5", TtlBranch::Main, false),
            Some(CacheTtl::OneHour)
        );
        assert_eq!(
            policy.ttl_for("claude-opus-5", TtlBranch::Subagent, false),
            Some(CacheTtl::FiveMinutes)
        );
        assert_eq!(
            policy.ttl_for("claude-opus-5", TtlBranch::Main, true),
            Some(CacheTtl::FiveMinutes),
            "a money write takes the metered set regardless of the plan override"
        );
        assert_eq!(
            policy.ttl_for("claude-haiku-4-5-20251001", TtlBranch::Main, true),
            Some(CacheTtl::FiveMinutes),
            "a per-model override on plan does not reach metered"
        );
        assert_eq!(
            policy.ttl_for("claude-opus-5", TtlBranch::Keepalive, false),
            Some(CacheTtl::OneHour),
            "a probe reads the plan set's own keepalive TTL"
        );
        assert_eq!(
            policy.ttl_for("claude-opus-5", TtlBranch::Keepalive, true),
            Some(CacheTtl::FiveMinutes),
            "a probe reads the metered set's own keepalive TTL"
        );
        assert_eq!(
            CacheTtlPolicy::default().ttl_for("claude-opus-5", TtlBranch::Main, true),
            None
        );
    }

    #[test]
    fn a_models_override_written_as_the_undated_id_keeps_applying_once_the_canonical_id_is_dated() {
        let policy = CacheTtlPolicy::parse(&serde_json::json!({
            "enabled": true,
            "plan": { "models": [{ "model": "claude-sonnet-4-5", "main": "5m", "subagent": "5m" }] },
        }))
        .unwrap();
        assert_eq!(
            policy.ttl_for("claude-sonnet-4-5-20250929", TtlBranch::Main, false),
            Some(CacheTtl::FiveMinutes),
            "the override written as the undated id still finds the dated one"
        );
        assert_eq!(
            policy.ttl_for("claude-sonnet-4-5", TtlBranch::Main, false),
            Some(CacheTtl::FiveMinutes),
            "the undated spelling itself still matches directly"
        );
        assert_eq!(
            policy.ttl_for("claude-x-2025", TtlBranch::Main, false),
            Some(CacheTtl::OneHour),
            "a four-digit suffix is not a date, so no fallback and no match"
        );
    }
}
