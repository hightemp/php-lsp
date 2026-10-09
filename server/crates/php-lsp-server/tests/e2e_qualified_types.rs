mod support;
use php_lsp_types::uri::path_to_uri;
use support::*;

async fn send(service: &mut LspService<PhpLspBackend>, request: Request) -> serde_json::Value {
    let response = service.ready().await.unwrap().call(request).await.unwrap();
    response
        .map(|response| {
            assert!(response.error().is_none(), "{response:?}");
            extract_result(Some(response))
        })
        .unwrap_or(serde_json::Value::Null)
}

async fn service() -> LspService<PhpLspBackend> {
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move { while socket.next().await.is_some() {} });
    send(
        &mut service,
        initialize_request_with_options(
            1,
            None,
            Some(json!({"stubExtensions":[],"indexVendor":false,"diagnosticsMode":"off"})),
        ),
    )
    .await;
    service
}

async fn models(service: &mut LspService<PhpLspBackend>) {
    send(
        service,
        did_open_notification(
            "file:///test/AbsoluteDecoy.php",
            "<?php namespace App; class Foo {public int $name; function decoyOnly():void {}} ",
        ),
    )
    .await;
    send(service,did_open_notification("file:///test/RelativeModel.php","<?php namespace App\\Sub\\App; class Foo {public string $name; function relativeOnly():void {}} ")).await;
}

