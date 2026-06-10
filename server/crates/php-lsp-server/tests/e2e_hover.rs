mod support;

use support::*;

#[tokio::test(flavor = "current_thread")]
async fn test_open_file_and_hover() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    // Initialize
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

    // Open a PHP file with a class
    let code = r#"<?php
namespace App;

class Greeter {
    public string $prefix = 'Hello';

    /** Say hello to someone. */
    public function greet(string $name): string {
        return "$this->prefix, $name!";
    }
}

$g = new Greeter();
$g->greet("World");
$g->prefix;
"#;
    let uri = "file:///test/Greeter.php";
    let class_position = utf16_position_at(code, "Greeter();");
    let property_position = utf16_position_at(code, "prefix;\n");

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    // Hover over "Greeter" in "new Greeter()"
    let resp = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(2, uri, class_position.0, class_position.1))
        .await
        .unwrap();

    let result = extract_result(resp);
    let contents = hover_markdown_value(&result);
    assert!(
        contents.contains("```php\nclass Greeter\n```"),
        "class hover should use source-like local declaration, got: {}",
        contents
    );
    assert!(
        contents.contains("**Symbol:** [`App\\Greeter`](<file:///test/Greeter.php#L4>)"),
        "class hover should expose linked FQN metadata, got: {}",
        contents
    );
    assert!(
        contents.contains("**Source:** [`/test/Greeter.php:4`](<file:///test/Greeter.php#L4>)"),
        "class hover should expose clickable source metadata, got: {}",
        contents
    );

    let property_resp = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(
            3,
            uri,
            property_position.0,
            property_position.1,
        ))
        .await
        .unwrap();
    let property_hover = hover_markdown_value(&extract_result(property_resp));
    assert!(
        property_hover.contains("```php\npublic string $prefix\n```"),
        "property hover should use source-like local declaration, got: {}",
        property_hover
    );
    assert!(
        property_hover
            .contains("**Symbol:** [`App\\Greeter::$prefix`](<file:///test/Greeter.php#L5>)"),
        "property hover should expose linked FQN metadata, got: {}",
        property_hover
    );
    assert!(
        property_hover
            .contains("**Source:** [`/test/Greeter.php:5`](<file:///test/Greeter.php#L5>)"),
        "property hover should expose clickable source metadata, got: {}",
        property_hover
    );

    // Shutdown
    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn test_hover_local_variable_with_inline_phpdoc_var() {
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

class Baz {
    public function test(): void {}
}

function makeBaz() {}

function run(): void {
    /**
     * Local baz variable.
     * @var Baz $baz2
     */
    $baz2 = makeBaz();
    $baz2->test();
}
"#;
    let uri = "file:///test/hover-var-phpdoc.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    // Hover on "$baz2" in "$baz2->test();"
    let resp = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(2, uri, 15, 6))
        .await
        .unwrap();

    let result = extract_result(resp);
    assert!(
        !result.is_null(),
        "hover should return content for local variable"
    );

    let contents = result
        .get("contents")
        .and_then(|c| c.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        contents.contains("$baz2") && contents.contains("Baz"),
        "hover should include variable name and inferred type, got: {}",
        contents
    );
    assert!(
        contents.contains("Local baz variable.") || contents.contains("@var"),
        "hover should include PHPDoc context, got: {}",
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
async fn test_hover_signature_types_include_class_links() {
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

class User {}
class Response {}

class Service {
    public function assign(User $user): Response {
        return new Response();
    }
}
"#;
    let uri = "file:///test/hover-signature-class-links.php";
    let assign_position = utf16_position_at(code, "assign(User");

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(2, uri, assign_position.0, assign_position.1))
        .await
        .unwrap();
    let hover = hover_markdown_value(&extract_result(hover));

    assert!(
        hover.contains("```php\npublic function assign(\n    User $user\n): Response\n```"),
        "expected source-like local method declaration, got: {}",
        hover
    );
    assert!(
        hover.contains(
            "**Symbol:** [`App\\Service::assign`](<file:///test/hover-signature-class-links.php#L8>)"
        ),
        "expected linked FQN metadata in member hover, got: {}",
        hover
    );
    assert!(
        hover.contains(
            "**Declared in:** [`App\\Service`](<file:///test/hover-signature-class-links.php#L7>)"
        ),
        "expected declaring class link in member hover, got: {}",
        hover
    );
    assert!(
        hover.contains(
            "**Source:** [`/test/hover-signature-class-links.php:8`](<file:///test/hover-signature-class-links.php#L8>)"
        ),
        "expected clickable source metadata in member hover, got: {}",
        hover
    );
    assert!(
        hover.contains("**Parameters:**")
            && hover.contains(
                "- `User $user`: [`User`](<file:///test/hover-signature-class-links.php#L4>)"
            ),
        "expected linked parameter detail in signature hover, got: {}",
        hover
    );
    assert!(
        hover.contains(
            "**Returns:** [`Response`](<file:///test/hover-signature-class-links.php#L5>)"
        ),
        "expected return class link in signature hover, got: {}",
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
async fn test_hover_constant_declarations_use_source_like_metadata() {
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

const GLOBAL_LIMIT = 10;

class Settings
{
    public const string MODE = 'safe';

    public function read(): void
    {
        self::MODE;
        GLOBAL_LIMIT;
    }
}
"#;
    let uri = "file:///test/hover-constants.php";
    let mode_position = utf16_position_at(code, "MODE;");
    let global_position = utf16_position_at(code, "GLOBAL_LIMIT;");

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let mode_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(2, uri, mode_position.0, mode_position.1))
        .await
        .unwrap();
    let mode_hover = hover_markdown_value(&extract_result(mode_hover));
    assert!(
        mode_hover.contains("```php\npublic const MODE\n```"),
        "class constant hover should use source-like declaration, got: {}",
        mode_hover
    );
    assert!(
        mode_hover
            .contains("**Symbol:** [`App\\Settings::MODE`](<file:///test/hover-constants.php#L8>)"),
        "class constant hover should expose linked FQN metadata, got: {}",
        mode_hover
    );
    assert!(
        mode_hover.contains(
            "**Source:** [`/test/hover-constants.php:8`](<file:///test/hover-constants.php#L8>)"
        ),
        "class constant hover should expose clickable source metadata, got: {}",
        mode_hover
    );

    let global_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(3, uri, global_position.0, global_position.1))
        .await
        .unwrap();
    let global_hover = hover_markdown_value(&extract_result(global_hover));
    assert!(
        global_hover.contains("```php\nconst GLOBAL_LIMIT\n```"),
        "global constant hover should use source-like declaration, got: {}",
        global_hover
    );
    assert!(
        global_hover
            .contains("**Symbol:** [`App\\GLOBAL_LIMIT`](<file:///test/hover-constants.php#L4>)"),
        "global constant hover should expose linked FQN metadata, got: {}",
        global_hover
    );
    assert!(
        global_hover.contains(
            "**Source:** [`/test/hover-constants.php:4`](<file:///test/hover-constants.php#L4>)"
        ),
        "global constant hover should expose clickable source metadata, got: {}",
        global_hover
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
async fn test_hover_class_relations_templates_and_generic_bindings() {
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
namespace Vendor\Repository {
class BaseRepository {}
}

namespace Vendor\Contracts {
interface Auditable {}
interface Traceable {}
}

namespace Vendor\Traits {
trait Timestamped {}
}

namespace Vendor {
class Builder {}
}

namespace App\Entity {
class User {}
}

namespace App\Repository {

use Vendor\Repository\BaseRepository;
use Vendor\Contracts\Auditable;
use Vendor\Contracts\Traceable;
use Vendor\Traits\Timestamped;
use Vendor\Builder;
use App\Entity\User;

/**
 * Repository docs.
 *
 * @template-covariant TEntity of User
 * @template TKey of int
 * @extends BaseRepository<TEntity>
 * @implements Auditable<User>
 * @use Timestamped<TEntity>
 * @mixin Builder<TEntity>
 */
class UserRepository extends BaseRepository implements Auditable, Traceable
{
    use Timestamped;
}

$repo = new UserRepository();
}
"#;
    let uri = "file:///test/hover-class-relations.php";
    let position = utf16_position_at(code, "UserRepository();");

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(2, uri, position.0, position.1))
        .await
        .unwrap();
    let hover = hover_markdown_value(&extract_result(hover));

    assert!(
        hover.contains("**Extends:**")
            && hover.contains("[`Vendor\\Repository\\BaseRepository`](<file:///test/hover-class-relations.php#L3>)&lt;`TEntity`&gt;"),
        "expected generic @extends relation with linked target, got: {}",
        hover
    );
    assert!(
        hover.contains("**Implements:**")
            && hover.contains("[`Vendor\\Contracts\\Auditable`](<file:///test/hover-class-relations.php#L7>)&lt;[`App\\Entity\\User`](<file:///test/hover-class-relations.php#L20>)&gt;")
            && hover.contains("[`Vendor\\Contracts\\Traceable`](<file:///test/hover-class-relations.php#L8>)"),
        "expected native and generic implements relations with links, got: {}",
        hover
    );
    assert_eq!(
        hover.matches("Vendor\\Contracts\\Auditable").count(),
        1,
        "native implements duplicate should be suppressed when @implements refines the same target, got: {}",
        hover
    );
    assert!(
        hover.contains("**Uses:**")
            && hover.contains("[`Vendor\\Traits\\Timestamped`](<file:///test/hover-class-relations.php#L12>)&lt;`TEntity`&gt;"),
        "expected generic @use relation with linked trait target, got: {}",
        hover
    );
    assert!(
        hover.contains("**Mixins:**")
            && hover.contains("[`Vendor\\Builder`](<file:///test/hover-class-relations.php#L16>)&lt;`TEntity`&gt;"),
        "expected generic @mixin relation with linked target, got: {}",
        hover
    );
    assert!(
        hover.contains("**Templates:**")
            && hover.contains(
                "- `covariant TEntity` of [`User`](<file:///test/hover-class-relations.php#L20>)"
            )
            && hover.contains("- `TKey` of `int`"),
        "expected template variance and bounds in hover, got: {}",
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
async fn test_hover_method_implements_and_overrides_links() {
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
namespace App\Contracts {
class Request {}
class Response {}
interface Handler
{
    public function handle(Request $request): Response;
}
}

namespace App\Service {
use App\Contracts\Handler;
use App\Contracts\Request;
use App\Contracts\Response;

class BaseHandler
{
    public function handle(Request $request): Response { return new Response(); }
}

final class ChildHandler extends BaseHandler implements Handler
{
    public function handle(Request $request): Response { return new Response(); }
}

$handler = new ChildHandler();
$handler->handle(new Request());
}
"#;
    let uri = "file:///test/hover-method-relations.php";
    let position = utf16_position_at(code, "handle(new Request");

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(2, uri, position.0, position.1))
        .await
        .unwrap();
    let hover = hover_markdown_value(&extract_result(hover));

    assert!(
        hover.contains("**Implements:**")
            && hover.contains(&format!("[`App\\Contracts\\Handler::handle`](<{}#L", uri)),
        "expected method-level implements link to interface declaration, got: {}",
        hover
    );
    assert!(
        hover.contains("**Overrides:**")
            && hover.contains(&format!("[`App\\Service\\BaseHandler::handle`](<{}#L", uri)),
        "expected method-level overrides link to parent method declaration, got: {}",
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
async fn test_hover_method_implements_vendor_interface_link() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-hover-method-relations-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp_root);
    let vendor_dir = tmp_root.join("vendor/doctrine/persistence/src");
    let src_dir = tmp_root.join("src");
    fs::create_dir_all(&vendor_dir).unwrap();
    fs::create_dir_all(&src_dir).unwrap();

    let vendor_path = vendor_dir.join("ObjectManager.php");
    let app_path = src_dir.join("EntityManager.php");
    let root_uri = php_lsp_types::uri::path_to_uri(&tmp_root).unwrap();
    let vendor_uri = php_lsp_types::uri::path_to_uri(&vendor_path).unwrap();
    let app_uri = php_lsp_types::uri::path_to_uri(&app_path).unwrap();

    let vendor_php = r#"<?php
namespace Doctrine\Persistence;

interface ObjectRepository {}

interface ObjectManager
{
    public function getRepository(string $className): ObjectRepository;
}
"#;
    let app_php = r#"<?php
namespace App;

use Doctrine\Persistence\ObjectManager;
use Doctrine\Persistence\ObjectRepository;

class User {}

final class EntityManager implements ObjectManager
{
    public function getRepository(string $className): ObjectRepository
    {
        throw new \RuntimeException();
    }
}

function run(EntityManager $em): void
{
    $em->getRepository(User::class);
}
"#;
    fs::write(&vendor_path, vendor_php).unwrap();
    fs::write(&app_path, app_php).unwrap();

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
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&vendor_uri, vendor_php))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&app_uri, app_php))
        .await
        .unwrap();

    let position = utf16_position_at(app_php, "getRepository(User");
    let hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(2, &app_uri, position.0, position.1))
        .await
        .unwrap();
    let hover = hover_markdown_value(&extract_result(hover));

    assert!(
        hover.contains("**Implements:**")
            && hover.contains(&format!(
                "[`Doctrine\\Persistence\\ObjectManager::getRepository`](<{}#L",
                vendor_uri
            )),
        "expected method-level implements link to vendor interface declaration, got: {}",
        hover
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
async fn test_hover_and_definition_on_vendor_trait_use_clause() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-trait-use-hover-definition-{}-{}",
        std::process::id(),
        nanos
    ));
    let app_dir = tmp_root.join("app/Domains/Contact/DavClient/Jobs");
    let bus_dir = tmp_root.join("vendor/illuminate/bus");
    let queue_dir = tmp_root.join("vendor/illuminate/queue");
    fs::create_dir_all(&app_dir).unwrap();
    fs::create_dir_all(&bus_dir).unwrap();
    fs::create_dir_all(&queue_dir).unwrap();

    fs::write(
        tmp_root.join("composer.json"),
        r#"{
  "autoload": {
    "psr-4": {
      "App\\": "app/",
      "Illuminate\\Bus\\": "vendor/illuminate/bus/",
      "Illuminate\\Queue\\": "vendor/illuminate/queue/"
    }
  }
}
"#,
    )
    .unwrap();

    let batchable_path = bus_dir.join("Batchable.php");
    let queueable_path = bus_dir.join("Queueable.php");
    let interacts_path = queue_dir.join("InteractsWithQueue.php");
    let serializes_path = queue_dir.join("SerializesModels.php");
    fs::write(
        &batchable_path,
        r#"<?php
namespace Illuminate\Bus;

trait Batchable
{
    public function batching(): bool { return false; }
}
"#,
    )
    .unwrap();
    fs::write(
        &queueable_path,
        r#"<?php
namespace Illuminate\Bus;

trait Queueable {}
"#,
    )
    .unwrap();
    fs::write(
        &interacts_path,
        r#"<?php
namespace Illuminate\Queue;

trait InteractsWithQueue {}
"#,
    )
    .unwrap();
    fs::write(
        &serializes_path,
        r#"<?php
namespace Illuminate\Queue;

trait SerializesModels {}
"#,
    )
    .unwrap();

    let app_path = app_dir.join("DeleteMultipleVCard.php");
    let app_php = r#"<?php
