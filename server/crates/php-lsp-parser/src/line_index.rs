//! Exact source-coordinate conversion: LSP recognizes CR/LF/CRLF; CST rows only LF.
//! Text is never normalized. Unicode separators are ordinary source characters.

#[cfg(test)]
pub(crate) mod counters {
    thread_local! {pub static BUILDS:std::cell::Cell<usize>=const {std::cell::Cell::new(0)};}
}

/// LSP content spans, including the final empty line after a terminator.
pub fn logical_line_spans(source: &str) -> impl Iterator<Item = std::ops::Range<usize>> + '_ {
    let mut start = 0;
    let mut done = false;
    std::iter::from_fn(move || {
        if done {
            return None;
        }
        let tail = &source.as_bytes()[start..];
        if let Some(relative) = tail.iter().position(|byte| matches!(byte, b'\r' | b'\n')) {
            let end = start + relative;
            let span = start..end;
            start = end
                + if source.as_bytes()[end] == b'\r'
                    && source.as_bytes().get(end + 1) == Some(&b'\n')
                {
                    2
                } else {
                    1
                };
            Some(span)
        } else {
            done = true;
            Some(start..source.len())
        }
    })
}
pub fn logical_lines(source: &str) -> impl Iterator<Item = &str> {
    logical_line_spans(source).map(|span| &source[span])
}
pub fn logical_line_at_offset(source: &str, offset: usize) -> std::ops::Range<usize> {
    let offset = offset.min(source.len());
    let mut spans = logical_line_spans(source).peekable();
    while let Some(span) = spans.next() {
        if spans.peek().is_none_or(|next| offset < next.start) {
            return span;
        }
    }
    source.len()..source.len()
}

#[derive(Debug, Clone)]
struct Line {
    start: usize,
    end: usize,
    // Unicode scalar boundaries relative to this content line; ASCII needs none.
    unicode: Vec<(usize, u32)>,
}

#[cfg(test)]
#[path = "line_index_tests.rs"]
mod tests;

#[derive(Debug, Clone)]
pub struct LineIndex {
    len: usize,
    tree_starts: Vec<usize>,
    lines: Vec<Line>,
}

impl LineIndex {
    pub fn new(source: &str) -> Self {
        #[cfg(test)]
        counters::BUILDS.with(|count| count.set(count.get() + 1));
        let tree_starts = std::iter::once(0)
            .chain(
                source
                    .bytes()
                    .enumerate()
                    .filter_map(|(i, b)| (b == b'\n').then_some(i + 1)),
            )
            .collect();
        let spans = logical_line_spans(source).map(|span| (span.start, span.end));
        let lines = spans
            .map(|(start, end)| {
                let text = &source[start..end];
                let mut unicode = Vec::new();
                if !text.is_ascii() {
                    unicode.push((0, 0));
                    let mut units = 0;
                    for (byte, ch) in text.char_indices() {
                        units += ch.len_utf16() as u32;
                        unicode.push((byte + ch.len_utf8(), units));
                    }
                }
                Line {
                    start,
                    end,
                    unicode,
                }
            })
            .collect();
        Self {
            len: source.len(),
            tree_starts,
            lines,
        }
    }

