//! Capabilities implemented by the current client, not aspirational features.
use lsp_types::{
    ClientCapabilities, CompletionClientCapabilities, CompletionItemCapability,
    GeneralClientCapabilities, GotoCapability, HoverClientCapabilities, MarkupKind,
    PositionEncodingKind, PublishDiagnosticsClientCapabilities, SignatureHelpClientCapabilities,
    TextDocumentClientCapabilities, TextDocumentSyncClientCapabilities,
    WorkspaceClientCapabilities,
};

/// Advertises only implemented request/notification shapes and UTF-16 positions.
#[must_use]
pub fn client_capabilities() -> ClientCapabilities {
    ClientCapabilities {
        workspace: Some(WorkspaceClientCapabilities {
            configuration: Some(true),
            workspace_folders: Some(true),
            apply_edit: Some(false),
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
                    // Plain-text insertion until snippet tabstops are implemented.
                    snippet_support: Some(false),
                    insert_replace_support: Some(true),
                    documentation_format: Some(vec![MarkupKind::Markdown, MarkupKind::PlainText]),
                    ..Default::default()
                }),
                context_support: Some(true),
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
            publish_diagnostics: Some(PublishDiagnosticsClientCapabilities {
                version_support: Some(true),
                related_information: Some(true),
                ..Default::default()
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
