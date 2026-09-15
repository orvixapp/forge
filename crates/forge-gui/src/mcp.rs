//! Shared MCP definitions: explicit, read-only imports and ACP passthrough.
//! No credential stores, OAuth sessions or source-client configuration writes.

use crate::config::{McpServerConfig, McpTransport};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use toml_edit::{DocumentMut, Item, Table, value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportSource {
    Claude,
    Codex,
    OpenCode,
}

impl ImportSource {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::OpenCode => "OpenCode",
        }
    }
}

#[derive(Debug, Default)]
pub struct ImportReport {
    pub servers: Vec<McpServerConfig>,
    /// Safe summaries only: never include source values or parser excerpts.
    pub warnings: Vec<String>,
    pub duplicates: usize,
}

/// Reads only the file explicitly selected by the user, with a size bound.
///
/// # Errors
/// Returns a safe summary for inaccessible or malformed files.
pub fn import_file(
    source: ImportSource,
    path: &Path,
    workspace: &Path,
    existing: &[McpServerConfig],
) -> Result<ImportReport, String> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|_| "Cannot read MCP configuration".to_owned())?;
    let mut text = String::new();
    file.take(2 * 1024 * 1024 + 1)
        .read_to_string(&mut text)
        .map_err(|_| "Cannot read MCP configuration".to_owned())?;
    if text.len() > 2 * 1024 * 1024 {
        return Err("MCP configuration exceeds 2 MiB".into());
    }
    import_text(source, &text, workspace, existing)
}

/// Imports connection definitions, disabled, without copying credentials.
///
/// # Errors
/// Returns a safe summary for malformed configuration.
pub fn import_text(
    source: ImportSource,
    text: &str,
    workspace: &Path,
    existing: &[McpServerConfig],
) -> Result<ImportReport, String> {
    let root: Value = if source == ImportSource::Codex {
        let table: toml::Value =
            toml::from_str(text).map_err(|_| "Invalid Codex TOML configuration".to_owned())?;
        serde_json::to_value(table).map_err(|_| "Invalid Codex configuration".to_owned())?
    } else {
        serde_json::from_str(&jsonc(text))
            .map_err(|_| "Invalid MCP JSON/JSONC configuration".to_owned())?
    };
    let key = match source {
        ImportSource::Claude => "mcpServers",
        ImportSource::Codex => "mcp_servers",
        ImportSource::OpenCode => "mcp",
    };
    let mut maps = Vec::new();
    if let Some(map) = root.get(key).and_then(Value::as_object) {
        maps.push(map);
    }
    // Claude's local scope is nested under projects[absolute workspace].
    if source == ImportSource::Claude
        && let Some(map) = workspace
            .to_str()
            .and_then(|path| root.get("projects")?.get(path)?.get(key)?.as_object())
    {
        maps.insert(0, map);
    }
    let mut report = ImportReport::default();
    for map in maps {
        for (name, node) in map {
            match import_server(source, name, node) {
                Ok(server) => {
                    if existing.iter().chain(&report.servers).any(|old| {
                        old.name.eq_ignore_ascii_case(name) || same_connection(old, &server)
                    }) {
                        report.duplicates += 1;
                    } else {
                        report.servers.push(server);
                    }
                }
                Err(reason) => report.warnings.push(format!("Server skipped: {reason}")),
            }
        }
    }
    Ok(report)
}

