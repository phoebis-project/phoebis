//! The version guard: a client other than the pinned one is relayed and tagged, and each new version is logged
//! once so an update anywhere shows up before its requests could diverge.

use std::collections::HashSet;
use std::sync::Mutex;

use axum::http::{header, HeaderMap};
use phoebis_profile::{HeaderSlot, Profile};

const MAX_LOGGED: usize = 64;

pub struct DriftGuard {
    app: &'static str,
    profile: &'static Profile,
    seen: Mutex<HashSet<String>>,
}

impl DriftGuard {
    pub fn new(profile: &'static Profile) -> DriftGuard {
        let app = profile
            .inference_header_order
            .iter()
            .find_map(|s| match s {
                HeaderSlot::Fixed("x-app", v) => Some(*v),
                _ => None,
            })
            .expect("the pinned profile always carries a fixed x-app header slot");
        DriftGuard {
            app,
            profile,
            seen: Mutex::new(HashSet::new()),
        }
    }

    pub fn check(&self, headers: &HeaderMap) -> Option<String> {
        let agent = headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())?;
        let version = agent
            .strip_prefix("claude-cli/")?
            .split(' ')
            .next()?
            .to_string();
        let app = headers
            .get("x-app")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(self.app);
        if version == self.profile.cli_version && app == self.app {
            return None;
        }
        let tag = if app == self.app {
            version
        } else {
            format!("{version}/{app}")
        };
        let mut seen = self.seen.lock().expect("drift set poisoned");
        if !seen.contains(&tag) && seen.len() < MAX_LOGGED {
            seen.insert(tag.clone());
            tracing::warn!(version = %tag, pinned = self.profile.cli_version, "a client version other than the pinned one is being relayed");
        }
        Some(tag)
    }
}

#[cfg(test)]
mod tests {
    use super::DriftGuard;
    use axum::http::HeaderMap;

    #[test]
    fn a_foreign_version_is_tagged_and_the_pinned_one_is_not() {
        let guard = DriftGuard::new(phoebis_profile::claude_code_2_1_269_linux_x64());
        let mut pinned = HeaderMap::new();
        pinned.insert(
            "user-agent",
            "claude-cli/2.1.269 (external, cli)".parse().unwrap(),
        );
        pinned.insert("x-app", "cli".parse().unwrap());
        assert_eq!(guard.check(&pinned), None);
        let mut newer = HeaderMap::new();
        newer.insert(
            "user-agent",
            "claude-cli/2.1.300 (external, cli)".parse().unwrap(),
        );
        newer.insert("x-app", "cli".parse().unwrap());
        assert_eq!(guard.check(&newer).as_deref(), Some("2.1.300"));
        let mut app = HeaderMap::new();
        app.insert(
            "user-agent",
            "claude-cli/2.1.269 (external, cli)".parse().unwrap(),
        );
        app.insert("x-app", "desktop".parse().unwrap());
        assert_eq!(guard.check(&app).as_deref(), Some("2.1.269/desktop"));
        assert_eq!(
            guard.check(&HeaderMap::new()),
            None,
            "a non-native caller without the headers is not drift"
        );
    }
}
