mod support;

use php_lsp_types::uri::path_to_uri;
use support::*;

#[tokio::test(flavor = "current_thread")]
async fn test_open_php_renamed_to_blade_rebuilds_virtual_document_atomically() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request(1))
        .await
        .unwrap();

    let target_uri = "file:///test/RenameTarget.php";
    let old_uri = "file:///test/rename-template.php";
    let new_uri = "file:///test/rename-template.blade.php";
    let template = "<div>{{ new \\App\\RenameTarget() }}</div>";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(
            target_uri,
            "<?php namespace App; class RenameTarget {}",
        ))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(old_uri, template))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_rename_files_notification(vec![(old_uri, new_uri)]))
        .await
        .unwrap();

    let (line, character) = utf16_position_at(template, "RenameTarget");
    let response = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(2, new_uri, line, character + 2))
        .await
        .unwrap();
    let result = extract_result(response);
    assert!(
        result.to_string().contains(target_uri),
        "renamed Blade snapshot should resolve through rebuilt virtual PHP: {result}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn test_local_variable_method_return_does_not_use_previous_method_phpdoc() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request(1))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();

    let code = r#"<?php
namespace App;

class MessageLog {}

class Handler {
    /**
     * @return array<string, int|string>
     */
    private function response(): array { return []; }

    private function log(): MessageLog { return new MessageLog(); }

    public function run(): void {
        $messageLog = $this->log();
    }
}
"#;
    let uri = "file:///test/local-method-return-ignores-previous-phpdoc.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let response = service
        .ready()
        .await
        .unwrap()
        .call(inlay_hint_request(2, uri, 0, 0, 17, 0))
        .await
        .unwrap();
    let result = extract_result(response);
    let hints = result.as_array().expect("expected inlay hint array");
    let labels: Vec<String> = hints.iter().filter_map(inlay_hint_label_text).collect();
    assert!(
        labels.iter().any(|label| label == ": MessageLog"),
        "expected same-class method native return inlay, got: {:?}",
        labels
    );
    assert!(
        !labels
            .iter()
            .any(|label| label == ": array<string, int|string>"),
        "previous method PHPDoc must not override the next method return, got: {:?}",
        labels
    );

    let hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(3, uri, 14, 10))
        .await
        .unwrap();
    let result = extract_result(hover);
    let contents = hover_markdown_value(&result);
    assert!(
        contents.contains("MessageLog $messageLog"),
        "expected hover from same-class method native return, got: {}",
        contents
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_doctrine_get_repository_chain_infers_custom_and_standard_returns() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request(1))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(
            "file:///test/doctrine/ServiceEntityRepository.php",
            r#"<?php
namespace Doctrine\Bundle\DoctrineBundle\Repository;

class ServiceEntityRepository {}
"#,
        ))
        .await
        .unwrap();

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(
            "file:///test/app/Entity/NumberStatus.php",
            r#"<?php
namespace App\Entity;

#[\Doctrine\ORM\Mapping\Entity(repositoryClass: \App\Repository\NumberStatusRepository::class)]
class NumberStatus {}
class RequestStatus {}
"#,
        ))
        .await
        .unwrap();

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(
            "file:///test/app/Repository/NumberStatusRepository.php",
            r#"<?php
namespace App\Repository;

use App\Entity\NumberStatus;
use Doctrine\Bundle\DoctrineBundle\Repository\ServiceEntityRepository;

class NumberStatusRepository extends ServiceEntityRepository {
    public function findByNameOrCreate(string $name): NumberStatus { return new NumberStatus(); }
}
"#,
        ))
        .await
        .unwrap();

    let code_with_markers = r#"<?php
namespace App;

use App\Entity\NumberStatus;
use App\Entity\RequestStatus;

class EntityManager {
    public function getRepository(string $class): object {}
}

class Handler {
    private EntityManager $em;

