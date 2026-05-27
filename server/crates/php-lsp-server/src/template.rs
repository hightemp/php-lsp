use php_lsp_parser::utf16::{utf16_col_to_byte, Utf16LineIndex};
use tower_lsp::ls_types::{Position, Range, SemanticToken};

#[derive(Debug, Clone)]
pub(crate) struct TemplateDocument {
    original_source: String,
    virtual_source: String,
    source_map: TemplateSourceMap,
    semantic_tokens: Vec<TemplateSemanticToken>,
}

impl TemplateDocument {
    pub(crate) fn virtual_source(&self) -> &str {
        &self.virtual_source
    }

    pub(crate) fn map_original_position_to_virtual(&self, position: Position) -> Option<Position> {
        let original_offset = byte_offset_for_position(&self.original_source, position)?;
        let virtual_offset = self.source_map.original_to_virtual(original_offset)?;
        Some(position_for_byte_offset(
            &self.virtual_source,
            virtual_offset,
        ))
    }

    pub(crate) fn map_virtual_range_to_original(&self, range: Range) -> Option<Range> {
        let virtual_start = byte_offset_for_position(&self.virtual_source, range.start)?;
        let virtual_end = byte_offset_for_position(&self.virtual_source, range.end)?;
        let (original_start, original_end) = self
            .source_map
            .virtual_range_to_original(virtual_start, virtual_end)?;
        Some(range_for_byte_offsets(
            &self.original_source,
            original_start,
            original_end,
        ))
    }

    pub(crate) fn map_diagnostics_to_original(
        &self,
        diagnostics: Vec<tower_lsp::ls_types::Diagnostic>,
    ) -> Vec<tower_lsp::ls_types::Diagnostic> {
        diagnostics
            .into_iter()
            .filter_map(|mut diagnostic| {
                diagnostic.range = self.map_virtual_range_to_original(diagnostic.range)?;
                Some(diagnostic)
            })
            .collect()
    }

    pub(crate) fn map_semantic_tokens_to_original(
        &self,
        tokens: Vec<SemanticToken>,
    ) -> Vec<SemanticToken> {
        let mut absolute = Vec::new();
        for token in decode_semantic_tokens(&tokens) {
            let start = Position::new(token.line, token.start);
            let end = Position::new(token.line, token.start.saturating_add(token.length));
            let Some(virtual_start) = byte_offset_for_position(&self.virtual_source, start) else {
                continue;
            };
            let Some(virtual_end) = byte_offset_for_position(&self.virtual_source, end) else {
                continue;
            };
            let Some((original_start, original_end)) = self
                .source_map
                .virtual_range_to_original(virtual_start, virtual_end)
            else {
                continue;
            };
            push_original_semantic_token(
                &self.original_source,
                original_start,
                original_end,
                token.token_type,
                token.token_modifiers_bitset,
                &mut absolute,
            );
        }

        for token in &self.semantic_tokens {
            push_original_semantic_token(
                &self.original_source,
                token.original_start,
                token.original_end,
                token.token_type,
                token.token_modifiers_bitset,
                &mut absolute,
            );
        }

        normalize_and_encode_semantic_tokens(absolute)
    }

    pub(crate) fn map_semantic_tokens_range_to_original(
        &self,
        tokens: Vec<SemanticToken>,
        requested_range: Range,
    ) -> Vec<SemanticToken> {
        let absolute: Vec<_> =
            decode_semantic_tokens(&self.map_semantic_tokens_to_original(tokens))
                .into_iter()
                .filter(|token| semantic_token_overlaps_range(*token, requested_range))
                .collect();
        encode_semantic_tokens(&absolute)
    }

    pub(crate) fn apply_change(&self, range: Option<Range>, text: &str) -> Self {
        let mut source = self.original_source.clone();
        apply_text_change(&mut source, range, text);
        preprocess_blade_template(&source)
    }
}

#[derive(Debug, Clone, Copy)]
struct TemplateSemanticToken {
    original_start: usize,
    original_end: usize,
    token_type: u32,
    token_modifiers_bitset: u32,
}

#[derive(Debug, Clone, Default)]
struct TemplateSourceMap {
    segments: Vec<SourceMapSegment>,
}

