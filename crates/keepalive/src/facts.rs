//! The facts a companion mod reports of its session and of each agent, and the one place each value is cleaned. A
//! value comes from another program, and the console, the admin API and `status --json` print it, so a value enters a
//! record only through the rules here: no control or format character, no credential of a remote, and a bound on its
//! size.

use unicode_general_category::{get_general_category, GeneralCategory};

/// The cap of a name, a title, a type or a branch, in bytes of UTF-8.
const SHORT_CAP: usize = 256;
/// The cap of a path or a remote, in bytes of UTF-8.
const LONG_CAP: usize = 1_024;

/// A session's facts as a report or a saved row gives them, ahead of this module's rules.
#[derive(Default, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub(crate) struct RawSessionFacts {
    pub(crate) branch: Option<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) project: Option<String>,
    pub(crate) repository: Option<String>,
    pub(crate) repository_root: Option<String>,
    pub(crate) title: Option<String>,
}

/// An agent's facts in the same form.
#[derive(Default, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub(crate) struct RawAgentFacts {
    pub(crate) agent_type: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) name: Option<String>,
}

/// The facts a companion reports of its session, each cleaned by this module's rules. Display state alone.
/// A value enters through `of` alone, a saved one too, so the type takes no `Deserialize`. It serializes because
/// `SessionView` does.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct SessionFacts {
    branch: Option<String>,
    cwd: Option<String>,
    project: Option<String>,
    repository: Option<String>,
    repository_root: Option<String>,
    title: Option<String>,
}

impl SessionFacts {
    /// The one way a value becomes a kept one. The conversion of a report and the load of a saved row call it.
    pub(crate) fn of(raw: RawSessionFacts) -> SessionFacts {
        SessionFacts {
            branch: whole(raw.branch, SHORT_CAP),
            cwd: whole(raw.cwd, LONG_CAP),
            project: whole(raw.project, LONG_CAP),
            // The cap counts the reduced remote, so a long raw URL with a short result is kept.
            repository: text(raw.repository)
                .and_then(|remote| reduce(&remote))
                .filter(|remote| remote.len() <= LONG_CAP),
            repository_root: whole(raw.repository_root, LONG_CAP),
            title: cut(raw.title, SHORT_CAP),
        }
    }

    /// The set as a saved row carries it.
    pub(crate) fn raw(&self) -> RawSessionFacts {
        RawSessionFacts {
            branch: self.branch.clone(),
            cwd: self.cwd.clone(),
            project: self.project.clone(),
            repository: self.repository.clone(),
            repository_root: self.repository_root.clone(),
            title: self.title.clone(),
        }
    }

    pub fn branch(&self) -> Option<&str> {
        self.branch.as_deref()
    }

    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    pub fn project(&self) -> Option<&str> {
        self.project.as_deref()
    }

    pub fn repository(&self) -> Option<&str> {
        self.repository.as_deref()
    }

    pub fn repository_root(&self) -> Option<&str> {
        self.repository_root.as_deref()
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }
}

/// The facts a companion reports of one agent.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct AgentFacts {
    agent_type: Option<String>,
    description: Option<String>,
    name: Option<String>,
}

impl AgentFacts {
    pub(crate) fn of(raw: RawAgentFacts) -> AgentFacts {
        AgentFacts {
            agent_type: whole(raw.agent_type, SHORT_CAP),
            description: cut(raw.description, SHORT_CAP),
            name: cut(raw.name, SHORT_CAP),
        }
    }

    pub(crate) fn raw(&self) -> RawAgentFacts {
        RawAgentFacts {
            agent_type: self.agent_type.clone(),
            description: self.description.clone(),
            name: self.name.clone(),
        }
    }

    pub fn agent_type(&self) -> Option<&str> {
        self.agent_type.as_deref()
    }

    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// No member stands, so the view lists no entry for the agent.
    pub fn is_empty(&self) -> bool {
        self.agent_type.is_none() && self.description.is_none() && self.name.is_none()
    }
}

