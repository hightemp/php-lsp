use super::*;
use php_lsp_parser::parser::FileParser;
use php_lsp_parser::semantic::{extract_semantic_diagnostics, SemanticDiagnosticKind};
use php_lsp_parser::symbols::extract_file_symbols;

fn source_stubs_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/stubs")
}

fn source_stubs_are_available(stubs_path: &Path) -> bool {
    stubs_path.join("Core/Core.php").is_file()
        && stubs_path.join("standard/standard_2.php").is_file()
        && stubs_path.join("standard/standard_8.php").is_file()
}

#[test]
fn test_candidate_stubs_paths_include_source_checkout_stubs_from_target_binary() {
    let root = Path::new("/tmp/project");
    let exe = Path::new("/repo/php-lsp/server/target/debug/php-lsp");
    let paths = candidate_stubs_paths_for_exe(root, None, Some(exe));

    assert!(
        paths
            .iter()
            .any(|path| path == Path::new("/repo/php-lsp/server/data/stubs")),
        "expected source checkout stubs path in {paths:?}"
    );
}

#[test]
fn test_candidate_stubs_paths_include_packaged_extension_stubs_from_platform_binary() {
    let root = Path::new("/tmp/project");
    let exe =
        Path::new("/home/user/.vscode/extensions/hightemp.ht-php-lsp-0.7.0/bin/linux-x64/php-lsp");
    let paths = candidate_stubs_paths_for_exe(root, None, Some(exe));

    assert!(
        paths.iter().any(|path| {
            path == Path::new("/home/user/.vscode/extensions/hightemp.ht-php-lsp-0.7.0/stubs")
        }),
        "expected packaged extension stubs path in {paths:?}"
    );
}

#[cfg(unix)]
#[test]
fn test_stub_bundle_validation_ignores_symlinked_php_entries_and_cycles() {
    use std::os::unix::fs::symlink;

    let root =
        std::env::temp_dir().join(format!("php-lsp-server-stub-cycle-{}", std::process::id()));
    let external = std::env::temp_dir().join(format!(
        "php-lsp-server-external-stub-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&external);
    std::fs::create_dir_all(&root).expect("create candidate root");
    std::fs::create_dir_all(&external).expect("create external root");
    std::fs::write(
        external.join("Outside.php"),
        "<?php function outside(): void;",
    )
    .expect("write external PHP file");
    symlink(&root, root.join("cycle")).expect("create candidate cycle");
    symlink(external.join("Outside.php"), root.join("Linked.php")).expect("link external PHP file");

    assert_eq!(
        unusable_stubs_path_reason(&root).as_deref(),
        Some("contains no PHP stub files")
    );

    std::fs::write(root.join("Real.php"), "<?php function real(): void;")
        .expect("write real PHP file");
    std::fs::create_dir_all(root.join("Core")).expect("create required stub directory");
    symlink(external.join("Outside.php"), root.join("Core/Core.php"))
        .expect("link required PHP file");
    let reason = unusable_stubs_path_reason(&root).expect("linked required file must be rejected");
    assert!(
        reason.contains("Core/Core.php"),
        "linked required file should remain missing: {reason}"
    );

    std::fs::remove_file(root.join("Core/Core.php")).expect("remove linked required file");
    std::fs::remove_dir(root.join("Core")).expect("remove required stub directory");
    std::fs::write(
        external.join("Core.php"),
        "<?php function external_core(): void;",
    )
    .expect("write external required file");
    symlink(&external, root.join("Core")).expect("link required extension directory");
    let reason = unusable_stubs_path_reason(&root).expect("linked required directory must fail");
    assert!(
        reason.contains("Core/Core.php"),
        "required file through linked directory should remain missing: {reason}"
    );

    std::fs::remove_dir_all(root).expect("remove candidate root");
    std::fs::remove_dir_all(external).expect("remove external root");
}

#[test]
fn test_load_configured_stubs_exposes_standard_builtin_functions_from_source_checkout() {
    let stubs_path = source_stubs_path();
    if !source_stubs_are_available(&stubs_path) {
        eprintln!(
            "Skipping server stubs smoke test: stubs not initialized at {}",
            stubs_path.display()
        );
        return;
    }

    let index = WorkspaceIndex::new();
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let loaded = load_configured_stubs(
        &index,
        &repo_root,
        None,
        Some(vec!["standard".to_string()]),
        PhpVersion::DEFAULT,
        true,
    );

    assert!(loaded > 0, "expected standard stubs to load");
    for fqn in ["in_array", "sprintf"] {
        let symbol = index
            .resolve_fqn(fqn)
            .unwrap_or_else(|| panic!("missing standard built-in function: {fqn}"));
        assert!(
            symbol.modifiers.is_builtin,
            "standard function should be marked built-in: {fqn}"
        );
    }
}

#[test]
fn test_default_stubs_expose_global_extension_functions_in_namespaces() {
    let stubs_path = source_stubs_path();
    if !source_stubs_are_available(&stubs_path)
        || !stubs_path.join("libxml/libxml.php").is_file()
        || !stubs_path.join("posix/posix.php").is_file()
    {
        eprintln!(
            "Skipping server stubs smoke test: libxml/posix stubs not initialized at {}",
            stubs_path.display()
        );
        return;
    }

    let index = WorkspaceIndex::new();
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let loaded = load_configured_stubs(&index, &repo_root, None, None, PhpVersion::DEFAULT, true);

    assert!(loaded > 0, "expected default stubs to load");
    for fqn in ["libxml_clear_errors", "libxml_get_errors", "posix_geteuid"] {
        let symbol = index
            .resolve_fqn(fqn)
            .unwrap_or_else(|| panic!("missing default extension function: {fqn}"));
        assert!(
            symbol.modifiers.is_builtin,
            "extension function should be marked built-in: {fqn}"
        );
    }
    for fqn in ["ZipArchive", "ZipArchive::open", "ZipArchive::CREATE"] {
        let symbol = index
            .resolve_fqn(fqn)
            .unwrap_or_else(|| panic!("missing default extension symbol: {fqn}"));
        assert!(
            symbol.modifiers.is_builtin,
            "extension symbol should be marked built-in: {fqn}"
        );
    }

    let code = r#"<?php
namespace App\Controller;

libxml_clear_errors();
libxml_get_errors();
posix_geteuid();
"#;
    let mut parser = FileParser::new();
    parser.parse_full(code);
    let tree = parser.tree().expect("test PHP should parse");
    let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
    let diagnostics =
        extract_semantic_diagnostics(tree, code, &file_symbols, |fqn, expected_kinds| {
            index.resolve_fqn_matching_kinds(fqn, expected_kinds)
        });
    let unknown_functions: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.kind == SemanticDiagnosticKind::UnknownFunction)
        .collect();

    assert!(
        unknown_functions.is_empty(),
        "default global extension stubs should satisfy namespaced unqualified calls, got: {:?}",
        unknown_functions
    );
}
