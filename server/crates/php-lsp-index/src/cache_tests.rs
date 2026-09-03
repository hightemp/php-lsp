use super::*;
use php_lsp_types::{
    ArrayShapeItem, NamespaceScope, ParamInfo, PhpDocTypeAlias, PhpDocTypeAliasImport, Signature,
    SymbolModifiers, SymbolReferenceReceiver, TemplateBinding, TemplateBindingKind, TemplateParam,
    TemplateVariance, TypeInfo, UseKind, UseStatement, Visibility,
};
use std::io::Write;

const CACHE_SCHEMA_FIXTURE_VERSION: u32 = 23;
const CACHE_SCHEMA_FIXTURE_SERIALIZED_LEN: usize = 3315;
const CACHE_SCHEMA_FIXTURE_HASH: u64 = 0x0518_12bd_a359_78dd;

fn unique_temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "php-lsp-cache-test-{}-{}",
        name,
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn make_symbol(uri: &str) -> SymbolInfo {
    SymbolInfo {
        name: "Foo".to_string(),
        fqn: "App\\Foo".to_string(),
        kind: PhpSymbolKind::Class,
        uri: uri.to_string(),
        range: (0, 0, 1, 0),
        selection_range: (0, 6, 0, 9),
        visibility: Visibility::Public,
        modifiers: SymbolModifiers::default(),
        attributes: vec![],
        doc_comment: None,
        signature: None,
        parent_fqn: None,
        extends: vec![],
        implements: vec![],
        traits: vec![],
        templates: vec![],
        template_bindings: vec![],
    }
}

fn make_member_symbol(uri: &str) -> SymbolInfo {
    let mut symbol = make_symbol(uri);
    symbol.name = "member".to_string();
    symbol.fqn = "App\\Foo::member".to_string();
    symbol.kind = PhpSymbolKind::Method;
    symbol.parent_fqn = Some("App\\Foo".to_string());
    symbol
}

fn cache_schema_symbol(uri: &str, name: &str, kind: PhpSymbolKind) -> SymbolInfo {
    SymbolInfo {
        name: name.to_string(),
        fqn: format!("App\\{name}"),
        kind,
        uri: uri.to_string(),
        range: (1, 2, 3, 4),
        selection_range: (1, 12, 1, 12 + name.len() as u32),
        visibility: Visibility::Protected,
        modifiers: SymbolModifiers {
            is_static: true,
            is_abstract: false,
            is_final: true,
            is_readonly: false,
            is_deprecated: true,
            is_builtin: false,
        },
        attributes: vec![],
        doc_comment: Some("/** @template T of object */".to_string()),
        signature: Some(Signature {
            params: vec![ParamInfo {
                name: "items".to_string(),
                type_info: Some(TypeInfo::Generic {
                    base: "array".to_string(),
                    args: vec![TypeInfo::ArrayShape(vec![ArrayShapeItem {
                        key: Some("id".to_string()),
                        optional: false,
                        value: TypeInfo::LiteralInt("42".to_string()),
                    }])],
                }),
                default_value: Some("[]".to_string()),
                is_variadic: false,
                is_by_ref: true,
                is_promoted: false,
            }],
            return_type: Some(TypeInfo::ClassString(Some(Box::new(TypeInfo::Simple(
                "App\\Foo".to_string(),
            ))))),
        }),
        parent_fqn: Some("App\\Base".to_string()),
        extends: vec!["App\\Base".to_string()],
        implements: vec!["App\\Contract".to_string()],
        traits: vec!["App\\SharedTrait".to_string()],
        templates: vec![TemplateParam {
            name: "T".to_string(),
            bound: Some(TypeInfo::Simple("object".to_string())),
            variance: TemplateVariance::Covariant,
        }],
        template_bindings: vec![TemplateBinding {
            kind: TemplateBindingKind::Extends,
            target: "App\\Base".to_string(),
            args: vec![TypeInfo::Static_],
        }],
    }
}

