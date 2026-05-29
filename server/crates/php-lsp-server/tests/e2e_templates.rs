mod support;

use support::*;

#[tokio::test(flavor = "current_thread")]
async fn test_hover_local_variable_class_string_template_return_type() {
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

    let code_with_marker = r#"<?php
namespace App;

class Widget {}

class ServiceLocator {
    /**
     * @template T of object
     * @param class-string<T> $class
     * @return ($class is class-string<T> ? T : object)
     */
    public function make($class) {}
}

function run(ServiceLocator $locator): void {
    $ma/*made*/de = $locator->make(Widget::class);
}
"#;
    let marker = "/*made*/";
    let marker_offset = code_with_marker
        .find(marker)
        .expect("test code should contain marker");
    let prefix = code_with_marker[..marker_offset].replace(marker, "");
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
    let line_start = prefix.rfind('\n').map(|idx| idx + 1).unwrap_or(0);
    let character = (prefix.len() - line_start) as u32;
    let code = code_with_marker.replace(marker, "");
    let uri = "file:///test/hover-class-string-template-return.php";

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
        .call(hover_request(2, uri, line, character))
        .await
        .unwrap();
    let result = extract_result(response);
    let hover = hover_markdown_value(&result);
    assert!(
        hover.contains("Widget $made"),
        "expected class-string template hover to resolve Widget, got: {}",
        hover
    );
    assert!(
        hover.contains("[`Widget`](<file:///test/hover-class-string-template-return.php#L4>)"),
        "expected clickable Widget type link, got: {}",
        hover
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
async fn test_blade_template_virtual_php_hover_completion_diagnostics_and_tokens() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    let tmp_root =
        std::env::temp_dir().join(format!("php-lsp-blade-template-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp_root);
    fs::create_dir_all(tmp_root.join("app")).unwrap();
    fs::create_dir_all(tmp_root.join("resources/views")).unwrap();

    let php_uri = format!("file://{}", tmp_root.join("app/User.php").to_string_lossy());
    let blade_uri = format!(
        "file://{}",
        tmp_root
            .join("resources/views/show.blade.php")
            .to_string_lossy()
    );
    let root_uri = format!("file://{}", tmp_root.to_string_lossy());
    let php_code = "<?php\nclass User { public function getName(): string { return ''; } }\n";
    let completion_marker = "/*complete*/";
    let blade_with_marker = format!(
        "<div>{{{{ User::class }}}}</div>\n@foreach ($items as $item)\n<span>{{{{ (new User())->get{} }}}}</span>\n@endforeach\n",
        completion_marker
    );
    let completion_offset = blade_with_marker
        .find(completion_marker)
        .expect("test Blade should contain completion marker");
    let blade = blade_with_marker.replace(completion_marker, "");
    let completion_prefix = &blade[..completion_offset];
    let completion_line = completion_prefix
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count() as u32;
    let completion_line_start = completion_prefix
        .rfind('\n')
        .map(|idx| idx + 1)
        .unwrap_or(0);
    let completion_character = (completion_prefix.len() - completion_line_start) as u32;

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
        .call(did_open_notification(&php_uri, php_code))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification_with_language(
            &blade_uri, "blade", &blade,
        ))
        .await
        .unwrap();

    let diagnostics =
        next_publish_diagnostics(&mut notifications, &blade_uri, Duration::from_secs(1)).await;
    assert_eq!(
        diagnostics["diagnostics"].as_array().map(Vec::len),
        Some(0),
        "plain HTML around Blade expressions should not produce whole-file diagnostics, got: {}",
        diagnostics
    );

    let hover_resp = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(2, &blade_uri, 0, 9))
        .await
        .unwrap();
    let hover = extract_result(hover_resp);
    let hover_text = hover["contents"]["value"].as_str().unwrap_or_default();
    assert!(
        hover_text.contains("class User"),
        "expected class hover inside Blade echo, got: {}",
        hover
    );
    assert_eq!(
        hover["range"]["start"]["line"].as_u64(),
        Some(0),
        "hover range should be mapped back to original template line, got: {}",
        hover
    );

    let completion_resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(
            3,
            &blade_uri,
            completion_line,
            completion_character,
        ))
        .await
        .unwrap();
    let completion = extract_result(completion_resp);
    let labels: Vec<String> = completion_items_from_result(&completion)
        .iter()
        .filter_map(|item| item.get("label").and_then(|value| value.as_str()))
        .map(str::to_string)
        .collect();
    assert!(
        labels.iter().any(|label| label == "getName"),
        "expected Blade echo completion to include User::getName, got: {:?}",
        labels
    );

    let tokens_resp = service
        .ready()
        .await
        .unwrap()
        .call(semantic_tokens_full_request(4, &blade_uri))
        .await
        .unwrap();
    let tokens = decode_semantic_tokens(&extract_result(tokens_resp));
    assert!(
        tokens.iter().any(|(line, start, len, token_type, _)| {
            (*line, *start, *len, *token_type) == (1, 0, 8, 11)
        }),
        "expected @foreach keyword semantic token mapped to original template, got: {:?}",
        tokens
    );

    let _ = fs::remove_dir_all(&tmp_root);
    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_blade_template_reports_safe_mapped_expression_diagnostics() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-blade-template-diagnostics-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp_root);
    fs::create_dir_all(tmp_root.join("app")).unwrap();
    fs::create_dir_all(tmp_root.join("resources/views")).unwrap();

    let php_path = tmp_root.join("app/User.php");
    let blade_path = tmp_root.join("resources/views/show.blade.php");
    let root_uri = format!("file://{}", tmp_root.to_string_lossy());
    let php_uri = format!("file://{}", php_path.to_string_lossy());
    let blade_uri = format!("file://{}", blade_path.to_string_lossy());
    let php_code = "<?php\nclass User { public function getName(): string { return ''; } }\n";
    let blade = "<div>{{ (new User())->missing() }}</div>\n";

    fs::write(&php_path, php_code).unwrap();
    fs::write(&blade_path, blade).unwrap();

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
        .call(did_open_notification(&php_uri, php_code))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification_with_language(
            &blade_uri, "blade", blade,
        ))
        .await
        .unwrap();

    let diagnostics =
        next_publish_diagnostics(&mut notifications, &blade_uri, Duration::from_secs(1)).await;
    let diagnostic_items = diagnostics["diagnostics"]
        .as_array()
        .expect("diagnostics array");
    assert!(
        diagnostic_items.iter().any(|diagnostic| {
            diagnostic["message"]
                .as_str()
                .is_some_and(|message| message.starts_with("Unknown method: "))
                && diagnostic["range"]["start"]["line"].as_u64() == Some(0)
                && diagnostic["range"]["start"]["character"]
                    .as_u64()
                    .unwrap_or(0)
                    > 0
        }),
        "expected mapped Blade expression diagnostic, got: {}",
        diagnostics
    );
    assert!(
        diagnostic_items.iter().all(|diagnostic| {
            let message = diagnostic["message"].as_str().unwrap_or_default();
            message != "Syntax error" && !message.starts_with("Missing ")
        }),
        "template syntax noise should stay suppressed, got: {}",
        diagnostics
    );

    let _ = fs::remove_dir_all(&tmp_root);
    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_twig_template_context_hover_completion_definition_and_tokens() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    let tmp_root =
        std::env::temp_dir().join(format!("php-lsp-twig-template-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp_root);
    fs::create_dir_all(tmp_root.join("src/Controller")).unwrap();
    fs::create_dir_all(tmp_root.join("src/Entity")).unwrap();
    fs::create_dir_all(tmp_root.join("templates/dashboard")).unwrap();
    fs::create_dir_all(tmp_root.join("templates/shared")).unwrap();

    let root_uri = format!("file://{}", tmp_root.to_string_lossy());
    let user_path = tmp_root.join("src/Entity/User.php");
    let controller_path = tmp_root.join("src/Controller/DashboardController.php");
    let twig_path = tmp_root.join("templates/dashboard/show.html.twig");
    let card_path = tmp_root.join("templates/shared/_card.html.twig");
    let user_uri = format!("file://{}", user_path.to_string_lossy());
    let controller_uri = format!("file://{}", controller_path.to_string_lossy());
    let twig_uri = format!("file://{}", twig_path.to_string_lossy());
    let card_uri = format!("file://{}", card_path.to_string_lossy());

    let user_php = r#"<?php
namespace App\Entity;

class User
{
    public string $name = '';
    public function getName(): string { return $this->name; }
}
"#;
    let controller_php = r#"<?php
namespace App\Controller;

use App\Entity\User;

final class DashboardController
{
    public function show(): void
    {
        $this->render('dashboard/show.html.twig', [
            'user' => new User(),
            'users' => [new User()],
        ]);
    }
}
"#;
    let complete_marker = "/*complete*/";
    let template_marker = "/*template*/";
    let twig_with_markers = format!(
        "<h1>{{{{ user.name }}}}</h1>\n{{% for item in users %}}\n  {{{{ item.get{} }}}}\n{{% endfor %}}\n{{% include 'shared/_card.html.twig{}' %}}\n",
        complete_marker, template_marker
    );
    let marker_position = |marker: &str| -> (u32, u32) {
        let marker_offset = twig_with_markers
            .find(marker)
            .expect("test Twig should contain marker");
        let mut prefix = twig_with_markers[..marker_offset].to_string();
        prefix = prefix.replace(complete_marker, "");
        prefix = prefix.replace(template_marker, "");
        let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
        let line_start = prefix.rfind('\n').map(|idx| idx + 1).unwrap_or(0);
        (line, (prefix.len() - line_start) as u32)
    };
    let (completion_line, completion_character) = marker_position(complete_marker);
    let (template_line, template_character) = marker_position(template_marker);
    let twig = twig_with_markers
        .replace(complete_marker, "")
        .replace(template_marker, "");

    fs::write(&user_path, user_php).unwrap();
    fs::write(&controller_path, controller_php).unwrap();
    fs::write(&twig_path, &twig).unwrap();
    fs::write(&card_path, "<article>{{ user.name }}</article>\n").unwrap();

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
        .call(did_open_notification(&user_uri, user_php))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&controller_uri, controller_php))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification_with_language(
            &twig_uri, "twig", &twig,
        ))
        .await
        .unwrap();

    let diagnostics =
        next_publish_diagnostics(&mut notifications, &twig_uri, Duration::from_secs(1)).await;
    assert_eq!(
        diagnostics["diagnostics"].as_array().map(Vec::len),
        Some(0),
        "Twig HTML/control blocks should not produce noisy diagnostics, got: {}",
        diagnostics
    );

    let hover_resp = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(2, &twig_uri, 0, 8))
        .await
        .unwrap();
    let hover = extract_result(hover_resp);
    let hover_text = hover["contents"]["value"].as_str().unwrap_or_default();
    assert!(
        hover_text.contains("App\\Entity\\User") || hover_text.contains("User $user"),
        "expected Twig context variable hover to include User type, got: {}",
        hover
    );

    let definition_resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(3, &twig_uri, 0, 13))
        .await
        .unwrap();
    let definition = extract_result(definition_resp);
    assert_eq!(
        definition.get("uri").and_then(|uri| uri.as_str()),
        Some(user_uri.as_str()),
        "Twig member definition should jump to the PHP property"
    );

    let completion_resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(
            4,
            &twig_uri,
            completion_line,
            completion_character,
        ))
        .await
        .unwrap();
    let completion = extract_result(completion_resp);
    let labels: Vec<String> = completion_items_from_result(&completion)
        .iter()
        .filter_map(|item| item.get("label").and_then(|value| value.as_str()))
        .map(str::to_string)
        .collect();
    assert!(
        labels.iter().any(|label| label == "getName"),
        "expected Twig foreach item completion to include User::getName, got: {:?}",
        labels
    );

    let template_completion_resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(
            5,
            &twig_uri,
            template_line,
            template_character,
        ))
        .await
        .unwrap();
    let template_completion = extract_result(template_completion_resp);
    let template_labels: Vec<String> = completion_items_from_result(&template_completion)
        .iter()
        .filter_map(|item| item.get("label").and_then(|value| value.as_str()))
        .map(str::to_string)
        .collect();
    assert!(
        template_labels
            .iter()
            .any(|label| label == "shared/_card.html.twig"),
        "expected Twig include path completion, got: {:?}",
        template_labels
    );

    let template_definition_resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(
            6,
            &twig_uri,
            template_line,
            template_character,
        ))
        .await
        .unwrap();
    let template_definition = extract_result(template_definition_resp);
    assert_eq!(
        template_definition.get("uri").and_then(|uri| uri.as_str()),
        Some(card_uri.as_str()),
        "Twig include path definition should jump to the template file, got: {}",
        template_definition
    );

    let tokens_resp = service
        .ready()
        .await
        .unwrap()
        .call(semantic_tokens_full_request(7, &twig_uri))
        .await
        .unwrap();
    let tokens = decode_semantic_tokens(&extract_result(tokens_resp));
    assert!(
        tokens.iter().any(|(line, start, len, token_type, _)| {
            (*line, *start, *len, *token_type) == (1, 3, 3, 11)
        }),
        "expected Twig for keyword semantic token, got: {:?}",
        tokens
    );

    let _ = fs::remove_dir_all(&tmp_root);
    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_completion_member_access_after_class_string_template_factory() {
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

    let code_with_marker = r#"<?php
namespace App;

class Widget {
    public function render(): string { return ''; }
}

class ServiceLocator {
    /**
     * @template T of object
     * @param class-string<T> $class
     * @return T
     */
    public function make($class) {}
}

function run(ServiceLocator $locator): void {
    $locator->make(Widget::class)->/*caret*/
}
"#;
    let marker = "/*caret*/";
    let offset = code_with_marker
        .find(marker)
        .expect("test code should contain caret marker");
    let code = code_with_marker.replace(marker, "");
    let prefix = &code[..offset];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
    let line_start = prefix.rfind('\n').map(|idx| idx + 1).unwrap_or(0);
    let character = (prefix.len() - line_start) as u32;
    let uri = "file:///test/completion-class-string-template-factory.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, &code))
        .await
        .unwrap();

    let resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(2, uri, line, character))
        .await
        .unwrap();
    let result = extract_result(resp);
    let items = completion_items_from_result(&result);
    let labels: Vec<&str> = items
        .iter()
        .filter_map(|item| item.get("label").and_then(|label| label.as_str()))
        .collect();
    assert!(
        labels.contains(&"render"),
        "expected completion after class-string<T> factory to include Widget::render, got: {:?}; result: {}",
        labels,
        result
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}
