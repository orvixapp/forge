//! Flattening of `WorkspaceEdit` into per-document text edits. Forge applies
//! edits through buffer transactions, so file creation, renaming and
//! deletion are rejected rather than performed behind the editor's back.

use lsp_types::{DocumentChangeOperation, DocumentChanges, OneOf, TextEdit, Uri, WorkspaceEdit};

/// Text edits per document, one entry per document, in the order the
/// server listed them.
pub type DocumentEdits = Vec<(Uri, Vec<TextEdit>)>;

fn push(result: &mut DocumentEdits, uri: &Uri, edits: impl Iterator<Item = TextEdit>) {
    match result.iter_mut().find(|(known, _)| known == uri) {
        Some((_, known)) => known.extend(edits),
        None => result.push((uri.clone(), edits.collect())),
    }
}

/// Flattens a workspace edit into text edits per document.
///
/// # Errors
/// When the edit contains resource operations (create/rename/delete).
pub fn text_edits(edit: &WorkspaceEdit) -> Result<DocumentEdits, String> {
    let mut result = DocumentEdits::new();
    // `documentChanges` wins when both are present (LSP 3.16 §workspaceEdit).
    match &edit.document_changes {
        Some(DocumentChanges::Edits(edits)) => {
            for document in edits {
                push(
                    &mut result,
                    &document.text_document.uri,
                    document.edits.iter().cloned().map(plain),
                );
            }
        }
        Some(DocumentChanges::Operations(operations)) => {
            for operation in operations {
                match operation {
                    DocumentChangeOperation::Edit(document) => push(
                        &mut result,
                        &document.text_document.uri,
                        document.edits.iter().cloned().map(plain),
                    ),
                    DocumentChangeOperation::Op(_) => {
                        return Err(
                            "the edit creates, renames or deletes files, which Forge does not apply"
                                .into(),
                        );
                    }
                }
            }
        }
        None => {
            let mut changes: Vec<_> = edit.changes.iter().flatten().collect();
            changes.sort_by(|(a, _), (b, _)| a.as_str().cmp(b.as_str()));
            for (uri, edits) in changes {
                push(&mut result, uri, edits.iter().cloned());
            }
        }
    }
    Ok(result)
}

fn plain(edit: OneOf<TextEdit, lsp_types::AnnotatedTextEdit>) -> TextEdit {
    match edit {
        OneOf::Left(edit) => edit,
        OneOf::Right(annotated) => annotated.text_edit,
    }
}

/// Total number of text edits across documents.
#[must_use]
pub fn edit_count(edits: &DocumentEdits) -> usize {
    edits.iter().map(|(_, edits)| edits.len()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{
        CreateFile, OptionalVersionedTextDocumentIdentifier, Position, Range, ResourceOp,
        TextDocumentEdit,
    };
    use std::str::FromStr;

    fn edit_at(line: u32) -> TextEdit {
        TextEdit {
            range: Range::new(Position::new(line, 0), Position::new(line, 1)),
            new_text: "x".into(),
        }
    }

    #[test]
    fn document_changes_take_precedence_and_resource_ops_are_refused() {
        let uri = Uri::from_str("file:///w/a.rs").unwrap();
        let edit = WorkspaceEdit {
            changes: Some(std::iter::once((uri.clone(), vec![edit_at(9)])).collect()),
            document_changes: Some(DocumentChanges::Edits(vec![TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier {
                    uri: uri.clone(),
                    version: Some(3),
                },
                edits: vec![OneOf::Left(edit_at(1)), OneOf::Left(edit_at(2))],
            }])),
            change_annotations: None,
        };
        let flat = text_edits(&edit).unwrap();
        assert_eq!(edit_count(&flat), 2);
        assert_eq!(flat[0].1[0].range.start.line, 1);
        let create = WorkspaceEdit {
            changes: None,
            document_changes: Some(DocumentChanges::Operations(vec![
                DocumentChangeOperation::Op(ResourceOp::Create(CreateFile {
                    uri,
                    options: None,
                    annotation_id: None,
                })),
            ])),
            change_annotations: None,
        };
        assert!(text_edits(&create).is_err());
    }
}