impl TemplateSourceMap {
    fn push_segment(&mut self, original_start: usize, original_end: usize, virtual_start: usize) {
        if original_end < original_start {
            return;
        }
        let len = original_end.saturating_sub(original_start);
        self.segments.push(SourceMapSegment {
            original_start,
            original_end,
            virtual_start,
            virtual_end: virtual_start + len,
        });
    }

    fn original_to_virtual(&self, offset: usize) -> Option<usize> {
        let segment = self
            .segments
            .iter()
            .find(|segment| segment.original_start <= offset && offset <= segment.original_end)?;
        Some(
            segment.virtual_start
                + offset
                    .saturating_sub(segment.original_start)
                    .min(segment.original_len()),
        )
    }

    fn virtual_range_to_original(
        &self,
        virtual_start: usize,
        virtual_end: usize,
    ) -> Option<(usize, usize)> {
        let mut original_start: Option<usize> = None;
        let mut original_end: Option<usize> = None;

        for segment in &self.segments {
            if virtual_start == virtual_end
                && segment.virtual_start <= virtual_start
                && virtual_start <= segment.virtual_end
            {
                let original = segment.original_start
                    + virtual_start
                        .saturating_sub(segment.virtual_start)
                        .min(segment.original_len());
                return Some((original, original));
            }

            let overlap_start = virtual_start.max(segment.virtual_start);
            let overlap_end = virtual_end.min(segment.virtual_end);
            if overlap_start > overlap_end {
                continue;
            }
            if overlap_start == overlap_end
                && !(segment.virtual_start <= overlap_start && overlap_start <= segment.virtual_end)
            {
                continue;
            }

            let mapped_start = segment.original_start
                + overlap_start
                    .saturating_sub(segment.virtual_start)
                    .min(segment.original_len());
            let mapped_end = segment.original_start
                + overlap_end
                    .saturating_sub(segment.virtual_start)
                    .min(segment.original_len());
            original_start =
                Some(original_start.map_or(mapped_start, |current| current.min(mapped_start)));
            original_end = Some(original_end.map_or(mapped_end, |current| current.max(mapped_end)));
        }

        Some((original_start?, original_end?))
    }
}

#[derive(Debug, Clone, Copy)]
struct SourceMapSegment {
    original_start: usize,
    original_end: usize,
    virtual_start: usize,
    virtual_end: usize,
}

