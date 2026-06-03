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
        "🇺🇸 👨‍👩‍👧‍👦 👍🏽 ❤️ é བོད <h1>{{{{- user.name -}}}}</h1>\n{{%- for item in users -%}}\n  {{{{- item.get{} -}}}}\n{{%- endfor -%}}\n{{%- include 'shared/_card.html.twig{}' -%}}\n",
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
        (line, prefix[line_start..].encode_utf16().count() as u32)
    };
    let (completion_line, completion_character) = marker_position(complete_marker);
    let (template_line, template_character) = marker_position(template_marker);
    let twig = twig_with_markers
        .replace(complete_marker, "")
        .replace(template_marker, "");
    let hover_position = utf16_position_at(&twig, "user.name");
    let definition_position = utf16_position_after(&twig, "user.n");

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
        .call(hover_request(
            2,
            &twig_uri,
            hover_position.0,
            hover_position.1,
        ))
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
        .call(definition_request(
            3,
            &twig_uri,
            definition_position.0,
            definition_position.1,
        ))
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
            (*line, *start, *len, *token_type) == (1, 4, 3, 11)
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
async fn test_twig_template_inlay_hints_are_mapped_to_original_source() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-twig-template-inlay-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp_root);
    fs::create_dir_all(tmp_root.join("src/Controller")).unwrap();
    fs::create_dir_all(tmp_root.join("src/Entity")).unwrap();
    fs::create_dir_all(tmp_root.join("templates/dashboard")).unwrap();

    let file_uri = |path: &std::path::Path| php_lsp_types::uri::path_to_uri(path).unwrap();
    let root_uri = file_uri(&tmp_root);
    let user_path = tmp_root.join("src/Entity/User.php");
    let controller_path = tmp_root.join("src/Controller/DashboardController.php");
    let twig_path = tmp_root.join("templates/dashboard/inlay.html.twig");
    let user_uri = file_uri(&user_path);
    let controller_uri = file_uri(&controller_path);
    let twig_uri = file_uri(&twig_path);

    let user_php = r#"<?php
namespace App\Entity;

class User
{
    public function getName(): string { return ''; }
    public function rename(string $name): string { return $name; }
}
"#;
    let controller_php = r#"<?php
namespace App\Controller;

use App\Entity\User;

final class DashboardController
{
    public function show(): void
    {
        $this->render('dashboard/inlay.html.twig', [
            'user' => new User(),
            'users' => [new User()],
        ]);
    }
}
"#;
    let twig = concat!(
        "🇺🇸 <h1>{{- user.rename('Alice') -}}</h1>\n",
        "{%- for item in users -%}\n",
        "  {{- item.getName() -}}\n",
        "{%- endfor -%}\n",
        "{%- set current = user -%}\n",
        "{{- current.getName() -}}\n",
    );
    let argument_position = utf16_position_at(twig, "'Alice'");
    let item_type_position = utf16_position_after(twig, "item");
    let current_type_position = utf16_position_after(twig, "current");

    fs::write(&user_path, user_php).unwrap();
    fs::write(&controller_path, controller_php).unwrap();
    fs::write(&twig_path, twig).unwrap();

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
        .call(did_open_notification_with_language(&twig_uri, "twig", twig))
        .await
        .unwrap();

    let diagnostics =
        next_publish_diagnostics(&mut notifications, &twig_uri, Duration::from_secs(1)).await;
    assert_eq!(
        diagnostics["diagnostics"].as_array().map(Vec::len),
        Some(0),
        "valid Twig should stay diagnostic-free before inlay hints, got: {}",
        diagnostics
    );

    let inlay_resp = service
        .ready()
        .await
        .unwrap()
        .call(inlay_hint_request(2, &twig_uri, 0, 0, 99, 0))
        .await
        .unwrap();
    let inlay_result = extract_result(inlay_resp);
    let hints = inlay_result
        .as_array()
        .expect("Twig inlayHint should return mapped hints");
    let labels: Vec<String> = hints.iter().filter_map(inlay_hint_label_text).collect();

    assert!(
        hints.iter().any(|hint| {
            inlay_hint_label_text(hint).as_deref() == Some("name:")
                && hint["position"]["line"].as_u64() == Some(argument_position.0 as u64)
                && hint["position"]["character"].as_u64() == Some(argument_position.1 as u64)
        }),
        "expected Twig call argument inlay hint at original string argument, got labels {:?}: {}",
        labels,
        inlay_result
    );
    assert!(
        hints.iter().any(|hint| {
            inlay_hint_label_text(hint).as_deref() == Some(": User")
                && hint["position"]["line"].as_u64() == Some(item_type_position.0 as u64)
                && hint["position"]["character"].as_u64()
                    == Some(item_type_position.1 as u64)
                && inlay_hint_has_label_part_location(hint, "User")
        }),
        "expected Twig foreach variable type inlay hint mapped to original item, got labels {:?}: {}",
        labels,
        inlay_result
    );
    assert!(
        hints.iter().any(|hint| {
            inlay_hint_label_text(hint).as_deref() == Some(": User")
                && hint["position"]["line"].as_u64() == Some(current_type_position.0 as u64)
                && hint["position"]["character"].as_u64()
                    == Some(current_type_position.1 as u64)
        }),
        "expected Twig set variable type inlay hint mapped to original current variable, got labels {:?}: {}",
        labels,
        inlay_result
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
async fn test_twig_context_infers_typed_controller_parameter_variables() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-twig-template-param-context-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp_root);
    fs::create_dir_all(tmp_root.join("src/Controller")).unwrap();
    fs::create_dir_all(tmp_root.join("src/Entity")).unwrap();
    fs::create_dir_all(tmp_root.join("templates/dashboard")).unwrap();

    let file_uri = |path: &std::path::Path| php_lsp_types::uri::path_to_uri(path).unwrap();
    let root_uri = file_uri(&tmp_root);
    let user_path = tmp_root.join("src/Entity/User.php");
    let controller_path = tmp_root.join("src/Controller/DashboardController.php");
    let twig_path = tmp_root.join("templates/dashboard/param.html.twig");
    let user_uri = file_uri(&user_path);
    let controller_uri = file_uri(&controller_path);
    let twig_uri = file_uri(&twig_path);

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
    public function fallbackOnly(): void
    {
        $user = null;
        $this->render('dashboard/param.html.twig', [
            'user' => $user,
        ]);
    }

    public function show(User $user): void
    {
        $messageLogs = [];
        $this->render('dashboard/param.html.twig', [
            'user' => $user,
            'messageLogs' => $messageLogs,
        ]);
    }
}
"#;
    let completion_marker = "/*complete*/";
    let twig_with_marker = format!(
        "{{{{ user.{} }}}}\n{{{{ user.name }}}}\n{{{{ messageLogs }}}}\n",
        completion_marker
    );
    let completion_offset = twig_with_marker
        .find(completion_marker)
        .expect("test Twig should contain completion marker");
    let completion_prefix = twig_with_marker[..completion_offset].replace(completion_marker, "");
    let completion_line = completion_prefix
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count() as u32;
    let completion_line_start = completion_prefix
        .rfind('\n')
        .map(|idx| idx + 1)
        .unwrap_or(0);
    let completion_character = completion_prefix[completion_line_start..]
        .encode_utf16()
        .count() as u32;
    let twig = twig_with_marker.replace(completion_marker, "");
    let hover_position = utf16_position_at(&twig, "user.name");
    let definition_position = utf16_position_after(&twig, "user.n");

    fs::write(&user_path, user_php).unwrap();
    fs::write(&controller_path, controller_php).unwrap();
    fs::write(&twig_path, &twig).unwrap();

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
        "typed parameter Twig context should suppress undefined variable diagnostics, got: {}",
        diagnostics
    );

    let hover_resp = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(
            2,
            &twig_uri,
            hover_position.0,
            hover_position.1,
        ))
        .await
        .unwrap();
    let hover = extract_result(hover_resp);
    let hover_text = hover["contents"]["value"].as_str().unwrap_or_default();
    assert!(
        hover_text.contains("?App\\Entity\\User $user"),
        "expected Twig hover to resolve typed controller parameter context, got: {}",
        hover
    );

    let completion_resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(
            3,
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
        labels.iter().any(|label| label == "getName") || labels.iter().any(|label| label == "name"),
        "expected Twig completion from typed controller parameter to include User members, got: {:?}",
        labels
    );

    let definition_resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(
            4,
            &twig_uri,
            definition_position.0,
            definition_position.1,
        ))
        .await
        .unwrap();
    let definition = extract_result(definition_resp);
    assert_eq!(
        definition.get("uri").and_then(|uri| uri.as_str()),
        Some(user_uri.as_str()),
        "Twig definition should jump from typed controller parameter member to PHP symbol, got: {}",
        definition
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
async fn test_twig_context_infers_nullable_conditional_render_variables() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-twig-nullable-render-context-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp_root);
    fs::create_dir_all(tmp_root.join("src/Controller")).unwrap();
    fs::create_dir_all(tmp_root.join("src/Entity")).unwrap();
    fs::create_dir_all(tmp_root.join("src/Repository")).unwrap();
    fs::create_dir_all(tmp_root.join("src/Service")).unwrap();
    fs::create_dir_all(tmp_root.join("templates/optional")).unwrap();

    let file_uri = |path: &std::path::Path| php_lsp_types::uri::path_to_uri(path).unwrap();
    let root_uri = file_uri(&tmp_root);
    let request_path = tmp_root.join("src/Entity/PortingRequest.php");
    let error_code_path = tmp_root.join("src/Entity/ErrorCode.php");
    let process_path = tmp_root.join("src/Entity/PortingProcess.php");
    let request_repository_path = tmp_root.join("src/Repository/PortingRequestRepository.php");
    let error_code_repository_path = tmp_root.join("src/Repository/ErrorCodeRepository.php");
    let manager_path = tmp_root.join("src/Service/PortingProcessManager.php");
    let controller_path = tmp_root.join("src/Controller/OptionalController.php");
    let twig_path = tmp_root.join("templates/optional/show.html.twig");
    let request_uri = file_uri(&request_path);
    let error_code_uri = file_uri(&error_code_path);
    let process_uri = file_uri(&process_path);
    let twig_uri = file_uri(&twig_path);

    let request_php = r#"<?php
namespace App\Entity;

class PortingRequest
{
    private int $id = 0;
    public function getId(): int { return $this->id; }
}
"#;
    let error_code_php = r#"<?php
namespace App\Entity;

use App\Repository\ErrorCodeRepository;
use Doctrine\ORM\Mapping as ORM;

#[ORM\Entity(repositoryClass: ErrorCodeRepository::class)]
class ErrorCode
{
    private string $description = '';
    public function getDescription(): string { return $this->description; }
}
"#;
    let process_php = r#"<?php
namespace App\Entity;

class PortingProcess
{
    private string $role = '';
    public function getRole(): string { return $this->role; }
}
"#;
    let request_repository_php = r#"<?php
namespace App\Repository;

use App\Entity\PortingRequest;
use Doctrine\Bundle\DoctrineBundle\Repository\ServiceEntityRepository;

/**
 * @extends ServiceEntityRepository<PortingRequest>
 */
class PortingRequestRepository extends ServiceEntityRepository
{
}
"#;
    let error_code_repository_php = r#"<?php
namespace App\Repository;

use Doctrine\Bundle\DoctrineBundle\Repository\ServiceEntityRepository;

class ErrorCodeRepository extends ServiceEntityRepository
{
}
"#;
    let manager_php = r#"<?php
namespace App\Service;

use App\Entity\PortingProcess;

class PortingProcessManager
{
    public function getOrCreateProcess(string $npId): ?PortingProcess
    {
        return null;
    }
}
"#;
    let controller_php = r#"<?php
namespace App\Controller;

use App\Repository\ErrorCodeRepository;
use App\Repository\PortingRequestRepository;
use App\Service\PortingProcessManager;

final class OptionalController
{
    public function __construct(private PortingProcessManager $portingProcessManager)
    {
    }

    public function show(
        string $npId,
        PortingRequestRepository $portingRequestRepository,
        ErrorCodeRepository $errorCodeRepository,
    ): void {
        $portingRequest = null;
        if ('' !== trim($npId)) {
            $portingRequest = $portingRequestRepository->findOneBy(['npId' => $npId]);
        }

        $errorCode = null;
        if ('' !== $npId) {
            $errorCode = $errorCodeRepository->findOneBy(['code' => 'ERR']);
        }

        $portingProcess = $this->portingProcessManager->getOrCreateProcess($npId);

        $this->render('optional/show.html.twig', [
            'portingRequest' => $portingRequest,
            'errorCode' => $errorCode,
            'portingProcess' => $portingProcess,
        ]);
    }
}
"#;
    let completion_marker = "/*complete*/";
    let twig_with_marker = format!(
        concat!(
            "{{% if portingRequest %}}\n",
            "{{{{ portingRequest.id }}}}\n",
            "{{% endif %}}\n",
            "{{% if errorCode and errorCode.description %}}\n",
            "{{{{ errorCode.description }}}}\n",
            "{{{{ errorCode.{} }}}}\n",
            "{{% endif %}}\n",
            "{{{{ portingProcess.role == 'donor' ? 'Donor' : 'Recipient' }}}}\n",
        ),
        completion_marker
    );
    let twig = twig_with_marker.replace(completion_marker, "");
    let completion_prefix = twig_with_marker[..twig_with_marker.find(completion_marker).unwrap()]
        .replace(completion_marker, "");
    let completion_position = utf16_position_for_offset(&twig, completion_prefix.len());
    let porting_request_root_position = utf16_position_at(&twig, "portingRequest %}");
    let porting_request_id_hover_position = utf16_position_for_offset(
        &twig,
        twig.find("portingRequest.id").unwrap() + "portingRequest.".len(),
    );
    let porting_request_id_definition_position = utf16_position_after(&twig, "portingRequest.i");
    let error_code_condition_position = utf16_position_for_offset(
        &twig,
        twig.find("errorCode.description %}").unwrap() + "errorCode.".len(),
    );
    let error_code_description_definition_position = utf16_position_after(&twig, "errorCode.d");
    let process_role_hover_position = utf16_position_for_offset(
        &twig,
        twig.find("portingProcess.role").unwrap() + "portingProcess.".len(),
    );
    let process_role_definition_position = utf16_position_after(&twig, "portingProcess.r");

    fs::write(&request_path, request_php).unwrap();
    fs::write(&error_code_path, error_code_php).unwrap();
    fs::write(&process_path, process_php).unwrap();
    fs::write(&request_repository_path, request_repository_php).unwrap();
    fs::write(&error_code_repository_path, error_code_repository_php).unwrap();
    fs::write(&manager_path, manager_php).unwrap();
    fs::write(&controller_path, controller_php).unwrap();
    fs::write(&twig_path, &twig).unwrap();

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
        .call(did_open_notification(&request_uri, request_php))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&error_code_uri, error_code_php))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&process_uri, process_php))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(
            &file_uri(&request_repository_path),
            request_repository_php,
        ))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(
            &file_uri(&error_code_repository_path),
            error_code_repository_php,
        ))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&file_uri(&manager_path), manager_php))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(
            &file_uri(&controller_path),
            controller_php,
        ))
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
        next_publish_diagnostics(&mut notifications, &twig_uri, Duration::from_secs(2)).await;
    assert_eq!(
        diagnostics["diagnostics"].as_array().map(Vec::len),
        Some(0),
        "nullable render context Twig fixture should stay diagnostic-clean, got: {}",
        diagnostics
    );

    let root_hover = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(hover_request(
                2,
                &twig_uri,
                porting_request_root_position.0,
                porting_request_root_position.1,
            ))
            .await
            .unwrap(),
    );
    let root_hover_text = hover_markdown_value(&root_hover);
    assert!(
        root_hover_text.contains("?App\\Entity\\PortingRequest $portingRequest"),
        "expected nullable PortingRequest root hover from conditional render context, got: {}",
        root_hover
    );

    let request_id_hover = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(hover_request(
                3,
                &twig_uri,
                porting_request_id_hover_position.0,
                porting_request_id_hover_position.1,
            ))
            .await
            .unwrap(),
    );
    let request_id_hover_text = hover_markdown_value(&request_id_hover);
    assert!(
        request_id_hover_text.contains("getId")
            || request_id_hover_text.contains("$id")
            || request_id_hover_text.contains("int"),
        "expected PortingRequest member hover from nullable context, got: {}",
        request_id_hover
    );
    let request_id_definition = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(definition_request(
                4,
                &twig_uri,
                porting_request_id_definition_position.0,
                porting_request_id_definition_position.1,
            ))
            .await
            .unwrap(),
    );
    assert_eq!(
        request_id_definition
            .get("uri")
            .and_then(|uri| uri.as_str()),
        Some(request_uri.as_str()),
        "expected PortingRequest.id definition to jump to entity symbol, got: {}",
        request_id_definition
    );

    let error_hover = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(hover_request(
                5,
                &twig_uri,
                error_code_condition_position.0,
                error_code_condition_position.1,
            ))
            .await
            .unwrap(),
    );
    let error_hover_text = hover_markdown_value(&error_hover);
    assert!(
        error_hover_text.contains("getDescription")
            || error_hover_text.contains("$description")
            || error_hover_text.contains("string"),
        "expected ErrorCode.description hover inside Twig condition, got: {}",
        error_hover
    );
    let error_definition = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(definition_request(
                6,
                &twig_uri,
                error_code_description_definition_position.0,
                error_code_description_definition_position.1,
            ))
            .await
            .unwrap(),
    );
    assert_eq!(
        error_definition.get("uri").and_then(|uri| uri.as_str()),
        Some(error_code_uri.as_str()),
        "expected ErrorCode.description definition to jump to entity symbol, got: {}",
        error_definition
    );

    let process_hover = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(hover_request(
                7,
                &twig_uri,
                process_role_hover_position.0,
                process_role_hover_position.1,
            ))
            .await
            .unwrap(),
    );
    let process_hover_text = hover_markdown_value(&process_hover);
    assert!(
        process_hover_text.contains("getRole")
            || process_hover_text.contains("$role")
            || process_hover_text.contains("string"),
        "expected PortingProcess.role hover from service member call render context, got: {}",
        process_hover
    );
    let process_definition = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(definition_request(
                8,
                &twig_uri,
                process_role_definition_position.0,
                process_role_definition_position.1,
            ))
            .await
            .unwrap(),
    );
    assert_eq!(
        process_definition.get("uri").and_then(|uri| uri.as_str()),
        Some(process_uri.as_str()),
        "expected PortingProcess.role definition to jump to entity symbol, got: {}",
        process_definition
    );

    let completion = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(completion_request(
                9,
                &twig_uri,
                completion_position.0,
                completion_position.1,
            ))
            .await
            .unwrap(),
    );
    let labels: Vec<String> = completion_items_from_result(&completion)
        .iter()
        .filter_map(|item| item.get("label").and_then(|value| value.as_str()))
        .map(str::to_string)
        .collect();
    assert!(
        labels.iter().any(|label| label == "description")
            || labels.iter().any(|label| label == "getDescription"),
        "expected ErrorCode property-style completion from nullable render context, got: {:?}",
        labels
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
async fn test_twig_context_infers_paginated_repository_item_variables() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-twig-pagination-context-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp_root);
    fs::create_dir_all(tmp_root.join("src/Controller")).unwrap();
    fs::create_dir_all(tmp_root.join("src/Entity")).unwrap();
    fs::create_dir_all(tmp_root.join("src/Repository")).unwrap();
    fs::create_dir_all(tmp_root.join("templates/data_request")).unwrap();

    let file_uri = |path: &std::path::Path| php_lsp_types::uri::path_to_uri(path).unwrap();
    let root_uri = file_uri(&tmp_root);
    let entity_path = tmp_root.join("src/Entity/DataRequest.php");
    let subscriber_path = tmp_root.join("src/Entity/Subscriber.php");
    let status_path = tmp_root.join("src/Entity/DataRequestStatus.php");
    let repository_path = tmp_root.join("src/Repository/DataRequestRepository.php");
    let controller_path = tmp_root.join("src/Controller/DataRequestController.php");
    let twig_path = tmp_root.join("templates/data_request/index.html.twig");
    let entity_uri = file_uri(&entity_path);
    let subscriber_uri = file_uri(&subscriber_path);
    let twig_uri = file_uri(&twig_path);

    let entity_php = r#"<?php
