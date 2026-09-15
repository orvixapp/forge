//! Settings menu (`settings.open`, `Ctrl+,`, gear in the top bar): opens the
//! TOML files in the editor, and offers small wizards for the things that
//! are tedious to type by hand — providers, agents, theme and language.
//! Every wizard ends by editing the user's `config.toml` in place with
//! `toml_edit`, so comments and ordering survive and the watcher applies
//! the change within a second like any manual edit.

use crate::window::{ForgeWindow, NotificationLevel, Picker, PickerKind, PromptKind, TextPrompt};
use forge_gui::config::{ProviderKind, TaskClass};
use forge_gui::i18n::{tr, trf};
use forge_gui::{config::Language, theme};
use gpui::Context;
use std::path::{Path, PathBuf};
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsItem {
    UserConfig,
    WorkspaceConfig,
    AddProvider,
    AddAgent,
    ImportMcp,
    ManageMcp,
    AddMcpHttp,
    ChooseTheme,
    ToggleLanguage,
    ToggleFormatOnSave,
}

const SETTINGS_ITEMS: [SettingsItem; 10] = [
    SettingsItem::UserConfig,
    SettingsItem::WorkspaceConfig,
    SettingsItem::AddProvider,
    SettingsItem::AddAgent,
    SettingsItem::ImportMcp,
    SettingsItem::ManageMcp,
    SettingsItem::AddMcpHttp,
    SettingsItem::ChooseTheme,
    SettingsItem::ToggleLanguage,
    SettingsItem::ToggleFormatOnSave,
];

const PROVIDER_KINDS: [(ProviderKind, &str); 4] = [
    (ProviderKind::Openai, "openai"),
    (ProviderKind::Anthropic, "anthropic"),
    (ProviderKind::Google, "google"),
    (ProviderKind::OpenaiCompatible, "openai-compatible"),
];

/// A wizard collects answers one prompt or picker at a time; `step` is the
/// next question to ask.
#[derive(Debug, Clone)]
pub enum Wizard {
    McpHttp {
        step: usize,
        name: String,
        url: String,
    },
    Provider {
        step: usize,
        name: String,
        kind: ProviderKind,
        model: String,
        base_url: String,
        api_key_env: String,
    },
    Agent {
        step: usize,
        name: String,
        command: String,
        args: Vec<String>,
    },
}

struct ProviderDraft {
    name: String,
    kind: ProviderKind,
    model: String,
    base_url: String,
    api_key_env: String,
}

/// Written when the user opens a config file that does not exist yet.
const USER_TEMPLATE: &str = r#"# Forge — user configuration. Every key is optional; changes apply within a
# second. Schema for editors: docs/config.schema.json in the repository.

[ui]
# language = "spanish"      # spanish | english
# theme = "forge-dark"       # forge-dark | forge-light | a file in themes/

[font]
# family = "monospace"
# size = 14
# line_height = 1.3

[terminal]
# shell = "/bin/zsh"
# args = ["-l"]
# shell_integration = true

[editor]
# tab_size = 4
# line_numbers = true
# word_wrap = false
# minimap = true
# autosave_ms = 0

# ----- agents (ACP) ---------------------------------------------------------
# [[providers]]
# name = "gpt"
# kind = "openai"              # openai | anthropic | google | openai-compatible
# model = "gpt-5"
# base_url = ""                # e.g. http://localhost:11434/v1 for Ollama
# api_key_env = "OPENAI_API_KEY"
# free = false

# [router]
# trivial = "gpt"
# normal = "gpt"
# deep = "gpt"
# default_class = "normal"

# [[agents]]
# name = "opencode"
# command = "opencode"
# args = ["acp"]
# provider = "gpt"
# worktree = false

# [[mcp_servers]]
# name = "filesystem"
# command = "npx"
# args = ["-y", "@modelcontextprotocol/server-filesystem", "."]

# [[keybindings]]
# key = "ctrl+shift+t"
# command = "terminal.newTab"
"#;

const WORKSPACE_TEMPLATE: &str = r"# Forge — workspace configuration (.forge/config.toml). Layered on top of
# the user file. Cannot set terminal.shell/args, providers, router or
# mcp_servers.

[editor]
# tab_size = 2
";

