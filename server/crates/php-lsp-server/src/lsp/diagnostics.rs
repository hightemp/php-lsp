//! Diagnostics LSP handlers extracted from `server.rs`.

use super::super::*;
use php_lsp_parser::resolve::{
    symbol_at_position_with_full_resolvers, FunctionTypeResolver, ResolvedFunctionType,
};
use tracing::Instrument;

fn build_analyzer_shell_command(template: &str, file_path: &Path) -> String {
    let escaped_file = shell_escape(&file_path.to_string_lossy());
    if template.contains("{file}") {
        template.replace("{file}", &escaped_file)
    } else {
        format!("{} {}", template, escaped_file)
    }
}

fn build_phpstan_shell_command(config: &PhpStanConfig, file_path: &Path) -> String {
    let mut template = config.command.clone();
    if let Some(memory_limit) = config.memory_limit.as_deref() {
        if template.contains("{memory_limit}") {
            template = template.replace("{memory_limit}", &shell_escape(memory_limit));
        } else if !template.contains("--memory-limit") {
            template.push_str(" --memory-limit=");
            template.push_str(&shell_escape(memory_limit));
        }
    } else if template.contains("{memory_limit}") {
        template = template.replace("{memory_limit}", "");
    }

    build_analyzer_shell_command(&template, file_path)
}

fn phpstan_json_message_line(message: &serde_json::Value) -> Option<u32> {
    message
        .get("line")
        .and_then(|value| value.as_u64())
        .and_then(|line| u32::try_from(line).ok())
}

fn phpstan_json_message_u32(message: &serde_json::Value, key: &str) -> Option<u32> {
    message
        .get(key)
        .and_then(|value| value.as_u64())
        .and_then(|value| u32::try_from(value).ok())
}

fn phpstan_file_key_matches(key: &str, target: &Path) -> bool {
    let key_path = PathBuf::from(key);
    if key_path == target {
        return true;
    }

    if let (Ok(key_canonical), Ok(target_canonical)) =
        (key_path.canonicalize(), target.canonicalize())
    {
        return key_canonical == target_canonical;
    }

    false
}

fn phpstan_message_to_diagnostic(message: &serde_json::Value) -> Option<Diagnostic> {
    let raw_message = message.get("message")?.as_str()?;
    let line = phpstan_json_message_line(message).unwrap_or(1).max(1);
    let start_line = line - 1;
    let start_character = phpstan_json_message_u32(message, "column")
        .unwrap_or(1)
        .saturating_sub(1);
    let end_line = phpstan_json_message_u32(message, "endLine")
        .unwrap_or(line)
        .max(1)
        - 1;
    let end_character = phpstan_json_message_u32(message, "endColumn")
        .map(|column| column.saturating_sub(1))
        .unwrap_or(start_character + 1);

    let tip = message
        .get("tip")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let diagnostic_message = if let Some(tip) = tip {
        format!("{}\n\n{}", raw_message, tip)
    } else {
        raw_message.to_string()
    };

    Some(Diagnostic {
        range: Range {
            start: Position::new(start_line, start_character),
            end: Position::new(end_line, end_character),
        },
        severity: Some(DiagnosticSeverity::ERROR),
        code: message
            .get("identifier")
            .and_then(|value| value.as_str())
            .map(|identifier| NumberOrString::String(identifier.to_string())),
        source: Some("phpstan".to_string()),
        message: diagnostic_message,
        ..Default::default()
    })
}

pub(in crate::server) fn parse_phpstan_json_diagnostics(
    stdout: &str,
    file_path: &Path,
) -> std::result::Result<Vec<Diagnostic>, String> {
    let value: serde_json::Value =
        serde_json::from_str(stdout).map_err(|err| format!("invalid PHPStan JSON: {}", err))?;
    let Some(files) = value.get("files").and_then(|files| files.as_object()) else {
        return Ok(vec![]);
    };

    let mut diagnostics = Vec::new();
    for (file_key, file_value) in files {
        if files.len() != 1 && !phpstan_file_key_matches(file_key, file_path) {
            continue;
        }

        let Some(messages) = file_value
            .get("messages")
            .and_then(|value| value.as_array())
        else {
            continue;
        };

        diagnostics.extend(messages.iter().filter_map(phpstan_message_to_diagnostic));
    }

    Ok(diagnostics)
}

pub(in crate::server) async fn run_phpstan_for_file(
    config: PhpStanConfig,
    file_path: PathBuf,
    workspace_root: Option<PathBuf>,
    cancellation: Option<OperationCancellationToken>,
) -> std::result::Result<Vec<Diagnostic>, String> {
    let command = build_phpstan_shell_command(&config, &file_path);
    let output = run_shell_command_with_timeout(
        "PHPStan",
        &command,
        workspace_root.as_deref(),
        config.timeout_ms,
        cancellation,
    )
    .await?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    if stdout.trim().is_empty() {
        if output.status.success() {
            return Ok(vec![]);
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        let details = stderr.trim();
        return Err(if details.is_empty() {
            format!("PHPStan command exited with {}", output.status)
        } else {
            format!("PHPStan command exited with {}: {}", output.status, details)
        });
    }

    parse_phpstan_json_diagnostics(&stdout, &file_path).map_err(|err| {
        if output.status.success() {
            err
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let details = stderr.trim();
            if details.is_empty() {
                format!("{} (exit {})", err, output.status)
            } else {
                format!("{} (exit {}: {})", err, output.status, details)
            }
        }
    })
}

fn psalm_issue_u32(issue: &serde_json::Value, key: &str) -> Option<u32> {
    issue
        .get(key)
        .and_then(|value| value.as_u64())
        .and_then(|value| u32::try_from(value).ok())
}

fn psalm_issue_path_matches(issue: &serde_json::Value, target: &Path) -> bool {
    let Some(path) = issue
        .get("file_path")
        .or_else(|| issue.get("file_name"))
        .and_then(|value| value.as_str())
    else {
        return true;
    };

    phpstan_file_key_matches(path, target)
}

fn psalm_severity(issue: &serde_json::Value) -> DiagnosticSeverity {
    match issue
        .get("severity")
        .and_then(|value| value.as_str())
        .map(|value| value.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("info") => DiagnosticSeverity::INFORMATION,
        Some("warning") => DiagnosticSeverity::WARNING,
        Some("error") | None => DiagnosticSeverity::ERROR,
        _ => DiagnosticSeverity::ERROR,
    }
}

fn psalm_issue_to_diagnostic(issue: &serde_json::Value) -> Option<Diagnostic> {
    let message = issue.get("message")?.as_str()?.to_string();
    let line_from = psalm_issue_u32(issue, "line_from").unwrap_or(1).max(1);
    let line_to = psalm_issue_u32(issue, "line_to")
        .unwrap_or(line_from)
        .max(1);
    let start_character = psalm_issue_u32(issue, "column_from")
        .unwrap_or(1)
        .saturating_sub(1);
    let end_character = psalm_issue_u32(issue, "column_to")
        .map(|column| column.saturating_sub(1))
        .unwrap_or(start_character + 1);

    Some(Diagnostic {
        range: Range {
            start: Position::new(line_from - 1, start_character),
            end: Position::new(line_to - 1, end_character),
        },
        severity: Some(psalm_severity(issue)),
        code: issue
            .get("type")
            .and_then(|value| value.as_str())
            .or_else(|| issue.get("shortcode").and_then(|value| value.as_str()))
            .map(|code| NumberOrString::String(code.to_string())),
        source: Some("psalm".to_string()),
        message,
        ..Default::default()
    })
}

pub(in crate::server) fn parse_psalm_json_diagnostics(
    stdout: &str,
    file_path: &Path,
) -> std::result::Result<Vec<Diagnostic>, String> {
    let value: serde_json::Value =
        serde_json::from_str(stdout).map_err(|err| format!("invalid Psalm JSON: {}", err))?;
    let issues = value
        .as_array()
        .or_else(|| value.get("issues").and_then(|issues| issues.as_array()))
        .or_else(|| value.get("errors").and_then(|errors| errors.as_array()));

    let Some(issues) = issues else {
        return Ok(vec![]);
    };

    Ok(issues
        .iter()
        .filter(|issue| psalm_issue_path_matches(issue, file_path))
        .filter_map(psalm_issue_to_diagnostic)
        .collect())
}

pub(in crate::server) async fn run_psalm_for_file(
    config: PsalmConfig,
    file_path: PathBuf,
    workspace_root: Option<PathBuf>,
    cancellation: Option<OperationCancellationToken>,
) -> std::result::Result<Vec<Diagnostic>, String> {
    let command = build_analyzer_shell_command(&config.command, &file_path);
    let output = run_shell_command_with_timeout(
        "Psalm",
        &command,
        workspace_root.as_deref(),
        config.timeout_ms,
        cancellation,
    )
    .await?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    if stdout.trim().is_empty() {
        if output.status.success() {
            return Ok(vec![]);
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        let details = stderr.trim();
        return Err(if details.is_empty() {
            format!("Psalm command exited with {}", output.status)
        } else {
            format!("Psalm command exited with {}: {}", output.status, details)
        });
    }

    parse_psalm_json_diagnostics(&stdout, &file_path).map_err(|err| {
        if output.status.success() {
            err
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let details = stderr.trim();
            if details.is_empty() {
                format!("{} (exit {})", err, output.status)
            } else {
                format!("{} (exit {}: {})", err, output.status, details)
            }
        }
    })
}

impl PhpLspBackend {
    pub(crate) async fn lsp_did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let uri_str = uri.as_str().to_string();
        let text = &params.text_document.text;
        let version = params.text_document.version;
        let template_kind = template_kind_for_document(&uri_str, &params.text_document.language_id);

        tracing::debug!("didOpen: {}", uri_str);
        self.log_trace(&format!("didOpen: {}", uri_str)).await;
        self.document_versions.insert(uri_str.clone(), version);
        self.cancel_debounced_diagnostics(&uri_str).await;
        self.cancel_analyzer_run(&uri_str).await;
        self.cancel_formatter_run(&uri_str).await;

        if let Some(template_kind) = template_kind {
            let twig_variable_types = if template_kind == TemplateKind::Twig {
                self.twig_variable_types_for_template(&uri_str).await
            } else {
                Vec::new()
            };
            let parser =
                self.open_template_document(&uri_str, text, template_kind, &twig_variable_types);
            self.index.remove_file(&uri_str);
            self.open_files.insert(uri_str, parser);
            self.publish_diagnostics(&uri).await;
            return;
        }

        self.template_documents.remove(&uri_str);
        let mut parser = FileParser::new();
        parser.parse_full(text);

        // Update index with symbols from this file
        let excluded = if let Some(path) = uri_to_path(&uri_str) {
            self.path_is_excluded_by_config(&path).await
        } else {
            false
        };
        if !excluded {
            if let Some(tree) = parser.tree() {
                let file_symbols = extract_file_symbols(tree, text, &uri_str);
                let references = collect_symbol_references_in_file(tree, text, &file_symbols);
                let sym_count = file_symbols.symbols.len();
                self.index
                    .update_file_with_references(&uri_str, file_symbols, references);
                self.log_trace(&format!("Indexed {} symbols from {}", sym_count, uri_str))
                    .await;
            }
        } else {
            self.index.remove_file(&uri_str);
        }

        self.open_files.insert(uri_str, parser);

        self.publish_diagnostics(&uri).await;
        if uri_is_php_file(&uri) {
            self.invalidate_twig_context_disk_cache_for_source_uri(uri.as_str())
                .await;
            self.refresh_open_twig_contexts_and_republish_diagnostics()
                .await;
        }
    }

    pub(crate) async fn lsp_did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let uri_str = uri.as_str().to_string();
        let version = params.text_document.version;

        tracing::debug!("didChange: {} version {}", uri_str, version);
        if !self.accept_document_version(&uri_str, version) {
            return;
        }
        self.cancel_analyzer_run(&uri_str).await;
        self.cancel_formatter_run(&uri_str).await;

        if let Some(template) = self.template_document(&uri_str) {
            let updated = params
                .content_changes
                .iter()
                .fold(template, |template, change| {
                    template.apply_change(change.range, &change.text)
                });
            let refresh_twig_contexts = updated.kind() == TemplateKind::Twig;
            let mut parser = FileParser::new();
            parser.parse_full(updated.virtual_source());
            self.template_documents.insert(uri_str.clone(), updated);
            self.index.remove_file(&uri_str);
            self.open_files.insert(uri_str.clone(), parser);
            self.semantic_tokens_cache.lock().await.remove(&uri_str);
            self.schedule_fast_diagnostics(uri, version).await;
            if refresh_twig_contexts {
                self.refresh_open_twig_contexts_and_republish_diagnostics()
                    .await;
            }
            return;
        }

        let excluded = if let Some(path) = uri_to_path(&uri_str) {
            self.path_is_excluded_by_config(&path).await
        } else {
            false
        };

        if let Some(mut parser) = self.open_files.get_mut(&uri_str) {
            for change in &params.content_changes {
                if let Some(range) = change.range {
                    parser.apply_edit(
                        range.start.line,
                        range.start.character,
                        range.end.line,
                        range.end.character,
                        &change.text,
                    );
                } else {
                    // Full content replacement
                    parser.parse_full(&change.text);
                }
            }

            // Update index with new symbols
            if excluded {
                self.index.remove_file(&uri_str);
            } else if let Some(tree) = parser.tree() {
                let source = parser.source();
                let file_symbols = extract_file_symbols(tree, &source, &uri_str);
                let references = collect_symbol_references_in_file(tree, &source, &file_symbols);
                self.index
                    .update_file_with_references(&uri_str, file_symbols, references);
            }
        }

        let refresh_twig_contexts = uri_is_php_file(&uri);
        self.schedule_fast_diagnostics(uri, version).await;
        if refresh_twig_contexts {
            self.invalidate_twig_context_disk_cache_for_source_uri(&uri_str)
                .await;
            self.refresh_open_twig_contexts_and_republish_diagnostics()
                .await;
        }
    }

    pub(crate) async fn lsp_did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        let uri_str = uri.as_str().to_string();
        tracing::debug!("didClose: {}", uri_str);
        let refresh_twig_contexts =
            uri_is_php_file(&uri) && !self.template_documents.contains_key(&uri_str);
        self.open_files.remove(&uri_str);
        self.template_documents.remove(&uri_str);
        self.document_versions.remove(&uri_str);
        self.cancel_debounced_diagnostics(&uri_str).await;
        self.cancel_analyzer_run(&uri_str).await;
        self.cancel_formatter_run(&uri_str).await;
        self.semantic_tokens_cache.lock().await.remove(&uri_str);
        // Clear diagnostics for closed file
        self.client.publish_diagnostics(uri, vec![], None).await;
        if refresh_twig_contexts {
            self.refresh_open_twig_contexts_and_republish_diagnostics()
                .await;
        }
    }

    pub(crate) async fn lsp_did_save(&self, params: DidSaveTextDocumentParams) {
        tracing::debug!("didSave: {}", params.text_document.uri.as_str());
        let refresh_twig_contexts = uri_is_php_file(&params.text_document.uri)
            && !self
                .template_documents
                .contains_key(params.text_document.uri.as_str());
        self.invalidate_request_fs_caches().await;
        self.cancel_debounced_diagnostics(params.text_document.uri.as_str())
            .await;
        self.publish_diagnostics(&params.text_document.uri).await;
        if refresh_twig_contexts {
            self.refresh_open_twig_contexts_and_republish_diagnostics()
                .await;
        }
    }
}

