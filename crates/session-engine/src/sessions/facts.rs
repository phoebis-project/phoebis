//! The facts a companion mod reports of its session and of each agent, and the one place each value is cleaned. A
//! value comes from another program, and the admin API, the console and `status --json` print it, so a value enters a
//! record only through the rules here: no control or format character, no credential of a remote, and a bound on its
//! size.

use unicode_general_category::{get_general_category, GeneralCategory};

/// The cap of a branch, a description, a name, a title or a type, in bytes of UTF-8.
const SHORT_CAP: usize = 256;
/// The cap of a path or a remote, in bytes of UTF-8.
const LONG_CAP: usize = 1_024;

/// A session's facts as a report or a saved row gives them, ahead of this module's rules.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize)]
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
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub(crate) struct RawAgentFacts {
    pub(crate) agent_type: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) name: Option<String>,
}

/// The facts a companion reports of its session. Display state alone. It wraps the raw form, and each value in it
/// passed this module's rules in `of`.
/// A value enters through `of` alone, a saved one too, so the type takes no `Deserialize`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionFacts(RawSessionFacts);

impl SessionFacts {
    /// The one way a value becomes a kept one. The conversion of a report and the read of a stored set call it.
    pub(crate) fn of(raw: RawSessionFacts) -> SessionFacts {
        SessionFacts(RawSessionFacts {
            branch: whole(raw.branch, SHORT_CAP),
            cwd: whole(raw.cwd, LONG_CAP),
            project: whole(raw.project, LONG_CAP),
            // The cap counts the reduced remote, so a long raw URL with a short result is kept.
            repository: text(raw.repository)
                .and_then(|remote| reduce(&remote))
                .filter(|remote| remote.len() <= LONG_CAP),
            repository_root: whole(raw.repository_root, LONG_CAP),
            title: cut(raw.title, SHORT_CAP),
        })
    }

    /// The stored value of the kind `project`: a JSON object of the branch, the working directory, the project, the
    /// repository and its root under the raw form's names, an absent one as `null`, and `None` where none of the five
    /// stands. The title stays out, since it has a row of its own.
    pub(crate) fn project_value(&self) -> Option<Vec<u8>> {
        // Whole, so a member that joins the raw form takes a decision here.
        let RawSessionFacts {
            branch,
            cwd,
            project,
            repository,
            repository_root,
            title: _,
        } = &self.0;
        if [branch, cwd, project, repository, repository_root]
            .iter()
            .all(|member| member.is_none())
        {
            return None;
        }
        serde_json::to_vec(&serde_json::json!({
            "branch": branch,
            "cwd": cwd,
            "project": project,
            "repository": repository,
            "repository_root": repository_root,
        }))
        .ok()
    }

    /// The stored value of the kind `title`: a JSON object with the one member `title`, `None` with no title.
    pub(crate) fn title_value(&self) -> Option<Vec<u8>> {
        let title = self.0.title.as_ref()?;
        serde_json::to_vec(&serde_json::json!({ "title": title })).ok()
    }

    pub fn branch(&self) -> Option<&str> {
        self.0.branch.as_deref()
    }

    pub fn cwd(&self) -> Option<&str> {
        self.0.cwd.as_deref()
    }

    pub fn project(&self) -> Option<&str> {
        self.0.project.as_deref()
    }

    pub fn repository(&self) -> Option<&str> {
        self.0.repository.as_deref()
    }

    pub fn repository_root(&self) -> Option<&str> {
        self.0.repository_root.as_deref()
    }

    pub fn title(&self) -> Option<&str> {
        self.0.title.as_deref()
    }
}

/// The facts a companion reports of one agent, kept as `SessionFacts` keeps a session's.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentFacts(RawAgentFacts);

impl AgentFacts {
    pub(crate) fn of(raw: RawAgentFacts) -> AgentFacts {
        AgentFacts(RawAgentFacts {
            agent_type: whole(raw.agent_type, SHORT_CAP),
            description: cut(raw.description, SHORT_CAP),
            name: cut(raw.name, SHORT_CAP),
        })
    }

    /// The stored value of the kind `agents`: the raw form's JSON object, `None` where no member stands.
    pub(crate) fn value(&self) -> Option<Vec<u8>> {
        (!self.is_empty())
            .then(|| serde_json::to_vec(&self.0).ok())
            .flatten()
    }

    pub fn agent_type(&self) -> Option<&str> {
        self.0.agent_type.as_deref()
    }