impl ForgeWindow {
    pub(crate) fn open_settings_menu(&mut self, cx: &mut Context<Self>) {
        let items = SETTINGS_ITEMS
            .iter()
            .map(|item| match item {
                SettingsItem::UserConfig => tr("Open user settings (config.toml)").to_owned(),
                SettingsItem::WorkspaceConfig => {
                    tr("Open workspace settings (.forge/config.toml)").to_owned()
                }
                SettingsItem::AddProvider => tr("Add provider…").to_owned(),
                SettingsItem::AddAgent => tr("Add agent…").to_owned(),
                SettingsItem::ImportMcp => tr("Import MCP servers…").to_owned(),
                SettingsItem::ManageMcp => tr("Manage MCP servers…").to_owned(),
                SettingsItem::AddMcpHttp => tr("Add HTTP MCP server…").to_owned(),
                SettingsItem::ChooseTheme => tr("Choose theme…").to_owned(),
                SettingsItem::ToggleLanguage => match self.config.ui.language {
                    Language::Spanish => tr("Switch to English").to_owned(),
                    Language::English => tr("Switch to Español").to_owned(),
                },
                SettingsItem::ToggleFormatOnSave => {
                    if self.config.lsp.format_on_save {
                        tr("Format on save: on (turn off)").to_owned()
                    } else {
                        tr("Format on save: off (turn on)").to_owned()
                    }
                }
            })
            .collect();
        self.wizard = None;
        self.picker = Some(Picker {
            title: tr("Settings").into(),
            items,
            index: 0,
            kind: PickerKind::Settings,
        });
        cx.notify();
    }

    pub(crate) fn settings_pick(&mut self, index: usize, cx: &mut Context<Self>) {
        match SETTINGS_ITEMS.get(index) {
            Some(SettingsItem::UserConfig) => {
                if let Some(path) = self.user_config_path() {
                    self.open_config_file(&path, USER_TEMPLATE, cx);
                }
            }
            Some(SettingsItem::WorkspaceConfig) => {
                let path = self.factory.cwd.join(".forge").join("config.toml");
                self.open_config_file(&path, WORKSPACE_TEMPLATE, cx);
            }
            Some(SettingsItem::AddProvider) => {
                self.wizard = Some(Wizard::Provider {
                    step: 0,
                    name: String::new(),
                    kind: ProviderKind::Openai,
                    model: String::new(),
                    base_url: String::new(),
                    api_key_env: String::new(),
                });
                self.wizard_next(cx);
            }
            Some(SettingsItem::AddAgent) => {
                self.wizard = Some(Wizard::Agent {
                    step: 0,
                    name: String::new(),
                    command: String::new(),
                    args: Vec::new(),
                });
                self.wizard_next(cx);
            }
            Some(SettingsItem::ImportMcp) => self.mcp_import_menu(cx),
            Some(SettingsItem::ManageMcp) => self.mcp_manage_menu(cx),
            Some(SettingsItem::AddMcpHttp) => {
                self.wizard = Some(Wizard::McpHttp {
                    step: 0,
                    name: String::new(),
                    url: String::new(),
                });
                self.wizard_next(cx);
            }
            Some(SettingsItem::ChooseTheme) => {
                let dir = self.themes_dir();
                let names = theme::available_themes(dir.as_deref());
                let index = names
                    .iter()
                    .position(|name| name.eq_ignore_ascii_case(&self.theme_name))
                    .unwrap_or(0);
                self.picker = Some(Picker {
                    title: tr("Choose theme…").into(),
                    items: names,
                    index,
                    kind: PickerKind::Theme,
                });
            }
            Some(SettingsItem::ToggleLanguage) => {
                let next = match self.config.ui.language {
                    Language::Spanish => "english",
                    Language::English => "spanish",
                };
                let result = self.edit_user_config(|doc| {
                    table_mut(doc, "ui")?["language"] = value(next);
                    Ok(())
                });
                match result {
                    Ok(path) => {
                        self.reload_config_if_changed(cx);
                        self.notify_user(
                            NotificationLevel::Info,
                            trf("Language: {}", &[&format!("{next} · {}", path.display())]),
                        );
                    }
                    Err(error) => self.notify_user(NotificationLevel::Error, error),
                }
            }
            Some(SettingsItem::ToggleFormatOnSave) => {
                let next = !self.config.lsp.format_on_save;
                let result = self.edit_user_config(|doc| {
                    table_mut(doc, "lsp")?["format_on_save"] = value(next);
                    Ok(())
                });
                match result {
                    Ok(_) => {
                        self.reload_config_if_changed(cx);
                        self.notify_user(
                            NotificationLevel::Info,
                            if next {
                                tr("Format on save enabled")
                            } else {
                                tr("Format on save disabled")
                            },
                        );
                    }
                    Err(error) => self.notify_user(NotificationLevel::Error, error),
                }
            }
            None => {}
        }
        cx.notify();
    }