namespace App\Entity;

class DataRequest
{
    private int $id = 0;
    private string $npId = '';
    /** @var array<int, string> */
    private $numbers = [];
    private ?DataRequestStatus $status = null;
    private ?Subscriber $subscriber = null;
    private \DateTimeImmutable $createdAt;

    public function getId(): int { return $this->id; }
    public function getNpId(): string { return $this->npId; }
    public function getNumbers(): array { return $this->numbers; }
    public function getStatus(): ?DataRequestStatus { return $this->status; }
    public function getSubscriber(): ?Subscriber { return $this->subscriber; }
    public function getCreatedAt(): \DateTimeImmutable { return $this->createdAt; }
    public function getDisplayName(): string { return (string) $this->id; }
    public function getFormattedId(string $prefix): string { return $prefix.$this->id; }
}
"#;
    let subscriber_php = r#"<?php
namespace App\Entity;

class Subscriber
{
    private int $id = 0;
    public function getId(): int { return $this->id; }
}
"#;
    let status_php = r#"<?php
namespace App\Entity;

class DataRequestStatus
{
    public function bootstrapBadgeClass(): string { return ''; }
    public function label(): string { return ''; }
}
"#;
    let repository_php = r#"<?php
namespace App\Repository;

use App\Entity\DataRequest;
use Doctrine\Bundle\DoctrineBundle\Repository\ServiceEntityRepository;
use Doctrine\ORM\QueryBuilder;

class DataRequestRepository extends ServiceEntityRepository
{
    public function createIndexQb(): QueryBuilder
    {
        return $this->createQueryBuilder('dr');
    }
}
"#;
    let controller_php = r#"<?php
namespace App\Controller;

use App\Repository\DataRequestRepository;
use Knp\Component\Pager\PaginatorInterface;

final class DataRequestController
{
    public function index(PaginatorInterface $paginator, DataRequestRepository $dataRequestRepository): void
    {
        $qb = $dataRequestRepository->createIndexQb();
        $pagination = $paginator->paginate($qb, 1, 10);

        $this->render('data_request/index.html.twig', [
            'pagination' => $pagination,
        ]);
    }
}
"#;
    let completion_marker = "/*complete*/";
    let path_completion_marker = "/*pathcomplete*/";
    let twig_with_markers = format!(
        concat!(
            "{{% for dr in pagination %}}\n",
            "{{{{ dr.{} }}}}\n",
            "{{{{ dr.id }}}}\n",
            "{{{{ dr.displayName }}}}\n",
            "{{% if dr.numbers is iterable and dr.numbers|length > 0 %}}\n",
            "{{% set shown = dr.numbers|slice(0, 5) %}}\n",
            "{{% for num in shown %}}{{{{ num }}}}{{% endfor %}}\n",
            "{{% if dr.status is not null %}}\n",
            "{{% set badgeClass = dr.status.bootstrapBadgeClass() %}}\n",
            "{{{{ badgeClass }}}} {{{{ dr.status.label() }}}}\n",
            "{{% endif %}}\n",
            "{{{{ path('subscriber_show', {{'id': dr.subscriber.id}}) }}}}\n",
            "{{{{ path('subscriber_show', {{'id': dr.{}}}) }}}}\n",
            "{{{{ dr.subscriber.id }}}}\n",
            "{{{{ dr.createdAt|date('d.m.Y') }}}}\n",
            "{{{{ path('data_request_show', {{'id': dr.id}}) }}}}\n",
            "{{% endif %}}\n",
            "{{% endfor %}}\n"
        ),
        completion_marker, path_completion_marker
    );
    let twig = twig_with_markers
        .replace(completion_marker, "")
        .replace(path_completion_marker, "");
    let marker_position = |marker: &str| {
        let marker_offset = twig_with_markers
            .find(marker)
            .expect("test Twig should contain marker");
        let prefix = twig_with_markers[..marker_offset]
            .replace(completion_marker, "")
            .replace(path_completion_marker, "");
        utf16_position_for_offset(&twig, prefix.len())
    };
    let completion_position = marker_position(completion_marker);
    let path_trailing_completion_position = marker_position(path_completion_marker);
    let hover_position = utf16_position_at(&twig, "dr.id");
    let definition_position = utf16_position_after(&twig, "dr.i");
    let display_name_position = utf16_position_at(&twig, "displayName");
    let foreach_variable_position = utf16_position_after(&twig, "dr");
    let filter_completion_position =
        utf16_position_for_offset(&twig, twig.find("dr.numbers is").unwrap() + "dr.".len());
    let nested_path_offset = twig.find("dr.subscriber.id})").unwrap();
    let nested_path_position =
        utf16_position_for_offset(&twig, nested_path_offset + "dr.subscriber.".len());
    let path_id_offset = twig.find("dr.id})").unwrap();
    let path_id_position = utf16_position_for_offset(&twig, path_id_offset + "dr.".len());
    let shown_variable_offset = twig.find("shown =").unwrap();
    let shown_variable_position =
        utf16_position_for_offset(&twig, shown_variable_offset + "shown".len());
    let num_foreach_offset = twig.find("num in shown").unwrap();
    let num_foreach_variable_position =
        utf16_position_for_offset(&twig, num_foreach_offset + "num".len());
    let shown_usage_position =
        utf16_position_for_offset(&twig, num_foreach_offset + "num in ".len());
    let num_hover_offset = twig.find("num }}").unwrap();
    let num_hover_position = utf16_position_for_offset(&twig, num_hover_offset + 1);
    let end_position = utf16_position_for_offset(&twig, twig.len());

    fs::write(&entity_path, entity_php).unwrap();
    fs::write(&subscriber_path, subscriber_php).unwrap();
    fs::write(&status_path, status_php).unwrap();
    fs::write(&repository_path, repository_php).unwrap();
    fs::write(&controller_path, controller_php).unwrap();
    fs::write(&twig_path, &twig).unwrap();

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
        .call(did_open_notification_with_language(
            &twig_uri, "twig", &twig,
        ))
        .await
        .unwrap();
    let diagnostics =
        next_publish_diagnostics(&mut notifications, &twig_uri, Duration::from_secs(2)).await;
    assert_eq!(
        diagnostics["diagnostics"].as_array().map(Vec::len),
        Some(0),
        "early opened paginated Twig context should stay diagnostic-clean, got: {}",
        diagnostics
    );

    service
        .ready()
        .await
        .unwrap()
        .call(initialized_notification())
        .await
        .unwrap();
    wait_for_indexing_phase(&mut notifications, "ready", Duration::from_secs(5)).await;
    let refreshed_diagnostics =
        next_publish_diagnostics(&mut notifications, &twig_uri, Duration::from_secs(2)).await;
    assert_eq!(
        refreshed_diagnostics["diagnostics"]
            .as_array()
            .map(Vec::len),
        Some(0),
        "refreshed paginated Twig context should stay diagnostic-clean, got: {}",
        refreshed_diagnostics
    );

    let hover_resp = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(
            2,
            &twig_uri,
            hover_position.0,
            hover_position.1,
        ))
        .await
        .unwrap();
    let hover = extract_result(hover_resp);
    let hover_text = hover["contents"]["value"].as_str().unwrap_or_default();
    assert!(
        hover_text.contains("DataRequest $dr"),
        "expected Twig hover to resolve paginator item variable type, got: {}",
        hover
    );

    let completion_resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(
            3,
            &twig_uri,
            completion_position.0,
            completion_position.1,
        ))
        .await
        .unwrap();
    let completion = extract_result(completion_resp);
    let labels: Vec<String> = completion_items_from_result(&completion)
        .iter()
        .filter_map(|item| item.get("label").and_then(|value| value.as_str()))
        .map(str::to_string)
        .collect();
    for expected in [
        "id",
        "npId",
        "numbers",
        "status",
        "subscriber",
        "createdAt",
        "displayName",
    ] {
        assert!(
            labels.iter().any(|label| label == expected),
            "expected Twig property-style completion `{expected}` from paginator item, got: {:?}",
            labels
        );
    }
    assert!(
        !labels.iter().any(|label| label == "formattedId"),
        "Twig property-style completion should not alias getters with required arguments, got: {:?}",
        labels
    );

    let filter_completion_resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(
            33,
            &twig_uri,
            filter_completion_position.0,
            filter_completion_position.1,
        ))
        .await
        .unwrap();
    let filter_completion = extract_result(filter_completion_resp);
    let filter_labels: Vec<String> = completion_items_from_result(&filter_completion)
        .iter()
        .filter_map(|item| item.get("label").and_then(|value| value.as_str()))
        .map(str::to_string)
        .collect();
    assert!(
        filter_labels.iter().any(|label| label == "numbers"),
        "expected Twig completion inside filter/test expression to include DataRequest property aliases, got: {:?}",
        filter_labels
    );

    let nested_completion_resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(
            34,
            &twig_uri,
            nested_path_position.0,
            nested_path_position.1,
        ))
        .await
        .unwrap();
    let nested_completion = extract_result(nested_completion_resp);
    let nested_labels: Vec<String> = completion_items_from_result(&nested_completion)
        .iter()
        .filter_map(|item| item.get("label").and_then(|value| value.as_str()))
        .map(str::to_string)
        .collect();
    assert!(
        nested_labels.iter().any(|label| label == "id"),
        "expected Twig completion inside path() nested member chain to include Subscriber property aliases, got: {:?}",
        nested_labels
    );

    let path_trailing_completion_resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(
            35,
            &twig_uri,
            path_trailing_completion_position.0,
            path_trailing_completion_position.1,
        ))
        .await
        .unwrap();
    let path_trailing_completion = extract_result(path_trailing_completion_resp);
    let path_trailing_labels: Vec<String> = completion_items_from_result(&path_trailing_completion)
        .iter()
        .filter_map(|item| item.get("label").and_then(|value| value.as_str()))
        .map(str::to_string)
        .collect();
    assert!(
        path_trailing_labels.iter().any(|label| label == "id")
            && path_trailing_labels
                .iter()
                .any(|label| label == "displayName"),
        "expected Twig completion after trailing `dr.` inside path() to include DataRequest property aliases, got: {:?}",
        path_trailing_labels
    );

    let definition_resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(
            4,
            &twig_uri,
            definition_position.0,
            definition_position.1,
        ))
        .await
        .unwrap();
    let definition = extract_result(definition_resp);
    assert_eq!(
        definition.get("uri").and_then(|uri| uri.as_str()),
        Some(entity_uri.as_str()),
        "Twig definition should jump from paginator item member to PHP symbol, got: {}",
        definition
    );

    let display_name_hover_resp = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(
            36,
            &twig_uri,
            display_name_position.0,
            display_name_position.1,
        ))
        .await
        .unwrap();
    let display_name_hover = extract_result(display_name_hover_resp);
    let display_name_hover_text = display_name_hover["contents"]["value"]
        .as_str()
        .unwrap_or_default();
    assert!(
        display_name_hover_text.contains("getDisplayName"),
        "expected Twig getter-derived property alias hover to resolve backing getter, got: {}",
        display_name_hover
    );

    let display_name_definition_resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(
            37,
            &twig_uri,
            display_name_position.0,
            display_name_position.1,
        ))
        .await
        .unwrap();
    let display_name_definition = extract_result(display_name_definition_resp);
    assert_eq!(
        display_name_definition
            .get("uri")
            .and_then(|uri| uri.as_str()),
        Some(entity_uri.as_str()),
        "Twig getter-derived property alias definition should jump to backing getter, got: {}",
        display_name_definition
    );

    for (request_id, needle, expected_hover) in [
        (40, "numbers is", "numbers"),
        (41, "numbers|slice", "numbers"),
        (42, "status is", "status"),
        (43, "id})", "id"),
        (44, "createdAt|date", "DateTimeImmutable"),
    ] {
        let position = utf16_position_at(&twig, needle);
        let hover_resp = service
            .ready()
            .await
            .unwrap()
            .call(hover_request(request_id, &twig_uri, position.0, position.1))
            .await
            .unwrap();
        let hover = extract_result(hover_resp);
        let hover_text = hover["contents"]["value"].as_str().unwrap_or_default();
        assert!(
            hover_text.contains(expected_hover),
            "expected Twig hover on `{needle}` to include `{expected_hover}`, got: {}",
            hover
        );
    }

    let shown_hover_resp = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(
            45,
            &twig_uri,
            shown_usage_position.0,
            shown_usage_position.1,
        ))
        .await
        .unwrap();
    let shown_hover = extract_result(shown_hover_resp);
    let shown_hover_text = shown_hover["contents"]["value"]
        .as_str()
        .unwrap_or_default();
    assert!(
        shown_hover_text.contains("$shown")
            && (shown_hover_text.contains("array<int, string>")
                || shown_hover_text.contains("array")),
        "expected Twig set variable hover to keep slice base collection type, got: {}",
        shown_hover
    );

    let num_hover_resp = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(
            46,
            &twig_uri,
            num_hover_position.0,
            num_hover_position.1,
        ))
        .await
        .unwrap();
    let num_hover = extract_result(num_hover_resp);
    let num_hover_text = num_hover["contents"]["value"].as_str().unwrap_or_default();
    assert!(
        num_hover_text.contains("string $num"),
        "expected Twig foreach item hover to infer string from sliced list, got: {}",
        num_hover
    );

    for (request_id, position, expected_uri) in [
        (
            50,
            utf16_position_at(&twig, "numbers is"),
            entity_uri.as_str(),
        ),
        (
            51,
            utf16_position_at(&twig, "status is"),
            entity_uri.as_str(),
        ),
        (52, nested_path_position, subscriber_uri.as_str()),
        (53, path_id_position, entity_uri.as_str()),
        (
            54,
            utf16_position_at(&twig, "createdAt|date"),
            entity_uri.as_str(),
        ),
    ] {
        let definition_resp = service
            .ready()
            .await
            .unwrap()
            .call(definition_request(
                request_id, &twig_uri, position.0, position.1,
            ))
            .await
            .unwrap();
        let definition = extract_result(definition_resp);
        assert_eq!(
            definition.get("uri").and_then(|uri| uri.as_str()),
            Some(expected_uri),
            "Twig definition at request {request_id} should jump to PHP symbol, got: {}",
            definition
        );
    }

    let inlay_resp = service
        .ready()
        .await
        .unwrap()
        .call(inlay_hint_request(
            5,
            &twig_uri,
            0,
            0,
            end_position.0,
            end_position.1,
        ))
        .await
        .unwrap();
    let inlay_result = extract_result(inlay_resp);
    let hints = inlay_result.as_array().cloned().unwrap_or_default();
    assert!(
        hints.iter().any(|hint| {
            inlay_hint_label_text(hint).as_deref() == Some(": DataRequest")
                && hint["position"]["line"].as_u64() == Some(foreach_variable_position.0 as u64)
                && hint["position"]["character"].as_u64()
                    == Some(foreach_variable_position.1 as u64)
        }),
        "expected Twig inlay hint for paginator item variable, got: {}",
        inlay_result
    );
    assert!(
        hints.iter().any(|hint| {
            inlay_hint_label_text(hint)
                .is_some_and(|label| label == ": array<int, string>" || label == ": array")
                && hint["position"]["line"].as_u64() == Some(shown_variable_position.0 as u64)
                && hint["position"]["character"].as_u64() == Some(shown_variable_position.1 as u64)
        }),
        "expected Twig inlay hint for sliced set variable, got: {}",
        inlay_result
    );
    assert!(
        hints.iter().any(|hint| {
            inlay_hint_label_text(hint).as_deref() == Some(": string")
                && hint["position"]["line"].as_u64() == Some(num_foreach_variable_position.0 as u64)
                && hint["position"]["character"].as_u64()
                    == Some(num_foreach_variable_position.1 as u64)
        }),
        "expected Twig inlay hint for sliced foreach item variable, got: {}",
        inlay_result
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
async fn test_twig_context_infers_dto_service_results_and_include_item_context() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-twig-dto-service-context-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp_root);
    fs::create_dir_all(tmp_root.join("src/Controller")).unwrap();
    fs::create_dir_all(tmp_root.join("src/Entity")).unwrap();
    fs::create_dir_all(tmp_root.join("src/Repository")).unwrap();
    fs::create_dir_all(tmp_root.join("src/Service/SftpCsv")).unwrap();
    fs::create_dir_all(tmp_root.join("templates/sftp_csv")).unwrap();
    fs::create_dir_all(tmp_root.join("templates/data_request")).unwrap();
    fs::create_dir_all(tmp_root.join("templates/components")).unwrap();

    let file_uri = |path: &std::path::Path| php_lsp_types::uri::path_to_uri(path).unwrap();
    let root_uri = file_uri(&tmp_root);
    let metadata_path = tmp_root.join("src/Service/SftpCsv/SftpCsvArchiveMetadata.php");
    let preview_path = tmp_root.join("src/Service/SftpCsv/SftpCsvPreview.php");
    let catalog_path = tmp_root.join("src/Service/SftpCsv/SftpCsvFileCatalog.php");
    let previewer_path = tmp_root.join("src/Service/SftpCsv/SftpCsvArchivePreviewer.php");
    let error_code_path = tmp_root.join("src/Entity/ErrorCode.php");
    let error_code_repository_path = tmp_root.join("src/Repository/ErrorCodeRepository.php");
    let controller_path = tmp_root.join("src/Controller/SftpCsvViewerController.php");
    let data_controller_path = tmp_root.join("src/Controller/DataRequestController.php");
    let index_twig_path = tmp_root.join("templates/sftp_csv/index.html.twig");
    let view_twig_path = tmp_root.join("templates/sftp_csv/view.html.twig");
    let data_twig_path = tmp_root.join("templates/data_request/show.html.twig");
    let component_twig_path = tmp_root.join("templates/components/autocomplete_input.html.twig");

    let metadata_uri = file_uri(&metadata_path);
    let preview_uri = file_uri(&preview_path);
    let error_code_uri = file_uri(&error_code_path);
    let index_twig_uri = file_uri(&index_twig_path);
    let view_twig_uri = file_uri(&view_twig_path);
    let data_twig_uri = file_uri(&data_twig_path);
    let component_twig_uri = file_uri(&component_twig_path);

    let metadata_php = r#"<?php
