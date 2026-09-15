// Included from lsp.rs to test the pure UI/worker boundary without GPUI.
use super::*;

#[test]
fn coalesced_snapshot_delta_preserves_unicode_and_multiple_changes() {
    for (before, after) in [
        ("a😀b\nold\n", "a字b!\nnew\n"),
        ("abc", "abc"),
        ("", "😀"),
        ("abc", ""),
    ] {
        let old = Rope::from_str(before);
        let new = Rope::from_str(after);
        let mut applied = old.clone();
        for edit in snapshot_delta(&old, &new) {
            applied.remove(edit.range.clone());
            applied.insert(edit.range.start, &edit.text);
        }
        assert_eq!(applied.to_string(), after);
    }
}

#[test]
fn completion_uses_server_replacement_and_additional_edits() {
    let rope = Rope::from_str("😀\nfoo\n");
    let item = lsp_types::CompletionItem {
        label: "display label".into(),
        text_edit: Some(lsp_types::CompletionTextEdit::Edit(lsp_types::TextEdit {
            range: lsp_types::Range::new(
                lsp_types::Position::new(1, 0),
                lsp_types::Position::new(1, 3),
            ),
            new_text: "actual_insert".into(),
        })),
        additional_text_edits: Some(vec![lsp_types::TextEdit {
            range: lsp_types::Range::new(
                lsp_types::Position::new(0, 0),
                lsp_types::Position::new(0, 0),
            ),
            new_text: "use module;\n".into(),
        }]),
        ..Default::default()
    };
    let result = completion_item(&item, &rope, 2, 5).unwrap();
    let edits = result.edits.unwrap();
    assert_eq!(edits[0].range, 2..5);
    assert_eq!(edits[0].text, "actual_insert");
    assert_eq!(edits[1].range, 0..0);
}

#[test]
fn save_as_and_cursor_changes_invalidate_requests() {
    let mut snapshot = Snapshot {
        path: "/tmp/a.rs".into(),
        rope: Rope::new(),
        version: 2,
        cursor: 0,
        dirty: true,
        first_line: 0,
        last_line: 10,
    };
    let original = snapshot.clone();
    snapshot.path = "/tmp/b.rs".into();
    assert!(!same_position(&original, &snapshot));
    snapshot = original.clone();
    snapshot.cursor = 1;
    assert!(!same_position(&original, &snapshot));
}