    public function run(): void {
        $number/*number*/Status = $this->em->getRepository(NumberStatus::class)
            ->findByNameOrCreate('active');
        $completed/*completed*/Status = $this->em->getRepository(RequestStatus::class)
            ->findOneBy(['name' => 'completed']);
    }
}
"#;
    let markers = ["/*number*/", "/*completed*/"];
    let marker_position = |marker: &str| -> (u32, u32) {
        let marker_offset = code_with_markers
            .find(marker)
            .expect("test code should contain marker");
        let mut prefix = code_with_markers[..marker_offset].to_string();
        for marker in markers {
            prefix = prefix.replace(marker, "");
        }
        let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
        let line_start = prefix.rfind('\n').map(|idx| idx + 1).unwrap_or(0);
        let character = (prefix.len() - line_start) as u32;
        (line, character)
    };
    let (number_line, number_character) = marker_position("/*number*/");
    let (completed_line, completed_character) = marker_position("/*completed*/");
    let mut code = code_with_markers.to_string();
    for marker in markers {
        code = code.replace(marker, "");
    }
    let uri = "file:///test/app/Handler.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, &code))
        .await
        .unwrap();

    let response = service
        .ready()
        .await
        .unwrap()
        .call(inlay_hint_request(2, uri, 0, 0, 20, 0))
        .await
        .unwrap();
    let result = extract_result(response);
    let hints = result.as_array().expect("expected inlay hint array");
    let labels: Vec<String> = hints.iter().filter_map(inlay_hint_label_text).collect();
    assert!(
        labels.iter().any(|label| label == ": NumberStatus"),
        "expected custom repository method return inlay, got: {:?}",
        labels
    );
    assert!(
        labels.iter().any(|label| label.contains("RequestStatus")),
        "expected standard findOneBy entity return inlay, got: {:?}",
        labels
    );
    assert!(
        !labels.iter().any(|label| label == ": object|null"),
        "standard repository inference should not fall back to object|null, got: {:?}",
        labels
    );

    let number_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(3, uri, number_line, number_character))
        .await
        .unwrap();
    let number_result = extract_result(number_hover);
    assert!(
        hover_markdown_value(&number_result).contains("NumberStatus $numberStatus"),
        "expected custom repository return hover, got: {}",
        number_result
    );

    let completed_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(4, uri, completed_line, completed_character))
        .await
        .unwrap();
    let completed_result = extract_result(completed_hover);
    assert!(
        hover_markdown_value(&completed_result).contains("RequestStatus"),
        "expected standard findOneBy entity return hover, got: {}",
        completed_result
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_watched_files_incrementally_reindex_created_changed_deleted_php_files() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let tmp_root =
        std::env::temp_dir().join(format!("php-lsp-watch-{}-{}", std::process::id(), nanos));
    fs::create_dir_all(&tmp_root).unwrap();
    let root_uri = format!("file://{}", tmp_root.to_string_lossy());

    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(1, Some(&root_uri), None))
        .await
        .unwrap();

    let watched_path = tmp_root.join("Watched.php");
    let watched_uri = format!("file://{}", watched_path.to_string_lossy());
    fs::write(
        &watched_path,
        "<?php\nnamespace Watched;\nclass Created {}\n",
    )
    .unwrap();

    service
        .ready()
        .await
        .unwrap()
        .call(did_change_watched_files_notification(vec![(
            &watched_uri,
            1,
        )]))
        .await
        .unwrap();

    let created_resp = service
        .ready()
        .await
        .unwrap()
        .call(workspace_symbol_request(2, "Created"))
        .await
        .unwrap();
    let created_result = extract_result(created_resp);
    let created_names = workspace_symbol_names(&created_result);
    assert!(
        created_names.iter().any(|name| name == "Created"),
        "created PHP file should be indexed, got: {}",
        created_result
    );

    fs::write(
        &watched_path,
        "<?php\nnamespace Watched;\nclass Updated {}\n",
    )
    .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_change_watched_files_notification(vec![(
            &watched_uri,
            2,
        )]))
        .await
        .unwrap();

    let updated_resp = service
        .ready()
        .await
        .unwrap()
        .call(workspace_symbol_request(3, "Updated"))
        .await
        .unwrap();
    let updated_result = extract_result(updated_resp);
    let updated_names = workspace_symbol_names(&updated_result);
    assert!(
        updated_names.iter().any(|name| name == "Updated"),
        "changed PHP file should update the index, got: {}",
        updated_result
    );

    let stale_resp = service
        .ready()
        .await
        .unwrap()
        .call(workspace_symbol_request(4, "Created"))
        .await
        .unwrap();
    let stale_result = extract_result(stale_resp);
    let stale_names = workspace_symbol_names(&stale_result);
    assert!(
        !stale_names.iter().any(|name| name == "Created"),
        "changed PHP file should remove stale symbols, got: {}",
        stale_result
    );

    fs::remove_file(&watched_path).unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_change_watched_files_notification(vec![(
            &watched_uri,
            3,
        )]))
        .await
        .unwrap();

    let deleted_resp = service
        .ready()
        .await
        .unwrap()
        .call(workspace_symbol_request(5, "Updated"))
        .await
        .unwrap();
    let deleted_result = extract_result(deleted_resp);
    let deleted_names = workspace_symbol_names(&deleted_result);
    assert!(
        !deleted_names.iter().any(|name| name == "Updated"),
        "deleted PHP file should be removed from the index, got: {}",
        deleted_result
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();

    let _ = fs::remove_dir_all(&tmp_root);
}

