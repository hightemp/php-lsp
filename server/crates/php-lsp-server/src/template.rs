use php_lsp_parser::utf16::{utf16_col_to_byte, Utf16LineIndex};
use std::collections::HashSet;
use tower_lsp::ls_types::{
    Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range, SemanticToken,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TemplateKind {
    Blade,
    Twig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TemplateVariableType {
    pub(crate) name: String,
    pub(crate) type_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TwigTemplatePathContext {
    pub(crate) prefix: String,
    pub(crate) key: String,
}

#[derive(Debug, Clone)]
pub(crate) struct TemplateDocument {
    kind: TemplateKind,
    original_source: String,
    virtual_source: String,
    source_map: TemplateSourceMap,
    semantic_tokens: Vec<TemplateSemanticToken>,
    twig_variable_types: Vec<TemplateVariableType>,
}

impl TemplateDocument {
    pub(crate) fn kind(&self) -> TemplateKind {
        self.kind
    }

    pub(crate) fn original_source(&self) -> &str {
        &self.original_source
    }

    pub(crate) fn virtual_source(&self) -> &str {
        &self.virtual_source
    }

    pub(crate) fn with_twig_variable_types(&self, variable_types: &[TemplateVariableType]) -> Self {
        match self.kind {
            TemplateKind::Blade => self.clone(),
            TemplateKind::Twig => preprocess_twig_template(&self.original_source, variable_types),
        }
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

    pub(crate) fn map_virtual_position_to_original(&self, position: Position) -> Option<Position> {
        let virtual_offset = byte_offset_for_position(&self.virtual_source, position)?;
        let (original_offset, _) = self
            .source_map
            .virtual_range_to_original(virtual_offset, virtual_offset)?;
        Some(position_for_byte_offset(
            &self.original_source,
            original_offset,
        ))
    }

    pub(crate) fn syntax_diagnostics(&self) -> Vec<Diagnostic> {
        match self.kind {
            TemplateKind::Blade => Vec::new(),
            TemplateKind::Twig => twig_syntax_diagnostics(&self.original_source),
        }
    }

    pub(crate) fn map_diagnostics_to_original(
        &self,
        diagnostics: Vec<Diagnostic>,
        diagnostics_mode_is_off: bool,
    ) -> Vec<Diagnostic> {
        let mut diagnostics = self.map_safe_diagnostics_to_original(diagnostics);
        if !diagnostics_mode_is_off {
            diagnostics.extend(self.syntax_diagnostics());
        }
        diagnostics
    }

    pub(crate) fn map_safe_diagnostics_to_original(
        &self,
        diagnostics: Vec<Diagnostic>,
    ) -> Vec<Diagnostic> {
        diagnostics
            .into_iter()
            .filter_map(|mut diagnostic| {
                if !self.is_safe_template_diagnostic(&diagnostic) {
                    return None;
                }
                diagnostic.range = self.map_virtual_range_to_original_exact(diagnostic.range)?;
                Some(diagnostic)
            })
            .collect()
    }

    fn map_virtual_range_to_original_exact(&self, range: Range) -> Option<Range> {
        let virtual_start = byte_offset_for_position(&self.virtual_source, range.start)?;
        let virtual_end = byte_offset_for_position(&self.virtual_source, range.end)?;
        let (original_start, original_end) = self
            .source_map
            .virtual_range_to_original_exact(virtual_start, virtual_end)?;
        Some(range_for_byte_offsets(
            &self.original_source,
            original_start,
            original_end,
        ))
    }

    fn is_safe_template_diagnostic(&self, diagnostic: &Diagnostic) -> bool {
        if diagnostic.source.as_deref() != Some("php-lsp") {
            return false;
        }

        let message = diagnostic.message.as_str();
        match diagnostic_code_str(diagnostic) {
            Some("php-lsp.undefinedVariable") => false,
            Some("php-lsp.unknownClass")
            | Some("php-lsp.argumentCountMismatch")
            | Some("php-lsp.typeCompatibility") => true,
            Some("php-lsp.members") => is_unknown_member_diagnostic_message(message),
            Some("php-lsp.unknownSymbols") => is_unknown_symbol_diagnostic_message(message),
            _ => {
                is_unknown_member_diagnostic_message(message)
                    || is_type_compatibility_diagnostic_message(message)
                    || is_unknown_symbol_diagnostic_message(message)
            }
        }
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
        match self.kind {
            TemplateKind::Blade => preprocess_blade_template(&source),
            TemplateKind::Twig => preprocess_twig_template(&source, &self.twig_variable_types),
        }
    }

    pub(crate) fn twig_template_path_context_at_position(
        &self,
        position: Position,
    ) -> Option<TwigTemplatePathContext> {
        if self.kind != TemplateKind::Twig {
            return None;
        }
        twig_template_path_context_at_position(&self.original_source, position)
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
    fn push_same_length_segment(
        &mut self,
        original_start: usize,
        original_end: usize,
        virtual_start: usize,
    ) {
        let len = original_end.saturating_sub(original_start);
        self.push_segment(
            original_start,
            original_end,
            virtual_start,
            virtual_start + len,
        );
    }

    fn push_segment(
        &mut self,
        original_start: usize,
        original_end: usize,
        virtual_start: usize,
        virtual_end: usize,
    ) {
        if original_end < original_start {
            return;
        }
        self.segments.push(SourceMapSegment {
            original_start,
            original_end,
            virtual_start,
            virtual_end,
        });
    }

    fn original_to_virtual(&self, offset: usize) -> Option<usize> {
        let segment = self
            .segments
            .iter()
            .find(|segment| segment.original_start <= offset && offset <= segment.original_end)?;
        Some(segment.map_original_to_virtual(offset))
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
                let original = segment.map_virtual_to_original(virtual_start);
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

            let mapped_start = segment.map_virtual_to_original(overlap_start);
            let mapped_end = segment.map_virtual_to_original(overlap_end);
            original_start =
                Some(original_start.map_or(mapped_start, |current| current.min(mapped_start)));
            original_end = Some(original_end.map_or(mapped_end, |current| current.max(mapped_end)));
        }

        Some((original_start?, original_end?))
    }

    fn virtual_range_to_original_exact(
        &self,
        virtual_start: usize,
        virtual_end: usize,
    ) -> Option<(usize, usize)> {
        if virtual_end < virtual_start {
            return None;
        }

        if virtual_start == virtual_end {
            let segment = self
                .segments
                .iter()
                .find(|segment| segment.contains_virtual_offset(virtual_start))?;
            let original = segment.map_virtual_to_original(virtual_start);
            return Some((original, original));
        }

        let mut cursor = virtual_start;
        let mut original_start: Option<usize> = None;
        let mut original_end: Option<usize> = None;

        while cursor < virtual_end {
            let segment = self
                .segments
                .iter()
                .find(|segment| segment.contains_virtual_offset(cursor))?;
            let covered_end = virtual_end.min(segment.virtual_end);
            if covered_end <= cursor {
                return None;
            }

            let mapped_start = segment.map_virtual_to_original(cursor);
            let mapped_end = segment.map_virtual_to_original(covered_end);
            original_start =
                Some(original_start.map_or(mapped_start, |current| current.min(mapped_start)));
            original_end = Some(original_end.map_or(mapped_end, |current| current.max(mapped_end)));
            cursor = covered_end;
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
    fn contains_virtual_offset(self, offset: usize) -> bool {
        self.virtual_start <= offset && offset < self.virtual_end
    }

    fn map_original_to_virtual(self, offset: usize) -> usize {
        map_offset_between_ranges(
            offset,
            self.original_start,
            self.original_end,
            self.virtual_start,
            self.virtual_end,
        )
    }

    fn map_virtual_to_original(self, offset: usize) -> usize {
        map_offset_between_ranges(
            offset,
            self.virtual_start,
            self.virtual_end,
            self.original_start,
            self.original_end,
        )
    }
}

fn map_offset_between_ranges(
    offset: usize,
    source_start: usize,
    source_end: usize,
    target_start: usize,
    target_end: usize,
) -> usize {
    if offset <= source_start {
        return target_start;
    }
    if offset >= source_end {
        return target_end;
    }

    let source_len = source_end.saturating_sub(source_start);
    let target_len = target_end.saturating_sub(target_start);
    if source_len == 0 || target_len == 0 {
        return target_start;
    }

    target_start + offset.saturating_sub(source_start) * target_len / source_len
}

fn diagnostic_code_str(diagnostic: &Diagnostic) -> Option<&str> {
    match diagnostic.code.as_ref()? {
        NumberOrString::String(code) => Some(code.as_str()),
        NumberOrString::Number(_) => None,
    }
}

fn is_unknown_symbol_diagnostic_message(message: &str) -> bool {
    message.starts_with("Unknown class: ")
}

fn is_unknown_member_diagnostic_message(message: &str) -> bool {
    message.starts_with("Unknown method: ")
        || message.starts_with("Unknown class constant: ")
        || message.starts_with("Unknown member: ")
}

fn is_type_compatibility_diagnostic_message(message: &str) -> bool {
    message.starts_with("Type mismatch for ")
        || message.starts_with("Return type mismatch in ")
        || message.starts_with("Property assignment type mismatch for ")
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

pub(crate) fn is_twig_template_uri(uri_str: &str) -> bool {
    uri_str.ends_with(".twig")
}

pub(crate) fn is_twig_template_language_id(language_id: &str) -> bool {
    matches!(language_id, "twig" | "html-twig")
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
        kind: TemplateKind::Blade,
        original_source: source.to_string(),
        virtual_source,
        source_map,
        semantic_tokens,
        twig_variable_types: Vec::new(),
    }
}

pub(crate) fn preprocess_twig_template(
    source: &str,
    variable_types: &[TemplateVariableType],
) -> TemplateDocument {
    let mut virtual_source = String::new();
    let mut source_map = TemplateSourceMap::default();
    let mut semantic_tokens = Vec::new();
    let mut offset = 0usize;
    let mut macro_aliases = HashSet::new();

    push_twig_context_prelude(&mut virtual_source, variable_types);

    while offset < source.len() {
        if source[offset..].starts_with("{#") {
            let end = twig_comment_delimiter_content(source, offset)
                .map(|content| content.close_end)
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
            if let Some(content) = twig_delimiter_content(source, offset, "{{", "}}") {
                push_twig_echo_fragment(
                    source,
                    content.content_start,
                    content.content_end,
                    &mut virtual_source,
                    &mut source_map,
                    &macro_aliases,
                );
                offset = content.close_end;
                continue;
            }
        }

        if source[offset..].starts_with("{%") {
            if let Some(content) = twig_delimiter_content(source, offset, "{%", "%}") {
                push_twig_tag_fragment(
                    source,
                    content.content_start,
                    content.content_end,
                    &mut virtual_source,
                    &mut source_map,
                    &mut semantic_tokens,
                    &mut macro_aliases,
                );
                if twig_tag_name(source, content.content_start, content.content_end)
                    == Some("verbatim")
                {
                    if let Some(endverbatim) = find_twig_named_tag(
                        source,
                        content.close_end,
                        twig_matching_end_tag_name(
                            source,
                            content.content_start,
                            content.content_end,
                        ),
                    ) {
                        push_twig_tag_fragment(
                            source,
                            endverbatim.content_start,
                            endverbatim.content_end,
                            &mut virtual_source,
                            &mut source_map,
                            &mut semantic_tokens,
                            &mut macro_aliases,
                        );
                        offset = endverbatim.close_end;
                    } else {
                        offset = source.len();
                    }
                    continue;
                }
                if twig_tag_name(source, content.content_start, content.content_end)
                    == Some("macro")
                {
                    offset = find_twig_named_tag(
                        source,
                        content.close_end,
                        twig_matching_end_tag_name(
                            source,
                            content.content_start,
                            content.content_end,
                        ),
                    )
                    .map(|endmacro| endmacro.close_end)
                    .unwrap_or(source.len());
                    continue;
                }
                offset = content.close_end;
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
        kind: TemplateKind::Twig,
        original_source: source.to_string(),
        virtual_source,
        source_map,
        semantic_tokens,
        twig_variable_types: variable_types.to_vec(),
    }
}

fn twig_syntax_diagnostics(source: &str) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let mut block_stack: Vec<TwigBlock> = Vec::new();
    let mut offset = 0usize;

    while offset < source.len() {
        if source[offset..].starts_with("{#") {
            match twig_comment_delimiter_content(source, offset) {
                Some(content) => {
                    offset = content.close_end;
                }
                None => {
                    diagnostics.push(twig_syntax_diagnostic(
                        source,
                        offset,
                        (offset + 2).min(source.len()),
                        "Unclosed Twig comment",
                    ));
                    break;
                }
            }
            continue;
        }

        if source[offset..].starts_with("{{") {
            match twig_delimiter_content(source, offset, "{{", "}}") {
                Some(content) => {
                    offset = content.close_end;
                }
                None => {
                    diagnostics.push(twig_syntax_diagnostic(
                        source,
                        offset,
                        (offset + 2).min(source.len()),
                        "Unclosed Twig expression",
                    ));
                    break;
                }
            }
            continue;
        }

        if source[offset..].starts_with("{%") {
            let Some(content) = twig_delimiter_content(source, offset, "{%", "%}") else {
                diagnostics.push(twig_syntax_diagnostic(
                    source,
                    offset,
                    (offset + 2).min(source.len()),
                    "Unclosed Twig tag",
                ));
                break;
            };

            if twig_tag_name(source, content.content_start, content.content_end) == Some("verbatim")
            {
                if let Some(endverbatim) = find_twig_named_tag(
                    source,
                    content.close_end,
                    twig_matching_end_tag_name(source, content.content_start, content.content_end),
                ) {
                    offset = endverbatim.close_end;
                } else {
                    diagnostics.push(twig_syntax_diagnostic(
                        source,
                        content.content_start,
                        content.content_start + "verbatim".len(),
                        "Unclosed Twig `verbatim` block, expected `endverbatim`",
                    ));
                    break;
                }
                continue;
            }
            check_twig_block_tag(
                source,
                offset,
                content.content_start,
                content.content_end,
                &mut block_stack,
                &mut diagnostics,
            );
            offset = content.close_end;
            continue;
        }

        offset += source[offset..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(1);
    }

    for block in block_stack.into_iter().rev() {
        diagnostics.push(twig_syntax_diagnostic(
            source,
            block.start,
            block.end,
            &format!(
                "Unclosed Twig `{}` block, expected `{}`",
                block.name, block.end_tag
            ),
        ));
    }

    diagnostics
}

#[derive(Debug, Clone)]
struct TwigBlock {
    name: String,
    end_tag: &'static str,
    start: usize,
    end: usize,
}

fn check_twig_block_tag(
    source: &str,
    tag_start: usize,
    content_start: usize,
    content_end: usize,
    block_stack: &mut Vec<TwigBlock>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some((name_start, name_end)) = twig_tag_name_range(source, content_start, content_end)
    else {
        diagnostics.push(twig_syntax_diagnostic(
            source,
            tag_start,
            (tag_start + 2).min(source.len()),
            "Missing Twig tag name",
        ));
        return;
    };
    if name_end <= name_start {
        diagnostics.push(twig_syntax_diagnostic(
            source,
            tag_start,
            (tag_start + 2).min(source.len()),
            "Missing Twig tag name",
        ));
        return;
    }

    let Some(name) = source.get(name_start..name_end) else {
        return;
    };
    let tag_range_end = name_end.max((tag_start + 2).min(source.len()));

    if let Some(end_tag) = twig_opening_block_end_tag(name, source, name_end, content_end) {
        block_stack.push(TwigBlock {
            name: name.to_string(),
            end_tag,
            start: name_start,
            end: tag_range_end,
        });
        return;
    }

    if twig_intermediate_block_tag(name) {
        if block_stack.last().is_some_and(|block| {
            (name == "else" && matches!(block.end_tag, "endif" | "endfor"))
                || (name == "elseif" && block.end_tag == "endif")
        }) {
            return;
        }

        diagnostics.push(twig_syntax_diagnostic(
            source,
            name_start,
            tag_range_end,
            &format!("Unexpected Twig `{name}` tag"),
        ));
        return;
    }

    if twig_known_closing_block_tag(name) {
        let Some(block) = block_stack.pop() else {
            diagnostics.push(twig_syntax_diagnostic(
                source,
                name_start,
                tag_range_end,
                &format!("Unexpected Twig `{name}` tag"),
            ));
            return;
        };
        if block.end_tag != name {
            diagnostics.push(twig_syntax_diagnostic(
                source,
                name_start,
                tag_range_end,
                &format!(
                    "Mismatched Twig `{name}` tag, expected `{}` for `{}` block",
                    block.end_tag, block.name
                ),
            ));
        }
    }
}

fn twig_matching_end_tag_name(
    source: &str,
    content_start: usize,
    content_end: usize,
) -> &'static str {
    let Some((name_start, name_end)) = twig_tag_name_range(source, content_start, content_end)
    else {
        return "";
    };
    let Some(name) = source.get(name_start..name_end) else {
        return "";
    };
    twig_opening_block_end_tag(name, source, name_end, content_end).unwrap_or("")
}

fn twig_opening_block_end_tag(
    name: &str,
    source: &str,
    name_end: usize,
    tag_end: usize,
) -> Option<&'static str> {
    match name {
        "if" => Some("endif"),
        "for" => Some("endfor"),
        "block" => Some("endblock"),
        "embed" => Some("endembed"),
        "autoescape" => Some("endautoescape"),
        "filter" => Some("endfilter"),
        "apply" => Some("endapply"),
        "with" => Some("endwith"),
        "cache" => Some("endcache"),
        "sandbox" => Some("endsandbox"),
        "trans" => Some("endtrans"),
        "macro" => Some("endmacro"),
        "verbatim" => Some("endverbatim"),
        "set" if twig_set_tag_starts_block(source, name_end, tag_end) => Some("endset"),
        _ => None,
    }
}

fn twig_set_tag_starts_block(source: &str, name_end: usize, tag_end: usize) -> bool {
    !source
        .get(name_end..tag_end)
        .unwrap_or("")
        .chars()
        .any(|ch| ch == '=')
}

fn twig_intermediate_block_tag(name: &str) -> bool {
    matches!(name, "else" | "elseif")
}

fn twig_known_closing_block_tag(name: &str) -> bool {
    matches!(
        name,
        "endif"
            | "endfor"
            | "endblock"
            | "endembed"
            | "endautoescape"
            | "endfilter"
            | "endapply"
            | "endwith"
            | "endcache"
            | "endsandbox"
            | "endtrans"
            | "endmacro"
            | "endverbatim"
            | "endset"
    )
}

#[derive(Debug, Clone, Copy)]
struct TwigDelimiterContent {
    content_start: usize,
    content_end: usize,
    close_end: usize,
}

fn twig_delimiter_content(
    source: &str,
    open_start: usize,
    open_delimiter: &str,
    close_delimiter: &str,
) -> Option<TwigDelimiterContent> {
    let mut content_start = open_start + open_delimiter.len();
    if source.as_bytes().get(content_start) == Some(&b'-') {
        content_start += 1;
    }

    let close_start = find_twig_delimiter_close(source, content_start, close_delimiter)?;
    let mut content_end = close_start;
    if content_end > content_start && source.as_bytes().get(content_end - 1) == Some(&b'-') {
        content_end -= 1;
    }

    Some(TwigDelimiterContent {
        content_start,
        content_end,
        close_end: close_start + close_delimiter.len(),
    })
}

fn twig_comment_delimiter_content(source: &str, open_start: usize) -> Option<TwigDelimiterContent> {
    let mut content_start = open_start + "{#".len();
    if source.as_bytes().get(content_start) == Some(&b'-') {
        content_start += 1;
    }

    let close_start = source.get(content_start..)?.find("#}")? + content_start;
    let mut content_end = close_start;
    if content_end > content_start && source.as_bytes().get(content_end - 1) == Some(&b'-') {
        content_end -= 1;
    }

    Some(TwigDelimiterContent {
        content_start,
        content_end,
        close_end: close_start + "#}".len(),
    })
}

fn twig_tag_name_range(
    source: &str,
    content_start: usize,
    content_end: usize,
) -> Option<(usize, usize)> {
    let name_start = skip_ascii_ws_in_range(source, content_start, content_end);
    let name_end = scan_identifier_end(source, name_start).min(content_end);
    (name_end > name_start).then_some((name_start, name_end))
}

fn twig_tag_name(source: &str, content_start: usize, content_end: usize) -> Option<&str> {
    let (name_start, name_end) = twig_tag_name_range(source, content_start, content_end)?;
    source.get(name_start..name_end)
}

fn find_twig_named_tag(
    source: &str,
    mut offset: usize,
    expected_name: &str,
) -> Option<TwigDelimiterContent> {
    while offset < source.len() {
        let relative = source.get(offset..)?.find("{%")?;
        let tag_start = offset + relative;
        if let Some(content) = twig_delimiter_content(source, tag_start, "{%", "%}") {
            if twig_tag_name(source, content.content_start, content.content_end)
                == Some(expected_name)
            {
                return Some(content);
            }
        }
        offset = tag_start + "{%".len();
    }

    None
}

fn find_twig_delimiter_close(source: &str, mut offset: usize, delimiter: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    let mut escaped = false;

    while offset < source.len() {
        let ch = source[offset..].chars().next()?;
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active_quote {
                quote = None;
            }
            offset += ch.len_utf8();
            continue;
        }

        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            offset += ch.len_utf8();
            continue;
        }

        if source[offset..].starts_with(delimiter) {
            return Some(offset);
        }

        offset += ch.len_utf8();
    }

    None
}

fn twig_syntax_diagnostic(source: &str, start: usize, end: usize, message: &str) -> Diagnostic {
    Diagnostic {
        range: range_for_byte_offsets(source, start, end.max(start)),
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("php-lsp.twigSyntax".to_string())),
        source: Some("php-lsp".to_string()),
        message: message.to_string(),
        ..Default::default()
    }
}

fn push_twig_context_prelude(virtual_source: &mut String, variable_types: &[TemplateVariableType]) {
    if variable_types.is_empty() {
        return;
    }

    virtual_source.push_str("<?php\n");
    for variable in variable_types {
        if !is_valid_twig_identifier(&variable.name) || variable.type_text.trim().is_empty() {
            continue;
        }
        virtual_source.push_str("/** @var ");
        virtual_source.push_str(variable.type_text.trim());
        virtual_source.push_str(" $");
        virtual_source.push_str(&variable.name);
        virtual_source.push_str(" */\n$");
        virtual_source.push_str(&variable.name);
        virtual_source.push_str(" = null;\n");
    }
    virtual_source.push_str("?>\n");
}

fn push_twig_echo_fragment(
    source: &str,
    original_start: usize,
    original_end: usize,
    virtual_source: &mut String,
    source_map: &mut TemplateSourceMap,
    macro_aliases: &HashSet<String>,
) {
    let (expr_start, expr_end) = trim_ascii_range(source, original_start, original_end);
    virtual_source.push_str("<?php echo ");
    let mapped = append_converted_twig_expression(
        source,
        expr_start,
        expr_end,
        virtual_source,
        source_map,
        macro_aliases,
    );
    if !mapped {
        virtual_source.push_str("null");
    }
    virtual_source.push_str("; ?>\n");
    if !mapped {
        append_twig_partial_member_chain_fragments(
            source,
            expr_start,
            expr_end,
            virtual_source,
            source_map,
            macro_aliases,
            None,
        );
    }
}

fn push_twig_tag_fragment(
    source: &str,
    content_start: usize,
    content_end: usize,
    virtual_source: &mut String,
    source_map: &mut TemplateSourceMap,
    semantic_tokens: &mut Vec<TemplateSemanticToken>,
    macro_aliases: &mut HashSet<String>,
) {
    let name_start = skip_ascii_ws_in_range(source, content_start, content_end);
    let name_end = scan_identifier_end(source, name_start).min(content_end);
    if name_end <= name_start {
        return;
    }
    let Some(name) = source.get(name_start..name_end) else {
        return;
    };

    semantic_tokens.push(TemplateSemanticToken {
        original_start: name_start,
        original_end: name_end,
        token_type: TOKEN_KEYWORD,
        token_modifiers_bitset: 0,
    });
    collect_twig_macro_aliases_from_tag(name, source, name_end, content_end, macro_aliases);

    match name {
        "if" | "elseif" => {
            let (expr_start, expr_end) = trim_ascii_range(source, name_end, content_end);
            if expr_start >= expr_end {
                return;
            }
            if name == "if" {
                virtual_source.push_str("<?php if (");
            } else {
                virtual_source.push_str("<?php elseif (");
            }
            let mapped = append_converted_twig_expression(
                source,
                expr_start,
                expr_end,
                virtual_source,
                source_map,
                macro_aliases,
            );
            if !mapped {
                virtual_source.push_str("true");
            }
            virtual_source.push_str("): ?>\n");
            if !mapped {
                append_twig_partial_member_chain_fragments(
                    source,
                    expr_start,
                    expr_end,
                    virtual_source,
                    source_map,
                    macro_aliases,
                    None,
                );
            }
        }
        "else" => virtual_source.push_str("<?php else: ?>\n"),
        "endif" => virtual_source.push_str("<?php endif; ?>\n"),
        "for" => push_twig_for_fragment(
            source,
            name_end,
            content_end,
            virtual_source,
            source_map,
            macro_aliases,
        ),
        "endfor" => virtual_source.push_str("<?php endforeach; ?>\n"),
        "set" => push_twig_set_fragment(
            source,
            name_end,
            content_end,
            virtual_source,
            source_map,
            macro_aliases,
        ),
        "block" | "endblock" | "extends" | "include" | "embed" | "endembed" | "use" | "import"
        | "from" => {}
        _ => {}
    }
}

fn collect_twig_macro_aliases_from_tag(
    tag_name: &str,
    source: &str,
    start: usize,
    end: usize,
    macro_aliases: &mut HashSet<String>,
) {
    let words = twig_tag_identifier_words(source, start, end);
    match tag_name {
        "import" => {
            if let Some(alias) = words
                .windows(2)
                .find_map(|window| (window[0] == "as").then_some(window[1].as_str()))
            {
                macro_aliases.insert(alias.to_string());
            }
        }
        "from" => {
            let Some(import_position) = words.iter().position(|word| word == "import") else {
                return;
            };
            let mut index = import_position + 1;
            while index < words.len() {
                match words[index].as_str() {
                    "as" => {
                        if let Some(alias) = words.get(index + 1) {
                            macro_aliases.insert(alias.clone());
                            index += 2;
                        } else {
                            index += 1;
                        }
                    }
                    name => {
                        macro_aliases.insert(name.to_string());
                        index += 1;
                    }
                }
            }
        }
        _ => {}
    }
}

fn twig_tag_identifier_words(source: &str, start: usize, end: usize) -> Vec<String> {
    let mut words = Vec::new();
    let mut offset = start;

    while offset < end {
        let Some(ch) = source[offset..end].chars().next() else {
            break;
        };
        if ch == '\'' || ch == '"' {
            let close = find_quoted_string_end(source, offset, end, ch)
                .unwrap_or(end.saturating_sub(ch.len_utf8()));
            offset = (close + ch.len_utf8()).min(end);
            continue;
        }
        if is_twig_identifier_start(ch) {
            let ident_end = scan_twig_identifier_end(source, offset, end);
            if let Some(word) = source.get(offset..ident_end) {
                words.push(word.to_string());
            }
            offset = ident_end;
            continue;
        }
        offset += ch.len_utf8();
    }

    words
}

fn push_twig_for_fragment(
    source: &str,
    start: usize,
    end: usize,
    virtual_source: &mut String,
    source_map: &mut TemplateSourceMap,
    macro_aliases: &HashSet<String>,
) {
    let rest_start = skip_ascii_ws_in_range(source, start, end);
    let item_start = rest_start;
    let item_end = scan_identifier_end(source, item_start).min(end);
    if item_end <= item_start {
        return;
    }
    let after_item = skip_ascii_ws_in_range(source, item_end, end);
    if !source
        .get(after_item..end)
        .is_some_and(|rest| rest.starts_with("in"))
    {
        return;
    }
    let after_in = after_item + "in".len();
    if source
        .as_bytes()
        .get(after_in)
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
    {
        return;
    }
    let collection_start = skip_ascii_ws_in_range(source, after_in, end);
    let (collection_start, collection_end) = trim_ascii_range(source, collection_start, end);
    if collection_start >= collection_end {
        return;
    }

    virtual_source.push_str("<?php foreach (");
    let mapped = append_converted_twig_expression(
        source,
        collection_start,
        collection_end,
        virtual_source,
        source_map,
        macro_aliases,
    );
    let preserved_base_range = (!mapped)
        .then(|| {
            append_twig_type_preserving_filter_base_expression(
                source,
                collection_start,
                collection_end,
                virtual_source,
                source_map,
                macro_aliases,
            )
        })
        .flatten();
    if !mapped && preserved_base_range.is_none() {
        virtual_source.push_str("(array) []");
    }
    virtual_source.push_str(" as ");
    let virtual_item_start = virtual_source.len();
    virtual_source.push('$');
    virtual_source.push_str(source.get(item_start..item_end).unwrap_or(""));
    source_map.push_segment(
        item_start,
        item_end,
        virtual_item_start,
        virtual_source.len(),
    );
    virtual_source.push_str("): ?>\n");
    if !mapped {
        append_twig_partial_member_chain_fragments(
            source,
            collection_start,
            collection_end,
            virtual_source,
            source_map,
            macro_aliases,
            preserved_base_range,
        );
    }
}

fn push_twig_set_fragment(
    source: &str,
    start: usize,
    end: usize,
    virtual_source: &mut String,
    source_map: &mut TemplateSourceMap,
    macro_aliases: &HashSet<String>,
) {
    let name_start = skip_ascii_ws_in_range(source, start, end);
    let name_end = scan_identifier_end(source, name_start).min(end);
    if name_end <= name_start {
        return;
    }
    let after_name = skip_ascii_ws_in_range(source, name_end, end);
    if source.as_bytes().get(after_name) != Some(&b'=') {
        return;
    }
    let (expr_start, expr_end) = trim_ascii_range(source, after_name + 1, end);
    if expr_start >= expr_end {
        return;
    }

    virtual_source.push_str("<?php ");
    let virtual_name_start = virtual_source.len();
    virtual_source.push('$');
    virtual_source.push_str(source.get(name_start..name_end).unwrap_or(""));
    source_map.push_segment(
        name_start,
        name_end,
        virtual_name_start,
        virtual_source.len(),
    );
    virtual_source.push_str(" = ");
    let mapped = append_converted_twig_expression(
        source,
        expr_start,
        expr_end,
        virtual_source,
        source_map,
        macro_aliases,
    );
    let preserved_base_range = (!mapped)
        .then(|| {
            append_twig_type_preserving_filter_base_expression(
                source,
                expr_start,
                expr_end,
                virtual_source,
                source_map,
                macro_aliases,
            )
        })
        .flatten();
    if !mapped && preserved_base_range.is_none() {
        virtual_source.push_str("null");
    }
    virtual_source.push_str("; ?>\n");
    if !mapped {
        append_twig_partial_member_chain_fragments(
            source,
            expr_start,
            expr_end,
            virtual_source,
            source_map,
            macro_aliases,
            preserved_base_range,
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnsupportedTwigExpression {
    Filter,
    Test,
    InOperator,
    FunctionCall,
    MacroCall,
    Ternary,
    NullCoalescing,
    ComplexAttributeAccess,
}

#[derive(Debug, Clone, Copy)]
struct TwigExpressionSegment {
    original_start: usize,
    original_end: usize,
    virtual_start: usize,
    virtual_end: usize,
}

fn append_converted_twig_expression(
    source: &str,
    original_start: usize,
    original_end: usize,
    virtual_source: &mut String,
    source_map: &mut TemplateSourceMap,
    macro_aliases: &HashSet<String>,
) -> bool {
    if unsupported_twig_expression(source, original_start, original_end, macro_aliases).is_some() {
        return false;
    }

    let virtual_base = virtual_source.len();
    let (converted, segments) =
        convert_twig_expression_to_php(source, original_start, original_end);
    virtual_source.push_str(&converted);
    for segment in segments {
        source_map.push_segment(
            segment.original_start,
            segment.original_end,
            virtual_base + segment.virtual_start,
            virtual_base + segment.virtual_end,
        );
    }
    true
}

fn append_twig_partial_member_chain_fragments(
    source: &str,
    original_start: usize,
    original_end: usize,
    virtual_source: &mut String,
    source_map: &mut TemplateSourceMap,
    macro_aliases: &HashSet<String>,
    excluded_range: Option<(usize, usize)>,
) {
    for (chain_start, chain_end) in
        twig_partial_member_chain_ranges(source, original_start, original_end, macro_aliases)
    {
        if excluded_range.is_some_and(|(start, end)| chain_start == start && chain_end == end) {
            continue;
        }
        if unsupported_twig_expression(source, chain_start, chain_end, macro_aliases).is_some() {
            continue;
        }

        let (converted, segments) = convert_twig_expression_to_php(source, chain_start, chain_end);
        if converted.trim().is_empty() {
            continue;
        }

        virtual_source.push_str("<?php ");
        let virtual_base = virtual_source.len();
        virtual_source.push_str(&converted);
        virtual_source.push_str("; ?>\n");
        for segment in segments {
            source_map.push_segment(
                segment.original_start,
                segment.original_end,
                virtual_base + segment.virtual_start,
                virtual_base + segment.virtual_end,
            );
        }
    }
}

fn append_twig_type_preserving_filter_base_expression(
    source: &str,
    original_start: usize,
    original_end: usize,
    virtual_source: &mut String,
    source_map: &mut TemplateSourceMap,
    macro_aliases: &HashSet<String>,
) -> Option<(usize, usize)> {
    let (base_start, base_end) =
        twig_type_preserving_filter_base_range(source, original_start, original_end)?;
    if unsupported_twig_expression(source, base_start, base_end, macro_aliases).is_some() {
        return None;
    }

    let (converted, segments) = convert_twig_expression_to_php(source, base_start, base_end);
    if converted.trim().is_empty() {
        return None;
    }

    let virtual_base = virtual_source.len();
    virtual_source.push_str(&converted);
    for segment in segments {
        source_map.push_segment(
            segment.original_start,
            segment.original_end,
            virtual_base + segment.virtual_start,
            virtual_base + segment.virtual_end,
        );
    }
    Some((base_start, base_end))
}

fn twig_type_preserving_filter_base_range(
    source: &str,
    original_start: usize,
    original_end: usize,
) -> Option<(usize, usize)> {
    let pipe = find_top_level_twig_pipe(source, original_start, original_end)?;
    let filter_start = skip_ascii_ws_in_range(source, pipe + '|'.len_utf8(), original_end);
    let filter_end = scan_twig_identifier_end(source, filter_start, original_end);
    let filter = source.get(filter_start..filter_end)?;
    if filter != "slice" {
        return None;
    }

    let (base_start, base_end) = trim_ascii_range(source, original_start, pipe);
    (base_start < base_end).then_some((base_start, base_end))
}

fn find_top_level_twig_pipe(source: &str, start: usize, end: usize) -> Option<usize> {
    let mut offset = start;
    let mut paren_depth = 0usize;

    while offset < end {
        let ch = source[offset..end].chars().next()?;
        if ch == '\'' || ch == '"' {
            let close = find_quoted_string_end(source, offset, end, ch)
                .unwrap_or(end.saturating_sub(ch.len_utf8()));
            offset = (close + ch.len_utf8()).min(end);
            continue;
        }

        match ch {
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '|' if paren_depth == 0 => return Some(offset),
            _ => {}
        }

        offset += ch.len_utf8();
    }

    None
}

fn twig_partial_member_chain_ranges(
    source: &str,
    original_start: usize,
    original_end: usize,
    macro_aliases: &HashSet<String>,
) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut offset = original_start;

    while offset < original_end {
        let Some(ch) = source[offset..original_end].chars().next() else {
            break;
        };

        if ch == '\'' || ch == '"' {
            let close = find_quoted_string_end(source, offset, original_end, ch)
                .unwrap_or(original_end.saturating_sub(ch.len_utf8()));
            offset = (close + ch.len_utf8()).min(original_end);
            continue;
        }

        if !is_twig_identifier_start(ch) {
            offset += ch.len_utf8();
            continue;
        }

        let root_start = offset;
        let root_end = scan_twig_identifier_end(source, root_start, original_end);
        let root = source.get(root_start..root_end).unwrap_or("");
        let root_lower = root.to_ascii_lowercase();
        if root == "_self"
            || macro_aliases.contains(root)
            || is_twig_expression_keyword(&root_lower)
        {
            offset = root_end;
            continue;
        }

        let mut cursor = root_end;
        let mut chain_end = root_end;
        let mut has_member = false;

        while cursor < original_end && source[cursor..original_end].starts_with('.') {
            let dot_end = cursor + '.'.len_utf8();
            if source[dot_end..original_end].starts_with('.') {
                break;
            }
            let Some(member_ch) = source[dot_end..original_end].chars().next() else {
                chain_end = dot_end;
                has_member = true;
                break;
            };
            if !is_twig_identifier_start(member_ch) {
                chain_end = dot_end;
                has_member = true;
                break;
            }

            let member_end = scan_twig_identifier_end(source, dot_end, original_end);
            chain_end = member_end;
            cursor = member_end;
            has_member = true;

            if twig_next_non_ws_char(source, cursor, original_end) == Some('(') {
                let call_open = skip_ascii_ws_in_range(source, cursor, original_end);
                let Some(call_close) =
                    find_balanced_twig_parenthesis_end(source, call_open, original_end)
                else {
                    break;
                };
                cursor = (call_close + ')'.len_utf8()).min(original_end);
                chain_end = cursor;
            }
        }

        if has_member {
            ranges.push((root_start, chain_end));
            offset = chain_end;
        } else {
            offset = root_end;
        }
    }

    ranges
}

fn is_twig_expression_keyword(lower: &str) -> bool {
    matches!(
        lower,
        "and" | "or" | "not" | "is" | "in" | "matches" | "as" | "with" | "only"
    ) || is_twig_literal(lower)
}

fn find_balanced_twig_parenthesis_end(source: &str, open: usize, end: usize) -> Option<usize> {
    if source.as_bytes().get(open) != Some(&b'(') {
        return None;
    }

    let mut offset = open;
    let mut depth = 0usize;
    while offset < end {
        let ch = source[offset..end].chars().next()?;
        if ch == '\'' || ch == '"' {
            let close = find_quoted_string_end(source, offset, end, ch)?;
            offset = (close + ch.len_utf8()).min(end);
            continue;
        }

        if ch == '(' {
            depth += 1;
        } else if ch == ')' {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return Some(offset);
            }
        }

        offset += ch.len_utf8();
    }

    None
}

fn unsupported_twig_expression(
    source: &str,
    original_start: usize,
    original_end: usize,
    macro_aliases: &HashSet<String>,
) -> Option<UnsupportedTwigExpression> {
    let mut offset = original_start;
    let mut after_member_access = false;
    let mut current_chain_root: Option<String> = None;

    while offset < original_end {
        let Some(ch) = source[offset..original_end].chars().next() else {
            break;
        };

        if ch == '\'' || ch == '"' {
            let close = find_quoted_string_end(source, offset, original_end, ch)
                .unwrap_or(original_end.saturating_sub(ch.len_utf8()));
            offset = (close + ch.len_utf8()).min(original_end);
            after_member_access = false;
            continue;
        }

        if ch == '|' {
            return Some(UnsupportedTwigExpression::Filter);
        }
        if ch == '?' {
            if source[offset + ch.len_utf8()..original_end].starts_with('?') {
                return Some(UnsupportedTwigExpression::NullCoalescing);
            }
            return Some(UnsupportedTwigExpression::Ternary);
        }
        if ch == '[' || ch == ']' {
            return Some(UnsupportedTwigExpression::ComplexAttributeAccess);
        }
        if ch == '.' {
            let next_offset = offset + ch.len_utf8();
            if source[next_offset..original_end].starts_with('.') {
                return Some(UnsupportedTwigExpression::ComplexAttributeAccess);
            }
            offset = next_offset;
            after_member_access = true;
            continue;
        }

        if is_twig_identifier_start(ch) {
            let ident_end = scan_twig_identifier_end(source, offset, original_end);
            let ident = source.get(offset..ident_end).unwrap_or("");
            let lower = ident.to_ascii_lowercase();

            if lower == "is" || lower == "matches" {
                return Some(UnsupportedTwigExpression::Test);
            }
            if lower == "in" {
                return Some(UnsupportedTwigExpression::InOperator);
            }
            if lower == "and" || lower == "or" || lower == "not" || is_twig_literal(&lower) {
                offset = ident_end;
                after_member_access = false;
                current_chain_root = None;
                continue;
            }
            if !after_member_access {
                current_chain_root = Some(ident.to_string());
            }
            if twig_next_non_ws_char(source, ident_end, original_end) == Some('(') {
                if after_member_access
                    && current_chain_root
                        .as_deref()
                        .is_some_and(|root| root == "_self" || macro_aliases.contains(root))
                {
                    return Some(UnsupportedTwigExpression::MacroCall);
                }
                if after_member_access {
                    offset = ident_end;
                    after_member_access = false;
                    continue;
                }
                if macro_aliases.contains(ident) {
                    return Some(UnsupportedTwigExpression::MacroCall);
                }
                return Some(UnsupportedTwigExpression::FunctionCall);
            }

            offset = ident_end;
            after_member_access = false;
            continue;
        }

        offset += ch.len_utf8();
        if !ch.is_whitespace() {
            after_member_access = false;
            if ch != '(' && ch != ')' {
                current_chain_root = None;
            }
        }
    }

    None
}

fn convert_twig_expression_to_php(
    source: &str,
    original_start: usize,
    original_end: usize,
) -> (String, Vec<TwigExpressionSegment>) {
    let mut converted = String::new();
    let mut segments = Vec::new();
    let mut offset = original_start;
    let mut after_member_access = false;

    while offset < original_end {
        let Some(ch) = source[offset..original_end].chars().next() else {
            break;
        };

        if ch == '|' {
            break;
        }

        if ch == '\'' || ch == '"' {
            let close = find_quoted_string_end(source, offset, original_end, ch)
                .unwrap_or(original_end.saturating_sub(ch.len_utf8()));
            let end = (close + ch.len_utf8()).min(original_end);
            let virtual_start = converted.len();
            converted.push_str(source.get(offset..end).unwrap_or(""));
            segments.push(TwigExpressionSegment {
                original_start: offset,
                original_end: end,
                virtual_start,
                virtual_end: converted.len(),
            });
            offset = end;
            after_member_access = false;
            continue;
        }

        if ch == '.' {
            let virtual_start = converted.len();
            converted.push_str("->");
            segments.push(TwigExpressionSegment {
                original_start: offset,
                original_end: offset + ch.len_utf8(),
                virtual_start,
                virtual_end: converted.len(),
            });
            offset += ch.len_utf8();
            after_member_access = true;
            continue;
        }

        if is_twig_identifier_start(ch) {
            let ident_end = scan_twig_identifier_end(source, offset, original_end);
            let ident = source.get(offset..ident_end).unwrap_or("");
            let lower = ident.to_ascii_lowercase();
            if matches!(lower.as_str(), "is" | "in") {
                break;
            }
            if lower == "and" || lower == "or" || lower == "not" {
                converted.push_str(match lower.as_str() {
                    "and" => "&&",
                    "or" => "||",
                    "not" => "!",
                    _ => ident,
                });
            } else if is_twig_literal(&lower)
                || twig_next_non_ws_char(source, ident_end, original_end) == Some('(')
                || after_member_access
            {
                let virtual_start = converted.len();
                converted.push_str(ident);
                segments.push(TwigExpressionSegment {
                    original_start: offset,
                    original_end: ident_end,
                    virtual_start,
                    virtual_end: converted.len(),
                });
            } else {
                let virtual_start = converted.len();
                converted.push('$');
                converted.push_str(ident);
                segments.push(TwigExpressionSegment {
                    original_start: offset,
                    original_end: ident_end,
                    virtual_start,
                    virtual_end: converted.len(),
                });
            }
            offset = ident_end;
            after_member_access = false;
            continue;
        }

        let virtual_start = converted.len();
        converted.push(ch);
        segments.push(TwigExpressionSegment {
            original_start: offset,
            original_end: offset + ch.len_utf8(),
            virtual_start,
            virtual_end: converted.len(),
        });
        offset += ch.len_utf8();
        if !ch.is_whitespace() {
            after_member_access = false;
        }
    }

    (converted, segments)
}

fn trim_ascii_range(source: &str, mut start: usize, mut end: usize) -> (usize, usize) {
    while start < end
        && source
            .as_bytes()
            .get(start)
            .is_some_and(u8::is_ascii_whitespace)
    {
        start += 1;
    }
    while end > start
        && source
            .as_bytes()
            .get(end - 1)
            .is_some_and(u8::is_ascii_whitespace)
    {
        end -= 1;
    }
    (start, end)
}

fn skip_ascii_ws_in_range(source: &str, mut offset: usize, end: usize) -> usize {
    while offset < end
        && source
            .as_bytes()
            .get(offset)
            .is_some_and(u8::is_ascii_whitespace)
    {
        offset += 1;
    }
    offset
}

fn is_valid_twig_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    is_twig_identifier_start(first) && chars.all(is_twig_identifier_continue)
}

fn is_twig_identifier_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_'
}

fn is_twig_identifier_continue(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

fn scan_twig_identifier_end(source: &str, start: usize, end: usize) -> usize {
    let mut current = start;
    for (relative, ch) in source[start..end].char_indices() {
        if relative == 0 {
            if !is_twig_identifier_start(ch) {
                return start;
            }
        } else if !is_twig_identifier_continue(ch) {
            break;
        }
        current = start + relative + ch.len_utf8();
    }
    current
}

fn is_twig_literal(lower: &str) -> bool {
    matches!(lower, "true" | "false" | "null" | "none")
}

fn twig_next_non_ws_char(source: &str, mut offset: usize, end: usize) -> Option<char> {
    while offset < end {
        let ch = source[offset..end].chars().next()?;
        if !ch.is_whitespace() {
            return Some(ch);
        }
        offset += ch.len_utf8();
    }
    None
}

fn find_quoted_string_end(source: &str, start: usize, end: usize, quote: char) -> Option<usize> {
    let mut escaped = false;
    let mut offset = start + quote.len_utf8();
    while offset < end {
        let ch = source[offset..end].chars().next()?;
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == quote {
            return Some(offset);
        }
        offset += ch.len_utf8();
    }
    None
}

fn twig_template_path_context_at_position(
    source: &str,
    position: Position,
) -> Option<TwigTemplatePathContext> {
    let offset = byte_offset_for_position(source, position)?;
    let bounds = template_string_literal_bounds_at_offset(source, offset)?;
    let (tag_start, tag_end) = twig_tag_bounds_containing(source, bounds.quote_start)?;
    let content = twig_delimiter_content(source, tag_start, "{%", "%}")?;
    if content.close_end != tag_end + "%}".len() || bounds.quote_start >= content.content_end {
        return None;
    }

    let (name_start, name_end) =
        twig_tag_name_range(source, content.content_start, content.content_end)?;
    let name = source.get(name_start..name_end)?;
    if !matches!(
        name,
        "include" | "extends" | "embed" | "use" | "import" | "from"
    ) {
        return None;
    }

    Some(TwigTemplatePathContext {
        prefix: source.get(bounds.content_start..offset)?.to_string(),
        key: source
            .get(bounds.content_start..bounds.content_end)
            .unwrap_or("")
            .to_string(),
    })
}

fn twig_tag_bounds_containing(source: &str, offset: usize) -> Option<(usize, usize)> {
    let open = source.get(..offset)?.rfind("{%")?;
    let last_close_before = source.get(..offset)?.rfind("%}");
    if last_close_before.is_some_and(|close| close > open) {
        return None;
    }
    let close = source
        .get(offset..)?
        .find("%}")
        .map(|relative| offset + relative)?;
    Some((open, close))
}

#[derive(Debug, Clone, Copy)]
struct TemplateStringLiteralBounds {
    quote_start: usize,
    content_start: usize,
    content_end: usize,
}

fn template_string_literal_bounds_at_offset(
    source: &str,
    offset: usize,
) -> Option<TemplateStringLiteralBounds> {
    let mut quote: Option<(char, usize)> = None;
    let mut escaped = false;
    for (idx, ch) in source.char_indices() {
        if idx >= offset {
            break;
        }
        if let Some((active_quote, _)) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active_quote {
                quote = None;
            }
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = Some((ch, idx));
        }
    }

    let (quote_char, quote_start) = quote?;
    let content_start = quote_start + quote_char.len_utf8();
    if offset < content_start {
        return None;
    }
    let content_end = find_quoted_string_end(source, offset, source.len(), quote_char)
        .unwrap_or_else(|| line_end_offset(source, offset));
    Some(TemplateStringLiteralBounds {
        quote_start,
        content_start,
        content_end,
    })
}

fn line_end_offset(source: &str, offset: usize) -> usize {
    source[offset..]
        .find('\n')
        .map(|relative| offset + relative)
        .unwrap_or(source.len())
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
    source_map.push_same_length_segment(original_start, original_end, virtual_start);
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

    #[test]
    fn twig_echo_maps_variable_and_member_chain_to_virtual_php() {
        let doc = preprocess_twig_template(
            "<h1>{{ user.name }}</h1>\n",
            &[TemplateVariableType {
                name: "user".to_string(),
                type_text: "App\\Entity\\User".to_string(),
            }],
        );
        assert!(doc.virtual_source().contains("$user->name"));

        let original_position = Position::new(0, 7);
        let virtual_position = doc
            .map_original_position_to_virtual(original_position)
            .expect("Twig variable should map to virtual PHP variable");
        let virtual_offset = byte_offset_for_position(doc.virtual_source(), virtual_position)
            .expect("virtual position offset");
        assert_eq!(
            doc.virtual_source()
                .get(virtual_offset..virtual_offset + "$user".len()),
            Some("$user")
        );
    }

    #[test]
    fn twig_whitespace_control_maps_expression_and_block_content() {
        let doc = preprocess_twig_template(
            "<h1>{{- user.name -}}</h1>\n{%- for item in users -%}\n{{- item.name -}}\n{%- endfor -%}\n",
            &[TemplateVariableType {
                name: "user".to_string(),
                type_text: "App\\Entity\\User".to_string(),
            }],
        );
        assert!(doc.virtual_source().contains("$user->name"));
        assert!(doc.virtual_source().contains("foreach ($users as $item)"));
        assert!(doc.virtual_source().contains("$item->name"));
        assert!(doc.syntax_diagnostics().is_empty());

        let original_position = Position::new(0, 8);
        let virtual_position = doc
            .map_original_position_to_virtual(original_position)
            .expect("Twig whitespace-control variable should map");
        let virtual_offset = byte_offset_for_position(doc.virtual_source(), virtual_position)
            .expect("virtual position offset");
        assert_eq!(
            doc.virtual_source()
                .get(virtual_offset..virtual_offset + "$user".len()),
            Some("$user")
        );
    }

    #[test]
    fn twig_verbatim_blocks_skip_inner_twig_syntax() {
        let doc = preprocess_twig_template(
            "{% verbatim %}{{ user.name }{% endverbatim %}\n{{ user.name }}\n",
            &[TemplateVariableType {
                name: "user".to_string(),
                type_text: "App\\Entity\\User".to_string(),
            }],
        );
        assert!(doc.syntax_diagnostics().is_empty());
        assert_eq!(doc.virtual_source().matches("$user->name").count(), 1);

        let tokens = doc.map_semantic_tokens_to_original(Vec::new());
        assert!(
            tokens.iter().any(|token| token.token_type == TOKEN_KEYWORD),
            "expected verbatim keyword semantic tokens"
        );
    }

    #[test]
    fn twig_verbatim_finds_end_tag_after_literal_broken_tag_opener() {
        let doc = preprocess_twig_template(
            "{% verbatim %}{% broken {% endverbatim %}\n{{ user.name }}\n",
            &[TemplateVariableType {
                name: "user".to_string(),
                type_text: "App\\Entity\\User".to_string(),
            }],
        );
        assert!(doc.syntax_diagnostics().is_empty());
        assert_eq!(doc.virtual_source().matches("$user->name").count(), 1);
    }

    #[test]
    fn twig_comments_ignore_quotes_while_finding_close_delimiter() {
        let doc = preprocess_twig_template(
            "{# don't map {{ broken } #}\n{{ user.name }}\n",
            &[TemplateVariableType {
                name: "user".to_string(),
                type_text: "App\\Entity\\User".to_string(),
            }],
        );
        assert!(doc.syntax_diagnostics().is_empty());
        assert_eq!(doc.virtual_source().matches("$user->name").count(), 1);

        let tokens = doc.map_semantic_tokens_to_original(Vec::new());
        assert!(
            tokens.iter().any(|token| token.token_type == TOKEN_COMMENT),
            "expected quoted Twig comment semantic token"
        );
    }

    #[test]
    fn twig_macro_blocks_are_valid_syntax_but_not_converted_to_php() {
        let doc = preprocess_twig_template(
            "{% macro input(name) %}{{ name }}{% endmacro %}\n{{ user.name }}\n",
            &[TemplateVariableType {
                name: "user".to_string(),
                type_text: "App\\Entity\\User".to_string(),
            }],
        );
        assert!(doc.syntax_diagnostics().is_empty());
        assert!(!doc.virtual_source().contains("function input"));
        assert!(!doc.virtual_source().contains("$name"));
        assert!(doc.virtual_source().contains("$user->name"));
    }

    #[test]
    fn twig_macro_body_is_still_checked_for_syntax_errors() {
        let doc = preprocess_twig_template("{% macro input(name) %}{{ name }{% endmacro %}\n", &[]);
        let messages: Vec<_> = doc
            .syntax_diagnostics()
            .into_iter()
            .map(|diagnostic| diagnostic.message)
            .collect();
        assert!(
            messages
                .iter()
                .any(|message| message == "Unclosed Twig expression"),
            "expected macro body expression diagnostic, got {messages:?}"
        );
    }

    #[test]
    fn twig_unknown_custom_paired_tags_do_not_report_syntax_errors() {
        let doc = preprocess_twig_template(
            "{% custom %}{{ user.name }}{% endcustom %}\n",
            &[TemplateVariableType {
                name: "user".to_string(),
                type_text: "App\\Entity\\User".to_string(),
            }],
        );
        assert!(doc.syntax_diagnostics().is_empty());
        assert!(doc.virtual_source().contains("$user->name"));
    }

    #[test]
    fn twig_control_blocks_and_template_paths_are_detected() {
        let doc = preprocess_twig_template(
            "{% for item in users %}\n{{ item.name }}\n{% endfor %}\n{% include 'shared/card.html.twig' %}\n",
            &[],
        );
        assert!(doc.virtual_source().contains("foreach ($users as $item)"));
        assert!(doc.virtual_source().contains("$item->name"));

        let context = doc
            .twig_template_path_context_at_position(Position::new(3, 23))
            .expect("include path context");
        assert_eq!(context.key, "shared/card.html.twig");
        assert_eq!(context.prefix, "shared/card");

        let tokens = doc.map_semantic_tokens_to_original(Vec::new());
        assert!(
            tokens.iter().any(|token| token.token_type == TOKEN_KEYWORD),
            "expected Twig keyword semantic tokens"
        );
    }

    #[test]
    fn twig_unsupported_expression_classifier_covers_backlog_constructs() {
        let macro_aliases = HashSet::new();
        for (source, expected) in [
            ("user.name|upper", UnsupportedTwigExpression::Filter),
            ("user is defined", UnsupportedTwigExpression::Test),
            ("user.id in ids", UnsupportedTwigExpression::InOperator),
            ("path('dashboard')", UnsupportedTwigExpression::FunctionCall),
            (
                "user.active ? 'yes' : 'no'",
                UnsupportedTwigExpression::Ternary,
            ),
            (
                "user.name ?? 'n/a'",
                UnsupportedTwigExpression::NullCoalescing,
            ),
            (
                "user['name']",
                UnsupportedTwigExpression::ComplexAttributeAccess,
            ),
            ("1..5", UnsupportedTwigExpression::ComplexAttributeAccess),
        ] {
            assert_eq!(
                unsupported_twig_expression(source, 0, source.len(), &macro_aliases),
                Some(expected),
                "expected unsupported kind for `{source}`"
            );
        }

        let mut macro_aliases = HashSet::new();
        macro_aliases.insert("forms".to_string());
        assert_eq!(
            unsupported_twig_expression(
                "forms.input(user)",
                0,
                "forms.input(user)".len(),
                &macro_aliases
            ),
            Some(UnsupportedTwigExpression::MacroCall)
        );
        assert_eq!(
            unsupported_twig_expression(
                "_self.input(user)",
                0,
                "_self.input(user)".len(),
                &HashSet::new()
            ),
            Some(UnsupportedTwigExpression::MacroCall)
        );
        assert_eq!(
            unsupported_twig_expression(
                "user.setAge(123)",
                0,
                "user.setAge(123)".len(),
                &HashSet::new()
            ),
            None,
            "plain object method calls remain best-effort PHP mappings"
        );
    }

    #[test]
    fn twig_unsupported_complex_expressions_emit_unmapped_placeholders() {
        let source = concat!(
            "{% import 'forms.html.twig' as forms %}\n",
            "{{ user.name|upper }}\n",
            "{% if user is defined %}visible{% endif %}\n",
            "{% for item in users|filter(u => u.active) %}{{ item.name }}{% endfor %}\n",
            "{% set label = attribute(user, dynamic_name) %}\n",
            "{{ forms.input(user) }}\n",
            "{{ _self.card(user) }}\n",
            "{{ user.name ?? 'n/a' }}\n",
            "{{ user['name'] }}\n",
        );
        let doc = preprocess_twig_template(
            source,
            &[TemplateVariableType {
                name: "user".to_string(),
                type_text: "App\\Entity\\User".to_string(),
            }],
        );

        assert!(doc.virtual_source().contains("<?php echo null; ?>"));
        assert!(doc.virtual_source().contains("<?php if (true): ?>"));
        assert!(doc
            .virtual_source()
            .contains("<?php foreach ((array) [] as $item): ?>"));
        assert!(doc.virtual_source().contains("$label = null"));

        for needle in [
            "upper",
            "user is defined",
            "users|filter",
            "attribute",
            "forms.input",
            "_self.card",
            "??",
            "user['name']",
        ] {
            let original_offset = source.find(needle).expect("fixture needle");
            let original_position = position_for_byte_offset(source, original_offset);
            assert!(
                doc.map_original_position_to_virtual(original_position)
                    .is_none(),
                "unsupported Twig expression `{needle}` should not map to virtual PHP"
            );
        }
    }

    #[test]
    fn twig_unsupported_expressions_map_inner_member_chains_to_virtual_php() {
        let source = concat!(
            "{{ user.name|upper }}\n",
            "{% if user.items is iterable and user.items|length > 0 %}visible{% endif %}\n",
            "{% set shown = user.items|slice(0, 5) %}\n",
            "{{ path('profile', {'id': user.profile.id}) }}\n",
            "{{ path('profile', {'id': user.}) }}\n",
            "{{ user.createdAt|date('d.m.Y') }}\n",
        );
        let doc = preprocess_twig_template(
            source,
            &[TemplateVariableType {
                name: "user".to_string(),
                type_text: "App\\Entity\\User".to_string(),
            }],
        );

        for expected in [
            "$shown = $user->items;",
            "$user->name;",
            "$user->items;",
            "$user->profile->id;",
            "$user->;",
            "$user->createdAt;",
        ] {
            assert!(
                doc.virtual_source().contains(expected),
                "expected partial Twig member chain `{expected}` in virtual PHP, got: {}",
                doc.virtual_source()
            );
        }
        assert_eq!(
            doc.virtual_source().matches("$user->items;").count(),
            3,
            "slice base should be mapped by the assignment without a duplicate no-op fragment, got: {}",
            doc.virtual_source()
        );

        for needle in [
            "user.name",
            "user.items",
            "user.profile.id",
            "user.createdAt",
        ] {
            let original_offset = source.find(needle).expect("fixture member chain");
            let original_position = position_for_byte_offset(source, original_offset);
            let virtual_position = doc
                .map_original_position_to_virtual(original_position)
                .unwrap_or_else(|| panic!("member chain `{needle}` should map"));
            let virtual_offset = byte_offset_for_position(doc.virtual_source(), virtual_position)
                .expect("virtual position offset");
            assert_eq!(
                doc.virtual_source()
                    .get(virtual_offset..virtual_offset + "$user".len()),
                Some("$user"),
                "member chain `{needle}` should map to a virtual PHP variable"
            );
        }
        let trailing_dot_offset = source
            .find("user.})")
            .map(|offset| offset + "user.".len())
            .expect("fixture trailing member access");
        let trailing_dot_position = position_for_byte_offset(source, trailing_dot_offset);
        let trailing_virtual_position = doc
            .map_original_position_to_virtual(trailing_dot_position)
            .expect("trailing member access cursor should map");
        let trailing_virtual_offset =
            byte_offset_for_position(doc.virtual_source(), trailing_virtual_position)
                .expect("trailing virtual offset");
        assert_eq!(
            doc.virtual_source()
                .get(trailing_virtual_offset.saturating_sub("->".len())..trailing_virtual_offset),
            Some("->"),
            "cursor after trailing Twig dot should map after virtual PHP member arrow"
        );

        for needle in ["upper", "path", "date"] {
            let original_offset = source.find(needle).expect("fixture unsupported token");
            let original_position = position_for_byte_offset(source, original_offset);
            assert!(
                doc.map_original_position_to_virtual(original_position)
                    .is_none(),
                "unsupported Twig token `{needle}` should stay unmapped"
            );
        }
    }

    #[test]
    fn twig_generated_context_diagnostics_are_unmapped() {
        let doc = preprocess_twig_template(
            "{{ user.name }}",
            &[TemplateVariableType {
                name: "user".to_string(),
                type_text: "App\\Entity\\User".to_string(),
            }],
        );
        let generated_prelude = Range::new(Position::new(1, 0), Position::new(1, 5));
        assert!(doc
            .map_virtual_range_to_original(generated_prelude)
            .is_none());
    }

    #[test]
    fn safe_template_diagnostics_map_exact_unknown_members_and_suppress_syntax() {
        let source = "<div>{{ (new User())->missing() }}</div>";
        let doc = preprocess_blade_template(source);
        let original_start = source.find("missing").expect("missing member in fixture");
        let original_end = original_start + "missing".len();
        let virtual_start = doc
            .source_map
            .original_to_virtual(original_start)
            .expect("member start should map");
        let virtual_end = doc
            .source_map
            .original_to_virtual(original_end)
            .expect("member end should map");
        let virtual_range =
            range_for_byte_offsets(doc.virtual_source(), virtual_start, virtual_end);

        let unknown_member = Diagnostic {
            range: virtual_range,
            source: Some("php-lsp".to_string()),
            code: Some(NumberOrString::String("php-lsp.members".to_string())),
            message: "Unknown method: User::missing".to_string(),
            ..Default::default()
        };
        let mapped = doc.map_safe_diagnostics_to_original(vec![unknown_member]);
        assert_eq!(mapped.len(), 1);
        assert_eq!(
            mapped[0].range,
            range_for_byte_offsets(source, original_start, original_end)
        );

        let syntax = Diagnostic {
            range: virtual_range,
            source: Some("php-lsp".to_string()),
            message: "Syntax error".to_string(),
            ..Default::default()
        };
        assert!(doc
            .map_safe_diagnostics_to_original(vec![syntax])
            .is_empty());
    }

    #[test]
    fn safe_template_diagnostics_require_full_source_map_coverage() {
        let doc = preprocess_blade_template("{{ $user }}");
        let generated_prefix = Range::new(Position::new(0, 0), Position::new(0, 5));
        let diagnostic = Diagnostic {
            range: generated_prefix,
            source: Some("php-lsp".to_string()),
            code: Some(NumberOrString::String("php-lsp.members".to_string())),
            message: "Unknown property: User::$name".to_string(),
            ..Default::default()
        };

        assert!(doc
            .map_safe_diagnostics_to_original(vec![diagnostic])
            .is_empty());
    }

    #[test]
    fn safe_template_diagnostics_suppress_unknown_properties() {
        let source = "{{ (new User())->missing }}";
        let doc = preprocess_blade_template(source);
        let original_start = source.find("missing").expect("property in fixture");
        let original_end = original_start + "missing".len();
        let virtual_start = doc
            .source_map
            .original_to_virtual(original_start)
            .expect("property start should map");
        let virtual_end = doc
            .source_map
            .original_to_virtual(original_end)
            .expect("property end should map");
        let diagnostic = Diagnostic {
            range: range_for_byte_offsets(doc.virtual_source(), virtual_start, virtual_end),
            source: Some("php-lsp".to_string()),
            code: Some(NumberOrString::String("php-lsp.members".to_string())),
            message: "Unknown property: User::$missing".to_string(),
            ..Default::default()
        };

        assert!(doc
            .map_safe_diagnostics_to_original(vec![diagnostic])
            .is_empty());
    }

    #[test]
    fn twig_safe_template_diagnostics_suppress_undefined_variables() {
        let source = "{{ standaloneVariable }}";
        let doc = preprocess_twig_template(source, &[]);
        let original_start = source
            .find("standaloneVariable")
            .expect("variable in fixture");
        let original_end = original_start + "standaloneVariable".len();
        let virtual_start = doc
            .source_map
            .original_to_virtual(original_start)
            .expect("variable start should map");
        let virtual_end = doc
            .source_map
            .original_to_virtual(original_end)
            .expect("variable end should map");
        let diagnostic = Diagnostic {
            range: range_for_byte_offsets(doc.virtual_source(), virtual_start, virtual_end),
            source: Some("php-lsp".to_string()),
            code: Some(NumberOrString::String(
                "php-lsp.undefinedVariable".to_string(),
            )),
            message: "Undefined variable: $standaloneVariable".to_string(),
            ..Default::default()
        };

        assert!(doc
            .map_safe_diagnostics_to_original(vec![diagnostic])
            .is_empty());
    }

    #[test]
    fn twig_copied_expression_tokens_map_for_type_diagnostics() {
        let source = "{{ user.setAge(123) }}";
        let doc = preprocess_twig_template(
            source,
            &[TemplateVariableType {
                name: "user".to_string(),
                type_text: "App\\Entity\\User".to_string(),
            }],
        );
        let original_start = source.find("123").expect("numeric literal in fixture");
        let original_end = original_start + "123".len();
        let virtual_start = doc
            .source_map
            .original_to_virtual(original_start)
            .expect("literal start should map");
        let virtual_end = doc
            .source_map
            .original_to_virtual(original_end)
            .expect("literal end should map");
        let diagnostic = Diagnostic {
            range: range_for_byte_offsets(doc.virtual_source(), virtual_start, virtual_end),
            source: Some("php-lsp".to_string()),
            code: Some(NumberOrString::String(
                "php-lsp.typeCompatibility".to_string(),
            )),
            message: "Type mismatch for App\\Entity\\User::setAge argument $age: expected string, got int"
                .to_string(),
            ..Default::default()
        };

        let mapped = doc.map_safe_diagnostics_to_original(vec![diagnostic]);
        assert_eq!(mapped.len(), 1);
        assert_eq!(
            mapped[0].range,
            range_for_byte_offsets(source, original_start, original_end)
        );
    }
}
