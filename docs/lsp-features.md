# LSP Feature Matrix

This matrix documents the behavior advertised by the current server. "Partial"
means the server implements the LSP method, but the behavior is intentionally
limited, performance-sensitive on large workspaces, or delegated to external
tools.

Latest acceptance refresh: 2026-05-28 (`IE-045`). The feature matrix reflects
the IDE intelligence milestone after PHPDoc/type inference, framework provider,
Blade-like document, and Symfony/Twig document work. Performance evidence lives
in `docs/production-baseline.md`.

Implementation ownership for the major feature areas is documented in the
Feature Ownership Map in `docs/architecture.md`; keep this file focused on
client-visible LSP behavior and known limits.

## Status Legend

| Status | Meaning |
|---|---|
| Supported | Implemented and expected to work for normal PHP projects. |
| Partial | Implemented with known limits documented in the notes. |
| Unsupported | Not advertised or returns no edit/result by design. |

## Synchronization And Workspace

| LSP feature | Status | Notes |
|---|---|---|
| `initialize` / `initialized` | Supported | Applies initialization options, loads stubs, starts background indexing, publishes status notifications. |
| `textDocument/didOpen` | Supported | Parses editor text, updates index, publishes diagnostics. |
| `textDocument/didChange` | Supported | Incremental parser edits, index refresh, version checks, debounced fast diagnostics. |
| `textDocument/didSave` | Supported | Publishes full diagnostics, including enabled external analyzers. |
| `textDocument/didClose` | Supported | Clears parser state, diagnostics, semantic-token state, and pending analyzer work. |
| `workspace/didChangeWatchedFiles` | Supported | Reindexes changed/created PHP files and removes deleted files. |
| `workspace/didChangeConfiguration` | Supported | Runtime updates for diagnostics, stubs, indexing, vendor, formatter, analyzers, and logging. |
| `workspace/didChangeWorkspaceFolders` | Supported | Adds/removes roots and indexes new roots. |
| `workspace/willCreateFiles` | Partial | Advertised for PHP files but currently returns no edit. |
| `workspace/didCreateFiles` | Supported | Reindexes created PHP files. |
| `workspace/willRenameFiles` | Unsupported | Not advertised. |
| `workspace/didRenameFiles` | Supported | Moves indexed file state from old URI to new URI. Does not rewrite namespaces/classes. |
| `workspace/willDeleteFiles` | Partial | Advertised for PHP files but currently returns no edit. |
| `workspace/didDeleteFiles` | Supported | Removes indexed symbols for deleted PHP files. |

## Navigation

| LSP feature | Status | Notes |
|---|---|---|
| `textDocument/definition` | Supported | Handles indexed symbols, local variables, `$this`, constructors, PHPDoc virtual members, PHPDoc/literal shape keys, static framework string keys, template paths, Symfony Twig route keys, and lazy vendor fallback. |
| `textDocument/declaration` | Supported | Goes to import declarations when applicable, otherwise falls back to definition. |
| `textDocument/typeDefinition` | Supported | Resolves variable/member/function return types where inferred or indexed, including common PHPDoc generic inheritance substitutions and PHPStan/Psalm type alias expansion. |
| `textDocument/implementation` | Supported | Interface/trait/base type to implementations, and method implementation lookup. |
| `textDocument/references` | Partial | Uses indexed per-file references for symbols and same-scope references for local variables. Workspace-wide references can still be expensive on large workspaces. |
| `textDocument/documentHighlight` | Supported | Local variables and non-local symbols in the current document. |
| `textDocument/selectionRange` | Supported | AST-based selection expansion. |
| `textDocument/linkedEditingRange` | Partial | Namespace/use alias ranges only. |
| `textDocument/documentLink` | Supported | Static `include`, `include_once`, `require`, and `require_once` paths resolve to existing local files. |

## Symbols And Hierarchies