/// Compute diagnostics for a file (syntax + semantic).
///
/// Extracted as a free function so it can be called both from
/// `publish_diagnostics` and from post-indexing re-checks.
pub(in crate::server) async fn compute_open_file_diagnostics(
    uri_str: &str,
    open_files: &DashMap<String, FileParser>,
    index: &Arc<WorkspaceIndex>,
    diagnostics_config: DiagnosticsRuntimeConfig,
    document_version: Option<i32>,
) -> Vec<Diagnostic> {
    let Some(source) = open_files.get(uri_str).map(|parser| parser.source()) else {
        return vec![];
    };

    compute_source_diagnostics_blocking(
        uri_str.to_string(),
        source,
        index.clone(),
        diagnostics_config,
        document_version,
    )
    .await
}

pub(in crate::server) async fn compute_source_diagnostics_blocking(
    uri_str: String,
    source: String,
    index: Arc<WorkspaceIndex>,
    diagnostics_config: DiagnosticsRuntimeConfig,
    document_version: Option<i32>,
) -> Vec<Diagnostic> {
    run_diagnostics_blocking(uri_str.clone(), document_version, move || {
        let mut parser = FileParser::new();
        parser.parse_full(&source);
        compute_diagnostics_with_config_for_version(
            &uri_str,
            &parser,
            &index,
            diagnostics_config,
            document_version,
        )
    })
    .await
}

pub(in crate::server) async fn run_diagnostics_blocking<F>(
    uri_str: String,
    document_version: Option<i32>,
    compute: F,
) -> Vec<Diagnostic>
where
    F: FnOnce() -> Vec<Diagnostic> + Send + 'static,
{
    let queued_at = Instant::now();
    let task_uri = uri_str.clone();
    let task = tokio::task::spawn_blocking(move || {
        let queue_wait = queued_at.elapsed();
        let queue_span = tracing::debug_span!(
            "diagnostics.queue_wait",
            uri = %uri_str,
            version = ?document_version,
            duration_ms = queue_wait.as_millis() as u64,
        );
        {
            let _entered = queue_span.enter();
            tracing::debug!("diagnostics compute dequeued");
        }

        let compute_started = Instant::now();
        let compute_span = tracing::debug_span!(
            "diagnostics.compute",
            uri = %uri_str,
            version = ?document_version,
            duration_ms = tracing::field::Empty,
        );
        let _entered = compute_span.enter();

        let diagnostics = compute();
        compute_span.record("duration_ms", compute_started.elapsed().as_millis() as u64);
        diagnostics
    });

    match task.await {
        Ok(diagnostics) => diagnostics,
        Err(err) => {
            tracing::warn!(
                uri = %task_uri,
                version = ?document_version,
                "Diagnostics blocking task failed: {err}"
            );
            vec![]
        }
    }
}

pub(in crate::server) fn current_parser_symbol_references(
    uri_str: &str,
    parser: &FileParser,
) -> Vec<php_lsp_types::SymbolReference> {
    let Some(tree) = parser.tree() else {
        return Vec::new();
    };
    let source = parser.source();
    let file_symbols = extract_file_symbols(tree, &source, uri_str);
    collect_symbol_references_in_file(tree, &source, &file_symbols)
}

pub(in crate::server) fn symbol_reference_matches(
    index: &WorkspaceIndex,
    reference: &php_lsp_types::SymbolReference,
    target_fqn: &str,
    target_kind: php_lsp_types::PhpSymbolKind,
    include_declaration: bool,
) -> bool {
    if reference.is_declaration && !include_declaration {
        return false;
    }

    if reference.target_fqn == target_fqn
        && reference_kind_matches(reference.target_kind, target_kind)
    {
        return true;
    }

    if !reference_kind_matches(reference.target_kind, target_kind) {
        return false;
    }

    if reference.is_declaration {
        member_declaration_matches_related_owner(index, reference, target_fqn, target_kind)
    } else {
        member_reference_matches_related_receiver(index, reference, target_fqn, target_kind)
    }
}

fn member_declaration_matches_related_owner(
    index: &WorkspaceIndex,
    reference: &php_lsp_types::SymbolReference,
    target_fqn: &str,
    target_kind: php_lsp_types::PhpSymbolKind,
) -> bool {
    if !is_member_symbol_kind(target_kind) {
        return false;
    }

    let Some((target_owner, target_member)) = target_fqn.rsplit_once("::") else {
        return false;
    };
    let Some((reference_owner, reference_member)) = reference.target_fqn.rsplit_once("::") else {
        return false;
    };
    member_names_match(reference_member, target_member, target_kind)
        && related_member_owner_matches(index, reference_owner, target_owner)
}

fn member_reference_matches_related_receiver(
    index: &WorkspaceIndex,
    reference: &php_lsp_types::SymbolReference,
    target_fqn: &str,
    target_kind: php_lsp_types::PhpSymbolKind,
) -> bool {
    if !is_member_symbol_kind(target_kind) {
        return false;
    }

    let Some((target_owner, target_member)) = target_fqn.rsplit_once("::") else {
        return false;
    };
    let Some((_, reference_member)) = reference.target_fqn.rsplit_once("::") else {
        return false;
    };
    if !member_names_match(reference_member, target_member, target_kind) {
        return false;
    }

    let Some(receiver_fqn) = reference.receiver.receiver_fqn() else {
        return false;
    };

    related_member_owner_matches(index, receiver_fqn, target_owner)
}

fn related_member_owner_matches(
    index: &WorkspaceIndex,
    reference_owner: &str,
    target_owner: &str,
) -> bool {
    fqn_matches(reference_owner, target_owner)
        || class_extends_or_implements(index, reference_owner, target_owner, &mut Vec::new())
        || class_or_ancestor_uses_trait(index, reference_owner, target_owner, &mut Vec::new())
}

fn member_names_match(
    reference_member: &str,
    target_member: &str,
    target_kind: php_lsp_types::PhpSymbolKind,
) -> bool {
    if target_kind == php_lsp_types::PhpSymbolKind::Property {
        return reference_member.trim_start_matches('$') == target_member.trim_start_matches('$');
    }

    reference_member == target_member
}

fn is_member_symbol_kind(kind: php_lsp_types::PhpSymbolKind) -> bool {
    matches!(
        kind,
        php_lsp_types::PhpSymbolKind::Method
            | php_lsp_types::PhpSymbolKind::Property
            | php_lsp_types::PhpSymbolKind::ClassConstant
            | php_lsp_types::PhpSymbolKind::EnumCase
    )
}

pub(in crate::server) fn reference_kind_matches(
    reference_kind: php_lsp_types::PhpSymbolKind,
    target_kind: php_lsp_types::PhpSymbolKind,
) -> bool {
    if reference_kind == target_kind {
        return true;
    }

    is_class_like_kind(reference_kind) && is_class_like_kind(target_kind)
}

pub(in crate::server) fn is_class_like_kind(kind: php_lsp_types::PhpSymbolKind) -> bool {
    matches!(
        kind,
        php_lsp_types::PhpSymbolKind::Class
            | php_lsp_types::PhpSymbolKind::Interface
            | php_lsp_types::PhpSymbolKind::Trait
            | php_lsp_types::PhpSymbolKind::Enum
    )
}

#[cfg(test)]
pub(in crate::server) fn compute_diagnostics(
    uri_str: &str,
    parser: &FileParser,
    index: &WorkspaceIndex,
    diagnostics_mode: DiagnosticsMode,
    php_version: PhpVersion,
) -> Vec<Diagnostic> {
    compute_diagnostics_with_runtime_config(
        uri_str,
        parser,
        index,
        DiagnosticsRuntimeConfig {
            mode: diagnostics_mode,
            php_version,
            ..DiagnosticsRuntimeConfig::default()
        },
        None,
    )
}

#[cfg(test)]
pub(crate) fn compute_diagnostics_with_config(
    uri_str: &str,
    parser: &FileParser,
    index: &WorkspaceIndex,
    diagnostics_mode: DiagnosticsMode,
    diagnostic_severity: DiagnosticSeverityConfig,
    php_version: PhpVersion,
) -> Vec<Diagnostic> {
    compute_diagnostics_with_runtime_config(
        uri_str,
        parser,
        index,
        DiagnosticsRuntimeConfig {
            mode: diagnostics_mode,
            severity: diagnostic_severity,
            php_version,
            ..DiagnosticsRuntimeConfig::default()
        },
        None,
    )
}

pub(crate) fn compute_diagnostics_with_runtime_config(
    uri_str: &str,
    parser: &FileParser,
    index: &WorkspaceIndex,
    diagnostics_config: DiagnosticsRuntimeConfig,
    document_version: Option<i32>,
) -> Vec<Diagnostic> {
    compute_diagnostics_with_config_for_version(
        uri_str,
        parser,
        index,
        diagnostics_config,
        document_version,
    )
}

pub(in crate::server) fn compute_diagnostics_with_config_for_version(
    uri_str: &str,
    parser: &FileParser,
    index: &WorkspaceIndex,
    diagnostics_config: DiagnosticsRuntimeConfig,
    document_version: Option<i32>,
) -> Vec<Diagnostic> {
    let diagnostics_started = Instant::now();
    if diagnostics_config.mode == DiagnosticsMode::Off {
        return vec![];
    }
    let diagnostic_severity = diagnostics_config.severity;
    let php_version = diagnostics_config.php_version;

    let tree = match parser.tree() {
        Some(t) => t,
        None => return vec![],
    };
    let source = parser.source();
    let utf16_index = Utf16LineIndex::new(&source);
    let type_cache = RequestTypeCache::new(uri_str, document_version);
    let framework_cache = crate::framework::FrameworkProviderCache::default();

    // Syntax errors (ERROR / MISSING nodes)
    let lsp_diags = extract_syntax_errors(tree, &source);
    let mut diagnostics: Vec<Diagnostic> = lsp_diags
        .into_iter()
        .map(|d| {
            // tree-sitter positions use byte columns; convert to UTF-16
            let start_char =
                utf16_index.byte_col_to_utf16(d.range.start.line, d.range.start.character);
            let end_char = utf16_index.byte_col_to_utf16(d.range.end.line, d.range.end.character);
            Diagnostic {
                range: Range {
                    start: Position::new(d.range.start.line, start_char),
                    end: Position::new(d.range.end.line, end_char),
                },
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("php-lsp".to_string()),
                message: d.message,
                ..Default::default()
            }
        })
        .collect();

    // Avoid semantic noise while the file has syntax errors.
    if !diagnostics.is_empty() {
        return diagnostics;
    }

    if diagnostics_config.mode == DiagnosticsMode::SyntaxOnly {
        return diagnostics;
    }

    // Semantic diagnostics (unknown class, function, unresolved use)
    let file_symbols = index
        .file_symbols
        .get(uri_str)
        .map(|entry| entry.value().clone())
        .unwrap_or_default();

    let semantic_started = Instant::now();
    let sem_diags =
        extract_semantic_diagnostics(tree, &source, &file_symbols, |fqn| index.resolve_fqn(fqn));
    warn_if_slow_diagnostic_phase(uri_str, "semantic", semantic_started);

    for sd in sem_diags {
        if let Some(diagnostic) =
            semantic_diagnostic_to_lsp(sd, &utf16_index, diagnostics_config.severity)
        {
            diagnostics.push(diagnostic);
        }
    }

    let member_type_budget_exceeded = member_type_diagnostic_budget_exceeded(
        tree.root_node(),
        diagnostics_config.budget.member_type_node_budget,
    );
    let skip_member_and_type_diagnostics = member_type_budget_exceeded.is_some();

    diagnostics.extend(apply_diagnostic_category(
        workspace_duplicate_symbol_diagnostics(uri_str, &file_symbols, index, &utf16_index),
        DiagnosticCategory::DuplicateSymbols,
        diagnostic_severity,
    ));
    if let Some(limit) = member_type_budget_exceeded {
        tracing::info!(
            uri = %uri_str,
            member_type_node_budget = limit,
            "Skipping member/type diagnostics because the file exceeded the configured diagnostics budget"
        );
        if diagnostics_config.budget.partial_analysis_diagnostic {
            diagnostics.push(partial_analysis_budget_diagnostic(limit));
        }
    }
    if diagnostic_severity
        .severity(DiagnosticCategory::Members)
        .is_some()
        && !skip_member_and_type_diagnostics
    {
        let members_started = Instant::now();
        diagnostics.extend(apply_diagnostic_category(
            member_access_diagnostics(
                uri_str,
                tree,
                &source,
                &file_symbols,
                index,
                &utf16_index,
                &type_cache,
                &framework_cache,
            ),
            DiagnosticCategory::Members,
            diagnostic_severity,
        ));
        warn_if_slow_diagnostic_phase(uri_str, "members", members_started);
    }
    if diagnostic_severity
        .severity(DiagnosticCategory::TypeCompatibility)
        .is_some()
        && !skip_member_and_type_diagnostics
    {
        let types_started = Instant::now();
        diagnostics.extend(apply_diagnostic_category(
            type_compatibility_diagnostics(
                tree,
                &source,
                &file_symbols,
                index,
                &utf16_index,
                &type_cache,
            ),
            DiagnosticCategory::TypeCompatibility,
            diagnostic_severity,
        ));
        warn_if_slow_diagnostic_phase(uri_str, "type compatibility", types_started);
    }
    diagnostics.extend(apply_diagnostic_category(
        override_signature_diagnostics(&file_symbols, index, &utf16_index),
        DiagnosticCategory::OverrideSignatures,
        diagnostic_severity,
    ));
    diagnostics.extend(apply_diagnostic_category(
        php_version_type_diagnostics(tree, &source, php_version, &utf16_index),
        DiagnosticCategory::PhpVersion,
        diagnostic_severity,
    ));

    warn_if_slow_diagnostic_phase(uri_str, "total", diagnostics_started);
    diagnostics
}

fn member_type_diagnostic_budget_exceeded(
    node: tree_sitter::Node,
    budget: Option<usize>,
) -> Option<usize> {
    let budget = budget?;
    (count_member_type_diagnostic_nodes_with_budget(node, Some(budget)) > budget).then_some(budget)
}

fn partial_analysis_budget_diagnostic(limit: usize) -> Diagnostic {
    Diagnostic {
        range: Range {
            start: Position::new(0, 0),
            end: Position::new(0, 0),
        },
        severity: Some(DiagnosticSeverity::INFORMATION),
        source: Some("php-lsp".to_string()),
        code: Some(NumberOrString::String("partial-analysis".to_string())),
        message: format!(
            "php-lsp skipped member and type diagnostics because this file exceeded the diagnostics budget of {limit} relevant syntax nodes. Set phpLsp.diagnostics.memberTypeNodeBudget higher or to 0 to analyze the whole file."
        ),
        ..Default::default()
    }
}

fn count_member_type_diagnostic_nodes_with_budget(
    node: tree_sitter::Node,
    budget: Option<usize>,
) -> usize {
    let mut count = usize::from(matches!(
        node.kind(),
        "member_access_expression"
            | "member_call_expression"
            | "scoped_call_expression"
            | "scoped_property_access_expression"
            | "class_constant_access_expression"
            | "function_call_expression"
            | "object_creation_expression"
            | "assignment_expression"
            | "return_statement"
    ));

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        count += count_member_type_diagnostic_nodes_with_budget(child, budget);
        if budget.is_some_and(|budget| count > budget) {
            break;
        }
    }
    count
}

pub(in crate::server) fn warn_if_slow_diagnostic_phase(
    uri_str: &str,
    phase: &str,
    started: Instant,
) {
    let elapsed = started.elapsed();
    if elapsed >= Duration::from_millis(DIAGNOSTIC_PHASE_SLOW_WARNING_MS) {
        tracing::warn!(
            "diagnostics {} phase took {} ms for {}",
            phase,
            elapsed.as_millis(),
            uri_str
        );
    }
}