namespace App\Service\SftpCsv;

final readonly class SftpCsvArchiveMetadata
{
    public function __construct(
        public string $type,
        public string $name,
        public string $path,
        public int $size = 0,
    ) {
    }
}
"#;
    let preview_php = r#"<?php
namespace App\Service\SftpCsv;

final readonly class SftpCsvPreview
{
    public function __construct(
        public string $csvName,
        public int $page,
        public int $perPage,
    ) {
    }

    public function hasPreviousPage(): bool
    {
        return $this->page > 1;
    }

    public function getPerPageQueryValue(): string
    {
        return (string)$this->perPage;
    }
}
"#;
    let catalog_php = r#"<?php
namespace App\Service\SftpCsv;

final class SftpCsvFileCatalog
{
    public function createFromDirectoryItem(string $type, array $item): ?SftpCsvArchiveMetadata
    {
        return null;
    }

    public function createFromSelection(string $type, string $fileName): SftpCsvArchiveMetadata
    {
        return new SftpCsvArchiveMetadata($type, $fileName, '/tmp/'.$fileName);
    }
}
"#;
    let previewer_php = r#"<?php
namespace App\Service\SftpCsv;

final class SftpCsvArchivePreviewer
{
    public function preview(string $remotePath, int $page, int $perPage): SftpCsvPreview
    {
        return new SftpCsvPreview('report.csv', $page, $perPage);
    }
}
"#;
    let error_code_php = r#"<?php
namespace App\Entity;

class ErrorCode
{
    private int $code = 0;
    private ?string $description = null;

    public function getCode(): int { return $this->code; }
    public function getDescription(): ?string { return $this->description; }
}
"#;
    let error_code_repository_php = r#"<?php
namespace App\Repository;

use App\Entity\ErrorCode;
use Doctrine\Bundle\DoctrineBundle\Repository\ServiceEntityRepository;

/**
 * @extends ServiceEntityRepository<ErrorCode>
 */
class ErrorCodeRepository extends ServiceEntityRepository
{
}
"#;
    let controller_php = r#"<?php
namespace App\Controller;

use App\Service\SftpCsv\SftpCsvArchiveMetadata;
use App\Service\SftpCsv\SftpCsvArchivePreviewer;
use App\Service\SftpCsv\SftpCsvFileCatalog;

final class SftpCsvViewerController
{
    public function __construct(
        private readonly SftpCsvFileCatalog $catalog,
        private readonly SftpCsvArchivePreviewer $previewer,
    ) {
    }

    public function index(): void
    {
        $type = 'incoming';
        $files = [];
        foreach ([['name' => 'archive.zip']] as $item) {
            $metadata = $this->catalog->createFromDirectoryItem($type, $item);
            if ($metadata instanceof SftpCsvArchiveMetadata) {
                $files[] = $metadata;
            }
        }

        $this->render('sftp_csv/index.html.twig', [
            'files' => $files,
        ]);
    }

    public function view(): void
    {
        $metadata = $this->catalog->createFromSelection('incoming', 'archive.zip');
        $preview = $this->previewer->preview($metadata->path, 1, 50);

        $this->render('sftp_csv/view.html.twig', [
            'file' => $metadata,
            'preview' => $preview,
        ]);
    }
}
"#;
    let data_controller_php = r#"<?php
namespace App\Controller;

use App\Repository\ErrorCodeRepository;

final class DataRequestController
{
    public function show(ErrorCodeRepository $errorCodeRepository): void
    {
        $errorCodes = $errorCodeRepository->findAll();

        $this->render('data_request/show.html.twig', [
            'errorCodes' => $errorCodes,
        ]);
    }
}
"#;
    let file_completion_marker = "/*filecomplete*/";
    let index_twig_with_marker = format!(
        concat!(
            "{{% for file in files %}}\n",
            "{{{{ file.{}name }}}} {{{{ file.type }}}}\n",
            "{{% endfor %}}\n"
        ),
        file_completion_marker
    );
    let index_twig = index_twig_with_marker.replace(file_completion_marker, "");
    let view_twig = concat!(
        "{{ file.name }}\n",
        "{{ preview.csvName }}\n",
        "{{ preview.perPageQueryValue }}\n",
        "{{ preview.hasPreviousPage() ? 'yes' : 'no' }}\n"
    );
    let data_twig = concat!(
        "{% include 'components/autocomplete_input.html.twig' with {\n",
        "    'id': 'reject_code',\n",
        "    'name': 'reject_code',\n",
        "    'items': errorCodes\n",
        "} %}\n"
    );
    let item_completion_marker = "/*itemcomplete*/";
    let component_twig_with_marker = format!(
        concat!(
            "{{% for item in items %}}\n",
            "{{{{ item.{}code }}}} {{{{ item.description }}}}\n",
            "{{% endfor %}}\n"
        ),
        item_completion_marker
    );
    let component_twig = component_twig_with_marker.replace(item_completion_marker, "");

    let file_completion_prefix = index_twig_with_marker
        [..index_twig_with_marker.find(file_completion_marker).unwrap()]
        .replace(file_completion_marker, "");
    let file_completion_position =
        utf16_position_for_offset(&index_twig, file_completion_prefix.len());
    let index_file_name_hover_position = utf16_position_at(&index_twig, "name }}");
    let index_file_name_definition_position = utf16_position_after(&index_twig, "file.n");
    let file_inlay_offset = index_twig.find("file in files").unwrap();
    let file_inlay_position =
        utf16_position_for_offset(&index_twig, file_inlay_offset + "file".len());
    let index_end_position = utf16_position_for_offset(&index_twig, index_twig.len());
    let view_file_name_hover_position = utf16_position_at(view_twig, "name }}");
    let view_file_name_definition_position = utf16_position_after(view_twig, "file.n");
    let csv_name_hover_position = utf16_position_at(view_twig, "csvName");
    let csv_name_definition_position = utf16_position_after(view_twig, "preview.c");
    let per_page_hover_position = utf16_position_at(view_twig, "perPageQueryValue");
    let per_page_definition_position = utf16_position_after(view_twig, "preview.p");
    let has_previous_hover_position = utf16_position_at(view_twig, "hasPreviousPage");
    let has_previous_definition_position = utf16_position_after(view_twig, "preview.h");
    let item_completion_prefix = component_twig_with_marker[..component_twig_with_marker
        .find(item_completion_marker)
        .unwrap()]
        .replace(item_completion_marker, "");
    let item_completion_position =
        utf16_position_for_offset(&component_twig, item_completion_prefix.len());
    let item_code_hover_position = utf16_position_at(&component_twig, "code }}");
    let item_code_definition_position = utf16_position_after(&component_twig, "item.c");
    let item_inlay_offset = component_twig.find("item in items").unwrap();
    let item_inlay_position =
        utf16_position_for_offset(&component_twig, item_inlay_offset + "item".len());
    let component_end_position = utf16_position_for_offset(&component_twig, component_twig.len());

    fs::write(&metadata_path, metadata_php).unwrap();
    fs::write(&preview_path, preview_php).unwrap();
    fs::write(&catalog_path, catalog_php).unwrap();
    fs::write(&previewer_path, previewer_php).unwrap();
    fs::write(&error_code_path, error_code_php).unwrap();
    fs::write(&error_code_repository_path, error_code_repository_php).unwrap();
    fs::write(&controller_path, controller_php).unwrap();
    fs::write(&data_controller_path, data_controller_php).unwrap();
    fs::write(&index_twig_path, &index_twig).unwrap();
    fs::write(&view_twig_path, view_twig).unwrap();
    fs::write(&data_twig_path, data_twig).unwrap();
    fs::write(&component_twig_path, &component_twig).unwrap();

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
    wait_for_indexing_phase(&mut notifications, "ready", Duration::from_secs(5)).await;
    for (uri, language, source) in [
        (index_twig_uri.as_str(), "twig", index_twig.as_str()),
        (view_twig_uri.as_str(), "twig", view_twig),
        (data_twig_uri.as_str(), "twig", data_twig),
        (component_twig_uri.as_str(), "twig", component_twig.as_str()),
    ] {
        service
            .ready()
            .await
            .unwrap()
            .call(did_open_notification_with_language(uri, language, source))
            .await
            .unwrap();
    }

    for (uri, label) in [
        (&index_twig_uri, "index"),
        (&view_twig_uri, "view"),
        (&data_twig_uri, "include caller"),
        (&component_twig_uri, "include component"),
    ] {
        let diagnostics =
            next_publish_diagnostics(&mut notifications, uri, Duration::from_secs(2)).await;
        assert_eq!(
            diagnostics["diagnostics"].as_array().map(Vec::len),
            Some(0),
            "DTO/service Twig {label} fixture should stay diagnostic-clean, got: {}",
            diagnostics
        );
    }

    for (request_id, uri, position, expected_labels) in [
        (
            2,
            index_twig_uri.as_str(),
            file_completion_position,
            ["name", "type"].as_slice(),
        ),
        (
            3,
            component_twig_uri.as_str(),
            item_completion_position,
            ["code", "description"].as_slice(),
        ),
    ] {
        let completion = extract_result(
            service
                .ready()
                .await
                .unwrap()
                .call(completion_request(request_id, uri, position.0, position.1))
                .await
                .unwrap(),
        );
        let labels: Vec<String> = completion_items_from_result(&completion)
            .iter()
            .filter_map(|item| item.get("label").and_then(|value| value.as_str()))
            .map(str::to_string)
            .collect();
        for expected in expected_labels {
            assert!(
                labels.iter().any(|label| label == expected),
                "expected Twig completion request {request_id} to include `{expected}`, got: {:?}",
                labels
            );
        }
    }

    for (request_id, uri, position, expected_text) in [
        (
            4,
            index_twig_uri.as_str(),
            index_file_name_hover_position,
            "SftpCsvArchiveMetadata::$name",
        ),
        (
            5,
            view_twig_uri.as_str(),
            view_file_name_hover_position,
            "SftpCsvArchiveMetadata::$name",
        ),
        (
            6,
            view_twig_uri.as_str(),
            csv_name_hover_position,
            "SftpCsvPreview::$csvName",
        ),
        (
            7,
            view_twig_uri.as_str(),
            per_page_hover_position,
            "getPerPageQueryValue",
        ),
        (
            8,
            view_twig_uri.as_str(),
            has_previous_hover_position,
            "hasPreviousPage",
        ),
        (
            9,
            component_twig_uri.as_str(),
            item_code_hover_position,
            "ErrorCode::$code",
        ),
    ] {
        let hover = extract_result(
            service
                .ready()
                .await
                .unwrap()
                .call(hover_request(request_id, uri, position.0, position.1))
                .await
                .unwrap(),
        );
        assert!(
            hover_markdown_value(&hover).contains(expected_text),
            "expected Twig hover request {request_id} to include `{expected_text}`, got: {}",
            hover
        );
    }

    for (request_id, uri, position, expected_uri) in [
        (
            10,
            index_twig_uri.as_str(),
            index_file_name_definition_position,
            metadata_uri.as_str(),
        ),
        (
            11,
            view_twig_uri.as_str(),
            view_file_name_definition_position,
            metadata_uri.as_str(),
        ),
        (
            12,
            view_twig_uri.as_str(),
            csv_name_definition_position,
            preview_uri.as_str(),
        ),
        (
            13,
            view_twig_uri.as_str(),
            per_page_definition_position,
            preview_uri.as_str(),
        ),
        (
            14,
            view_twig_uri.as_str(),
            has_previous_definition_position,
            preview_uri.as_str(),
        ),
        (
            15,
            component_twig_uri.as_str(),
            item_code_definition_position,
            error_code_uri.as_str(),
        ),
    ] {
        let definition = extract_result(
            service
                .ready()
                .await
                .unwrap()
                .call(definition_request(request_id, uri, position.0, position.1))
                .await
                .unwrap(),
        );
        assert_eq!(
            definition.get("uri").and_then(|uri| uri.as_str()),
            Some(expected_uri),
            "expected Twig definition request {request_id} to resolve `{expected_uri}`, got: {}",
            definition
        );
    }

    let index_inlay = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(inlay_hint_request(
                16,
                &index_twig_uri,
                0,
                0,
                index_end_position.0,
                index_end_position.1,
            ))
            .await
            .unwrap(),
    );
    let index_hints = index_inlay.as_array().cloned().unwrap_or_default();
    assert!(
        index_hints.iter().any(|hint| {
            inlay_hint_label_text(hint)
                .is_some_and(|label| label.contains("SftpCsvArchiveMetadata"))
                && hint["position"]["line"].as_u64() == Some(file_inlay_position.0 as u64)
                && hint["position"]["character"].as_u64() == Some(file_inlay_position.1 as u64)
                && inlay_hint_has_label_part_location(hint, "SftpCsvArchiveMetadata")
        }),
        "expected Twig SFTP file foreach inlay hint to include SftpCsvArchiveMetadata class link, got: {}",
        index_inlay
    );

    let component_inlay = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(inlay_hint_request(
                17,
                &component_twig_uri,
                0,
                0,
                component_end_position.0,
                component_end_position.1,
            ))
            .await
            .unwrap(),
    );
    let component_hints = component_inlay.as_array().cloned().unwrap_or_default();
    assert!(
        component_hints.iter().any(|hint| {
            inlay_hint_label_text(hint).as_deref() == Some(": ErrorCode")
                && hint["position"]["line"].as_u64() == Some(item_inlay_position.0 as u64)
                && hint["position"]["character"].as_u64() == Some(item_inlay_position.1 as u64)
                && inlay_hint_has_label_part_location(hint, "ErrorCode")
        }),
        "expected Twig include component item inlay hint to include ErrorCode class link, got: {}",
        component_inlay
    );

    let data_twig_without_items = concat!(
        "{% include 'components/autocomplete_input.html.twig' with {\n",
        "    'id': 'reject_code',\n",
        "    'name': 'reject_code',\n",
        "    'items': []\n",
        "} %}\n"
    );
    service
        .ready()
        .await
        .unwrap()
        .call(did_change_full_notification(
            &data_twig_uri,
            2,
            data_twig_without_items,
        ))
        .await
        .unwrap();
    let _ = next_publish_diagnostics(
        &mut notifications,
        &component_twig_uri,
        Duration::from_secs(2),
    )
    .await;
    let changed_component_hover = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(hover_request(
                18,
                &component_twig_uri,
                item_code_hover_position.0,
                item_code_hover_position.1,
            ))
            .await
            .unwrap(),
    );
    assert!(
        changed_component_hover.is_null()
            || !hover_markdown_value(&changed_component_hover).contains("ErrorCode"),
        "expected Twig include component context to refresh after caller didChange, got stale hover: {}",
        changed_component_hover
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
async fn test_twig_context_infers_repository_array_assignment_for_message_logs() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-twig-message-log-context-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp_root);
    fs::create_dir_all(tmp_root.join("src/Controller")).unwrap();
    fs::create_dir_all(tmp_root.join("src/Entity")).unwrap();
    fs::create_dir_all(tmp_root.join("src/Repository")).unwrap();
    fs::create_dir_all(tmp_root.join("templates/debt_suspension")).unwrap();

    let file_uri = |path: &std::path::Path| php_lsp_types::uri::path_to_uri(path).unwrap();
    let root_uri = file_uri(&tmp_root);
    let message_log_path = tmp_root.join("src/Entity/MessageLog.php");
    let message_type_path = tmp_root.join("src/Entity/MessageTypes.php");
    let error_code_path = tmp_root.join("src/Entity/ErrorCode.php");
    let repository_path = tmp_root.join("src/Repository/MessageLogRepository.php");
    let controller_path = tmp_root.join("src/Controller/DebtSuspensionController.php");
    let twig_path = tmp_root.join("templates/debt_suspension/show.html.twig");
    let message_log_uri = file_uri(&message_log_path);
    let message_type_uri = file_uri(&message_type_path);
    let twig_uri = file_uri(&twig_path);

    let message_log_php = r#"<?php
