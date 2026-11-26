//! The hijack guard: what an observed request does to the agent's recipe.

/// What an observation does to the agent's recipe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Guard {
    Auxiliary,
    Compaction,
    Foreign,
    Rearm,
    Restart,
}

/// The guard of a request that names its class: one answer for `guard`, the chain mark and the prebuild, so a class
/// added to `REFRESHING_CLASSES` reaches all three.
pub(crate) fn class_guard(class: &str) -> Guard {
    // A compaction reads the old prefix to write a context the next turn starts from, so a recipe armed from it
    // would keep warm a body no turn reads again.
    if class == "compaction" {
        Guard::Compaction
    } else if phoebis_routing::REFRESHING_CLASSES.contains(&class) {
        Guard::Rearm
    } else {
        Guard::Auxiliary
    }
}

/// The hijack guard: a class names the turn directly. Without one, the request's own chain against the armed
/// recipe's two hashes decides, the same fallback the routing crate's `warmth_of` applies when a request
/// carries no chain at all. `recipe` is the armed recipe's `(static_hash, tail_hash)`, `None` for a first
/// observation with nothing to compare against.
pub fn guard(
    request_class: Option<&str>,
    static_hash: Option<&str>,
    boundaries: &[String],
    recipe: Option<(&str, &str)>,
    model_matches: bool,
) -> Guard {
    if let Some(class) = request_class {
        return class_guard(class);
    }
    let Some((recipe_static, recipe_tail)) = recipe else {
        return Guard::Rearm;
    };
    if static_hash.is_none() && boundaries.is_empty() {
        return Guard::Rearm;
    }
    if boundaries.iter().any(|boundary| boundary == recipe_tail) {
        return Guard::Rearm;
    }
    if static_hash == Some(recipe_static) && model_matches {
        return Guard::Restart;
    }
    Guard::Foreign
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refreshing_class_rearms_whatever_the_chain_says() {
        assert_eq!(
            guard(Some("subagent"), None, &[], Some(("s", "t")), false),
            Guard::Rearm
        );
    }

    #[test]
    fn a_compaction_answers_its_own_verdict_whatever_the_chain_says() {
        let continuing = ["t".to_string()];
        let fresh = ["fresh".to_string()];
        for (static_hash, boundaries, recipe, model_matches) in [
            (None, &[][..], Some(("s", "t")), false),
            (None, &[][..], None, false),
            (Some("s"), &continuing[..], Some(("s", "t")), true),
            (Some("s"), &fresh[..], Some(("s", "t")), true),
        ] {
            assert_eq!(
                guard(
                    Some("compaction"),
                    static_hash,
                    boundaries,
                    recipe,
                    model_matches
                ),
                Guard::Compaction,
                "{static_hash:?} {boundaries:?} {recipe:?}"
            );
        }
    }

    #[test]
    fn any_other_class_is_auxiliary() {
        assert_eq!(
            guard(Some("title"), None, &[], Some(("s", "t")), false),
            Guard::Auxiliary
        );
    }

    #[test]
    fn a_first_observation_with_no_recipe_rearms_whatever_it_says() {
        assert_eq!(
            guard(None, Some("s"), &["t".to_string()], None, false),
            Guard::Rearm
        );
    }

    #[test]
    fn a_body_without_a_chain_rearms_every_request() {
        assert_eq!(
            guard(None, None, &[], Some(("s", "t")), false),
            Guard::Rearm
        );
    }

    #[test]
    fn boundaries_holding_the_recipes_tail_rearm() {
        assert_eq!(
            guard(
                None,
                Some("other"),
                &["a".into(), "t".into()],
                Some(("s", "t")),
                false
            ),
            Guard::Rearm
        );
    }

    #[test]
    fn the_same_static_prefix_and_model_without_a_continuation_restarts() {
        assert_eq!(
            guard(None, Some("s"), &["fresh".into()], Some(("s", "t")), true),
            Guard::Restart
        );
    }

    #[test]
    fn the_same_static_prefix_on_another_model_is_foreign_not_a_restart() {
        assert_eq!(
            guard(None, Some("s"), &["fresh".into()], Some(("s", "t")), false),
            Guard::Foreign
        );
    }

    #[test]
    fn neither_the_chain_nor_the_static_prefix_matching_is_foreign() {
        assert_eq!(
            guard(
                None,
                Some("other"),
                &["fresh".into()],
                Some(("s", "t")),
                false
            ),
            Guard::Foreign
        );
    }
}