pub(in crate::server) fn semantic_diagnostic_to_lsp(
    diagnostic: SemanticDiagnostic,
    utf16_index: &Utf16LineIndex,
    severity_config: DiagnosticSeverityConfig,
) -> Option<Diagnostic> {
    let category = semantic_diagnostic_category(&diagnostic.kind);
    let severity = severity_config.severity(category)?;
    Some(Diagnostic {
        range: Range {
            start: Position::new(
                diagnostic.range.0,
                utf16_index.byte_col_to_utf16(diagnostic.range.0, diagnostic.range.1),
            ),
            end: Position::new(
                diagnostic.range.2,
                utf16_index.byte_col_to_utf16(diagnostic.range.2, diagnostic.range.3),
            ),
        },
        severity: Some(severity),
        code: Some(NumberOrString::String(
            semantic_diagnostic_code(&diagnostic.kind).to_string(),
        )),
        source: Some("php-lsp".to_string()),
        message: diagnostic.message,
        ..Default::default()
    })
}

pub(in crate::server) fn semantic_diagnostic_category(
    kind: &SemanticDiagnosticKind,
) -> DiagnosticCategory {
    match kind {
        SemanticDiagnosticKind::UnknownClass
        | SemanticDiagnosticKind::UnknownFunction
        | SemanticDiagnosticKind::UnresolvedUse => DiagnosticCategory::UnknownSymbols,
        SemanticDiagnosticKind::ArgumentCountMismatch => DiagnosticCategory::TypeCompatibility,
        SemanticDiagnosticKind::UndefinedVariable => DiagnosticCategory::UnknownSymbols,
        SemanticDiagnosticKind::UnusedImport
        | SemanticDiagnosticKind::UnusedVariable
        | SemanticDiagnosticKind::UnusedParameter => DiagnosticCategory::Unused,
        SemanticDiagnosticKind::DuplicateSymbol => DiagnosticCategory::DuplicateSymbols,
    }
}

pub(in crate::server) fn semantic_diagnostic_code(kind: &SemanticDiagnosticKind) -> &'static str {
    match kind {
        SemanticDiagnosticKind::UnknownClass => "php-lsp.unknownClass",
        SemanticDiagnosticKind::UnknownFunction => "php-lsp.unknownFunction",
        SemanticDiagnosticKind::UnresolvedUse => "php-lsp.unresolvedUse",
        SemanticDiagnosticKind::ArgumentCountMismatch => "php-lsp.argumentCountMismatch",
        SemanticDiagnosticKind::UndefinedVariable => "php-lsp.undefinedVariable",
        SemanticDiagnosticKind::UnusedImport => "php-lsp.unusedImport",
        SemanticDiagnosticKind::UnusedVariable => "php-lsp.unusedVariable",
        SemanticDiagnosticKind::UnusedParameter => "php-lsp.unusedParameter",
        SemanticDiagnosticKind::DuplicateSymbol => "php-lsp.duplicateSymbol",
    }
}

pub(in crate::server) fn apply_diagnostic_category(
    diagnostics: Vec<Diagnostic>,
    category: DiagnosticCategory,
    severity_config: DiagnosticSeverityConfig,
) -> Vec<Diagnostic> {
    let Some(severity) = severity_config.severity(category) else {
        return Vec::new();
    };

    diagnostics
        .into_iter()
        .map(|mut diagnostic| {
            diagnostic.severity = Some(severity);
            if diagnostic.code.is_none() {
                diagnostic.code = Some(NumberOrString::String(category.code().to_string()));
            }
            diagnostic
        })
        .collect()
}

pub(in crate::server) fn diagnostic_at_byte_range(
    range: (u32, u32, u32, u32),
    utf16_index: &Utf16LineIndex,
    message: String,
) -> Diagnostic {
    Diagnostic {
        range: Range {
            start: Position::new(range.0, utf16_index.byte_col_to_utf16(range.0, range.1)),
            end: Position::new(range.2, utf16_index.byte_col_to_utf16(range.2, range.3)),
        },
        severity: Some(DiagnosticSeverity::WARNING),
        source: Some("php-lsp".to_string()),
        message,
        ..Default::default()
    }
}

#[allow(clippy::too_many_arguments)]
pub(in crate::server) fn member_access_diagnostics(
    uri_str: &str,
    tree: &tree_sitter::Tree,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
    type_cache: &RequestTypeCache,
    framework_cache: &crate::framework::FrameworkProviderCache,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    walk_member_access_diagnostics(
        tree,
        tree.root_node(),
        uri_str,
        source,
        file_symbols,
        index,
        utf16_index,
        type_cache,
        framework_cache,
        &mut diagnostics,
    );
    diagnostics
}

#[allow(clippy::too_many_arguments)]
pub(in crate::server) fn walk_member_access_diagnostics(
    tree: &tree_sitter::Tree,
    node: tree_sitter::Node,
    uri_str: &str,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
    type_cache: &RequestTypeCache,
    framework_cache: &crate::framework::FrameworkProviderCache,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if matches!(
        node.kind(),
        "member_access_expression"
            | "member_call_expression"
            | "scoped_call_expression"
            | "scoped_property_access_expression"
            | "class_constant_access_expression"
    ) {
        check_member_access_node(
            tree,
            node,
            uri_str,
            source,
            file_symbols,
            index,
            utf16_index,
            type_cache,
            framework_cache,
            diagnostics,
        );
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_member_access_diagnostics(
            tree,
            child,
            uri_str,
            source,
            file_symbols,
            index,
            utf16_index,
            type_cache,
            framework_cache,
            diagnostics,
        );
    }
}

#[allow(clippy::too_many_arguments)]
pub(in crate::server) fn check_member_access_node(
    tree: &tree_sitter::Tree,
    node: tree_sitter::Node,
    uri_str: &str,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
    type_cache: &RequestTypeCache,
    framework_cache: &crate::framework::FrameworkProviderCache,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if node_inside_anonymous_class_body(node, source) {
        return;
    }

    let Some(name_node) = member_reference_name_node(node) else {
        return;
    };
    if is_magic_class_constant_access(node, name_node, source) {
        return;
    }
    let pos = name_node.start_position();
    let member_type_resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
        type_cache.cached_string(
            (0, 0, 0, 0),
            "member-type",
            format!("{class_fqn}::{member_name}"),
            || {
                resolve_diagnostic_member_type(
                    index,
                    class_fqn,
                    member_name,
                    uri_str,
                    file_symbols,
                    source,
                )
            },
        )
    };
    let callable_param_resolver = |ctx: CallableParameterContext<'_>| {
        resolve_callable_parameter_type_from_index(index, file_symbols, ctx)
    };
    let function_type_resolver =
        |function_name: &str| resolve_function_type_from_index(index, function_name);
    let Some(sym_at_pos) = symbol_at_position_with_request_cache(
        type_cache,
        tree,
        source,
        pos.row as u32,
        pos.column as u32,
        file_symbols,
        "diagnostic-member-access",
        Some(&member_type_resolver),
        Some(&callable_param_resolver),
        Some(&function_type_resolver),
    ) else {
        return;
    };

    if !matches!(
        sym_at_pos.ref_kind,
        RefKind::MethodCall
            | RefKind::PropertyAccess
            | RefKind::StaticPropertyAccess
            | RefKind::ClassConstant
    ) || !sym_at_pos.fqn.contains("::")
    {
        return;
    }

    let Some(resolved) = resolve_member_for_ref_kind(index, &sym_at_pos) else {
        if is_phpunit_testcase_helper_call(&sym_at_pos, file_symbols, index)
            || is_phpunit_test_double_api_call(
                tree,
                source,
                file_symbols,
                index,
                type_cache,
                &sym_at_pos,
            )
            || is_missing_parent_constructor_call(&sym_at_pos)
            || is_enum_builtin_method_call(index, &sym_at_pos)
            || is_dynamic_member_access(
                index,
                file_symbols,
                uri_str,
                source,
                &sym_at_pos,
                framework_cache,
            )
        {
            return;
        }

        diagnostics.push(member_diagnostic(
            &sym_at_pos,
            utf16_index,
            unknown_member_message(&sym_at_pos),
        ));
        return;
    };

    if let Some(message) = static_instance_misuse_message(node.kind(), &sym_at_pos, &resolved) {
        diagnostics.push(member_diagnostic(&sym_at_pos, utf16_index, message));
    }

    if let Some(message) =
        visibility_violation_message(index, &resolved, file_symbols, sym_at_pos.range)
    {
        diagnostics.push(member_diagnostic(&sym_at_pos, utf16_index, message));
    }
}

pub(in crate::server) fn member_reference_name_node(
    node: tree_sitter::Node,
) -> Option<tree_sitter::Node> {
    node.child_by_field_name("name").or_else(|| {
        if node.kind() == "class_constant_access_expression" {
            node.named_child(1)
        } else {
            None
        }
    })
}

pub(in crate::server) fn is_phpunit_testcase_helper_call(
    sym_at_pos: &SymbolAtPosition,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
) -> bool {
    if sym_at_pos.ref_kind != RefKind::MethodCall
        || !file_is_phpunit_test_context(file_symbols, index)
        || !phpunit_testcase_helper_method(&sym_at_pos.name)
    {
        return false;
    }

    matches!(
        sym_at_pos.object_expr.as_deref(),
        Some("$this" | "self" | "static" | "parent")
    )
}

pub(in crate::server) fn file_is_phpunit_test_context(
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
) -> bool {
    file_symbols.symbols.iter().any(|sym| {
        matches!(sym.kind, php_lsp_types::PhpSymbolKind::Class)
            && sym.extends.iter().any(|parent| {
                is_phpunit_testcase_like_fqn(parent)
                    || class_extends_or_implements(
                        index,
                        parent.trim_start_matches('\\'),
                        "PHPUnit\\Framework\\TestCase",
                        &mut Vec::new(),
                    )
            })
    }) || file_symbols.symbols.iter().any(|sym| {
        matches!(sym.kind, php_lsp_types::PhpSymbolKind::Trait)
            && (sym.name.ends_with("TestTrait")
                || sym
                    .fqn
                    .split('\\')
                    .any(|segment| segment.eq_ignore_ascii_case("Tests")))
    })
}

pub(in crate::server) fn is_phpunit_testcase_like_fqn(fqn: &str) -> bool {
    let fqn = fqn.trim_start_matches('\\');
    fqn == "PHPUnit\\Framework\\TestCase" || fqn.ends_with("\\TestCase")
}

pub(in crate::server) fn phpunit_testcase_helper_method(member_name: &str) -> bool {
    member_name.starts_with("assert")
        || matches!(
            member_name,
            "fail"
                | "markTestIncomplete"
                | "markTestSkipped"
                | "setUp"
                | "tearDown"
                | "createMock"
                | "createConfiguredMock"
                | "createPartialMock"
                | "createStub"
                | "createStubForIntersectionOfInterfaces"
                | "createMockForIntersectionOfInterfaces"
                | "once"
                | "never"
                | "any"
                | "exactly"
                | "atLeast"
                | "atLeastOnce"
                | "atMost"
                | "callback"
                | "anything"
                | "equalTo"
                | "identicalTo"
                | "isInstanceOf"
                | "isType"
                | "stringContains"
                | "logicalAnd"
                | "logicalOr"
                | "logicalNot"
                | "containsEqual"
                | "containsIdentical"
        )
}

pub(in crate::server) fn is_phpunit_test_double_api_call(
    tree: &tree_sitter::Tree,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    type_cache: &RequestTypeCache,
    sym_at_pos: &SymbolAtPosition,
) -> bool {
    if sym_at_pos.ref_kind != RefKind::MethodCall
        || !phpunit_test_double_api_method(&sym_at_pos.name)
    {
        return false;
    }

    let Some(prop_name) = sym_at_pos
        .object_expr
        .as_deref()
        .and_then(|object_expr| object_expr.strip_prefix("$this->"))
        .filter(|prop_name| !prop_name.contains("->"))
    else {
        return false;
    };

    let member_type_resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
        phpunit_testcase_factory_return_type(member_name)
            .map(str::to_string)
            .or_else(|| {
                type_cache.cached_string(
                    (0, 0, 0, 0),
                    "member-type",
                    format!("{class_fqn}::{member_name}"),
                    || resolve_member_type_from_index(index, class_fqn, member_name),
                )
            })
    };

    infer_property_type_from_assignments(
        tree,
        source,
        prop_name,
        file_symbols,
        Some(&member_type_resolver),
    )
    .into_iter()
    .any(|class_fqn| {
        phpunit_test_double_type_has_method(&class_fqn, &sym_at_pos.name)
            || resolve_member_on_class_for_ref_kind(
                index,
                &class_fqn,
                &sym_at_pos.name,
                sym_at_pos.ref_kind,
                None,
            )
            .is_some()
    })
}

pub(in crate::server) fn phpunit_testcase_factory_return_type(
    member_name: &str,
) -> Option<&'static str> {
    match member_name {
        "createMock"
        | "createConfiguredMock"
        | "createPartialMock"
        | "createMockForIntersectionOfInterfaces" => {
            Some("PHPUnit\\Framework\\MockObject\\MockObject")
        }
        "createStub" | "createStubForIntersectionOfInterfaces" => {
            Some("PHPUnit\\Framework\\MockObject\\Stub")
        }
        _ => None,
    }
}

fn resolve_diagnostic_member_type(
    index: &WorkspaceIndex,
    class_fqn: &str,
    member_name: &str,
    uri_str: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    source: &str,
) -> Option<String> {
    if class_fqn.is_empty() {
        return resolve_member_type_from_index(index, class_fqn, member_name);
    }

    let virtual_property_type = || {
        framework_virtual_member_type_fqn(
            index,
            class_fqn,
            member_name,
            Some(uri_str),
            Some(file_symbols),
            Some(source),
        )
        .or_else(|| phpdoc_virtual_property_type_fqn(index, class_fqn, member_name))
    };

    if member_name.starts_with('$') {
        resolve_declared_property_type_from_index(index, class_fqn, member_name)
            .or_else(virtual_property_type)
            .or_else(|| resolve_member_type_from_index(index, class_fqn, member_name))
    } else {
        resolve_member_type_from_index(index, class_fqn, member_name).or_else(virtual_property_type)
    }
}

fn resolve_declared_property_type_from_index(
    index: &WorkspaceIndex,
    class_fqn: &str,
    member_name: &str,
) -> Option<String> {
    let bare_name = member_name.trim_start_matches('$');
    index
        .get_members(class_fqn)
        .into_iter()
        .find(|sym| {
            sym.kind == php_lsp_types::PhpSymbolKind::Property
                && sym.parent_fqn.as_deref() == Some(class_fqn)
                && (sym.name == member_name || sym.name == bare_name)
        })
        .or_else(|| {
            index.get_members(class_fqn).into_iter().find(|sym| {
                sym.kind == php_lsp_types::PhpSymbolKind::Property
                    && (sym.name == member_name || sym.name == bare_name)
            })
        })
        .and_then(|sym| symbol_return_type_text_from_index(index, class_fqn, &sym))
}

fn resolve_function_type_from_index(
    index: &WorkspaceIndex,
    function_fqn: &str,
) -> Option<ResolvedFunctionType> {
    let sym = index.resolve_fqn(function_fqn).or_else(|| {
        function_fqn
            .rsplit_once('\\')
            .and_then(|(_, short_name)| index.resolve_fqn(short_name))
    })?;
    if sym.kind != php_lsp_types::PhpSymbolKind::Function {
        return None;
    }
    symbol_return_type_text_from_index(index, &sym.fqn, &sym)
        .map(|type_text| ResolvedFunctionType::with_signature(type_text, sym.signature.clone()))
}