namespace App\Domains\Contact\DavClient\Jobs;

use Illuminate\Bus\Batchable;
use Illuminate\Bus\Queueable;
use Illuminate\Queue\InteractsWithQueue;
use Illuminate\Queue\SerializesModels;

class DeleteMultipleVCard
{
    use Batchable, InteractsWithQueue, Queueable, SerializesModels;
}
"#;
    fs::write(&app_path, app_php).unwrap();

    let root_uri = php_lsp_types::uri::path_to_uri(&tmp_root).unwrap();
    let app_uri = php_lsp_types::uri::path_to_uri(&app_path).unwrap();
    let batchable_uri = php_lsp_types::uri::path_to_uri(&batchable_path).unwrap();

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
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&app_uri, app_php))
        .await
        .unwrap();

    let trait_position = utf16_position_at(app_php, "Batchable,");
    let hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(
            2,
            &app_uri,
            trait_position.0,
            trait_position.1,
        ))
        .await
        .unwrap();
    let hover = hover_markdown_value(&extract_result(hover));
    assert!(
        hover.contains("trait Batchable")
            && hover.contains("Illuminate\\Bus\\Batchable")
            && hover.contains(&batchable_uri),
        "hover on class trait-use should resolve vendor trait source, got: {}",
        hover
    );

    let definition = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(
            3,
            &app_uri,
            trait_position.0,
            trait_position.1,
        ))
        .await
        .unwrap();
    let definition = extract_result(definition);
    let target_uri = definition
        .get("uri")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    assert_eq!(
        target_uri, batchable_uri,
        "definition on class trait-use should point to Batchable trait"
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
async fn test_hover_callsite_generic_resolved_returns() {
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

class Widget {}

class ServiceLocator
{
    /**
     * @template T of object
     * @param class-string<T> $class
     * @return T
     */
    public function make($class) {}

    /**
     * @template T of object
     * @param class-string<T>|string $class
     * @return ($class is class-string<T> ? T : object)
     */
    public function conditional($class) {}
}

function run(ServiceLocator $locator): void
{
    $locator->make(Widget::class);
    $locator->conditional(Widget::class);
}
"#;
    let uri = "file:///test/hover-callsite-generic.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let make_position = utf16_position_at(code, "make(Widget");
    let make_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(2, uri, make_position.0, make_position.1))
        .await
        .unwrap();
    let make_hover = hover_markdown_value(&extract_result(make_hover));

    assert!(
        make_hover.contains("**Resolved returns:**")
            && make_hover.contains("[`App\\Widget`](<file:///test/hover-callsite-generic.php#L4>)"),
        "expected generic class-string call hover to show concrete Widget return, got: {}",
        make_hover
    );

    let conditional_position = utf16_position_at(code, "conditional(Widget");
    let conditional_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(
            3,
            uri,
            conditional_position.0,
            conditional_position.1,
        ))
        .await
        .unwrap();
    let conditional_hover = hover_markdown_value(&extract_result(conditional_hover));

    assert!(
        conditional_hover.contains("**Resolved returns:**")
            && conditional_hover
                .contains("[`App\\Widget`](<file:///test/hover-callsite-generic.php#L4>)"),
        "expected conditional generic call hover to show concrete Widget return, got: {}",
        conditional_hover
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
async fn test_hover_callsite_doctrine_repository_resolved_returns() {
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
namespace Doctrine\ORM {
/**
 * @template TEntity of object
 */
class EntityRepository
{
    /**
     * @return TEntity|null
     */
    public function find($id): ?object {}

    /**
     * @return TEntity|null
     */
    public function findOneBy(array $criteria): ?object {}

    /**
     * @return list<TEntity>
     */
    public function findBy(array $criteria): array {}
}
}

namespace Doctrine\Persistence {
use Doctrine\ORM\EntityRepository;

interface ObjectManager
{
    /**
     * @template T of object
     * @param class-string<T> $className
     * @return EntityRepository<T>
     */
    public function getRepository(string $className): EntityRepository;
}
}

namespace App\Entity {
class RequestStatus {}
}

namespace App {
use App\Entity\RequestStatus;
use Doctrine\Persistence\ObjectManager;

function run(ObjectManager $em): void
{
    $em->getRepository(RequestStatus::class)->find(123);
    $em->getRepository(RequestStatus::class)->findOneBy(['name' => 'completed']);
    $em->getRepository(RequestStatus::class)->findBy(['name' => 'completed']);
}
}
"#;
    let uri = "file:///test/hover-callsite-doctrine.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let repository_position = utf16_position_at(code, "getRepository(RequestStatus");
    let repository_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(
            2,
            uri,
            repository_position.0,
            repository_position.1,
        ))
        .await
        .unwrap();
    let repository_hover = hover_markdown_value(&extract_result(repository_hover));

    assert!(
        repository_hover.contains("**Resolved returns:**")
            && repository_hover.contains("EntityRepository<App\\Entity\\RequestStatus>")
            && repository_hover.contains(
                "[`App\\Entity\\RequestStatus`](<file:///test/hover-callsite-doctrine.php#L"
            ),
        "expected getRepository hover to show concrete EntityRepository<RequestStatus>, got: {}",
        repository_hover
    );

    let find_position = utf16_position_at(code, "find(123");
    let find_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(3, uri, find_position.0, find_position.1))
        .await
        .unwrap();
    let find_hover = hover_markdown_value(&extract_result(find_hover));

    assert!(
        find_hover.contains("**Resolved returns:**")
            && find_hover.contains(
                "[`App\\Entity\\RequestStatus`](<file:///test/hover-callsite-doctrine.php#L"
            ),
        "expected repository find hover to show concrete RequestStatus return, got: {}",
        find_hover
    );

    let find_position = utf16_position_at(code, "findOneBy(['name'");
    let find_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(4, uri, find_position.0, find_position.1))
        .await
        .unwrap();
    let find_hover = hover_markdown_value(&extract_result(find_hover));

    assert!(
        find_hover.contains("**Resolved returns:**")
            && find_hover.contains(
                "[`App\\Entity\\RequestStatus`](<file:///test/hover-callsite-doctrine.php#L"
            ),
        "expected repository findOneBy hover to show concrete RequestStatus return, got: {}",
        find_hover
    );

    let find_by_position = utf16_position_at(code, "findBy(['name'");
    let find_by_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(
            5,
            uri,
            find_by_position.0,
            find_by_position.1,
        ))
        .await
        .unwrap();
    let find_by_hover = hover_markdown_value(&extract_result(find_by_hover));

    assert!(
        find_by_hover.contains("**Resolved returns:**")
            && find_by_hover.contains("list<App\\Entity\\RequestStatus>")
            && find_by_hover.contains(
                "[`App\\Entity\\RequestStatus`](<file:///test/hover-callsite-doctrine.php#L"
            ),
        "expected repository findBy hover to show concrete list<RequestStatus> return, got: {}",
        find_by_hover
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
async fn test_hover_doctrine_repository_extends_generic_binding() {
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
namespace Doctrine\Bundle\DoctrineBundle\Repository {
/**
 * @template TEntity of object
 */
class ServiceEntityRepository {}
}

namespace App\Entity {
class DataRequest {}
}

namespace App\Repository {

use App\Entity\DataRequest;
use Doctrine\Bundle\DoctrineBundle\Repository\ServiceEntityRepository;

/**
 * @extends ServiceEntityRepository<DataRequest>
 */
class DataRequestRepository extends ServiceEntityRepository {}

$repo = new DataRequestRepository();
}
"#;
    let uri = "file:///test/hover-doctrine-repository-generic.php";
    let position = utf16_position_at(code, "DataRequestRepository();");

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(2, uri, position.0, position.1))
        .await
        .unwrap();
    let hover = hover_markdown_value(&extract_result(hover));

    assert!(
        hover.contains("**Extends:**")
            && hover.contains("[`Doctrine\\Bundle\\DoctrineBundle\\Repository\\ServiceEntityRepository`](<file:///test/hover-doctrine-repository-generic.php#L6>)&lt;[`App\\Entity\\DataRequest`](<file:///test/hover-doctrine-repository-generic.php#L10>)&gt;"),
        "expected Doctrine repository generic @extends binding with entity link, got: {}",
        hover
    );
    assert_eq!(
        hover
            .matches("Doctrine\\Bundle\\DoctrineBundle\\Repository\\ServiceEntityRepository")
            .count(),
        1,
        "native extends duplicate should be suppressed when @extends refines the same target, got: {}",
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
async fn test_hover_symfony_controller_roles_and_route_attributes() {
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
namespace Symfony\Bundle\FrameworkBundle\Controller {
abstract class AbstractController {}
}

namespace Symfony\Component\Routing\Attribute {
class Route {}
}

namespace App\Controller {

use Symfony\Bundle\FrameworkBundle\Controller\AbstractController;
use Symfony\Component\Routing\Attribute\Route as SymfonyRoute;

#[SymfonyRoute('/data-request')]
final class DataRequestController extends AbstractController
{
    #[SymfonyRoute('/{id<\d+>}', name: 'app_data_request_show', methods: ['GET'])]
    public function show(): void {}
}

$controller = new DataRequestController();
$controller->show();
}
"#;
    let uri = "file:///test/hover-symfony-controller-attributes.php";
    let class_position = utf16_position_at(code, "DataRequestController();");
    let method_position = utf16_position_at(code, "show();");

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let class_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(2, uri, class_position.0, class_position.1))
        .await
        .unwrap();
    let class_hover = hover_markdown_value(&extract_result(class_hover));

    assert!(
        class_hover.contains(
            "```php\n#[SymfonyRoute('/data-request')]\nfinal class DataRequestController\n```"
        ),
        "expected route attribute above controller declaration, got: {}",
        class_hover
    );
    assert!(
        class_hover.contains("**Framework:** `Controller`"),
        "expected controller framework role, got: {}",
        class_hover
    );
    assert!(
        class_hover.contains("**Attributes:**")
            && class_hover.contains("`#[SymfonyRoute('/data-request')]`"),
        "expected route attribute metadata section, got: {}",
        class_hover
    );

    let method_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(3, uri, method_position.0, method_position.1))
        .await
        .unwrap();
    let method_hover = hover_markdown_value(&extract_result(method_hover));

    assert!(
        method_hover.contains("```php\n#[SymfonyRoute('/{id<\\d+>}', name: 'app_data_request_show', methods: ['GET'])]\npublic function show(): void\n```"),
        "expected route attribute above controller action declaration, got: {}",
        method_hover
    );
    assert!(
        method_hover.contains("**Framework:** `Controller action`"),
        "expected controller action role, got: {}",
        method_hover
    );
    assert!(
        method_hover.contains(
            "`#[SymfonyRoute('/{id<\\d+>}', name: 'app_data_request_show', methods: ['GET'])]`"
        ),
        "expected action route attribute metadata, got: {}",
        method_hover
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
async fn test_hover_doctrine_entity_roles_repository_and_property_attributes() {
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
namespace Doctrine\ORM\Mapping {
class Entity {}
class Table {}
class Index {}
class Id {}
class Column {}
class ManyToOne {}
}

namespace App\Repository {
class DataRequestRepository {}
}

namespace App\Entity {

use App\Repository\DataRequestRepository;
use Doctrine\ORM\Mapping as ORM;

#[ORM\Entity(repositoryClass: DataRequestRepository::class)]
#[ORM\Table(name: 'data_requests')]
#[ORM\Index(name: 'idx_data_requests_status', columns: ['status'])]
class DataRequest
{
    #[ORM\Id]
    #[ORM\Column(type: 'integer')]
    private int $id;

    #[ORM\ManyToOne(targetEntity: Subscriber::class)]
    private ?Subscriber $subscriber = null;
}

class Subscriber {}

$request = new DataRequest();
$request->subscriber;
}
"#;
    let uri = "file:///test/hover-doctrine-entity-attributes.php";
    let class_position = utf16_position_at(code, "DataRequest();");
    let property_position = utf16_position_at(code, "subscriber;");

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let class_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(2, uri, class_position.0, class_position.1))
        .await
        .unwrap();
    let class_hover = hover_markdown_value(&extract_result(class_hover));

    assert!(
        class_hover.contains("```php\n#[ORM\\Entity(repositoryClass: DataRequestRepository::class)]\n#[ORM\\Table(name: 'data_requests')]\n#[ORM\\Index(name: 'idx_data_requests_status', columns: ['status'])]\nclass DataRequest\n```"),
        "expected Doctrine attributes above entity declaration, got: {}",
        class_hover
    );
    assert!(
        class_hover.contains("**Framework:** `Entity`"),
        "expected entity framework role, got: {}",
        class_hover
    );
    assert!(
        class_hover.contains("**Repository:**")
            && class_hover.contains(
                "DataRequestRepository`](<file:///test/hover-doctrine-entity-attributes.php#L12>)"
            ),
        "expected linked repositoryClass metadata, got: {}",
        class_hover
    );
    assert!(
        class_hover.contains("**Attributes:**")
            && class_hover
                .contains("`#[ORM\\Entity(repositoryClass: DataRequestRepository::class)]`")
            && class_hover.contains("`#[ORM\\Table(name: 'data_requests')]`")
            && class_hover
                .contains("`#[ORM\\Index(name: 'idx_data_requests_status', columns: ['status'])]`"),
        "expected Doctrine entity attribute metadata, got: {}",
        class_hover
    );

    let property_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(
            3,
            uri,
            property_position.0,
            property_position.1,
        ))
        .await
        .unwrap();
    let property_hover = hover_markdown_value(&extract_result(property_hover));

    assert!(
        property_hover.contains("```php\n#[ORM\\ManyToOne(targetEntity: Subscriber::class)]\nprivate ?Subscriber $subscriber\n```"),
        "expected Doctrine association attribute above property declaration, got: {}",
        property_hover
    );
    assert!(
        property_hover.contains("**Framework:** `Doctrine association`"),
        "expected Doctrine association framework role, got: {}",
        property_hover
    );
    assert!(
        property_hover.contains("`#[ORM\\ManyToOne(targetEntity: Subscriber::class)]`"),
        "expected Doctrine association attribute metadata, got: {}",
        property_hover
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
async fn test_hover_framework_roles_avoid_attribute_argument_and_basename_false_positives() {
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
namespace App\Attribute {
class Route {}
}

namespace App {

class AbstractController {}
class EntityRepository {}
class Custom {}

final class LocalController extends AbstractController
{
    #[Custom(label: 'Route')]
    public function show(): void {}

    #[\App\Attribute\Route]
    public function localRoute(): void {}
}

final class LocalRepository extends EntityRepository {}

$controller = new LocalController();
$controller->show();
$controller->localRoute();
$repository = new LocalRepository();
}
"#;
    let uri = "file:///test/hover-framework-role-false-positives.php";
    let controller_position = utf16_position_at(code, "LocalController();");
    let method_position = utf16_position_at(code, "show();");
    let local_route_position = utf16_position_at(code, "localRoute();");
    let repository_position = utf16_position_at(code, "LocalRepository();");

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let controller_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(
            2,
            uri,
            controller_position.0,
            controller_position.1,
        ))
        .await
        .unwrap();
    let controller_hover = hover_markdown_value(&extract_result(controller_hover));
    assert!(
        !controller_hover.contains("**Framework:**"),
        "local AbstractController basename must not infer Symfony controller role, got: {}",
        controller_hover
    );

    let method_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(3, uri, method_position.0, method_position.1))
        .await
        .unwrap();
    let method_hover = hover_markdown_value(&extract_result(method_hover));
    assert!(
        method_hover.contains("`#[Custom(label: 'Route')]`")
            && !method_hover.contains("Controller action"),
        "attribute argument text must not infer route/controller action role, got: {}",
        method_hover
    );

    let local_route_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(
            4,
            uri,
            local_route_position.0,
            local_route_position.1,
        ))
        .await
        .unwrap();
    let local_route_hover = hover_markdown_value(&extract_result(local_route_hover));
    assert!(
        local_route_hover.contains("`#[\\App\\Attribute\\Route]`")
            && !local_route_hover.contains("Controller action"),
        "local Route attribute FQN must not infer Symfony controller action role, got: {}",
        local_route_hover
    );

    let repository_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(
            5,
            uri,
            repository_position.0,
            repository_position.1,
        ))
        .await
        .unwrap();
    let repository_hover = hover_markdown_value(&extract_result(repository_hover));
    assert!(
        !repository_hover.contains("**Framework:**"),
        "local EntityRepository basename must not infer Doctrine repository role, got: {}",
        repository_hover
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
async fn test_hover_framework_roles_use_nearest_namespace_import_for_attributes() {
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
namespace Symfony\Bundle\FrameworkBundle\Controller {
abstract class AbstractController {}
}

namespace Symfony\Component\Routing\Attribute {
class Route {}
}

namespace App\Attribute {
class Route {}
}

namespace App\Controller {
use Symfony\Bundle\FrameworkBundle\Controller\AbstractController;
use Symfony\Component\Routing\Attribute\Route;

final class WebController extends AbstractController
{
    #[Route('/web')]
    public function webShow(): void {}
}

$web = new WebController();
$web->webShow();
}

namespace App\Api {
use App\Attribute\Route;

#[Route]
final class LocalAction
{
    public function localShow(): void {}
}

$api = new LocalAction();
$api->localShow();
}
"#;
    let uri = "file:///test/hover-framework-role-scoped-imports.php";
    let local_position = utf16_position_at(code, "LocalAction();");

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let local_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(2, uri, local_position.0, local_position.1))
        .await
        .unwrap();
    let local_hover = hover_markdown_value(&extract_result(local_hover));
    assert!(
        local_hover.contains("`#[Route]`") && !local_hover.contains("Controller action"),
        "local Route import after an earlier Symfony Route import must not infer Symfony role, got: {}",
        local_hover
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
async fn test_hover_framework_roles_do_not_leak_imports_into_later_namespace() {
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
namespace Symfony\Component\Routing\Attribute {
class Route {}
}

namespace App\Controller {
use Symfony\Component\Routing\Attribute\Route;

final class WebController
{
    #[Route('/web')]
    public function webShow(): void {}
}
}

namespace App\NoLocalImport {

#[Route]
final class NoLocalImportAction
{
    public function noLocalShow(): void {}
}

$noLocal = new NoLocalImportAction();
$noLocal->noLocalShow();
}
"#;
    let uri = "file:///test/hover-framework-role-import-leak.php";
    let no_local_position = utf16_position_at(code, "NoLocalImportAction();");

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let no_local_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(
            2,
            uri,
            no_local_position.0,
            no_local_position.1,
        ))
        .await
        .unwrap();
    let no_local_hover = hover_markdown_value(&extract_result(no_local_hover));
    assert!(
        no_local_hover.contains("`#[Route]`") && !no_local_hover.contains("Controller action"),
        "an unimported Route in a later namespace must not reuse an earlier Symfony import, got: {}",
        no_local_hover
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
async fn test_hover_parameters_section_includes_all_signature_params() {
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

class DataRequest {}
class Response {}

class Service {
    /**
     * Configure request handling.
     *
     * @param DataRequest $request Current request entity
     * @param array<string, mixed> $criteria Search criteria
     * @param mixed $payload Raw payload
     */
    public function configure(
        DataRequest $request,
        &$counter,
        $raw,
        array $criteria = [],
        mixed $payload = null,
        ?Response $response = null,
        string ...$tags
    ): Response {
        return new Response();
    }
}
"#;
    let uri = "file:///test/hover-rich-parameters.php";
    let configure_position = utf16_position_at(code, "configure(");

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(
            2,
            uri,
            configure_position.0,
            configure_position.1,
        ))
        .await
        .unwrap();
    let hover = hover_markdown_value(&extract_result(hover));

    assert!(
        hover.contains("```php\npublic function configure("),
        "expected PHP-highlighted multiline function signature, got: {}",
        hover
    );
    assert!(
        !hover.contains("public function App\\Service::configure("),
        "method FQN should stay outside the code block, got: {}",
        hover
    );
    assert!(
        hover.contains(
            "**Symbol:** [`App\\Service::configure`](<file:///test/hover-rich-parameters.php#L15>)"
        ),
        "expected linked FQN metadata in method hover, got: {}",
        hover
    );
    assert!(
        hover.contains(
            "**Source:** [`/test/hover-rich-parameters.php:15`](<file:///test/hover-rich-parameters.php#L15>)"
        ),
        "expected clickable source metadata in method hover, got: {}",
        hover
    );
    assert!(
        hover.contains("Configure request handling."),
        "expected PHPDoc summary in hover, got: {}",
        hover
    );
    assert!(
        hover.contains("**Parameters:**"),
        "expected parameter section, got: {}",
        hover
    );
    assert!(
        hover.contains(
            "- `DataRequest $request`: [`DataRequest`](<file:///test/hover-rich-parameters.php#L4>) — Current request entity"
        ),
        "expected class parameter with description, got: {}",
        hover
    );
    assert!(
        hover.contains("- `&$counter`: `untyped`"),
        "expected by-reference untyped parameter, got: {}",
        hover
    );
    assert!(
        hover.contains("- `$raw`: `untyped`"),
        "expected plain untyped parameter, got: {}",
        hover
    );
    assert!(
        hover.contains(
            "- `array<string, mixed> $criteria = []`: `array<string, mixed>` — Search criteria"
        ),
        "expected PHPDoc-refined array parameter with description, got: {}",
        hover
    );
    assert!(
        hover.contains("- `mixed $payload = null`: `mixed` — Raw payload"),
        "expected mixed parameter with description, got: {}",
        hover
    );
    assert!(
        hover.contains(
            "- `?Response $response = null`: ?[`Response`](<file:///test/hover-rich-parameters.php#L5>)"
        ),
        "expected nullable class parameter link, got: {}",
        hover
    );
    assert!(
        hover.contains("- `string ...$tags`: `string`"),
        "expected variadic scalar parameter, got: {}",
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
async fn test_hover_phpdoc_and_virtual_member_types_include_class_links() {
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

class User {}

/**
 * @property User $owner Related owner
 * @method User find(User $fallback)
 */
class Supported {}

function demo(Supported $subject, User $fallback): void {
    $subject->owner;
    $subject->find($fallback);
}
"#;
    let uri = "file:///test/hover-phpdoc-class-links.php";
    let supported_position = utf16_position_at(code, "Supported {}\n\nfunction");
    let owner_position = utf16_position_at(code, "owner;");
    let find_position = utf16_position_at(code, "find($fallback");

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let class_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(
            2,
            uri,
            supported_position.0,
            supported_position.1,
        ))
        .await
        .unwrap();
    let class_hover = hover_markdown_value(&extract_result(class_hover));
    assert!(
        class_hover.contains("@property User $owner")
            && class_hover
                .contains("Type: [`User`](<file:///test/hover-phpdoc-class-links.php#L4>)")
            && class_hover.contains("@method User find(User $fallback)")
            && class_hover
                .contains("Returns: [`User`](<file:///test/hover-phpdoc-class-links.php#L4>)")
            && class_hover
                .contains("`$fallback`: [`User`](<file:///test/hover-phpdoc-class-links.php#L4>)"),
        "expected class PHPDoc hover links, got: {}",
        class_hover
    );

    let owner_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(3, uri, owner_position.0, owner_position.1))
        .await
        .unwrap();
    let owner_hover = hover_markdown_value(&extract_result(owner_hover));
    assert!(
        owner_hover.contains("@property User $owner")
            && owner_hover.contains(
                "**Type:** [`App\\User`](<file:///test/hover-phpdoc-class-links.php#L4>)"
            ),
        "expected virtual property type link, got: {}",
        owner_hover
    );

    let method_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(4, uri, find_position.0, find_position.1))
        .await
        .unwrap();
    let method_hover = hover_markdown_value(&extract_result(method_hover));
    assert!(
        method_hover.contains("@method User find(User $fallback)")
            && method_hover.contains(
                "**Returns:** [`App\\User`](<file:///test/hover-phpdoc-class-links.php#L4>)"
            )
            && method_hover.contains(
                "- `User $fallback`: [`User`](<file:///test/hover-phpdoc-class-links.php#L4>)"
            ),
        "expected virtual method type links, got: {}",
        method_hover
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
async fn test_inline_phpdoc_var_overrides_weak_assignment_inference_for_hover_and_inlay() {
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

class User {}
class UserRepository {}

function maybeObject(): object|null { return null; }
function repo(): object { return new \stdClass(); }

function run(): void {
    /** @var User|null $user */
    $user = maybeObject();
    $user;
    /** @var UserRepository $repo */
    $repo = repo();
    $repo;
}
"#;
    let uri = "file:///test/inline-phpdoc-overrides-weak-rhs.php";

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
        labels
            .iter()
            .any(|label| label.contains("User") && label.contains("null")),
        "expected inline @var nullable User inlay to override object|null, got: {:?}",
        labels
    );
    assert!(
        labels.iter().any(|label| label == ": UserRepository"),
        "expected inline @var UserRepository inlay to override object, got: {:?}",
        labels
    );
    assert!(
        !labels
            .iter()
            .any(|label| label == ": object|null" || label == ": object"),
        "weak object RHS inference should not win over inline @var, got: {:?}",
        labels
    );

    let user_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(3, uri, 11, 5))
        .await
        .unwrap();
    let user_result = extract_result(user_hover);
    let user_contents = user_result
        .get("contents")
        .and_then(|contents| contents.get("value"))
        .and_then(|value| value.as_str())
        .unwrap_or("");
    assert!(
        user_contents.contains("$user")
            && user_contents.contains("User")
            && user_contents.contains("null"),
        "expected declaration hover to use inline @var nullable User, got: {}",
        user_contents
    );

    let repo_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(4, uri, 14, 5))
        .await
        .unwrap();
    let repo_result = extract_result(repo_hover);
    let repo_contents = repo_result
        .get("contents")
        .and_then(|contents| contents.get("value"))
        .and_then(|value| value.as_str())
        .unwrap_or("");
    assert!(
        repo_contents.contains("$repo") && repo_contents.contains("UserRepository"),
        "expected declaration hover to use inline @var UserRepository, got: {}",
        repo_contents
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
async fn test_inline_phpdoc_var_hover_on_multiline_ternary_assignment_declaration() {
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

class RegionChangeRequest {}

function maybeRegion(): ?RegionChangeRequest { return null; }

function run(bool $enabled): void {
    /** @var RegionChangeRequest|null $regionChangeRequest */
    $regionChangeRequest = $enabled
        ? maybeRegion()
        : null;
}
"#;
    let uri = "file:///test/inline-phpdoc-ternary-declaration-hover.php";

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
        .call(inlay_hint_request(2, uri, 0, 0, 13, 0))
        .await
        .unwrap();
    let result = extract_result(response);
    let hints = result.as_array().expect("expected inlay hint array");
    let labels: Vec<String> = hints.iter().filter_map(inlay_hint_label_text).collect();
    assert!(
        labels
            .iter()
            .any(|label| label.contains("RegionChangeRequest") && label.contains("null")),
        "expected inline @var ternary inlay, got: {:?}",
        labels
    );

    let hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(3, uri, 9, 5))
        .await
        .unwrap();
    let result = extract_result(hover);
    let contents = result
        .get("contents")
        .and_then(|contents| contents.get("value"))
        .and_then(|value| value.as_str())
        .unwrap_or("");
    assert!(
        contents.contains("$regionChangeRequest")
            && contents.contains("RegionChangeRequest")
            && contents.contains("null"),
        "expected declaration hover to use inline @var on multiline ternary, got: {}",
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
async fn test_local_variable_hover_for_array_shape_and_scalar_ternary_assignment() {
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

    let code_with_markers = r#"<?php
namespace App;

class Request {}

function update(string $npId, Request $request, bool $donor): void {
    $con/*context*/text = [
        'npId' => $npId,
        'request' => $request,
        'status' => 'transfered',
    ];

    $tra/*transition*/nsition = $donor
        ? 'donor_receive_complete'
        : 'recipient_receive_complete';
}
"#;
    let markers = ["/*context*/", "/*transition*/"];
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
    let (context_line, context_character) = marker_position("/*context*/");
    let (transition_line, transition_character) = marker_position("/*transition*/");
    let mut code = code_with_markers.to_string();
    for marker in markers {
        code = code.replace(marker, "");
    }
    let uri = "file:///test/local-array-shape-and-ternary-hover.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, &code))
        .await
        .unwrap();

    let inlay_response = service
        .ready()
        .await
        .unwrap()
        .call(inlay_hint_request(2, uri, 0, 0, 17, 0))
        .await
        .unwrap();
    let inlay_result = extract_result(inlay_response);
    let hints = inlay_result.as_array().expect("expected inlay hint array");
    let labels: Vec<String> = hints.iter().filter_map(inlay_hint_label_text).collect();
    assert!(
        labels.iter().any(|label| label.contains("array{")),
        "expected array-shape inlay, got: {:?}",
        labels
    );
    assert!(
        labels.iter().any(|label| label == ": string"),
        "expected scalar ternary string inlay, got: {:?}",
        labels
    );

    let context_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(3, uri, context_line, context_character))
        .await
        .unwrap();
    let context_result = extract_result(context_hover);
    let context_text = hover_markdown_value(&context_result);
    assert!(
        context_text.contains("array{") && context_text.contains("$context"),
        "expected array-shape local variable hover, got: {}",
        context_text
    );

    let transition_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(4, uri, transition_line, transition_character))
        .await
        .unwrap();
    let transition_result = extract_result(transition_hover);
    let transition_text = hover_markdown_value(&transition_result);
    assert!(
        transition_text.contains("string $transition"),
        "expected scalar ternary local variable hover, got: {}",
        transition_text
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
async fn test_hover_expands_multiline_phpdoc_shape_alias_for_local_var() {
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

    let code_with_markers = r#"<?php
function run(): void {
    /** @var RowAlias $row */
    $ro/*row*/w = [];
}

/**
 * @phpstan-type RowAlias array{
 *   'alias-key': string,
 *   nested: array{
 *     leaf: int,
 *   },
 * }
 */
"#;
    let marker_offset = code_with_markers
        .find("/*row*/")
        .expect("test code should contain row marker");
    let prefix = code_with_markers[..marker_offset].to_string();
    let row_line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
    let row_start = prefix.rfind('\n').map(|idx| idx + 1).unwrap_or(0);
    let row_character = (prefix.len() - row_start) as u32;
    let code = code_with_markers.replace("/*row*/", "");
    let uri = "file:///test/multiline-shape-alias-hover.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, &code))
        .await
        .unwrap();

    let hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(2, uri, row_line, row_character))
        .await
        .unwrap();
    let hover_result = extract_result(hover);
    let hover_text = hover_markdown_value(&hover_result);
    assert!(
        hover_text.contains("array{'alias-key': string")
            && hover_text.contains("nested: array{leaf: int}")
            && hover_text.contains("$row"),
        "expected multiline type alias to expand in hover, got: {}",
        hover_text
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
async fn test_hover_local_variable_method_return_types_and_links() {
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

    let code_with_markers = r#"<?php
namespace App;

class PortingProcess {}

class PortingRequest {
    public function getPortingProcess(): ?PortingProcess { return new PortingProcess(); }
}

class DonorProcess {
    public function getCurrentPlace(): ?string { return 'si'; }
}

abstract class SoapHandler {
    protected function ensureProcessCreated(): ?PortingProcess { return new PortingProcess(); }
}

abstract class BaseHandler extends SoapHandler {
    protected function updatePortingProcess(): bool { return true; }
}

class CdbHandler extends BaseHandler {
    public function handle(PortingRequest $portingRequest): void {
        $recipient/*recipient*/Process = $this->ensureProcessCreated();
        $recipientProcess/*updated*/Updated = $this->updatePortingProcess();
    }
}
"#;
    let markers = ["/*recipient*/", "/*updated*/"];
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
    let (recipient_line, recipient_character) = marker_position("/*recipient*/");
    let (updated_line, updated_character) = marker_position("/*updated*/");
    let mut code = code_with_markers.to_string();
    for marker in markers {
        code = code.replace(marker, "");
    }
    let uri = "file:///test/hover-local-method-return-types.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, &code))
        .await
        .unwrap();

    let recipient_resp = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(2, uri, recipient_line, recipient_character))
        .await
        .unwrap();
    let recipient_result = extract_result(recipient_resp);
    let recipient_hover = hover_markdown_value(&recipient_result);
    assert!(
        recipient_hover.contains("?PortingProcess $recipientProcess"),
        "expected nullable PortingProcess variable hover, got: {}",
        recipient_hover
    );
    assert!(
        recipient_hover
            .contains("?[`PortingProcess`](<file:///test/hover-local-method-return-types.php#L4>)"),
        "expected clickable PortingProcess type link, got: {}",
        recipient_hover
    );

    let updated_resp = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(3, uri, updated_line, updated_character))
        .await
        .unwrap();
    let updated_result = extract_result(updated_resp);
    let updated_hover = hover_markdown_value(&updated_result);
    assert!(
        updated_hover.contains("bool $recipientProcessUpdated"),
        "expected bool variable hover from method return type, got: {}",
        updated_hover
    );
    assert!(
        updated_hover.contains("**Type:** `bool`"),
        "expected bool type section, got: {}",
        updated_hover
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
async fn test_inlay_hints_for_parameters_and_phpdoc_types() {
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

/**
 * @param string $name
 * @param int $count
 * @return string
 */
function label($name, $count) { return $name; }

class Formatter {
    /**
     * @param string $prefix
     * @return string
     */
    public function format($prefix) { return $prefix; }
}

function run(Formatter $formatter): void {
    label("Ada", 2);
    $formatter->format("Hi");
}
"#;
    let uri = "file:///test/inlay-hints.php";

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
        .call(inlay_hint_request(2, uri, 0, 0, 22, 0))
        .await
        .unwrap();
    let result = extract_result(response);
    let hints = result.as_array().expect("expected inlay hint array");
    let labels: Vec<&str> = hints
        .iter()
        .filter_map(|hint| hint.get("label").and_then(|label| label.as_str()))
        .collect();

    for expected in ["name:", "count:", "prefix:", ": string", ": int"] {
        assert!(
            labels.contains(&expected),
            "expected `{}` in inlay hint labels, got: {:?}",
            expected,
            labels
        );
    }
    assert!(
        hints
            .iter()
            .any(|hint| hint.get("kind").and_then(|kind| kind.as_u64()) == Some(2)),
        "expected parameter hint kind, got: {}",
        result
    );
    assert!(
        hints
            .iter()
            .any(|hint| hint.get("kind").and_then(|kind| kind.as_u64()) == Some(1)),
        "expected type hint kind, got: {}",
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

#[tokio::test(flavor = "current_thread")]
async fn test_inlay_hints_for_methods_and_large_scopes() {
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

class ScopeDemo
{
    public function process(array $items): void
    {
        if (count($items) > 0) {
            $first = 1;
            $second = 2;
            $third = 3;
            $fourth = 4;
            $fifth = 5;
            $sixth = 6;
            $seventh = 7;
            $eighth = 8;
        } else {
            $fallbackFirst = 1;
            $fallbackSecond = 2;
            $fallbackThird = 3;
            $fallbackFourth = 4;
            $fallbackFifth = 5;
            $fallbackSixth = 6;
            $fallbackSeventh = 7;
            $fallbackEighth = 8;
        }

        foreach ($this->veryLongIterableFactory($items, $this->fallbackItems(), $this->archivedItems(), $this->externalItems(), $this->finalItems()) as $key => $value) {
            $loopFirst = $key;
            $loopSecond = $value;
            $loopThird = $items;
            $loopFourth = $loopFirst;
            $loopFifth = $loopSecond;
            $loopSixth = $loopThird;
            $loopSeventh = $loopFourth;
            $loopEighth = $loopFifth;
        }

        try {
            $tryFirst = 1;
            $trySecond = 2;
            $tryThird = 3;
            $tryFourth = 4;
            $tryFifth = 5;
            $trySixth = 6;
            $trySeventh = 7;
            $tryEighth = 8;
        } catch (\Throwable $exception) {
            $catchFirst = 1;
            $catchSecond = 2;
            $catchThird = 3;
            $catchFourth = 4;
            $catchFifth = 5;
            $catchSixth = 6;
            $catchSeventh = 7;
            $catchEighth = 8;
        } finally {
            $finallyFirst = 1;
            $finallySecond = 2;
            $finallyThird = 3;
            $finallyFourth = 4;
            $finallyFifth = 5;
            $finallySixth = 6;
            $finallySeventh = 7;
            $finallyEighth = 8;
        }
    }
}

function helper(): void
{
    $value = 1;
    while ($value < 2) {
        $value++;
    }
}
"#;
    let uri = "file:///test/scope-end-inlay-hints.php";

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
        .call(inlay_hint_request(2, uri, 0, 0, 90, 0))
        .await
        .unwrap();
    let result = extract_result(response);
    let hints = result.as_array().expect("expected inlay hint array");
    let labels: Vec<String> = hints.iter().filter_map(inlay_hint_label_text).collect();

    for expected in [
        "ScopeDemo::process()",
        "class ScopeDemo",
        "function helper()",
        "if (count($items) > 0)",
    ] {
        assert!(
            labels.iter().any(|label| label == expected),
            "expected scope hint `{}` in labels, got: {:?}",
            expected,
            labels
        );
    }
    let foreach_label = labels
        .iter()
        .find(|label| label.starts_with("foreach ($this->veryLongIterableFactory("))
        .expect("expected truncated foreach header scope hint");
    assert!(
        foreach_label.contains("...") && foreach_label.ends_with("as $key => $value)"),
        "expected foreach hint to preserve header start and loop variables, got: {foreach_label}"
    );
    assert!(
        foreach_label.chars().count() <= 96,
        "foreach hint should be truncated to a compact label, got: {foreach_label}"
    );
    assert!(
        !labels
            .iter()
            .any(|label| matches!(label.as_str(), "else" | "try" | "catch" | "finally")),
        "expression-less and try/catch/finally blocks should not produce scope hints, got: {:?}",
        labels
    );
    assert!(
        !labels.iter().any(|label| label.starts_with("while")),
        "short while block should not produce noisy scope hint, got: {:?}",
        labels
    );
    assert!(
        hints.iter().any(|hint| {
            inlay_hint_label_text(hint).as_deref() == Some("ScopeDemo::process()")
                && hint.get("kind").is_none()
                && hint
                    .get("tooltip")
                    .and_then(|tooltip| tooltip.as_str())
                    .is_some_and(|tooltip| tooltip == "End of ScopeDemo::process()")
        }),
        "method scope hint should be an untyped end-of-scope hint with tooltip: {}",
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

#[tokio::test(flavor = "current_thread")]
async fn test_inlay_hints_for_local_variable_types() {
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

class User {}
class Widget {}

class Repository {
    public function find(): User { return new User(); }
}

class ServiceLocator {
    /**
     * @template T of object
     * @param class-string<T> $class
     * @return T
     */
    public function make($class) {}

    /**
     * @template T of object
     * @param class-string<T>|string $class
     * @return ($class is class-string<T> ? T : object)
     */
    public function conditional($class) {}
}

class PortingProcess {}

class PortingRequest {
    public function getPortingProcess(): ?PortingProcess { return new PortingProcess(); }
}

abstract class SoapHandler {
    protected function ensureProcessCreated(): ?PortingProcess { return new PortingProcess(); }
}

abstract class BaseHandler extends SoapHandler {
    protected function updatePortingProcess(): bool { return true; }
}

function run(Repository $repo): void {
    $created = new User();
    $found = $repo->find();
    /** @var array<int, User> $users */
    $users = [];
    foreach ($users as $item) {
        $copy = $item;
    }
    $count = 1;
}

function resolve(ServiceLocator $locator, string $unknownClass): void {
    $made = $locator->make(Widget::class);
    $conditional = $locator->conditional(Widget::class);
    /** @var class-string<Widget> $widgetClass */
    $widgetClass = Widget::class;
    $fromVariable = $locator->make($widgetClass);
    $fallback = $locator->conditional($unknownClass);
}

class CdbHandler extends BaseHandler {
    public function handle(PortingRequest $portingRequest, \stdClass $message, DonorProcess $donorProcess): void {
        $portingProcess = $portingRequest->getPortingProcess();
        $recipientProcess = $this->ensureProcessCreated();
        $recipientProcessUpdated = $this->updatePortingProcess();
        $requestId = (string)($message->NPRequestId ?? '');
        $currentPlace = (string)$donorProcess->getCurrentPlace();
    }
}
"#;
    let uri = "file:///test/local-variable-inlay-hints.php";

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
        .call(inlay_hint_request(2, uri, 0, 0, 68, 0))
        .await
        .unwrap();
    let result = extract_result(response);
    let hints = result.as_array().expect("expected inlay hint array");
    let labels: Vec<String> = hints.iter().filter_map(inlay_hint_label_text).collect();

    assert!(
        labels
            .iter()
            .filter(|label| label.as_str() == ": User")
            .count()
            >= 3,
        "expected User local variable type hints, got: {:?}",
        labels
    );
    assert!(
        labels.iter().any(|label| label == ": array<int, User>"),
        "expected PHPDoc generic local variable type hint, got: {:?}",
        labels
    );
    assert!(
        labels
            .iter()
            .filter(|label| label.as_str() == ": Widget")
            .count()
            >= 3,
        "expected class-string template and conditional return hints, got: {:?}",
        labels
    );
    assert!(
        labels.iter().any(|label| label == ": T|object"),
        "expected unresolved conditional return fallback union hint, got: {:?}",
        labels
    );
    assert!(
        labels.iter().any(|label| label == ": ?PortingProcess"),
        "expected nullable PortingProcess method-return hint, got: {:?}",
        labels
    );
    assert!(
        labels.iter().any(|label| label == ": bool"),
        "expected bool method-return hint, got: {:?}",
        labels
    );
    assert!(
        labels
            .iter()
            .filter(|label| label.as_str() == ": string")
            .count()
            >= 2,
        "expected explicit string cast local variable hints, got: {:?}",
        labels
    );
    assert!(
        hints.iter().any(|hint| {
            inlay_hint_label_text(hint).as_deref() == Some(": bool")
                && hint
                    .get("tooltip")
                    .and_then(|tooltip| tooltip.as_str())
                    .is_some_and(|tooltip| tooltip.contains("bool"))
        }),
        "expected bool local variable hint tooltip to include the inferred type: {}",
        result
    );
    assert!(
        hints.iter().any(|hint| {
            inlay_hint_label_text(hint).as_deref() == Some(": ?PortingProcess")
                && hint
                    .get("tooltip")
                    .and_then(|tooltip| tooltip.as_str())
                    .is_some_and(|tooltip| tooltip.contains("App\\PortingProcess"))
        }),
        "expected PortingProcess local variable hint tooltip to include the target FQN: {}",
        result
    );
    assert!(
        !labels.iter().any(|label| label == ": int"),
        "scalar assignment should not produce a noisy local variable hint: {:?}",
        labels
    );
    assert!(
        hints
            .iter()
            .any(|hint| inlay_hint_has_label_part_location(hint, "User")),
        "expected object local variable hint label to include a navigable User location: {}",
        result
    );
    assert!(
        hints
            .iter()
            .any(|hint| inlay_hint_has_label_part_location(hint, "PortingProcess")),
        "expected nullable PortingProcess hint label to include a navigable type location: {}",
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

#[tokio::test(flavor = "current_thread")]
async fn test_simplexml_stubs_drive_inlay_hints_and_hovers() {
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

    let code_with_markers = r#"<?php
namespace {
class SimpleXMLElement {
    /** @return static */
    private function __get($name) {}
    /**
     * @return static[]|false|null
     */
    public function xpath(string $expression): array|false|null { return []; }
    public function registerXPathNamespace(string $prefix, string $namespace): bool { return true; }
}

function simplexml_load_string(string $data): SimpleXMLElement|false { return new SimpleXMLElement(); }
}

namespace App;

function parse(string $responseXml): void {
    $x/*xml*/ml = simplexml_load_string($responseXml);
    if (false === $xml) {
        return;
    }
    $xml->registerXPath/*method*/Namespace('s', 'urn');
    $result/*nodes*/Nodes = $xml->xpath('//x');
    /** @var \SimpleXMLElement $result */
    $result = $resultNodes[0];
    $status = (string)($result->Status/*prop*/Code ?? '');
}
"#;
    let markers = ["/*xml*/", "/*method*/", "/*nodes*/", "/*prop*/"];
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
    let (xml_line, xml_character) = marker_position("/*xml*/");
    let (method_line, method_character) = marker_position("/*method*/");
    let (nodes_line, nodes_character) = marker_position("/*nodes*/");
    let (prop_line, prop_character) = marker_position("/*prop*/");
    let mut code = code_with_markers.to_string();
    for marker in markers {
        code = code.replace(marker, "");
    }
    let uri = "file:///test/simplexml-inlay-hover.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, &code))
        .await
        .unwrap();

    let inlay_response = service
        .ready()
        .await
        .unwrap()
        .call(inlay_hint_request(2, uri, 0, 0, 30, 0))
        .await
        .unwrap();
    let inlay_result = extract_result(inlay_response);
    let hints = inlay_result.as_array().expect("expected inlay hint array");
    let labels: Vec<String> = hints.iter().filter_map(inlay_hint_label_text).collect();
    assert!(
        labels
            .iter()
            .any(|label| label == ": SimpleXMLElement|false"),
        "expected simplexml_load_string return type hint, got: {:?}",
        labels
    );
    assert!(
        labels
            .iter()
            .any(|label| label == ": array<SimpleXMLElement>|false|null"),
        "expected xpath PHPDoc generic return type hint, got: {:?}",
        labels
    );

    let xml_hover = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(hover_request(3, uri, xml_line, xml_character))
            .await
            .unwrap(),
    );
    let xml_hover = hover_markdown_value(&xml_hover);
    assert!(
        xml_hover.contains("SimpleXMLElement|false $xml"),
        "expected local variable hover from global function fallback, got: {}",
        xml_hover
    );

    let method_hover = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(hover_request(4, uri, method_line, method_character))
            .await
            .unwrap(),
    );
    let method_hover = hover_markdown_value(&method_hover);
    assert!(
        method_hover.contains("```php\npublic function registerXPathNamespace(")
            && !method_hover.contains("public function SimpleXMLElement::registerXPathNamespace"),
        "expected method hover on SimpleXMLElement receiver, got: {}",
        method_hover
    );
    assert!(
        method_hover.contains(
            "**Symbol:** [`SimpleXMLElement::registerXPathNamespace`](<file:///test/simplexml-inlay-hover.php#L10>)"
        ),
        "expected linked FQN metadata for SimpleXMLElement method hover, got: {}",
        method_hover
    );
    assert!(
        method_hover.contains(
            "**Source:** [`/test/simplexml-inlay-hover.php:10`](<file:///test/simplexml-inlay-hover.php#L10>)"
        ),
        "expected clickable source metadata for SimpleXMLElement method hover, got: {}",
        method_hover
    );

    let nodes_hover = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(hover_request(5, uri, nodes_line, nodes_character))
            .await
            .unwrap(),
    );
    let nodes_hover = hover_markdown_value(&nodes_hover);
    assert!(
        nodes_hover.contains("array<SimpleXMLElement>|false|null $resultNodes"),
        "expected local variable hover from xpath PHPDoc return type, got: {}",
        nodes_hover
    );

    let prop_hover = extract_result(
        service
            .ready()
            .await
            .unwrap()
            .call(hover_request(6, uri, prop_line, prop_character))
            .await
            .unwrap(),
    );
    let prop_hover = hover_markdown_value(&prop_hover);
    assert!(
        prop_hover.contains("property SimpleXMLElement::$StatusCode: SimpleXMLElement"),
        "expected magic-property hover from __get return type, got: {}",
        prop_hover
    );
    assert!(
        prop_hover.contains("[`SimpleXMLElement`](<file:///test/simplexml-inlay-hover.php#L3>)"),
        "expected clickable SimpleXMLElement type link in property hover, got: {}",
        prop_hover
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
async fn test_foreach_value_inlay_and_hover_from_indexed_method_generic_return() {
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

    let code_with_markers = r#"<?php
namespace Doctrine\Common\Collections;

interface Collection {}

namespace App\Entity;

use Doctrine\Common\Collections\Collection;

class ReversePortingNumber {
    public function getPhoneNumber(): string { return ''; }
}

class ReverseRequest {
    /**
     * @return Collection<int, ReversePortingNumber>
     */
    public function getReversePortingNumbers(): Collection {}
}

namespace App\Soap\Inbound\Handler;

use App\Entity\ReverseRequest;

function update(ReverseRequest $reverseRequest): void {
    foreach ($reverseRequest->getReversePortingNumbers() as $port/*decl*/ingNumber) {
        $pn = $porting/*usage*/Number->getPhoneNumber();
    }
}
"#;
    let markers = ["/*decl*/", "/*usage*/"];
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
    let (usage_line, usage_character) = marker_position("/*usage*/");
    let mut code = code_with_markers.to_string();
    for marker in markers {
        code = code.replace(marker, "");
    }
    let uri = "file:///test/foreach-indexed-method-generic-return.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, &code))
        .await
        .unwrap();

    let inlay_response = service
        .ready()
        .await
        .unwrap()
        .call(inlay_hint_request(2, uri, 0, 0, 30, 0))
        .await
        .unwrap();
    let inlay_result = extract_result(inlay_response);
    let hints = inlay_result.as_array().expect("expected inlay hint array");
    let labels: Vec<String> = hints.iter().filter_map(inlay_hint_label_text).collect();
    assert!(
        labels.iter().any(|label| label == ": ReversePortingNumber"),
        "expected foreach value type hint from indexed method PHPDoc return, got: {:?}",
        labels
    );
    assert!(
        hints
            .iter()
            .any(|hint| inlay_hint_has_label_part_location(hint, "ReversePortingNumber")),
        "expected foreach value inlay hint to include a navigable type location: {}",
        inlay_result
    );

    let hover_response = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(3, uri, usage_line, usage_character))
        .await
        .unwrap();
    let hover_result = extract_result(hover_response);
    let hover = hover_markdown_value(&hover_result);
    assert!(
        hover.contains("ReversePortingNumber $portingNumber"),
        "expected foreach value hover from indexed method PHPDoc return, got: {}",
        hover
    );
    assert!(
        hover.contains("[`ReversePortingNumber`](<file:///test/foreach-indexed-method-generic-return.php#L10>)"),
        "expected clickable ReversePortingNumber type link in hover, got: {}",
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
async fn test_hover_source_line_for_vendor_path_symbol() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-hover-vendor-source-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp_root);
    let vendor_dir = tmp_root.join("vendor/acme/package/src");
    let src_dir = tmp_root.join("src");
    fs::create_dir_all(&vendor_dir).unwrap();
    fs::create_dir_all(&src_dir).unwrap();

    let vendor_path = vendor_dir.join("Library.php");
    let app_path = src_dir.join("App.php");
    let root_uri = php_lsp_types::uri::path_to_uri(&tmp_root).unwrap();
    let vendor_uri = php_lsp_types::uri::path_to_uri(&vendor_path).unwrap();
    let app_uri = php_lsp_types::uri::path_to_uri(&app_path).unwrap();

    let vendor_php = r#"<?php
namespace VendorPkg;

class Library
{
    public function normalize(string $value): string { return trim($value); }
}
"#;
    let app_php = r#"<?php
namespace App;

use VendorPkg\Library;

function run(Library $library): void
{
    $library->normalize(' value ');
}
"#;
    fs::write(&vendor_path, vendor_php).unwrap();
    fs::write(&app_path, app_php).unwrap();

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
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&vendor_uri, vendor_php))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&app_uri, app_php))
        .await
        .unwrap();

    let normalize_position = utf16_position_at(app_php, "normalize(");
    let hover_response = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(
            2,
            &app_uri,
            normalize_position.0,
            normalize_position.1,
        ))
        .await
        .unwrap();
    let hover = hover_markdown_value(&extract_result(hover_response));

    assert!(
        hover.contains("```php\npublic function normalize(\n    string $value\n): string\n```"),
        "vendor-path method hover should use source-like local declaration, got: {}",
        hover
    );
    assert!(
        hover.contains(&format!(
            "**Symbol:** [`VendorPkg\\Library::normalize`](<{}#L6>)",
            vendor_uri
        )),
        "vendor-path method hover should expose linked FQN metadata, got: {}",
        hover
    );
    assert!(
        hover.contains(&format!(
            "**Source:** [`{}:6`](<{}#L6>)",
            vendor_path.display(),
            vendor_uri
        )),
        "vendor-path method hover should expose clickable vendor source metadata, got: {}",
        hover
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
async fn test_foreach_value_inlay_and_hover_from_doctrine_collection_target_entity() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let tmp_root = std::env::temp_dir().join(format!(
        "php-lsp-doctrine-collection-{}-{}",
        std::process::id(),
        nanos
    ));
    let entity_dir = tmp_root.join("src/Entity");
    let handler_dir = tmp_root.join("src/Handler");
    fs::create_dir_all(&entity_dir).unwrap();
    fs::create_dir_all(&handler_dir).unwrap();

    let entity_path = entity_dir.join("Order.php");
    let entity_code = r#"<?php
namespace App\Entity;

use Doctrine\Common\Collections\Collection;
use Doctrine\ORM\Mapping as ORM;

class OrderItem {
    public function getStatus(): ?OrderStatus { return new OrderStatus(); }
    public function sku(): string { return ''; }
}

class OrderStatus {
    public function name(): string { return ''; }
}

class Order {
    #[ORM\OneToMany(
        targetEntity: OrderItem::class,
        mappedBy: 'order',
        cascade: ['persist']
    )]
    private Collection $items;

    public function getItems(): Collection {
        return $this->items;
    }
}
"#;
    fs::write(&entity_path, entity_code).unwrap();

    let handler_path = handler_dir.join("CompleteHandler.php");
    let code_with_markers = r#"<?php
namespace App\Handler;

use App\Entity\Order;

function update(Order $order): void {
    foreach ($order->getItems() as $it/*decl*/em) {
        $sku = $it/*usage*/em->sk/*sku*/u();
        $st/*status*/atusName = $item->get/*getStatus*/Status()?->name();
    }
}
"#;
    let markers = [
        "/*decl*/",
        "/*usage*/",
        "/*sku*/",
        "/*status*/",
        "/*getStatus*/",
    ];
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
    let (usage_line, usage_character) = marker_position("/*usage*/");
    let (sku_line, sku_character) = marker_position("/*sku*/");
    let (status_line, status_character) = marker_position("/*status*/");
    let (get_status_line, get_status_character) = marker_position("/*getStatus*/");
    let mut handler_code = code_with_markers.to_string();
    for marker in markers {
        handler_code = handler_code.replace(marker, "");
    }
    fs::write(&handler_path, &handler_code).unwrap();

    let entity_uri = format!("file://{}", entity_path.to_string_lossy());
    let handler_uri = format!("file://{}", handler_path.to_string_lossy());

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
        .call(did_open_notification(&entity_uri, entity_code))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&handler_uri, &handler_code))
        .await
        .unwrap();

    let inlay_response = service
        .ready()
        .await
        .unwrap()
        .call(inlay_hint_request(2, &handler_uri, 0, 0, 10, 0))
        .await
        .unwrap();
    let inlay_result = extract_result(inlay_response);
    let hints = inlay_result.as_array().expect("expected inlay hint array");
    let labels: Vec<String> = hints.iter().filter_map(inlay_hint_label_text).collect();
    assert!(
        labels.iter().any(|label| label == ": OrderItem"),
        "expected foreach value type hint from Doctrine targetEntity, got: {:?}",
        labels
    );

    let hover_response = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(3, &handler_uri, usage_line, usage_character))
        .await
        .unwrap();
    let hover_result = extract_result(hover_response);
    let hover = hover_markdown_value(&hover_result);
    assert!(
        hover.contains("OrderItem $item"),
        "expected foreach value hover from Doctrine targetEntity, got: {}",
        hover
    );

    let sku_hover_response = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(4, &handler_uri, sku_line, sku_character))
        .await
        .unwrap();
    let sku_hover_result = extract_result(sku_hover_response);
    let sku_hover = hover_markdown_value(&sku_hover_result);
    assert!(
        sku_hover.contains("```php\npublic function sku(): string")
            && !sku_hover.contains("public function App\\Entity\\OrderItem::sku")
            && sku_hover.contains(": string"),
        "expected method hover from Doctrine targetEntity foreach receiver, got: {}",
        sku_hover
    );
    assert!(
        sku_hover.contains("**Symbol:** [`App\\Entity\\OrderItem::sku`]"),
        "expected linked FQN metadata for Doctrine targetEntity method hover, got: {}",
        sku_hover
    );
    assert!(
        sku_hover.contains("**Source:**") && sku_hover.contains("src/Entity/Order.php:9"),
        "expected clickable source metadata for Doctrine targetEntity method hover, got: {}",
        sku_hover
    );

    let get_status_hover_response = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(
            5,
            &handler_uri,
            get_status_line,
            get_status_character,
        ))
        .await
        .unwrap();
    let get_status_hover_result = extract_result(get_status_hover_response);
    let get_status_hover = hover_markdown_value(&get_status_hover_result);
    assert!(
        get_status_hover.contains("```php\npublic function getStatus(): ?OrderStatus")
            && !get_status_hover.contains("public function App\\Entity\\OrderItem::getStatus")
            && get_status_hover.contains("?OrderStatus"),
        "expected nullable method hover from Doctrine targetEntity foreach receiver, got: {}",
        get_status_hover
    );
    assert!(
        get_status_hover.contains("**Symbol:** [`App\\Entity\\OrderItem::getStatus`]"),
        "expected linked FQN metadata for nullable method hover, got: {}",
        get_status_hover
    );

    let status_hover_response = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(
            6,
            &handler_uri,
            status_line,
            status_character.saturating_sub(2),
        ))
        .await
        .unwrap();
    let status_hover_result = extract_result(status_hover_response);
    let status_hover = hover_markdown_value(&status_hover_result);
    assert!(
        status_hover.contains("string $statusName"),
        "expected follow-on member chain hover from Doctrine targetEntity foreach value, got: {}",
        status_hover
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
async fn test_foreach_value_inlay_and_hover_from_array_keys_after_array_write() {
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

    let code_with_markers = r#"<?php
function handleRegionChangeRequestComplete(array $numbers): void {
    $normalizedNumbers = [];
    foreach ($numbers as $number) {
        $normalizedNumber = preg_replace('/\D+/', '', is_scalar($number) ? (string)$number : '') ?? '';
        if ('' !== $normalizedNumber) {
            $normalizedNumbers[$normalizedNumber] = true;
        }
    }
    $numbers = array_keys($normalizedNumbers);

    foreach ($numbers as $phone/*decl*/Number) {
        strlen($phone/*usage*/Number);
    }
}
"#;
    let markers = ["/*decl*/", "/*usage*/"];
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
    let (usage_line, usage_character) = marker_position("/*usage*/");
    let mut code = code_with_markers.to_string();
    for marker in markers {
        code = code.replace(marker, "");
    }
    let uri = "file:///test/foreach-array-keys-normalized.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, &code))
        .await
        .unwrap();

    let inlay_response = service
        .ready()
        .await
        .unwrap()
        .call(inlay_hint_request(2, uri, 0, 0, 20, 0))
        .await
        .unwrap();
    let inlay_result = extract_result(inlay_response);
    let hints = inlay_result.as_array().expect("expected inlay hint array");
    let labels: Vec<String> = hints.iter().filter_map(inlay_hint_label_text).collect();
    assert!(
        labels.iter().any(|label| label == ": string"),
        "expected foreach value type hint from array_keys normalized array, got: {:?}",
        labels
    );

    let hover_response = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(3, uri, usage_line, usage_character))
        .await
        .unwrap();
    let hover_result = extract_result(hover_response);
    let hover = hover_markdown_value(&hover_result);
    assert!(
        hover.contains("string $phoneNumber"),
        "expected foreach value hover from array_keys normalized array, got: {}",
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
async fn test_callback_parameter_inference_from_indexed_signatures() {
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

    let definitions = r#"<?php
namespace App;

class User {
    public function getName(): string { return ''; }
}

/**
 * @template TItem
 */
class ExternalCollection {
    /**
     * @template TResult
     * @param callable(TItem): TResult $callback
     * @return ExternalCollection<TResult>
     */
    public function map(callable $callback): self { return $this; }
}

/**
 * @template TItem
 * @template TResult
 * @param callable(TItem): TResult $callback
 * @param array<int, TItem> $items
 * @return array<int, TResult>
 */
function external_map(callable $callback, array $items): array { return []; }
"#;

    let usage = r#"<?php
namespace App;

function run(): void {
    /** @var ExternalCollection<User> $users */
    $users = loadUsers();
    $users->map(fn($user) => $user->getName());

    /** @var array<int, User> $arrayUsers */
    $arrayUsers = [];
    external_map(fn($mappedUser) => $mappedUser->getName(), $arrayUsers);
}
"#;

    let defs_uri = "file:///test/callback-definitions.php";
    let usage_uri = "file:///test/callback-usage.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(defs_uri, definitions))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(usage_uri, usage))
        .await
        .unwrap();

    let find_line_col = |needle: &str| -> (u32, u32) {
        for (line, row) in usage.lines().enumerate() {
            if let Some(col) = row.find(needle) {
                return (line as u32, col as u32);
            }
        }
        panic!("needle not found: {needle}");
    };

    let (collection_line, collection_col) = find_line_col("$user) =>");
    let collection_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(
            2,
            usage_uri,
            collection_line,
            collection_col + 1,
        ))
        .await
        .unwrap();
    let collection_hover_text = hover_markdown_value(&extract_result(collection_hover));
    assert!(
        collection_hover_text.contains("User $user"),
        "collection callback parameter hover should infer User, got: {}",
        collection_hover_text
    );

    let (function_line, function_col) = find_line_col("$mappedUser) =>");
    let function_hover = service
        .ready()
        .await
        .unwrap()
        .call(hover_request(3, usage_uri, function_line, function_col + 1))
        .await
        .unwrap();
    let function_hover_text = hover_markdown_value(&extract_result(function_hover));
    assert!(
        function_hover_text.contains("User $mappedUser"),
        "function callback parameter hover should infer User, got: {}",
        function_hover_text
    );

    let (method_line, method_col) = find_line_col("$mappedUser->getName");
    let definition = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(
            4,
            usage_uri,
            method_line,
            method_col + "$mappedUser->".len() as u32,
        ))
        .await
        .unwrap();
    let definition_result = extract_result(definition);
    assert_eq!(
        definition_result
            .get("uri")
            .and_then(|value| value.as_str()),
        Some(defs_uri),
        "callback parameter method definition should resolve through indexed signature: {}",
        definition_result
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}
