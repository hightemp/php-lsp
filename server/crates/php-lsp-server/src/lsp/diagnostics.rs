//! Diagnostics LSP handlers extracted from `server.rs`.

use super::super::*;

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
            let mut parser = FileParser::new();
            parser.parse_full(updated.virtual_source());
            self.template_documents.insert(uri_str.clone(), updated);
            self.index.remove_file(&uri_str);
            self.open_files.insert(uri_str.clone(), parser);
            self.semantic_tokens_cache.lock().await.remove(&uri_str);
            self.schedule_fast_diagnostics(uri, version).await;
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

        self.schedule_fast_diagnostics(uri, version).await;
    }

    pub(crate) async fn lsp_did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        let uri_str = uri.as_str().to_string();
        tracing::debug!("didClose: {}", uri_str);
        self.open_files.remove(&uri_str);
        self.template_documents.remove(&uri_str);
        self.document_versions.remove(&uri_str);
        self.cancel_debounced_diagnostics(&uri_str).await;
        self.cancel_analyzer_run(&uri_str).await;
        self.cancel_formatter_run(&uri_str).await;
        self.semantic_tokens_cache.lock().await.remove(&uri_str);
        // Clear diagnostics for closed file
        self.client.publish_diagnostics(uri, vec![], None).await;
    }

    pub(crate) async fn lsp_did_save(&self, params: DidSaveTextDocumentParams) {
        tracing::debug!("didSave: {}", params.text_document.uri.as_str());
        self.cancel_debounced_diagnostics(params.text_document.uri.as_str())
            .await;
        self.publish_diagnostics(&params.text_document.uri).await;
    }
}