    pub(crate) fn settings_apply_theme(&mut self, index: usize, cx: &mut Context<Self>) {
        let dir = self.themes_dir();
        let Some(name) = theme::available_themes(dir.as_deref()).get(index).cloned() else {
            return;
        };
        match self.edit_user_config(|doc| {
            table_mut(doc, "ui")?["theme"] = value(name.as_str());
            Ok(())
        }) {
            Ok(_) => {
                self.reload_config_if_changed(cx);
                self.notify_user(NotificationLevel::Info, trf("Theme: {}", &[&name]));
            }
            Err(error) => self.notify_user(NotificationLevel::Error, error),
        }
        cx.notify();
    }

    /// A text answer for the running wizard.
    pub(crate) fn wizard_text(&mut self, answer: &str, cx: &mut Context<Self>) {
        let answer = answer.trim().to_owned();
        match &mut self.wizard {
            Some(Wizard::McpHttp { step, name, url }) => {
                if answer.is_empty() {
                    self.wizard = None;
                    return;
                }
                if *step == 0 {
                    *name = answer;
                } else {
                    *url = answer;
                }
                *step += 1;
            }
            Some(Wizard::Provider {
                step,
                name,
                model,
                base_url,
                api_key_env,
                ..
            }) => {
                match *step {
                    0 => {
                        if answer.is_empty() {
                            self.wizard = None;
                            return;
                        }
                        *name = answer;
                    }
                    2 => *model = answer,
                    3 => *base_url = answer,
                    4 => *api_key_env = answer,
                    _ => {}
                }
                *step += 1;
            }
            Some(Wizard::Agent {
                step,
                name,
                command,
                args,
            }) => {
                match *step {
                    0 | 1 if answer.is_empty() => {
                        self.wizard = None;
                        return;
                    }
                    0 => *name = answer,
                    1 => *command = answer,
                    2 => *args = answer.split_whitespace().map(str::to_owned).collect(),
                    _ => {}
                }
                *step += 1;
            }
            None => return,
        }
        self.wizard_next(cx);
    }

    /// A picker answer for the running wizard.
    pub(crate) fn wizard_choice(&mut self, index: usize, cx: &mut Context<Self>) {
        match self.wizard.clone() {
            Some(Wizard::Provider { step: 1, .. }) => {
                if let Some(Wizard::Provider { step, kind, .. }) = &mut self.wizard {
                    *kind = PROVIDER_KINDS
                        .get(index)
                        .map_or(ProviderKind::Openai, |k| k.0);
                    *step += 1;
                }
                self.wizard_next(cx);
            }
            Some(Wizard::Provider {
                step: 5,
                name,
                kind,
                model,
                base_url,
                api_key_env,
            }) => {
                self.wizard = None;
                let draft = ProviderDraft {
                    name,
                    kind,
                    model,
                    base_url,
                    api_key_env,
                };
                self.write_provider(&draft, index, cx);
            }
            Some(Wizard::Agent {
                step: 3,
                name,
                command,
                args,
            }) => {
                self.wizard = None;
                let provider = index
                    .checked_sub(1)
                    .and_then(|i| self.config.providers.get(i))
                    .map(|provider| provider.name.clone());
                let result = self.edit_user_config(|doc| {
                    let mut table = Table::new();
                    table["name"] = value(name.as_str());
                    table["command"] = value(command.as_str());
                    let mut list = toml_edit::Array::new();
                    for arg in &args {
                        list.push(arg.as_str());
                    }
                    table["args"] = value(list);
                    table["enabled"] = value(true);
                    if let Some(provider) = &provider {
                        table["provider"] = value(provider.as_str());
                    }
                    push_table(doc, "agents", table)
                });
                match result {
                    Ok(path) => {
                        self.reload_config_if_changed(cx);
                        self.notify_user(
                            NotificationLevel::Info,
                            trf("Agent {} added to {}", &[&name, &path.display()]),
                        );
                    }
                    Err(error) => self.notify_user(NotificationLevel::Error, error),
                }
            }
            _ => self.wizard = None,
        }
        cx.notify();
    }

