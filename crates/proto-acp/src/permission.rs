//! `PermissionBroker` and capability-based security policy (ARCHITECTURE.md §23.2).
//!
//! Evaluates permission requests for agents, MCP servers and extensions.
//! Decisions can have a TTL of `Once`, `Session` (in-memory) or `Always`,
//! persisted in the **user's** config directory keyed by workspace — never
//! inside the workspace, where a cloned repository could ship grants. An
//! audit log records every decision.

use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// Target subject governed by a permission policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "name")]
pub enum PermissionSubject {
    Extension(String),
    Agent(String),
    McpServer(String),
    Task(String),
    Any,
}

impl PermissionSubject {
    #[must_use]
    pub fn matches_agent(&self, agent_name: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Agent(name) => name.eq_ignore_ascii_case(agent_name),
            _ => false,
        }
    }
}

/// Capability requested by an actor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "name")]
pub enum PermissionCapability {
    FsRead,
    FsWrite,
    ProcessSpawn,
    Net,
    Terminal,
    Clipboard,
    Secrets,
    Tool(String),
    Any,
}

impl PermissionCapability {
    #[must_use]
    pub fn matches(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Any, _)
            | (_, Self::Any)
            | (Self::FsRead, Self::FsRead)
            | (Self::FsWrite, Self::FsWrite)
            | (Self::ProcessSpawn, Self::ProcessSpawn)
            | (Self::Net, Self::Net)
            | (Self::Terminal, Self::Terminal)
            | (Self::Clipboard, Self::Clipboard)
            | (Self::Secrets, Self::Secrets) => true,
            (Self::Tool(a), Self::Tool(b)) => a.eq_ignore_ascii_case(b),
            _ => false,
        }
    }

    /// Capability of an ACP tool call from its `kind` (`read`, `edit`,
    /// `delete`, `move`, `search`, `execute`, `fetch`, `think`, `other`),
    /// falling back to the tool name only when the agent sent no kind.
    #[must_use]
    pub fn from_tool_call(kind: Option<&str>, tool_name: &str) -> Self {
        match kind.map(str::to_ascii_lowercase).as_deref() {
            Some("read" | "search") => Self::FsRead,
            Some("edit" | "delete" | "move") => Self::FsWrite,
            Some("execute") => Self::ProcessSpawn,
            Some("fetch") => Self::Net,
            Some(_) => Self::Tool(tool_name.to_owned()),
            None => Self::from_tool_name(tool_name),
        }
    }

    /// Best guess from a tool name, for agents that omit `kind`.
    #[must_use]
    pub fn from_tool_name(tool_name: &str) -> Self {
        let name = tool_name.to_ascii_lowercase();
        if name.starts_with("fs/read") || name.contains("read_file") {
            Self::FsRead
        } else if name.starts_with("fs/write")
            || name.contains("write_file")
            || name.contains("edit")
        {
            Self::FsWrite
        } else if name.starts_with("terminal/") {
            Self::Terminal
        } else if matches!(
            name.as_str(),
            "bash"
                | "sh"
                | "shell"
                | "exec"
                | "execute"
                | "execute_command"
                | "run_command"
                | "run_shell"
        ) {
            Self::ProcessSpawn
        } else {
            Self::Tool(tool_name.to_owned())
        }
    }

    /// Capabilities whose remembered grants need a scope: a blanket
    /// "always allow" on running commands is never recorded.
    #[must_use]
    pub const fn needs_scope(&self) -> bool {
        matches!(
            self,
            Self::ProcessSpawn | Self::Terminal | Self::Net | Self::Secrets | Self::Any
        )
    }
}

/// Scope restricting where or how a capability can be exercised.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum PermissionScope {
    Glob(String),
    CommandPattern(String),
    Host(String),
    Path(PathBuf),
    Any,
}

