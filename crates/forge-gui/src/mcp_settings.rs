//! Minimal settings pickers over the common MCP configuration.

use crate::window::{ForgeWindow, NotificationLevel, Picker, PickerKind, PromptKind, TextPrompt};
use forge_gui::i18n::{tr, trf};
use forge_gui::mcp::{ImportSource, append_imports, import_file};
use gpui::Context;
use std::path::PathBuf;
use toml_edit::{Array, DocumentMut, Table, value};

const SOURCES: [ImportSource; 3] = [
    ImportSource::Claude,
    ImportSource::Codex,
    ImportSource::OpenCode,
];

impl ForgeWindow {
    pub(crate) fn mcp_add_http(&mut self, name: &str, url: &str, cx: &mut Context<Self>) {
        // Reuse the same validation/deduplication as imports. Only public
        // connection metadata is submitted to the common config.
        let text = serde_json::json!({"mcpServers":{name:{"type":"http","url":url}}}).to_string();
        let report = match forge_gui::mcp::import_text(
            ImportSource::Claude,
            &text,
            &self.factory.cwd,
            &self.config.mcp_servers,
        ) {
            Ok(report) if report.warnings.is_empty() && report.duplicates == 0 => report,
            _ => {
                self.notify_user(
                    NotificationLevel::Error,
                    tr("Invalid or duplicate MCP server — use HTTPS and a unique name"),
                );
                return;
            }
        };
        let result = self.edit_user_config(|doc| {
            append_imports(doc, &report.servers)?;
            Ok(())
        });
        self.mcp_config_result(result, cx);
    }

    pub(crate) fn mcp_import_menu(&mut self, cx: &mut Context<Self>) {
        self.picker = Some(Picker {
            title: tr("Import MCP definitions only — no credentials").into(),
            items: SOURCES.iter().map(|source| source.label().into()).collect(),
            index: 0,
            kind: PickerKind::McpImport,
        });
        cx.notify();
    }

    pub(crate) fn mcp_import_source(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(source) = SOURCES.get(index).copied() else {
            return;
        };
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let path = match source {
            ImportSource::Claude => home.join(".claude.json"),
            ImportSource::Codex => std::env::var_os("CODEX_HOME")
                .map_or_else(|| home.join(".codex"), PathBuf::from)
                .join("config.toml"),
            ImportSource::OpenCode => std::env::var_os("XDG_CONFIG_HOME")
                .map_or_else(|| home.join(".config"), PathBuf::from)
                .join("opencode/opencode.json"),
        };
        self.rename = Some(TextPrompt {
            title: trf("MCP configuration file from {}", &[&source.label()]),
            value: path.to_string_lossy().into_owned(),
            kind: PromptKind::McpImport(source),
        });
        cx.notify();
    }

    pub(crate) fn mcp_import_file(
        &mut self,
        source: ImportSource,
        path: &str,
        cx: &mut Context<Self>,
    ) {
        if path.is_empty() {
            return;
        }
        let path = PathBuf::from(path);
        let path = if path.is_absolute() {
            path
        } else {
            self.factory.cwd.join(path)
        };
        let report = match import_file(source, &path, &self.factory.cwd, &self.config.mcp_servers) {
            Ok(report) => report,
            Err(error) => {
                self.notify_user(NotificationLevel::Error, error);
                return;
            }
        };
        let mut imported = 0;
        let result = if report.servers.is_empty() {
            Ok(())
        } else {
            self.edit_user_config(|doc| {
                imported = append_imports(doc, &report.servers)?;
                Ok(())
            })
            .map(|_| ())
        };
        match result {
            Ok(()) => {
                self.reload_config_if_changed(cx);
                self.notify_user(NotificationLevel::Info, trf(
                    "MCP: {} imported (disabled), {} duplicates, {} skipped. Review in settings.",
                    &[&imported, &report.duplicates, &report.warnings.len()],
                ));
                if report.warnings.is_empty() {
                    self.mcp_manage_menu(cx);
                } else {
                    // Show every safe reason in an actionable, native picker.
                    self.picker = Some(Picker {
                        title: tr("MCP import warnings — no credentials copied").into(),
                        items: report.warnings,
                        index: 0,
                        kind: PickerKind::McpImportWarnings,
                    });
                }
            }
            Err(error) => self.notify_user(NotificationLevel::Error, error),
        }
        cx.notify();
    }

