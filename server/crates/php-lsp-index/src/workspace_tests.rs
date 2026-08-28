use super::*;
use php_lsp_types::*;

fn make_class(name: &str, fqn: &str, uri: &str) -> SymbolInfo {
    SymbolInfo {
        name: name.to_string(),
        fqn: fqn.to_string(),
        kind: PhpSymbolKind::Class,
        uri: uri.to_string(),
        range: (0, 0, 10, 0),
        selection_range: (0, 6, 0, 6 + name.len() as u32),
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

fn make_function(name: &str, fqn: &str, uri: &str) -> SymbolInfo {
    SymbolInfo {
        name: name.to_string(),
        fqn: fqn.to_string(),
        kind: PhpSymbolKind::Function,
        uri: uri.to_string(),
        range: (0, 0, 5, 0),
        selection_range: (0, 9, 0, 9 + name.len() as u32),
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

fn make_method(name: &str, parent_fqn: &str, uri: &str) -> SymbolInfo {
    SymbolInfo {
        name: name.to_string(),
        fqn: format!("{parent_fqn}::{name}"),
        kind: PhpSymbolKind::Method,
        uri: uri.to_string(),
        range: (1, 4, 3, 5),
        selection_range: (1, 20, 1, 20 + name.len() as u32),
        visibility: Visibility::Public,
        modifiers: SymbolModifiers::default(),
        attributes: vec![],
        doc_comment: None,
        signature: None,
        parent_fqn: Some(parent_fqn.to_string()),
        extends: vec![],
        implements: vec![],
        traits: vec![],
        templates: vec![],
        template_bindings: vec![],
    }
}

#[test]
fn test_update_and_resolve() {
    let index = WorkspaceIndex::new();
    let sym = make_class("Foo", "App\\Foo", "file:///test.php");
    let file_symbols = FileSymbols {
        namespace: Some("App".to_string()),
        use_statements: vec![],
        symbols: vec![sym],
        ..Default::default()
    };

    index.update_file("file:///test.php", file_symbols);

    let found = index.resolve_fqn("App\\Foo");
    assert!(found.is_some());
    assert_eq!(found.unwrap().name, "Foo");
}

#[test]
fn test_type_and_function_lookup_is_ascii_case_insensitive_but_constants_are_not() {
    let index = WorkspaceIndex::new();
    let class = make_class("MixedCaseClass", r"App\MixedCaseClass", "file:///case.php");
    let function = make_function(
        "MixedCaseFunction",
        r"App\MixedCaseFunction",
        "file:///case.php",
    );
    let mut constant = make_class(
        "MixedCaseConstant",
        r"App\MixedCaseConstant",
        "file:///case.php",
    );
    constant.kind = PhpSymbolKind::GlobalConstant;

    index.update_file(
        "file:///case.php",
        FileSymbols {
            symbols: vec![class, function, constant],
            ..Default::default()
        },
    );

    assert_eq!(
        index.resolve_fqn(r"app\mixedcaseclass").unwrap().name,
        "MixedCaseClass"
    );
    assert_eq!(
        index.resolve_fqn(r"APP\MIXEDCASEFUNCTION").unwrap().name,
        "MixedCaseFunction"
    );
    assert!(index.contains_type(r"APP\MIXEDCASECLASS"));
    assert_eq!(
        index.resolve_fqn(r"aPP\MixedCaseConstant").unwrap().name,
        "MixedCaseConstant"
    );
    assert!(index.resolve_fqn(r"App\mixedcaseconstant").is_none());
    assert!(index.resolve_fqn(r"App\MixedCaseConstant").is_some());
}

#[test]
fn test_kind_aware_lookup_distinguishes_class_and_function_with_same_fqn() {
    let index = WorkspaceIndex::new();
    let class = make_class("SharedName", r"App\SharedName", "file:///class.php");
    let function = make_function("sHAREDnAME", r"App\sHAREDnAME", "file:///function.php");

    index.update_file(
        "file:///class.php",
        FileSymbols {
            symbols: vec![class],
            ..Default::default()
        },
    );
    index.update_file(
        "file:///function.php",
        FileSymbols {
            symbols: vec![function],
            ..Default::default()
        },
    );

    let class = index
        .resolve_fqn_matching_kinds(r"app\SHAREDNAME", &[PhpSymbolKind::Class])
        .expect("class lookup should use the type symbol table");
    assert_eq!(class.name, "SharedName");

    let function = index
        .resolve_fqn_matching_kinds(r"APP\sharedname", &[PhpSymbolKind::Function])
        .expect("function lookup should use the function symbol table");
    assert_eq!(function.name, "sHAREDnAME");
}

#[test]
fn test_member_lookup_accepts_owner_casing_without_relaxing_properties() {
    let index = WorkspaceIndex::new();
    let class = make_class("Owner", r"App\Owner", "file:///owner.php");
    let method = make_method("MixedCaseMethod", r"App\Owner", "file:///owner.php");
    let mut property = make_method("MixedCaseProperty", r"App\Owner", "file:///owner.php");
    property.kind = PhpSymbolKind::Property;
    property.fqn = r"App\Owner::$MixedCaseProperty".to_string();

    index.update_file(
        "file:///owner.php",
        FileSymbols {
            symbols: vec![class, method, property],
            ..Default::default()
        },
    );

    assert!(index.resolve_fqn(r"app\owner::MIXEDCASEMETHOD").is_some());
    assert!(index
        .resolve_fqn(r"app\owner::$MixedCaseProperty")
        .is_some());
    assert!(index
        .resolve_fqn(r"app\owner::$mixedcaseproperty")
        .is_none());
}

#[test]
fn test_remove_file() {
    let index = WorkspaceIndex::new();
    let sym = make_class("Foo", "App\\Foo", "file:///test.php");
    let file_symbols = FileSymbols {
        namespace: Some("App".to_string()),
        use_statements: vec![],
        symbols: vec![sym],
        ..Default::default()
    };

    index.update_file("file:///test.php", file_symbols);
    assert!(index.resolve_fqn("App\\Foo").is_some());

    index.remove_file("file:///test.php");
    assert!(index.resolve_fqn("App\\Foo").is_none());
}

#[test]
fn test_remove_file_preserves_duplicate_fqn_from_other_file() {
    let index = WorkspaceIndex::new();
    let file_a = FileSymbols {
        namespace: Some("App".to_string()),
        use_statements: vec![],
        symbols: vec![make_class("Foo", "App\\Foo", "file:///a.php")],
        ..Default::default()
    };
    let file_b = FileSymbols {
        namespace: Some("App".to_string()),
        use_statements: vec![],
        symbols: vec![make_class("Foo", "App\\Foo", "file:///b.php")],
        ..Default::default()
    };

    index.update_file("file:///a.php", file_a);
    index.update_file("file:///b.php", file_b);

    index.remove_file("file:///a.php");

    let found = index
        .resolve_fqn("App\\Foo")
        .expect("duplicate FQN remains");
    assert_eq!(found.uri, "file:///b.php");
}

#[test]
fn test_remove_file_restores_mixed_kind_class_like_symbol_with_equivalent_fqn() {
    let index = WorkspaceIndex::new();
    let class = make_class("Thing", "App\\Thing", "file:///class.php");
    let mut interface = make_class("THING", "App\\THING", "file:///interface.php");
    interface.kind = PhpSymbolKind::Interface;

    index.update_file(
        "file:///class.php",
        FileSymbols {
            namespace: Some("App".to_string()),
            symbols: vec![class],
            ..Default::default()
        },
    );
    index.update_file(
        "file:///interface.php",
        FileSymbols {
            namespace: Some("App".to_string()),
            symbols: vec![interface],
            ..Default::default()
        },
    );

    assert_eq!(
        index.resolve_fqn("app\\thing").map(|symbol| symbol.kind),
        Some(PhpSymbolKind::Interface)
    );

    index.remove_file("file:///interface.php");

    let restored = index
        .resolve_fqn("APP\\THING")
        .expect("class-like symbol from the remaining file must be restored");
    assert_eq!(restored.kind, PhpSymbolKind::Class);
    assert_eq!(restored.uri, "file:///class.php");
}

#[test]
fn test_search() {
    let index = WorkspaceIndex::new();
    let file_symbols = FileSymbols {
        namespace: Some("App".to_string()),
        use_statements: vec![],
        symbols: vec![
            make_class("FooController", "App\\FooController", "file:///a.php"),
            make_class("BarService", "App\\BarService", "file:///a.php"),
            make_function("helper_foo", "App\\helper_foo", "file:///a.php"),
        ],
        ..Default::default()
    };

    index.update_file("file:///a.php", file_symbols);

    let results = index.search("foo");
    assert_eq!(results.len(), 2); // FooController + helper_foo
}

#[test]
fn test_update_replaces_old() {
    let index = WorkspaceIndex::new();

    let sym_v1 = FileSymbols {
        namespace: None,
        use_statements: vec![],
        symbols: vec![make_class("Foo", "Foo", "file:///test.php")],
        ..Default::default()
    };
    index.update_file("file:///test.php", sym_v1);
    assert!(index.resolve_fqn("Foo").is_some());

    let sym_v2 = FileSymbols {
        namespace: None,
        use_statements: vec![],
        symbols: vec![make_class("Bar", "Bar", "file:///test.php")],
        ..Default::default()
    };
    index.update_file("file:///test.php", sym_v2);
    assert!(index.resolve_fqn("Foo").is_none());
    assert!(index.resolve_fqn("Bar").is_some());
}

#[test]
fn replacement_top_level_key_scan_ignores_large_member_volume() {
    let uri = "file:///large-member-generation.php";
    let mut symbols = vec![make_class("Owner", "App\\Owner", uri)];
    symbols
        .extend((0..10_000).map(|index| make_method(&format!("method{index}"), "App\\Owner", uri)));
    let file_symbols = FileSymbols {
        symbols,
        ..Default::default()
    };

    let keys = top_level_generation_keys(&file_symbols);
    assert_eq!(
        keys.len(),
        1,
        "replacement diff must hash one class key rather than compare every member"
    );
    assert!(keys.contains(&(
        TopLevelSymbolTable::Types,
        case_insensitive_fqn_key("App\\Owner")
    )));

    let index = WorkspaceIndex::new();
    index.update_file(uri, file_symbols);
    index.update_file(
        uri,
        FileSymbols {
            symbols: vec![make_class("Replacement", "App\\Replacement", uri)],
            ..Default::default()
        },
    );
    assert!(index.resolve_fqn("App\\Owner").is_none());
    assert!(index.resolve_fqn("App\\Replacement").is_some());
}

#[test]
fn direct_member_index_replaces_the_previous_file_generation() {
    let index = WorkspaceIndex::new();
    let uri = "file:///members.php";
    index.update_file(
        uri,
        FileSymbols {
            symbols: vec![
                make_class("Owner", "App\\Owner", uri),
                make_method("oldMember", "App\\Owner", uri),
            ],
            ..Default::default()
        },
    );

    assert!(index.resolve_fqn("App\\Owner::oldMember").is_some());

    index.update_file(
        uri,
        FileSymbols {
            symbols: vec![
                make_class("Owner", "App\\Owner", uri),
                make_method("newMember", "App\\Owner", uri),
            ],
            ..Default::default()
        },
    );

    assert!(index.resolve_fqn("App\\Owner::oldMember").is_none());
    assert!(index.resolve_fqn("App\\Owner::newMember").is_some());
    let sources = index
        .direct_members_by_parent
        .get(&case_insensitive_fqn_key("App\\Owner"))
        .map(|entry| Arc::clone(entry.value()))
        .expect("direct-member bucket");
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].uri.as_ref(), uri);
    assert_eq!(sources[0].symbol_indices.as_ref(), &[1]);
    let file_symbols = index.file_symbols.get(uri).expect("indexed file snapshot");
    assert!(Arc::ptr_eq(file_symbols.value(), &sources[0].file_symbols));
    assert!(index
        .file_update_generations
        .get(uri)
        .is_some_and(|generation| *generation > 0));
}

#[test]
fn removing_file_preserves_direct_members_from_duplicate_parent_in_another_file() {
    let index = WorkspaceIndex::new();
    let first_uri = "file:///first.php";
    let second_uri = "file:///second.php";
    index.update_file(
        first_uri,
        FileSymbols {
            symbols: vec![make_method("fromFirst", "App\\Shared", first_uri)],
            ..Default::default()
        },
    );
    index.update_file(
        second_uri,
        FileSymbols {
            symbols: vec![make_method("fromSecond", "App\\Shared", second_uri)],
            ..Default::default()
        },
    );

    let mut names = index
        .get_direct_members("app\\shared")
        .into_iter()
        .map(|member| member.name.clone())
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(names, vec!["fromFirst", "fromSecond"]);

    index.remove_file(first_uri);

    let remaining = index.get_direct_members("APP\\SHARED");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].name, "fromSecond");
    assert_eq!(remaining[0].uri, second_uri);

    index.remove_file(second_uri);
    assert!(!index
        .direct_members_by_parent
        .contains_key(&case_insensitive_fqn_key("App\\Shared")));
}