fn cache_schema_fixture() -> IndexCache {
    let uri = "file:///tmp/php-lsp-cache-schema/src/Foo.php";
    let class_symbol = cache_schema_symbol(uri, "Foo", PhpSymbolKind::Class);
    let function_symbol = cache_schema_symbol(uri, "helper", PhpSymbolKind::Function);
    let constant_symbol = cache_schema_symbol(uri, "APP_VERSION", PhpSymbolKind::GlobalConstant);

    IndexCache {
        schema_version: CACHE_SCHEMA_VERSION,
        namespace: CacheNamespace::Workspace.as_str().to_string(),
        php_lsp_version: "0.7.0".to_string(),
        workspace_root: "/tmp/php-lsp-cache-schema".to_string(),
        config_hash: 0x0123_4567_89ab_cdef,
        stubs_hash: 0xfedc_ba98_7654_3210,
        created_at_unix_ms: 1_765_000_000_123,
        files: vec![CachedFile {
            uri: uri.to_string(),
            relative_path: "src/Foo.php".to_string(),
            metadata: CachedFileMetadata {
                modified_secs: 1_765_000_000,
                modified_nanos: 123_456_789,
                modified_status: ModifiedTimeStatus::Available,
                size: 321,
                content_hash: 0x1020_3040_5060_7080,
            },
            file_symbols: FileSymbols {
                namespace: Some("App".to_string()),
                namespace_scopes: vec![NamespaceScope {
                    namespace: Some("App".to_string()),
                    range: (0, 0, 12, 1),
                }],
                use_statements: vec![
                    UseStatement {
                        fqn: "Vendor\\Package\\Thing".to_string(),
                        alias: Some("ThingAlias".to_string()),
                        kind: UseKind::Class,
                        namespace: Some("App".to_string()),
                        range: (0, 5, 0, 32),
                    },
                    UseStatement {
                        fqn: "Vendor\\Package\\helper".to_string(),
                        alias: None,
                        kind: UseKind::Function,
                        namespace: Some("App".to_string()),
                        range: (1, 5, 1, 40),
                    },
                    UseStatement {
                        fqn: "Vendor\\Package\\APP_CONST".to_string(),
                        alias: Some("APP_CONST_ALIAS".to_string()),
                        kind: UseKind::Constant,
                        namespace: Some("App".to_string()),
                        range: (2, 5, 2, 44),
                    },
                ],
                symbols: vec![class_symbol.clone(), function_symbol.clone()],
                type_aliases: vec![PhpDocTypeAlias {
                    name: "Payload".to_string(),
                    type_info: TypeInfo::Union(vec![
                        TypeInfo::ObjectShape(vec![ArrayShapeItem {
                            key: Some("name".to_string()),
                            optional: true,
                            value: TypeInfo::Nullable(Box::new(TypeInfo::LiteralString(
                                "demo".to_string(),
                            ))),
                        }]),
                        TypeInfo::LiteralNull,
                    ]),
                }],
                type_alias_imports: vec![PhpDocTypeAliasImport {
                    name: "ExternalPayload".to_string(),
                    source_alias: "Payload".to_string(),
                    source_type: "Vendor\\Package\\Thing".to_string(),
                }],
            },
            references: vec![
                SymbolReference {
                    target_fqn: "App\\Foo".to_string(),
                    target_kind: PhpSymbolKind::Class,
                    range: (5, 4, 5, 7),
                    is_declaration: true,
                    starts_with_dollar: false,
                    allows_global_fallback: false,
                    rename_range: None,
                    preserve_spelling_on_rename: false,
                    is_import_target: false,
                    receiver: SymbolReferenceReceiver::None,
                },
                SymbolReference {
                    target_fqn: "App\\Foo::bar".to_string(),
                    target_kind: PhpSymbolKind::Method,
                    range: (8, 10, 8, 13),
                    is_declaration: false,
                    starts_with_dollar: false,
                    allows_global_fallback: false,
                    rename_range: None,
                    preserve_spelling_on_rename: false,
                    is_import_target: false,
                    receiver: SymbolReferenceReceiver::ResolvedType {
                        type_fqn: "App\\Foo".to_string(),
                    },
                },
                SymbolReference {
                    target_fqn: "App\\Foo::$name".to_string(),
                    target_kind: PhpSymbolKind::Property,
                    range: (9, 15, 9, 20),
                    is_declaration: false,
                    starts_with_dollar: true,
                    allows_global_fallback: false,
                    rename_range: None,
                    preserve_spelling_on_rename: false,
                    is_import_target: false,
                    receiver: SymbolReferenceReceiver::StaticClass {
                        class_fqn: "App\\Foo".to_string(),
                    },
                },
                SymbolReference {
                    target_fqn: "App\\Foo::missing".to_string(),
                    target_kind: PhpSymbolKind::Method,
                    range: (10, 15, 10, 22),
                    is_declaration: false,
                    starts_with_dollar: false,
                    allows_global_fallback: false,
                    rename_range: None,
                    preserve_spelling_on_rename: false,
                    is_import_target: false,
                    receiver: SymbolReferenceReceiver::Unresolved,
                },
            ],
        }],
        top_level: CachedTopLevelSymbols {
            types: vec![class_symbol],
            functions: vec![function_symbol],
            constants: vec![constant_symbol],
        },
    }
}