    pub(crate) fn mcp_manage_menu(&mut self, cx: &mut Context<Self>) {
        let mut items: Vec<_> = self
            .config
            .mcp_servers
            .iter()
            .map(|server| {
                let access = if server.agents.is_empty() {
                    tr("All agents").into()
                } else {
                    server.agents.join(", ")
                };
                format!(
                    "{} · {} · {} · {}",
                    server.name,
                    if server.enabled {
                        tr("Enabled")
                    } else {
                        tr("Disabled")
                    },
                    if server.url.is_empty() {
                        "stdio"
                    } else {
                        "HTTP"
                    },
                    access
                )
            })
            .collect();
        items.push(tr("Edit MCP definitions in user settings…").into());
        self.picker = Some(Picker {
            title: tr("MCP servers — changes apply to new sessions").into(),
            items,
            index: 0,
            kind: PickerKind::McpServers,
        });
        cx.notify();
    }

    pub(crate) fn mcp_server_menu(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(server) = self.config.mcp_servers.get(index) else {
            self.settings_pick(0, cx);
            return;
        };
        self.picker = Some(Picker {
            title: format!("MCP · {}", server.name),
            items: vec![
                tr(if server.enabled {
                    "Disable server"
                } else {
                    "Enable server (starts a process or connects remotely)"
                })
                .into(),
                tr("Choose allowed agents…").into(),
                tr("Edit MCP definitions in user settings…").into(),
            ],
            index: 0,
            kind: PickerKind::McpAction(index),
        });
        cx.notify();
    }

    pub(crate) fn mcp_server_action(
        &mut self,
        server_index: usize,
        action: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(server) = self.config.mcp_servers.get(server_index).cloned() else {
            return;
        };
        match action {
            0 => {
                // Validate before enabling; an HTTP capability is checked later
                // against the actual selected ACP agent, not guessed by name.
                if !server.enabled
                    && !server.url.is_empty()
                    && let Err(error) = forge_gui::mcp::validate_url(&server.url)
                {
                    self.notify_user(NotificationLevel::Error, error);
                    return;
                }
                let result = self.edit_user_config(|doc| {
                    server_table(doc, &server.name)?["enabled"] = value(!server.enabled);
                    Ok(())
                });
                self.mcp_config_result(result, cx);
            }
            1 => {
                self.rename = Some(TextPrompt {
                    title: tr("Allowed agent names, comma separated (empty = all)").into(),
                    value: server.agents.join(", "),
                    kind: PromptKind::McpAgents(server_index),
                });
            }
            _ => self.settings_pick(0, cx),
        }
        cx.notify();
    }

    pub(crate) fn mcp_set_agents(&mut self, index: usize, text: &str, cx: &mut Context<Self>) {
        let Some(server) = self.config.mcp_servers.get(index).cloned() else {
            return;
        };
        let mut agents = Vec::new();
        for name in text
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            let Some(agent) = self
                .config
                .agents
                .iter()
                .find(|agent| agent.name.eq_ignore_ascii_case(name))
            else {
                self.notify_user(
                    NotificationLevel::Error,
                    tr("Unknown agent name — add it in settings first"),
                );
                return;
            };
            if !agents.contains(&agent.name) {
                agents.push(agent.name.clone());
            }
        }
        let result = self.edit_user_config(|doc| {
            let mut list = Array::new();
            for name in &agents {
                list.push(name.as_str());
            }
            server_table(doc, &server.name)?["agents"] = value(list);
            Ok(())
        });
        self.mcp_config_result(result, cx);
    }

    fn mcp_config_result(&mut self, result: Result<PathBuf, String>, cx: &mut Context<Self>) {
        match result {
            Ok(_) => {
                self.reload_config_if_changed(cx);
                self.notify_user(
                    NotificationLevel::Info,
                    tr("MCP settings saved — open a new agent session"),
                );
                self.mcp_manage_menu(cx);
            }
            Err(error) => self.notify_user(NotificationLevel::Error, error),
        }
    }
}

fn server_table<'a>(doc: &'a mut DocumentMut, name: &str) -> Result<&'a mut Table, String> {
    doc.get_mut("mcp_servers")
        .and_then(toml_edit::Item::as_array_of_tables_mut)
        .and_then(|tables| {
            tables
                .iter_mut()
                .find(|table| table.get("name").and_then(toml_edit::Item::as_str) == Some(name))
        })
        .ok_or_else(|| tr("MCP server no longer exists — reopen settings").into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_edits_preserve_other_servers_and_comments() {
        let mut doc: DocumentMut = "# keep\n[[mcp_servers]]\nname='one'\ncommand='node'\n[[mcp_servers]]\nname='two'\ncommand='other'\n".parse().unwrap();
        server_table(&mut doc, "two").unwrap()["enabled"] = value(false);
        assert!(doc.to_string().starts_with("# keep"));
        let config: forge_gui::config::Config = toml::from_str(&doc.to_string()).unwrap();
        assert!(config.mcp_servers[0].enabled);
        assert!(!config.mcp_servers[1].enabled);
        assert!(server_table(&mut doc, "missing").is_err());
    }
}