#[test]
fn concurrent_member_writers_are_serialized_and_readers_keep_an_immutable_snapshot() {
    let index = Arc::new(WorkspaceIndex::new());
    let uri = "file:///concurrent-members.php";
    index.update_file(
        uri,
        FileSymbols {
            symbols: vec![
                make_class("Owner", "App\\Owner", uri),
                make_method("oldOne", "App\\Owner", uri),
                make_method("oldTwo", "App\\Owner", uri),
            ],
            ..Default::default()
        },
    );

    let writer_a_index = Arc::clone(&index);
    let (writer_a_staged_tx, writer_a_staged_rx) = std::sync::mpsc::channel();
    let (writer_a_release_tx, writer_a_release_rx) = std::sync::mpsc::channel();
    let writer_a = std::thread::spawn(move || {
        writer_a_index.update_file_with_references_with_hook(
            uri,
            FileSymbols {
                symbols: vec![
                    make_class("Owner", "App\\Owner", uri),
                    make_method("fromWriterA", "App\\Owner", uri),
                ],
                ..Default::default()
            },
            Vec::new(),
            || {
                writer_a_staged_tx.send(()).expect("report staged writer A");
                writer_a_release_rx.recv().expect("release writer A");
            },
        );
    });

    writer_a_staged_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("writer A reached the staged snapshot");

    let writer_b_index = Arc::clone(&index);
    let (writer_b_started_tx, writer_b_started_rx) = std::sync::mpsc::channel();
    let (writer_b_done_tx, writer_b_done_rx) = std::sync::mpsc::channel();
    let writer_b = std::thread::spawn(move || {
        writer_b_started_tx.send(()).expect("report writer B start");
        writer_b_index.update_file(
            uri,
            FileSymbols {
                symbols: vec![
                    make_class("Owner", "App\\Owner", uri),
                    make_method("fromWriterB", "App\\Owner", uri),
                ],
                ..Default::default()
            },
        );
        writer_b_done_tx
            .send(())
            .expect("report writer B completion");
    });
    writer_b_started_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("writer B started");
    assert!(
        writer_b_done_rx
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err(),
        "writer B must wait for writer A's per-URI commit"
    );

    let reader_index = Arc::clone(&index);
    let (reader_tx, reader_rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut names = reader_index
            .get_direct_members("App\\Owner")
            .into_iter()
            .map(|member| member.name.clone())
            .collect::<Vec<_>>();
        names.sort();
        reader_tx.send(names).expect("return member snapshot");
    });
    assert_eq!(
        reader_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("reader should not wait for an in-progress replacement"),
        vec!["oldOne", "oldTwo"]
    );
    reader.join().expect("member reader joined");

    writer_a_release_tx.send(()).expect("release writer A");
    writer_a.join().expect("writer A joined");
    writer_b_done_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("writer B completed after writer A");
    writer_b.join().expect("writer B joined");

    let names = index
        .get_direct_members("App\\Owner")
        .into_iter()
        .map(|member| member.name.clone())
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["fromWriterB"]);
}

