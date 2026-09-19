use std::path::PathBuf;

use lsp_types::{Position, Range, TextDocumentContentChangeEvent, Url};
use qbx_lua_analysis::scope::{resolve, Resolution};
use qbx_lua_syntax::ast::Chunk;
use qbx_lua_syntax::{parse, LineCol, LineIndex, Span};

use crate::index::FileId;

pub struct Document {
    pub uri: Url,
    pub path: PathBuf,
    pub version: i32,
    pub text: String,
    pub lines: LineIndex,
    pub chunk: Chunk,
    pub resolution: Resolution,
    pub file: FileId,
}

impl Document {
    pub fn new(uri: Url, path: PathBuf, version: i32, text: String) -> Self {
        let lines = LineIndex::new(&text);
        let chunk = parse(&text);
        let resolution = resolve(&chunk);
        Self { uri, path, version, text, lines, chunk, resolution, file: 0 }
    }

    pub fn apply_changes(&mut self, version: i32, changes: Vec<TextDocumentContentChangeEvent>) {
        for change in changes {
            match change.range {
                Some(range) => {
                    let start = self.offset(range.start) as usize;
                    let end = self.offset(range.end) as usize;
                    self.text.replace_range(start..end.max(start), &change.text);
                    self.lines = LineIndex::new(&self.text);
                }
                None => {
                    self.text = change.text;
                    self.lines = LineIndex::new(&self.text);
                }
            }
        }
        self.version = version;
        self.chunk = parse(&self.text);
        self.resolution = resolve(&self.chunk);
    }

    pub fn offset(&self, position: Position) -> u32 {
        self.lines.offset_utf16(&self.text, LineCol { line: position.line, col: position.character })
    }

    pub fn position(&self, offset: u32) -> Position {
        let pos = self.lines.line_col_utf16(&self.text, offset);
        Position::new(pos.line, pos.col)
    }

    pub fn range(&self, span: Span) -> Range {
        Range::new(self.position(span.start), self.position(span.end.max(span.start)))
    }

    pub fn is_manifest(&self) -> bool {
        qbx_lua_analysis::project::is_manifest_file(&self.path)
    }
}