#[tokio::test(flavor = "current_thread")]
async fn test_did_close_restores_disk_index_after_discarding_unsaved_changes() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request(1))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-close-restore-{}-{}",
        std::process::id(),
        nanos
    ));
    fs::create_dir_all(&tmp_root).unwrap();
    let file_path = tmp_root.join("Discarded.php");
    let uri = path_to_uri(&file_path).expect("temporary PHP file URI");
    let uri_str = uri.as_str();
    let disk_code = "<?php\nnamespace CloseRestore;\nclass PersistedAfterDiscard {}\n";
    let unsaved_code = "<?php\nnamespace CloseRestore;\nclass UnsavedBeforeDiscard {}\n";
    fs::write(&file_path, disk_code).unwrap();

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri_str, disk_code))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_change_full_notification(uri_str, 2, unsaved_code))
        .await
        .unwrap();

    let unsaved_response = service
        .ready()
        .await
        .unwrap()
        .call(workspace_symbol_request(2, "UnsavedBeforeDiscard"))
        .await
        .unwrap();
    assert!(
        workspace_symbol_names(&extract_result(unsaved_response))
            .iter()
            .any(|name| name == "UnsavedBeforeDiscard"),
        "didChange must publish the unsaved open-document snapshot"
    );

    service
        .ready()
        .await
        .unwrap()
        .call(did_close_notification(uri_str))
        .await
        .unwrap();

    let restored_response = service
        .ready()
        .await
        .unwrap()
        .call(workspace_symbol_request(3, "PersistedAfterDiscard"))
        .await
        .unwrap();
    let restored_result = extract_result(restored_response);
    assert!(
        workspace_symbol_names(&restored_result)
            .iter()
            .any(|name| name == "PersistedAfterDiscard"),
        "didClose must restore the unchanged on-disk symbol: {restored_result}"
    );

    let stale_response = service
        .ready()
        .await
        .unwrap()
        .call(workspace_symbol_request(4, "UnsavedBeforeDiscard"))
        .await
        .unwrap();
    let stale_result = extract_result(stale_response);
    assert!(
        !workspace_symbol_names(&stale_result)
            .iter()
            .any(|name| name == "UnsavedBeforeDiscard"),
        "didClose must remove the discarded unsaved symbol: {stale_result}"
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();

    let _ = fs::remove_file(&file_path);
    let _ = fs::remove_dir(&tmp_root);
}

#[tokio::test(flavor = "current_thread")]
async fn test_workspace_file_operations_update_index_uris() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let tmp_root =
        std::env::temp_dir().join(format!("php-lsp-fileops-{}-{}", std::process::id(), nanos));
    fs::create_dir_all(&tmp_root).unwrap();
    let root_uri = format!("file://{}", tmp_root.to_string_lossy());

    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(1, Some(&root_uri), None))
        .await
        .unwrap();

    let created_path = tmp_root.join("Created.php");
    let created_uri = format!("file://{}", created_path.to_string_lossy());
    fs::write(
        &created_path,
        "<?php\nnamespace FileOps;\nclass FileOperationTarget {}\n",
    )
    .unwrap();

    service
        .ready()
        .await
        .unwrap()
        .call(did_create_files_notification(vec![&created_uri]))
        .await
        .unwrap();

    let created_resp = service
        .ready()
        .await
        .unwrap()
        .call(workspace_symbol_request(2, "FileOperationTarget"))
        .await
        .unwrap();
    let created_result = extract_result(created_resp);
    assert!(
        workspace_symbol_uris(&created_result)
            .iter()
            .any(|uri| uri == &created_uri),
        "didCreateFiles should index the new PHP file, got: {}",
        created_result
    );

    let renamed_path = tmp_root.join("Renamed.php");
    let renamed_uri = format!("file://{}", renamed_path.to_string_lossy());
    fs::rename(&created_path, &renamed_path).unwrap();

    service
        .ready()
        .await
        .unwrap()
        .call(did_rename_files_notification(vec![(
            &created_uri,
            &renamed_uri,
        )]))
        .await
        .unwrap();

    let renamed_resp = service
        .ready()
        .await
        .unwrap()
        .call(workspace_symbol_request(3, "FileOperationTarget"))
        .await
        .unwrap();
    let renamed_result = extract_result(renamed_resp);
    let renamed_uris = workspace_symbol_uris(&renamed_result);
    assert!(
        renamed_uris.iter().any(|uri| uri == &renamed_uri)
            && !renamed_uris.iter().any(|uri| uri == &created_uri),
        "didRenameFiles should move symbol locations to the new URI, got: {}",
        renamed_result
    );

    fs::remove_file(&renamed_path).unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_delete_files_notification(vec![&renamed_uri]))
        .await
        .unwrap();

    let deleted_resp = service
        .ready()
        .await
        .unwrap()
        .call(workspace_symbol_request(4, "FileOperationTarget"))
        .await
        .unwrap();
    let deleted_result = extract_result(deleted_resp);
    assert!(
        workspace_symbol_names(&deleted_result).is_empty(),
        "didDeleteFiles should remove deleted PHP symbols, got: {}",
        deleted_result
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();

    let _ = fs::remove_dir_all(&tmp_root);
}