    /// Writes the provider and, per the last picker, points the router or
    /// one agent at it.
    fn write_provider(&mut self, draft: &ProviderDraft, use_for: usize, cx: &mut Context<Self>) {
        let agents: Vec<String> = self
            .config
            .agents
            .iter()
            .map(|agent| agent.name.clone())
            .collect();
        let kind_name = PROVIDER_KINDS
            .iter()
            .find(|(k, _)| *k == draft.kind)
            .map_or("openai", |k| k.1);
        let result = self.edit_user_config(|doc| {
            let mut table = Table::new();
            table["name"] = value(draft.name.as_str());
            table["kind"] = value(kind_name);
            table["model"] = value(draft.model.as_str());
            if !draft.base_url.is_empty() {
                table["base_url"] = value(draft.base_url.as_str());
            }
            if !draft.api_key_env.is_empty() {
                table["api_key_env"] = value(draft.api_key_env.as_str());
            }
            push_table(doc, "providers", table)?;
            match use_for {
                0 => {}
                1 => {
                    for class in [TaskClass::Trivial, TaskClass::Normal, TaskClass::Deep] {
                        table_mut(doc, "router")?[class.label()] = value(draft.name.as_str());
                    }
                }
                agent => {
                    let wanted = agents.get(agent - 2).cloned().unwrap_or_default();
                    if let Some(list) = doc.get_mut("agents").and_then(Item::as_array_of_tables_mut)
                    {
                        for entry in list.iter_mut() {
                            if entry.get("name").and_then(Item::as_str) == Some(&wanted) {
                                entry["provider"] = value(draft.name.as_str());
                            }
                        }
                    }
                }
            }
            Ok(())
        });
        match result {
            Ok(path) => {
                self.reload_config_if_changed(cx);
                self.notify_user(
                    NotificationLevel::Info,
                    trf("Provider {} added to {}", &[&draft.name, &path.display()]),
                );
            }
            Err(error) => self.notify_user(NotificationLevel::Error, error),
        }
    }

    /// Shows the prompt or picker for the wizard's current step.
    fn wizard_next(&mut self, cx: &mut Context<Self>) {
        if let Some(Wizard::McpHttp { step: 2, name, url }) = self.wizard.clone() {
            self.wizard = None;
            self.mcp_add_http(&name, &url, cx);
            return;
        }
        let (prompt, picker) = match &self.wizard {
            Some(Wizard::McpHttp { step, .. }) => (
                Some(tr(if *step == 0 {
                    "MCP server name"
                } else {
                    "MCP HTTPS URL (OAuth is authorized separately in each agent)"
                })),
                None,
            ),
            Some(Wizard::Provider { step, .. }) => match *step {
                0 => (Some(tr("Provider name (e.g. gpt, local)")), None),
                1 => (
                    None,
                    Some((
                        tr("Provider kind"),
                        PROVIDER_KINDS.iter().map(|k| k.1.to_owned()).collect(),
                    )),
                ),
                2 => (
                    Some(tr("Model (e.g. gpt-5, claude-opus-5, qwen2.5-coder)")),
                    None,
                ),
                3 => (Some(tr("Base URL (empty = the provider's default)")), None),
                4 => (
                    Some(tr(
                        "Environment variable holding the API key (empty = none)",
                    )),
                    None,
                ),
                _ => {
                    let mut items = vec![
                        tr("Only add it").to_owned(),
                        tr("All task classes (router)").to_owned(),
                    ];
                    items.extend(
                        self.config
                            .agents
                            .iter()
                            .map(|agent| format!("{}: {}", tr("Agent"), agent.name)),
                    );
                    (None, Some((tr("Use this provider for"), items)))
                }
            },
            Some(Wizard::Agent { step, .. }) => match *step {
                0 => (Some(tr("Agent name (e.g. opencode, codex)")), None),
                1 => (Some(tr("Command (e.g. opencode)")), None),
                2 => (Some(tr("Arguments, space separated (e.g. acp)")), None),
                _ => {
                    let mut items = vec![tr("None (the agent's own login)").to_owned()];
                    items.extend(
                        self.config
                            .providers
                            .iter()
                            .map(forge_gui::config::ProviderConfig::describe),
                    );
                    (None, Some((tr("Provider for this agent"), items)))
                }
            },
            None => return,
        };
        self.picker = None;
        self.rename = None;
        if let Some(title) = prompt {
            self.rename = Some(TextPrompt {
                title: title.into(),
                value: String::new(),
                kind: PromptKind::Wizard,
            });
        } else if let Some((title, items)) = picker {
            self.picker = Some(Picker {
                title: title.into(),
                items,
                index: 0,
                kind: PickerKind::Wizard,
            });
        }
        cx.notify();
    }