#[test]
fn member_resolution_waits_for_first_file_generation_commit() {
    let index = Arc::new(WorkspaceIndex::new());
    let uri = "file:///first-generation.php";
    let writer_index = Arc::clone(&index);
    let (staged_tx, staged_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        writer_index.update_file_with_references_with_hook(
            uri,
            FileSymbols {
                symbols: vec![
                    make_class("Owner", "App\\Owner", uri),
                    make_method("loaded", "App\\Owner", uri),
                ],
                ..Default::default()
            },
            Vec::new(),
            || {
                staged_tx.send(()).expect("report staged first generation");
                release_rx.recv().expect("release first generation");
            },
        );
    });

    staged_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("first generation reached staged publication");
    assert!(index.contains_type("App\\Owner"));
    assert!(
        index.get_direct_members("App\\Owner").is_empty(),
        "the deterministic hook must expose the pre-fix publication gap"
    );

    let reader_index = Arc::clone(&index);
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        result_tx
            .send(reader_index.resolve_member("App\\Owner::loaded"))
            .expect("return committed member resolution");
    });
    assert!(
        result_rx
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err(),
        "member resolution must wait for the owning file generation"
    );

    release_tx.send(()).expect("release first generation");
    writer.join().expect("first-generation writer joined");
    let resolved = result_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("member resolution completed after commit")
        .expect("committed member");
    reader.join().expect("member reader joined");
    assert_eq!(resolved.fqn, "App\\Owner::loaded");
}

#[test]
fn member_resolution_does_not_mix_replacement_generations() {
    let index = Arc::new(WorkspaceIndex::new());
    let uri = "file:///replacement-generation.php";
    index.update_file(
        uri,
        FileSymbols {
            symbols: vec![
                make_class("Owner", "App\\Owner", uri),
                make_method("oldMember", "App\\Owner", uri),
            ],
            ..Default::default()
        },
    );

    let writer_index = Arc::clone(&index);
    let (staged_tx, staged_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        writer_index.update_file_with_references_with_hook(
            uri,
            FileSymbols {
                symbols: vec![
                    make_class("Owner", "App\\Owner", uri),
                    make_method("newMember", "App\\Owner", uri),
                ],
                ..Default::default()
            },
            Vec::new(),
            || {
                staged_tx.send(()).expect("report staged replacement");
                release_rx.recv().expect("release replacement");
            },
        );
    });
    staged_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("replacement reached staged publication");

    let reader_index = Arc::clone(&index);
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        result_tx
            .send(reader_index.resolve_member("App\\Owner::newMember"))
            .expect("return replacement member resolution");
    });
    assert!(
        result_rx
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err(),
        "resolution must not combine the new type with old members"
    );

    release_tx.send(()).expect("release replacement");
    writer.join().expect("replacement writer joined");
    let resolved = result_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("replacement resolution completed")
        .expect("new generation member");
    reader.join().expect("replacement reader joined");
    assert_eq!(resolved.fqn, "App\\Owner::newMember");
    assert!(index.resolve_member("App\\Owner::oldMember").is_none());
}