namespace App\Entity;

class MessageLog
{
    private int $id = 0;
    private ?MessageTypes $messageType = null;
    private string $direction = '';
    private ?ErrorCode $errorCode = null;

    public function getId(): int { return $this->id; }
    public function getMessageType(): ?MessageTypes { return $this->messageType; }
    public function getDirection(): string { return $this->direction; }
    public function getErrorCode(): ?ErrorCode { return $this->errorCode; }
}
"#;
    let message_type_php = r#"<?php
namespace App\Entity;

class MessageTypes
{
    private string $name = '';
    public function getName(): string { return $this->name; }
}
"#;
    let error_code_php = r#"<?php
namespace App\Entity;

class ErrorCode
{
    private int $code = 0;
    public function getCode(): int { return $this->code; }
}
"#;
    let repository_php = r#"<?php
namespace App\Repository;

use App\Entity\MessageLog;

class MessageLogRepository
{
    /**
     * @return MessageLog[]
     */
    public function findAllByNpId(string $npId): array
    {
        return [];
    }
}
"#;
    let controller_php = r#"<?php
namespace App\Controller;

use App\Repository\MessageLogRepository;

final class DebtSuspensionController
{
    public function show(MessageLogRepository $messageLogRepository): void
    {
        $logs = [];
        $npId = '123';
        if ('' !== $npId) {
            $logs = $messageLogRepository->findAllByNpId($npId);
        }

        $this->render('debt_suspension/show.html.twig', [
            'messageLogs' => $logs,
        ]);
    }
}
"#;
    let completion_marker = "/*complete*/";
    let twig_with_marker = format!(
        concat!(
            "{{% if messageLogs is defined and messageLogs|length > 0 %}}\n",
            "{{% for messageLog in messageLogs %}}\n",
            "{{{{ messageLog.{} }}}}\n",
            "{{{{ messageLog.id }}}}\n",
            "{{{{ messageLog.messageType.name }}}}\n",
            "{{{{ messageLog.errorCode.code }}}}\n",
            "{{% endfor %}}\n",
            "{{% endif %}}\n",
        ),
        completion_marker
    );
    let twig = twig_with_marker.replace(completion_marker, "");
    let completion_offset = twig_with_marker
        .find(completion_marker)
        .expect("test Twig should contain completion marker");
    let completion_prefix = twig_with_marker[..completion_offset].replace(completion_marker, "");
    let completion_position = utf16_position_for_offset(&twig, completion_prefix.len());
    let message_logs_position = utf16_position_at(&twig, "messageLogs is defined");
    let message_logs_definition_position = message_logs_position;
    let foreach_variable_offset = twig.find("messageLog in messageLogs").unwrap();
    let foreach_variable_position =
        utf16_position_for_offset(&twig, foreach_variable_offset + "messageLog".len());
    let message_log_hover_position = utf16_position_at(&twig, "messageLog in messageLogs");
    let id_hover_position = utf16_position_at(&twig, "id }}");
    let id_definition_position = utf16_position_after(&twig, "messageLog.i");
    let nested_name_position = utf16_position_at(&twig, "name }}");
    let end_position = utf16_position_for_offset(&twig, twig.len());

    fs::write(&message_log_path, message_log_php).unwrap();
    fs::write(&message_type_path, message_type_php).unwrap();
    fs::write(&error_code_path, error_code_php).unwrap();
    fs::write(&repository_path, repository_php).unwrap();
    fs::write(&controller_path, controller_php).unwrap();
    fs::write(&twig_path, &twig).unwrap();

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
    wait_for_indexing_phase(&mut notifications, "ready", Duration::from_secs(5)).await;
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
        next_publish_diagnostics(&mut notifications, &twig_uri, Duration::from_secs(2)).await;
    assert_eq!(
        diagnostics["diagnostics"].as_array().map(Vec::len),
        Some(0),
        "messageLogs Twig fixture should stay diagnostic-clean, got: {}",
        diagnostics
    );

    let message_logs_hover_resp = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(
            2,
            &twig_uri,
            message_logs_position.0,
            message_logs_position.1,
        ))
        .await
        .unwrap();
    let message_logs_hover = extract_result(message_logs_hover_resp);
    let message_logs_hover_text = message_logs_hover["contents"]["value"]
        .as_str()
        .unwrap_or_default();
    assert!(
        message_logs_hover_text.contains("array<int, App\\Entity\\MessageLog> $messageLogs")
            || message_logs_hover_text.contains("array<int, MessageLog> $messageLogs")
            || message_logs_hover_text.contains("array $messageLogs"),
        "expected Twig hover on `messageLogs is defined` to resolve collection type, got: {}",
        message_logs_hover
    );

    let message_logs_definition_resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(
            3,
            &twig_uri,
            message_logs_definition_position.0,
            message_logs_definition_position.1,
        ))
        .await
        .unwrap();
    let message_logs_definition = extract_result(message_logs_definition_resp);
    assert_eq!(
        message_logs_definition
            .get("uri")
            .and_then(|uri| uri.as_str()),
        Some(twig_uri.as_str()),
        "context variable definition should stay mapped to Twig source when generated prelude is the declaration, got: {}",
        message_logs_definition
    );

    let message_log_hover_resp = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(
            4,
            &twig_uri,
            message_log_hover_position.0,
            message_log_hover_position.1,
        ))
        .await
        .unwrap();
    let message_log_hover = extract_result(message_log_hover_resp);
    let message_log_hover_text = message_log_hover["contents"]["value"]
        .as_str()
        .unwrap_or_default();
    assert!(
        message_log_hover_text.contains("MessageLog $messageLog")
            && message_log_hover_text.contains(message_log_uri.as_str()),
        "expected Twig foreach variable hover to include clickable MessageLog class link, got: {}",
        message_log_hover
    );

    let completion_resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(
            5,
            &twig_uri,
            completion_position.0,
            completion_position.1,
        ))
        .await
        .unwrap();
    let completion = extract_result(completion_resp);
    let labels: Vec<String> = completion_items_from_result(&completion)
        .iter()
        .filter_map(|item| item.get("label").and_then(|value| value.as_str()))
        .map(str::to_string)
        .collect();
    for expected in ["id", "messageType", "direction", "errorCode"] {
        assert!(
            labels.iter().any(|label| label == expected),
            "expected Twig messageLog completion `{expected}`, got: {:?}",
            labels
        );
    }

    let id_hover_resp = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(
            6,
            &twig_uri,
            id_hover_position.0,
            id_hover_position.1,
        ))
        .await
        .unwrap();
    let id_hover = extract_result(id_hover_resp);
    let id_hover_text = id_hover["contents"]["value"].as_str().unwrap_or_default();
    assert!(
        id_hover_text.contains("getId") || id_hover_text.contains("property"),
        "expected Twig messageLog.id hover to resolve property/getter, got: {}",
        id_hover
    );

    let id_definition_resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(
            7,
            &twig_uri,
            id_definition_position.0,
            id_definition_position.1,
        ))
        .await
        .unwrap();
    let id_definition = extract_result(id_definition_resp);
    assert_eq!(
        id_definition.get("uri").and_then(|uri| uri.as_str()),
        Some(message_log_uri.as_str()),
        "Twig messageLog.id definition should jump to MessageLog, got: {}",
        id_definition
    );

    let nested_name_definition_resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(
            8,
            &twig_uri,
            nested_name_position.0,
            nested_name_position.1,
        ))
        .await
        .unwrap();
    let nested_name_definition = extract_result(nested_name_definition_resp);
    assert_eq!(
        nested_name_definition
            .get("uri")
            .and_then(|uri| uri.as_str()),
        Some(message_type_uri.as_str()),
        "Twig nested messageLog.messageType.name definition should jump to MessageTypes, got: {}",
        nested_name_definition
    );

    let inlay_resp = service
        .ready()
        .await
        .unwrap()
        .call(inlay_hint_request(
            9,
            &twig_uri,
            0,
            0,
            end_position.0,
            end_position.1,
        ))
        .await
        .unwrap();
    let inlay_result = extract_result(inlay_resp);
    let hints = inlay_result.as_array().cloned().unwrap_or_default();
    assert!(
        hints.iter().any(|hint| {
            inlay_hint_label_text(hint).as_deref() == Some(": MessageLog")
                && hint["position"]["line"].as_u64() == Some(foreach_variable_position.0 as u64)
                && hint["position"]["character"].as_u64()
                    == Some(foreach_variable_position.1 as u64)
                && inlay_hint_has_label_part_location(hint, "MessageLog")
        }),
        "expected Twig messageLog foreach inlay hint to include MessageLog class link, got: {}",
        inlay_result
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
async fn test_twig_context_infers_array_shape_rows_nested_arrays_and_compact_variables() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-twig-array-shape-context-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp_root);
    fs::create_dir_all(tmp_root.join("src/Controller")).unwrap();
    fs::create_dir_all(tmp_root.join("src/Entity")).unwrap();
    fs::create_dir_all(tmp_root.join("src/Repository")).unwrap();
    fs::create_dir_all(tmp_root.join("templates/message_log")).unwrap();
    fs::create_dir_all(tmp_root.join("templates/extended")).unwrap();

    let file_uri = |path: &std::path::Path| php_lsp_types::uri::path_to_uri(path).unwrap();
    let root_uri = file_uri(&tmp_root);
    let message_log_path = tmp_root.join("src/Entity/MessageLog.php");
    let message_type_path = tmp_root.join("src/Entity/MessageTypes.php");
    let repository_path = tmp_root.join("src/Repository/MessageLogRepository.php");
    let controller_path = tmp_root.join("src/Controller/ShapeController.php");
    let index_twig_path = tmp_root.join("templates/message_log/index.html.twig");
    let form_twig_path = tmp_root.join("templates/extended/form.html.twig");
    let message_log_uri = file_uri(&message_log_path);
    let message_type_uri = file_uri(&message_type_path);
    let repository_uri = file_uri(&repository_path);
    let controller_uri = file_uri(&controller_path);
    let index_twig_uri = file_uri(&index_twig_path);
    let form_twig_uri = file_uri(&form_twig_path);

    let message_log_php = r#"<?php
namespace App\Entity;

class MessageLog
{
    private int $id = 0;
    private ?MessageTypes $messageType = null;

    public function getId(): int { return $this->id; }
    public function getMessageType(): ?MessageTypes { return $this->messageType; }
}
"#;
    let message_type_php = r#"<?php
namespace App\Entity;

class MessageTypes
{
    private string $name = '';
    public function getName(): string { return $this->name; }
}
"#;
    let repository_php = r#"<?php
namespace App\Repository;

use App\Entity\MessageLog;

final class MessageLogRepository
{
    /**
     * @return list<array{messageLog: MessageLog, portingRequestId: int, npId: string}>
     */
    public function fetchRows(): array
    {
        return [];
    }
}
"#;
    let controller_php = r#"<?php
namespace App\Controller;

use App\Repository\MessageLogRepository;