#[tokio::test(flavor = "current_thread")]
async fn test_workspace_folders_index_multiple_roots() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-multiroot-{}-{}",
        std::process::id(),
        nanos
    ));
    let root_a = tmp_root.join("root-a");
    let root_b = tmp_root.join("root-b");
    fs::create_dir_all(root_a.join("src")).unwrap();
    fs::create_dir_all(root_b.join("src")).unwrap();
    fs::write(
        root_a.join("composer.json"),
        r#"{"autoload":{"psr-4":{"RootA\\":"src/"}}}"#,
    )
    .unwrap();
    fs::write(
        root_b.join("composer.json"),
        r#"{"autoload":{"psr-4":{"RootB\\":"src/"}}}"#,
    )
    .unwrap();
    let root_a_service = root_a.join("src/RootAService.php");
    let root_b_service = root_b.join("src/RootBService.php");
    fs::write(
        &root_a_service,
        "<?php\nnamespace RootA;\nclass RootAService {}\n",
    )
    .unwrap();
    fs::write(
        &root_b_service,
        "<?php\nnamespace RootB;\nclass RootBService {}\n",
    )
    .unwrap();

    let root_a_uri = format!("file://{}", root_a.to_string_lossy());
    let root_b_uri = format!("file://{}", root_b.to_string_lossy());
    let init_resp = service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_workspace_folders(
            1,
            vec![("root-a", &root_a_uri), ("root-b", &root_b_uri)],
        ))
        .await
        .unwrap();
    let init_result = extract_result(init_resp);
    assert_eq!(
        init_result["capabilities"]["workspace"]["workspaceFolders"]["supported"].as_bool(),
        Some(true),
        "server should advertise workspaceFolders support, got: {}",
        init_result
    );

    service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();

    let expected_a_uri = format!("file://{}", root_a_service.to_string_lossy());
    let expected_b_uri = format!("file://{}", root_b_service.to_string_lossy());
    let mut result = json!(null);
    for attempt in 0..50 {
        let resp = service
            .ready()
            .await
            .unwrap()
            .call(workspace_symbol_request(10 + attempt, "Root"))
            .await
            .unwrap();
        result = extract_result(resp);
        let uris = workspace_symbol_uris(&result);
        if uris.iter().any(|uri| uri == &expected_a_uri)
            && uris.iter().any(|uri| uri == &expected_b_uri)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let uris = workspace_symbol_uris(&result);
    assert!(
        uris.iter().any(|uri| uri == &expected_a_uri)
            && uris.iter().any(|uri| uri == &expected_b_uri),
        "workspace/symbol should include PHP symbols from both workspace folders, got: {}",
        result
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();

    let _ = fs::remove_dir_all(&tmp_root);
}

#[tokio::test(flavor = "current_thread")]
async fn test_multi_root_include_paths_do_not_leak_between_roots() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    let tmp = std::env::temp_dir().join(format!(
        "php-lsp-multiroot-include-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let root_a = tmp.join("root-a");
    let root_b = tmp.join("root-b");
    for root in [&root_a, &root_b] {
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("extra-shared")).unwrap();
        fs::write(
            root.join("composer.json"),
            r#"{"autoload":{"psr-4":{"App\\":"src/"}}}"#,
        )
        .unwrap();
    }
    let root_a_service = root_a.join("src/RootAService.php");
    let root_b_service = root_b.join("src/RootBService.php");
    let root_a_leak = root_a.join("extra-shared/RootALeak.php");
    let root_b_extra = root_b.join("extra-shared/RootBExtra.php");
    fs::write(&root_a_service, "<?php class RootAService {}\n").unwrap();
    fs::write(&root_b_service, "<?php class RootBService {}\n").unwrap();
    fs::write(&root_a_leak, "<?php class RootALeak {}\n").unwrap();
    fs::write(&root_b_extra, "<?php class RootBExtra {}\n").unwrap();

    let root_a_uri = path_to_uri(&root_a).unwrap();
    let root_b_uri = path_to_uri(&root_b).unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_workspace_folders_and_options(
            1,
            vec![("root-a", &root_a_uri), ("root-b", &root_b_uri)],
            Some(json!({
                "configurationVersion": 2,
                "global": {},
                "workspaceFolders": [
                    { "uri": root_a_uri, "settings": {} },
                    { "uri": root_b_uri, "settings": { "includePaths": ["extra-shared"] } }
                ]
            })),
        ))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();

    let expected_a = path_to_uri(&root_a_service).unwrap();
    let expected_b = path_to_uri(&root_b_service).unwrap();
    let expected_extra_b = path_to_uri(&root_b_extra).unwrap();
    let forbidden_leak_a = path_to_uri(&root_a_leak).unwrap();
    let mut result = json!(null);
    for attempt in 0..80 {
        result = extract_result(
            service
                .ready()
                .await
                .unwrap()
                .call(workspace_symbol_request(10 + attempt, "Root"))
                .await
                .unwrap(),
        );
        let uris = workspace_symbol_uris(&result);
        if uris.iter().any(|uri| uri == &expected_a)
            && uris.iter().any(|uri| uri == &expected_b)
            && uris.iter().any(|uri| uri == &expected_extra_b)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let uris = workspace_symbol_uris(&result);
    assert!(uris.iter().any(|uri| uri == &expected_a));
    assert!(uris.iter().any(|uri| uri == &expected_b));
    assert!(uris.iter().any(|uri| uri == &expected_extra_b));
    assert!(
        uris.iter().all(|uri| uri != &forbidden_leak_a),
        "root B includePaths must not scan the matching relative path in root A: {result}"
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
    fs::remove_dir_all(tmp).unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn test_external_symlink_is_indexed_and_physical_watch_event_updates_logical_uri() {
    use std::os::unix::fs::symlink;

    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let tmp = std::env::temp_dir().join(format!(
        "php-lsp-e2e-external-symlink-{}-{nanos}",
        std::process::id()
    ));
    let root = tmp.join("workspace");
    let external = tmp.join("external-src");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&external).unwrap();
    fs::write(
        root.join("composer.json"),
        r#"{"autoload":{"psr-4":{"Linked\\":"src/"}}}"#,
    )
    .unwrap();
    let physical_file = external.join("Subject.php");
    fs::write(
        &physical_file,
        "<?php namespace Linked; class BeforePhysicalWatch {}",
    )
    .unwrap();
    symlink(&external, root.join("src")).unwrap();

    let root_uri = path_to_uri(&root).unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(1, Some(&root_uri), None))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();

    let logical_uri = path_to_uri(&root.join("src/Subject.php")).unwrap();
    let mut before = json!(null);
    for request_id in 10..90 {
        before = extract_result(
            service
                .ready()
                .await
                .unwrap()
                .call(workspace_symbol_request(request_id, "BeforePhysicalWatch"))
                .await
                .unwrap(),
        );
        if workspace_symbol_uris(&before)
            .iter()
            .any(|uri| uri == &logical_uri)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        workspace_symbol_uris(&before)
            .iter()
            .any(|uri| uri == &logical_uri),
        "external file should be indexed through its logical symlink URI: {before}"
    );

    fs::write(
        &physical_file,
        "<?php namespace Linked; class AfterPhysicalWatch {}",
    )
    .unwrap();
    let physical_uri = path_to_uri(&physical_file).unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_change_watched_files_notification(vec![(
            &physical_uri,
            2,
        )]))
        .await
        .unwrap();

    let after = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(workspace_symbol_request(100, "AfterPhysicalWatch"))
            .await
            .unwrap(),
    );
    assert!(
        workspace_symbol_uris(&after)
            .iter()
            .any(|uri| uri == &logical_uri),
        "physical watcher event should update the logical indexed URI: {after}"
    );
    let stale = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(workspace_symbol_request(101, "BeforePhysicalWatch"))
            .await
            .unwrap(),
    );
    assert!(workspace_symbol_names(&stale).is_empty());

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(999))
        .await
        .unwrap();
    fs::remove_dir_all(tmp).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_repeated_composer_reindex_only_publishes_latest_ready_run() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    let tmp = std::env::temp_dir().join(format!(
        "php-lsp-reindex-generation-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let src = tmp.join("src");
    fs::create_dir_all(&src).unwrap();
    let composer = tmp.join("composer.json");
    let composer_source = r#"{"autoload":{"psr-4":{"App\\":"src/"}}}"#;
    fs::write(&composer, composer_source).unwrap();
    for index in 0..256 {
        fs::write(
            src.join(format!("Subject{index:03}.php")),
            format!("<?php namespace App; class Subject{index:03} {{}}\n"),
        )
        .unwrap();
    }
    let root_uri = path_to_uri(&tmp).unwrap();
    let composer_uri = path_to_uri(&composer).unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(1, Some(&root_uri), None))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();
    wait_for_indexing_phase(&mut notifications, "ready", Duration::from_secs(10)).await;

    fs::write(&composer, composer_source).unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_change_watched_files_notification(vec![(
            &composer_uri,
            2,
        )]))
        .await
        .unwrap();
    let first =
        next_indexing_status_for_phase(&mut notifications, "discovering", Duration::from_secs(5))
            .await;

    fs::write(&composer, composer_source).unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_change_watched_files_notification(vec![(
            &composer_uri,
            2,
        )]))
        .await
        .unwrap();
    let second =
        next_indexing_status_for_phase(&mut notifications, "discovering", Duration::from_secs(5))
            .await;
    let first_run = first["indexingRunId"].as_u64().expect("first run id");
    let second_run = second["indexingRunId"].as_u64().expect("second run id");
    assert!(second_run > first_run);
    assert_eq!(
        second["workspaceFolder"].as_str(),
        Some(tmp.to_string_lossy().as_ref())
    );

    let ready =
        next_indexing_status_for_phase(&mut notifications, "ready", Duration::from_secs(10)).await;
    assert_eq!(ready["indexingRunId"].as_u64(), Some(second_run));

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(999))
        .await
        .unwrap();
    fs::remove_dir_all(tmp).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_removed_workspace_rejects_late_indexing_publication() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    let tmp = std::env::temp_dir().join(format!(
        "php-lsp-removed-indexing-root-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&tmp).unwrap();
    for index in 0..256 {
        fs::write(
            tmp.join(format!("Removed{index:03}.php")),
            format!("<?php class Removed{index:03} {{}}\n"),
        )
        .unwrap();
    }
    let root_uri = path_to_uri(&tmp).unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_workspace_folders(
            1,
            vec![("removed", &root_uri)],
        ))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();
    let discovering =
        next_indexing_status_for_phase(&mut notifications, "discovering", Duration::from_secs(5))
            .await;
    let removed_run = discovering["indexingRunId"]
        .as_u64()
        .expect("removed workspace run id");

    service
        .ready()
        .await
        .unwrap()
        .call(did_change_workspace_folders_notification(
            Vec::new(),
            vec![("removed", &root_uri)],
        ))
        .await
        .unwrap();
    let symbols = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(workspace_symbol_request(10, "Removed"))
            .await
            .unwrap(),
    );
    assert!(workspace_symbol_names(&symbols).is_empty());

    let deadline = tokio::time::Instant::now() + Duration::from_millis(250);
    while let Ok(Some(notification)) = tokio::time::timeout_at(deadline, notifications.recv()).await
    {
        if notification.method() != "phpLsp/indexingStatus" {
            continue;
        }
        let params = notification.params().expect("indexing status params");
        assert!(
            params["phase"] != "ready" || params["indexingRunId"].as_u64() != Some(removed_run),
            "removed workspace run published a late ready status: {params}"
        );
    }

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(999))
        .await
        .unwrap();
    fs::remove_dir_all(tmp).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_reindexing_one_workspace_does_not_cancel_other_root_run() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });
    let tmp = std::env::temp_dir().join(format!(
        "php-lsp-root-scoped-reindex-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let root_a = tmp.join("a");
    let root_b = tmp.join("b");
    let composer_source = r#"{"autoload":{"psr-4":{"App\\":"src/"}}}"#;
    for root in [&root_a, &root_b] {
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("composer.json"), composer_source).unwrap();
        for index in 0..64 {
            fs::write(
                root.join(format!("src/Subject{index:03}.php")),
                format!("<?php class Subject{index:03} {{}}\n"),
            )
            .unwrap();
        }
    }
    let root_a_uri = path_to_uri(&root_a).unwrap();
    let root_b_uri = path_to_uri(&root_b).unwrap();
    let composer_a_uri = path_to_uri(&root_a.join("composer.json")).unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_workspace_folders(
            1,
            vec![("a", &root_a_uri), ("b", &root_b_uri)],
        ))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();
    let initial_a =
        next_indexing_status_for_phase(&mut notifications, "discovering", Duration::from_secs(5))
            .await;
    let initial_b =
        next_indexing_status_for_phase(&mut notifications, "discovering", Duration::from_secs(5))
            .await;
    assert_eq!(
        initial_a["workspaceFolder"].as_str(),
        Some(root_a.to_string_lossy().as_ref())
    );
    assert_eq!(
        initial_b["workspaceFolder"].as_str(),
        Some(root_b.to_string_lossy().as_ref())
    );
    let initial_a_run = initial_a["indexingRunId"].as_u64().unwrap();
    let initial_b_run = initial_b["indexingRunId"].as_u64().unwrap();

    fs::write(root_a.join("composer.json"), composer_source).unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_change_watched_files_notification(vec![(
            &composer_a_uri,
            2,
        )]))
        .await
        .unwrap();
    let mut ready_runs = std::collections::HashMap::new();
    let replacement_a = loop {
        let notification = tokio::time::timeout(Duration::from_secs(5), notifications.recv())
            .await
            .expect("timed out waiting for replacement root A run")
            .expect("notification channel closed");
        if notification.method() != "phpLsp/indexingStatus" {
            continue;
        }
        let params = notification.params().cloned().unwrap();
        if params["phase"] == "ready" {
            ready_runs.insert(
                params["workspaceFolder"].as_str().unwrap().to_string(),
                params["indexingRunId"].as_u64().unwrap(),
            );
        }
        if params["phase"] == "discovering"
            && params["workspaceFolder"].as_str() == Some(root_a.to_string_lossy().as_ref())
            && params["indexingRunId"].as_u64().unwrap_or_default() > initial_a_run
        {
            break params;
        }
    };
    let replacement_a_run = replacement_a["indexingRunId"].as_u64().unwrap();
    assert!(replacement_a_run > initial_a_run);

    while ready_runs.len() < 2 {
        let ready =
            next_indexing_status_for_phase(&mut notifications, "ready", Duration::from_secs(10))
                .await;
        ready_runs.insert(
            ready["workspaceFolder"].as_str().unwrap().to_string(),
            ready["indexingRunId"].as_u64().unwrap(),
        );
    }
    assert_eq!(
        ready_runs.get(root_b.to_string_lossy().as_ref()),
        Some(&initial_b_run)
    );
    assert_eq!(
        ready_runs.get(root_a.to_string_lossy().as_ref()),
        Some(&replacement_a_run)
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(999))
        .await
        .unwrap();
    fs::remove_dir_all(tmp).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_initial_indexing_statuses_keep_run_fifo_order() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });
    let tmp = std::env::temp_dir().join(format!(
        "php-lsp-indexing-status-order-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&tmp).unwrap();
    fs::write(tmp.join("Subject.php"), "<?php class Subject {}\n").unwrap();
    let root_uri = path_to_uri(&tmp).unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(1, Some(&root_uri), None))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();

    let mut phases = Vec::new();
    let mut run_id = None;
    while phases.last().map(String::as_str) != Some("ready") {
        let notification = tokio::time::timeout(Duration::from_secs(5), notifications.recv())
            .await
            .expect("timed out waiting for ordered indexing statuses")
            .expect("notification channel closed");
        if notification.method() != "phpLsp/indexingStatus" {
            continue;
        }
        let params = notification.params().expect("indexing status params");
        let Some(current_run) = params["indexingRunId"].as_u64() else {
            continue;
        };
        if run_id.get_or_insert(current_run) != &current_run {
            continue;
        }
        phases.push(params["phase"].as_str().unwrap().to_string());
    }
    let position = |phase: &str| {
        phases
            .iter()
            .position(|candidate| candidate == phase)
            .unwrap()
    };
    assert!(position("discovering") < position("loadingStubs"));
    assert!(position("loadingStubs") < position("stubsLoaded"));
    assert!(position("stubsLoaded") < position("indexing"));
    assert!(position("indexing") < position("ready"));

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(999))
        .await
        .unwrap();
    fs::remove_dir_all(tmp).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_configuration_reindex_reserves_one_run_before_stub_reload() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });
    let tmp = std::env::temp_dir().join(format!(
        "php-lsp-configuration-run-order-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&tmp).unwrap();
    fs::write(tmp.join("Subject.php"), "<?php class Subject {}\n").unwrap();
    let root_uri = path_to_uri(&tmp).unwrap();
    let settings = |exclude_paths: Vec<&str>| {
        json!({
            "configurationVersion": 2,
            "global": { "composerEnabled": false, "stubExtensions": [] },
            "workspaceFolders": [{
                "uri": root_uri,
                "settings": { "excludePaths": exclude_paths }
            }]
        })
    };
    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(
            1,
            Some(&root_uri),
            Some(settings(Vec::new())),
        ))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();
    let initial_ready =
        next_indexing_status_for_phase(&mut notifications, "ready", Duration::from_secs(5)).await;
    let initial_run = initial_ready["indexingRunId"].as_u64().unwrap();

    service
        .ready()
        .await
        .unwrap()
        .call(did_change_configuration_notification(settings(vec![
            "generated",
        ])))
        .await
        .unwrap();

    let started = std::time::Instant::now();
    let mut replacement_run = None;
    let mut phases = Vec::new();
    while phases.last().map(String::as_str) != Some("ready") {
        let remaining = Duration::from_secs(5)
            .checked_sub(started.elapsed())
            .expect("timed out waiting for configuration reindex statuses");
        let notification = tokio::time::timeout(remaining, notifications.recv())
            .await
            .expect("timed out waiting for configuration reindex statuses")
            .expect("notification channel closed");
        if notification.method() != "phpLsp/indexingStatus" {
            continue;
        }
        let params = notification.params().expect("indexing status params");
        let Some(run_id) = params["indexingRunId"].as_u64() else {
            panic!("root-specific stub/reindex status must carry a run id: {params}");
        };
        if run_id <= initial_run {
            continue;
        }
        assert_eq!(
            replacement_run.get_or_insert(run_id),
            &run_id,
            "configuration stub reload and reindex must share one run"
        );
        phases.push(params["phase"].as_str().unwrap().to_string());
    }
    let position = |phase: &str| {
        phases
            .iter()
            .position(|candidate| candidate == phase)
            .unwrap_or_else(|| panic!("missing phase `{phase}` in {phases:?}"))
    };
    assert!(position("discovering") < position("loadingStubs"));
    assert!(position("loadingStubs") < position("stubsLoaded"));
    assert!(position("stubsLoaded") < position("indexing"));
    assert!(position("indexing") < position("ready"));

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(999))
        .await
        .unwrap();
    fs::remove_dir_all(tmp).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_shutdown_cancels_active_indexing_without_late_ready() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });
    let tmp = std::env::temp_dir().join(format!(
        "php-lsp-indexing-shutdown-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&tmp).unwrap();
    for index in 0..512 {
        fs::write(
            tmp.join(format!("Subject{index:03}.php")),
            format!("<?php class Subject{index:03} {{}}\n"),
        )
        .unwrap();
    }
    let root_uri = path_to_uri(&tmp).unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialize_request_with_options(1, Some(&root_uri), None))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();
    let discovering =
        next_indexing_status_for_phase(&mut notifications, "discovering", Duration::from_secs(5))
            .await;
    let run_id = discovering["indexingRunId"].as_u64().unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_millis(250);
    while let Ok(Some(notification)) = tokio::time::timeout_at(deadline, notifications.recv()).await
    {
        if notification.method() != "phpLsp/indexingStatus" {
            continue;
        }
        let params = notification.params().expect("indexing status params");
        assert!(
            params["phase"] != "ready" || params["indexingRunId"].as_u64() != Some(run_id),
            "shutdown run published a late ready status: {params}"
        );
    }
    fs::remove_dir_all(tmp).unwrap();
}