    pub fn description(&self) -> Option<&str> {
        self.0.description.as_deref()
    }

    pub fn name(&self) -> Option<&str> {
        self.0.name.as_deref()
    }

    /// No member stands, so the view lists no entry for the agent.
    pub(crate) fn is_empty(&self) -> bool {
        self.0.agent_type.is_none() && self.0.description.is_none() && self.0.name.is_none()
    }
}

/// The stored facts of one session: its own, and one entry per agent that the registry holds a set of, in agent id
/// order. Each value passed this module's rules again on its way out of the registry.
#[derive(Debug, Default)]
pub struct StoredSessionFacts {
    pub agents: Vec<(String, AgentFacts)>,
    pub session: SessionFacts,
}

/// The text rule every value passes first: each run of line breaks and tabs folds to one space, each other control and
/// format character goes, white space at both ends goes, and a value left empty is absent.
fn text(value: Option<String>) -> Option<String> {
    let value = value?;
    let mut kept = String::with_capacity(value.len());
    let mut in_run = false;
    for c in value.chars() {
        if matches!(c, '\u{9}'..='\u{d}' | '\u{85}' | '\u{2028}' | '\u{2029}') {
            if !in_run {
                kept.push(' ');
            }
            in_run = true;
        } else {
            // `char::is_control` reads Cc alone, and the bidirectional overrides, the isolates and the zero-width
            // characters are Cf.
            if !matches!(
                get_general_category(c),
                GeneralCategory::Control | GeneralCategory::Format
            ) {
                kept.push(c);
            }
            in_run = false;
        }
    }
    let kept = kept.trim();
    (!kept.is_empty()).then(|| kept.to_owned())
}

/// A branch, a path or a type is absent over its cap, since a cut one would name another branch, directory or type.
fn whole(value: Option<String>, cap: usize) -> Option<String> {
    text(value).filter(|value| value.len() <= cap)
}

/// A description, a name or a title over its cap keeps the longest prefix of whole characters within it.
fn cut(value: Option<String>, cap: usize) -> Option<String> {
    let value = text(value)?;
    if value.len() <= cap {
        return Some(value);
    }
    let end = value.floor_char_boundary(cap);
    Some(value[..end].trim_end().to_owned())
}

/// A remote as `host/path`, without the scheme, the user information, the port, the query, the fragment and one
/// trailing `.git`, or absent where the value names a local path or a git helper address or the rule cannot read it.
fn reduce(remote: &str) -> Option<String> {
    if remote
        .get(..7)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("file://"))
    {
        return None;
    }
    let reduced = if let Some((scheme, rest)) = remote.split_once("://") {
        // The other two forms hold no `://`, so one behind anything but a scheme follows a drive letter, a query, a
        // directory name or a git helper's transport.
        if !is_scheme(scheme) {
            return None;
        }
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
        // One letter ahead of the colon is a Windows drive, so the value is a local path. A second colon makes the
        // value a git helper address, `transport::address`, whose address can be a local path.
        let drive = matches!(side.as_bytes(), [letter] if letter.is_ascii_alphabetic());
        if drive || path.starts_with(':') || path.contains('@') {
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

/// An ASCII letter and then one or more ASCII letters, digits, `+`, `-` or `.`. One letter alone is a Windows drive.
fn is_scheme(value: &str) -> bool {
    let mut bytes = value.bytes();
    value.len() > 1
        && bytes.next().is_some_and(|b| b.is_ascii_alphabetic())
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
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

/// The last check: a result is kept only where it reads as `host/path` itself, so a credential, a local path or a port
/// that is no number never stands as a repository. A cut can leave white space other than U+0020 at an end, such as a
/// no-break space ahead of `.git`, and the text rule would trim it at the next load and change a kept value, so such a
/// result is absent.
fn reads_reduced(value: &str) -> bool {
    // A `[` that no `]` closes ahead of the first `/` would hide each colon behind it from the colon test.
    let host_side = value.find('/').map_or(value, |at| &value[..at]);
    let closed = host_side
        .rfind('[')
        .is_none_or(|open| host_side[open..].contains(']'));
    !value.contains(['@', '?', '#', '\\', ' '])
        && !value.starts_with(char::is_whitespace)
        && !value.ends_with(char::is_whitespace)
        && !value.contains("://")
        && !value.starts_with(['/', '.', '~'])
        && host_colon(value).is_none()
        && closed
}
