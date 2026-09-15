//! Client capabilities declaration for Forge LSP client.

use lsp_types::*;

/// Builds standard `ClientCapabilities` advertised by Forge to language servers.
#[must_use]
pub fn client_capabilities() -> ClientCapabilities {
    ClientCapabilities {
        workspace: Some(WorkspaceClientCapabilities {
            apply_edit: Some(true),
            workspace_edit: Some(WorkspaceEditClientCapabilities {
                document_changes: Some(true),
                resource_operations: Some(vec![
                    ResourceOperationKind::Create,
                    ResourceOperationKind::Rename,
                    ResourceOperationKind::Delete,
                ]),
                failure_handling: Some(FailureHandlingKind::TextOnlyTransactional),
                normalizes_line_endings: Some(true),
                change_annotation_support: None,
            }),
            symbol: Some(WorkspaceSymbolClientCapabilities {
                dynamic_registration: Some(false),
                symbol_kind: Some(SymbolKindCapability {
                    value_set: None,
                }),
                tag_support: None,
                resolve_support: None,
            }),
            configuration: Some(true),
            did_change_watched_files: Some(DidChangeWatchedFilesClientCapabilities {
                dynamic_registration: Some(true),
                relative_pattern_support: Some(true),
            }),
            workspace_folders: Some(true),
            file_operations: None,
            inline_value: None,
            inlay_hint: None,
            diagnostic: Some(DiagnosticWorkspaceClientCapabilities {
                refresh_support: Some(true),
            }),
            code_lens: None,
            semantic_tokens: None,
            execute_command: None,
            did_change_configuration: Some(DynamicRegistrationClientCapabilities {
                dynamic_registration: Some(true),
            }),
        }),
        text_document: Some(TextDocumentClientCapabilities {
            synchronization: Some(TextDocumentSyncClientCapabilities {
                dynamic_registration: Some(false),
                will_save: Some(false),
                will_save_wait_until: Some(true),
                did_save: Some(true),
            }),
            completion: Some(CompletionClientCapabilities {
                dynamic_registration: Some(false),
                completion_item: Some(CompletionItemCapability {
                    snippet_support: Some(true),
                    commit_characters_support: Some(true),
                    documentation_format: Some(vec![MarkupKind::Markdown, MarkupKind::PlainText]),
                    deprecated_support: Some(true),
                    preselect_support: Some(true),
                    tag_support: None,
                    insert_replace_support: Some(true),
                    resolve_support: Some(CompletionItemCapabilityResolveSupport {
                        properties: vec![
                            "documentation".to_string(),
                            "detail".to_string(),
                            "additionalTextEdits".to_string(),
                        ],
                    }),
                    insert_text_mode_support: None,
                    label_details_support: Some(true),
                }),
                completion_item_kind: Some(CompletionItemKindCapability {
                    value_set: None,
                }),
                context_support: Some(true),
                insert_text_mode: None,
                completion_list: None,
            }),
            hover: Some(HoverClientCapabilities {
                dynamic_registration: Some(false),
                content_format: Some(vec![MarkupKind::Markdown, MarkupKind::PlainText]),
            }),
            signature_help: Some(SignatureHelpClientCapabilities {
                dynamic_registration: Some(false),
                signature_information: Some(SignatureInformationSettings {
                    documentation_format: Some(vec![MarkupKind::Markdown, MarkupKind::PlainText]),
                    parameter_information: Some(ParameterInformationSettings {
                        label_offset_support: Some(true),
                    }),
                    active_parameter_support: Some(true),
                }),
                context_support: Some(true),
            }),
            references: Some(ReferenceClientCapabilities {
                dynamic_registration: Some(false),
            }),
            document_highlight: Some(DocumentHighlightClientCapabilities {
                dynamic_registration: Some(false),
            }),
            document_symbol: Some(DocumentSymbolClientCapabilities {
                dynamic_registration: Some(false),
                symbol_kind: Some(SymbolKindCapability {
                    value_set: None,
                }),
                hierarchical_document_symbol_support: Some(true),
                tag_support: None,
            }),
            formatting: Some(DocumentFormattingClientCapabilities {
                dynamic_registration: Some(false),
            }),
            range_formatting: Some(DocumentRangeFormattingClientCapabilities {
                dynamic_registration: Some(false),
            }),
            on_type_formatting: None,
            declaration: Some(GotoCapability {
                dynamic_registration: Some(false),
                link_support: Some(true),
            }),
            definition: Some(GotoCapability {
                dynamic_registration: Some(false),
                link_support: Some(true),
            }),
            type_definition: Some(GotoCapability {
                dynamic_registration: Some(false),
                link_support: Some(true),
            }),
            implementation: Some(GotoCapability {
                dynamic_registration: Some(false),
                link_support: Some(true),
            }),
            code_action: Some(CodeActionClientCapabilities {
                dynamic_registration: Some(false),
                code_action_literal_support: Some(CodeActionLiteralSupport {
                    code_action_kind: CodeActionKindLiteralSupport {
                        value_set: vec![
                            CodeActionKind::QUICKFIX.as_str().to_string(),
                            CodeActionKind::REFACTOR.as_str().to_string(),
                            CodeActionKind::REFACTOR_EXTRACT.as_str().to_string(),
                            CodeActionKind::REFACTOR_INLINE.as_str().to_string(),
                            CodeActionKind::REFACTOR_REWRITE.as_str().to_string(),
                            CodeActionKind::SOURCE.as_str().to_string(),
                            CodeActionKind::SOURCE_ORGANIZE_IMPORTS.as_str().to_string(),
                        ],
                    },
                }),
                is_preferred_support: Some(true),
                disabled_support: Some(true),
                data_support: Some(true),
                resolve_support: None,
                honors_change_annotations: None,
            }),
            code_lens: None,
            document_link: None,
            color_provider: None,
            rename: Some(RenameClientCapabilities {
                dynamic_registration: Some(false),
                prepare_support: Some(true),
                prepare_support_default_behavior: None,
                honors_change_annotations: None,
            }),
            publish_diagnostics: Some(PublishDiagnosticsClientCapabilities {
                related_information: Some(true),
                tag_support: None,
                version_support: Some(true),
                code_description_support: Some(true),
                data_support: Some(true),
            }),
            folding_range: None,
            selection_range: None,
            call_hierarchy: None,
            semantic_tokens: None,
            linked_editing_range: None,
            moniker: None,
            type_hierarchy: None,
            inline_value: None,
            inlay_hint: None,
            diagnostic: Some(DiagnosticClientCapabilities {
                dynamic_registration: Some(false),
                related_document_support: Some(false),
            }),
        }),
        window: Some(WindowClientCapabilities {
            work_done_progress: Some(true),
            show_message: None,
            show_document: None,
        }),
        general: Some(GeneralClientCapabilities {
            stale_request_support: None,
            regular_expressions: None,
            markdown: None,
            position_encodings: Some(vec![PositionEncodingKind::UTF16, PositionEncodingKind::UTF8]),
        }),
        notebook_document: None,
        experimental: None,
    }
}