async fn assert_members(
    service: &mut LspService<PhpLspBackend>,
    uri: &str,
    source: &str,
    needle: &str,
    expected: &str,
    forbidden: &str,
    target_uri: &str,
) {
    let (line, col) = utf16_position_after(source, needle);
    let result = send(service, completion_request(20, uri, line, col)).await;
    let items = completion_items_from_result(&result);
    assert!(
        items.iter().any(|item| item["label"] == expected),
        "missing {expected}: {result}; source={source}"
    );
    assert!(
        !items.iter().any(|item| item["label"] == forbidden),
        "foreign member: {result}"
    );
    let (line, col) = utf16_position_at(source, "$model;");
    let hover = send(service, hover_request(21, uri, line, col + 1)).await;
    assert!(
        hover_markdown_value(&hover).contains(target_uri),
        "wrong hover target: {hover}"
    );
    let definition = send(service, type_definition_request(22, uri, line, col + 1)).await;
    let location = definition
        .as_array()
        .and_then(|items| items.first())
        .unwrap_or(&definition);
    assert_eq!(
        location["uri"], target_uri,
        "wrong type definition: {definition}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn cross_file_native_return_uses_relative_type_despite_absolute_decoy() {
    let mut service = service().await;
    models(&mut service).await;
    let uri = "file:///test/RelativeNativeReturn.php";
    let source="<?php namespace App\\Sub; class Service {function load(): App\\Foo {return new App\\Foo();}}\n$model=(new Service())->load();\n$model;\n$model->";
    send(&mut service, did_open_notification(uri, source)).await;
    assert_members(
        &mut service,
        uri,
        source,
        "$model->",
        "relativeOnly",
        "decoyOnly",
        "file:///test/RelativeModel.php",
    )
    .await;
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn inherited_native_return_keeps_its_declaration_scope_across_namespaces() {
    let mut service = service().await;
    models(&mut service).await;
    send(&mut service, did_open_notification("file:///test/BaseService.php", "<?php namespace App\\Sub; class Base {function load(): App\\Foo {return new App\\Foo();}} ")).await;
    let uri = "file:///test/InheritedConsumer.php";
    let source="<?php namespace Other; class Child extends \\App\\Sub\\Base {}\n$model=(new Child())->load();\n$model;\n$model->";
    send(&mut service, did_open_notification(uri, source)).await;
    assert_members(
        &mut service,
        uri,
        source,
        "$model->",
        "relativeOnly",
        "decoyOnly",
        "file:///test/RelativeModel.php",
    )
    .await;
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn phpdoc_return_and_unsaved_absolute_change_update_type_targets() {
    let mut service = service().await;
    models(&mut service).await;
    let uri = "file:///test/RelativeDocReturn.php";
    let source="<?php namespace App\\Sub; class Service {/** @return App\\Foo */ function load(): object {return new App\\Foo();}}\n$model=(new Service())->load();\n$model;\n$model->";
    send(&mut service, did_open_notification(uri, source)).await;
    assert_members(
        &mut service,
        uri,
        source,
        "$model->",
        "relativeOnly",
        "decoyOnly",
        "file:///test/RelativeModel.php",
    )
    .await;
    let changed = source
        .replace("@return App\\Foo", "@return \\App\\Foo")
        .replace("new App\\Foo()", "new \\App\\Foo()")
        .replace('\n', "\r\n");
    send(&mut service, did_change_full_notification(uri, 2, &changed)).await;
    assert_members(
        &mut service,
        uri,
        &changed,
        "$model->",
        "decoyOnly",
        "relativeOnly",
        "file:///test/AbsoluteDecoy.php",
    )
    .await;
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn phpdoc_foreach_inlay_and_hover_keep_relative_type_after_unicode() {
    let mut service = service().await;
    models(&mut service).await;
    let uri = "file:///test/RelativeForeach.php";
    let source="<?php namespace App\\Sub; class Service {function run() {\r\n/** @var array<int, App\\Foo> $items */ $items=[];\r\nforeach($items as $model) { /* 😀 */ $model; $model->relativeOnly(); }\r\n}}";
    send(&mut service, did_open_notification(uri, source)).await;
    let (line, col) = utf16_position_at(source, "$model;");
    let hover = send(&mut service, hover_request(20, uri, line, col + 1)).await;
    assert!(
        hover_markdown_value(&hover).contains("file:///test/RelativeModel.php"),
        "wrong hover: {hover}"
    );
    let (end_line, end_col) = utf16_position_for_offset(source, source.len());
    let hints = send(
        &mut service,
        inlay_hint_request(21, uri, 0, 0, end_line, end_col),
    )
    .await;
    let (item_line, item_col) = utf16_position_after(source, "as $model");
    let hint = hints
        .as_array()
        .unwrap()
        .iter()
        .find(|hint| {
            hint["position"] == json!({"line":item_line,"character":item_col}) && hint["kind"] == 1
        })
        .expect("foreach type hint");
    let target = hint["label"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|part| part.get("location"))
        .expect("type link");
    assert_eq!(
        target["uri"], "file:///test/RelativeModel.php",
        "wrong inlay: {hint}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn template_factory_arguments_keep_resolved_class_string_and_object_targets() {
    let mut service = service().await;
    send(
        &mut service,
        did_open_notification(
            "file:///test/FactoryModel.php",
            "<?php namespace Lib; class Model {function modelOnly():void {}} ",
        ),
    )
    .await;
    let uri = "file:///test/QualifiedFactory.php";
    for expression in [
        r"$factory->make(\Lib\Model::class)",
        r"$factory->make('\Lib\Model')",
        r"$factory->identity(new \Lib\Model())",
    ] {
        let source=format!("<?php namespace App\\Sub; class Factory {{ /**\n * @template T of object\n * @param class-string<T> $class\n * @return T\n */ function make(string $class): object {{return new $class();}} /**\n * @template T of object\n * @param T $value\n * @return T\n */ function identity(object $value): object {{return $value;}} }}\n$factory=new Factory();\n$model={expression};\n$model;\n$model->");
        send(&mut service, did_open_notification(uri, &source)).await;
        let offset = source.find(expression).unwrap() + expression.find("->").unwrap() + 2;
        let (line, col) = utf16_position_for_offset(&source, offset);
        let hover = send(&mut service, hover_request(30, uri, line, col)).await;
        let text = hover_markdown_value(&hover);
        assert!(
            text.contains("**Resolved returns:**")
                && text.contains("file:///test/FactoryModel.php"),
            "wrong {expression} return: {text}"
        );
        let (end_line, end_col) = utf16_position_for_offset(&source, source.len());
        let hints = send(
            &mut service,
            inlay_hint_request(31, uri, 0, 0, end_line, end_col),
        )
        .await;
        let (line, col) = utf16_position_after(&source, "$model");
        let hint = hints
            .as_array()
            .unwrap()
            .iter()
            .find(|hint| {
                hint["position"] == json!({"line":line,"character":col}) && hint["kind"] == 1
            })
            .unwrap_or_else(|| panic!("missing {expression} hint: {hints}"));
        let parts = hint["label"]
            .as_array()
            .unwrap_or_else(|| panic!("missing {expression} link: {hint}"));
        let target = parts.iter().find_map(|part| part.get("location")).unwrap();
        assert_eq!(
            target["uri"], "file:///test/FactoryModel.php",
            "wrong {expression} target: {hint}"
        );
        send(&mut service, did_close_notification(uri)).await;
    }
    send(&mut service, shutdown_request(99)).await;
}

struct Workspace(std::path::PathBuf);

#[tokio::test(flavor = "current_thread")]
async fn doctrine_repository_results_keep_cross_namespace_hover_and_member_targets() {
    let mut service = service().await;
    send(
        &mut service,
        did_open_notification(
            "file:///test/DoctrineModel.php",
            "<?php namespace Lib; class Model {function modelOnly():void {}} ",
        ),
    )
    .await;
    send(
        &mut service,
        did_open_notification(
            "file:///test/DoctrineDecoy.php",
            "<?php namespace Lib\\Lib; class Model {function decoyOnly():void {}} ",
        ),
    )
    .await;
    send(&mut service, did_open_notification("file:///test/DoctrineApi.php", "<?php namespace Doctrine\\ORM; class EntityManagerInterface {function getRepository(string $class): EntityRepository {return new EntityRepository();}} class EntityRepository {function find(int $id): ?object {return null;}}")).await;
    let uri = "file:///test/DoctrineRelativeConsumer.php";
    let source = "<?php namespace App\\Sub; function run(\\Doctrine\\ORM\\EntityManagerInterface $em) { $model=$em->getRepository(\\Lib\\Model::class)->find(1); $model; $model->modelOnly(); }";
    send(&mut service, did_open_notification(uri, source)).await;
    let (line, col) = utf16_position_at(source, "$model;");
    let hover = send(&mut service, hover_request(40, uri, line, col + 1)).await;
    assert!(
        hover_markdown_value(&hover).contains("file:///test/DoctrineModel.php"),
        "wrong Doctrine type: {hover}"
    );
    let (line, col) = utf16_position_at(source, "modelOnly();");
    let definition = send(&mut service, definition_request(41, uri, line, col)).await;
    let location = definition
        .as_array()
        .and_then(|items| items.first())
        .unwrap_or(&definition);
    assert_eq!(
        location["uri"], "file:///test/DoctrineModel.php",
        "wrong Doctrine member: {definition}"
    );
    send(&mut service, shutdown_request(99)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn outside_class_phpdoc_types_use_file_scope_instead_of_global_decoys() {
    let mut service = service().await;
    models(&mut service).await;
    send(
        &mut service,
        did_open_notification(
            "file:///test/GlobalFoo.php",
            "<?php class Foo {function decoyOnly():void {}} ",
        ),
    )
    .await;
    send(
        &mut service,
        did_open_notification(
            "file:///test/NamespacedFoo.php",
            "<?php namespace App\\Sub; class Foo {function relativeOnly():void {}} ",
        ),
    )
    .await;
    let uri = "file:///test/OutsideClassTypes.php";
    for (name, target) in [
        ("App\\Foo", "file:///test/RelativeModel.php"),
        ("Foo", "file:///test/NamespacedFoo.php"),
    ] {
        let source = format!("<?php namespace App\\Sub; /** @var {name} $model */ $model=$unknown;\n$model;\n$model->");
        send(&mut service, did_open_notification(uri, &source)).await;
        assert_members(
            &mut service,
            uri,
            &source,
            "$model->",
            "relativeOnly",
            "decoyOnly",
            target,
        )
        .await;
        send(&mut service, did_close_notification(uri)).await;
    }
    send(&mut service, shutdown_request(99)).await;
}
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn twig_controller_method_return_preserves_qualified_relative_type() {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let workspace = Workspace(
        std::env::temp_dir().join(format!("php-lsp-p2-24-{}-{nonce}", std::process::id())),
    );
    let root = &workspace.0;
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("templates")).unwrap();
    fs::write(
        root.join("composer.json"),
        r#"{"autoload":{"psr-4":{"":"src/"}}}"#,
    )
    .unwrap();
    fs::write(
        root.join("src/Decoy.php"),
        "<?php namespace App; class Foo {public int $name; public int $decoyOnly;}",
    )
    .unwrap();
    fs::write(
        root.join("src/Relative.php"),
        "<?php namespace App\\Sub\\App; class Foo {public string $name; public int $relativeOnly;}",
    )
    .unwrap();
    fs::write(root.join("src/Controller.php"),"<?php namespace App\\Sub; class Controller {function model(): App\\Foo {return new App\\Foo();} function page() {$this->render('view.twig',['item'=>$this->model()]);}}").unwrap();
    let twig = "{{ item.name }}";
    fs::write(root.join("templates/view.twig"), twig).unwrap();
    let uri = path_to_uri(&root.join("templates/view.twig")).unwrap();
    let root_uri = path_to_uri(root).unwrap();
    let (mut service, mut socket) = LspService::new(PhpLspBackend::new);
    let (tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(message) = socket.next().await {
            let _ = tx.send(message);
        }
    });
    send(
        &mut service,
        initialize_request_with_options(
            1,
            Some(&root_uri),
            Some(json!({"stubExtensions":[],"indexVendor":false,"diagnosticsMode":"off"})),
        ),
    )
    .await;
    send(&mut service, initialized_notification()).await;
    wait_for_indexing_phase(&mut notifications, "ready", Duration::from_secs(15)).await;
    send(
        &mut service,
        did_open_notification_with_language(&uri, "twig", twig),
    )
    .await;
    let completion = send(&mut service, completion_request(20, &uri, 0, 8)).await;
    let items = completion_items_from_result(&completion);
    assert!(
        items.iter().any(|item| item["label"] == "relativeOnly"),
        "wrong Twig context: {completion}"
    );
    assert!(
        !items.iter().any(|item| item["label"] == "decoyOnly"),
        "foreign Twig member: {completion}"
    );
    let definition = send(&mut service, definition_request(21, &uri, 0, 10)).await;
    let location = definition
        .as_array()
        .and_then(|items| items.first())
        .unwrap_or(&definition);
    assert_eq!(
        location["uri"],
        path_to_uri(&root.join("src/Relative.php")).unwrap(),
        "wrong Twig definition: {definition}"
    );
    send(&mut service, shutdown_request(99)).await;
}