impl PermissionScope {
    /// Whether `target` (a command line, path or host) falls under the
    /// scope. Empty patterns never match: a rule must say what it covers
    /// (`Any` is the explicit wildcard).
    #[must_use]
    pub fn matches(&self, target: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Glob(pattern) => !pattern.is_empty() && glob_matches(pattern, target),
            Self::CommandPattern(pattern) => command_matches(pattern, target),
            Self::Host(host) => !host.is_empty() && target.eq_ignore_ascii_case(host),
            Self::Path(path) => {
                let target_path = Path::new(target);
                !path.as_os_str().is_empty()
                    && (target_path.starts_with(path) || target_path == path)
            }
        }
    }
}

/// `*` matches any run of characters, `?` one character; everything else
/// is literal. Good enough for `src/**`, `*.rs` and `docs/*.md`.
fn glob_matches(pattern: &str, target: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let target: Vec<char> = target.chars().collect();
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < target.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == target[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            // Collapse `**` and remember where to backtrack to.
            while p < pattern.len() && pattern[p] == '*' {
                p += 1;
            }
            star = Some((p, t));
        } else if let Some((star_p, star_t)) = star {
            p = star_p;
            t = star_t + 1;
            star = Some((star_p, t));
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

/// Token-wise prefix: `cargo test` covers `cargo test --workspace` but not
/// `cargo test-x`, and `git` covers `git status` but not `gitfoo`. A
/// trailing `*` token is accepted and ignored. Empty patterns never match.
fn command_matches(pattern: &str, target: &str) -> bool {
    let wanted: Vec<&str> = pattern
        .split_whitespace()
        .filter(|token| *token != "*")
        .collect();
    if wanted.is_empty() {
        return false;
    }
    let actual: Vec<&str> = target.split_whitespace().collect();
    wanted.len() <= actual.len() && wanted.iter().zip(&actual).all(|(w, a)| w == a)
}

/// Decision reached for a permission query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    Allow,
    Ask,
    Deny,
}

/// Time-to-live of a permission decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionTtl {
    Once,
    Session,
    Always,
}

/// A configured or remembered policy rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRule {
    pub subject: PermissionSubject,
    pub capability: PermissionCapability,
    pub scope: PermissionScope,
    pub decision: PermissionDecision,
    pub ttl: PermissionTtl,
}

/// Audit log record of a permission request and its outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionAuditEntry {
    pub timestamp_ms: u64,
    pub subject: String,
    pub capability: String,
    pub scope: String,
    pub decision: PermissionDecision,
    pub ttl: PermissionTtl,
}

/// Evaluates and persists security policies for Forge.
#[derive(Debug, Clone)]
pub struct PermissionBroker {
    /// Where `Always` rules for this workspace live; `None` keeps them in
    /// memory only (no config directory).
    store: Option<PathBuf>,
    persistent_rules: Vec<PermissionRule>,
    session_rules: Vec<PermissionRule>,
    audit_log: Vec<PermissionAuditEntry>,
}