final class ShapeController
{
    public function index(MessageLogRepository $repository): void
    {
        $pagination = $repository->fetchRows();

        $items = [];
        $items[] = ['🇺🇸中国བོད' => 'label', 'nr' => 'NR-1', 'code' => 'ERR', 'description' => 'Failure'];

        $fields = [
            ['name' => 'operatorCode', 'type' => 'choice', 'required' => true],
            ['name' => 'comment', 'type' => 'text', 'required' => false],
        ];
        $result = ['success' => true, 'message' => 'Saved'];
        $configParams = [
            'encryption' => ['temp_dir_path' => '/tmp/lsp', 'enabled' => true],
            'sftp' => ['host' => 'localhost', 'port' => 22],
        ];

        $this->render('message_log/index.html.twig', [
            'pagination' => $pagination,
            'items' => $items,
            'config_params' => $configParams,
        ]);
        $this->render('extended/form.html.twig', compact('fields', 'result'));
    }
}
"#;

    let row_completion_marker = "/*rowcomplete*/";
    let index_twig_with_marker = format!(
        concat!(
            "{{% for row in pagination %}}\n",
            "{{{{ row.{}messageLog }}}}\n",
            "{{% set message_log = row.messageLog %}}\n",
            "{{{{ message_log.id }}}}\n",
            "{{{{ message_log.messageType.name }}}}\n",
            "{{{{ row.npId }}}}\n",
            "{{% endfor %}}\n",
            "{{% for item in items %}}\n",
            "{{{{ item.nr }}}} {{{{ item.description }}}}\n",
            "{{% endfor %}}\n",
            "{{{{ config_params.encryption.temp_dir_path }}}}\n",
            "{{% set enc = config_params.encryption %}}\n",
            "{{{{ enc.temp_dir_path }}}}\n",
            "{{{{ config_params.sftp.port }}}}\n",
        ),
        row_completion_marker
    );
    let index_twig = index_twig_with_marker.replace(row_completion_marker, "");
    let row_completion_prefix = index_twig_with_marker
        [..index_twig_with_marker.find(row_completion_marker).unwrap()]
        .replace(row_completion_marker, "");
    let row_completion_position =
        utf16_position_for_offset(&index_twig, row_completion_prefix.len());
    let row_foreach_offset = index_twig.find("row in pagination").unwrap();
    let row_inlay_position =
        utf16_position_for_offset(&index_twig, row_foreach_offset + "row".len());
    let message_log_set_offset = index_twig.find("message_log =").unwrap();
    let message_log_inlay_position =
        utf16_position_for_offset(&index_twig, message_log_set_offset + "message_log".len());
    let message_log_id_hover_position = utf16_position_at(&index_twig, "id }}");
    let message_log_id_definition_position = utf16_position_after(&index_twig, "message_log.i");
    let message_type_name_definition_position = utf16_position_at(&index_twig, "name }}");
    let row_np_id_hover_position = utf16_position_at(&index_twig, "npId }}");
    let row_np_id_definition_position = utf16_position_after(&index_twig, "row.n");
    let item_foreach_offset = index_twig.find("item in items").unwrap();
    let item_inlay_position =
        utf16_position_for_offset(&index_twig, item_foreach_offset + "item".len());
    let item_nr_hover_position = utf16_position_at(&index_twig, "nr }}");
    let item_nr_definition_position = utf16_position_after(&index_twig, "item.n");
    let config_temp_hover_position = utf16_position_at(&index_twig, "temp_dir_path");
    let config_temp_definition_position = config_temp_hover_position;
    let enc_temp_definition_position = utf16_position_after(&index_twig, "enc.t");
    let config_port_hover_position = utf16_position_at(&index_twig, "port }}");
    let config_port_definition_position = config_port_hover_position;
    let index_end_position = utf16_position_for_offset(&index_twig, index_twig.len());

    let field_completion_marker = "/*fieldcomplete*/";
    let form_twig_with_marker = format!(
        concat!(
            "{{% for f in fields %}}\n",
            "{{{{ f.{}type }}}}\n",
            "{{{{ f.type }}}}\n",
            "{{% endfor %}}\n",
            "{{{{ result.success }}}}\n",
        ),
        field_completion_marker
    );
    let form_twig = form_twig_with_marker.replace(field_completion_marker, "");
    let field_completion_prefix = form_twig_with_marker
        [..form_twig_with_marker.find(field_completion_marker).unwrap()]
        .replace(field_completion_marker, "");
    let field_completion_position =
        utf16_position_for_offset(&form_twig, field_completion_prefix.len());
    let field_foreach_offset = form_twig.find("f in fields").unwrap();
    let field_inlay_position =
        utf16_position_for_offset(&form_twig, field_foreach_offset + "f".len());
    let field_type_hover_position = utf16_position_at(&form_twig, "type }}");
    let field_type_definition_position = utf16_position_after(&form_twig, "f.t");
    let result_success_hover_position = utf16_position_at(&form_twig, "success");
    let result_success_definition_position = utf16_position_after(&form_twig, "result.s");
    let form_end_position = utf16_position_for_offset(&form_twig, form_twig.len());

    fs::write(&message_log_path, message_log_php).unwrap();
    fs::write(&message_type_path, message_type_php).unwrap();
    fs::write(&repository_path, repository_php).unwrap();
    fs::write(&controller_path, controller_php).unwrap();
    fs::write(&index_twig_path, &index_twig).unwrap();
    fs::write(&form_twig_path, &form_twig).unwrap();

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
    wait_for_indexing_phase(&mut notifications, "ready", Duration::from_secs(5)).await;
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification_with_language(
            &index_twig_uri,
            "twig",
            &index_twig,
        ))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification_with_language(
            &form_twig_uri,
            "twig",
            &form_twig,
        ))
        .await
        .unwrap();

    let index_diagnostics =
        next_publish_diagnostics(&mut notifications, &index_twig_uri, Duration::from_secs(2)).await;
    assert_eq!(
        index_diagnostics["diagnostics"].as_array().map(Vec::len),
        Some(0),
        "array-shape Twig index fixture should stay diagnostic-clean, got: {}",
        index_diagnostics
    );
    let form_diagnostics =
        next_publish_diagnostics(&mut notifications, &form_twig_uri, Duration::from_secs(2)).await;
    assert_eq!(
        form_diagnostics["diagnostics"].as_array().map(Vec::len),
        Some(0),
        "array-shape Twig compact fixture should stay diagnostic-clean, got: {}",
        form_diagnostics
    );

    let row_completion_resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(
            2,
            &index_twig_uri,
            row_completion_position.0,
            row_completion_position.1,
        ))
        .await
        .unwrap();
    let row_completion = extract_result(row_completion_resp);
    let row_labels: Vec<String> = completion_items_from_result(&row_completion)
        .iter()
        .filter_map(|item| item.get("label").and_then(|value| value.as_str()))
        .map(str::to_string)
        .collect();
    for expected in ["messageLog", "portingRequestId", "npId"] {
        assert!(
            row_labels.iter().any(|label| label == expected),
            "expected Twig row shape completion `{expected}`, got: {:?}",
            row_labels
        );
    }

    let field_completion_resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(
            3,
            &form_twig_uri,
            field_completion_position.0,
            field_completion_position.1,
        ))
        .await
        .unwrap();
    let field_completion = extract_result(field_completion_resp);
    let field_labels: Vec<String> = completion_items_from_result(&field_completion)
        .iter()
        .filter_map(|item| item.get("label").and_then(|value| value.as_str()))
        .map(str::to_string)
        .collect();
    for expected in ["name", "type", "required"] {
        assert!(
            field_labels.iter().any(|label| label == expected),
            "expected Twig compact field shape completion `{expected}`, got: {:?}",
            field_labels
        );
    }

    let row_np_id_hover = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(hover_request(
                4,
                &index_twig_uri,
                row_np_id_hover_position.0,
                row_np_id_hover_position.1,
            ))
            .await
            .unwrap(),
    );
    assert!(
        hover_markdown_value(&row_np_id_hover).contains("string npId"),
        "expected Twig row.npId hover from array shape, got: {}",
        row_np_id_hover
    );

    let message_log_id_hover = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(hover_request(
                5,
                &index_twig_uri,
                message_log_id_hover_position.0,
                message_log_id_hover_position.1,
            ))
            .await
            .unwrap(),
    );
    let message_log_id_hover_text = hover_markdown_value(&message_log_id_hover);
    assert!(
        message_log_id_hover_text.contains("getId") || message_log_id_hover_text.contains("$id"),
        "expected Twig set variable member hover to resolve MessageLog id, got: {}",
        message_log_id_hover
    );

    for (request_id, position, expected_text) in [
        (6, item_nr_hover_position, "string nr"),
        (7, config_temp_hover_position, "string temp_dir_path"),
        (8, config_port_hover_position, "int port"),
    ] {
        let hover = extract_result(
            service
                .ready()
                .await
                .unwrap()
                .call(hover_request(
                    request_id,
                    &index_twig_uri,
                    position.0,
                    position.1,
                ))
                .await
                .unwrap(),
        );
        assert!(
            hover_markdown_value(&hover).contains(expected_text),
            "expected Twig shape hover `{expected_text}`, got: {}",
            hover
        );
    }

    for (request_id, position, expected_text) in [
        (9, field_type_hover_position, "string type"),
        (10, result_success_hover_position, "bool success"),
    ] {
        let hover = extract_result(
            service
                .ready()
                .await
                .unwrap()
                .call(hover_request(
                    request_id,
                    &form_twig_uri,
                    position.0,
                    position.1,
                ))
                .await
                .unwrap(),
        );
        assert!(
            hover_markdown_value(&hover).contains(expected_text),
            "expected Twig compact shape hover `{expected_text}`, got: {}",
            hover
        );
    }

    let source_position = |source: &str, needle: &str| {
        let (line, character) = utf16_position_at(source, needle);
        (line as u64, character as u64)
    };
    for (request_id, uri, position, expected_uri, expected_position) in [
        (
            11,
            index_twig_uri.as_str(),
            row_np_id_definition_position,
            repository_uri.as_str(),
            source_position(repository_php, "npId"),
        ),
        (
            12,
            index_twig_uri.as_str(),
            item_nr_definition_position,
            controller_uri.as_str(),
            source_position(controller_php, "nr"),
        ),
        (
            13,
            form_twig_uri.as_str(),
            field_type_definition_position,
            controller_uri.as_str(),
            source_position(controller_php, "type"),
        ),
        (
            14,
            form_twig_uri.as_str(),
            result_success_definition_position,
            controller_uri.as_str(),
            source_position(controller_php, "success"),
        ),
        (
            15,
            index_twig_uri.as_str(),
            config_temp_definition_position,
            controller_uri.as_str(),
            source_position(controller_php, "temp_dir_path"),
        ),
        (
            16,
            index_twig_uri.as_str(),
            enc_temp_definition_position,
            controller_uri.as_str(),
            source_position(controller_php, "temp_dir_path"),
        ),
        (
            17,
            index_twig_uri.as_str(),
            config_port_definition_position,
            controller_uri.as_str(),
            source_position(controller_php, "port"),
        ),
    ] {
        let definition = extract_result(
            service
                .ready()
                .await
                .unwrap()
                .call(definition_request(request_id, uri, position.0, position.1))
                .await
                .unwrap(),
        );
        assert_eq!(
            definition.get("uri").and_then(|value| value.as_str()),
            Some(expected_uri),
            "expected Twig source-backed shape definition request {request_id} to resolve `{expected_uri}`, got: {}",
            definition
        );
        assert_eq!(
            definition["range"]["start"]["line"].as_u64(),
            Some(expected_position.0),
            "expected Twig source-backed shape definition request {request_id} to jump to source line {}, got: {}",
            expected_position.0,
            definition
        );
        assert_eq!(
            definition["range"]["start"]["character"].as_u64(),
            Some(expected_position.1),
            "expected Twig source-backed shape definition request {request_id} to jump to source character {}, got: {}",
            expected_position.1,
            definition
        );
    }

    for (request_id, uri, position, expected_uri) in [
        (
            18,
            index_twig_uri.as_str(),
            message_log_id_definition_position,
            message_log_uri.as_str(),
        ),
        (
            19,
            index_twig_uri.as_str(),
            message_type_name_definition_position,
            message_type_uri.as_str(),
        ),
    ] {
        let definition = extract_result(
            service
                .ready()
                .await
                .unwrap()
                .call(definition_request(request_id, uri, position.0, position.1))
                .await
                .unwrap(),
        );
        assert_eq!(
            definition.get("uri").and_then(|value| value.as_str()),
            Some(expected_uri),
            "expected Twig definition request {request_id} to resolve `{expected_uri}`, got: {}",
            definition
        );
    }

    let index_inlay = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(inlay_hint_request(
                20,
                &index_twig_uri,
                0,
                0,
                index_end_position.0,
                index_end_position.1,
            ))
            .await
            .unwrap(),
    );
    let index_hints = index_inlay.as_array().cloned().unwrap_or_default();
    assert!(
        index_hints.iter().any(|hint| {
            inlay_hint_label_text(hint).is_some_and(|label| {
                label.contains("messageLog: App\\Entity\\MessageLog")
                    && label.contains("portingRequestId: int")
                    && label.contains("npId: string")
            }) && hint["position"]["line"].as_u64() == Some(row_inlay_position.0 as u64)
                && hint["position"]["character"].as_u64() == Some(row_inlay_position.1 as u64)
        }),
        "expected Twig row foreach inlay hint to show array shape, got: {}",
        index_inlay
    );
    assert!(
        index_hints.iter().any(|hint| {
            inlay_hint_label_text(hint).as_deref() == Some(": MessageLog")
                && hint["position"]["line"].as_u64() == Some(message_log_inlay_position.0 as u64)
                && hint["position"]["character"].as_u64()
                    == Some(message_log_inlay_position.1 as u64)
                && inlay_hint_has_label_part_location(hint, "MessageLog")
        }),
        "expected Twig set variable inlay hint from row.messageLog with class link, got: {}",
        index_inlay
    );
    assert!(
        index_hints.iter().any(|hint| {
            inlay_hint_label_text(hint).is_some_and(|label| {
                label.contains("nr: string")
                    && label.contains("code: string")
                    && label.contains("description: string")
            }) && hint["position"]["line"].as_u64() == Some(item_inlay_position.0 as u64)
                && hint["position"]["character"].as_u64() == Some(item_inlay_position.1 as u64)
        }),
        "expected Twig item foreach inlay hint from append-built array shape, got: {}",
        index_inlay
    );

    let form_inlay = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(inlay_hint_request(
                21,
                &form_twig_uri,
                0,
                0,
                form_end_position.0,
                form_end_position.1,
            ))
            .await
            .unwrap(),
    );
    let form_hints = form_inlay.as_array().cloned().unwrap_or_default();
    assert!(
        form_hints.iter().any(|hint| {
            inlay_hint_label_text(hint).is_some_and(|label| {
                label.contains("array{name: string")
                    && label.contains("type: string")
                    && label.contains("required: bool")
            }) && hint["position"]["line"].as_u64() == Some(field_inlay_position.0 as u64)
                && hint["position"]["character"].as_u64() == Some(field_inlay_position.1 as u64)
        }),
        "expected Twig compact foreach inlay hint to show field array shape, got: {}",
        form_inlay
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
async fn test_twig_context_infers_symfony_globals_forms_and_form_errors() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-twig-symfony-globals-forms-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp_root);
    for dir in [
        "src/Controller",
        "src/Entity",
        "src/Form",
        "src/Symfony/Bundle/FrameworkBundle/Controller",
        "src/Symfony/Component/Form",
        "src/Symfony/Component/Security/Core/Exception",
        "src/Symfony/Component/Security/Core/User",
        "src/Symfony/Component/Security/Http/Authentication",
        "templates/components",
        "templates/form",
        "templates/operator",
        "templates/debt_suspension",
        "templates/porting_request",
        "templates/registration",
        "templates/security",
    ] {
        fs::create_dir_all(tmp_root.join(dir)).unwrap();
    }

    let file_uri = |path: &std::path::Path| php_lsp_types::uri::path_to_uri(path).unwrap();
    let root_uri = file_uri(&tmp_root);
    let user_path = tmp_root.join("src/Entity/User.php");
    let registration_form_path = tmp_root.join("src/Form/RegistrationFormType.php");
    let operator_form_path = tmp_root.join("src/Form/OperatorType.php");
    let debt_form_path = tmp_root.join("src/Form/DebtSuspensionType.php");
    let porting_form_path = tmp_root.join("src/Form/PortingRequestType.php");
    let auth_exception_path =
        tmp_root.join("src/Symfony/Component/Security/Core/Exception/AuthenticationException.php");
    let form_error_path = tmp_root.join("src/Symfony/Component/Form/FormError.php");
    let form_core_path = tmp_root.join("src/Symfony/Component/Form/Core.php");
    let abstract_controller_path =
        tmp_root.join("src/Symfony/Bundle/FrameworkBundle/Controller/AbstractController.php");
    let user_interface_path =
        tmp_root.join("src/Symfony/Component/Security/Core/User/UserInterface.php");
    let auth_utils_path =
        tmp_root.join("src/Symfony/Component/Security/Http/Authentication/AuthenticationUtils.php");
    let registration_controller_path = tmp_root.join("src/Controller/RegistrationController.php");
    let operator_controller_path = tmp_root.join("src/Controller/OperatorController.php");
    let debt_controller_path = tmp_root.join("src/Controller/DebtSuspensionController.php");
    let porting_controller_path = tmp_root.join("src/Controller/PortingRequestController.php");
    let security_controller_path = tmp_root.join("src/Controller/SecurityController.php");
    let base_twig_path = tmp_root.join("templates/base.html.twig");
    let login_twig_path = tmp_root.join("templates/security/login.html.twig");
    let form_theme_twig_path = tmp_root.join("templates/form/form_theme.html.twig");
    let registration_twig_path = tmp_root.join("templates/registration/register.html.twig");
    let operator_twig_path = tmp_root.join("templates/operator/new.html.twig");
    let debt_twig_path = tmp_root.join("templates/debt_suspension/new.html.twig");
    let porting_twig_path = tmp_root.join("templates/porting_request/new.html.twig");
    let component_twig_path =
        tmp_root.join("templates/components/subscriber_autocomplete.html.twig");

    let user_uri = file_uri(&user_path);
    let registration_form_uri = file_uri(&registration_form_path);
    let operator_form_uri = file_uri(&operator_form_path);
    let debt_form_uri = file_uri(&debt_form_path);
    let porting_form_uri = file_uri(&porting_form_path);
    let auth_exception_uri = file_uri(&auth_exception_path);
    let form_error_uri = file_uri(&form_error_path);
    let base_twig_uri = file_uri(&base_twig_path);
    let login_twig_uri = file_uri(&login_twig_path);
    let form_theme_twig_uri = file_uri(&form_theme_twig_path);
    let registration_twig_uri = file_uri(&registration_twig_path);
    let operator_twig_uri = file_uri(&operator_twig_path);
    let debt_twig_uri = file_uri(&debt_twig_path);
    let component_twig_uri = file_uri(&component_twig_path);

    let user_interface_php = r#"<?php
