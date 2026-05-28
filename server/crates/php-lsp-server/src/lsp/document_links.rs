//! Document Links LSP handlers extracted from `server.rs`.

use super::super::*;

impl PhpLspBackend {
    pub(crate) async fn lsp_document_link(
        &self,
        params: DocumentLinkParams,
    ) -> Result<Option<Vec<DocumentLink>>> {
        let uri_str = params.text_document.uri.as_str().to_string();
        let Some(file_path) = uri_to_path(&uri_str) else {
            return Ok(None);
        };

        let links = if let Some(parser) = self.open_files.get(&uri_str) {
            let Some(tree) = parser.tree() else {
                return Ok(None);
            };
            document_links_for_source(&parser.source(), tree, &file_path)
        } else {
            let Ok(source) =
                read_file_to_string_blocking(file_path.clone(), "documentLink source read").await
            else {
                return Ok(None);
            };
            let mut parser = FileParser::new();
            parser.parse_full(&source);
            let Some(tree) = parser.tree() else {
                return Ok(None);
            };
            document_links_for_source(&source, tree, &file_path)
        };

        if links.is_empty() {
            Ok(None)
        } else {
            Ok(Some(links))
        }
    }
}
