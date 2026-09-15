//! Capabilities implemented by the current client, not aspirational features.
use lsp_types::{
    ClientCapabilities, CodeActionCapabilityResolveSupport, CodeActionClientCapabilities,
    CodeActionKind, CodeActionKindLiteralSupport, CodeActionLiteralSupport,
    CompletionClientCapabilities, CompletionItemCapability, CompletionItemCapabilityResolveSupport,
    DiagnosticClientCapabilities, DocumentFormattingClientCapabilities,
    DynamicRegistrationClientCapabilities, GeneralClientCapabilities, GotoCapability,
    HoverClientCapabilities, InsertTextMode, MarkupKind, PositionEncodingKind,
    PublishDiagnosticsClientCapabilities, RenameClientCapabilities,
    SignatureHelpClientCapabilities, TextDocumentClientCapabilities,
    TextDocumentSyncClientCapabilities, WorkspaceClientCapabilities,
    WorkspaceEditClientCapabilities, WorkspaceSymbolClientCapabilities,
};

/// Advertises only implemented request/notification shapes and UTF-16 positions.
#[must_use]
pub fn client_capabilities() -> ClientCapabilities {
    ClientCapabilities {
        workspace: Some(WorkspaceClientCapabilities {
            configuration: Some(true),
            workspace_folders: Some(true),
            // Server-initiated `workspace/applyEdit` is not implemented; the
            // edits Forge applies are answers to its own requests.
            apply_edit: Some(false),
            workspace_edit: Some(WorkspaceEditClientCapabilities {
                document_changes: Some(true),
                // Only text edits: file creation/rename/deletion is rejected.
                resource_operations: None,
                ..Default::default()
            }),
            symbol: Some(WorkspaceSymbolClientCapabilities::default()),
            ..Default::default()
        }),
        text_document: Some(TextDocumentClientCapabilities {
            synchronization: Some(TextDocumentSyncClientCapabilities {
                dynamic_registration: Some(false),
                did_save: Some(true),
                ..Default::default()
            }),
            completion: Some(CompletionClientCapabilities {
                completion_item: Some(CompletionItemCapability {
                    // Tabstops/placeholders are expanded by the editor; choices
                    // take their first option, variables resolve to nothing.
                    snippet_support: Some(true),
                    insert_replace_support: Some(true),
                    documentation_format: Some(vec![MarkupKind::Markdown, MarkupKind::PlainText]),
                    insert_text_mode_support: None,
                    resolve_support: Some(CompletionItemCapabilityResolveSupport {
                        properties: vec![
                            "documentation".into(),
                            "detail".into(),
                            "additionalTextEdits".into(),
                        ],
                    }),
                    ..Default::default()
                }),
                context_support: Some(true),
                insert_text_mode: Some(InsertTextMode::AS_IS),
                ..Default::default()
            }),
            hover: Some(HoverClientCapabilities {
                content_format: Some(vec![MarkupKind::Markdown, MarkupKind::PlainText]),
                ..Default::default()
            }),
            signature_help: Some(SignatureHelpClientCapabilities::default()),
            definition: Some(GotoCapability {
                link_support: Some(true),
                ..Default::default()
            }),
            implementation: Some(GotoCapability {
                link_support: Some(true),
                ..Default::default()
            }),
            references: Some(DynamicRegistrationClientCapabilities::default()),
            code_action: Some(CodeActionClientCapabilities {
                code_action_literal_support: Some(CodeActionLiteralSupport {
                    code_action_kind: CodeActionKindLiteralSupport {
                        value_set: vec![
                            CodeActionKind::QUICKFIX.as_str().to_owned(),
                            CodeActionKind::REFACTOR.as_str().to_owned(),
                            CodeActionKind::REFACTOR_EXTRACT.as_str().to_owned(),
                            CodeActionKind::REFACTOR_INLINE.as_str().to_owned(),
                            CodeActionKind::REFACTOR_REWRITE.as_str().to_owned(),
                            CodeActionKind::SOURCE.as_str().to_owned(),
                            CodeActionKind::SOURCE_ORGANIZE_IMPORTS.as_str().to_owned(),
                        ],
                    },
                }),
                // Edits are resolved lazily so listing actions stays cheap.
                resolve_support: Some(CodeActionCapabilityResolveSupport {
                    properties: vec!["edit".into()],
                }),
                data_support: Some(true),
                ..Default::default()
            }),
            formatting: Some(DocumentFormattingClientCapabilities::default()),
            rename: Some(RenameClientCapabilities {
                prepare_support: Some(true),
                ..Default::default()
            }),
            publish_diagnostics: Some(PublishDiagnosticsClientCapabilities {
                version_support: Some(true),
                related_information: Some(true),
                ..Default::default()
            }),
            diagnostic: Some(DiagnosticClientCapabilities {
                dynamic_registration: Some(false),
                related_document_support: Some(true),
            }),
            ..Default::default()
        }),
        general: Some(GeneralClientCapabilities {
            position_encodings: Some(vec![PositionEncodingKind::UTF16]),
            ..Default::default()
        }),
        ..Default::default()
    }
}