namespace Symfony\Component\Security\Core\User;

interface UserInterface
{
    public function getUserIdentifier(): string;
}
"#;
    let auth_exception_php = r#"<?php
namespace Symfony\Component\Security\Core\Exception;

class AuthenticationException
{
    public function getMessageKey(): string { return ''; }
    public function getMessageData(): array { return []; }
}
"#;
    let auth_utils_php = r#"<?php
namespace Symfony\Component\Security\Http\Authentication;

use Symfony\Component\Security\Core\Exception\AuthenticationException;

class AuthenticationUtils
{
    public function getLastAuthenticationError(): ?AuthenticationException { return null; }
    public function getLastUsername(): string { return ''; }
}
"#;
    let form_core_php = r#"<?php
namespace Symfony\Component\Form;

abstract class AbstractType {}

interface FormBuilderInterface
{
    public function add(string $child, ?string $type = null, array $options = []): static;
}

interface FormInterface
{
    public function createView(): FormView;
}

class FormView
{
    /**
     * @var array{id: string, full_name: string, name: string, value: mixed, errors: list<FormError>}
     */
    public array $vars = [];

    /**
     * @var list<FormError>
     */
    public array $errors = [];
}
"#;
    let form_error_php = r#"<?php
namespace Symfony\Component\Form;

class FormError
{
    public function getMessage(): string { return ''; }
}
"#;
    let abstract_controller_php = r#"<?php
namespace Symfony\Bundle\FrameworkBundle\Controller;

use Symfony\Component\Form\FormInterface;

abstract class AbstractController
{
    public function createForm(string $type, mixed $data = null, array $options = []): FormInterface {}
    public function render(string $view, array $parameters = []): void {}
}
"#;
    let user_php = r#"<?php
namespace App\Entity;

use Symfony\Component\Security\Core\User\UserInterface;

class User implements UserInterface
{
    private ?int $id = null;
    private ?string $email = null;
    private array $roles = [];

    public function getId(): ?int { return $this->id; }
    public function getEmail(): ?string { return $this->email; }
    public function getUserIdentifier(): string { return (string) $this->email; }
    public function getRoles(): array { return $this->roles; }
}
"#;
    let registration_form_php = r#"<?php
namespace App\Form;

use Symfony\Component\Form\AbstractType;
use Symfony\Component\Form\FormBuilderInterface;

class RegistrationFormType extends AbstractType
{
    public function buildForm(FormBuilderInterface $builder, array $options): void
    {
        $builder
            ->add('email')
            ->add('agreeTerms')
            ->add('plainPassword');
    }
}
"#;
    let operator_form_php = r#"<?php
namespace App\Form;

use Symfony\Component\Form\AbstractType;
use Symfony\Component\Form\FormBuilderInterface;

class OperatorType extends AbstractType
{
    public function buildForm(FormBuilderInterface $builder, array $options): void
    {
        $builder
            ->add('name')
            ->add('inn')
            ->add('mnc')
            ->add('operatorCode')
            ->add('isOwn');
    }
}
"#;
    let debt_form_php = r#"<?php
namespace App\Form;

use Symfony\Component\Form\AbstractType;
use Symfony\Component\Form\FormBuilderInterface;

class DebtSuspensionType extends AbstractType
{
    public function buildForm(FormBuilderInterface $builder, array $options): void
    {
        $builder
            ->add('recipientOperator')
            ->add('phoneNumbersInput')
            ->add('repaymentType');
    }
}
"#;
    let porting_form_php = r#"<?php
namespace App\Form;

use Symfony\Component\Form\AbstractType;
use Symfony\Component\Form\FormBuilderInterface;

class PortingRequestType extends AbstractType
{
    public function buildForm(FormBuilderInterface $builder, array $options): void
    {
        $builder
            ->add('subscriber')
            ->add('requestedPortingDateTime')
            ->add('requestType');
    }
}
"#;
    let registration_controller_php = r#"<?php
namespace App\Controller;

use App\Entity\User;
use App\Form\RegistrationFormType;
use Symfony\Bundle\FrameworkBundle\Controller\AbstractController;

class RegistrationController extends AbstractController
{
    public function register(): void
    {
        $form = $this->createForm(RegistrationFormType::class, new User());
        $this->render('registration/register.html.twig', [
            'registrationForm' => $form,
        ]);
    }
}
"#;
    let operator_controller_php = r#"<?php
namespace App\Controller;

use App\Form\OperatorType;
use Symfony\Bundle\FrameworkBundle\Controller\AbstractController;

class OperatorController extends AbstractController
{
    public function new(): void
    {
        $form = $this->createForm(OperatorType::class);
        $this->render('operator/new.html.twig', ['form' => $form]);
    }
}
"#;
    let debt_controller_php = r#"<?php
namespace App\Controller;

use App\Form\DebtSuspensionType;
use Symfony\Bundle\FrameworkBundle\Controller\AbstractController;

class DebtSuspensionController extends AbstractController
{
    public function new(): void
    {
        $form = $this->createForm(DebtSuspensionType::class);
        $this->render('debt_suspension/new.html.twig', ['form' => $form]);
    }
}
"#;
    let porting_controller_php = r#"<?php
namespace App\Controller;

use App\Form\PortingRequestType;
use Symfony\Bundle\FrameworkBundle\Controller\AbstractController;

class PortingRequestController extends AbstractController
{
    public function new(): void
    {
        $form = $this->createForm(PortingRequestType::class);
        $this->render('porting_request/new.html.twig', ['form' => $form]);
    }
}
"#;
    let security_controller_php = r#"<?php
namespace App\Controller;

use Symfony\Bundle\FrameworkBundle\Controller\AbstractController;
use Symfony\Component\Security\Http\Authentication\AuthenticationUtils;

class SecurityController extends AbstractController
{
    public function login(AuthenticationUtils $authenticationUtils): void
    {
        $error = $authenticationUtils->getLastAuthenticationError();
        $lastUsername = $authenticationUtils->getLastUsername();
        $this->render('security/login.html.twig', [
            'last_username' => $lastUsername,
            'error' => $error,
        ]);
    }
}
"#;

    let base_twig = r#"{{ app.current_route }}
{{ app.user.id }}
{{ app.user.email }}
"#;
    let login_twig = r#"{% if error %}
{{ error.messageKey }}
{% endif %}
{% if app.user %}
{{ app.user.userIdentifier }}
{% endif %}
"#;
    let form_theme_twig = r#"{% block form_errors %}
    {% if errors|length > 0 %}
        {% for error in errors %}
            {{ error.message }}
        {% endfor %}
    {% endif %}
{% endblock %}
"#;
    let registration_completion_marker = "/*complete*/";
    let registration_twig_with_marker = format!(
        concat!(
            "{{% for flash_error in app.flashes('verify_email_error') %}}{{{{ flash_error }}}}{{% endfor %}}\n",
            "{{{{ registrationForm.{}email }}}}\n",
            "{{{{ registrationForm.plainPassword }}}}\n",
            "{{{{ registrationForm.agreeTerms }}}}\n",
        ),
        registration_completion_marker
    );
    let registration_prefix = registration_twig_with_marker[..registration_twig_with_marker
        .find(registration_completion_marker)
        .unwrap()]
        .replace(registration_completion_marker, "");
    let registration_twig =
        registration_twig_with_marker.replace(registration_completion_marker, "");
    let registration_completion_position =
        utf16_position_for_offset(&registration_twig, registration_prefix.len());
    let registration_email_definition_position =
        utf16_position_after(&registration_twig, "registrationForm.e");
    let registration_password_definition_position =
        utf16_position_after(&registration_twig, "registrationForm.p");
    let registration_agree_definition_position =
        utf16_position_after(&registration_twig, "registrationForm.a");
    let operator_twig = r#"{{ form.name }}
{{ form.inn }}
{{ form.mnc }}
{{ form.operatorCode }}
{{ form.isOwn }}
"#;
    let debt_twig = r#"{% if form.vars.errors|length > 0 %}
    {% for error in form.vars.errors %}
        {{ error.message }}
    {% endfor %}
{% endif %}
{{ form.recipientOperator }}
{{ form.repaymentType }}
{{ form.phoneNumbersInput }}
"#;
    let porting_twig = r#"{% include 'components/subscriber_autocomplete.html.twig' with {
    form_field: form.subscriber,
    porting_date_field_id: form.requestedPortingDateTime.vars.id,
    request_type_field_id: form.requestType.vars.id
} only %}
"#;
    let component_twig = r#"{% set field_id = form_field.vars.id %}
{% set field_name = form_field.vars.full_name %}
{{ field_id }} {{ field_name }}
"#;

    for (path, source) in [
        (&user_interface_path, user_interface_php),
        (&auth_exception_path, auth_exception_php),
        (&auth_utils_path, auth_utils_php),
        (&form_core_path, form_core_php),
        (&form_error_path, form_error_php),
        (&abstract_controller_path, abstract_controller_php),
        (&user_path, user_php),
        (&registration_form_path, registration_form_php),
        (&operator_form_path, operator_form_php),
        (&debt_form_path, debt_form_php),
        (&porting_form_path, porting_form_php),
        (&registration_controller_path, registration_controller_php),
        (&operator_controller_path, operator_controller_php),
        (&debt_controller_path, debt_controller_php),
        (&porting_controller_path, porting_controller_php),
        (&security_controller_path, security_controller_php),
    ] {
        fs::write(path, source).unwrap();
    }
    for (path, source) in [
        (&base_twig_path, base_twig),
        (&login_twig_path, login_twig),
        (&form_theme_twig_path, form_theme_twig),
        (&registration_twig_path, registration_twig.as_str()),
        (&operator_twig_path, operator_twig),
        (&debt_twig_path, debt_twig),
        (&porting_twig_path, porting_twig),
        (&component_twig_path, component_twig),
    ] {
        fs::write(path, source).unwrap();
    }

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
    wait_for_indexing_phase(&mut notifications, "ready", Duration::from_secs(5)).await;

    for (uri, source) in [
        (base_twig_uri.as_str(), base_twig),
        (login_twig_uri.as_str(), login_twig),
        (form_theme_twig_uri.as_str(), form_theme_twig),
        (registration_twig_uri.as_str(), registration_twig.as_str()),
        (operator_twig_uri.as_str(), operator_twig),
        (debt_twig_uri.as_str(), debt_twig),
        (component_twig_uri.as_str(), component_twig),
    ] {
        service
            .ready()
            .await
            .unwrap()
            .call(did_open_notification_with_language(uri, "twig", source))
            .await
            .unwrap();
        let diagnostics =
            next_publish_diagnostics(&mut notifications, uri, Duration::from_secs(2)).await;
        assert_eq!(
            diagnostics["diagnostics"].as_array().map(Vec::len),
            Some(0),
            "Symfony Twig fixture `{uri}` should stay diagnostic-clean, got: {}",
            diagnostics
        );
    }

    let app_route_hover = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(hover_request(
                2,
                &base_twig_uri,
                utf16_position_at(base_twig, "current_route").0,
                utf16_position_at(base_twig, "current_route").1,
            ))
            .await
            .unwrap(),
    );
    assert!(
        hover_markdown_value(&app_route_hover).contains("current_route"),
        "expected app.current_route hover from Symfony app global, got: {}",
        app_route_hover
    );

    for (request_id, uri, source, position, expected_uri) in [
        (
            3,
            base_twig_uri.as_str(),
            base_twig,
            utf16_position_after(base_twig, "app.user.i"),
            user_uri.as_str(),
        ),
        (
            4,
            base_twig_uri.as_str(),
            base_twig,
            utf16_position_after(base_twig, "app.user.e"),
            user_uri.as_str(),
        ),
        (
            5,
            login_twig_uri.as_str(),
            login_twig,
            utf16_position_after(login_twig, "app.user.u"),
            user_uri.as_str(),
        ),
        (
            6,
            login_twig_uri.as_str(),
            login_twig,
            utf16_position_after(login_twig, "error.m"),
            auth_exception_uri.as_str(),
        ),
        (
            7,
            form_theme_twig_uri.as_str(),
            form_theme_twig,
            utf16_position_after(form_theme_twig, "error.m"),
            form_error_uri.as_str(),
        ),
    ] {
        let definition = extract_result(
            service
                .ready()
                .await
                .unwrap()
                .call(definition_request(request_id, uri, position.0, position.1))
                .await
                .unwrap(),
        );
        assert_eq!(
            definition.get("uri").and_then(|value| value.as_str()),
            Some(expected_uri),
            "expected Symfony Twig definition request {request_id} in `{source}` to resolve `{expected_uri}`, got: {}",
            definition
        );
    }

    let registration_completion = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(completion_request(
                8,
                &registration_twig_uri,
                registration_completion_position.0,
                registration_completion_position.1,
            ))
            .await
            .unwrap(),
    );
    let registration_labels: Vec<String> = completion_items_from_result(&registration_completion)
        .iter()
        .filter_map(|item| item.get("label").and_then(|value| value.as_str()))
        .map(str::to_string)
        .collect();
    for expected in ["email", "plainPassword", "agreeTerms"] {
        assert!(
            registration_labels.iter().any(|label| label == expected),
            "expected registration form field completion `{expected}`, got: {:?}",
            registration_labels
        );
    }

    let source_position = |source: &str, needle: &str| {
        let (line, character) = utf16_position_at(source, needle);
        (line as u64, character as u64)
    };
    for (request_id, uri, position, expected_uri, expected_position) in [
        (
            9,
            registration_twig_uri.as_str(),
            registration_email_definition_position,
            registration_form_uri.as_str(),
            source_position(registration_form_php, "email"),
        ),
        (
            10,
            registration_twig_uri.as_str(),
            registration_password_definition_position,
            registration_form_uri.as_str(),
            source_position(registration_form_php, "plainPassword"),
        ),
        (
            11,
            registration_twig_uri.as_str(),
            registration_agree_definition_position,
            registration_form_uri.as_str(),
            source_position(registration_form_php, "agreeTerms"),
        ),
        (
            12,
            operator_twig_uri.as_str(),
            utf16_position_after(operator_twig, "form.operatorC"),
            operator_form_uri.as_str(),
            source_position(operator_form_php, "operatorCode"),
        ),
        (
            13,
            operator_twig_uri.as_str(),
            utf16_position_after(operator_twig, "form.is"),
            operator_form_uri.as_str(),
            source_position(operator_form_php, "isOwn"),
        ),
        (
            14,
            debt_twig_uri.as_str(),
            utf16_position_after(debt_twig, "form.recipientO"),
            debt_form_uri.as_str(),
            source_position(debt_form_php, "recipientOperator"),
        ),
        (
            15,
            debt_twig_uri.as_str(),
            utf16_position_after(debt_twig, "form.phone"),
            debt_form_uri.as_str(),
            source_position(debt_form_php, "phoneNumbersInput"),
        ),
        (
            16,
            component_twig_uri.as_str(),
            utf16_position_after(component_twig, "vars.i"),
            porting_form_uri.as_str(),
            source_position(porting_form_php, "subscriber"),
        ),
    ] {
        let definition = extract_result(
            service
                .ready()
                .await
                .unwrap()
                .call(definition_request(request_id, uri, position.0, position.1))
                .await
                .unwrap(),
        );
        assert_eq!(
            definition.get("uri").and_then(|value| value.as_str()),
            Some(expected_uri),
            "expected Symfony form field definition request {request_id} to resolve `{expected_uri}`, got: {}",
            definition
        );
        assert_eq!(
            definition["range"]["start"]["line"].as_u64(),
            Some(expected_position.0),
            "expected Symfony form field definition request {request_id} to jump to source line {}, got: {}",
            expected_position.0,
            definition
        );
        assert_eq!(
            definition["range"]["start"]["character"].as_u64(),
            Some(expected_position.1),
            "expected Symfony form field definition request {request_id} to jump to source character {}, got: {}",
            expected_position.1,
            definition
        );
    }

    let component_id_hover = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(hover_request(
                17,
                &component_twig_uri,
                utf16_position_at(component_twig, "id %}").0,
                utf16_position_at(component_twig, "id %}").1,
            ))
            .await
            .unwrap(),
    );
    assert!(
        hover_markdown_value(&component_id_hover).contains("string id"),
        "expected component form_field.vars.id hover from FormView vars shape, got: {}",
        component_id_hover
    );

    let form_theme_inlay = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(inlay_hint_request(
                18,
                &form_theme_twig_uri,
                0,
                0,
                utf16_position_for_offset(form_theme_twig, form_theme_twig.len()).0,
                utf16_position_for_offset(form_theme_twig, form_theme_twig.len()).1,
            ))
            .await
            .unwrap(),
    );
    let form_theme_hints = form_theme_inlay.as_array().cloned().unwrap_or_default();
    let error_inlay_position = utf16_position_for_offset(
        form_theme_twig,
        form_theme_twig.find("error in").unwrap() + "error".len(),
    );
    assert!(
        form_theme_hints.iter().any(|hint| {
            inlay_hint_label_text(hint).as_deref() == Some(": FormError")
                && hint["position"]["line"].as_u64() == Some(error_inlay_position.0 as u64)
                && hint["position"]["character"].as_u64() == Some(error_inlay_position.1 as u64)
                && inlay_hint_has_label_part_location(hint, "FormError")
        }),
        "expected form theme foreach inlay hint to link FormError, got: {}",
        form_theme_inlay
    );

    let component_inlay = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(inlay_hint_request(
                19,
                &component_twig_uri,
                0,
                0,
                utf16_position_for_offset(component_twig, component_twig.len()).0,
                utf16_position_for_offset(component_twig, component_twig.len()).1,
            ))
            .await
            .unwrap(),
    );
    let component_hints = component_inlay.as_array().cloned().unwrap_or_default();
    let field_id_inlay_position = utf16_position_for_offset(
        component_twig,
        component_twig.find("field_id =").unwrap() + "field_id".len(),
    );
    assert!(
        component_hints.iter().any(|hint| {
            inlay_hint_label_text(hint).as_deref() == Some(": string")
                && hint["position"]["line"].as_u64() == Some(field_id_inlay_position.0 as u64)
                && hint["position"]["character"].as_u64() == Some(field_id_inlay_position.1 as u64)
        }),
        "expected component field_id inlay hint from form_field.vars.id, got: {}",
        component_inlay
    );

    let unsaved_registration_form_php = registration_form_php.replace(
        "->add('plainPassword');",
        "->add('plainPassword')\n            ->add('displayName');",
    );
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(
            &registration_form_uri,
            &unsaved_registration_form_php,
        ))
        .await
        .unwrap();
    let diagnostics = next_publish_diagnostics(
        &mut notifications,
        &registration_twig_uri,
        Duration::from_secs(2),
    )
    .await;
    assert_eq!(
        diagnostics["diagnostics"].as_array().map(Vec::len),
        Some(0),
        "open unsaved Symfony FormType should refresh Twig context quietly, got: {}",
        diagnostics
    );
    let refreshed_registration_completion = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(completion_request(
                20,
                &registration_twig_uri,
                registration_completion_position.0,
                registration_completion_position.1,
            ))
            .await
            .unwrap(),
    );
    let refreshed_registration_labels: Vec<String> =
        completion_items_from_result(&refreshed_registration_completion)
            .iter()
            .filter_map(|item| item.get("label").and_then(|value| value.as_str()))
            .map(str::to_string)
            .collect();
    assert!(
        refreshed_registration_labels
            .iter()
            .any(|label| label == "displayName"),
        "expected open unsaved FormType field completion `displayName`, got: {:?}",
        refreshed_registration_labels
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
async fn test_twig_foreach_entity_collection_members_from_doctrine_target_entity() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-twig-entity-collection-context-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp_root);
    fs::create_dir_all(tmp_root.join("src/Controller")).unwrap();
    fs::create_dir_all(tmp_root.join("src/Entity")).unwrap();
    fs::create_dir_all(tmp_root.join("templates/porting_request")).unwrap();

    let file_uri = |path: &std::path::Path| php_lsp_types::uri::path_to_uri(path).unwrap();
    let root_uri = file_uri(&tmp_root);
    let request_path = tmp_root.join("src/Entity/PortingRequest.php");
    let number_path = tmp_root.join("src/Entity/PortingNumber.php");
    let history_path = tmp_root.join("src/Entity/RequestStatusHistory.php");
    let status_path = tmp_root.join("src/Entity/RequestStatus.php");
    let controller_path = tmp_root.join("src/Controller/PortingRequestController.php");
    let twig_path = tmp_root.join("templates/porting_request/show.html.twig");
    let number_uri = file_uri(&number_path);
    let history_uri = file_uri(&history_path);
    let status_uri = file_uri(&status_path);
    let twig_uri = file_uri(&twig_path);

    let request_php = r#"<?php