#[test]
fn member_resolution_waits_before_replacement_top_level_publish() {
    let index = Arc::new(WorkspaceIndex::new());
    let uri = "file:///pre-publish-replacement.php";
    index.update_file(
        uri,
        FileSymbols {
            symbols: vec![
                make_class("Owner", "App\\Owner", uri),
                make_method("oldMember", "App\\Owner", uri),
            ],
            ..Default::default()
        },
    );

    let writer_index = Arc::clone(&index);
    let (staged_tx, staged_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        writer_index.update_file_with_references_with_hooks(
            uri,
            FileSymbols {
                symbols: vec![
                    make_class("Owner", "App\\Owner", uri),
                    make_method("newMember", "App\\Owner", uri),
                ],
                ..Default::default()
            },
            Vec::new(),
            || {
                staged_tx
                    .send(())
                    .expect("report replacement before top-level publish");
                release_rx
                    .recv()
                    .expect("release replacement top-level publish");
            },
            || {},
        );
    });
    staged_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("replacement paused before top-level publish");
    assert_eq!(
        index
            .get_type("App\\Owner")
            .map(|symbol| symbol.uri.clone()),
        Some(uri.to_string()),
        "old committed type must stay published while its replacement is staged"
    );

    let reader_index = Arc::clone(&index);
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        result_tx
            .send(reader_index.resolve_member("App\\Owner::newMember"))
            .expect("return pre-publish replacement resolution");
    });
    assert!(
        result_rx
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err(),
        "reader must wait rather than return transient None before type reinsertion"
    );

    release_tx
        .send(())
        .expect("release replacement top-level publish");
    writer.join().expect("pre-publish writer joined");
    let resolved = result_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("resolution completed after top-level publish")
        .expect("replacement member resolved");
    reader.join().expect("pre-publish reader joined");
    assert_eq!(resolved.fqn, "App\\Owner::newMember");
}

#[test]
fn committed_type_generation_detects_replacement_and_removal() {
    let index = WorkspaceIndex::new();
    let uri = "file:///generation-validation.php";
    index.update_file(
        uri,
        FileSymbols {
            symbols: vec![make_class("Owner", "App\\Owner", uri)],
            ..Default::default()
        },
    );
    let first = index
        .get_committed_type("App\\Owner")
        .expect("first committed generation");
    assert!(index.type_generation_is_current(&first.generation));

    index.update_file(
        uri,
        FileSymbols {
            symbols: vec![
                make_class("Owner", "App\\Owner", uri),
                make_method("newMember", "App\\Owner", uri),
            ],
            ..Default::default()
        },
    );
    assert!(!index.type_generation_is_current(&first.generation));
    let second = index
        .get_committed_type("App\\Owner")
        .expect("replacement committed generation");
    assert!(second.generation.generation() > first.generation.generation());
    assert!(index.type_generation_is_current(&second.generation));

    index.remove_file(uri);
    assert!(!index.type_generation_is_current(&second.generation));
}

#[test]
fn test_resolve_member() {
    let index = WorkspaceIndex::new();
    let class_sym = make_class("Foo", "App\\Foo", "file:///test.php");
    let method_sym = SymbolInfo {
        name: "increment".to_string(),
        fqn: "App\\Foo::increment".to_string(),
        kind: PhpSymbolKind::Method,
        uri: "file:///test.php".to_string(),
        range: (10, 0, 15, 0),
        selection_range: (10, 20, 10, 29),
        visibility: Visibility::Public,
        modifiers: SymbolModifiers::default(),
        attributes: vec![],
        doc_comment: None,
        signature: None,
        parent_fqn: Some("App\\Foo".to_string()),
        extends: vec![],
        implements: vec![],
        traits: vec![],
        templates: vec![],
        template_bindings: vec![],
    };
    let file_symbols = FileSymbols {
        namespace: Some("App".to_string()),
        use_statements: vec![],
        symbols: vec![class_sym, method_sym],
        ..Default::default()
    };
    index.update_file("file:///test.php", file_symbols);

    // resolve_fqn should find the class
    let found = index.resolve_fqn("App\\Foo");
    assert!(found.is_some());
    assert_eq!(found.unwrap().name, "Foo");

    // resolve_fqn should also find the method via Class::member
    let found = index.resolve_fqn("App\\Foo::increment");
    assert!(found.is_some());
    let method = found.unwrap();
    assert_eq!(method.name, "increment");
    assert_eq!(method.kind, PhpSymbolKind::Method);

    // Non-existent member should return None
    assert!(index.resolve_fqn("App\\Foo::nonexistent").is_none());
}

#[test]
fn test_resolve_method_names_case_insensitively_only() {
    let index = WorkspaceIndex::new();
    let uri = "file:///test.php";
    let class_sym = make_class("Foo", "App\\Foo", uri);
    let method_sym = make_method("propFind", "App\\Foo", uri);
    let mut conflicting_property_sym = make_method("propfind", "App\\Foo", uri);
    conflicting_property_sym.kind = PhpSymbolKind::Property;
    conflicting_property_sym.fqn = "App\\Foo::$propfind".to_string();
    let mut property_sym = make_method("PortingNumber", "App\\Foo", uri);
    property_sym.kind = PhpSymbolKind::Property;
    property_sym.fqn = "App\\Foo::$PortingNumber".to_string();
    let mut constant_sym = make_method("STATE_READY", "App\\Foo", uri);
    constant_sym.kind = PhpSymbolKind::ClassConstant;
    constant_sym.fqn = "App\\Foo::STATE_READY".to_string();

    index.update_file(
        uri,
        FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![
                class_sym,
                conflicting_property_sym,
                method_sym,
                property_sym,
                constant_sym,
            ],
            ..Default::default()
        },
    );

    let found = index
        .resolve_member_matching_kinds("App\\Foo::propfind", &[PhpSymbolKind::Method])
        .expect("PHP method lookup should ignore ASCII case");
    assert_eq!(found.name, "propFind");
    assert_eq!(found.kind, PhpSymbolKind::Method);

    assert!(
        index
            .resolve_member_matching_kinds("App\\Foo::$propfind", &[PhpSymbolKind::Property])
            .is_some(),
        "Kind-aware lookup should still find the exact property"
    );

    assert!(
        index
            .resolve_member_matching_kinds("App\\Foo::$portingnumber", &[PhpSymbolKind::Property])
            .is_none(),
        "PHP property lookup must remain case-sensitive"
    );
    assert!(
            index
                .resolve_member_matching_kinds(
                    "App\\Foo::state_ready",
                    &[PhpSymbolKind::ClassConstant],
                )
                .is_none(),
            "PHP class constant lookup must remain case-sensitive"
        );
}