fn import_server(
    source: ImportSource,
    name: &str,
    node: &Value,
) -> Result<McpServerConfig, String> {
    if !valid_name(name) || name.eq_ignore_ascii_case("forge") {
        return Err("invalid or reserved name".into());
    }
    if node.get("type").and_then(Value::as_str) == Some("sse") {
        return Err("legacy SSE requires an explicit stdio bridge".into());
    }
    // Never silently discard restrictions that would broaden tool access.
    for key in [
        "enabled_tools",
        "disabled_tools",
        "cwd",
        "oauth_resource",
        "http_headers_helper",
        "experimental_environment",
    ] {
        if node.get(key).is_some() {
            return Err(format!("{key} needs manual migration"));
        }
    }
    if node
        .get("oauth")
        .is_some_and(|oauth| oauth != &Value::Bool(true))
    {
        return Err("custom OAuth settings need separate authorization".into());
    }
    let mut server = McpServerConfig {
        name: name.into(),
        enabled: false,
        ..Default::default()
    };
    if let Some(url) = node.get("url").and_then(Value::as_str) {
        validate_url(url)?;
        server.transport = McpTransport::Http;
        server.url = url.into();
        let headers = node.get("headers").or_else(|| node.get("http_headers"));
        server.headers = reference_map(headers)?;
        if let Some(headers) = node.get("env_http_headers").and_then(Value::as_object) {
            for (key, variable) in headers {
                let variable = variable
                    .as_str()
                    .filter(|name| valid_variable(name))
                    .ok_or_else(|| "invalid header environment reference".to_owned())?;
                server
                    .headers
                    .insert(key.clone(), format!("${{{variable}}}"));
            }
        }
        if let Some(variable) = node.get("bearer_token_env_var").and_then(Value::as_str) {
            if !valid_variable(variable) {
                return Err("invalid bearer environment reference".into());
            }
            server
                .headers
                .insert("Authorization".into(), format!("Bearer ${{{variable}}}"));
        }
    } else {
        if source == ImportSource::OpenCode {
            let command = string_array(node.get("command"))?;
            let (program, args) = command
                .split_first()
                .ok_or_else(|| "missing command".to_owned())?;
            server.command.clone_from(program);
            server.args = args.to_vec();
        } else {
            server.command = node
                .get("command")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing command".to_owned())?
                .into();
            server.args = string_array(node.get("args"))?;
        }
        if server.command.is_empty()
            || server.command.chars().any(char::is_whitespace)
            || std::iter::once(&server.command)
                .chain(&server.args)
                .any(|arg| unsafe_argument(arg))
        {
            return Err(
                "command contains inline credentials or shell syntax; migrate manually".into(),
            );
        }
        let env = node.get("environment").or_else(|| node.get("env"));
        server.env = reference_map(env)?;
        if let Some(vars) = node.get("env_vars") {
            for variable in string_array(Some(vars))? {
                if !valid_variable(&variable) {
                    return Err("invalid environment reference".into());
                }
                server
                    .env
                    .insert(variable.clone(), format!("${{{variable}}}"));
            }
        }
    }
    Ok(server)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-".contains(c))
}

fn valid_variable(name: &str) -> bool {
    !name.is_empty()
        && !name.as_bytes()[0].is_ascii_digit()
        && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

fn normalize_reference(text: &str) -> Option<String> {
    let (prefix, reference) = text
        .strip_prefix("Bearer ")
        .map_or(("", text), |rest| ("Bearer ", rest));
    let variable = reference
        .strip_prefix("${")
        .or_else(|| reference.strip_prefix("{env:"))?
        .strip_suffix('}')?;
    valid_variable(variable).then(|| format!("{prefix}${{{variable}}}"))
}

fn reference_map(node: Option<&Value>) -> Result<BTreeMap<String, String>, String> {
    let Some(node) = node else {
        return Ok(BTreeMap::new());
    };
    let map = node
        .as_object()
        .ok_or_else(|| "invalid environment/header map".to_owned())?;
    map.iter().map(|(key, value)| {
        let reference = value.as_str().and_then(normalize_reference)
            .ok_or_else(|| "literal environment/header values are not copied; replace them with environment references".to_owned())?;
        Ok((key.clone(), reference))
    }).collect()
}

fn string_array(node: Option<&Value>) -> Result<Vec<String>, String> {
    node.map_or_else(
        || Ok(Vec::new()),
        |node| {
            node.as_array()
                .ok_or_else(|| "invalid command arguments".to_owned())?
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| "invalid command arguments".to_owned())
                })
                .collect()
        },
    )
}

fn unsafe_argument(arg: &str) -> bool {
    let lower = arg.to_ascii_lowercase();
    [
        "token",
        "secret",
        "password",
        "api-key",
        "apikey",
        "authorization",
        "client-key",
    ]
    .iter()
    .any(|part| lower.contains(part))
        || ["sk-", "ntn_", "ghp_", "github_pat_", "eyJ"]
            .iter()
            .any(|prefix| arg.contains(prefix))
        || arg.contains(['\n', '\r', ';', '|', '`'])
        || arg.contains("$(")
        || (arg.contains("://") && validate_url(arg).is_err())
}

