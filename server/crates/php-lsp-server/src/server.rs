//! LSP server implementation — LanguageServer trait.

use dashmap::DashMap;
use php_lsp_index::workspace::WorkspaceIndex;
use php_lsp_parser::diagnostics::extract_syntax_errors;
use php_lsp_parser::parser::FileParser;
use std::sync::Arc;
use tower_lsp::jsonrpc::Result;
use tower_lsp::ls_types::*;
use tower_lsp::{Client, LanguageServer};

/// Main LSP backend holding all state.
pub struct PhpLspBackend {
    /// Client handle for sending notifications to VS Code.
    client: Client,
    /// Open document parsers (URI string → FileParser).
    open_files: DashMap<String, FileParser>,
    /// Global workspace symbol index.
    #[allow(dead_code)]
    index: Arc<WorkspaceIndex>,
}

impl PhpLspBackend {
    pub fn new(client: Client) -> Self {
        PhpLspBackend {
            client,
            open_files: DashMap::new(),
            index: Arc::new(WorkspaceIndex::new()),
        }
    }

    /// Publish diagnostics for a file.
    async fn publish_diagnostics(&self, uri: &Uri) {
        let uri_str = uri.as_str().to_string();

        let diagnostics = {
            if let Some(parser) = self.open_files.get(&uri_str) {
                if let Some(tree) = parser.tree() {
                    // extract_syntax_errors returns lsp_types::Diagnostic,
                    // we need to convert to ls_types::Diagnostic
                    let lsp_diags = extract_syntax_errors(tree, &parser.source());
                    lsp_diags
                        .into_iter()
                        .map(|d| Diagnostic {
                            range: Range {
                                start: Position::new(d.range.start.line, d.range.start.character),
                                end: Position::new(d.range.end.line, d.range.end.character),
                            },
                            severity: Some(DiagnosticSeverity::ERROR),
                            source: Some("php-lsp".to_string()),
                            message: d.message,
                            ..Default::default()
                        })
                        .collect::<Vec<_>>()
                } else {
                    vec![]
                }
            } else {
                vec![]
            }
        };

        self.client
            .publish_diagnostics(uri.clone(), diagnostics, None)
            .await;
    }
}

impl LanguageServer for PhpLspBackend {
    async fn initialize(&self, _params: InitializeParams) -> Result<InitializeResult> {
        tracing::info!("php-lsp: initialize");

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::INCREMENTAL),
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(false),
                        })),
                        ..Default::default()
                    },
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                })),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![
                        "$".to_string(),
                        ">".to_string(),
                        ":".to_string(),
                        "\\".to_string(),
                    ]),
                    resolve_provider: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "php-lsp".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        tracing::info!("php-lsp: initialized");
        self.client
            .log_message(MessageType::INFO, "php-lsp server initialized")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        tracing::info!("php-lsp: shutdown");
        Ok(())
    }

    // --- Document Synchronization ---

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let uri_str = uri.as_str().to_string();
        let text = &params.text_document.text;

        tracing::debug!("didOpen: {}", uri_str);

        let mut parser = FileParser::new();
        parser.parse_full(text);
        self.open_files.insert(uri_str, parser);

        self.publish_diagnostics(&uri).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let uri_str = uri.as_str().to_string();

        tracing::debug!("didChange: {}", uri_str);

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
        }

        self.publish_diagnostics(&uri).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        let uri_str = uri.as_str().to_string();
        tracing::debug!("didClose: {}", uri_str);
        self.open_files.remove(&uri_str);
        // Clear diagnostics for closed file
        self.client.publish_diagnostics(uri, vec![], None).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        tracing::debug!("didSave: {}", params.text_document.uri.as_str());
    }

    // --- Language Features (stubs for now) ---

    async fn hover(&self, _params: HoverParams) -> Result<Option<Hover>> {
        // TODO: M-014
        Ok(None)
    }

    async fn goto_definition(
        &self,
        _params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        // TODO: M-015
        Ok(None)
    }

    async fn references(&self, _params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        // TODO: M-019
        Ok(None)
    }

    async fn rename(&self, _params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        // TODO: M-020
        Ok(None)
    }

    async fn prepare_rename(
        &self,
        _params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        // TODO: M-020
        Ok(None)
    }

    async fn document_symbol(
        &self,
        _params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        // TODO: M-021
        Ok(None)
    }

    async fn symbol(
        &self,
        _params: WorkspaceSymbolParams,
    ) -> Result<Option<WorkspaceSymbolResponse>> {
        // TODO: M-022
        Ok(None)
    }

    async fn completion(&self, _params: CompletionParams) -> Result<Option<CompletionResponse>> {
        // TODO: M-017
        Ok(None)
    }

    async fn completion_resolve(&self, item: CompletionItem) -> Result<CompletionItem> {
        // TODO: M-018
        Ok(item)
    }
}