impl PermissionBroker {
    /// A broker for `workspace` whose remembered grants are stored under
    /// `store_dir` (the user's config directory), one file per workspace.
    /// Nothing is ever read from the workspace itself.
    #[must_use]
    pub fn new(workspace: &Path, store_dir: Option<&Path>) -> Self {
        let store = store_dir.map(|dir| Self::store_path(dir, workspace));
        let persistent_rules = store
            .as_ref()
            .filter(|path| path.is_file())
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|data| serde_json::from_str(&data).ok())
            .unwrap_or_default();
        Self {
            store,
            persistent_rules,
            session_rules: Vec::new(),
            audit_log: Vec::new(),
        }
    }

    /// `<store_dir>/permissions/<sanitised workspace path>-<hash>.json`.
    #[must_use]
    pub fn store_path(store_dir: &Path, workspace: &Path) -> PathBuf {
        let text = workspace.to_string_lossy();
        let mut name: String = text
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        if name.len() > 80 {
            name = name[name.len() - 80..].to_owned();
        }
        let hash = text.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        });
        store_dir
            .join("permissions")
            .join(format!("{name}-{hash:016x}.json"))
    }

    /// Evaluates existing policies for `agent_name`, `capability` and `scope`.
    ///
    /// Session decisions have priority over persistent rules. If no rule
    /// matches, returns [`PermissionDecision::Ask`].
    #[must_use]
    pub fn evaluate(
        &self,
        agent_name: &str,
        capability: &PermissionCapability,
        scope: &str,
    ) -> PermissionDecision {
        // Check session rules newest-first
        for rule in self.session_rules.iter().rev() {
            if rule.subject.matches_agent(agent_name)
                && rule.capability.matches(capability)
                && rule.scope.matches(scope)
            {
                return rule.decision;
            }
        }

        // Check persistent rules newest-first
        for rule in self.persistent_rules.iter().rev() {
            if rule.subject.matches_agent(agent_name)
                && rule.capability.matches(capability)
                && rule.scope.matches(scope)
            {
                return rule.decision;
            }
        }

        PermissionDecision::Ask
    }

    /// Records a decision made by the user or policy engine.
    ///
    /// If `ttl` is `Session`, it is retained in memory.
    /// If `ttl` is `Always`, it is retained in memory and written to `.forge/permissions.json`.
    pub fn record_decision(
        &mut self,
        agent_name: &str,
        capability: PermissionCapability,
        scope: PermissionScope,
        decision: PermissionDecision,
        ttl: PermissionTtl,
    ) {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));

        self.audit_log.push(PermissionAuditEntry {
            timestamp_ms: now_ms,
            subject: agent_name.to_owned(),
            capability: format!("{capability:?}"),
            scope: format!("{scope:?}"),
            decision,
            ttl,
        });

        // A remembered *grant* on running commands, terminals or the
        // network needs a concrete scope; otherwise it is honoured once.
        let blanket = matches!(scope, PermissionScope::Any)
            || match &scope {
                PermissionScope::CommandPattern(pattern) | PermissionScope::Glob(pattern) => {
                    pattern.split_whitespace().all(|token| token == "*")
                }
                PermissionScope::Host(host) => host.is_empty(),
                PermissionScope::Path(path) => path.as_os_str().is_empty(),
                PermissionScope::Any => true,
            };
        let ttl = if decision == PermissionDecision::Allow && blanket && capability.needs_scope() {
            PermissionTtl::Once
        } else {
            ttl
        };
        let rule = PermissionRule {
            subject: PermissionSubject::Agent(agent_name.to_owned()),
            capability,
            scope,
            decision,
            ttl,
        };

        match ttl {
            PermissionTtl::Once => {}
            PermissionTtl::Session => {
                self.session_rules.push(rule);
            }
            PermissionTtl::Always => {
                self.session_rules.push(rule.clone());
                self.persistent_rules.push(rule);
                let _ = self.save_persistent();
            }
        }
    }

    /// Saves persistent rules to the user store; a no-op without one.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be created or the file cannot be written.
    pub fn save_persistent(&self) -> Result<(), std::io::Error> {
        let Some(path) = &self.store else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(&self.persistent_rules)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, data)
    }

    #[must_use]
    pub fn session_rules(&self) -> &[PermissionRule] {
        &self.session_rules
    }

    #[must_use]
    pub fn persistent_rules(&self) -> &[PermissionRule] {
        &self.persistent_rules
    }

    #[must_use]
    pub fn audit_log(&self) -> &[PermissionAuditEntry] {
        &self.audit_log
    }
}

/// Picks the best matching ACP `optionId` from an options array according to the user's decision.
#[must_use]
pub fn select_option_id(
    options: &[serde_json::Value],
    decision: PermissionDecision,
    ttl: PermissionTtl,
) -> Option<String> {
    let kind_of = |option: &serde_json::Value| -> String {
        option
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase()
    };
    let pick = |kinds: &[&str]| -> Option<String> {
        kinds.iter().find_map(|wanted| {
            options
                .iter()
                .find(|option| kind_of(option) == *wanted)
                .and_then(extract_option_id)
        })
    };
    // ACP option kinds are exactly `allow_once`, `allow_always`,
    // `reject_once` and `reject_always`; nothing else is ever picked, so a
    // denial can never land on an allow option (the caller cancels instead).
    match (decision, ttl) {
        (PermissionDecision::Allow, PermissionTtl::Always) => pick(&["allow_always", "allow_once"]),
        (PermissionDecision::Allow, _) => pick(&["allow_once", "allow_always"]),
        (PermissionDecision::Deny, PermissionTtl::Always) => {
            pick(&["reject_always", "reject_once"])
        }
        (PermissionDecision::Deny, _) => pick(&["reject_once", "reject_always"]),
        (PermissionDecision::Ask, _) => None,
    }
}