#[test]
fn test_resolve_inherited_member() {
    let index = WorkspaceIndex::new();

    // Parent class with a method
    let parent_class = SymbolInfo {
        name: "SoapHandler".to_string(),
        fqn: "App\\SoapHandler".to_string(),
        kind: PhpSymbolKind::Class,
        uri: "file:///parent.php".to_string(),
        range: (0, 0, 20, 0),
        selection_range: (0, 6, 0, 17),
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
    };
    let parent_method = SymbolInfo {
        name: "okResponse".to_string(),
        fqn: "App\\SoapHandler::okResponse".to_string(),
        kind: PhpSymbolKind::Method,
        uri: "file:///parent.php".to_string(),
        range: (5, 4, 8, 5),
        selection_range: (5, 20, 5, 30),
        visibility: Visibility::Protected,
        modifiers: SymbolModifiers::default(),
        attributes: vec![],
        doc_comment: None,
        signature: None,
        parent_fqn: Some("App\\SoapHandler".to_string()),
        extends: vec![],
        implements: vec![],
        traits: vec![],
        templates: vec![],
        template_bindings: vec![],
    };
    let parent_file = FileSymbols {
        namespace: Some("App".to_string()),
        use_statements: vec![],
        symbols: vec![parent_class, parent_method],
        ..Default::default()
    };
    index.update_file("file:///parent.php", parent_file);

    // Child class that extends the parent
    let child_class = SymbolInfo {
        name: "TestHandler".to_string(),
        fqn: "App\\TestHandler".to_string(),
        kind: PhpSymbolKind::Class,
        uri: "file:///child.php".to_string(),
        range: (0, 0, 5, 0),
        selection_range: (0, 6, 0, 17),
        visibility: Visibility::Public,
        modifiers: SymbolModifiers::default(),
        attributes: vec![],
        doc_comment: None,
        signature: None,
        parent_fqn: None,
        extends: vec!["App\\SoapHandler".to_string()],
        implements: vec![],
        traits: vec![],
        templates: vec![],
        template_bindings: vec![],
    };
    let child_file = FileSymbols {
        namespace: Some("App".to_string()),
        use_statements: vec![],
        symbols: vec![child_class],
        ..Default::default()
    };
    index.update_file("file:///child.php", child_file);

    // Resolving TestHandler::okResponse should find the parent's method
    let found = index.resolve_fqn("App\\TestHandler::okResponse");
    assert!(found.is_some(), "should resolve inherited member");
    let method = found.unwrap();
    assert_eq!(method.name, "okResponse");
    assert_eq!(method.fqn, "App\\SoapHandler::okResponse");

    // get_members should include inherited members
    let members = index.get_members("App\\TestHandler");
    assert!(
        members.iter().any(|m| m.name == "okResponse"),
        "inherited method should be in get_members"
    );
}

#[test]
fn test_resolve_member_inherited_through_interface_extends_chain() {
    let index = WorkspaceIndex::new();

    let mut base_interface = make_class("BaseForm", "Vendor\\BaseForm", "file:///base.php");
    base_interface.kind = PhpSymbolKind::Interface;
    let base_method = make_method("handleRequest", "Vendor\\BaseForm", "file:///base.php");
    index.update_file(
        "file:///base.php",
        FileSymbols {
            namespace: Some("Vendor".to_string()),
            use_statements: vec![],
            symbols: vec![base_interface, base_method],
            ..Default::default()
        },
    );

    let mut flow_interface = make_class("FlowForm", "Vendor\\FlowForm", "file:///flow.php");
    flow_interface.kind = PhpSymbolKind::Interface;
    flow_interface.extends = vec!["Vendor\\BaseForm".to_string()];
    index.update_file(
        "file:///flow.php",
        FileSymbols {
            namespace: Some("Vendor".to_string()),
            use_statements: vec![],
            symbols: vec![flow_interface],
            ..Default::default()
        },
    );

    let found = index
        .resolve_fqn("Vendor\\FlowForm::handleRequest")
        .expect("interface should inherit members through extends");
    assert_eq!(found.fqn, "Vendor\\BaseForm::handleRequest");

    let members = index.get_members("Vendor\\FlowForm");
    assert!(
        members.iter().any(|member| member.name == "handleRequest"),
        "interface-extended method should be included in get_members"
    );
}

#[test]
fn test_resolve_trait_member() {
    let index = WorkspaceIndex::new();

    let trait_sym = SymbolInfo {
        name: "Assertions".to_string(),
        fqn: "App\\Assertions".to_string(),
        kind: PhpSymbolKind::Trait,
        uri: "file:///trait.php".to_string(),
        range: (0, 0, 10, 0),
        selection_range: (0, 6, 0, 16),
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
    };
    let trait_method = SymbolInfo {
        name: "assertOk".to_string(),
        fqn: "App\\Assertions::assertOk".to_string(),
        kind: PhpSymbolKind::Method,
        uri: "file:///trait.php".to_string(),
        range: (2, 4, 4, 5),
        selection_range: (2, 20, 2, 28),
        visibility: Visibility::Public,
        modifiers: SymbolModifiers::default(),
        attributes: vec![],
        doc_comment: None,
        signature: None,
        parent_fqn: Some("App\\Assertions".to_string()),
        extends: vec![],
        implements: vec![],
        traits: vec![],
        templates: vec![],
        template_bindings: vec![],
    };
    index.update_file(
        "file:///trait.php",
        FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![trait_sym, trait_method],
            ..Default::default()
        },
    );

    let class_sym = SymbolInfo {
        name: "TestCase".to_string(),
        fqn: "App\\TestCase".to_string(),
        kind: PhpSymbolKind::Class,
        uri: "file:///class.php".to_string(),
        range: (0, 0, 5, 0),
        selection_range: (0, 6, 0, 14),
        visibility: Visibility::Public,
        modifiers: SymbolModifiers::default(),
        attributes: vec![],
        doc_comment: None,
        signature: None,
        parent_fqn: None,
        extends: vec![],
        implements: vec![],
        traits: vec!["App\\Assertions".to_string()],
        templates: vec![],
        template_bindings: vec![],
    };
    index.update_file(
        "file:///class.php",
        FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![class_sym],
            ..Default::default()
        },
    );

    let found = index.resolve_fqn("App\\TestCase::assertOk");
    assert!(found.is_some(), "should resolve methods mixed in by traits");
    assert_eq!(found.unwrap().fqn, "App\\Assertions::assertOk");
}