/// Only TLS or loopback, without credentials in URLs (including query strings).
///
/// # Errors
/// Returns a safe summary for insecure endpoints.
pub fn validate_url(endpoint: &str) -> Result<(), String> {
    let url = url::Url::parse(endpoint).map_err(|_| "invalid MCP endpoint".to_owned())?;
    let loopback = match url.host() {
        Some(url::Host::Domain("localhost")) => true,
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    };
    if url.host().is_none()
        || !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("MCP endpoint must use HTTPS (or loopback HTTP), without URL credentials/query/fragment".into());
    }
    Ok(())
}

fn same_connection(a: &McpServerConfig, b: &McpServerConfig) -> bool {
    a.transport == b.transport
        && a.command == b.command
        && a.args == b.args
        && a.env == b.env
        && a.headers == b.headers
        && a.url.trim_end_matches('/') == b.url.trim_end_matches('/')
}

/// Appends without overwriting an existing name/connection, preserving comments.
///
/// # Errors
/// Returns an error when the destination is not a Forge config.
pub fn append_imports(doc: &mut DocumentMut, servers: &[McpServerConfig]) -> Result<usize, String> {
    let config: crate::config::Config = toml::from_str(&doc.to_string())
        .map_err(|_| "Invalid destination configuration".to_owned())?;
    let entry = doc
        .entry("mcp_servers")
        .or_insert(Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
    let tables = entry
        .as_array_of_tables_mut()
        .ok_or_else(|| "mcp_servers is not an array of tables".to_owned())?;
    let mut accepted = config.mcp_servers;
    let mut count = 0;
    for server in servers {
        if accepted
            .iter()
            .any(|old| old.name.eq_ignore_ascii_case(&server.name) || same_connection(old, server))
        {
            continue;
        }
        let mut table = Table::new();
        table["name"] = value(server.name.as_str());
        table["enabled"] = value(false);
        table["transport"] = value(if server.transport == McpTransport::Http {
            "http"
        } else {
            "stdio"
        });
        if server.transport == McpTransport::Http {
            table["url"] = value(server.url.as_str());
        } else {
            table["command"] = value(server.command.as_str());
            let mut args = toml_edit::Array::new();
            for arg in &server.args {
                args.push(arg.as_str());
            }
            table["args"] = value(args);
        }
        for (key, map) in [("env", &server.env), ("headers", &server.headers)] {
            if !map.is_empty() {
                let mut entries = toml_edit::InlineTable::new();
                for (name, reference) in map {
                    entries.insert(name, reference.as_str().into());
                }
                table[key] = value(entries);
            }
        }
        tables.push(table);
        accepted.push(server.clone());
        count += 1;
    }
    Ok(count)
}

/// Resolves references only at session creation; errors never disclose values.
///
/// # Errors
/// Returns an error for invalid definitions or missing environment variables.
pub fn acp_entry(
    server: &McpServerConfig,
    agent: &str,
    capabilities: &Value,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Option<Value>, String> {
    if !server.enabled
        || (!server.agents.is_empty()
            && !server
                .agents
                .iter()
                .any(|name| name.eq_ignore_ascii_case(agent)))
    {
        return Ok(None);
    }
    if !valid_name(&server.name) || server.name.eq_ignore_ascii_case("forge") {
        return Err("invalid or reserved MCP name".into());
    }
    let resolve = |text: &str| -> Result<String, String> {
        if let Some(reference) = normalize_reference(text) {
            let (prefix, variable) = reference
                .strip_prefix("Bearer ")
                .map_or(("", reference.as_str()), |rest| ("Bearer ", rest));
            let variable = &variable[2..variable.len() - 1];
            let value = lookup(variable)
                .ok_or_else(|| format!("missing environment variable {variable}"))?;
            Ok(format!("{prefix}{value}"))
        } else if text.contains("${") || text.contains("{env:") {
            Err("unsupported environment reference".into())
        } else {
            Ok(text.into())
        }
    };
    let entries =
        |map: &BTreeMap<String, String>, references_only: bool| -> Result<Vec<Value>, String> {
            map.iter()
                .map(|(name, text)| {
                    if references_only && normalize_reference(text).is_none() {
                        return Err("HTTP headers must use environment references".into());
                    }
                    let value = resolve(text)?;
                    if references_only
                        && (name.is_empty()
                            || !name.bytes().all(|byte| {
                                byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
                            })
                            || value.contains(['\r', '\n']))
                    {
                        return Err("invalid HTTP header".into());
                    }
                    Ok(json!({"name": name, "value": value}))
                })
                .collect()
        };
    if server.transport == McpTransport::Http {
        if !server.command.is_empty() || !server.args.is_empty() || !server.env.is_empty() {
            return Err("HTTP MCP definitions cannot contain a stdio command/environment".into());
        }
        validate_url(&server.url)?;
        if capabilities
            .pointer("/mcpCapabilities/http")
            .and_then(Value::as_bool)
            != Some(true)
        {
            return Err("agent does not advertise ACP MCP HTTP support; configure a stdio bridge explicitly".into());
        }
        Ok(Some(
            json!({"type":"http", "name":server.name, "url":server.url, "headers":entries(&server.headers, true)?}),
        ))
    } else {
        if server.command.is_empty() || !server.url.is_empty() || !server.headers.is_empty() {
            return Err("invalid stdio MCP definition".into());
        }
        let args: Result<Vec<_>, _> = server.args.iter().map(|arg| resolve(arg)).collect();
        Ok(Some(
            json!({"name":server.name, "command":resolve(&server.command)?, "args":args?, "env":entries(&server.env, false)?}),
        ))
    }
}

// JSONC comments/trailing commas are removed outside quoted strings only.
fn jsonc(text: &str) -> String {
    let mut out = Vec::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut quoted = false;
    while i < bytes.len() {
        let byte = bytes[i];
        if quoted && byte == b'\\' && i + 1 < bytes.len() {
            out.extend_from_slice(&bytes[i..i + 2]);
            i += 2;
            continue;
        }
        if byte == b'"' {
            quoted = !quoted;
        }
        if !quoted && bytes.get(i..i + 2) == Some(b"//") {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            out.push(b' ');
            continue;
        }
        if !quoted && bytes.get(i..i + 2) == Some(b"/*") {
            i += 2;
            while i + 1 < bytes.len() && &bytes[i..i + 2] != b"*/" {
                i += 1;
            }
            if i + 1 >= bytes.len() {
                return String::new();
            }
            i += 2;
            out.push(b' ');
            continue;
        }
        out.push(byte);
        i += 1;
    }
    let mut result = out.clone();
    quoted = false;
    i = 0;
    while i < out.len() {
        if quoted && out[i] == b'\\' {
            i += 2;
            continue;
        }
        if out[i] == b'"' {
            quoted = !quoted;
        }
        if !quoted && out[i] == b',' {
            let next = out[i + 1..].iter().find(|byte| !byte.is_ascii_whitespace());
            if matches!(next, Some(b'}' | b']')) {
                result[i] = b' ';
            }
        }
        i += 1;
    }
    String::from_utf8(result).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_all_clients_disabled_and_without_oauth_state() {
        for (source, text) in [
            (
                ImportSource::Claude,
                r#"{"mcpServers":{"notion":{"type":"http","url":"https://mcp.notion.com/mcp"}},"oauth":{"access_token":"NEVER_COPY"}}"#,
            ),
            (
                ImportSource::Codex,
                "[mcp_servers.notion]\nurl = 'https://mcp.notion.com/mcp'\n[unrelated]\naccess_token = 'NEVER_COPY'",
            ),
            (
                ImportSource::OpenCode,
                r#"{/* comment */ "mcp":{"notion":{"type":"remote","url":"https://mcp.notion.com/mcp",},},"auth":"NEVER_COPY"}"#,
            ),
        ] {
            let report = import_text(source, text, Path::new("/workspace"), &[]).unwrap();
            assert_eq!(report.servers.len(), 1, "{report:?}");
            assert!(!report.servers[0].enabled);
            let mut doc = DocumentMut::new();
            append_imports(&mut doc, &report.servers).unwrap();
            assert!(!doc.to_string().contains("NEVER_COPY"));
        }
    }

    #[test]
    fn skips_literal_secrets_and_restrictions_without_disclosing_them() {
        for definition in [
            r#"{"command":"node","env":{"KEY":"NEVER_COPY"}}"#,
            r#"{"url":"https://example.com/mcp","headers":{"Authorization":"NEVER_COPY"}}"#,
            r#"{"command":"node","args":["--token","NEVER_COPY"]}"#,
            r#"{"command":"node","disabled_tools":["write"]}"#,
            r#"{"url":"https://example.com/mcp?token=NEVER_COPY"}"#,
        ] {
            let text = format!("{{\"mcpServers\":{{\"test\":{definition}}}}}");
            let report = import_text(ImportSource::Claude, &text, Path::new("/"), &[]).unwrap();
            assert!(report.servers.is_empty());
            assert!(!format!("{report:?}").contains("NEVER_COPY"));
        }
        let error =
            import_text(ImportSource::Claude, "NEVER_COPY", Path::new("/"), &[]).unwrap_err();
        assert!(!error.contains("NEVER_COPY"));
    }

    #[test]
    fn deduplicates_names_and_connections_without_overwriting() {
        let text = r#"{"mcpServers":{"one":{"command":"node","args":["server.js"]},"two":{"command":"node","args":["server.js"]}}}"#;
        let report = import_text(ImportSource::Claude, text, Path::new("/"), &[]).unwrap();
        assert_eq!(report.servers.len(), 1);
        assert_eq!(report.duplicates, 1);
        let mut doc: DocumentMut = "# preserve comments\n[ui]\nlanguage = 'spanish'\n"
            .parse()
            .unwrap();
        assert_eq!(append_imports(&mut doc, &report.servers).unwrap(), 1);
        let before = doc.to_string();
        assert_eq!(append_imports(&mut doc, &report.servers).unwrap(), 0);
        assert_eq!(doc.to_string(), before);
        assert!(before.starts_with("# preserve comments"));
    }

    #[test]
    fn references_resolve_only_for_allowed_agents_and_capabilities() {
        let text =
            "[mcp_servers.remote]\nurl='https://example.com/mcp'\nbearer_token_env_var='MCP_KEY'";
        let mut server = import_text(ImportSource::Codex, text, Path::new("/"), &[])
            .unwrap()
            .servers
            .remove(0);
        server.enabled = true;
        server.agents = vec!["Codex".into()];
        let capabilities = json!({"mcpCapabilities":{"http":true}});
        assert!(
            acp_entry(&server, "OpenCode", &capabilities, |_| panic!(
                "must not resolve"
            ))
            .unwrap()
            .is_none()
        );
        assert!(acp_entry(&server, "Codex", &json!({}), |_| None).is_err());
        assert!(
            acp_entry(&server, "Codex", &capabilities, |_| None)
                .unwrap_err()
                .contains("MCP_KEY")
        );
        let entry = acp_entry(&server, "codex", &capabilities, |_| {
            Some("runtime-value".into())
        })
        .unwrap()
        .unwrap();
        assert_eq!(entry["headers"][0]["value"], "Bearer runtime-value");
        assert_eq!(entry["type"], "http");
    }

    #[test]
    fn endpoint_security_and_jsonc_preserve_strings() {
        for url in [
            "http://localhost/mcp",
            "http://127.0.0.2/mcp",
            "http://[::1]/mcp",
            "https://mcp.notion.com/mcp",
        ] {
            validate_url(url).unwrap();
        }
        for url in [
            "http://example.com/mcp",
            "https://user:pass@example.com",
            "file:///tmp/mcp",
            "https://example.com?key=x",
            "http://localhost.evil/mcp",
        ] {
            assert!(validate_url(url).is_err());
        }
        assert_eq!(
            serde_json::from_str::<Value>(&jsonc(
                r#"{"a":"https://host/*not comment*/", "b":"é \\\"",}"#
            ))
            .unwrap()["a"],
            "https://host/*not comment*/"
        );
    }

    #[test]
    fn claude_workspace_scope_and_stdio_backward_compatibility() {
        let report = import_text(ImportSource::Claude, r#"{"projects":{"/repo":{"mcpServers":{"local":{"command":"node","args":["server.js"],"env":{"KEY":"${MY_KEY}"}}}},"/other":{"mcpServers":{"other":{"command":"other"}}}}}"#, Path::new("/repo"), &[]).unwrap();
        assert_eq!(report.servers.len(), 1);
        let config: crate::config::Config =
            toml::from_str("[[mcp_servers]]\nname='old'\ncommand='node'\nargs=['server.js']")
                .unwrap();
        assert!(config.mcp_servers[0].enabled);
        assert!(
            acp_entry(&config.mcp_servers[0], "any", &json!({}), |_| None)
                .unwrap()
                .is_some()
        );
    }
}