namespace App\Entity;

use Doctrine\Common\Collections\Collection;
use Doctrine\ORM\Mapping as ORM;

class PortingRequest
{
    #[ORM\OneToMany(
        targetEntity: PortingNumber::class,
        mappedBy: 'request',
        cascade: ['persist']
    )]
    private Collection $portingNumbers;

    #[ORM\OneToMany(
        targetEntity: RequestStatusHistory::class,
        mappedBy: 'request',
        cascade: ['persist']
    )]
    private Collection $statusHistories;

    public function getPortingNumbers(): Collection
    {
        return $this->portingNumbers;
    }

    public function addPortingNumber(PortingNumber $portingNumber): static
    {
        return $this;
    }

    public function removePortingNumber(PortingNumber $portingNumber): static
    {
        return $this;
    }

    public function getStatusHistory(): Collection
    {
        return $this->statusHistories;
    }

    public function addStatusHistory(RequestStatusHistory $statusHistory): static
    {
        return $this;
    }

    public function removeStatusHistory(RequestStatusHistory $statusHistory): static
    {
        return $this;
    }
}
"#;
    let number_php = r#"<?php
namespace App\Entity;

class PortingNumber
{
    private ?int $id = null;
    private string $phoneNumber = '';

    public function getId(): ?int { return $this->id; }
    public function getPhoneNumber(): string { return $this->phoneNumber; }
}
"#;
    let history_php = r#"<?php
namespace App\Entity;

class RequestStatusHistory
{
    private ?int $id = null;
    private ?RequestStatus $status = null;

    public function getId(): ?int { return $this->id; }
    public function getStatus(): ?RequestStatus { return $this->status; }
}
"#;
    let status_php = r#"<?php
namespace App\Entity;

class RequestStatus
{
    private string $name = '';
    public function getName(): string { return $this->name; }
}
"#;
    let controller_php = r#"<?php
namespace App\Controller;

use App\Entity\PortingRequest;

final class PortingRequestController
{
    public function show(PortingRequest $portingRequest): void
    {
        $this->render('porting_request/show.html.twig', [
            'portingRequest' => $portingRequest,
        ]);
    }
}
"#;
    let completion_marker = "/*complete*/";
    let twig_with_marker = format!(
        concat!(
            "{{% for portingNumber in portingRequest.portingNumbers %}}\n",
            "{{{{ portingNumber.{} }}}}\n",
            "{{{{ portingNumber.id }}}}\n",
            "{{{{ portingNumber.phoneNumber }}}}\n",
            "{{% endfor %}}\n",
            "{{% for statusHistory in portingRequest.statusHistory %}}\n",
            "{{{{ statusHistory.id }}}}\n",
            "{{{{ statusHistory.status.name }}}}\n",
            "{{% endfor %}}\n",
        ),
        completion_marker
    );
    let twig = twig_with_marker.replace(completion_marker, "");
    let completion_prefix = twig_with_marker[..twig_with_marker.find(completion_marker).unwrap()]
        .replace(completion_marker, "");
    let completion_position = utf16_position_for_offset(&twig, completion_prefix.len());
    let porting_number_hover_position = utf16_position_at(&twig, "portingNumber.id");
    let porting_number_id_definition_position = utf16_position_after(&twig, "portingNumber.i");
    let porting_number_id_member_hover_offset =
        twig.find("portingNumber.id").unwrap() + "portingNumber.".len();
    let porting_number_id_member_hover_position =
        utf16_position_for_offset(&twig, porting_number_id_member_hover_offset);
    let status_history_hover_position = utf16_position_at(&twig, "statusHistory.id");
    let status_history_id_definition_position = utf16_position_after(&twig, "statusHistory.i");
    let status_member_hover_offset =
        twig.find("statusHistory.status.name").unwrap() + "statusHistory.".len();
    let status_member_hover_position = utf16_position_for_offset(&twig, status_member_hover_offset);
    let status_name_member_hover_offset =
        twig.find("statusHistory.status.name").unwrap() + "statusHistory.status.".len();
    let status_name_member_hover_position =
        utf16_position_for_offset(&twig, status_name_member_hover_offset);
    let status_name_definition_position = utf16_position_at(&twig, "name }}");
    let porting_number_inlay_offset = twig.find("portingNumber in portingRequest").unwrap();
    let porting_number_inlay_position =
        utf16_position_for_offset(&twig, porting_number_inlay_offset + "portingNumber".len());
    let status_history_inlay_offset = twig.find("statusHistory in portingRequest").unwrap();
    let status_history_inlay_position =
        utf16_position_for_offset(&twig, status_history_inlay_offset + "statusHistory".len());
    let end_position = utf16_position_for_offset(&twig, twig.len());

    fs::write(&request_path, request_php).unwrap();
    fs::write(&number_path, number_php).unwrap();
    fs::write(&history_path, history_php).unwrap();
    fs::write(&status_path, status_php).unwrap();
    fs::write(&controller_path, controller_php).unwrap();
    fs::write(&twig_path, &twig).unwrap();

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
    wait_for_indexing_phase(&mut notifications, "ready", Duration::from_secs(5)).await;
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
        next_publish_diagnostics(&mut notifications, &twig_uri, Duration::from_secs(2)).await;
    assert_eq!(
        diagnostics["diagnostics"].as_array().map(Vec::len),
        Some(0),
        "entity collection Twig fixture should stay diagnostic-clean, got: {}",
        diagnostics
    );

    let porting_number_hover = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(hover_request(
                2,
                &twig_uri,
                porting_number_hover_position.0,
                porting_number_hover_position.1,
            ))
            .await
            .unwrap(),
    );
    let porting_number_hover_text = porting_number_hover["contents"]["value"]
        .as_str()
        .unwrap_or_default();
    assert!(
        porting_number_hover_text.contains("PortingNumber $portingNumber")
            && porting_number_hover_text.contains(number_uri.as_str()),
        "expected Twig foreach variable hover to resolve PortingNumber with a class link, got: {}",
        porting_number_hover
    );

    let status_history_hover = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(hover_request(
                3,
                &twig_uri,
                status_history_hover_position.0,
                status_history_hover_position.1,
            ))
            .await
            .unwrap(),
    );
    let status_history_hover_text = status_history_hover["contents"]["value"]
        .as_str()
        .unwrap_or_default();
    assert!(
        status_history_hover_text.contains("RequestStatusHistory $statusHistory")
            && status_history_hover_text.contains(history_uri.as_str()),
        "expected Twig foreach variable hover to resolve RequestStatusHistory with a class link, got: {}",
        status_history_hover
    );

    for (request_id, position, expected_method, expected_uri) in [
        (
            4,
            porting_number_id_member_hover_position,
            "getId",
            number_uri.as_str(),
        ),
        (
            5,
            status_member_hover_position,
            "getStatus",
            status_uri.as_str(),
        ),
        (
            6,
            status_name_member_hover_position,
            "getName",
            status_uri.as_str(),
        ),
    ] {
        let hover = extract_result(
            service
                .ready()
                .await
                .unwrap()
                .call(hover_request(request_id, &twig_uri, position.0, position.1))
                .await
                .unwrap(),
        );
        let hover_text = hover["contents"]["value"].as_str().unwrap_or_default();
        assert!(
            hover_text.contains(expected_method) && hover_text.contains(expected_uri),
            "expected Twig property hover `{expected_method}` with class link `{expected_uri}`, got: {}",
            hover
        );
    }

    let completion = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(completion_request(
                7,
                &twig_uri,
                completion_position.0,
                completion_position.1,
            ))
            .await
            .unwrap(),
    );
    let labels: Vec<String> = completion_items_from_result(&completion)
        .iter()
        .filter_map(|item| item.get("label").and_then(|value| value.as_str()))
        .map(str::to_string)
        .collect();
    for expected in ["id", "phoneNumber"] {
        assert!(
            labels.iter().any(|label| label == expected),
            "expected Twig completion `{expected}` from Doctrine collection item, got: {:?}",
            labels
        );
    }

    for (request_id, position, expected_uri) in [
        (
            8,
            porting_number_id_definition_position,
            number_uri.as_str(),
        ),
        (
            9,
            status_history_id_definition_position,
            history_uri.as_str(),
        ),
        (10, status_name_definition_position, status_uri.as_str()),
    ] {
        let definition = extract_result(
            service
                .ready()
                .await
                .unwrap()
                .call(definition_request(
                    request_id, &twig_uri, position.0, position.1,
                ))
                .await
                .unwrap(),
        );
        assert_eq!(
            definition.get("uri").and_then(|uri| uri.as_str()),
            Some(expected_uri),
            "Twig definition request {request_id} should jump to expected entity symbol, got: {}",
            definition
        );
    }

    let inlay_result = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(inlay_hint_request(
                11,
                &twig_uri,
                0,
                0,
                end_position.0,
                end_position.1,
            ))
            .await
            .unwrap(),
    );
    let hints = inlay_result.as_array().cloned().unwrap_or_default();
    for (expected_label, expected_position, expected_link) in [
        (
            ": PortingNumber",
            porting_number_inlay_position,
            "PortingNumber",
        ),
        (
            ": RequestStatusHistory",
            status_history_inlay_position,
            "RequestStatusHistory",
        ),
    ] {
        assert!(
            hints.iter().any(|hint| {
                inlay_hint_label_text(hint).as_deref() == Some(expected_label)
                    && hint["position"]["line"].as_u64() == Some(expected_position.0 as u64)
                    && hint["position"]["character"].as_u64()
                        == Some(expected_position.1 as u64)
                    && inlay_hint_has_label_part_location(hint, expected_link)
            }),
            "expected Twig entity collection inlay hint `{expected_label}` with class link, got: {}",
            inlay_result
        );
    }

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
async fn test_twig_paginator_context_prefers_explicit_iterable_item_type() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-twig-pagination-explicit-item-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp_root);
    fs::create_dir_all(tmp_root.join("src/Controller")).unwrap();
    fs::create_dir_all(tmp_root.join("src/Entity")).unwrap();
    fs::create_dir_all(tmp_root.join("src/Repository")).unwrap();
    fs::create_dir_all(tmp_root.join("templates/data_request")).unwrap();

    let file_uri = |path: &std::path::Path| php_lsp_types::uri::path_to_uri(path).unwrap();
    let root_uri = file_uri(&tmp_root);
    let entity_path = tmp_root.join("src/Entity/DataRequest.php");
    let repository_path = tmp_root.join("src/Repository/DataRequestRepository.php");
    let controller_path = tmp_root.join("src/Controller/DataRequestController.php");
    let twig_path = tmp_root.join("templates/data_request/codes.html.twig");
    let twig_uri = file_uri(&twig_path);

    let entity_php = r#"<?php
namespace App\Entity;

class DataRequest
{
    public int $id = 0;
    public function getId(): int { return $this->id; }
}
"#;
    let repository_php = r#"<?php
namespace App\Repository;

use Doctrine\Bundle\DoctrineBundle\Repository\ServiceEntityRepository;

class DataRequestRepository extends ServiceEntityRepository
{
    /**
     * @return array<int, string>
     */
    public function npCodes(): array
    {
        return [];
    }
}
"#;
    let controller_php = r#"<?php
namespace App\Controller;

use App\Repository\DataRequestRepository;
use Knp\Component\Pager\PaginatorInterface;