pub(in crate::server) fn phpunit_test_double_api_method(member_name: &str) -> bool {
    matches!(
        member_name,
        "expects"
            | "method"
            | "with"
            | "withAnyParameters"
            | "withConsecutive"
            | "will"
            | "willReturn"
            | "willReturnArgument"
            | "willReturnCallback"
            | "willReturnMap"
            | "willReturnOnConsecutiveCalls"
            | "willReturnReference"
            | "willReturnSelf"
            | "willThrowException"
    )
}

pub(in crate::server) fn phpunit_test_double_type_has_method(
    class_fqn: &str,
    member_name: &str,
) -> bool {
    is_phpunit_test_double_type(class_fqn) && phpunit_test_double_api_method(member_name)
}

pub(in crate::server) fn is_dynamic_member_access(
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    uri_str: &str,
    source: &str,
    sym_at_pos: &SymbolAtPosition,
    framework_cache: &crate::framework::FrameworkProviderCache,
) -> bool {
    let Some((class_fqn, member_name)) = sym_at_pos.fqn.rsplit_once("::") else {
        return false;
    };

    if is_generic_object_type_name(class_fqn) {
        return true;
    }

    if class_has_unindexed_ancestor(index, class_fqn, &mut Vec::new()) {
        return true;
    }

    let framework_query = crate::framework::VirtualMemberQuery::from_ref_kind(
        class_fqn,
        member_name,
        sym_at_pos.ref_kind,
    );
    let framework_ctx = crate::framework::FrameworkProviderContext::new(index)
        .with_source_uri(Some(uri_str))
        .with_workspace(None, None)
        .with_file(Some(file_symbols), Some(source))
        .with_relevant_files(&[]);
    let framework_registry = crate::framework::default_framework_provider_registry();

    if sym_at_pos.ref_kind == RefKind::MethodCall {
        return framework_query.is_some_and(|query| {
            framework_cache.has_virtual_member(&framework_registry, &framework_ctx, &query)
        }) || is_unindexed_imported_type(index, file_symbols, class_fqn);
    }

    if sym_at_pos.ref_kind != RefKind::PropertyAccess {
        return false;
    }

    if fqn_matches(class_fqn, "stdClass")
        || class_has_magic_get(index, class_fqn)
        || is_phpunit_test_double_type(class_fqn)
    {
        return true;
    }

    if framework_query.is_some_and(|query| {
        framework_cache.has_virtual_member(&framework_registry, &framework_ctx, &query)
    }) {
        return true;
    }

    let bare_member_name = member_name.strip_prefix('$').unwrap_or(member_name);
    matches!(bare_member_name, "name" | "value")
        && index
            .types
            .get(class_fqn.trim_start_matches('\\'))
            .is_some_and(|sym| sym.kind == php_lsp_types::PhpSymbolKind::Enum)
}

pub(in crate::server) fn class_has_magic_get(index: &WorkspaceIndex, class_fqn: &str) -> bool {
    index
        .resolve_fqn(&format!("{}::__get", class_fqn.trim_start_matches('\\')))
        .is_some_and(|symbol| symbol.kind == php_lsp_types::PhpSymbolKind::Method)
}

pub(in crate::server) fn is_missing_parent_constructor_call(sym_at_pos: &SymbolAtPosition) -> bool {
    sym_at_pos.ref_kind == RefKind::MethodCall
        && sym_at_pos.name == "__construct"
        && sym_at_pos.object_expr.as_deref() == Some("parent")
}

pub(in crate::server) fn is_enum_builtin_method_call(
    index: &WorkspaceIndex,
    sym_at_pos: &SymbolAtPosition,
) -> bool {
    if sym_at_pos.ref_kind != RefKind::MethodCall
        || !matches!(sym_at_pos.name.as_str(), "cases" | "from" | "tryFrom")
    {
        return false;
    }

    let Some((class_fqn, _)) = sym_at_pos.fqn.rsplit_once("::") else {
        return false;
    };

    index
        .types
        .get(class_fqn.trim_start_matches('\\'))
        .is_some_and(|sym| sym.kind == php_lsp_types::PhpSymbolKind::Enum)
}

pub(in crate::server) fn class_has_unindexed_ancestor(
    index: &WorkspaceIndex,
    class_fqn: &str,
    visited: &mut Vec<String>,
) -> bool {
    let class_fqn = class_fqn.trim_start_matches('\\');
    if visited
        .iter()
        .any(|visited| fqn_matches(visited, class_fqn))
    {
        return false;
    }
    visited.push(class_fqn.to_string());

    let Some(class_sym) = index
        .types
        .get(class_fqn)
        .map(|entry| entry.value().clone())
    else {
        return false;
    };

    class_sym
        .extends
        .iter()
        .chain(class_sym.implements.iter())
        .any(|parent| {
            let parent = parent.trim_start_matches('\\');
            if parent.is_empty() || fqn_matches(parent, class_fqn) {
                return false;
            }
            !index.types.contains_key(parent)
                || class_has_unindexed_ancestor(index, parent, visited)
        })
}

pub(in crate::server) fn is_unindexed_imported_type(
    index: &WorkspaceIndex,
    file_symbols: &php_lsp_types::FileSymbols,
    class_fqn: &str,
) -> bool {
    let normalized = class_fqn.trim_start_matches('\\');
    if index.types.contains_key(normalized) {
        return false;
    }

    file_symbols.use_statements.iter().any(|use_statement| {
        matches!(use_statement.kind, php_lsp_types::UseKind::Class)
            && use_statement.fqn.trim_start_matches('\\') == normalized
    })
}

pub(in crate::server) fn is_generic_object_type_name(class_fqn: &str) -> bool {
    class_fqn
        .trim_start_matches('\\')
        .rsplit('\\')
        .next()
        .is_some_and(|name| name.eq_ignore_ascii_case("object"))
}

pub(in crate::server) fn is_phpunit_test_double_type(class_fqn: &str) -> bool {
    matches!(
        class_fqn.trim_start_matches('\\'),
        "PHPUnit\\Framework\\MockObject\\MockObject"
            | "PHPUnit\\Framework\\MockObject\\Stub"
            | "PHPUnit\\Framework\\MockObject\\MockBuilder"
            | "PHPUnit\\Framework\\MockObject\\InvocationMocker"
    )
}

pub(in crate::server) fn is_magic_class_constant_access(
    node: tree_sitter::Node,
    name_node: tree_sitter::Node,
    source: &str,
) -> bool {
    node.kind() == "class_constant_access_expression"
        && source[name_node.byte_range()].eq_ignore_ascii_case("class")
}

pub(in crate::server) fn member_diagnostic(
    sym_at_pos: &SymbolAtPosition,
    utf16_index: &Utf16LineIndex,
    message: String,
) -> Diagnostic {
    diagnostic_at_byte_range(sym_at_pos.range, utf16_index, message)
}

pub(in crate::server) fn symbol_kind_matches_ref_kind(
    sym: &php_lsp_types::SymbolInfo,
    ref_kind: RefKind,
) -> bool {
    matches!(
        (ref_kind, sym.kind),
        (RefKind::MethodCall, php_lsp_types::PhpSymbolKind::Method)
            | (
                RefKind::PropertyAccess,
                php_lsp_types::PhpSymbolKind::Property
            )
            | (
                RefKind::StaticPropertyAccess,
                php_lsp_types::PhpSymbolKind::Property
            )
            | (
                RefKind::ClassConstant,
                php_lsp_types::PhpSymbolKind::ClassConstant
            )
            | (
                RefKind::ClassConstant,
                php_lsp_types::PhpSymbolKind::EnumCase
            )
    )
}

pub(in crate::server) fn resolve_member_for_ref_kind(
    index: &WorkspaceIndex,
    sym_at_pos: &SymbolAtPosition,
) -> Option<std::sync::Arc<php_lsp_types::SymbolInfo>> {
    if let Some(sym) = index.resolve_fqn(&sym_at_pos.fqn) {
        if symbol_kind_matches_ref_kind(&sym, sym_at_pos.ref_kind) {
            return Some(sym);
        }
    }

    let (class_fqn, member_name) = sym_at_pos.fqn.rsplit_once("::")?;
    resolve_member_on_class_for_ref_kind(
        index,
        class_fqn,
        member_name,
        sym_at_pos.ref_kind,
        Some(&sym_at_pos.fqn),
    )
}

pub(in crate::server) fn resolve_member_on_class_for_ref_kind(
    index: &WorkspaceIndex,
    class_fqn: &str,
    member_name: &str,
    ref_kind: RefKind,
    exact_fqn: Option<&str>,
) -> Option<std::sync::Arc<php_lsp_types::SymbolInfo>> {
    let bare_name = member_name.strip_prefix('$').unwrap_or(member_name);
    index.get_members(class_fqn).into_iter().find(|sym| {
        symbol_kind_matches_ref_kind(sym, ref_kind)
            && (exact_fqn.is_some_and(|fqn| sym.fqn == fqn)
                || sym.name == member_name
                || sym.name == bare_name)
    })
}

pub(in crate::server) fn unknown_member_message(sym_at_pos: &SymbolAtPosition) -> String {
    match sym_at_pos.ref_kind {
        RefKind::MethodCall => format!("Unknown method: {}", sym_at_pos.fqn),
        RefKind::PropertyAccess | RefKind::StaticPropertyAccess => {
            format!("Unknown property: {}", sym_at_pos.fqn)
        }
        RefKind::ClassConstant => format!("Unknown class constant: {}", sym_at_pos.fqn),
        _ => format!("Unknown member: {}", sym_at_pos.fqn),
    }
}

pub(crate) fn lazy_resolvable_diagnostic_fqn(message: &str) -> Option<String> {
    for prefix in [
        "Unresolved use statement: ",
        "Unknown class: ",
        "Unknown method: ",
        "Unknown class constant: ",
    ] {
        if let Some(fqn) = message.strip_prefix(prefix) {
            let fqn = fqn.trim();
            if fqn.contains('\\') || fqn.contains("::") {
                return Some(fqn.to_string());
            }
        }
    }

    None
}

pub(in crate::server) fn static_instance_misuse_message(
    node_kind: &str,
    sym_at_pos: &SymbolAtPosition,
    sym: &php_lsp_types::SymbolInfo,
) -> Option<String> {
    match sym.kind {
        php_lsp_types::PhpSymbolKind::Method => match (node_kind, sym.modifiers.is_static) {
            ("member_call_expression", true)
                if sym_at_pos.object_expr.as_deref() == Some("$this") =>
            {
                None
            }
            ("member_call_expression", true) => Some(format!(
                "Static method called as instance method: {}",
                sym.fqn
            )),
            ("scoped_call_expression", false)
                if matches!(
                    sym_at_pos.object_expr.as_deref(),
                    Some("self" | "static" | "parent")
                ) =>
            {
                None
            }
            ("scoped_call_expression", false) => {
                Some(format!("Instance method called statically: {}", sym.fqn))
            }
            _ => None,
        },
        php_lsp_types::PhpSymbolKind::Property => match (node_kind, sym.modifiers.is_static) {
            ("member_access_expression", true) => Some(format!(
                "Static property accessed as instance property: {}",
                sym.fqn
            )),
            ("scoped_property_access_expression", false) => Some(format!(
                "Instance property accessed statically: {}",
                sym.fqn
            )),
            _ => None,
        },
        _ => None,
    }
}

pub(in crate::server) fn visibility_violation_message(
    index: &WorkspaceIndex,
    sym: &php_lsp_types::SymbolInfo,
    file_symbols: &php_lsp_types::FileSymbols,
    access_range: (u32, u32, u32, u32),
) -> Option<String> {
    let declaring_class = sym.parent_fqn.as_deref()?;
    match sym.visibility {
        php_lsp_types::Visibility::Public => None,
        php_lsp_types::Visibility::Private => {
            let current_class = current_class_fqn_at_range(file_symbols, access_range);
            let accessible = current_class.as_deref().is_some_and(|current| {
                fqn_matches(current, declaring_class)
                    || class_uses_trait(index, current, declaring_class, &mut Vec::new())
            });
            (!accessible).then(|| format!("Private member is not accessible here: {}", sym.fqn))
        }
        php_lsp_types::Visibility::Protected => {
            let current_class = current_class_fqn_at_range(file_symbols, access_range);
            let accessible = current_class.as_deref().is_some_and(|current| {
                class_can_access_protected_member(index, current, declaring_class)
            });
            (!accessible).then(|| format!("Protected member is not accessible here: {}", sym.fqn))
        }
    }
}

pub(in crate::server) fn current_class_fqn_at_range(
    file_symbols: &php_lsp_types::FileSymbols,
    range: (u32, u32, u32, u32),
) -> Option<String> {
    current_class_symbol_at_range(file_symbols, range).map(|sym| sym.fqn.clone())
}

pub(in crate::server) fn current_class_symbol_at_range(
    file_symbols: &php_lsp_types::FileSymbols,
    range: (u32, u32, u32, u32),
) -> Option<&php_lsp_types::SymbolInfo> {
    let mut current = None;
    for sym in file_symbols.symbols.iter().filter(|sym| {
        matches!(
            sym.kind,
            php_lsp_types::PhpSymbolKind::Class
                | php_lsp_types::PhpSymbolKind::Interface
                | php_lsp_types::PhpSymbolKind::Trait
                | php_lsp_types::PhpSymbolKind::Enum
        ) && byte_range_contains(sym.range, range)
    }) {
        if current.is_none_or(|candidate: &php_lsp_types::SymbolInfo| {
            byte_range_contains(candidate.range, sym.range)
        }) {
            current = Some(sym);
        }
    }
    current
}

pub(in crate::server) fn class_can_access_protected_member(
    index: &WorkspaceIndex,
    current_class: &str,
    declaring_class: &str,
) -> bool {
    if fqn_matches(current_class, declaring_class) {
        return true;
    }
    class_extends_or_implements(index, current_class, declaring_class, &mut Vec::new())
        || class_or_ancestor_uses_trait(index, current_class, declaring_class, &mut Vec::new())
}

pub(in crate::server) fn class_extends_or_implements(
    index: &WorkspaceIndex,
    current_class: &str,
    target_class: &str,
    visited: &mut Vec<String>,
) -> bool {
    let current_class = current_class.trim_start_matches('\\');
    let target_class = target_class.trim_start_matches('\\');
    if visited
        .iter()
        .any(|visited| fqn_matches(visited, current_class))
    {
        return false;
    }
    visited.push(current_class.to_string());

    let Some(class_sym) = index
        .types
        .get(current_class)
        .map(|entry| entry.value().clone())
    else {
        return false;
    };

    class_sym
        .extends
        .iter()
        .chain(class_sym.implements.iter())
        .any(|parent| {
            fqn_matches(parent, target_class)
                || class_extends_or_implements(index, parent, target_class, visited)
        })
}

pub(in crate::server) fn class_or_ancestor_uses_trait(
    index: &WorkspaceIndex,
    current_class: &str,
    target_trait: &str,
    visited: &mut Vec<String>,
) -> bool {
    let current_class = current_class.trim_start_matches('\\');
    if visited
        .iter()
        .any(|visited| fqn_matches(visited, current_class))
    {
        return false;
    }
    visited.push(current_class.to_string());

    if class_uses_trait(index, current_class, target_trait, &mut Vec::new()) {
        return true;
    }

    let Some(class_sym) = index
        .types
        .get(current_class)
        .map(|entry| entry.value().clone())
    else {
        return false;
    };

    class_sym
        .extends
        .iter()
        .any(|parent| class_or_ancestor_uses_trait(index, parent, target_trait, visited))
}

