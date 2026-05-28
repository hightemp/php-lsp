mod support;

use support::*;

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

/**
 * @extends ServiceEntityRepository<NumberStatus>
 */
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