final class DataRequestController
{
    public function codes(PaginatorInterface $paginator, DataRequestRepository $dataRequestRepository): void
    {
        $pagination = $paginator->paginate($dataRequestRepository->npCodes(), 1, 10);

        $this->render('data_request/codes.html.twig', [
            'pagination' => $pagination,
        ]);
    }
}
"#;
    let completion_marker = "/*complete*/";
    let twig_with_marker = format!(
        "{{% for code in pagination %}}\n{{{{ code }}}}\n{{{{ code.{} }}}}\n{{% endfor %}}\n",
        completion_marker
    );
    let completion_offset = twig_with_marker
        .find(completion_marker)
        .expect("test Twig should contain completion marker");
    let completion_position = utf16_position_for_offset(
        &twig_with_marker.replace(completion_marker, ""),
        completion_offset,
    );
    let twig = twig_with_marker.replace(completion_marker, "");
    let hover_position = utf16_position_at(&twig, "code }}");
    let foreach_variable_position = utf16_position_after(&twig, "code");
    let end_position = utf16_position_for_offset(&twig, twig.len());

    fs::write(&entity_path, entity_php).unwrap();
    fs::write(&repository_path, repository_php).unwrap();
    fs::write(&controller_path, controller_php).unwrap();
    fs::write(&twig_path, &twig).unwrap();

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
    wait_for_indexing_phase(&mut notifications, "ready", Duration::from_secs(5)).await;
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
        next_publish_diagnostics(&mut notifications, &twig_uri, Duration::from_secs(2)).await;
    assert_eq!(
        diagnostics["diagnostics"].as_array().map(Vec::len),
        Some(0),
        "explicit paginator item Twig context should stay diagnostic-clean, got: {}",
        diagnostics
    );

    let hover_resp = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(
            2,
            &twig_uri,
            hover_position.0,
            hover_position.1,
        ))
        .await
        .unwrap();
    let hover = extract_result(hover_resp);
    let hover_text = hover["contents"]["value"].as_str().unwrap_or_default();
    assert!(
        hover_text.contains("string $code"),
        "expected Twig hover to keep explicit paginator item string type, got: {}",
        hover
    );

    let completion_resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(
            3,
            &twig_uri,
            completion_position.0,
            completion_position.1,
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
        !labels.iter().any(|label| label == "getId" || label == "id"),
        "explicit string paginator item should not fall back to DataRequest members, got: {:?}",
        labels
    );

    let inlay_resp = service
        .ready()
        .await
        .unwrap()
        .call(inlay_hint_request(
            4,
            &twig_uri,
            0,
            0,
            end_position.0,
            end_position.1,
        ))
        .await
        .unwrap();
    let inlay_result = extract_result(inlay_resp);
    let hints = inlay_result.as_array().cloned().unwrap_or_default();
    assert!(
        hints.iter().any(|hint| {
            inlay_hint_label_text(hint).as_deref() == Some(": string")
                && hint["position"]["line"].as_u64() == Some(foreach_variable_position.0 as u64)
                && hint["position"]["character"].as_u64()
                    == Some(foreach_variable_position.1 as u64)
        }),
        "expected Twig inlay hint for explicit paginator item string type, got: {}",
        inlay_result
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
async fn test_twig_template_reports_twig_syntax_diagnostics() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-twig-template-syntax-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp_root);
    fs::create_dir_all(tmp_root.join("templates")).unwrap();
    let twig_path = tmp_root.join("templates/broken.html.twig");
    let twig_uri = php_lsp_types::uri::path_to_uri(&twig_path).unwrap();
    let twig = "{% if user %}\n{{ user.name }\n";
    fs::write(&twig_path, twig).unwrap();

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
        .call(did_open_notification_with_language(&twig_uri, "twig", twig))
        .await
        .unwrap();

    let diagnostics =
        next_publish_diagnostics(&mut notifications, &twig_uri, Duration::from_secs(1)).await;
    let messages = published_diagnostic_messages(&diagnostics);
    assert!(
        messages
            .iter()
            .any(|message| message == "Unclosed Twig expression"),
        "expected unclosed Twig expression diagnostic, got: {}",
        diagnostics
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("Unclosed Twig `if` block")),
        "expected unclosed Twig if block diagnostic, got: {}",
        diagnostics
    );
    assert!(
        diagnostics["diagnostics"].as_array().is_some_and(|items| {
            items.iter().all(|diagnostic| {
                diagnostic["code"].as_str() == Some("php-lsp.twigSyntax")
                    && diagnostic["range"]["start"]["line"].as_u64().is_some()
            })
        }),
        "Twig syntax diagnostics should carry explicit code and mapped original ranges, got: {}",
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
async fn test_twig_complex_expressions_are_best_effort_and_quiet() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-twig-complex-expressions-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp_root);
    fs::create_dir_all(tmp_root.join("templates")).unwrap();
    let twig_path = tmp_root.join("templates/complex.html.twig");
    let twig_uri = php_lsp_types::uri::path_to_uri(&twig_path).unwrap();
    let twig = concat!(
        "{% import 'forms.html.twig' as forms %}\n",
        "{{ user.name|upper }}\n",
        "{% if user is defined %}visible{% endif %}\n",
        "{% if user.id in ids %}allowed{% endif %}\n",
        "{% for item in users|filter(u => u.active) %}{{ item.name }}{% endfor %}\n",
        "{% set label = attribute(user, dynamic_name) %}{{ label }}\n",
        "{{ path('dashboard') }}\n",
        "{{ forms.input(user) }}\n",
        "{{ _self.card(user) }}\n",
        "{{ user.active ? 'yes' : 'no' }}\n",
        "{{ user.name ?? 'n/a' }}\n",
        "{{ user['name'] }}\n",
        "{% verbatim %}{{ user.name }{% endverbatim %}\n",
    );
    let path_position = utf16_position_at(twig, "path('dashboard')");
    let bracket_position = utf16_position_after(twig, "user[");
    let filter_completion_position = utf16_position_after(twig, "user.name|");
    fs::write(&twig_path, twig).unwrap();

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
        .call(did_open_notification_with_language(&twig_uri, "twig", twig))
        .await
        .unwrap();

    let diagnostics =
        next_publish_diagnostics(&mut notifications, &twig_uri, Duration::from_secs(1)).await;
    assert_eq!(
        diagnostics["diagnostics"].as_array().map(Vec::len),
        Some(0),
        "unsupported Twig expressions should stay quiet, got: {}",
        diagnostics
    );

    let hover_resp = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(
            2,
            &twig_uri,
            path_position.0,
            path_position.1,
        ))
        .await
        .unwrap();
    assert!(
        extract_result(hover_resp).is_null(),
        "unsupported Twig functions should not be mapped to misleading PHP hover"
    );

    let definition_resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(
            3,
            &twig_uri,
            bracket_position.0,
            bracket_position.1,
        ))
        .await
        .unwrap();
    assert!(
        extract_result(definition_resp).is_null(),
        "unsupported Twig bracket access should not return a misleading definition"
    );

    let completion_resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(
            4,
            &twig_uri,
            filter_completion_position.0,
            filter_completion_position.1,
        ))
        .await
        .unwrap();
    let completion = extract_result(completion_resp);
    assert!(
        completion_items_from_result(&completion).is_empty(),
        "unsupported Twig filter expression should not return misleading completions, got: {}",
        completion
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
async fn test_twig_context_types_refresh_after_controller_render_context_change() {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (notification_tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(notification) = socket.next().await {
            let _ = notification_tx.send(notification);
        }
    });

    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-twig-context-refresh-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp_root);
    fs::create_dir_all(tmp_root.join("src/Controller")).unwrap();
    fs::create_dir_all(tmp_root.join("src/Entity")).unwrap();
    fs::create_dir_all(tmp_root.join("templates/dashboard")).unwrap();

    let file_uri = |path: &std::path::Path| php_lsp_types::uri::path_to_uri(path).unwrap();
    let root_uri = file_uri(&tmp_root);
    let user_path = tmp_root.join("src/Entity/User.php");
    let admin_path = tmp_root.join("src/Entity/Admin.php");
    let controller_path = tmp_root.join("src/Controller/DashboardController.php");
    let twig_path = tmp_root.join("templates/dashboard/show.html.twig");
    let user_uri = file_uri(&user_path);
    let admin_uri = file_uri(&admin_path);
    let controller_uri = file_uri(&controller_path);
    let twig_uri = file_uri(&twig_path);

    let user_php = r#"<?php
namespace App\Entity;

class User
{
    public function getName(): string { return ''; }
}
"#;
    let admin_php = r#"<?php
namespace App\Entity;

class Admin
{
    public function getRole(): string { return ''; }
}
"#;
    let controller_php = |class_name: &str| {
        format!(
            r#"<?php
namespace App\Controller;

use App\Entity\Admin;
use App\Entity\User;

final class DashboardController
{{
    public function show(): void
    {{
        $this->render('dashboard/show.html.twig', [
            'user' => new {class_name}(),
        ]);
    }}
}}
"#
        )
    };
    let completion_marker = "/*complete*/";
    let twig_with_marker = format!("{{{{ user.get{} }}}}\n", completion_marker);
    let completion_offset = twig_with_marker
        .find(completion_marker)
        .expect("test Twig should contain completion marker");
    let completion_prefix = &twig_with_marker[..completion_offset];
    let completion_line = completion_prefix
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count() as u32;
    let completion_line_start = completion_prefix
        .rfind('\n')
        .map(|idx| idx + 1)
        .unwrap_or(0);
    let completion_character = (completion_prefix.len() - completion_line_start) as u32;
    let twig = twig_with_marker.replace(completion_marker, "");
    let initial_controller_php = controller_php("User");
    let changed_controller_php = controller_php("Admin");

    fs::write(&user_path, user_php).unwrap();
    fs::write(&admin_path, admin_php).unwrap();
    fs::write(&controller_path, &initial_controller_php).unwrap();
    fs::write(&twig_path, &twig).unwrap();

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
        .call(did_open_notification(&admin_uri, admin_php))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(
            &controller_uri,
            &initial_controller_php,
        ))
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
        "initial Twig context should stay quiet, got: {}",
        diagnostics
    );

    let initial_completion_resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(
            2,
            &twig_uri,
            completion_line,
            completion_character,
        ))
        .await
        .unwrap();
    let initial_completion = extract_result(initial_completion_resp);
    let initial_labels: Vec<String> = completion_items_from_result(&initial_completion)
        .iter()
        .filter_map(|item| item.get("label").and_then(|value| value.as_str()))
        .map(str::to_string)
        .collect();
    assert!(
        initial_labels.iter().any(|label| label == "getName"),
        "expected initial User context completion to include getName, got: {:?}",
        initial_labels
    );

    fs::write(&controller_path, &changed_controller_php).unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_change_full_notification(
            &controller_uri,
            2,
            &changed_controller_php,
        ))
        .await
        .unwrap();

    let diagnostics =
        next_publish_diagnostics(&mut notifications, &twig_uri, Duration::from_secs(1)).await;
    assert_eq!(
        diagnostics["diagnostics"].as_array().map(Vec::len),
        Some(0),
        "refreshed Twig context should stay quiet, got: {}",
        diagnostics
    );

    let refreshed_completion_resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(
            3,
            &twig_uri,
            completion_line,
            completion_character,
        ))
        .await
        .unwrap();
    let refreshed_completion = extract_result(refreshed_completion_resp);
    let refreshed_labels: Vec<String> = completion_items_from_result(&refreshed_completion)
        .iter()
        .filter_map(|item| item.get("label").and_then(|value| value.as_str()))
        .map(str::to_string)
        .collect();
    assert!(
        refreshed_labels.iter().any(|label| label == "getRole"),
        "expected refreshed Admin context completion to include getRole, got: {:?}",
        refreshed_labels
    );
    assert!(
        !refreshed_labels.iter().any(|label| label == "getName"),
        "stale User completion should not survive controller context change, got: {:?}",
        refreshed_labels
    );

    service
        .ready()
        .await
        .unwrap()
        .call(did_close_notification(&controller_uri))
        .await
        .unwrap();

    let closed_completion_resp = service
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
    let closed_completion = extract_result(closed_completion_resp);
    let closed_labels: Vec<String> = completion_items_from_result(&closed_completion)
        .iter()
        .filter_map(|item| item.get("label").and_then(|value| value.as_str()))
        .map(str::to_string)
        .collect();
    assert!(
        closed_labels.iter().any(|label| label == "getRole"),
        "expected refreshed disk context after closing controller, got: {:?}",
        closed_labels
    );
    assert!(
        !closed_labels.iter().any(|label| label == "getName"),
        "stale User completion should not survive controller close, got: {:?}",
        closed_labels
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
async fn test_twig_context_disk_cache_is_evicted_for_opened_php_source_change() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-twig-context-cache-evict-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp_root);
    fs::create_dir_all(tmp_root.join("src/Controller")).unwrap();
    fs::create_dir_all(tmp_root.join("src/Entity")).unwrap();
    fs::create_dir_all(tmp_root.join("templates/dashboard")).unwrap();

    let file_uri = |path: &std::path::Path| php_lsp_types::uri::path_to_uri(path).unwrap();
    let root_uri = file_uri(&tmp_root);
    let user_path = tmp_root.join("src/Entity/User.php");
    let admin_path = tmp_root.join("src/Entity/Admin.php");
    let controller_path = tmp_root.join("src/Controller/DashboardController.php");
    let twig_path = tmp_root.join("templates/dashboard/show.html.twig");
    let user_uri = file_uri(&user_path);
    let admin_uri = file_uri(&admin_path);
    let controller_uri = file_uri(&controller_path);
    let twig_uri = file_uri(&twig_path);

    let user_php = r#"<?php
namespace App\Entity;

class User
{
    public function getName(): string { return ''; }
}
"#;
    let admin_php = r#"<?php
namespace App\Entity;

class Admin
{
    public function getRole(): string { return ''; }
}
"#;
    let controller_php = |class_name: &str| {
        format!(
            r#"<?php
namespace App\Controller;

use App\Entity\Admin;
use App\Entity\User;

final class DashboardController
{{
    public function show(): void
    {{
        $this->render('dashboard/show.html.twig', [
            'user' => new {class_name}(),
        ]);
    }}
}}
"#
        )
    };
    let completion_marker = "/*complete*/";
    let twig_with_marker = format!("{{{{ user.get{} }}}}\n", completion_marker);
    let completion_offset = twig_with_marker
        .find(completion_marker)
        .expect("test Twig should contain completion marker");
    let completion_prefix = &twig_with_marker[..completion_offset];
    let completion_line = completion_prefix
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count() as u32;
    let completion_line_start = completion_prefix
        .rfind('\n')
        .map(|idx| idx + 1)
        .unwrap_or(0);
    let completion_character = completion_prefix[completion_line_start..]
        .encode_utf16()
        .count() as u32;
    let twig = twig_with_marker.replace(completion_marker, "");
    let initial_controller_php = controller_php("User");
    let changed_controller_php = controller_php("Admin");

    fs::write(&user_path, user_php).unwrap();
    fs::write(&admin_path, admin_php).unwrap();
    fs::write(&controller_path, &initial_controller_php).unwrap();
    fs::write(&twig_path, &twig).unwrap();

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
        .call(did_open_notification(&admin_uri, admin_php))
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

    let initial_completion_resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(
            2,
            &twig_uri,
            completion_line,
            completion_character,
        ))
        .await
        .unwrap();
    let initial_completion = extract_result(initial_completion_resp);
    let initial_labels: Vec<String> = completion_items_from_result(&initial_completion)
        .iter()
        .filter_map(|item| item.get("label").and_then(|value| value.as_str()))
        .map(str::to_string)
        .collect();
    assert!(
        initial_labels.iter().any(|label| label == "getName"),
        "expected warmed disk cache to provide User context, got: {:?}",
        initial_labels
    );

    fs::write(&controller_path, &changed_controller_php).unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(
            &controller_uri,
            &changed_controller_php,
        ))
        .await
        .unwrap();

    let opened_completion_resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(
            3,
            &twig_uri,
            completion_line,
            completion_character,
        ))
        .await
        .unwrap();
    let opened_completion = extract_result(opened_completion_resp);
    let opened_labels: Vec<String> = completion_items_from_result(&opened_completion)
        .iter()
        .filter_map(|item| item.get("label").and_then(|value| value.as_str()))
        .map(str::to_string)
        .collect();
    assert!(
        opened_labels.iter().any(|label| label == "getRole"),
        "expected open controller overlay to provide Admin context, got: {:?}",
        opened_labels
    );

    service
        .ready()
        .await
        .unwrap()
        .call(did_close_notification(&controller_uri))
        .await
        .unwrap();

    let closed_completion_resp = service
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
    let closed_completion = extract_result(closed_completion_resp);
    let closed_labels: Vec<String> = completion_items_from_result(&closed_completion)
        .iter()
        .filter_map(|item| item.get("label").and_then(|value| value.as_str()))
        .map(str::to_string)
        .collect();
    assert!(
        closed_labels.iter().any(|label| label == "getRole"),
        "expected closed controller to keep refreshed disk Admin context, got: {:?}",
        closed_labels
    );
    assert!(
        !closed_labels.iter().any(|label| label == "getName"),
        "stale User context should not survive controller open/close, got: {:?}",
        closed_labels
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