/// The JSON-RPC result for a permission request: the selected option, or
/// `cancelled` when no option of the wanted kind exists.
#[must_use]
pub fn permission_outcome(option_id: Option<String>) -> serde_json::Value {
    match option_id {
        Some(option_id) => serde_json::json!({
            "outcome": { "outcome": "selected", "optionId": option_id }
        }),
        None => serde_json::json!({ "outcome": { "outcome": "cancelled" } }),
    }
}

fn extract_option_id(option: &serde_json::Value) -> Option<String> {
    option
        .get("optionId")
        .or_else(|| option.get("id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("forge-perm-{name}-{}", std::process::id()))
    }

    #[test]
    fn evaluates_session_rules_before_persistent() {
        let workspace = temp("ws");
        let mut broker = PermissionBroker::new(&workspace, None);
        assert_eq!(
            broker.evaluate("OpenCode", &PermissionCapability::Terminal, "printf test"),
            PermissionDecision::Ask
        );
        broker.record_decision(
            "OpenCode",
            PermissionCapability::Terminal,
            PermissionScope::CommandPattern("printf *".into()),
            PermissionDecision::Allow,
            PermissionTtl::Session,
        );
        assert_eq!(
            broker.evaluate("OpenCode", &PermissionCapability::Terminal, "printf test"),
            PermissionDecision::Allow
        );
        assert_eq!(
            broker.evaluate("OpenCode", &PermissionCapability::Terminal, "printfx test"),
            PermissionDecision::Ask,
            "token-wise, not a text prefix"
        );
        assert_eq!(
            broker.evaluate("OpenCode", &PermissionCapability::Terminal, "rm -rf /"),
            PermissionDecision::Ask
        );
    }

    #[test]
    fn always_rules_live_in_the_user_store_not_the_workspace() {
        let workspace = temp("ws2");
        let store = temp("store");
        let _ = std::fs::remove_dir_all(&store);
        let mut broker = PermissionBroker::new(&workspace, Some(&store));
        broker.record_decision(
            "Codex",
            PermissionCapability::FsWrite,
            PermissionScope::Glob("src/**".into()),
            PermissionDecision::Allow,
            PermissionTtl::Always,
        );
        let path = PermissionBroker::store_path(&store, &workspace);
        assert!(path.is_file(), "{}", path.display());
        assert!(path.starts_with(&store));
        assert!(
            !workspace.join(".forge").exists(),
            "nothing written into the workspace"
        );
        // A different workspace does not inherit the grant.
        let other = PermissionBroker::new(&temp("ws3"), Some(&store));
        assert!(other.persistent_rules().is_empty());
        let reloaded = PermissionBroker::new(&workspace, Some(&store));
        assert_eq!(reloaded.persistent_rules().len(), 1);
        assert_eq!(
            reloaded.evaluate("Codex", &PermissionCapability::FsWrite, "src/main.rs"),
            PermissionDecision::Allow
        );
        assert_eq!(
            reloaded.evaluate("Codex", &PermissionCapability::FsWrite, "Cargo.toml"),
            PermissionDecision::Ask
        );
        let _ = std::fs::remove_dir_all(store);
    }

    #[test]
    fn blanket_grants_on_commands_are_not_remembered() {
        let mut broker = PermissionBroker::new(&temp("ws4"), None);
        broker.record_decision(
            "Codex",
            PermissionCapability::ProcessSpawn,
            PermissionScope::CommandPattern(String::new()),
            PermissionDecision::Allow,
            PermissionTtl::Always,
        );
        assert!(broker.session_rules().is_empty());
        assert!(broker.persistent_rules().is_empty());
        assert_eq!(
            broker.evaluate("Codex", &PermissionCapability::ProcessSpawn, "rm -rf /"),
            PermissionDecision::Ask
        );
        // A denial, or a scoped grant, is remembered normally.
        broker.record_decision(
            "Codex",
            PermissionCapability::ProcessSpawn,
            PermissionScope::Any,
            PermissionDecision::Deny,
            PermissionTtl::Session,
        );
        assert_eq!(
            broker.evaluate("Codex", &PermissionCapability::ProcessSpawn, "ls"),
            PermissionDecision::Deny
        );
        // A tool without a natural scope may still be remembered.
        broker.record_decision(
            "Codex",
            PermissionCapability::Tool("think".into()),
            PermissionScope::Any,
            PermissionDecision::Allow,
            PermissionTtl::Session,
        );
        assert_eq!(
            broker.evaluate("Codex", &PermissionCapability::Tool("think".into()), ""),
            PermissionDecision::Allow
        );
    }

    #[test]
    fn scopes_match_globs_commands_and_paths() {
        assert!(PermissionScope::Glob("*.rs".into()).matches("main.rs"));
        assert!(PermissionScope::Glob("src/**".into()).matches("src/a/b.rs"));
        assert!(!PermissionScope::Glob("src/*.rs".into()).matches("docs/x.md"));
        assert!(!PermissionScope::Glob(String::new()).matches("anything"));
        assert!(
            PermissionScope::CommandPattern("cargo test".into()).matches("cargo test --workspace")
        );
        assert!(!PermissionScope::CommandPattern("cargo test".into()).matches("cargo test-x"));
        assert!(PermissionScope::CommandPattern("git *".into()).matches("git status"));
        assert!(!PermissionScope::CommandPattern("git".into()).matches("gitfoo"));
        assert!(!PermissionScope::CommandPattern("*".into()).matches("rm -rf /"));
        assert!(PermissionScope::Path("/tmp/x".into()).matches("/tmp/x/y"));
        assert!(!PermissionScope::Path(PathBuf::new()).matches("/etc/passwd"));
    }

    #[test]
    fn capabilities_come_from_the_tool_call_kind() {
        assert_eq!(
            PermissionCapability::from_tool_call(Some("execute"), "run_shell"),
            PermissionCapability::ProcessSpawn
        );
        assert_eq!(
            PermissionCapability::from_tool_call(Some("edit"), "apply_patch"),
            PermissionCapability::FsWrite
        );
        assert_eq!(
            PermissionCapability::from_tool_call(None, "bash"),
            PermissionCapability::ProcessSpawn
        );
        assert_eq!(
            PermissionCapability::from_tool_call(Some("think"), "plan"),
            PermissionCapability::Tool("plan".into())
        );
    }

    #[test]
    fn option_selection_never_picks_the_wrong_kind() {
        // Reject listed first, like some agents do.
        let options = serde_json::json!([
            {"optionId": "r1", "name": "Rechazar", "kind": "reject_once"},
            {"optionId": "a1", "name": "Permitir una vez", "kind": "allow_once"},
            {"optionId": "a2", "name": "Permitir siempre", "kind": "allow_always"}
        ]);
        let options = options.as_array().unwrap();
        assert_eq!(
            select_option_id(options, PermissionDecision::Allow, PermissionTtl::Once),
            Some("a1".into())
        );
        assert_eq!(
            select_option_id(options, PermissionDecision::Allow, PermissionTtl::Always),
            Some("a2".into())
        );
        assert_eq!(
            select_option_id(options, PermissionDecision::Deny, PermissionTtl::Once),
            Some("r1".into())
        );
        // Only allow options offered: a denial cancels instead of allowing.
        let allow_only = serde_json::json!([
            {"optionId": "a1", "kind": "allow_once"}
        ]);
        let allow_only = allow_only.as_array().unwrap();
        assert_eq!(
            select_option_id(allow_only, PermissionDecision::Deny, PermissionTtl::Once),
            None
        );
        assert_eq!(
            permission_outcome(None)["outcome"]["outcome"],
            serde_json::json!("cancelled")
        );
        assert_eq!(
            permission_outcome(Some("a1".into()))["outcome"]["optionId"],
            serde_json::json!("a1")
        );
    }
}