#[test]
fn test_resolve_member_no_infinite_loop() {
    let index = WorkspaceIndex::new();

    // Two classes that extend each other (pathological case)
    let class_a = SymbolInfo {
        name: "A".to_string(),
        fqn: "A".to_string(),
        kind: PhpSymbolKind::Class,
        uri: "file:///a.php".to_string(),
        range: (0, 0, 5, 0),
        selection_range: (0, 6, 0, 7),
        visibility: Visibility::Public,
        modifiers: SymbolModifiers::default(),
        attributes: vec![],
        doc_comment: None,
        signature: None,
        parent_fqn: None,
        extends: vec!["B".to_string()],
        implements: vec![],
        traits: vec![],
        templates: vec![],
        template_bindings: vec![],
    };
    let class_b = SymbolInfo {
        name: "B".to_string(),
        fqn: "B".to_string(),
        kind: PhpSymbolKind::Class,
        uri: "file:///b.php".to_string(),
        range: (0, 0, 5, 0),
        selection_range: (0, 6, 0, 7),
        visibility: Visibility::Public,
        modifiers: SymbolModifiers::default(),
        attributes: vec![],
        doc_comment: None,
        signature: None,
        parent_fqn: None,
        extends: vec!["A".to_string()],
        implements: vec![],
        traits: vec![],
        templates: vec![],
        template_bindings: vec![],
    };
    let file_a = FileSymbols {
        namespace: None,
        use_statements: vec![],
        symbols: vec![class_a],
        ..Default::default()
    };
    let file_b = FileSymbols {
        namespace: None,
        use_statements: vec![],
        symbols: vec![class_b],
        ..Default::default()
    };
    index.update_file("file:///a.php", file_a);
    index.update_file("file:///b.php", file_b);

    // Should not hang — just return None
    assert!(index.resolve_fqn("A::nonexistent").is_none());
}

#[test]
fn test_hierarchy_visited_sets_handle_trait_mixin_and_parent_cycles() {
    let index = WorkspaceIndex::new();

    let mut root = make_class("Root", "App\\Root", "file:///hierarchy.php");
    root.extends = vec!["App\\Parent".to_string()];
    root.traits = vec!["App\\SharedTrait".to_string()];
    root.template_bindings = vec![TemplateBinding {
        kind: TemplateBindingKind::Mixin,
        target: "App\\Mixin".to_string(),
        args: vec![],
    }];

    let mut parent = make_class("Parent", "App\\Parent", "file:///hierarchy.php");
    parent.extends = vec!["App\\Root".to_string()];

    let mut trait_sym = make_class("SharedTrait", "App\\SharedTrait", "file:///hierarchy.php");
    trait_sym.kind = PhpSymbolKind::Trait;
    trait_sym.traits = vec!["App\\Root".to_string()];

    let mut mixin = make_class("Mixin", "App\\Mixin", "file:///hierarchy.php");
    mixin.extends = vec!["App\\Root".to_string()];

    index.update_file(
        "file:///hierarchy.php",
        FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![
                root,
                parent,
                make_method("parentMethod", "App\\Parent", "file:///hierarchy.php"),
                trait_sym,
                make_method("traitMethod", "App\\SharedTrait", "file:///hierarchy.php"),
                mixin,
                make_method("mixinMethod", "App\\Mixin", "file:///hierarchy.php"),
            ],
            ..Default::default()
        },
    );

    assert_eq!(
        index
            .resolve_fqn("App\\Root::traitMethod")
            .map(|sym| sym.fqn.clone())
            .as_deref(),
        Some("App\\SharedTrait::traitMethod")
    );
    assert_eq!(
        index
            .resolve_fqn("App\\Root::mixinMethod")
            .map(|sym| sym.fqn.clone())
            .as_deref(),
        Some("App\\Mixin::mixinMethod")
    );
    assert_eq!(
        index
            .resolve_fqn("App\\Root::parentMethod")
            .map(|sym| sym.fqn.clone())
            .as_deref(),
        Some("App\\Parent::parentMethod")
    );
    assert!(index.resolve_fqn("App\\Root::missing").is_none());

    let member_names = index
        .get_members("App\\Root")
        .into_iter()
        .map(|member| member.name.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        member_names,
        vec!["traitMethod", "mixinMethod", "parentMethod"]
    );

    let hierarchy_fqns = index
        .get_type_hierarchy_symbols("App\\Root")
        .into_iter()
        .map(|symbol| symbol.fqn.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        hierarchy_fqns,
        vec!["App\\Root", "App\\SharedTrait", "App\\Parent"]
    );
}

#[test]
fn test_resolve_inherited_member_after_incremental_load() {
    // Simulates vendor lazy-loading: child class is indexed first,
    // parent is added later. After parent is indexed, inherited
    // member resolution should work.
    let index = WorkspaceIndex::new();

    // Step 1: Index child class (extends a parent not yet indexed)
    let child_class = SymbolInfo {
        name: "MyTest".to_string(),
        fqn: "App\\MyTest".to_string(),
        kind: PhpSymbolKind::Class,
        uri: "file:///tests/MyTest.php".to_string(),
        range: (0, 0, 10, 0),
        selection_range: (0, 6, 0, 12),
        visibility: Visibility::Public,
        modifiers: SymbolModifiers::default(),
        attributes: vec![],
        doc_comment: None,
        signature: None,
        parent_fqn: None,
        extends: vec!["Vendor\\TestCase".to_string()],
        implements: vec![],
        traits: vec![],
        templates: vec![],
        template_bindings: vec![],
    };
    let child_file = FileSymbols {
        namespace: Some("App".to_string()),
        use_statements: vec![],
        symbols: vec![child_class],
        ..Default::default()
    };
    index.update_file("file:///tests/MyTest.php", child_file);

    // Before parent is loaded, inherited member should NOT resolve
    assert!(
        index.resolve_fqn("App\\MyTest::doSetUp").is_none(),
        "member should not resolve before parent is indexed"
    );

    // Step 2: Index parent class (vendor lazy-load simulation)
    let parent_class = SymbolInfo {
        name: "TestCase".to_string(),
        fqn: "Vendor\\TestCase".to_string(),
        kind: PhpSymbolKind::Class,
        uri: "file:///vendor/TestCase.php".to_string(),
        range: (0, 0, 20, 0),
        selection_range: (0, 6, 0, 14),
        visibility: Visibility::Public,
        modifiers: SymbolModifiers::default(),
        attributes: vec![],
        doc_comment: None,
        signature: None,
        parent_fqn: None,
        extends: vec!["Vendor\\BaseAssert".to_string()],
        implements: vec![],
        traits: vec![],
        templates: vec![],
        template_bindings: vec![],
    };
    let parent_method = SymbolInfo {
        name: "doSetUp".to_string(),
        fqn: "Vendor\\TestCase::doSetUp".to_string(),
        kind: PhpSymbolKind::Method,
        uri: "file:///vendor/TestCase.php".to_string(),
        range: (5, 4, 8, 5),
        selection_range: (5, 20, 5, 27),
        visibility: Visibility::Protected,
        modifiers: SymbolModifiers::default(),
        attributes: vec![],
        doc_comment: None,
        signature: None,
        parent_fqn: Some("Vendor\\TestCase".to_string()),
        extends: vec![],
        implements: vec![],
        traits: vec![],
        templates: vec![],
        template_bindings: vec![],
    };
    let parent_file = FileSymbols {
        namespace: Some("Vendor".to_string()),
        use_statements: vec![],
        symbols: vec![parent_class, parent_method],
        ..Default::default()
    };
    index.update_file("file:///vendor/TestCase.php", parent_file);

    // After parent is indexed, inherited member SHOULD resolve
    let found = index.resolve_fqn("App\\MyTest::doSetUp");
    assert!(
        found.is_some(),
        "member should resolve after parent is indexed"
    );
    assert_eq!(found.unwrap().name, "doSetUp");

    // Step 3: Index grandparent class (deeper vendor lazy-load)
    let gp_class = SymbolInfo {
        name: "BaseAssert".to_string(),
        fqn: "Vendor\\BaseAssert".to_string(),
        kind: PhpSymbolKind::Class,
        uri: "file:///vendor/BaseAssert.php".to_string(),
        range: (0, 0, 30, 0),
        selection_range: (0, 6, 0, 16),
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
    };
    let gp_method = SymbolInfo {
        name: "createStub".to_string(),
        fqn: "Vendor\\BaseAssert::createStub".to_string(),
        kind: PhpSymbolKind::Method,
        uri: "file:///vendor/BaseAssert.php".to_string(),
        range: (10, 4, 13, 5),
        selection_range: (10, 20, 10, 30),
        visibility: Visibility::Public,
        modifiers: SymbolModifiers::default(),
        attributes: vec![],
        doc_comment: None,
        signature: None,
        parent_fqn: Some("Vendor\\BaseAssert".to_string()),
        extends: vec![],
        implements: vec![],
        traits: vec![],
        templates: vec![],
        template_bindings: vec![],
    };
    let gp_file = FileSymbols {
        namespace: Some("Vendor".to_string()),
        use_statements: vec![],
        symbols: vec![gp_class, gp_method],
        ..Default::default()
    };
    index.update_file("file:///vendor/BaseAssert.php", gp_file);

    // Grandparent method should now resolve through the full chain
    let found = index.resolve_fqn("App\\MyTest::createStub");
    assert!(
        found.is_some(),
        "grandparent method should resolve through inheritance chain"
    );
    assert_eq!(found.unwrap().name, "createStub");
}