impl SourceMapSegment {
    fn original_len(self) -> usize {
        self.original_end.saturating_sub(self.original_start)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AbsoluteSemanticToken {
    line: u32,
    start: u32,
    length: u32,
    token_type: u32,
    token_modifiers_bitset: u32,
}

const TOKEN_KEYWORD: u32 = 11;
const TOKEN_COMMENT: u32 = 13;

pub(crate) fn is_blade_template_uri(uri_str: &str) -> bool {
    uri_str.ends_with(".blade.php")
}

pub(crate) fn is_blade_template_language_id(language_id: &str) -> bool {
    matches!(language_id, "blade" | "laravel-blade")
}

pub(crate) fn preprocess_blade_template(source: &str) -> TemplateDocument {
    let mut virtual_source = String::new();
    let mut source_map = TemplateSourceMap::default();
    let mut semantic_tokens = Vec::new();
    let mut offset = 0usize;

    while offset < source.len() {
        if source[offset..].starts_with("{{--") {
            let end = source[offset + 4..]
                .find("--}}")
                .map(|relative| offset + 4 + relative + 4)
                .unwrap_or(source.len());
            semantic_tokens.push(TemplateSemanticToken {
                original_start: offset,
                original_end: end,
                token_type: TOKEN_COMMENT,
                token_modifiers_bitset: 0,
            });
            offset = end;
            continue;
        }

        if source[offset..].starts_with("{{") {
            if let Some(end) = source[offset + 2..]
                .find("}}")
                .map(|relative| offset + 2 + relative)
            {
                push_mapped_php_fragment(
                    source,
                    offset + 2,
                    end,
                    "<?php echo ",
                    "; ?>\n",
                    &mut virtual_source,
                    &mut source_map,
                );
                offset = end + 2;
                continue;
            }
        }

        if source[offset..].starts_with("{!!") {
            if let Some(end) = source[offset + 3..]
                .find("!!}")
                .map(|relative| offset + 3 + relative)
            {
                push_mapped_php_fragment(
                    source,
                    offset + 3,
                    end,
                    "<?php echo ",
                    "; ?>\n",
                    &mut virtual_source,
                    &mut source_map,
                );
                offset = end + 3;
                continue;
            }
        }

        if source.as_bytes()[offset] == b'@' && !directive_is_escaped(source, offset) {
            if let Some(next_offset) = push_directive_fragment(
                source,
                offset,
                &mut virtual_source,
                &mut source_map,
                &mut semantic_tokens,
            ) {
                offset = next_offset;
                continue;
            }
        }

        offset += source[offset..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(1);
    }

    TemplateDocument {
        original_source: source.to_string(),
        virtual_source,
        source_map,
        semantic_tokens,
    }
}

fn push_mapped_php_fragment(
    source: &str,
    original_start: usize,
    original_end: usize,
    prefix: &str,
    suffix: &str,
    virtual_source: &mut String,
    source_map: &mut TemplateSourceMap,
) {
    let virtual_start = virtual_source.len() + prefix.len();
    virtual_source.push_str(prefix);
    virtual_source.push_str(source.get(original_start..original_end).unwrap_or(""));
    virtual_source.push_str(suffix);
    source_map.push_segment(original_start, original_end, virtual_start);
}

fn push_directive_fragment(
    source: &str,
    offset: usize,
    virtual_source: &mut String,
    source_map: &mut TemplateSourceMap,
    semantic_tokens: &mut Vec<TemplateSemanticToken>,
) -> Option<usize> {
    let name_start = offset + 1;
    let name_end = scan_identifier_end(source, name_start);
    if name_end == name_start {
        return None;
    }

    let name = source.get(name_start..name_end)?;
    semantic_tokens.push(TemplateSemanticToken {
        original_start: offset,
        original_end: name_end,
        token_type: TOKEN_KEYWORD,
        token_modifiers_bitset: 0,
    });

    match name {
        "if" | "elseif" | "foreach" | "isset" | "empty" => {
            let (args_start, args_end, directive_end) = directive_args_range(source, name_end)?;
            match name {
                "if" => push_mapped_php_fragment(
                    source,
                    args_start,
                    args_end,
                    "<?php if (",
                    "): ?>\n",
                    virtual_source,
                    source_map,
                ),
                "elseif" => push_mapped_php_fragment(
                    source,
                    args_start,
                    args_end,
                    "<?php elseif (",
                    "): ?>\n",
                    virtual_source,
                    source_map,
                ),
                "foreach" => push_mapped_php_fragment(
                    source,
                    args_start,
                    args_end,
                    "<?php foreach (",
                    "): ?>\n",
                    virtual_source,
                    source_map,
                ),
                "isset" => push_mapped_php_fragment(
                    source,
                    args_start,
                    args_end,
                    "<?php if (isset(",
                    ")): ?>\n",
                    virtual_source,
                    source_map,
                ),
                "empty" => push_mapped_php_fragment(
                    source,
                    args_start,
                    args_end,
                    "<?php if (empty(",
                    ")): ?>\n",
                    virtual_source,
                    source_map,
                ),
                _ => {}
            }
            Some(directive_end)
        }
        "else" => {
            virtual_source.push_str("<?php else: ?>\n");
            Some(name_end)
        }
        "endif" | "endisset" | "endempty" => {
            virtual_source.push_str("<?php endif; ?>\n");
            Some(name_end)
        }
        "endforeach" => {
            virtual_source.push_str("<?php endforeach; ?>\n");
            Some(name_end)
        }
        _ => None,
    }
}

fn directive_is_escaped(source: &str, offset: usize) -> bool {
    offset > 0 && source.as_bytes().get(offset - 1) == Some(&b'@')
}

fn scan_identifier_end(source: &str, start: usize) -> usize {
    let mut end = start;
    for (relative, ch) in source[start..].char_indices() {
        if relative == 0 {
            if !ch.is_ascii_alphabetic() && ch != '_' {
                return start;
            }
        } else if !ch.is_ascii_alphanumeric() && ch != '_' {
            break;
        }
        end = start + relative + ch.len_utf8();
    }
    end
}

fn directive_args_range(source: &str, after_name: usize) -> Option<(usize, usize, usize)> {
    let mut offset = after_name;
    while offset < source.len() {
        let ch = source[offset..].chars().next()?;
        if !ch.is_whitespace() {
            break;
        }
        offset += ch.len_utf8();
    }

    if source.as_bytes().get(offset) != Some(&b'(') {
        return None;
    }

    let close = find_matching_paren(source, offset)?;
    Some((offset + 1, close, close + 1))
}

fn find_matching_paren(source: &str, open: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut depth = 0usize;
    let mut quote = None;
    let mut escaped = false;
    let mut offset = open;

    while offset < bytes.len() {
        let byte = bytes[offset];
        if let Some(quote_byte) = quote {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == quote_byte {
                quote = None;
            }
            offset += 1;
            continue;
        }

        match byte {
            b'\'' | b'"' => quote = Some(byte),
            b'(' => depth += 1,
            b')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(offset);
                }
            }
            _ => {}
        }
        offset += 1;
    }

    None
}

pub(crate) fn apply_text_change(source: &mut String, range: Option<Range>, text: &str) {
    let Some(range) = range else {
        source.clear();
        source.push_str(text);
        return;
    };
    let Some(start) = byte_offset_for_position(source, range.start) else {
        return;
    };
    let Some(end) = byte_offset_for_position(source, range.end) else {
        return;
    };
    source.replace_range(start.min(end)..end.max(start), text);
}

fn byte_offset_for_position(source: &str, position: Position) -> Option<usize> {
    let line_start = *line_start_offsets(source).get(position.line as usize)?;
    let byte_col = utf16_col_to_byte(source, position.line, position.character) as usize;
    Some((line_start + byte_col).min(source.len()))
}

fn position_for_byte_offset(source: &str, offset: usize) -> Position {
    let offsets = line_start_offsets(source);
    let line_idx = offsets
        .partition_point(|line_start| *line_start <= offset)
        .saturating_sub(1);
    let line_start = offsets.get(line_idx).copied().unwrap_or(0);
    let byte_col = offset.saturating_sub(line_start) as u32;
    let utf16_index = Utf16LineIndex::new(source);
    Position::new(
        line_idx as u32,
        utf16_index.byte_col_to_utf16(line_idx as u32, byte_col),
    )
}

fn range_for_byte_offsets(source: &str, start: usize, end: usize) -> Range {
    Range {
        start: position_for_byte_offset(source, start),
        end: position_for_byte_offset(source, end),
    }
}

fn line_start_offsets(source: &str) -> Vec<usize> {
    let mut offsets = vec![0];
    for (idx, byte) in source.bytes().enumerate() {
        if byte == b'\n' {
            offsets.push(idx + 1);
        }
    }
    offsets
}

fn decode_semantic_tokens(tokens: &[SemanticToken]) -> Vec<AbsoluteSemanticToken> {
    let mut line = 0u32;
    let mut start = 0u32;
    tokens
        .iter()
        .map(|token| {
            line = line.saturating_add(token.delta_line);
            if token.delta_line == 0 {
                start = start.saturating_add(token.delta_start);
            } else {
                start = token.delta_start;
            }
            AbsoluteSemanticToken {
                line,
                start,
                length: token.length,
                token_type: token.token_type,
                token_modifiers_bitset: token.token_modifiers_bitset,
            }
        })
        .collect()
}

fn encode_semantic_tokens(tokens: &[AbsoluteSemanticToken]) -> Vec<SemanticToken> {
    let mut previous_line = 0u32;
    let mut previous_start = 0u32;

    tokens
        .iter()
        .enumerate()
        .map(|(index, token)| {
            let delta_line = if index == 0 {
                token.line
            } else {
                token.line.saturating_sub(previous_line)
            };
            let delta_start = if delta_line == 0 {
                token.start.saturating_sub(previous_start)
            } else {
                token.start
            };
            previous_line = token.line;
            previous_start = token.start;
            SemanticToken {
                delta_line,
                delta_start,
                length: token.length,
                token_type: token.token_type,
                token_modifiers_bitset: token.token_modifiers_bitset,
            }
        })
        .collect()
}

fn normalize_and_encode_semantic_tokens(
    mut tokens: Vec<AbsoluteSemanticToken>,
) -> Vec<SemanticToken> {
    tokens.retain(|token| token.length > 0);
    tokens.sort_by_key(|token| (token.line, token.start, token.length, token.token_type));
    tokens.dedup_by_key(|token| {
        (
            token.line,
            token.start,
            token.length,
            token.token_type,
            token.token_modifiers_bitset,
        )
    });
    encode_semantic_tokens(&tokens)
}

fn push_original_semantic_token(
    source: &str,
    original_start: usize,
    original_end: usize,
    token_type: u32,
    token_modifiers_bitset: u32,
    tokens: &mut Vec<AbsoluteSemanticToken>,
) {
    if original_end <= original_start {
        return;
    }
    let start = position_for_byte_offset(source, original_start);
    let end = position_for_byte_offset(source, original_end);
    if start.line != end.line {
        let line_offsets = line_start_offsets(source);
        for line in start.line..=end.line {
            let line_start = *line_offsets.get(line as usize).unwrap_or(&source.len());
            let line_end = line_offsets
                .get(line as usize + 1)
                .copied()
                .map(|next| next.saturating_sub(1))
                .unwrap_or(source.len());
            let segment_start = if line == start.line {
                original_start
            } else {
                line_start
            };
            let segment_end = if line == end.line {
                original_end
            } else {
                line_end
            };
            push_original_semantic_token(
                source,
                segment_start,
                segment_end,
                token_type,
                token_modifiers_bitset,
                tokens,
            );
        }
        return;
    }

    tokens.push(AbsoluteSemanticToken {
        line: start.line,
        start: start.character,
        length: end.character.saturating_sub(start.character),
        token_type,
        token_modifiers_bitset,
    });
}

fn semantic_token_overlaps_range(token: AbsoluteSemanticToken, range: Range) -> bool {
    let token_start = Position::new(token.line, token.start);
    let token_end = Position::new(token.line, token.start.saturating_add(token.length));
    position_before(token_start, range.end) && position_before(range.start, token_end)
}

fn position_before(left: Position, right: Position) -> bool {
    left.line < right.line || (left.line == right.line && left.character < right.character)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blade_echo_maps_original_position_to_virtual_php() {
        let doc = preprocess_blade_template("<div>{{ $user->name }}</div>\n");
        assert!(doc
            .virtual_source()
            .contains("<?php echo  $user->name ; ?>"));

        let original_position = Position::new(0, 8);
        let virtual_position = doc
            .map_original_position_to_virtual(original_position)
            .expect("template expression position should map");
        let virtual_offset = byte_offset_for_position(doc.virtual_source(), virtual_position)
            .expect("virtual position offset");
        assert_eq!(
            doc.virtual_source()
                .get(virtual_offset..virtual_offset + "$user".len()),
            Some("$user")
        );
    }

    #[test]
    fn blade_directives_create_virtual_php_and_semantic_tokens() {
        let doc = preprocess_blade_template(
            "@if ($user)\n{{-- comment --}}\n@foreach ($items as $item)\n{{ $item }}\n@endforeach\n@endif\n",
        );
        assert!(doc.virtual_source().contains("<?php if ($user): ?>"));
        assert!(doc
            .virtual_source()
            .contains("<?php foreach ($items as $item): ?>"));
        assert!(doc.virtual_source().contains("<?php endforeach; ?>"));
        assert!(doc.virtual_source().contains("<?php endif; ?>"));

        let tokens = doc.map_semantic_tokens_to_original(Vec::new());
        assert!(
            tokens.iter().any(|token| token.token_type == TOKEN_KEYWORD),
            "expected directive keyword semantic tokens"
        );
        assert!(
            tokens.iter().any(|token| token.token_type == TOKEN_COMMENT),
            "expected comment semantic tokens"
        );
    }

    #[test]
    fn blade_range_mapping_suppresses_unmapped_generated_php() {
        let doc = preprocess_blade_template("<div>{{ $user }}</div>");
        let generated_prefix = Range::new(Position::new(0, 0), Position::new(0, 5));
        assert!(doc
            .map_virtual_range_to_original(generated_prefix)
            .is_none());

        let user_virtual = doc
            .map_original_position_to_virtual(Position::new(0, 8))
            .expect("mapped user position");
        let user_range = Range::new(
            user_virtual,
            Position::new(user_virtual.line, user_virtual.character + 5),
        );
        let original = doc
            .map_virtual_range_to_original(user_range)
            .expect("mapped variable range");
        assert_eq!(original.start, Position::new(0, 8));
        assert_eq!(original.end, Position::new(0, 13));
    }
}