    pub fn len_bytes(&self) -> usize {
        self.len
    }
    pub fn has_bare_cr(&self) -> bool {
        self.lines.len() > self.tree_starts.len()
    }
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }
    pub fn line_range(&self, line: u32) -> Option<std::ops::Range<usize>> {
        self.lines
            .get(line as usize)
            .map(|line| line.start..line.end)
    }
    pub fn line_range_with_ending(&self, line: u32) -> Option<std::ops::Range<usize>> {
        let content = self.line_range(line)?;
        Some(
            content.start
                ..self
                    .lines
                    .get(line as usize + 1)
                    .map_or(self.len, |line| line.start),
        )
    }
    pub fn lines<'a>(&'a self, source: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.lines
            .iter()
            .map(move |line| &source[line.start..line.end])
    }

    /// Invalid lines are caller policy; columns clamp before CR/LF and at scalar boundaries.
    pub fn position_to_offset(&self, line: u32, column: u32) -> Option<usize> {
        let line = self.lines.get(line as usize)?;
        let byte = if line.unicode.is_empty() {
            (column as usize).min(line.end - line.start)
        } else {
            let i = line
                .unicode
                .partition_point(|(_, units)| *units <= column)
                .saturating_sub(1);
            line.unicode[i].0
        };
        Some(line.start + byte)
    }
    pub fn point_to_offset(&self, row: u32, byte_column: u32) -> Option<usize> {
        let start = *self.tree_starts.get(row as usize)?;
        let end = self
            .tree_starts
            .get(row as usize + 1)
            .map_or(self.len, |next| next - 1);
        Some(start + (byte_column as usize).min(end - start))
    }
    pub fn offset_to_point(&self, offset: usize) -> (u32, u32) {
        let offset = offset.min(self.len);
        let row = self
            .tree_starts
            .partition_point(|start| *start <= offset)
            .saturating_sub(1);
        (row as u32, (offset - self.tree_starts[row]) as u32)
    }
    /// Offsets inside a terminator map to the preceding content EOL. After CRLF is next line 0.
    pub fn offset_to_position(&self, offset: usize) -> (u32, u32) {
        let offset = offset.min(self.len);
        let row = self
            .lines
            .partition_point(|line| line.start <= offset)
            .saturating_sub(1);
        let line = &self.lines[row];
        let byte = (offset - line.start).min(line.end - line.start);
        let column = if line.unicode.is_empty() {
            byte as u32
        } else {
            let i = line
                .unicode
                .partition_point(|(b, _)| *b <= byte)
                .saturating_sub(1);
            line.unicode[i].1
        };
        (row as u32, column)
    }
    /// Convert a complete CST byte point. Out-of-document rows remain invalid.
    pub fn point_to_position(&self, row: u32, byte_column: u32) -> (u32, u32) {
        self.point_to_offset(row, byte_column)
            .map_or((row.max(self.lines.len() as u32), byte_column), |offset| {
                self.offset_to_position(offset)
            })
    }
    /// Convert a complete LSP position. Out-of-document rows remain invalid.
    pub fn position_to_point(&self, line: u32, column: u32) -> (u32, u32) {
        self.position_to_offset(line, column)
            .map_or((line, column), |offset| self.offset_to_point(offset))
    }
    pub fn range_to_lsp(&self, range: (u32, u32, u32, u32)) -> (u32, u32, u32, u32) {
        let a = self.point_to_position(range.0, range.1);
        let b = self.point_to_position(range.2, range.3);
        (a.0, a.1, b.0, b.1)
    }
    pub fn range_to_points(&self, range: (u32, u32, u32, u32)) -> (u32, u32, u32, u32) {
        let a = self.position_to_point(range.0, range.1);
        let b = self.position_to_point(range.2, range.3);
        (a.0, a.1, b.0, b.1)
    }

    /// Non-empty per-line UTF-16 spans, excluding all line terminator bytes.
    pub fn segments(&self, start: usize, end: usize) -> impl Iterator<Item = (u32, u32, u32)> + '_ {
        let rows = if start < end {
            self.offset_to_position(start).0..self.offset_to_position(end).0 + 1
        } else {
            0..0
        };
        rows.filter_map(move |row| {
            let line = &self.lines[row as usize];
            let a = start.max(line.start);
            let b = end.min(line.end);
            if a >= b {
                return None;
            }
            let start = self.offset_to_position(a).1;
            let end = self.offset_to_position(b).1;
            (end > start).then_some((row, start, end))
        })
    }

    /// Legacy column-only conversion. Protocol code must use `point_to_position`.
    #[deprecated(note = "Use point_to_position: bare CR can change both row and column")]
    pub fn byte_col_to_utf16(&self, row: u32, byte_column: u32) -> u32 {
        self.point_to_position(row, byte_column).1
    }
}
