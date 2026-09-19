use std::collections::HashMap;

use lsp_types::{CodeAction, CodeActionKind, CodeActionOrCommand, Diagnostic, NumberOrString, Position, Range, TextEdit, WorkspaceEdit};

use super::diagnostics::{FixData, SOURCE};
use crate::document::Document;

fn edit_for(doc: &Document, edits: Vec<TextEdit>) -> WorkspaceEdit {
    WorkspaceEdit { changes: Some(HashMap::from([(doc.uri.clone(), edits)])), ..WorkspaceEdit::default() }
}

pub fn code_actions(doc: &Document, diagnostics: &[Diagnostic]) -> Vec<CodeActionOrCommand> {
    let mut actions = Vec::new();
    let ours = diagnostics.iter().filter(|d| d.source.as_deref() == Some(SOURCE));
    for diagnostic in ours {
        let Some(NumberOrString::String(code)) = &diagnostic.code else { continue };
        let fix = diagnostic.data.clone().and_then(|data| serde_json::from_value::<FixData>(data).ok());
        if let Some(fix) = fix {
            let edits = fix.edits.into_iter().map(|(range, text)| TextEdit::new(range, text)).collect();
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: fix.title,
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: Some(vec![diagnostic.clone()]),
                edit: Some(edit_for(doc, edits)),
                is_preferred: Some(true),
                ..CodeAction::default()
            }));
        }
        if code == "syntax-error" {
            continue;
        }
        let line = diagnostic.range.start.line;
        let line_text = doc.lines.line_span(line).text(&doc.text);
        let indent: String = line_text.chars().take_while(|c| c.is_whitespace() && *c != '\n' && *c != '\r').collect();
        let at = Position::new(line, 0);
        actions.push(CodeActionOrCommand::CodeAction(CodeAction {
            title: format!("Disable {code} for this line"),
            kind: Some(CodeActionKind::QUICKFIX),
            diagnostics: Some(vec![diagnostic.clone()]),
            edit: Some(edit_for(
                doc,
                vec![TextEdit::new(Range::new(at, at), format!("{indent}-- qbx-lint: disable-next-line {code}\n"))],
            )),
            ..CodeAction::default()
        }));
    }
    actions
}