fn test_config() -> IndexCacheConfig {
    IndexCacheConfig {
        namespace: CacheNamespace::Workspace,
        php_lsp_version: "0.4.1".to_string(),
        php_version: "8.2".to_string(),
        include_paths: vec!["src".to_string()],
        exclude_paths: vec!["vendor".to_string()],
        traversal_max_files: Some(100_000),
        traversal_max_entries: Some(1_000_000),
        stub_extensions: vec!["Core".to_string()],
        stubs_hash: 42,
    }
}

#[test]
fn traversal_limits_participate_in_cache_config_hash() {
    let baseline = test_config();
    let mut different_files = baseline.clone();
    different_files.traversal_max_files = Some(50_000);
    let mut unlimited_entries = baseline.clone();
    unlimited_entries.traversal_max_entries = None;

    assert_ne!(baseline.config_hash(), different_files.config_hash());
    assert_ne!(baseline.config_hash(), unlimited_entries.config_hash());
}

#[test]
fn cache_schema_fixture_matches_version_guard() {
    let cache = cache_schema_fixture();
    assert_eq!(cache.schema_version, CACHE_SCHEMA_VERSION);
    assert_eq!(
        CACHE_SCHEMA_VERSION, CACHE_SCHEMA_FIXTURE_VERSION,
        "CACHE_SCHEMA_VERSION changed; update CACHE_SCHEMA_FIXTURE_* constants together"
    );

    let bytes = bincode::serialize(&cache).unwrap();
    assert_eq!(
        bytes.len(),
        CACHE_SCHEMA_FIXTURE_SERIALIZED_LEN,
        "serialized cache fixture size changed; bump CACHE_SCHEMA_VERSION and update \
             CACHE_SCHEMA_FIXTURE_* constants together"
    );
    assert_eq!(
        stable_hash_bytes(&bytes),
        CACHE_SCHEMA_FIXTURE_HASH,
        "serialized cache fixture hash changed; bump CACHE_SCHEMA_VERSION and update \
             CACHE_SCHEMA_FIXTURE_* constants together"
    );
}