#[test]
fn test_template_substitution_for_generic_repository_method() {
    let index = WorkspaceIndex::new();

    let mut repository = make_class("Repository", "App\\Repository", "file:///repo.php");
    repository.kind = PhpSymbolKind::Interface;
    repository.templates = vec![TemplateParam {
        name: "TEntity".to_string(),
        bound: Some(TypeInfo::Simple("object".to_string())),
        variance: TemplateVariance::Covariant,
    }];
    let repository_method = SymbolInfo {
        name: "find".to_string(),
        fqn: "App\\Repository::find".to_string(),
        kind: PhpSymbolKind::Method,
        uri: "file:///repo.php".to_string(),
        range: (3, 4, 3, 40),
        selection_range: (3, 20, 3, 24),
        visibility: Visibility::Public,
        modifiers: SymbolModifiers::default(),
        attributes: vec![],
        doc_comment: None,
        signature: Some(Signature {
            params: vec![],
            return_type: Some(TypeInfo::Simple("TEntity".to_string())),
        }),
        parent_fqn: Some("App\\Repository".to_string()),
        extends: vec![],
        implements: vec![],
        traits: vec![],
        templates: vec![],
        template_bindings: vec![],
    };
    index.update_file(
        "file:///repo.php",
        FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![repository, repository_method],
            ..Default::default()
        },
    );

    let mut user_repository = make_class(
        "UserRepository",
        "App\\UserRepository",
        "file:///user_repo.php",
    );
    user_repository.implements = vec!["App\\Repository".to_string()];
    user_repository.template_bindings = vec![TemplateBinding {
        kind: TemplateBindingKind::Implements,
        target: "App\\Repository".to_string(),
        args: vec![TypeInfo::Simple("App\\User".to_string())],
    }];
    index.update_file(
        "file:///user_repo.php",
        FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![user_repository],
            ..Default::default()
        },
    );

    let found = index
        .resolve_fqn("App\\UserRepository::find")
        .expect("generic inherited method should resolve");
    assert_eq!(
        found
            .signature
            .as_ref()
            .and_then(|sig| sig.return_type.clone()),
        Some(TypeInfo::Simple("App\\User".to_string()))
    );
}

#[test]
fn test_template_substitution_for_collection_item_type() {
    let index = WorkspaceIndex::new();

    let mut collection = make_class("Collection", "App\\Collection", "file:///collection.php");
    collection.templates = vec![TemplateParam {
        name: "TItem".to_string(),
        bound: None,
        variance: TemplateVariance::Covariant,
    }];
    let first_method = SymbolInfo {
        name: "first".to_string(),
        fqn: "App\\Collection::first".to_string(),
        kind: PhpSymbolKind::Method,
        uri: "file:///collection.php".to_string(),
        range: (3, 4, 3, 40),
        selection_range: (3, 20, 3, 25),
        visibility: Visibility::Public,
        modifiers: SymbolModifiers::default(),
        attributes: vec![],
        doc_comment: None,
        signature: Some(Signature {
            params: vec![],
            return_type: Some(TypeInfo::Simple("TItem".to_string())),
        }),
        parent_fqn: Some("App\\Collection".to_string()),
        extends: vec![],
        implements: vec![],
        traits: vec![],
        templates: vec![],
        template_bindings: vec![],
    };
    index.update_file(
        "file:///collection.php",
        FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![collection, first_method],
            ..Default::default()
        },
    );

    let mut user_collection =
        make_class("UserCollection", "App\\UserCollection", "file:///users.php");
    user_collection.extends = vec!["App\\Collection".to_string()];
    user_collection.template_bindings = vec![TemplateBinding {
        kind: TemplateBindingKind::Extends,
        target: "App\\Collection".to_string(),
        args: vec![TypeInfo::Simple("App\\User".to_string())],
    }];
    index.update_file(
        "file:///users.php",
        FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![user_collection],
            ..Default::default()
        },
    );

    let members = index.get_members("App\\UserCollection");
    let first = members
        .iter()
        .find(|member| member.name == "first")
        .expect("inherited collection method should be returned");
    assert_eq!(
        first
            .signature
            .as_ref()
            .and_then(|signature| signature.return_type.clone()),
        Some(TypeInfo::Simple("App\\User".to_string()))
    );
}