/// The text rule every value passes first: each run of line breaks and tabs folds to one space, each other control and
/// format character goes, white space at both ends goes, and a value left empty is absent.
fn text(value: Option<String>) -> Option<String> {
    let value = value?;
    let mut folded = String::with_capacity(value.len());
    let mut in_run = false;
    for c in value.chars() {
        if matches!(c, '\u{9}'..='\u{d}' | '\u{85}' | '\u{2028}' | '\u{2029}') {
            if !in_run {
                folded.push(' ');
            }
            in_run = true;
        } else {
            folded.push(c);
            in_run = false;
        }
    }
    // `char::is_control` reads Cc alone, and the bidirectional overrides, the isolates and the zero-width characters
    // are Cf.
    let kept: String = folded
        .chars()
        .filter(|c| {
            !matches!(
                get_general_category(*c),
                GeneralCategory::Control | GeneralCategory::Format
            )
        })
        .collect();
    let kept = kept.trim();
    (!kept.is_empty()).then(|| kept.to_owned())
}

/// A path, a branch or a type is absent over its cap, since a cut one would name another directory, branch or type.
fn whole(value: Option<String>, cap: usize) -> Option<String> {
    text(value).filter(|value| value.len() <= cap)
}

/// A name, a description or a title over its cap keeps the longest prefix of whole characters within it.
fn cut(value: Option<String>, cap: usize) -> Option<String> {
    let value = text(value)?;
    if value.len() <= cap {
        return Some(value);
    }
    let end = value
        .char_indices()
        .map(|(at, c)| at + c.len_utf8())
        .take_while(|end| *end <= cap)
        .last()
        .unwrap_or(0);
    Some(value[..end].trim_end().to_owned())
}

/// A remote as `host/path`, without the scheme, the user information, the port, the query, the fragment and one
/// trailing `.git`, or absent where the value names a local path or the rule cannot read it.
fn reduce(remote: &str) -> Option<String> {
    if remote
        .get(..7)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("file://"))
    {
        return None;
    }
    let reduced = if let Some((_, rest)) = remote.split_once("://") {
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, path) = rest.split_at(end);
        // A raw `/`, `?` or `#` in the user information ends the authority early, so an `@` behind it is the tail of a
        // credential that the cuts below would hide from the last check.
        if path.contains('@') {
            return None;
        }
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        // A port that is no number stays in the host, where the last check refuses it.
        let host = host
            .trim_end_matches(|c: char| c.is_ascii_digit())
            .strip_suffix(':')
            .unwrap_or(host);
        joined(host, path)?
    } else if let Some(colon) = host_colon(remote) {
        let (side, path) = (&remote[..colon], &remote[colon + 1..]);
        // One letter ahead of the colon is a Windows drive, so the value is a local path.
        let drive = matches!(side.as_bytes(), [letter] if letter.is_ascii_alphabetic());
        if drive || path.contains('@') {
            return None;
        }
        joined(side.rsplit_once('@').map_or(side, |(_, host)| host), path)?
    } else {
        // The form the companion sends, kept as given: a second pass that cut it again would strip a second `.git`
        // and put two repositories into one group.
        let (host, path) = remote.split_once('/')?;
        if host.is_empty() || path.is_empty() {
            return None;
        }
        remote.to_owned()
    };
    reads_reduced(&reduced).then_some(reduced)
}

/// The host in ASCII lower case, one `/` and the path without its query, its fragment, its outer slashes and one
/// trailing `.git`.
fn joined(host: &str, path: &str) -> Option<String> {
    let path = path.find(['?', '#']).map_or(path, |at| &path[..at]);
    let path = path.trim_start_matches('/').trim_end_matches('/');
    let path = path
        .strip_suffix(".git")
        .unwrap_or(path)
        .trim_end_matches('/');
    if host.is_empty() || path.is_empty() {
        return None;
    }
    Some(format!("{}/{path}", host.to_ascii_lowercase()))
}

/// The first `:` outside square brackets where it stands ahead of the value's first `/`, which ends a host's side. A
/// bracketed IPv6 host holds colons of its own.
fn host_colon(value: &str) -> Option<usize> {
    let mut bracketed = false;
    for (at, c) in value.char_indices() {
        match c {
            '[' => bracketed = true,
            ']' => bracketed = false,
            ':' if !bracketed => return Some(at),
            '/' => return None,
            _ => {}
        }
    }
    None
}

/// The last check: a result is kept only where it reads as `host/path` itself, so a port that is no number, a
/// credential or a local path never stands as a repository.
fn reads_reduced(value: &str) -> bool {
    !value.contains(['@', '?', '#', '\\', ' '])
        && !value.contains("://")
        && !value.starts_with(['/', '.', '~'])
        && host_colon(value).is_none()
}