| LSP feature | Status | Notes |
|---|---|---|
| `textDocument/documentSymbol` | Supported | Nested namespace/type/member symbols with signatures and deprecation tags. |
| `workspace/symbol` | Supported | Ranked search over indexed workspace symbols, limited to 200 results. |
| `textDocument/prepareCallHierarchy` | Supported | Functions, methods, constructors, and containing callable fallback. |
| `callHierarchy/incomingCalls` | Partial | Scans indexed files and can read unopened files. Can be expensive on large workspaces. |
| `callHierarchy/outgoingCalls` | Supported | Reads the target callable file and resolves outgoing calls through the index. |
| `textDocument/prepareTypeHierarchy` | Supported | Classes, interfaces, traits, and enums. |
| `typeHierarchy/supertypes` | Supported | Uses extends/implements/use relationships and lazy class indexing. |
| `typeHierarchy/subtypes` | Supported | Uses indexed direct subtype relationships. |

## Editing And Refactoring

| LSP feature | Status | Notes |
|---|---|---|
| `textDocument/rename` | Partial | Supports classes, functions, methods, properties, constants, enum cases, and same-scope local variables. New names are validated by symbol kind; variables and properties still accept optional `$` input and normalize edits correctly. Built-ins and PHPDoc virtual members are not renamed. Workspace rename can still be expensive on large workspaces. |
| `textDocument/prepareRename` | Supported | Rejects unsupported, built-in, virtual, or unsafe unresolved member targets before rename. |
| `textDocument/codeAction` quick fix | Supported | Adds imports for unresolved classes/functions when candidates exist, removes one unused import, bulk-removes unused imports through organize imports, applies diagnostic replacement metadata, and supports opt-in PHPStan/Psalm local fixes for ignore comments, missing `@throws`, iterable PHPDoc value types, and prefixed class-name replacements. |
| `textDocument/codeAction` implement missing methods | Supported | Generates concrete stubs for missing interface, abstract parent, and abstract trait methods. Preserves method PHPDoc, analyzer-specific contract tags, safe method attributes, visibility, static, params, defaults, and native-safe return types. Edits are resolved lazily and stale document versions resolve to a no-op edit. |
| `textDocument/codeAction` generate members | Supported | Generates constructors and property getters/setters from indexed property symbols. Handles readonly/static properties, bool getter naming, nullable/default values, refined property PHPDoc types, analyzer-specific `@phpstan-var`/`@psalm-var` tags, and native-safe signatures. |
| `textDocument/codeAction` visibility and promotion refactors | Supported | Changes visibility for methods, properties, constants, and promoted properties with interface, abstract, and override contract guards. Promotes simple constructor assignment patterns to constructor property promotion, moving safe property PHPDoc and attributes onto the promoted parameter and refusing complex assignment patterns. |
| `textDocument/codeAction` PHPDoc signature sync | Supported | Updates `@param` order/types/tokens and native-return-driven `@return` tags from function/method signatures. Preserves descriptions, analyzer-specific richer generic PHPDoc types, summaries, and unrelated tags such as templates, throws, deprecation, virtual properties, and virtual methods. |
| `textDocument/codeAction` extract and inline refactors | Supported | Extracts exact selected expressions to collision-free local variables, extracts class-scope scalar literals to collision-free `private const` members, and inlines local variables with one simple assignment and one or more same-block reads. Refuses non-literals, out-of-class constants, branch/closure crossing, reassignment, compound assignment, and self-referential RHS cases. Edits are resolved lazily and stale document versions resolve to a no-op edit. |
| `source.organizeImports` | Supported | Sorts import statements with the existing class/function/constant grouping and removes unused imports from semantic references instead of raw text matches. Class imports used only in parsed PHPDoc type positions are kept; mentions in comments, strings, summaries, or PHPDoc prose do not count as usage. |
| `codeAction/resolve` | Supported | Used for heavier refactor actions so `textDocument/codeAction` can return lightweight actions first. |
| `refactor.rewrite` add return type | Partial | Adds return types from PHPDoc where supported by the configured PHP version. Edits are resolved lazily and stale document versions resolve to a no-op edit. |
| Native PHP formatter | Unsupported | Formatting is delegated to external commands. There is no `built-in` provider; see ADR-017 in `DECISIONS.md`. |
| `textDocument/formatting` | Partial | Uses trusted `phpLsp.formatting.provider`, `phpLsp.formatting.command`, or auto-detected Composer tools (`pint`, `php-cs-fixer`, `phpcbf`). Project `.php-lsp.toml` commands require `phpLsp.allowProjectCommands`. External formatter processes are timeout-bound and cancellable. |
| `textDocument/rangeFormatting` | Partial | Uses the same external formatter resolution, but formats only selected PHP fragments via temporary files and never formats the whole document for a range request. |
| `textDocument/onTypeFormatting` | Supported | Local indentation edits for newline, semicolon, and closing brace. |