pub(in crate::server) fn class_uses_trait(
    index: &WorkspaceIndex,
    current_class: &str,
    target_trait: &str,
    visited: &mut Vec<String>,
) -> bool {
    let current_class = current_class.trim_start_matches('\\');
    if visited
        .iter()
        .any(|visited| fqn_matches(visited, current_class))
    {
        return false;
    }
    visited.push(current_class.to_string());

    let Some(class_sym) = index
        .types
        .get(current_class)
        .map(|entry| entry.value().clone())
    else {
        return false;
    };

    class_sym.traits.iter().any(|used_trait| {
        fqn_matches(used_trait, target_trait)
            || class_uses_trait(index, used_trait, target_trait, visited)
    })
}

pub(in crate::server) fn fqn_matches(left: &str, right: &str) -> bool {
    left.trim_start_matches('\\') == right.trim_start_matches('\\')
}

pub(in crate::server) fn byte_range_contains(
    outer: (u32, u32, u32, u32),
    inner: (u32, u32, u32, u32),
) -> bool {
    (inner.0 > outer.0 || (inner.0 == outer.0 && inner.1 >= outer.1))
        && (inner.2 < outer.2 || (inner.2 == outer.2 && inner.3 <= outer.3))
}

pub(in crate::server) fn node_inside_anonymous_class_body(
    node: tree_sitter::Node,
    source: &str,
) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == "object_creation_expression" {
            let text = &source[parent.byte_range()];
            if text.trim_start().starts_with("new class") {
                return text.find('{').is_some_and(|body_start| {
                    node.start_byte() > parent.start_byte().saturating_add(body_start)
                });
            }
        }
        current = parent.parent();
    }
    false
}

#[derive(Debug, Clone)]
pub(in crate::server) struct InferredExprType {
    pub(in crate::server) display: String,
    pub(in crate::server) comparable: String,
    pub(in crate::server) range: (u32, u32, u32, u32),
}

pub(in crate::server) fn type_compatibility_diagnostics(
    tree: &tree_sitter::Tree,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
    type_cache: &RequestTypeCache,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    walk_type_compatibility_diagnostics(
        tree,
        tree.root_node(),
        source,
        file_symbols,
        index,
        utf16_index,
        type_cache,
        &mut diagnostics,
    );
    diagnostics
}

#[allow(clippy::too_many_arguments)]
pub(in crate::server) fn walk_type_compatibility_diagnostics(
    tree: &tree_sitter::Tree,
    node: tree_sitter::Node,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
    type_cache: &RequestTypeCache,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match node.kind() {
        "function_call_expression" => check_function_call_type_compatibility(
            tree,
            node,
            source,
            file_symbols,
            index,
            utf16_index,
            type_cache,
            diagnostics,
        ),
        "member_call_expression" | "scoped_call_expression" => {
            check_member_call_type_compatibility(
                tree,
                node,
                source,
                file_symbols,
                index,
                utf16_index,
                type_cache,
                diagnostics,
            )
        }
        "object_creation_expression" => check_constructor_type_compatibility(
            tree,
            node,
            source,
            file_symbols,
            index,
            utf16_index,
            type_cache,
            diagnostics,
        ),
        "return_statement" => check_return_type_compatibility(
            node,
            source,
            file_symbols,
            index,
            utf16_index,
            type_cache,
            diagnostics,
        ),
        "assignment_expression" => check_property_assignment_type_compatibility(
            tree,
            node,
            source,
            file_symbols,
            index,
            utf16_index,
            type_cache,
            diagnostics,
        ),
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_type_compatibility_diagnostics(
            tree,
            child,
            source,
            file_symbols,
            index,
            utf16_index,
            type_cache,
            diagnostics,
        );
    }
}

#[allow(clippy::too_many_arguments)]
pub(in crate::server) fn check_function_call_type_compatibility(
    tree: &tree_sitter::Tree,
    node: tree_sitter::Node,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
    type_cache: &RequestTypeCache,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(name_node) = node
        .child_by_field_name("function")
        .or_else(|| node.named_child(0))
    else {
        return;
    };
    let Some((_, sym)) = resolve_reference_symbol_at_node_cached(
        tree,
        source,
        name_node,
        file_symbols,
        index,
        type_cache,
    ) else {
        return;
    };

    if sym.kind == php_lsp_types::PhpSymbolKind::Function {
        check_call_argument_types(
            node,
            &sym,
            source,
            file_symbols,
            index,
            utf16_index,
            type_cache,
            diagnostics,
        );
    }
}

#[allow(clippy::too_many_arguments)]
pub(in crate::server) fn check_member_call_type_compatibility(
    tree: &tree_sitter::Tree,
    node: tree_sitter::Node,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
    type_cache: &RequestTypeCache,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(name_node) = member_reference_name_node(node) else {
        return;
    };
    let Some((_, sym)) = resolve_reference_symbol_at_node_cached(
        tree,
        source,
        name_node,
        file_symbols,
        index,
        type_cache,
    ) else {
        return;
    };

    if sym.kind == php_lsp_types::PhpSymbolKind::Method {
        check_call_argument_types(
            node,
            &sym,
            source,
            file_symbols,
            index,
            utf16_index,
            type_cache,
            diagnostics,
        );
    }
}

#[allow(clippy::too_many_arguments)]
pub(in crate::server) fn check_constructor_type_compatibility(
    tree: &tree_sitter::Tree,
    node: tree_sitter::Node,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
    type_cache: &RequestTypeCache,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(name_node) = object_creation_class_node(node) else {
        return;
    };
    let Some((_, sym)) = resolve_reference_symbol_at_node_cached(
        tree,
        source,
        name_node,
        file_symbols,
        index,
        type_cache,
    ) else {
        return;
    };

    if sym.kind == php_lsp_types::PhpSymbolKind::Method && sym.name == "__construct" {
        check_call_argument_types(
            node,
            &sym,
            source,
            file_symbols,
            index,
            utf16_index,
            type_cache,
            diagnostics,
        );
    }
}

#[allow(clippy::too_many_arguments)]
pub(in crate::server) fn check_call_argument_types(
    call_node: tree_sitter::Node,
    callable: &php_lsp_types::SymbolInfo,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
    type_cache: &RequestTypeCache,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(signature) = callable.signature.as_ref() else {
        return;
    };

    let callable_file_symbols = index.file_symbols.get(&callable.uri);
    let expected_file_symbols = callable_file_symbols
        .as_ref()
        .map(|entry| entry.value())
        .unwrap_or(file_symbols);

    let arguments = call_arguments(call_node, source);
    for (arg_index, arg) in arguments.into_iter().enumerate() {
        let Some(param) = signature_param_for_call_arg(signature, arg_index, arg.name.as_deref())
        else {
            continue;
        };
        let Some(expected) = param.type_info.as_ref() else {
            continue;
        };
        let Some(actual) =
            infer_expression_type_cached(arg.value_node, source, file_symbols, type_cache)
        else {
            continue;
        };

        let substituted_expected =
            type_info_with_callable_template_bounds(expected, callable, index);
        let expected = substituted_expected.as_ref().unwrap_or(expected);
        if !type_info_accepts_inferred_type(expected, &actual, expected_file_symbols, index) {
            diagnostics.push(diagnostic_at_byte_range(
                actual.range,
                utf16_index,
                format!(
                    "Type mismatch for {} argument ${}: expected {}, got {}",
                    callable.fqn, param.name, expected, actual.display
                ),
            ));
        }
    }
}

pub(in crate::server) fn check_return_type_compatibility(
    node: tree_sitter::Node,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
    type_cache: &RequestTypeCache,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if node_inside_anonymous_class_body(node, source) {
        return;
    }

    let Some(expr_node) = return_expression_node(node) else {
        return;
    };
    let Some(callable) = containing_callable_symbol(file_symbols, node_range_node(node)) else {
        return;
    };
    let Some(signature) = callable.signature.as_ref() else {
        return;
    };
    let Some(expected) = signature.return_type.as_ref() else {
        return;
    };
    if matches!(
        expected,
        php_lsp_types::TypeInfo::Void | php_lsp_types::TypeInfo::Mixed
    ) {
        return;
    }
    let Some(actual) = infer_expression_type_cached(expr_node, source, file_symbols, type_cache)
    else {
        return;
    };

    if !type_info_accepts_inferred_type(expected, &actual, file_symbols, index) {
        diagnostics.push(diagnostic_at_byte_range(
            actual.range,
            utf16_index,
            format!(
                "Return type mismatch in {}: expected {}, got {}",
                callable.fqn, expected, actual.display
            ),
        ));
    }
}

#[allow(clippy::too_many_arguments)]
pub(in crate::server) fn check_property_assignment_type_compatibility(
    tree: &tree_sitter::Tree,
    node: tree_sitter::Node,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
    type_cache: &RequestTypeCache,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(left_node) = node
        .child_by_field_name("left")
        .or_else(|| node.named_child(0))
    else {
        return;
    };
    if !matches!(
        left_node.kind(),
        "member_access_expression" | "scoped_property_access_expression"
    ) {
        return;
    }
    let Some(right_node) = node
        .child_by_field_name("right")
        .or_else(|| node.named_child(1))
    else {
        return;
    };
    let Some(name_node) = member_reference_name_node(left_node) else {
        return;
    };
    let Some((_, property)) = resolve_reference_symbol_at_node_cached(
        tree,
        source,
        name_node,
        file_symbols,
        index,
        type_cache,
    ) else {
        return;
    };

    if property.kind != php_lsp_types::PhpSymbolKind::Property {
        return;
    }
    let Some(expected) = property
        .signature
        .as_ref()
        .and_then(|signature| signature.return_type.as_ref())
    else {
        return;
    };
    let Some(actual) = infer_expression_type_cached(right_node, source, file_symbols, type_cache)
    else {
        return;
    };

    let property_file_symbols = index.file_symbols.get(&property.uri);
    let expected_file_symbols = property_file_symbols
        .as_ref()
        .map(|entry| entry.value())
        .unwrap_or(file_symbols);

    if !type_info_accepts_inferred_type(expected, &actual, expected_file_symbols, index) {
        diagnostics.push(diagnostic_at_byte_range(
            actual.range,
            utf16_index,
            format!(
                "Property assignment type mismatch for {}: expected {}, got {}",
                property.fqn, expected, actual.display
            ),
        ));
    }
}

pub(in crate::server) fn resolve_reference_symbol_at_node(
    tree: &tree_sitter::Tree,
    source: &str,
    node: tree_sitter::Node,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
) -> Option<(SymbolAtPosition, Arc<php_lsp_types::SymbolInfo>)> {
    let pos = node.start_position();
    let member_type_resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
        resolve_member_type_from_index(index, class_fqn, member_name)
    };
    let callable_param_resolver =
        |ctx: CallableParameterContext<'_>| -> Option<php_lsp_types::TypeInfo> {
            resolve_callable_parameter_type_from_index(index, file_symbols, ctx)
        };
    let sym_at_pos = symbol_at_position_with_resolvers(
        tree,
        source,
        pos.row as u32,
        pos.column as u32,
        file_symbols,
        Some(&member_type_resolver),
        Some(&callable_param_resolver),
    )?;
    let resolved = resolve_symbol_at_position_from_index(index, &sym_at_pos)?;
    Some((sym_at_pos, resolved))
}

#[allow(clippy::too_many_arguments)]
pub(in crate::server) fn symbol_at_position_with_request_cache(
    type_cache: &RequestTypeCache,
    tree: &tree_sitter::Tree,
    source: &str,
    line: u32,
    byte_col: u32,
    file_symbols: &php_lsp_types::FileSymbols,
    expected_context: &'static str,
    member_type_resolver: Option<MemberTypeResolver<'_>>,
    callable_resolver: Option<CallableParamTypeResolver<'_>>,
    function_resolver: Option<FunctionTypeResolver<'_>>,
) -> Option<SymbolAtPosition> {
    type_cache.cached_symbol(
        line,
        byte_col,
        "symbol-at-position",
        expected_context,
        || {
            symbol_at_position_with_full_resolvers(
                tree,
                source,
                line,
                byte_col,
                file_symbols,
                member_type_resolver,
                callable_resolver,
                function_resolver,
            )
        },
    )
}

pub(in crate::server) fn resolve_reference_symbol_at_node_cached(
    tree: &tree_sitter::Tree,
    source: &str,
    node: tree_sitter::Node,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    type_cache: &RequestTypeCache,
) -> Option<(SymbolAtPosition, Arc<php_lsp_types::SymbolInfo>)> {
    let pos = node.start_position();
    let member_type_resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
        type_cache.cached_string(
            (0, 0, 0, 0),
            "member-type",
            format!("{class_fqn}::{member_name}"),
            || resolve_member_type_from_index(index, class_fqn, member_name),
        )
    };
    let callable_param_resolver =
        |ctx: CallableParameterContext<'_>| -> Option<php_lsp_types::TypeInfo> {
            resolve_callable_parameter_type_from_index(index, file_symbols, ctx)
        };
    let sym_at_pos = symbol_at_position_with_request_cache(
        type_cache,
        tree,
        source,
        pos.row as u32,
        pos.column as u32,
        file_symbols,
        "reference-symbol",
        Some(&member_type_resolver),
        Some(&callable_param_resolver),
        None,
    )?;
    let resolved = resolve_symbol_at_position_from_index(index, &sym_at_pos)?;
    Some((sym_at_pos, resolved))
}

pub(in crate::server) fn resolve_symbol_at_position_from_index(
    index: &WorkspaceIndex,
    sym_at_pos: &SymbolAtPosition,
) -> Option<Arc<php_lsp_types::SymbolInfo>> {
    if let Some(symbol) = index.resolve_fqn(&sym_at_pos.fqn) {
        return Some(symbol);
    }

    if matches!(
        sym_at_pos.ref_kind,
        RefKind::FunctionCall | RefKind::GlobalConstant
    ) {
        if let Some((_, short_name)) = sym_at_pos.fqn.rsplit_once('\\') {
            return index.resolve_fqn(short_name);
        }
    }

    None
}

#[derive(Debug, Clone)]
pub(in crate::server) struct CallArgument<'tree> {
    pub(in crate::server) value_node: tree_sitter::Node<'tree>,
    pub(in crate::server) name: Option<String>,
}

pub(in crate::server) fn call_arguments<'tree>(
    call_node: tree_sitter::Node<'tree>,
    source: &str,
) -> Vec<CallArgument<'tree>> {
    let Some(arguments) = call_node.child_by_field_name("arguments").or_else(|| {
        let mut cursor = call_node.walk();
        let arguments = call_node
            .children(&mut cursor)
            .find(|child| child.kind() == "arguments");
        arguments
    }) else {
        return Vec::new();
    };

    let mut result = Vec::new();
    let mut cursor = arguments.walk();
    for child in arguments.named_children(&mut cursor) {
        if child.kind() == "argument" {
            result.push(CallArgument {
                value_node: argument_value_node(child).unwrap_or(child),
                name: argument_name(child, source),
            });
        }
    }
    result
}

pub(in crate::server) fn argument_value_node(
    argument: tree_sitter::Node,
) -> Option<tree_sitter::Node> {
    argument.child_by_field_name("value").or_else(|| {
        let mut cursor = argument.walk();
        argument.named_children(&mut cursor).last()
    })
}

pub(in crate::server) fn argument_name(
    argument: tree_sitter::Node,
    source: &str,
) -> Option<String> {
    if let Some(name_node) = argument.child_by_field_name("name") {
        return Some(normalize_argument_name(node_text(source, name_node)));
    }

    let text = node_text(source, argument);
    let colon_index = text.find(':')?;
    let value_start = argument_value_node(argument)
        .map(|value| value.start_byte().saturating_sub(argument.start_byte()))
        .unwrap_or(text.len());

    (colon_index < value_start).then(|| normalize_argument_name(&text[..colon_index]))
}

pub(in crate::server) fn normalize_argument_name(name: &str) -> String {
    name.trim()
        .trim_start_matches('$')
        .trim_end_matches(':')
        .trim()
        .to_string()
}