    pub(crate) fn user_config_path(&mut self) -> Option<PathBuf> {
        let path = self.factory.config_path.clone();
        if path.is_none() {
            self.notify_user(
                NotificationLevel::Warning,
                tr("No user config path (benchmark mode)"),
            );
        }
        path
    }

    /// Opens `path` in an editor tab, creating it from `template` first
    /// when it does not exist.
    fn open_config_file(&mut self, path: &Path, template: &str, cx: &mut Context<Self>) {
        if !path.exists() {
            let created = path
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::write(path, template));
            if let Err(error) = created {
                self.notify_user(
                    NotificationLevel::Error,
                    trf("Could not write {}: {}", &[&path.display(), &error]),
                );
                return;
            }
        }
        self.open_file(path, None, None, cx);
    }

    /// Edits the user's `config.toml` in place (comments preserved) and
    /// returns its path.
    pub(crate) fn edit_user_config(
        &mut self,
        edit: impl FnOnce(&mut DocumentMut) -> Result<(), String>,
    ) -> Result<PathBuf, String> {
        let path = self
            .user_config_path()
            .ok_or_else(|| tr("No user config path (benchmark mode)").to_owned())?;
        // Never overwrite live, unsaved settings through a wizard.
        if self
            .tabs
            .iter()
            .filter_map(crate::window::Tab::editor)
            .any(|editor| editor.path() == Some(path.as_path()) && editor.buffer.is_dirty())
        {
            return Err(tr("Save or discard unsaved settings before using a wizard").into());
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(_) => return Err(tr("Cannot read user settings").into()),
        };
        let mut doc: DocumentMut = text
            .parse()
            .map_err(|error| trf("Invalid configuration: {}", &[&error]))?;
        edit(&mut doc)?;
        let written = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| {
                let tmp = path.with_extension("toml.tmp");
                std::fs::write(&tmp, doc.to_string())?;
                std::fs::rename(&tmp, &path)
            });
        written.map_err(|error| trf("Could not write {}: {}", &[&path.display(), &error]))?;
        Ok(path)
    }
}

/// The `[key]` table, created as a proper (not inline) table when missing.
fn table_mut<'a>(doc: &'a mut DocumentMut, key: &str) -> Result<&'a mut Table, String> {
    doc.entry(key)
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| format!("{key} is not a table"))
}

/// Appends `table` to the `[[key]]` array of tables, creating it.
fn push_table(doc: &mut DocumentMut, key: &str, table: Table) -> Result<(), String> {
    let entry = doc
        .entry(key)
        .or_insert(Item::ArrayOfTables(ArrayOfTables::new()));
    entry
        .as_array_of_tables_mut()
        .ok_or_else(|| format!("{key} is not an array of tables"))?
        .push(table);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_table_creates_and_appends_arrays_of_tables() {
        let mut doc: DocumentMut = "# keep me\n[ui]\ntheme = \"forge-dark\"\n".parse().unwrap();
        let mut provider = Table::new();
        provider["name"] = value("gpt");
        provider["kind"] = value("openai");
        push_table(&mut doc, "providers", provider).unwrap();
        let mut agent = Table::new();
        agent["name"] = value("opencode");
        push_table(&mut doc, "agents", agent).unwrap();
        table_mut(&mut doc, "router").unwrap()["normal"] = value("gpt");
        table_mut(&mut doc, "ui").unwrap()["language"] = value("english");
        let text = doc.to_string();
        assert!(text.starts_with("# keep me\n"), "{text}");
        assert!(
            text.contains("[[providers]]\nname = \"gpt\"\nkind = \"openai\""),
            "{text}"
        );
        assert!(text.contains("[[agents]]\nname = \"opencode\""), "{text}");
        assert!(text.contains("[router]\nnormal = \"gpt\""), "{text}");
        assert!(text.contains("language = \"english\""), "{text}");
        // The result must still be a valid Forge config.
        let parsed: forge_gui::config::Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.providers[0].name, "gpt");
        assert_eq!(parsed.router.normal, "gpt");
        assert_eq!(parsed.ui.language, Language::English);
    }

    #[test]
    fn templates_are_valid_configs() {
        let user: forge_gui::config::Config = toml::from_str(USER_TEMPLATE).unwrap();
        assert_eq!(user, forge_gui::config::Config::default());
        let _workspace: forge_gui::config::Config = toml::from_str(WORKSPACE_TEMPLATE).unwrap();
    }
}
