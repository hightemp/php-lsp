use super::*;

pub(in crate::server) fn phpdoc_native_type_diagnostics(
    file: &php_lsp_types::FileSymbols,
    index: &WorkspaceIndex,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    for symbol in &file.symbols {
        let (Some(signature), Some((sl, sc, el, ec))) =
            (&symbol.signature, symbol.doc_comment_range)
        else {
            continue;
        };
        let range = Range::new(Position::new(sl, sc), Position::new(el, ec));
        let mut check =
            |tag: String, doc: &php_lsp_types::TypeInfo, native: &php_lsp_types::TypeInfo| {
                if index.phpdoc_type_refinement(symbol, doc, native)
                    == php_lsp_types::type_refinement::TypeRefinement::Incompatible
                {
                    diagnostics.push(Diagnostic {
                        range,
                        severity: Some(DiagnosticSeverity::WARNING),
                        code: Some(NumberOrString::String("phpdoc-type-mismatch".to_string())),
                        source: Some("php-lsp".to_string()),
                        message: format!(
                            "PHPDoc {tag} type {doc} is incompatible with native type {native}"
                        ),
                        ..Default::default()
                    });
                }
            };
        for param in &signature.params {
            if let (Some(doc), Some(native)) = (&param.phpdoc_type_info, &param.native_type_info) {
                check(format!("@param ${}", param.name), doc, native);
            }
        }
        if let (Some(doc), Some(native)) =
            (&signature.phpdoc_return_type, &signature.native_return_type)
        {
            check(
                if symbol.kind == php_lsp_types::PhpSymbolKind::Property {
                    "@var"
                } else {
                    "@return"
                }
                .to_string(),
                doc,
                native,
            );
        }
    }
    diagnostics
}