## Intelligence

| LSP feature | Status | Notes |
|---|---|---|
| Diagnostics: syntax | Supported | Tree-sitter syntax errors. |
| Diagnostics: built-in semantic | Supported | Unknown symbols, unused code, duplicate symbols, member access, type compatibility, override signatures, PHP-version checks. Unqualified function calls follow current-namespace then global/built-in fallback before reporting unknown functions. PHPDoc numeric literal parsing covers the supported scalar integer/float forms, but type compatibility and override variance checks remain conservative approximations rather than full PHPStan/Psalm parity. Without Composer/vendor metadata, external framework symbols can be reported as unknown; highly dynamic framework members such as some Eloquent relation APIs remain best-effort. |
| Diagnostics: PHPStan | Partial | Optional external command, timeout-bound, JSON output required. |
| Diagnostics: Psalm | Partial | Optional external command, timeout-bound, JSON output required. |
| `textDocument/hover` | Supported | Symbols, source-like PHP declarations/signatures, linked FQN and source-file metadata for indexed symbols, linked class relations (`Extends`, `Implements`, `Uses`, `Mixins`), PHPDoc template/generic bindings, template variance and bounds, indexed PHP 8 attributes above declarations, Symfony/Doctrine framework role metadata, Doctrine `repositoryClass` links, complete signature parameter sections with scalar/array/mixed/untyped/default/by-ref/variadic parameters, PHPDoc parameter descriptions, types, variables, deprecation, PHPDoc virtual members, clickable class links in resolvable type sections, expanded indexed PHPDoc type aliases, local file-level PHPDoc shape aliases, call-site `class-string<T>` / conditional return inference, closure callback parameter inference from `callable(...)` signatures, and mapped Blade/Twig expression hovers where virtual PHP can resolve the symbol. |
| `textDocument/completion` | Supported | Classes, interfaces, traits, enums, functions, constants, members, variables, namespaces, keywords, snippets, auto-import edits, `use` FQN insertion, prefix-ranked namespace candidates, expanded member signature aliases, shape keys/properties from PHPDoc, local file-level shape aliases, and literal arrays, read/write-aware PHPDoc virtual properties, static PHPDoc virtual methods, framework string keys, Blade/Twig expression completions, Twig template path completions, callback parameter member chains, and member chains after `class-string<T>` factory calls. |
| `completionItem/resolve` | Supported | Enriches PHPDoc virtual member completions, including parsed `@method` parameters/defaults when available. |
| `textDocument/signatureHelp` | Supported | Functions, methods, constructors, and active parameter tracking. |
| `textDocument/inlayHint` | Supported | Argument labels, inferred PHPDoc parameter/return hints, and useful inferred local variable type hints for assignments, foreach key/value variables, `class-string<T>` factories, callback parameters, and conditional returns. |
| `textDocument/codeLens` | Partial | Reference-count lenses for symbols. Counts use indexed references but can still be expensive across very large workspaces. |
| `textDocument/foldingRange` | Supported | PHP structures, comments, arrays, namespaces, and blocks. |
| `textDocument/semanticTokens/full` | Supported | Full semantic token snapshots with result IDs. |
| `textDocument/semanticTokens/full/delta` | Supported | Delta edits from previous full snapshots. |
| `textDocument/semanticTokens/range` | Supported | Range semantic token requests for open files. |

## Template Documents