pub(in crate::server) fn signature_param_for_arg(
    signature: &php_lsp_types::Signature,
    arg_index: usize,
) -> Option<&php_lsp_types::ParamInfo> {
    signature
        .params
        .get(arg_index)
        .or_else(|| signature.params.last().filter(|param| param.is_variadic))
}

pub(in crate::server) fn signature_param_for_call_arg<'a>(
    signature: &'a php_lsp_types::Signature,
    arg_index: usize,
    name: Option<&str>,
) -> Option<&'a php_lsp_types::ParamInfo> {
    if let Some(name) = name {
        return signature.params.iter().find(|param| {
            param
                .name
                .trim_start_matches('$')
                .eq_ignore_ascii_case(name)
        });
    }

    signature_param_for_arg(signature, arg_index)
}

pub(in crate::server) fn return_expression_node(
    node: tree_sitter::Node,
) -> Option<tree_sitter::Node> {
    node.child_by_field_name("value")
        .or_else(|| node.named_child(0))
}

pub(in crate::server) fn object_creation_class_node(
    node: tree_sitter::Node,
) -> Option<tree_sitter::Node> {
    let mut cursor = node.walk();
    let class_node = node.named_children(&mut cursor).find(|child| {
        matches!(
            child.kind(),
            "name" | "qualified_name" | "namespace_name" | "relative_scope"
        )
    });
    class_node
}

pub(in crate::server) fn containing_callable_symbol(
    file_symbols: &php_lsp_types::FileSymbols,
    range: (u32, u32, u32, u32),
) -> Option<&php_lsp_types::SymbolInfo> {
    file_symbols
        .symbols
        .iter()
        .filter(|sym| {
            matches!(
                sym.kind,
                php_lsp_types::PhpSymbolKind::Function | php_lsp_types::PhpSymbolKind::Method
            ) && byte_range_contains(sym.range, range)
        })
        .min_by_key(|sym| {
            (
                sym.range.2.saturating_sub(sym.range.0),
                sym.range.3.saturating_sub(sym.range.1),
            )
        })
}

pub(in crate::server) fn infer_expression_type_cached(
    node: tree_sitter::Node,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    type_cache: &RequestTypeCache,
) -> Option<InferredExprType> {
    let normalized = normalized_expression_node(node);
    type_cache.cached_inferred_expr(
        node_range_node(normalized),
        "diagnostic-expression-type",
        normalized.kind(),
        || infer_expression_type(normalized, source, file_symbols),
    )
}

pub(in crate::server) fn infer_expression_type(
    node: tree_sitter::Node,
    source: &str,
    file_symbols: &php_lsp_types::FileSymbols,
) -> Option<InferredExprType> {
    let node = normalized_expression_node(node);
    let raw = source[node.byte_range()].trim();
    let lower = raw.to_ascii_lowercase();
    let kind = node.kind();
    let range = node_range_node(node);

    if kind == "object_creation_expression" {
        let class_node = object_creation_class_node(node)?;
        let class_name = source[class_node.byte_range()].trim();
        let fqn = resolve_class_name_pub(class_name, file_symbols);
        return Some(InferredExprType {
            display: fqn.clone(),
            comparable: fqn,
            range,
        });
    }

    if expression_is_spaceship_operator_expression(node, source) {
        return Some(inferred_builtin_type("int", range));
    }

    if expression_is_boolean_operator_expression(node, source) {
        return Some(inferred_builtin_type("bool", range));
    }

    if kind.contains("conditional") || raw.contains(" ? ") {
        return None;
    }

    if lower == "null" {
        return Some(inferred_builtin_type("null", range));
    }
    if lower == "true" || lower == "false" {
        return Some(inferred_builtin_type(&lower, range));
    }
    if raw.starts_with('"') || raw.starts_with('\'') {
        return Some(InferredExprType {
            display: "string".to_string(),
            comparable: raw.to_string(),
            range,
        });
    }
    if kind.contains("string") {
        return Some(inferred_builtin_type("string", range));
    }
    if kind.contains("array") || raw.starts_with('[') || lower.starts_with("array(") {
        return Some(inferred_builtin_type("array", range));
    }

    let numeric = lower.trim_start_matches(['+', '-']);
    if kind.contains("float") || numeric.parse::<f64>().is_ok() && numeric.contains('.') {
        return Some(inferred_builtin_type("float", range));
    }
    if kind.contains("integer") || numeric.parse::<i64>().is_ok() {
        return Some(InferredExprType {
            display: "int".to_string(),
            comparable: raw.to_string(),
            range,
        });
    }

    None
}

pub(in crate::server) fn normalized_expression_node(
    mut node: tree_sitter::Node,
) -> tree_sitter::Node {
    loop {
        match node.kind() {
            "argument" => {
                let Some(inner) = argument_value_node(node) else {
                    return node;
                };
                node = inner;
            }
            "parenthesized_expression" => {
                let Some(inner) = node.named_child(0) else {
                    return node;
                };
                node = inner;
            }
            _ => return node,
        }
    }
}

fn expression_is_boolean_operator_expression(node: tree_sitter::Node, source: &str) -> bool {
    match node.kind() {
        "unary_op_expression" => source[node.byte_range()].trim_start().starts_with('!'),
        "binary_expression" => {
            binary_root_operator_kind(node, source) == Some(ReturnOperatorKind::Boolean)
        }
        _ => false,
    }
}

fn expression_is_spaceship_operator_expression(node: tree_sitter::Node, source: &str) -> bool {
    node.kind() == "binary_expression"
        && binary_root_operator_kind(node, source) == Some(ReturnOperatorKind::Spaceship)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReturnOperatorKind {
    Boolean,
    Spaceship,
}

fn binary_root_operator_kind(node: tree_sitter::Node, source: &str) -> Option<ReturnOperatorKind> {
    let operator = binary_root_operator_text(node, source)?;
    if operator == "<=>" {
        return Some(ReturnOperatorKind::Spaceship);
    }
    if matches!(
        operator,
        "===" | "!==" | "==" | "!=" | "<>" | "<=" | ">=" | "&&" | "||" | "<" | ">"
    ) || matches!(
        operator.to_ascii_lowercase().as_str(),
        "and" | "or" | "xor" | "instanceof"
    ) {
        return Some(ReturnOperatorKind::Boolean);
    }

    None
}

fn binary_root_operator_text<'a>(node: tree_sitter::Node, source: &'a str) -> Option<&'a str> {
    let left = node
        .child_by_field_name("left")
        .or_else(|| node.named_child(0))?;
    let right = node
        .child_by_field_name("right")
        .or_else(|| node.named_child(1))?;
    if left.end_byte() > right.start_byte() {
        return None;
    }
    source
        .get(left.end_byte()..right.start_byte())
        .map(str::trim)
}

pub(in crate::server) fn inferred_builtin_type(
    name: &str,
    range: (u32, u32, u32, u32),
) -> InferredExprType {
    InferredExprType {
        display: name.to_string(),
        comparable: name.to_string(),
        range,
    }
}

pub(in crate::server) fn type_info_accepts_inferred_type(
    expected: &php_lsp_types::TypeInfo,
    actual: &InferredExprType,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
) -> bool {
    match expected {
        php_lsp_types::TypeInfo::Mixed => true,
        php_lsp_types::TypeInfo::Nullable(inner) => {
            actual.comparable == "null"
                || type_info_accepts_inferred_type(inner, actual, file_symbols, index)
        }
        php_lsp_types::TypeInfo::Union(types) => types.iter().any(|type_info| {
            type_info_accepts_inferred_type(type_info, actual, file_symbols, index)
        }),
        php_lsp_types::TypeInfo::Conditional {
            if_type, else_type, ..
        } => {
            type_info_accepts_inferred_type(if_type, actual, file_symbols, index)
                || type_info_accepts_inferred_type(else_type, actual, file_symbols, index)
        }
        php_lsp_types::TypeInfo::Intersection(_) => true,
        php_lsp_types::TypeInfo::Simple(name) => {
            simple_type_accepts_inferred_type(name, actual, file_symbols, index)
        }
        php_lsp_types::TypeInfo::Generic { base, .. } => {
            simple_type_accepts_inferred_type(base, actual, file_symbols, index)
        }
        php_lsp_types::TypeInfo::ArrayShape(_) => actual.comparable == "array",
        php_lsp_types::TypeInfo::ObjectShape(_) => actual.comparable == "object",
        php_lsp_types::TypeInfo::Callable { .. } => actual.comparable == "callable",
        php_lsp_types::TypeInfo::ClassString(_) => actual.comparable == "string",
        php_lsp_types::TypeInfo::LiteralString(value)
        | php_lsp_types::TypeInfo::LiteralInt(value)
        | php_lsp_types::TypeInfo::LiteralFloat(value) => actual.comparable == value.as_str(),
        php_lsp_types::TypeInfo::LiteralBool(value) => {
            actual.comparable == if *value { "true" } else { "false" }
        }
        php_lsp_types::TypeInfo::LiteralNull => actual.comparable == "null",
        php_lsp_types::TypeInfo::Self_
        | php_lsp_types::TypeInfo::Static_
        | php_lsp_types::TypeInfo::Parent_ => true,
        php_lsp_types::TypeInfo::Void | php_lsp_types::TypeInfo::Never => false,
    }
}

pub(in crate::server) fn type_info_with_callable_template_bounds(
    type_info: &php_lsp_types::TypeInfo,
    callable: &php_lsp_types::SymbolInfo,
    index: &WorkspaceIndex,
) -> Option<php_lsp_types::TypeInfo> {
    let mut template_bounds: Vec<(String, Option<php_lsp_types::TypeInfo>)> = callable
        .templates
        .iter()
        .map(|template| (template.name.clone(), template.bound.clone()))
        .collect();

    if let Some(parent_fqn) = callable.parent_fqn.as_deref() {
        if let Some(parent) = index.resolve_fqn(parent_fqn) {
            template_bounds.extend(
                parent
                    .templates
                    .iter()
                    .map(|template| (template.name.clone(), template.bound.clone())),
            );
        }
    }

    substitute_callable_template_bounds(type_info, &template_bounds)
}

fn substitute_callable_template_bounds(
    type_info: &php_lsp_types::TypeInfo,
    template_bounds: &[(String, Option<php_lsp_types::TypeInfo>)],
) -> Option<php_lsp_types::TypeInfo> {
    if template_bounds.is_empty() {
        return None;
    }

    match type_info {
        php_lsp_types::TypeInfo::Simple(name) => {
            let name = name.trim_start_matches('\\');
            template_bounds
                .iter()
                .find(|(template, _)| template == name)
                .map(|(_, bound)| bound.clone().unwrap_or(php_lsp_types::TypeInfo::Mixed))
        }
        php_lsp_types::TypeInfo::Generic { base, args } => {
            if let Some((_, bound)) = template_bounds
                .iter()
                .find(|(template, _)| template == base)
            {
                return Some(bound.clone().unwrap_or(php_lsp_types::TypeInfo::Mixed));
            }

            let mut changed = false;
            let args = args
                .iter()
                .map(|arg| {
                    substitute_callable_template_bounds(arg, template_bounds).map_or_else(
                        || arg.clone(),
                        |substituted| {
                            changed = true;
                            substituted
                        },
                    )
                })
                .collect();

            changed.then_some(php_lsp_types::TypeInfo::Generic {
                base: base.clone(),
                args,
            })
        }
        php_lsp_types::TypeInfo::ArrayShape(items)
        | php_lsp_types::TypeInfo::ObjectShape(items) => {
            let mut changed = false;
            let items = items
                .iter()
                .map(|item| {
                    substitute_callable_template_bounds(&item.value, template_bounds).map_or_else(
                        || item.clone(),
                        |value| {
                            changed = true;
                            php_lsp_types::ArrayShapeItem {
                                key: item.key.clone(),
                                optional: item.optional,
                                value,
                            }
                        },
                    )
                })
                .collect();

            changed.then_some(match type_info {
                php_lsp_types::TypeInfo::ArrayShape(_) => {
                    php_lsp_types::TypeInfo::ArrayShape(items)
                }
                _ => php_lsp_types::TypeInfo::ObjectShape(items),
            })
        }
        php_lsp_types::TypeInfo::Callable {
            params,
            return_type,
        } => {
            let mut changed = false;
            let params = params
                .iter()
                .map(|param| {
                    substitute_callable_template_bounds(param, template_bounds).map_or_else(
                        || param.clone(),
                        |substituted| {
                            changed = true;
                            substituted
                        },
                    )
                })
                .collect();
            let return_type = return_type.as_ref().map(|return_type| {
                substitute_callable_template_bounds(return_type, template_bounds).map_or_else(
                    || return_type.clone(),
                    |substituted| {
                        changed = true;
                        Box::new(substituted)
                    },
                )
            });

            changed.then_some(php_lsp_types::TypeInfo::Callable {
                params,
                return_type,
            })
        }
        php_lsp_types::TypeInfo::ClassString(inner) => inner.as_ref().and_then(|inner| {
            substitute_callable_template_bounds(inner, template_bounds)
                .map(|inner| php_lsp_types::TypeInfo::ClassString(Some(Box::new(inner))))
        }),
        php_lsp_types::TypeInfo::Conditional {
            subject,
            target,
            if_type,
            else_type,
        } => {
            let mut changed = false;
            let target = substitute_callable_template_bounds(target, template_bounds).map_or_else(
                || target.clone(),
                |substituted| {
                    changed = true;
                    Box::new(substituted)
                },
            );
            let if_type = substitute_callable_template_bounds(if_type, template_bounds)
                .map_or_else(
                    || if_type.clone(),
                    |substituted| {
                        changed = true;
                        Box::new(substituted)
                    },
                );
            let else_type = substitute_callable_template_bounds(else_type, template_bounds)
                .map_or_else(
                    || else_type.clone(),
                    |substituted| {
                        changed = true;
                        Box::new(substituted)
                    },
                );

            changed.then_some(php_lsp_types::TypeInfo::Conditional {
                subject: subject.clone(),
                target,
                if_type,
                else_type,
            })
        }
        php_lsp_types::TypeInfo::Union(types) | php_lsp_types::TypeInfo::Intersection(types) => {
            let mut changed = false;
            let types = types
                .iter()
                .map(|type_info| {
                    substitute_callable_template_bounds(type_info, template_bounds).map_or_else(
                        || type_info.clone(),
                        |substituted| {
                            changed = true;
                            substituted
                        },
                    )
                })
                .collect();

            changed.then_some(match type_info {
                php_lsp_types::TypeInfo::Union(_) => php_lsp_types::TypeInfo::Union(types),
                _ => php_lsp_types::TypeInfo::Intersection(types),
            })
        }
        php_lsp_types::TypeInfo::Nullable(inner) => {
            substitute_callable_template_bounds(inner, template_bounds)
                .map(|inner| php_lsp_types::TypeInfo::Nullable(Box::new(inner)))
        }
        php_lsp_types::TypeInfo::LiteralString(_)
        | php_lsp_types::TypeInfo::LiteralInt(_)
        | php_lsp_types::TypeInfo::LiteralFloat(_)
        | php_lsp_types::TypeInfo::LiteralBool(_)
        | php_lsp_types::TypeInfo::LiteralNull
        | php_lsp_types::TypeInfo::Void
        | php_lsp_types::TypeInfo::Never
        | php_lsp_types::TypeInfo::Mixed
        | php_lsp_types::TypeInfo::Self_
        | php_lsp_types::TypeInfo::Static_
        | php_lsp_types::TypeInfo::Parent_ => None,
    }
}