#[test]
fn test_type_alias_expands_class_scoped_array_shape() {
    let index = WorkspaceIndex::new();

    let mut service = make_class("UserService", "App\\UserService", "file:///service.php");
    service.doc_comment =
        Some("/**\n * @phpstan-type UserShape array{id: int, name?: string}\n */".to_string());
    let method = SymbolInfo {
        name: "getShape".to_string(),
        fqn: "App\\UserService::getShape".to_string(),
        kind: PhpSymbolKind::Method,
        uri: "file:///service.php".to_string(),
        range: (5, 4, 7, 5),
        selection_range: (5, 20, 5, 28),
        visibility: Visibility::Public,
        modifiers: SymbolModifiers::default(),
        attributes: vec![],
        doc_comment: Some("/** @return UserShape */".to_string()),
        signature: Some(Signature {
            params: vec![],
            return_type: Some(TypeInfo::Simple("UserShape".to_string())),
        }),
        parent_fqn: Some("App\\UserService".to_string()),
        extends: vec![],
        implements: vec![],
        traits: vec![],
        templates: vec![],
        template_bindings: vec![],
    };
    index.update_file(
        "file:///service.php",
        FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![service, method],
            ..Default::default()
        },
    );

    let found = index
        .resolve_fqn("App\\UserService::getShape")
        .expect("method with type alias should resolve");
    let return_type = found
        .signature
        .as_ref()
        .and_then(|signature| signature.return_type.as_ref())
        .expect("return type should be available");
    let TypeInfo::ArrayShape(items) = return_type else {
        panic!("expected alias to expand to array shape, got {return_type:?}");
    };
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].key.as_deref(), Some("id"));
    assert_eq!(items[1].key.as_deref(), Some("name"));
    assert!(items[1].optional);
}

#[test]
fn test_imported_type_alias_expands_from_source_class() {
    let index = WorkspaceIndex::new();

    let mut types = make_class("Types", "App\\Types", "file:///types.php");
    types.doc_comment = Some("/**\n * @phpstan-type UserShape array{id: int}\n */".to_string());
    index.update_file(
        "file:///types.php",
        FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![types],
            ..Default::default()
        },
    );

    let mut service = make_class("UserService", "App\\UserService", "file:///service.php");
    service.doc_comment =
        Some("/**\n * @phpstan-import-type UserShape from Types as LocalShape\n */".to_string());
    let method = SymbolInfo {
        name: "getShape".to_string(),
        fqn: "App\\UserService::getShape".to_string(),
        kind: PhpSymbolKind::Method,
        uri: "file:///service.php".to_string(),
        range: (5, 4, 7, 5),
        selection_range: (5, 20, 5, 28),
        visibility: Visibility::Public,
        modifiers: SymbolModifiers::default(),
        attributes: vec![],
        doc_comment: Some("/** @return LocalShape */".to_string()),
        signature: Some(Signature {
            params: vec![],
            return_type: Some(TypeInfo::Simple("LocalShape".to_string())),
        }),
        parent_fqn: Some("App\\UserService".to_string()),
        extends: vec![],
        implements: vec![],
        traits: vec![],
        templates: vec![],
        template_bindings: vec![],
    };
    index.update_file(
        "file:///service.php",
        FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![service, method],
            ..Default::default()
        },
    );

    let found = index
        .resolve_fqn("App\\UserService::getShape")
        .expect("method with imported type alias should resolve");
    assert!(matches!(
        found
            .signature
            .as_ref()
            .and_then(|signature| signature.return_type.as_ref()),
        Some(TypeInfo::ArrayShape(_))
    ));
}

#[test]
fn test_file_level_type_alias_expands_function_return() {
    let index = WorkspaceIndex::new();

    let function = SymbolInfo {
        name: "getShape".to_string(),
        fqn: "App\\getShape".to_string(),
        kind: PhpSymbolKind::Function,
        uri: "file:///functions.php".to_string(),
        range: (6, 0, 8, 1),
        selection_range: (6, 9, 6, 17),
        visibility: Visibility::Public,
        modifiers: SymbolModifiers::default(),
        attributes: vec![],
        doc_comment: Some("/** @return UserShape */".to_string()),
        signature: Some(Signature {
            params: vec![],
            return_type: Some(TypeInfo::Simple("UserShape".to_string())),
        }),
        parent_fqn: None,
        extends: vec![],
        implements: vec![],
        traits: vec![],
        templates: vec![],
        template_bindings: vec![],
    };
    index.update_file(
        "file:///functions.php",
        FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![function],
            type_aliases: vec![PhpDocTypeAlias {
                name: "UserShape".to_string(),
                type_info: TypeInfo::ArrayShape(vec![ArrayShapeItem {
                    key: Some("id".to_string()),
                    optional: false,
                    value: TypeInfo::Simple("int".to_string()),
                }]),
            }],
            ..Default::default()
        },
    );

    let found = index
        .resolve_fqn("App\\getShape")
        .expect("function with file-level type alias should resolve");
    assert!(matches!(
        found
            .signature
            .as_ref()
            .and_then(|signature| signature.return_type.as_ref()),
        Some(TypeInfo::ArrayShape(_))
    ));
}

#[test]
fn test_recursive_type_alias_falls_back_to_raw_alias() {
    let index = WorkspaceIndex::new();

    let mut service = make_class("LoopService", "App\\LoopService", "file:///loop.php");
    service.doc_comment = Some("/**\n * @phpstan-type A B\n * @phpstan-type B A\n */".to_string());
    let method = SymbolInfo {
        name: "loop".to_string(),
        fqn: "App\\LoopService::loop".to_string(),
        kind: PhpSymbolKind::Method,
        uri: "file:///loop.php".to_string(),
        range: (5, 4, 7, 5),
        selection_range: (5, 20, 5, 24),
        visibility: Visibility::Public,
        modifiers: SymbolModifiers::default(),
        attributes: vec![],
        doc_comment: Some("/** @return A */".to_string()),
        signature: Some(Signature {
            params: vec![],
            return_type: Some(TypeInfo::Simple("A".to_string())),
        }),
        parent_fqn: Some("App\\LoopService".to_string()),
        extends: vec![],
        implements: vec![],
        traits: vec![],
        templates: vec![],
        template_bindings: vec![],
    };
    index.update_file(
        "file:///loop.php",
        FileSymbols {
            namespace: Some("App".to_string()),
            use_statements: vec![],
            symbols: vec![service, method],
            ..Default::default()
        },
    );

    let found = index
        .resolve_fqn("App\\LoopService::loop")
        .expect("recursive alias method should still resolve");
    assert_eq!(
        found
            .signature
            .as_ref()
            .and_then(|signature| signature.return_type.clone()),
        Some(TypeInfo::Simple("A".to_string()))
    );
}