| Area | Status | Notes |
|---|---|---|
| Blade-like `.blade.php` documents | Partial | VS Code language contribution plus virtual PHP/source-map support for escaped/raw echo blocks and common `@if`, `@foreach`, `@isset`, and `@empty` control directives. Diagnostics are best-effort: exact source-mapped method/class/type expression errors can be reported, while syntax noise, generated PHP, view-variable context, template functions, and magic/incomplete properties stay suppressed. |
| Symfony/Twig `.twig` and `.html.twig` documents | Partial | Separate Twig language target with virtual PHP/source-map support for simple `{{ expr }}`, `{% if %}`, `{% for item in items %}`, `{% set name = expr %}`, comments, common block/include/extends/import semantic tokens, mapped hover/completion/definition/inlay hints, static include/extends/embed path completion and definition, static literal template-path definition for existing files under `templates/`, Symfony `path()`/`url()` route-key definition to `#[Route(name: ...)]`, best-effort exact-mapped expression diagnostics, and conservative Twig delimiter/block syntax diagnostics. Filters, tests, `in`, functions, macros, ternaries, null coalescing, and dynamic/bracket attribute access remain unsupported as full Twig expressions, but simple `object.member` chains, unfinished `object.` completion positions, and root variables inside unsupported filters/tests such as `items is defined` or `items|length` are source-mapped as no-op PHP fragments for hover/completion/definition. Type-preserving filters such as `items|slice(...)` and `items|filter(...)` additionally map the base collection so foreach item hover/completion/definition/inlay inference can keep its existing value type. Twig member completion also adds getter-derived property-style labels such as `id` for `getId()`, and hover/definition can fall back to the backing getter when no property symbol exists; getter-backed hovers use the same source-like declaration plus linked FQN/source metadata as PHP hovers. Twig `foreach` over Doctrine entity collections, Symfony form errors, and PHPDoc/inferred array-shape rows can infer item hover/completion/definition/inlay types. Twig attribute access over array shapes supports keys such as `row.npId`, nested keys such as `config_params.sftp.port`, Symfony `app.current_route`/`app.user`, Symfony `FormView` fields such as `form.email`, and local `{% set message_log = row.messageLog %}` variables; when source ranges are known, shape-key definitions jump to the PHPDoc shape key, literal array key, or `FormType::buildForm()->add('field')` field declaration. Foreach values over non-parameterized `array`/`iterable` can show `mixed` hover while `: mixed` foreach inlay hints stay suppressed. |
| Twig context variables | Partial | Statically inferred from simple PHP `render('template.html.twig', ['name' => expr])` call sites and other literal-template call sites where the next argument is a static context array, such as mail/notifier helpers. `new Class()`, simple arrays of new objects, typed controller parameter variables, nullable locals assigned conditionally before render, indexed `$this->service->method()` return types, repository method results with iterable PHPDoc/native return types, literal nested array shapes, `$items[] = [...]` append-built shapes, common `array_values` / `array_filter` / `array_map` / `explode` / `preg_split` list pipelines, `compact('name')` variables, Doctrine magic `find*`/`findOneBy*` repository results, Knp-style paginator variables backed by Doctrine repository/query-builder sources, and Symfony forms created via `createForm(SomeType::class, ...)` seed PHPDoc variables in virtual PHP. Form context extraction reads indexed `FormType::buildForm()` field names from `add('field')` calls and exposes each field as a `FormView`-like object with common `vars` keys such as `id` and `full_name`. Symfony fallback globals seed `app`, login `error`, and form-theme `errors` without booting Symfony; `app.user` prefers an indexed class implementing `Symfony\Component\Security\Core\User\UserInterface`. One-level Twig `{% include ... with {...} %}` calls can pass inferred caller variables and simple member chains into component templates, preserving foreach item hover/completion/definition and inlay hints for values such as `items: errorCodes` and `form_field: form.subscriber`. Custom Doctrine repositories can be resolved from indexed `@extends ServiceEntityRepository<Entity>` PHPDoc or ORM `repositoryClass` attributes without synchronous request-time source reads. Short PHPDoc class names from indexed repository methods are resolved against the method's own file before they are used in Twig foreach hover/definition/inlay links. Render keys with unknown value types are seeded as `mixed` to avoid false undefined-variable diagnostics. Open Twig documents refresh this inferred prelude after relevant PHP controller/render edits, open Twig caller edits, and workspace reindex events. The server does not boot Symfony or execute Twig extensions. |

## Explicit Non-Goals For Current Milestone

- Namespace/class rewrites during file rename.
- Native formatter implementation.
- Full PHP static analyzer replacement.
- Full Blade/Twig engine parity, runtime template inheritance evaluation, or
  execution of framework containers/extensions.
- Complete generic/template/type-alias/shape type system parity with
  PHPStan/Psalm.
- Guaranteed sublinear references/rename/codeLens performance on very large
  workspaces without additional reference-index sharding or aggregation.