pub(in crate::server) fn simple_type_accepts_inferred_type(
    expected: &str,
    actual: &InferredExprType,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
) -> bool {
    let expected_lower = expected.trim_start_matches('\\').to_ascii_lowercase();
    let actual_lower = actual
        .comparable
        .trim_start_matches('\\')
        .to_ascii_lowercase();
    let actual_display_lower = actual.display.trim_start_matches('\\').to_ascii_lowercase();

    match expected_lower.as_str() {
        "mixed" => true,
        "string" => {
            actual_lower == "string"
                || actual_display_lower == "string"
                || inferred_string_literal_inner(&actual.comparable).is_some()
        }
        "non-empty-string" => {
            actual_lower == "non-empty-string"
                || inferred_string_literal_inner(&actual.comparable)
                    .is_some_and(|inner| !inner.is_empty())
        }
        "literal-string" => {
            actual_lower == "literal-string"
                || inferred_string_literal_inner(&actual.comparable).is_some()
        }
        "int" => actual_lower == "int" || actual_lower.parse::<i64>().is_ok(),
        "positive-int" => actual_lower.parse::<i64>().is_ok_and(|value| value > 0),
        "negative-int" => actual_lower.parse::<i64>().is_ok_and(|value| value < 0),
        "non-negative-int" => actual_lower.parse::<i64>().is_ok_and(|value| value >= 0),
        "non-positive-int" => actual_lower.parse::<i64>().is_ok_and(|value| value <= 0),
        "non-zero-int" => actual_lower.parse::<i64>().is_ok_and(|value| value != 0),
        "float" => {
            actual_lower == "float" || actual_lower == "int" || actual_lower.parse::<i64>().is_ok()
        }
        "bool" => matches!(actual_lower.as_str(), "bool" | "true" | "false"),
        "false" => actual_lower == "false",
        "true" => actual_lower == "true",
        "null" => actual_lower == "null",
        "array" => actual_lower == "array",
        "iterable" => actual_lower == "array",
        "object" => !is_builtin_comparable_type(&actual_lower),
        "callable" => true,
        "void" | "never" => false,
        _ => {
            let expected_fqn = if expected.starts_with('\\') {
                expected.trim_start_matches('\\').to_string()
            } else {
                resolve_class_name_pub(expected, file_symbols)
            };
            let actual_fqn = actual.comparable.trim_start_matches('\\');
            expected_fqn == actual_fqn
                || class_extends_or_implements(index, actual_fqn, &expected_fqn, &mut Vec::new())
        }
    }
}

pub(in crate::server) fn inferred_string_literal_inner(raw: &str) -> Option<&str> {
    let raw = raw.trim();
    let quote = raw.as_bytes().first().copied()?;
    if quote != b'\'' && quote != b'"' {
        return None;
    }
    if raw.as_bytes().last().copied() != Some(quote) || raw.len() < 2 {
        return None;
    }
    raw.get(1..raw.len() - 1)
}

pub(in crate::server) fn is_builtin_comparable_type(name: &str) -> bool {
    matches!(
        name,
        "array" | "bool" | "false" | "float" | "int" | "null" | "string" | "true"
    )
}

pub(in crate::server) fn node_range_node(node: tree_sitter::Node) -> (u32, u32, u32, u32) {
    let start = node.start_position();
    let end = node.end_position();
    (
        start.row as u32,
        start.column as u32,
        end.row as u32,
        end.column as u32,
    )
}

pub(in crate::server) fn override_signature_diagnostics(
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    for class_sym in file_symbols.symbols.iter().filter(|sym| {
        matches!(
            sym.kind,
            php_lsp_types::PhpSymbolKind::Class
                | php_lsp_types::PhpSymbolKind::Interface
                | php_lsp_types::PhpSymbolKind::Trait
        )
    }) {
        let child_methods: Vec<_> = file_symbols
            .symbols
            .iter()
            .filter(|sym| {
                sym.kind == php_lsp_types::PhpSymbolKind::Method
                    && sym.parent_fqn.as_deref() == Some(class_sym.fqn.as_str())
                    && !is_phpdoc_virtual_method_symbol(sym, class_sym)
            })
            .collect();

        for child_method in child_methods {
            if child_method.name == "__construct" {
                continue;
            }

            let mut reported = false;
            for parent_fqn in class_sym.extends.iter().chain(class_sym.implements.iter()) {
                let parent_member_fqn = format!("{}::{}", parent_fqn, child_method.name);
                let Some(parent_method) = index.resolve_fqn(&parent_member_fqn) else {
                    continue;
                };
                if parent_method.kind != php_lsp_types::PhpSymbolKind::Method {
                    continue;
                }
                let parent_file_symbols_guard = index.file_symbols.get(&parent_method.uri);
                let parent_file_symbols: &php_lsp_types::FileSymbols =
                    match parent_file_symbols_guard.as_ref() {
                        Some(entry) => entry.value(),
                        None => file_symbols,
                    };
                if !override_signatures_are_compatible(
                    child_method,
                    &parent_method,
                    file_symbols,
                    parent_file_symbols,
                    index,
                ) {
                    diagnostics.push(diagnostic_at_byte_range(
                        child_method.selection_range,
                        utf16_index,
                        format!(
                            "Incompatible override signature: {} differs from {}",
                            child_method.fqn, parent_method.fqn
                        ),
                    ));
                    reported = true;
                    break;
                }
            }
            if reported {
                continue;
            }
        }
    }

    diagnostics
}

pub(in crate::server) fn is_phpdoc_virtual_method_symbol(
    method: &php_lsp_types::SymbolInfo,
    owner: &php_lsp_types::SymbolInfo,
) -> bool {
    method.kind == php_lsp_types::PhpSymbolKind::Method
        && method.parent_fqn.as_deref() == Some(owner.fqn.as_str())
        && method.doc_comment.as_deref() == owner.doc_comment.as_deref()
        && method
            .doc_comment
            .as_deref()
            .is_some_and(|doc| doc.contains("@method"))
}

pub(in crate::server) fn override_signatures_are_compatible(
    child_method: &php_lsp_types::SymbolInfo,
    parent_method: &php_lsp_types::SymbolInfo,
    child_file_symbols: &php_lsp_types::FileSymbols,
    parent_file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
) -> bool {
    let (Some(child_sig), Some(parent_sig)) = (
        child_method.signature.as_ref(),
        parent_method.signature.as_ref(),
    ) else {
        return true;
    };

    if child_sig.params.len() < parent_sig.params.len() {
        return false;
    }
    if child_sig
        .params
        .iter()
        .skip(parent_sig.params.len())
        .any(|param| !signature_param_is_optional(param))
    {
        return false;
    }

    for (child_param, parent_param) in child_sig.params.iter().zip(parent_sig.params.iter()) {
        if child_param.is_variadic != parent_param.is_variadic
            || child_param.is_by_ref != parent_param.is_by_ref
            || (parent_param.default_value.is_some() && child_param.default_value.is_none())
            || !override_param_type_is_compatible(
                child_param.type_info.as_ref(),
                parent_param.type_info.as_ref(),
                child_file_symbols,
                parent_file_symbols,
                child_method.parent_fqn.as_deref(),
                parent_method.parent_fqn.as_deref(),
                index,
            )
        {
            return false;
        }
    }

    match (&child_sig.return_type, &parent_sig.return_type) {
        (Some(child_return), Some(parent_return)) => override_return_type_is_compatible(
            child_return,
            parent_return,
            child_file_symbols,
            parent_file_symbols,
            child_method.parent_fqn.as_deref(),
            parent_method.parent_fqn.as_deref(),
            index,
        ),
        (None, Some(_)) => false,
        _ => true,
    }
}

pub(in crate::server) fn signature_param_is_optional(param: &php_lsp_types::ParamInfo) -> bool {
    param.default_value.is_some() || param.is_variadic
}

pub(in crate::server) fn override_param_type_is_compatible(
    child_type: Option<&php_lsp_types::TypeInfo>,
    parent_type: Option<&php_lsp_types::TypeInfo>,
    child_file_symbols: &php_lsp_types::FileSymbols,
    parent_file_symbols: &php_lsp_types::FileSymbols,
    child_owner_fqn: Option<&str>,
    parent_owner_fqn: Option<&str>,
    index: &WorkspaceIndex,
) -> bool {
    match (child_type, parent_type) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(child_type), Some(parent_type)) => {
            type_info_is_mixed(child_type)
                || type_info_is_owner_template(parent_type, parent_owner_fqn, index)
                || normalized_type_info_for_override(
                    child_type,
                    child_file_symbols,
                    child_owner_fqn,
                ) == normalized_type_info_for_override(
                    parent_type,
                    parent_file_symbols,
                    parent_owner_fqn,
                )
                || type_info_refines_native(parent_type, child_type)
                || override_param_class_type_is_contravariant(
                    child_type,
                    parent_type,
                    child_file_symbols,
                    parent_file_symbols,
                    child_owner_fqn,
                    parent_owner_fqn,
                    index,
                )
        }
    }
}

pub(in crate::server) fn override_param_class_type_is_contravariant(
    child_type: &php_lsp_types::TypeInfo,
    parent_type: &php_lsp_types::TypeInfo,
    child_file_symbols: &php_lsp_types::FileSymbols,
    parent_file_symbols: &php_lsp_types::FileSymbols,
    child_owner_fqn: Option<&str>,
    parent_owner_fqn: Option<&str>,
    index: &WorkspaceIndex,
) -> bool {
    match (
        simple_class_fqn_for_override(child_type, child_file_symbols, child_owner_fqn),
        simple_class_fqn_for_override(parent_type, parent_file_symbols, parent_owner_fqn),
    ) {
        (Some(child_fqn), Some(parent_fqn)) => {
            class_extends_or_implements(index, &parent_fqn, &child_fqn, &mut Vec::new())
        }
        _ => false,
    }
}

pub(in crate::server) fn override_return_type_is_compatible(
    child_type: &php_lsp_types::TypeInfo,
    parent_type: &php_lsp_types::TypeInfo,
    child_file_symbols: &php_lsp_types::FileSymbols,
    parent_file_symbols: &php_lsp_types::FileSymbols,
    child_owner_fqn: Option<&str>,
    parent_owner_fqn: Option<&str>,
    index: &WorkspaceIndex,
) -> bool {
    if type_info_is_mixed(parent_type) {
        return true;
    }

    let child_normalized =
        normalized_type_info_for_override(child_type, child_file_symbols, child_owner_fqn);
    let parent_normalized =
        normalized_type_info_for_override(parent_type, parent_file_symbols, parent_owner_fqn);
    if child_normalized == parent_normalized {
        return true;
    }
    if type_info_refines_native(parent_type, child_type)
        || type_info_refines_native(child_type, parent_type)
    {
        return true;
    }

    match (
        simple_class_fqn_for_override(child_type, child_file_symbols, child_owner_fqn),
        simple_class_fqn_for_override(parent_type, parent_file_symbols, parent_owner_fqn),
    ) {
        (Some(child_fqn), Some(parent_fqn)) => {
            class_extends_or_implements(index, &child_fqn, &parent_fqn, &mut Vec::new())
        }
        _ => false,
    }
}

pub(in crate::server) fn type_info_is_mixed(type_info: &php_lsp_types::TypeInfo) -> bool {
    match type_info {
        php_lsp_types::TypeInfo::Mixed => true,
        php_lsp_types::TypeInfo::Simple(name) => name.eq_ignore_ascii_case("mixed"),
        _ => false,
    }
}

pub(in crate::server) fn type_info_is_owner_template(
    type_info: &php_lsp_types::TypeInfo,
    owner_fqn: Option<&str>,
    index: &WorkspaceIndex,
) -> bool {
    let Some(owner_fqn) = owner_fqn else {
        return false;
    };
    let php_lsp_types::TypeInfo::Simple(name) = type_info else {
        return false;
    };

    index
        .types
        .get(owner_fqn.trim_start_matches('\\'))
        .is_some_and(|owner| {
            owner
                .templates
                .iter()
                .any(|template| template.name == *name)
        })
}