#[test]
fn cache_roundtrip_loads_valid_file_symbols() {
    let root = unique_temp_dir("roundtrip");
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();
    let file = src.join("Foo.php");
    fs::write(&file, "<?php class Foo {}").unwrap();
    let uri = path_to_uri(&file).unwrap();

    let index = WorkspaceIndex::new();
    index.update_file(
        &uri,
        FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![make_symbol(&uri), make_member_symbol(&uri)],
            ..Default::default()
        },
    );

    let config = test_config();
    let cache = build_cache_from_index(&index, &root, std::slice::from_ref(&file), &config);
    assert_eq!(cache.files.len(), 1);
    assert_eq!(cache.top_level.types.len(), 1);

    let cache_path = root.join("index.bin");
    save_cache_atomic(&cache_path, &cache).unwrap();

    let loaded = WorkspaceIndex::new();
    let report = load_valid_cached_files(
        &loaded,
        &cache_path,
        &root,
        std::slice::from_ref(&file),
        &config,
    );
    assert_eq!(report.loaded_files, 1);
    assert!(report.parse_files.is_empty());
    assert!(loaded.resolve_fqn("App\\Foo").is_some());
    assert!(loaded.resolve_fqn("App\\Foo::member").is_some());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn cache_invalidates_stale_schema_version() {
    let root = unique_temp_dir("stale-schema");
    let file = root.join("Foo.php");
    fs::write(&file, "<?php class Foo {}").unwrap();
    let uri = path_to_uri(&file).unwrap();

    let index = WorkspaceIndex::new();
    index.update_file(
        &uri,
        FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![make_symbol(&uri)],
            ..Default::default()
        },
    );

    let config = test_config();
    let mut cache = build_cache_from_index(&index, &root, std::slice::from_ref(&file), &config);
    cache.schema_version = CACHE_SCHEMA_VERSION - 1;
    let cache_path = root.join("index.bin");
    save_cache_atomic(&cache_path, &cache).unwrap();

    let loaded = WorkspaceIndex::new();
    let report = load_valid_cached_files(
        &loaded,
        &cache_path,
        &root,
        std::slice::from_ref(&file),
        &config,
    );

    assert_eq!(report.loaded_files, 0);
    assert_eq!(report.missing_files, 1);
    assert_eq!(report.parse_files, vec![file.clone()]);
    assert!(
        report
            .miss_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("schema version mismatch")),
        "unexpected miss reason: {:?}",
        report.miss_reason
    );
    assert!(loaded.resolve_fqn("App\\Foo").is_none());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn malformed_bincode_cache_is_a_cache_miss() {
    let root = unique_temp_dir("malformed-bincode");
    let file = root.join("Foo.php");
    fs::write(&file, "<?php class Foo {}").unwrap();
    let cache_path = root.join("index.bin");
    fs::write(&cache_path, [0xff]).unwrap();

    let loaded = WorkspaceIndex::new();
    let config = test_config();
    let report = load_valid_cached_files(
        &loaded,
        &cache_path,
        &root,
        std::slice::from_ref(&file),
        &config,
    );

    assert_eq!(report.loaded_files, 0);
    assert_eq!(report.missing_files, 1);
    assert_eq!(report.parse_files, vec![file.clone()]);
    assert!(
        report
            .miss_reason
            .as_deref()
            .is_some_and(|reason| reason.starts_with("failed to load cache:")),
        "unexpected miss reason: {:?}",
        report.miss_reason
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn cache_roundtrip_loads_file_references() {
    let root = unique_temp_dir("references");
    let file = root.join("Foo.php");
    fs::write(&file, "<?php class Foo {}").unwrap();
    let uri = path_to_uri(&file).unwrap();
    let references = vec![SymbolReference {
        target_fqn: "App\\Foo".to_string(),
        target_kind: PhpSymbolKind::Class,
        range: (3, 12, 3, 15),
        is_declaration: false,
        starts_with_dollar: false,
        allows_global_fallback: false,
        rename_range: None,
        preserve_spelling_on_rename: false,
        is_import_target: false,
        receiver: Default::default(),
    }];

    let index = WorkspaceIndex::new();
    index.update_file_with_references(
        &uri,
        FileSymbols {
            namespace: None,
            use_statements: vec![],
            symbols: vec![make_symbol(&uri)],
            ..Default::default()
        },
        references.clone(),
    );

    let config = test_config();
    let cache = build_cache_from_index(&index, &root, std::slice::from_ref(&file), &config);
    assert_eq!(cache.files[0].references, references);

    let cache_path = root.join("index.bin");
    save_cache_atomic(&cache_path, &cache).unwrap();

    let loaded = WorkspaceIndex::new();
    let report = load_valid_cached_files(
        &loaded,
        &cache_path,
        &root,
        std::slice::from_ref(&file),
        &config,
    );
    assert_eq!(report.loaded_files, 1);
    assert_eq!(
        loaded
            .file_references
            .get(&uri)
            .map(|entry| entry.value().clone())
            .unwrap_or_default(),
        references
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn cache_invalidates_changed_file_metadata() {
    let root = unique_temp_dir("changed");
    let file = root.join("Foo.php");
    fs::write(&file, "<?php class Foo {}").unwrap();
    let uri = path_to_uri(&file).unwrap();

    let index = WorkspaceIndex::new();
    index.update_file(
        &uri,
        FileSymbols {
            namespace: None,
            use_statements: vec![],
            symbols: vec![make_symbol(&uri)],
            ..Default::default()
        },
    );

    let config = test_config();
    let cache = build_cache_from_index(&index, &root, std::slice::from_ref(&file), &config);
    let cache_path = root.join("index.bin");
    save_cache_atomic(&cache_path, &cache).unwrap();

    let mut handle = fs::OpenOptions::new().append(true).open(&file).unwrap();
    writeln!(handle, "\n// changed").unwrap();

    let loaded = WorkspaceIndex::new();
    let report = load_valid_cached_files(
        &loaded,
        &cache_path,
        &root,
        std::slice::from_ref(&file),
        &config,
    );
    assert_eq!(report.loaded_files, 0);
    assert_eq!(report.stale_files, 1);
    assert_eq!(report.parse_files, vec![file.clone()]);
    assert!(loaded.resolve_fqn("App\\Foo").is_none());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn cache_path_uses_workspace_hash_under_php_lsp_dir() {
    let base = PathBuf::from("/tmp/php-lsp-cache-base");
    let path = cache_file_path_with_base(base.clone(), Path::new("/tmp/project"));
    assert_eq!(
        path.file_name().and_then(|p| p.to_str()),
        Some(CACHE_FILE_NAME)
    );
    assert!(path.starts_with(base.join("php-lsp")));
    assert!(path.ends_with(Path::new("workspace").join(CACHE_FILE_NAME)));
}

#[test]
fn concurrent_saves_to_same_cache_path_do_not_share_temp_file() {
    let root = unique_temp_dir("concurrent-save");
    let file = root.join("src").join("Foo.php");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, "<?php class Foo {}").unwrap();
    let uri = path_to_uri(&file).unwrap();
    let cache_path = root.join("cache").join(CACHE_FILE_NAME);
    let config = test_config();

    let mut handles = Vec::new();
    for _ in 0..8 {
        let root = root.clone();
        let file = file.clone();
        let uri = uri.clone();
        let cache_path = cache_path.clone();
        let config = config.clone();
        handles.push(std::thread::spawn(move || {
            let index = WorkspaceIndex::new();
            index.update_file(
                &uri,
                FileSymbols {
                    namespace: Some("App".to_string()),
                    use_statements: vec![],
                    symbols: vec![make_symbol(&uri)],
                    ..Default::default()
                },
            );
            let cache = build_cache_from_index(&index, &root, &[file], &config);
            save_cache_atomic(&cache_path, &cache)
        }));
    }

    for handle in handles {
        handle.join().unwrap().unwrap();
    }

    let loaded = load_cache(&cache_path).unwrap();
    assert_eq!(loaded.files.len(), 1);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn cache_save_over_existing_file_replaces_previous_snapshot() {
    let root = unique_temp_dir("replace-existing");
    let file = root.join("src").join("Foo.php");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, "<?php class Foo {}").unwrap();
    let uri = path_to_uri(&file).unwrap();
    let cache_path = root.join("cache").join(CACHE_FILE_NAME);
    let config = test_config();

    let first_index = WorkspaceIndex::new();
    first_index.update_file(
        &uri,
        FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![make_symbol(&uri)],
            ..Default::default()
        },
    );
    let first_cache =
        build_cache_from_index(&first_index, &root, std::slice::from_ref(&file), &config);
    save_cache_atomic(&cache_path, &first_cache).unwrap();

    let second_index = WorkspaceIndex::new();
    let mut bar_symbol = make_symbol(&uri);
    bar_symbol.name = "Bar".to_string();
    bar_symbol.fqn = "App\\Bar".to_string();
    second_index.update_file(
        &uri,
        FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![bar_symbol],
            ..Default::default()
        },
    );
    let second_cache =
        build_cache_from_index(&second_index, &root, std::slice::from_ref(&file), &config);
    save_cache_atomic(&cache_path, &second_cache).unwrap();

    let loaded = load_cache(&cache_path).unwrap();
    assert_eq!(loaded.files.len(), 1);
    assert_eq!(loaded.top_level.types[0].fqn, "App\\Bar");
    let leaked_tmp_files: Vec<_> = fs::read_dir(cache_path.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("tmp"))
        .collect();
    assert!(
        leaked_tmp_files.is_empty(),
        "cache replacement should not leave temp files behind: {leaked_tmp_files:?}"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn file_metadata_hash_distinguishes_same_size_content() {
    let root = unique_temp_dir("content-hash");
    let file = root.join("Foo.php");
    fs::write(&file, "<?php class Foo {}").unwrap();
    let first = file_metadata(&file).unwrap();

    fs::write(&file, "<?php class Bar {}").unwrap();
    let second = file_metadata(&file).unwrap();

    assert_eq!(first.size, second.size);
    assert_ne!(first.content_hash, second.content_hash);
    assert_ne!(first, second);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn file_metadata_records_timestamp_fallback_reason_and_keeps_hash_backstop() {
    let unavailable = file_metadata_from_parts(
        b"<?php A",
        7,
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "modified time unavailable",
        )),
    );
    assert_eq!(unavailable.modified_secs, 0);
    assert_eq!(unavailable.modified_nanos, 0);
    assert_eq!(unavailable.modified_status, ModifiedTimeStatus::Unavailable);

    let unavailable_changed = file_metadata_from_parts(
        b"<?php B",
        7,
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "modified time unavailable",
        )),
    );
    assert_eq!(unavailable.size, unavailable_changed.size);
    assert_eq!(
        unavailable.modified_status,
        unavailable_changed.modified_status
    );
    assert_ne!(unavailable.content_hash, unavailable_changed.content_hash);
    assert_ne!(unavailable, unavailable_changed);

    let before_epoch_time = UNIX_EPOCH
        .checked_sub(std::time::Duration::from_secs(1))
        .expect("one second before Unix epoch should be representable");
    let before_epoch = file_metadata_from_parts(b"<?php A", 7, Ok(before_epoch_time));
    assert_eq!(before_epoch.modified_secs, 0);
    assert_eq!(before_epoch.modified_nanos, 0);
    assert_eq!(
        before_epoch.modified_status,
        ModifiedTimeStatus::BeforeUnixEpoch
    );
    assert_ne!(before_epoch, unavailable);

    let available_time = UNIX_EPOCH
        .checked_add(std::time::Duration::new(42, 7))
        .expect("post-epoch timestamp should be representable");
    let available = file_metadata_from_parts(b"<?php A", 7, Ok(available_time));
    assert_eq!(available.modified_secs, 42);
    assert_eq!(available.modified_nanos, 7);
    assert_eq!(available.modified_status, ModifiedTimeStatus::Available);
}

#[test]
fn unix_ms_returns_zero_for_pre_epoch_times() {
    let before_epoch_time = UNIX_EPOCH
        .checked_sub(std::time::Duration::from_millis(1))
        .expect("one millisecond before Unix epoch should be representable");
    assert_eq!(unix_ms(before_epoch_time), 0);

    let available_time = UNIX_EPOCH
        .checked_add(std::time::Duration::from_millis(1500))
        .expect("post-epoch timestamp should be representable");
    assert_eq!(unix_ms(available_time), 1500);
}

#[test]
fn cache_path_uses_separate_namespace_directories() {
    let base = PathBuf::from("/tmp/php-lsp-cache-base");
    let root = Path::new("/tmp/project");
    let workspace =
        cache_file_path_with_base_for_namespace(base.clone(), root, CacheNamespace::Workspace);
    let stubs = cache_file_path_with_base_for_namespace(base.clone(), root, CacheNamespace::Stubs);
    let vendor = cache_file_path_with_base_for_namespace(base, root, CacheNamespace::Vendor);

    assert_ne!(workspace, stubs);
    assert_ne!(workspace, vendor);
    assert_ne!(stubs, vendor);
    assert!(workspace.ends_with(Path::new("workspace").join(CACHE_FILE_NAME)));
    assert!(stubs.ends_with(Path::new("stubs").join(CACHE_FILE_NAME)));
    assert!(vendor.ends_with(Path::new("vendor").join(CACHE_FILE_NAME)));
}

#[test]
fn cache_roundtrip_preserves_encoded_file_uris() {
    let root = unique_temp_dir("encoded-uri");
    let src = root.join("src #1%");
    fs::create_dir_all(&src).unwrap();
    let file = src.join("Привет File.php");
    fs::write(&file, "<?php class Foo {}").unwrap();
    let uri = path_to_uri(&file).unwrap();

    assert!(uri.contains("src%20%231%25"));
    assert!(uri.contains("%D0%9F%D1%80%D0%B8%D0%B2%D0%B5%D1%82%20File.php"));

    let index = WorkspaceIndex::new();
    index.update_file(
        &uri,
        FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![make_symbol(&uri)],
            ..Default::default()
        },
    );

    let config = test_config();
    let cache = build_cache_from_index(&index, &root, std::slice::from_ref(&file), &config);
    assert_eq!(cache.files[0].uri, uri);

    let cache_path = root.join("index.bin");
    save_cache_atomic(&cache_path, &cache).unwrap();

    let loaded = WorkspaceIndex::new();
    let report = load_valid_cached_files(
        &loaded,
        &cache_path,
        &root,
        std::slice::from_ref(&file),
        &config,
    );
    assert_eq!(report.loaded_files, 1);
    assert!(report.parse_files.is_empty());
    assert!(loaded.file_symbols.contains_key(&uri));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn cache_invalidates_legacy_raw_file_uri_for_encoded_path() {
    let root = unique_temp_dir("legacy-uri");
    let src = root.join("src #1%");
    fs::create_dir_all(&src).unwrap();
    let file = src.join("Foo File.php");
    fs::write(&file, "<?php class Foo {}").unwrap();
    let legacy_uri = format!("file://{}", file.display());
    let encoded_uri = path_to_uri(&file).unwrap();
    assert_ne!(legacy_uri, encoded_uri);

    let metadata = file_metadata(&file).unwrap();
    let cache = IndexCache {
        schema_version: CACHE_SCHEMA_VERSION,
        namespace: CacheNamespace::Workspace.as_str().to_string(),
        php_lsp_version: test_config().php_lsp_version,
        workspace_root: normalized_path_string(&root),
        config_hash: test_config().config_hash(),
        stubs_hash: test_config().stubs_hash,
        created_at_unix_ms: 0,
        files: vec![CachedFile {
            uri: legacy_uri.clone(),
            relative_path: relative_cache_path(&root, &file),
            metadata,
            file_symbols: FileSymbols {
                namespace: Some("App".to_string()),
                use_statements: vec![],
                symbols: vec![make_symbol(&legacy_uri)],
                ..Default::default()
            },
            references: Vec::new(),
        }],
        top_level: CachedTopLevelSymbols::default(),
    };

    let cache_path = root.join("index.bin");
    save_cache_atomic(&cache_path, &cache).unwrap();

    let loaded = WorkspaceIndex::new();
    let config = test_config();
    let report = load_valid_cached_files(
        &loaded,
        &cache_path,
        &root,
        std::slice::from_ref(&file),
        &config,
    );

    assert_eq!(report.loaded_files, 0);
    assert_eq!(report.stale_files, 1);
    assert_eq!(report.parse_files, vec![file.clone()]);
    assert!(loaded.file_symbols.get(&legacy_uri).is_none());
    assert!(loaded.file_symbols.get(&encoded_uri).is_none());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn prepared_cache_write_does_not_replace_destination_until_commit() {
    let root = unique_temp_dir("prepared-write");
    fs::create_dir_all(&root).unwrap();
    let cache_path = root.join("index.bin");
    fs::write(&cache_path, b"current").unwrap();
    let cache = IndexCache {
        schema_version: CACHE_SCHEMA_VERSION,
        namespace: CacheNamespace::Workspace.as_str().to_string(),
        php_lsp_version: "test".to_string(),
        workspace_root: normalized_path_string(&root),
        config_hash: 1,
        stubs_hash: 2,
        created_at_unix_ms: 3,
        files: Vec::new(),
        top_level: CachedTopLevelSymbols::default(),
    };

    let prepared = prepare_cache_write(&cache_path, &cache).unwrap();
    assert_eq!(fs::read(&cache_path).unwrap(), b"current");
    drop(prepared);
    assert_eq!(fs::read(&cache_path).unwrap(), b"current");

    prepare_cache_write(&cache_path, &cache)
        .unwrap()
        .commit()
        .unwrap();
    assert_eq!(load_cache(&cache_path).unwrap().config_hash, 1);
    fs::remove_dir_all(root).unwrap();
}