pub(in crate::server) fn normalized_type_info_for_override(
    type_info: &php_lsp_types::TypeInfo,
    file_symbols: &php_lsp_types::FileSymbols,
    owner_fqn: Option<&str>,
) -> String {
    match type_info {
        php_lsp_types::TypeInfo::Simple(name) => {
            normalized_simple_type_for_override(name, file_symbols, owner_fqn)
        }
        php_lsp_types::TypeInfo::Union(types) => {
            let mut parts: Vec<_> = types
                .iter()
                .map(|type_info| {
                    normalized_type_info_for_override(type_info, file_symbols, owner_fqn)
                })
                .collect();
            parts.sort();
            format!("union({})", parts.join("|"))
        }
        php_lsp_types::TypeInfo::Intersection(types) => {
            let mut parts: Vec<_> = types
                .iter()
                .map(|type_info| {
                    normalized_type_info_for_override(type_info, file_symbols, owner_fqn)
                })
                .collect();
            parts.sort();
            format!("intersection({})", parts.join("&"))
        }
        php_lsp_types::TypeInfo::Nullable(inner) => format!(
            "?{}",
            normalized_type_info_for_override(inner, file_symbols, owner_fqn)
        ),
        php_lsp_types::TypeInfo::Generic { base, args } => {
            let base = normalized_simple_type_for_override(base, file_symbols, owner_fqn);
            let args = args
                .iter()
                .map(|type_info| {
                    normalized_type_info_for_override(type_info, file_symbols, owner_fqn)
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("{}<{}>", base, args)
        }
        php_lsp_types::TypeInfo::Conditional {
            if_type, else_type, ..
        } => format!(
            "conditional({}|{})",
            normalized_type_info_for_override(if_type, file_symbols, owner_fqn),
            normalized_type_info_for_override(else_type, file_symbols, owner_fqn)
        ),
        php_lsp_types::TypeInfo::ArrayShape(_)
        | php_lsp_types::TypeInfo::ObjectShape(_)
        | php_lsp_types::TypeInfo::Callable { .. }
        | php_lsp_types::TypeInfo::ClassString(_)
        | php_lsp_types::TypeInfo::LiteralString(_)
        | php_lsp_types::TypeInfo::LiteralInt(_)
        | php_lsp_types::TypeInfo::LiteralFloat(_)
        | php_lsp_types::TypeInfo::LiteralBool(_)
        | php_lsp_types::TypeInfo::LiteralNull => type_info.to_string(),
        php_lsp_types::TypeInfo::Void => "void".to_string(),
        php_lsp_types::TypeInfo::Never => "never".to_string(),
        php_lsp_types::TypeInfo::Mixed => "mixed".to_string(),
        php_lsp_types::TypeInfo::Self_ | php_lsp_types::TypeInfo::Static_ => owner_fqn
            .map(|owner| owner.trim_start_matches('\\').to_string())
            .unwrap_or_else(|| type_info.to_string()),
        php_lsp_types::TypeInfo::Parent_ => "parent".to_string(),
    }
}

pub(in crate::server) fn normalized_simple_type_for_override(
    name: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    owner_fqn: Option<&str>,
) -> String {
    let lower = name.trim_start_matches('\\').to_ascii_lowercase();
    if matches!(lower.as_str(), "self" | "static") {
        return owner_fqn
            .map(|owner| owner.trim_start_matches('\\').to_string())
            .unwrap_or(lower);
    }
    if lower == "parent" {
        return lower;
    }
    if is_builtin_type_name(name) {
        return lower;
    }
    resolve_class_name_pub(name, file_symbols)
        .trim_start_matches('\\')
        .to_string()
}

pub(in crate::server) fn simple_class_fqn_for_override(
    type_info: &php_lsp_types::TypeInfo,
    file_symbols: &php_lsp_types::FileSymbols,
    owner_fqn: Option<&str>,
) -> Option<String> {
    match type_info {
        php_lsp_types::TypeInfo::Simple(name) if !is_builtin_type_name(name) => {
            let lower = name.trim_start_matches('\\').to_ascii_lowercase();
            if matches!(lower.as_str(), "self" | "static") {
                return owner_fqn.map(|owner| owner.trim_start_matches('\\').to_string());
            }
            if lower == "parent" {
                return None;
            }
            Some(
                resolve_class_name_pub(name, file_symbols)
                    .trim_start_matches('\\')
                    .to_string(),
            )
        }
        php_lsp_types::TypeInfo::Self_ | php_lsp_types::TypeInfo::Static_ => {
            owner_fqn.map(|owner| owner.trim_start_matches('\\').to_string())
        }
        php_lsp_types::TypeInfo::Generic { base, .. } if !is_builtin_type_name(base) => Some(
            resolve_class_name_pub(base, file_symbols)
                .trim_start_matches('\\')
                .to_string(),
        ),
        _ => None,
    }
}

pub(in crate::server) fn php_version_type_diagnostics(
    tree: &tree_sitter::Tree,
    source: &str,
    php_version: PhpVersion,
    utf16_index: &Utf16LineIndex,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    walk_php_version_type_diagnostics(
        tree.root_node(),
        source,
        php_version,
        utf16_index,
        &mut diagnostics,
    );
    diagnostics
}

pub(in crate::server) fn walk_php_version_type_diagnostics(
    node: tree_sitter::Node,
    source: &str,
    php_version: PhpVersion,
    utf16_index: &Utf16LineIndex,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for (field_name, is_return_type) in [("type", false), ("return_type", true)] {
        if let Some(type_node) = node.child_by_field_name(field_name) {
            check_declared_type_php_version(
                type_node,
                source,
                php_version,
                is_return_type,
                utf16_index,
                diagnostics,
            );
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_php_version_type_diagnostics(child, source, php_version, utf16_index, diagnostics);
    }
}

pub(in crate::server) fn check_declared_type_php_version(
    type_node: tree_sitter::Node,
    source: &str,
    php_version: PhpVersion,
    is_return_type: bool,
    utf16_index: &Utf16LineIndex,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let type_text = source[type_node.byte_range()].trim();
    if declared_type_hint_is_supported(type_text, php_version, is_return_type) {
        return;
    }

    diagnostics.push(diagnostic_at_byte_range(
        node_range_node(type_node),
        utf16_index,
        format!(
            "Type is not supported by PHP {}: {}",
            php_version_label(php_version),
            type_text
        ),
    ));
}

pub(in crate::server) fn declared_type_hint_is_supported(
    type_text: &str,
    php_version: PhpVersion,
    is_return_type: bool,
) -> bool {
    let trimmed = type_text.trim();
    if let Some(inner) = trimmed.strip_prefix('?') {
        return php_version.at_least(7, 1)
            && !inner.contains(['|', '&'])
            && simple_declared_type_hint_is_supported(inner, php_version, false, is_return_type);
    }

    if trimmed.contains('|') {
        return php_version.at_least(8, 0)
            && trimmed.split('|').all(|part| {
                let part = part.trim();
                !matches!(part.to_ascii_lowercase().as_str(), "void" | "never")
                    && simple_declared_type_hint_is_supported(
                        part,
                        php_version,
                        true,
                        is_return_type,
                    )
            });
    }

    if trimmed.contains('&') {
        return php_version.at_least(8, 1)
            && trimmed
                .split('&')
                .all(|part| intersection_declared_type_hint_is_supported(part.trim()));
    }

    simple_declared_type_hint_is_supported(trimmed, php_version, false, is_return_type)
}

pub(in crate::server) fn simple_declared_type_hint_is_supported(
    type_text: &str,
    php_version: PhpVersion,
    in_union: bool,
    is_return_type: bool,
) -> bool {
    let lower = type_text
        .trim()
        .trim_start_matches('\\')
        .to_ascii_lowercase();
    match lower.as_str() {
        "" => false,
        "void" => is_return_type && php_version.at_least(7, 1),
        "never" => is_return_type && php_version.at_least(8, 1),
        "mixed" => php_version.at_least(8, 0),
        "static" => is_return_type && php_version.at_least(8, 0),
        "false" | "null" => {
            if in_union {
                php_version.at_least(8, 0)
            } else {
                php_version.at_least(8, 2)
            }
        }
        "true" => php_version.at_least(8, 2),
        "resource" => false,
        _ => true,
    }
}

pub(in crate::server) fn intersection_declared_type_hint_is_supported(type_text: &str) -> bool {
    let lower = type_text
        .trim()
        .trim_start_matches('\\')
        .to_ascii_lowercase();
    !matches!(
        lower.as_str(),
        "" | "array"
            | "bool"
            | "callable"
            | "false"
            | "float"
            | "int"
            | "iterable"
            | "mixed"
            | "never"
            | "null"
            | "object"
            | "resource"
            | "string"
            | "true"
            | "void"
    )
}

pub(in crate::server) fn php_version_label(php_version: PhpVersion) -> String {
    format!("{}.{}", php_version.major, php_version.minor)
}

pub(in crate::server) fn workspace_duplicate_symbol_diagnostics(
    uri_str: &str,
    file_symbols: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
    utf16_index: &Utf16LineIndex,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    for sym in &file_symbols.symbols {
        if sym.modifiers.is_builtin || !is_duplicate_checked_symbol_kind(sym.kind) {
            continue;
        }

        let has_duplicate = index.file_symbols.iter().any(|entry| {
            if entry.key().as_str() == uri_str {
                return false;
            }

            entry.value().symbols.iter().any(|other| {
                other.kind == sym.kind && other.fqn == sym.fqn && !other.modifiers.is_builtin
            })
        });

        if has_duplicate {
            diagnostics.push(Diagnostic {
                range: Range {
                    start: Position::new(
                        sym.selection_range.0,
                        utf16_index.byte_col_to_utf16(sym.selection_range.0, sym.selection_range.1),
                    ),
                    end: Position::new(
                        sym.selection_range.2,
                        utf16_index.byte_col_to_utf16(sym.selection_range.2, sym.selection_range.3),
                    ),
                },
                severity: Some(DiagnosticSeverity::WARNING),
                source: Some("php-lsp".to_string()),
                message: format!("Duplicate symbol: {}", sym.fqn),
                ..Default::default()
            });
        }
    }

    diagnostics
}

pub(in crate::server) fn is_duplicate_checked_symbol_kind(
    kind: php_lsp_types::PhpSymbolKind,
) -> bool {
    matches!(
        kind,
        php_lsp_types::PhpSymbolKind::Class
            | php_lsp_types::PhpSymbolKind::Interface
            | php_lsp_types::PhpSymbolKind::Trait
            | php_lsp_types::PhpSymbolKind::Enum
            | php_lsp_types::PhpSymbolKind::Function
            | php_lsp_types::PhpSymbolKind::GlobalConstant
    )
}

pub(in crate::server) fn current_class_fqn(
    file_symbols: &php_lsp_types::FileSymbols,
) -> Option<String> {
    file_symbols.symbols.iter().find_map(|sym| {
        matches!(
            sym.kind,
            php_lsp_types::PhpSymbolKind::Class
                | php_lsp_types::PhpSymbolKind::Interface
                | php_lsp_types::PhpSymbolKind::Trait
                | php_lsp_types::PhpSymbolKind::Enum
        )
        .then(|| sym.fqn.clone())
    })
}

impl PhpLspBackend {
    pub(in crate::server) async fn phpstan_diagnostics_for_uri(
        &self,
        uri: &Uri,
        cancellation: OperationCancellationToken,
    ) -> Vec<Diagnostic> {
        let config = self.phpstan_config.lock().await.clone();
        if !config.enabled {
            return vec![];
        }

        if !uri_is_php_file(uri) {
            return vec![];
        }

        let Some(file_path) = uri_to_path(uri.as_str()) else {
            return vec![];
        };
        if !file_path.exists() {
            return vec![];
        }

        let workspace_root = self.workspace_root_for_uri(uri.as_str()).await;
        match run_phpstan_for_file(config, file_path, workspace_root, Some(cancellation)).await {
            Ok(diagnostics) => diagnostics,
            Err(message) => {
                if message.contains("command cancelled") {
                    tracing::debug!(
                        "PHPStan diagnostics cancelled for {}: {}",
                        uri.as_str(),
                        message
                    );
                    return vec![];
                }
                tracing::warn!(
                    "PHPStan diagnostics failed for {}: {}",
                    uri.as_str(),
                    message
                );
                self.client
                    .log_message(
                        MessageType::WARNING,
                        format!("php-lsp PHPStan diagnostics failed: {}", message),
                    )
                    .await;
                vec![]
            }
        }
    }

    pub(in crate::server) async fn psalm_diagnostics_for_uri(
        &self,
        uri: &Uri,
        cancellation: OperationCancellationToken,
    ) -> Vec<Diagnostic> {
        let config = self.psalm_config.lock().await.clone();
        if !config.enabled {
            return vec![];
        }

        if !uri_is_php_file(uri) {
            return vec![];
        }

        let Some(file_path) = uri_to_path(uri.as_str()) else {
            return vec![];
        };
        if !file_path.exists() {
            return vec![];
        }

        let workspace_root = self.workspace_root_for_uri(uri.as_str()).await;
        match run_psalm_for_file(config, file_path, workspace_root, Some(cancellation)).await {
            Ok(diagnostics) => diagnostics,
            Err(message) => {
                if message.contains("command cancelled") {
                    tracing::debug!(
                        "Psalm diagnostics cancelled for {}: {}",
                        uri.as_str(),
                        message
                    );
                    return vec![];
                }
                tracing::warn!("Psalm diagnostics failed for {}: {}", uri.as_str(), message);
                self.client
                    .log_message(
                        MessageType::WARNING,
                        format!("php-lsp Psalm diagnostics failed: {}", message),
                    )
                    .await;
                vec![]
            }
        }
    }

    pub(in crate::server) fn references_for_file(
        &self,
        file_uri: &str,
        target_fqn: &str,
        target_kind: php_lsp_types::PhpSymbolKind,
        include_declaration: bool,
    ) -> Vec<php_lsp_types::SymbolReference> {
        let mut refs = if let Some(parser) = self.open_files.get(file_uri) {
            current_parser_symbol_references(file_uri, &parser)
        } else {
            self.index
                .file_references
                .get(file_uri)
                .map(|entry| entry.value().clone())
                .unwrap_or_default()
        };
        refs.retain(|reference| {
            symbol_reference_matches(
                &self.index,
                reference,
                target_fqn,
                target_kind,
                include_declaration,
            )
        });
        refs
    }

    /// Publish diagnostics for a file.
    pub(in crate::server) async fn publish_diagnostics(&self, uri: &Uri) {
        let uri_str = uri.as_str().to_string();
        let template_document = self.template_document(&uri_str);
        let version = self.current_document_version(&uri_str);
        let diagnostics_mode = *self.diagnostics_mode.lock().await;
        let should_preresolve_dependencies = template_document.is_none()
            && diagnostics_mode == DiagnosticsMode::BasicSemantic
            && *self.index_vendor.lock().await;

        // Pre-resolve use statements via lazy indexing so that vendor classes
        // are available for the synchronous `compute_diagnostics` resolver.
        if should_preresolve_dependencies {
            if let Some(fs) = self.index.file_symbols.get(&uri_str) {
                let fqns_to_resolve: Vec<String> = fs
                    .use_statements
                    .iter()
                    .filter(|u| u.kind == php_lsp_types::UseKind::Class)
                    .filter(|u| u.fqn.contains('\\'))
                    .map(|u| u.fqn.clone())
                    .collect();
                drop(fs); // release DashMap ref before async calls
                for fqn in fqns_to_resolve {
                    self.lazy_index_class_dependencies(&fqn).await;
                }
            }
        }

        // Also pre-resolve: class FQNs from aliased qualified names used in code.
        // e.g. `use Symfony\...\Constraints as Assert;` → `new Assert\NotBlank`
        // → need to lazily index `Symfony\...\Constraints\NotBlank`.
        if should_preresolve_dependencies {
            let alias_fqns = if let Some(parser) = self.open_files.get(&uri_str) {
                if let Some(tree) = parser.tree() {
                    let source = parser.source();
                    self.index
                        .file_symbols
                        .get(&uri_str)
                        .map(|fs| collect_aliased_class_fqns(tree, &source, &fs))
                        .unwrap_or_default()
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };
            for fqn in alias_fqns {
                self.lazy_index_class_dependencies(&fqn).await;
            }
        }

        let diagnostic_severity = *self.diagnostic_severity.lock().await;
        let diagnostic_budget = *self.diagnostic_budget.lock().await;
        let php_version = *self.php_version.lock().await;
        let diagnostics_config = DiagnosticsRuntimeConfig {
            mode: diagnostics_mode,
            severity: diagnostic_severity,
            budget: diagnostic_budget,
            php_version,
        };
        let mut diagnostics = compute_open_file_diagnostics(
            &uri_str,
            &self.open_files,
            &self.index,
            diagnostics_config,
            version,
        )
        .await;
        if let Some(template) = &template_document {
            diagnostics = template
                .map_diagnostics_to_original(diagnostics, diagnostics_mode == DiagnosticsMode::Off);
        } else if should_preresolve_dependencies {
            diagnostics = self
                .filter_lazy_resolved_symbol_diagnostics(diagnostics)
                .await;
        }

        let has_syntax_errors = diagnostics.iter().any(|diagnostic| {
            diagnostic.source.as_deref() == Some("php-lsp")
                && diagnostic.severity == Some(DiagnosticSeverity::ERROR)
        });
        if template_document.is_none()
            && diagnostics_mode == DiagnosticsMode::BasicSemantic
            && !has_syntax_errors
        {
            let analyzer_token = self.start_analyzer_run(&uri_str).await;
            diagnostics.extend(
                self.phpstan_diagnostics_for_uri(uri, analyzer_token.clone())
                    .await,
            );
            if analyzer_token.is_cancelled() {
                self.finish_analyzer_run(&uri_str, &analyzer_token).await;
                return;
            }
            diagnostics.extend(
                self.psalm_diagnostics_for_uri(uri, analyzer_token.clone())
                    .await,
            );
            if analyzer_token.is_cancelled() {
                self.finish_analyzer_run(&uri_str, &analyzer_token).await;
                return;
            }
            self.finish_analyzer_run(&uri_str, &analyzer_token).await;
        }

        if self.current_document_version(&uri_str) != version {
            tracing::debug!(
                "Skipping stale diagnostics for {}: computed for version {:?}, current {:?}",
                uri_str,
                version,
                self.current_document_version(&uri_str)
            );
            return;
        }

        let publish_started = Instant::now();
        let publish_span = tracing::debug_span!(
            "diagnostics.publish",
            uri = %uri_str,
            version = ?version,
            duration_ms = tracing::field::Empty,
        );
        async {
            self.client
                .publish_diagnostics(uri.clone(), diagnostics, version)
                .await;
        }
        .instrument(publish_span.clone())
        .await;
        publish_span.record("duration_ms", publish_started.elapsed().as_millis() as u64);
    }

    pub(in crate::server) async fn filter_lazy_resolved_symbol_diagnostics(
        &self,
        diagnostics: Vec<Diagnostic>,
    ) -> Vec<Diagnostic> {
        let mut filtered = Vec::with_capacity(diagnostics.len());

        for diagnostic in diagnostics {
            if diagnostic.source.as_deref() == Some("php-lsp") {
                if let Some(fqn) = lazy_resolvable_diagnostic_fqn(&diagnostic.message) {
                    if self.resolve_fqn_lazy(&fqn).await.is_some() {
                        continue;
                    }
                }
            }
            filtered.push(diagnostic);
        }

        filtered
    }
}
