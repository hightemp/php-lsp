# PHP Language Server — Roadmap и задачи

## Обзор этапов

| Этап | Срок | Цель |
|------|------|------|
| MVP | 4 недели | Рабочий LSP с базовыми фичами: diagnostics, hover, definition, completion, references, rename, symbols |
| v1 | 4-6 недель после MVP | signatureHelp, codeAction, formatting, semanticTokens, disk cache |
| vNext | Ongoing | inlayHints, callHierarchy, typeHierarchy, PHPStan/Psalm интеграция |

---

## Этап MVP (4 недели)

### Неделя 1: Scaffold + Transport + Parser

- [x] **M-001** Инициализация репозитория *(done 2026-02-08)*
  - git init, .gitignore (Rust + Node + VS Code)
  - LICENSE (MIT)
  - README.md (минимальный: что это, как собрать)

- [x] **M-002** Cargo workspace *(done 2026-02-08)*
  - Корневой `server/Cargo.toml` (workspace)
  - Crate `php-lsp-types` — общие типы (SymbolKind, TypeInfo, Visibility)
  - Crate `php-lsp-parser` — tree-sitter-php обёртка
  - Crate `php-lsp-index` — заглушка
  - Crate `php-lsp-completion` — заглушка
  - Crate `php-lsp-server` — точка входа (main.rs)

- [x] **M-003** VS Code extension scaffold *(done 2026-02-08)*
  - `client/package.json` (activationEvents, contributes.configuration, vscode-languageclient)
  - `client/tsconfig.json`
  - `client/esbuild.mjs`
  - `client/src/extension.ts` (activate/deactivate, LanguageClient с stdio)

- [x] **M-004** GitHub Actions CI *(done 2026-02-08)*
  - Workflow: cargo clippy + cargo fmt --check + cargo test
  - Workflow: npm ci + npm run build (client)
  - Matrix: ubuntu-latest (основной)

- [x] **M-005** LSP hello-world *(done 2026-02-08)*
  - `main.rs`: tokio::main, tower-lsp-server, stdio transport
  - `server.rs`: LanguageServer trait — initialize (возврат ServerCapabilities), shutdown, exit
  - Проверка: клиент запускает сервер, Output channel показывает initialized

- [x] **M-006** Интеграция tree-sitter-php *(done 2026-02-08)*
  - `parser.rs`: FileParser struct (tree_sitter::Parser + ropey::Rope + Tree)
  - `parse_full(source)` — полный парсинг
  - `apply_edit(TextDocumentContentChangeEvent)` — инкрементальный
  - Unit-тесты: парсинг класса, функции, error recovery (5 тестов)

### Неделя 2: Document Sync + Index Core + Diagnostics

- [x] **M-007** didOpen / didChange / didClose / didSave *(done 2026-02-08)*
  - Менеджер открытых документов (DashMap<String, FileParser>)
  - didOpen: parse_full → сохранить в map
  - didChange: apply_edit (incremental, TextDocumentSyncKind=2)
  - didClose: удалить из map
  - didSave: noop (пока)
  - Debounce didChange: пока без debounce (TODO)

- [x] **M-008** Diagnostics (синтаксические) *(done 2026-02-08)*
  - Обход CST: найти ERROR и MISSING ноды tree-sitter
  - Маппинг в Diagnostic (range, severity=Error, source="php-lsp")
  - publishDiagnostics при didOpen/didChange
  - Тесты: 3 теста (valid, invalid, multiple errors)

- [x] **M-009** Индекс — структуры данных *(done 2026-02-08)*
  - `php-lsp-types`: SymbolInfo, SymbolKind, Visibility, SymbolModifiers, Signature, ParamInfo, TypeInfo
  - `php-lsp-index/workspace.rs`: WorkspaceIndex (DashMap-based)
  - API: update_file, remove_file, resolve_fqn, search, get_members
  - Unit-тесты CRUD (4 теста)

- [x] **M-010** Symbol extraction из CST *(done 2026-02-08)*
  - `php-lsp-parser/symbols.rs`: обход CST tree-sitter
  - Извлечение: class, interface, trait, enum, function, method, property, class_constant, global constant, enum_case
  - Извлечение: namespace (с и без фигурных скобок), use statements (class/function/const)
  - Извлечение: visibility, modifiers (static, abstract, readonly, final)
  - Извлечение: type hints (union, intersection, nullable), signatures, constructor promotion
  - 13 тестов на все типы символов

### Неделя 3: Composer + Hover + Definition + Stubs

- [x] **M-011** Composer.json парсинг *(done 2026-02-08)*
  - `php-lsp-index/composer.rs`: парсинг composer.json (serde_json)
  - Извлечение autoload/autoload-dev: psr-4, psr-0, classmap, files
  - NamespaceMap: prefix → directory, resolve_class_to_paths, source_directories
  - 9 тестов включая Laravel-like composer.json

- [x] **M-012** Workspace индексация (background) *(done 2026-02-08)*
  - При `initialized`: запуск фоновой задачи
  - Обход файлов workspace по composer namespace map
  - Парсинг каждого .php файла → extract_symbols → update_file
  - Progress reporting: window/workDoneProgress/create + $/progress
  - Семафор для ограничения параллелизма

- [x] **M-013** phpstorm-stubs *(done 2026-02-08)*
  - Git submodule: server/data/stubs → JetBrains/phpstorm-stubs
  - Загрузка при старте: парсинг stubs для расширений из конфига
  - Добавление в индекс с модификатором defaultLibrary
  - Кэширование (опционально, можно в v1)

- [x] **M-014** textDocument/hover *(done 2026-02-08)*
  - Определение символа под курсором (CST node → FQN)
  - Поиск в индексе: resolve_fqn
  - Формирование Hover: Markdown с FQN, сигнатурой, PHPDoc
  - Тесты: hover на классе, методе, built-in функции

- [x] **M-015** textDocument/definition *(done 2026-02-08)*
  - Определение символа под курсором → FQN
  - Поиск в индексе → Location (uri + range)
  - Поддержка: class, interface, trait, enum, function, method, property, const
  - Тесты: cross-file definition

- [x] **M-016** PHPDoc мини-парсер *(done 2026-02-08)*
  - `php-lsp-parser/phpdoc.rs`: парсинг doc-комментариев
  - Теги: @param, @return, @var, @throws, @deprecated, @property, @property-read, @property-write, @method
  - Поддержка union/intersection/nullable типов
  - 12 тестов

### Неделя 4: Completion + References + Rename + Symbols + Polish

- [x] **M-017** textDocument/completion *(done 2026-02-08)*
  - `php-lsp-completion/context.rs`: определение контекста (->  ::  $  \  free)
  - Провайдеры:
    - MemberAccess: методы/свойства по типу объекта (best-effort)
    - StaticAccess: статические методы/свойства/константы
    - Variable: локальные переменные ($)
    - Namespace: классы/функции из namespace (\)
    - FreeContext: классы, функции, ключевые слова PHP
  - triggerCharacters: ['$', '>', ':', '\\']
  - resolveProvider: true
  - Тесты на каждый контекст

- [x] **M-018** completionItem/resolve *(done 2026-02-08)*
  - Подгрузка PHPDoc, полной сигнатуры, deprecated
  - Тест

- [x] **M-019** textDocument/references *(done 2026-02-08)*
  - Определение символа → FQN
  - Поиск по индексу references
  - Параметр includeDeclaration
  - Тест: все ссылки на класс в workspace

- [x] **M-020** textDocument/rename + prepareRename *(done 2026-02-08)*
  - prepareRename: валидация позиции (возврат null на ключевых словах)
  - rename: собрать все ссылки + определение → WorkspaceEdit
  - Проверки: имя не пустое, нет коллизий
  - Тесты

- [x] **M-021** textDocument/documentSymbol *(done 2026-02-08)*
  - Иерархический формат (DocumentSymbol[])
  - namespace → class → method/property/const
  - Тест

- [x] **M-022** workspace/symbol *(done 2026-02-08)*
  - Fuzzy-match по query в глобальном индексе
  - Возврат WorkspaceSymbol[] с location
  - Тест

- [x] **M-023** Vendor lazy indexing *(done 2026-02-08)*
  - При resolve_fqn не найден → проверить namespace_map → найти файл в vendor → парсить on-demand
  - Кэшировать распарсенные vendor-файлы
  - Тест

- [x] **M-024** Семантические диагностики (базовые) *(done 2026-02-08)*
  - Неизвестный класс (не найден в индексе) — Warning
  - Неизвестная функция — Warning
  - Неразрешённый use — Warning
  - Тесты

- [x] **M-025** Трейсинг и логирование *(done 2026-02-08)*
  - Поддержка trace из InitializeParams (off/messages/verbose)
  - $/logTrace при verbose
  - window/logMessage для важных событий
  - logLevel из конфига

- [x] **M-026** End-to-end тестирование *(done 2026-02-09)*
  - In-process mock client тесты (tower-lsp LspService + socket draining)
  - 6 E2E тестов: initialize_and_shutdown, open_file_and_hover, goto_definition, completion, document_symbols, rename
  - tests/e2e.rs с helper-функциями для JSON-RPC запросов

- [x] **M-027** Тест-fixtures *(done 2026-02-08)*
  - test-fixtures/basic/ — минимальный PHP (hello.php, Foo.php)
  - test-fixtures/composer-psr4/ — PSR-4 с composer.json + src/Service/UserService.php
  - test-fixtures/syntax-errors/ — битый код (broken.php)

---

## Hotfix backlog (post-MVP)

- [x] **H-001** Built-in function resolution в namespace (definition/rename) *(done 2026-02-15)*
  - Символы вида `strlen()` внутри namespace не должны резолвиться только как `App\\Ns\\strlen`
  - Добавить fallback до global/built-in функции при lookup в server/resolve-path
  - Проверить блокировку rename для built-in (invalid params)

- [x] **H-002** References для class constant (`Class::CONST`, `self::CONST`) *(done 2026-02-15)*
  - Поддержать CST-узел `class_constant_access_expression` в поиске ссылок
  - Убедиться, что references включает declaration + все usage

- [x] **H-003** Ложный `ArgumentCountMismatch` на stubs с version-gated сигнатурами *(done 2026-02-15)*
  - Нормализовать извлечение параметров из stubs (дубликаты параметров одного имени, variadic-варианты)
  - Устранить false positive (пример: `array_map`)

- [x] **H-004** Шум семантических диагностик при синтаксически битом файле *(done 2026-02-15)*
  - Не публиковать semantic warning'и, если в файле есть syntax errors
  - Оставлять только syntax diagnostics до восстановления парсинга

- [x] **H-005** Консистентный FQN для qualified function call *(done 2026-02-15)*
  - Исправить резолв `A\\B\\fn()` (без потери префикса и без двойного namespace в сообщениях)
  - Синхронизировать поведение в `resolve`, `semantic`, `references`

- [x] **H-006** Реально применить `phpVersion` из VS Code в сервере *(done 2026-05-22 via PR-030)*
  - Проблема: клиент отправляет `phpVersion`, но сервер его не использует; stubs загружаются по `DEFAULT_EXTENSIONS` без учета версии PHP.
  - Что сделать:
    - Расширить `initializationOptions`/runtime config на стороне сервера: читать `phpVersion`, валидировать формат (например `8.1`, `8.2`, `8.3`).
    - Добавить обработку `workspace/didChangeConfiguration` и перезагрузку конфигурации без рестарта сервера.
    - Связать `phpVersion` с набором доступных built-in символов/стабов (фильтрация version-gated API).
  - Критерии готовности:
    - При смене `phpVersion` в VS Code меняются diagnostics/definition/completion для version-specific API.
    - Есть e2e-тест(ы) минимум для двух версий (например, API доступен в 8.2 и недоступен в 8.1).
    - В README/документации явно описано текущее поведение `phpVersion`.

- [x] **H-007** Убрать ложную semantic-ошибку для `self`/`static` в type hints *(done 2026-02-15)*
  - Проблема: корректный код вида `public function withSelf(self $arg): static` помечается как ошибка (ложный unknown-type).
  - Что сделать:
    - Пройти `semantic`-проверку type-reference и учесть все CST-варианты, где `self|static|parent` могут появляться (параметры, return type, nullable/union/intersection).
    - Убедиться, что built-in type list корректно применяется не только к `named_type`, но и к оберткам типов.
  - Критерии готовности:
    - На кейсе `withSelf(self $arg): static` нет diagnostic warning/error.
    - Добавлены regression-тесты для `self`, `static`, `parent` в аргументах и return type (включая nullable/union при допустимом синтаксисе).
    - Не сломаны текущие проверки unknown-class/unknown-type.

- [x] **H-008** PHPDoc parser: корректный разбор сложных типов и тегов *(done 2026-05-22 via PR-031)*
  - Проблема: текущий разбор `@param/@return/@var` обрезает тип до первого слова и неустойчив для `array<int, User>`, `(A&B)|null`, `callable(...)`, и похожих форм.
  - Что сделать:
    - Переписать извлечение type-expression из строки тега без `first_word` (учесть generic-скобки `<...>`, круглые скобки, `|`, `&`, `?`).
    - Сохранить текущую совместимость для простых случаев и malformed-тегов (не падать, игнорировать невалидное безопасно).
    - Расширить `parse_method_tag`: извлекать параметры и описание, а не только имя/return/static.
  - Критерии готовности:
    - Добавлены unit-тесты на сложные типы (включая пробелы внутри generic).
    - `@method` содержит распарсенные params (минимум имя + type если указан).
    - Все существующие тесты `phpdoc` проходят + новые regression-тесты.

- [x] **H-009** PHPDoc model: разделить `@property`, `@property-read`, `@property-write` *(done 2026-05-22 via PR-032)*
  - Проблема: в модели нет различия read/write семантики virtual property; сейчас все варианты обрабатываются одинаково.
  - Что сделать:
    - Расширить структуру `PhpDocProperty` флагами доступа (`readable`/`writable`) или enum-kind.
    - Обновить parser и сериализацию типов.
    - Добавить миграционные правки по месту использования (hover/completion).
  - Критерии готовности:
    - Для трех тегов (`@property`, `@property-read`, `@property-write`) в тестах получаются разные значения access-mode.
    - Обратная совместимость: старые кейсы не ломаются.

- [x] **H-010** UI для PHPDoc в LSP: показывать `@throws`, `@var`, `@property*`, `@method` *(done 2026-05-22 via PR-033)*
  - Проблема: hover/completion сейчас показывают только summary + `@param` + `@return` + `@deprecated`; остальные полезные теги теряются.
  - Что сделать:
    - Расширить markdown-рендер в `textDocument/hover` и `completionItem/resolve`.
    - Для class-symbol в hover добавить virtual members (`@property*`, `@method`) и `@throws`/`@var`, где применимо.
    - Согласовать формат вывода (чтобы не было дубликатов между signature и phpdoc-блоком).
  - Критерии готовности:
    - На `test-fixtures/lsp-cases/src/PhpDoc/SupportedTags.php` в hover видны `@throws` и virtual members.
    - В completion resolve видны doc-блоки с расширенными тегами без поломки markdown.

- [x] **H-011** Type inference из inline/local PHPDoc `@var` *(done 2026-02-15)*
  - Проблема: локальные аннотации `/** @var Type $x */` не участвуют в резолве типа, поэтому страдает completion/definition после присваивания.
  - Что сделать:
    - Добавить extraction inline-`@var` рядом с assignment/variable nodes.
    - Встроить это в `resolve` (best-effort) как fallback к нативным type hints.
    - Покрыть кейсы: одиночная переменная, reassignment, scope boundaries.
  - Критерии готовности:
    - На фикстуре с inline `@var` (`lsp-cases/src/PhpDoc/EdgeCases.php`) улучшается member completion.
    - Нет cross-scope false positives в references/definition.

- [x] **H-012** PHPDoc virtual members в completion/definition *(done 2026-05-22 via PR-033)*
  - Проблема: class-level `@property` и `@method` сейчас не участвуют в навигации/автодополнении как виртуальные члены.
  - Что сделать:
    - Подмешивать virtual members из PHPDoc класса в completion для `$obj->`.
    - Добавить go-to-definition/references на doc-объявление virtual member (минимальный MVP: definition на строку в doc-комментарии).
    - Для rename явно зафиксировать поведение (поддерживается/не поддерживается) и добавить guard.
  - Критерии готовности:
    - На `SupportedTags.php` видны `findById`/`label` в completion.
    - `Ctrl+Click` по virtual member возвращает definition в doc-comment.
    - Есть e2e/интеграционные тесты на completion + definition для virtual members.

- [x] **H-013** E2E покрытие PHPDoc (fixture-driven) *(done 2026-05-22 via PR-034)*
  - Проблема: есть unit-тесты parser-а, но не хватает сквозных e2e-тестов LSP на PHPDoc-поведение.
  - Что сделать:
    - Добавить e2e сценарии на `hover`, `completionItem/resolve`, `definition` по кейсам из `test-fixtures/lsp-cases/src/PhpDoc/*`.
    - Зафиксировать ожидаемое поведение по каждому тегу (что поддерживается, что игнорируется).
    - Обновить `test-fixtures/lsp-cases/README.md` и `README.md` статусами поддержки.
  - Критерии готовности:
    - E2E тесты падают на регрессиях по PHPDoc UI/навигации.
    - Документация соответствует фактическому поведению и тестам.

- [x] **H-014** Go-to-definition: наследование и chained member access *(done 2026-02-26)*
  - Проблема 1: `$this->okResponse()` — вызов метода, определённого в родительском классе. Go-to-definition не находил метод, т.к. не обходил цепочку наследования (extends/implements).
  - Проблема 2: `$this->timerService->method('start')` — chained member access. Не мог определить тип промежуточного свойства для дальнейшего lookup'а.
  - Что сделано:
    - `php-lsp-types`: добавлены поля `extends: Vec<String>`, `implements: Vec<String>` в `SymbolInfo`.
    - `php-lsp-parser/symbols.rs`: извлечение `base_clause` (extends) и `class_interface_clause` (implements) при парсинге class-like деклараций; FQN-резолв через use statements.
    - `php-lsp-index/workspace.rs`: `resolve_member` и `get_members` теперь обходят иерархию наследования рекурсивно (с защитой от циклических ссылок).
    - `php-lsp-parser/resolve.rs`: `try_resolve_object_type` теперь обрабатывает `member_access_expression` — резолвит тип объекта, затем ищет тип свойства в file symbols.
  - Тесты: 6 новых тестов (4 на парсинг extends/implements, 2 на наследование в индексе).

- [x] **H-015** Go-to-definition: vendor-классы и cross-file type resolution *(done 2026-03-03)*
  - Проблема: go-to-definition не работает для методов из vendor-зависимостей (PHPUnit `createStub`, `method` и т.п.).
  - Три бага:
    1. **Bug 1 — `::member` ломает PSR-4 resolve**: `resolve_fqn_lazy` получает FQN вида `PHPUnit\Framework\TestCase::createStub`, передаёт его целиком в `ns_map.resolve_class_to_paths()` и `resolve_vendor_paths()`. Суффикс `::createStub` ломает PSR-4 prefix matching → vendor файл не подгружается.
       - Фикс: в `resolve_fqn_lazy` (server.rs) перед PSR-4 lookup вызвать `rsplit_once("::")` чтобы извлечь class FQN без member, использовать class FQN для поиска файла, после индексации повторить resolve по полному FQN.
    2. **Bug 2 — нет рекурсивной подгрузки parent классов**: после lazy-индексации vendor-файла (напр. `TestCase.php`) его parent-классы (`Assert`, trait'ы) НЕ подгружаются рекурсивно. `resolve_member_in_hierarchy` обходит extends/implements, но parent-классы отсутствуют в индексе.
       - Фикс: после lazy-индексации файла прочитать `extends`/`implements` загруженного класса, рекурсивно вызвать `resolve_fqn_lazy` для каждого parent FQN (с глубиной ≤10).
    3. **Bug 3 — `try_resolve_object_type` не видит cross-file символы**: в `resolve.rs` `try_resolve_object_type` ищет тип свойства только в `file_symbols` (текущий файл). Свойства/методы определённые в других файлах (vendor, другие src файлы) невидимы. `member_call_expression` вообще возвращает `None`.
       - Фикс: ввести callback `MemberTypeResolver = Option<&dyn Fn(&str, &str) -> Option<String>>` в `symbol_at_position`/`try_resolve_object_type`. В `server.rs` при вызове `goto_definition` передавать closure с доступом к `WorkspaceIndex`. Обработать `member_call_expression` (определение return type метода через index).
  - Тестовые данные:
    - `test-fixtures/vendor-resolve/` — фикстура с fake vendor структурой (composer.json, vendor/composer/installed.json, vendor package files)
    - Покрывает: vendor lazy load, inheritance chain из vendor, chained member access, member_call_expression
  - Тесты:
    - Unit-тест в workspace.rs: inheritance chain across separately-indexed files
    - E2E тесты в e2e.rs: go-to-definition через vendor class, inherited vendor method, chained method call
  - Критерии готовности:
    - `$this->createStub()` (метод из parent vendor-класса) → go-to-definition находит его
    - `$this->timerService->method('start')` (chained member call) → go-to-definition на `method` работает
    - Все тесты проходят `cargo test --all`

- [x] **H-016** Тестирование go-to-definition на реальном проекте + 3 bugfix'а *(done 2026-03-05)*
  - Проверка go-to-definition на реальном Symfony+PHPUnit проекте (bdpn-ui, ~4000 PHP файлов).
  - Создан тестовый скрипт `scripts/test-goto-def.py` — Python LSP-клиент для автотестирования.
  - 13 test cases: наследование, vendor-классы, same-file методы, stub-методы, chained calls, type hints.
  - Три бага найдены и исправлены:
    1. **Bug 1 — `member_call_expression` не ищет return type в локальных `file_symbols`**: метод `makeHandler()` определён в том же файле, но `try_resolve_object_type` не проверял `file_symbols` для вызовов методов (только для свойств). Результат: `$handler->callOkResponse()` не работал.
       - Фикс: в `try_resolve_object_type` (resolve.rs), `member_call_expression` branch — добавлен локальный поиск по `file_symbols.symbols` перед fallback в resolver callback.
    2. **Bug 2 — `MemberTypeResolver` не передавался через цепочку inferring переменных**: `find_variable_inference_before_usage` вызывал `try_resolve_object_type(right, ..., None)`, теряя resolver. Результат: `$repo = $this->createStub(EntityRepository::class)` не резолвил тип через cross-file lookup.
       - Фикс: параметр `resolver: Option<MemberTypeResolver<'_>>` пробросан через всю цепочку: `infer_variable_type` → `infer_variable_type_in_scope` → `infer_variable_in_scope` → `find_variable_inference_before_usage` → `try_resolve_object_type`.
    3. **Bug 3 — `create_work_done_progress` блокирует workspace indexing навсегда**: `client.create_work_done_progress()` отправляет запрос клиенту и ожидает ответ. Если клиент не обрабатывает `window/workDoneProgress/create`, task блокируется навсегда, workspace НЕ индексируется.
       - Фикс: обёрнут вызов в `tokio::time::timeout(Duration::from_secs(5))`.
  - Результат: **10/13 тестов проходят** (было 6/13 → 10/13).
  - Оставшиеся 3 фейла — PHPUnit mock/stub паттерн (свойство объявлено как `TimerService`, а `expects()`/`method()` на `MockObject`/`Stub`). Требуется поддержка intersection types (`MockObject&TimerService`).
  - Все 132 unit/e2e тестов проходят, clippy clean.

- [x] **H-017** Auto-discovery composer.json + property assignment fallback + chain resolution *(done 2026-03-05)*
  - **Auto-discovery composer.json в subdirectories**: добавлена `find_composer_json(root)` — если `composer.json` нет в корне workspace, сканирует поддиректории (глубина 1), пропускает `node_modules/vendor/.git/docker/cache/logs/tmp`, при нескольких кандидатах предпочитает файл с секцией `autoload`. `workspace_root` обновляется на найденный effective root.
  - **Property assignment type fallback**: добавлен `infer_property_type_from_assignments()` — сканирует AST класса для `$this->prop = <expr>`, резолвит тип RHS через `try_resolve_object_type`. Используется как fallback в `goto_definition` когда объявленный тип свойства не содержит запрошенный member.
  - **Multi-type property resolution**: `find_all_property_assignment_types()` собирает ВСЕ distinct типы из разных assignments (напр. `createStub` → `Stub`, `createMock` → `MockObject`), fallback пробует каждый тип по порядку.
  - **Chain resolution**: secondary fallback в `try_resolve_object_type`'s `member_call_expression` handler — когда метод не найден на объявленном типе и объект `$this->prop`, ищет assignment-inferred type и пробует метод на нём. Обрабатывает `$this->em->method(...)->willReturn(...)` цепочки.
  - **Helper functions**: `find_enclosing_class_node()` (walks up to class/interface/trait/enum), `property_assignment_rhs()` (matches `$this->prop = <expr>` pattern), `find_all_property_assignment_types()` (recursive DFS collecting types).
  - **Bug fix**: column positions в тестах были off-by-1 (col 28 = `>` in `->`, а не `e` in `expects`).
  - **Unit test**: `test_infer_property_type_from_assignments` — тестирует с mock resolver, проверяет `em` и `timerService`.
  - Результат: **16/16 тестов проходят** (было 10/13 → 16/16).
  - 133 unit/e2e теста, clippy clean.

- [x] **H-018** Lazy-resolve use statements в diagnostics *(done 2026-03-05)*
  - **Проблема**: «Unresolved use statement: Doctrine\ORM\EntityManagerInterface» и другие vendor-классы показывались как warnings, потому что `compute_diagnostics` использовал синхронный `index.resolve_fqn()`, который НЕ триггерил lazy vendor indexing.
  - **Фикс**: в `publish_diagnostics` перед вызовом `compute_diagnostics` добавлен pre-resolve: итерируем use statements файла, для каждого неизвестного class-type FQN вызываем `lazy_index_class()` (async), который находит файл через PSR-4/vendor маппинг и индексирует.
  - **Результат**: vendor use statements (Doctrine, PHPUnit, Psr\Log и др.) резолвятся on-demand, ложные warnings исчезли.
  - Тест на реальном проекте: `Diagnostics: 0 (clean)` для SoapHandlerTest.php.
  - 133 unit/e2e теста, clippy clean.

- [x] **H-019** Go-to-definition для use statements *(done 2026-03-05)*
  - **Проблема**: go-to-definition на `use Doctrine\ORM\EntityManagerInterface;` не работал — FQN резолвился как `EntityManagerInterface` (короткое имя) вместо полного `Doctrine\ORM\EntityManagerInterface`.
  - **Причина**: `resolve_node` видел `name` → `qualified_name` (или `namespace_name`) → `namespace_use_clause`. Для `name` node с parent `qualified_name`/`namespace_name` попадал в generic wildcard → `resolve_name_node` → `is_constant_reference_context` → resolve как constant с коротким именем.
  - **Фикс**: добавлены `is_inside_use_clause()` и `extract_use_clause_fqn()` — при обнаружении что cursor внутри `namespace_use_clause` (walk up до 3 уровней), извлекается полный FQN из `qualified_name` child и возвращается как `RefKind::ClassName`.
  - **Unit test**: `test_resolve_use_statement_goto_def` — проверяет cursor на каждом сегменте (first/middle/last), single-segment use.
  - Результат: **19/19 тестов** (3 новых use statement теста).
  - 134 unit/e2e теста, clippy clean.

- [x] **H-020** Aliased use statements, qualified names, closure params *(done 2026-03-18)*
  - **Проблема 1**: `use Symfony\...\Constraints as Assert;` — ложная диагностика «Unresolved use statement» (FQN — namespace, не класс).
  - **Фикс 1**: в `check_use_statements` (semantic.rs) при `alias.is_some()` пропускаем диагностику для неразрешённых FQN.
  - **Проблема 2**: `new Assert\NotBlank(...)` — go-to-definition не работал: parent ноды `name("NotBlank")` — `qualified_name`, а не `object_creation_expression`.
  - **Фикс 2**: добавлены `find_qualified_name_ancestor()` и `is_inside_class_reference_context()` в resolve.rs, новый match arm для `"qualified_name" | "namespace_name"` в контексте class reference.
  - **Проблема 3**: `$er->createQueryBuilder()` и `$subscriber->getLastName()` внутри closure — go-to-definition не работал.
  - **Причина**: `find_enclosing_function` использовал `"anonymous_function_creation_expression"`, но tree-sitter PHP использует `"anonymous_function"`.
  - **Фикс 3**: добавлен `"anonymous_function"` в resolve.rs (2 места) и references.rs (1 место).
  - **Проблема 4**: 8 «Unknown class» warnings для классов через aliased namespace (Assert\NotBlank → Symfony\...\Constraints\NotBlank).
  - **Фикс 4**: добавлена `collect_aliased_class_fqns()` в semantic.rs — обходит CST и собирает FQN классов из aliased qualified names. В `publish_diagnostics` (server.rs) вызывается перед `compute_diagnostics` для pre-resolve через `lazy_index_class`.
  - **Unit tests**: `test_resolve_new_qualified_name`, `test_resolve_closure_param_method_call`, `test_resolve_closure_param_method_chain`, `test_aliased_use_no_false_diagnostic`.
  - **E2E test**: `scripts/test-porting-request-type.py` — 33 теста по всему PortingRequestType.php.
  - Результат: **33/33 go-to-def**, **0 diagnostics**, **19/19 regression tests**.
  - 138 unit тестов, clippy clean.

- [x] **H-021** `new ClassName()` go-to-definition → конструктор *(done 2026-03-18)*
  - **Проблема**: `new Assert\NotBlank(...)` вёл на объявление класса (строка 24), а не на конструктор `__construct` (строка 41).
  - **Фикс**: добавлен `RefKind::Constructor` в resolve.rs. Для `new ClassName()` и `new Alias\Class()` возвращается `fqn = "ClassName::__construct"` с `RefKind::Constructor`. В `goto_definition` и `hover` (server.rs) при неудаче поиска `__construct` делается fallback на класс.
  - **Затронутые файлы**: resolve.rs (новый RefKind + 2 match arms + helper `is_inside_object_creation_context`), server.rs (goto_definition fallback + hover fallback + references/rename match arms).
  - **Результат**: `new Assert\NotBlank` → NotBlank.php:41 (`__construct`), `new Assert\Length` → Length.php:69 (`__construct`).
  - **E2E**: 33/33 go-to-def, 19/19 regression, 0 diagnostics.
  - 138 unit тестов, clippy clean.

- [x] **H-022** Исправление UTF-16 позиций — файл подсвечивался целиком красным при редактировании *(done 2026-03-18)*
  - **Проблема**: при редактировании файлов с кириллицей (и другими не-ASCII символами) LSP-сервер подсвечивал весь файл ошибками. После исправления ошибки диагностика не обновлялась.
  - **Причина**: LSP протокол передаёт `Position.character` в кодовых единицах UTF-16, а tree-sitter использует байтовые смещения для `Point.column`. `apply_edit` в parser.rs трактовал UTF-16 символы как байтовые смещения, что повреждало содержимое rope-буфера на строках с кириллицей (2 байта UTF-8, 1 UTF-16 code unit).
  - **Фикс**:
    - **parser.rs**: переписан `apply_edit` — новая `utf16_position_to_byte()` правильно конвертирует UTF-16 позиции в байтовые смещения. Tree-sitter `Point` теперь создаётся с байтовыми колонками.
    - **utf16.rs** (НОВЫЙ модуль): `Utf16LineIndex` (индекс для batch-конверсии), `byte_col_to_utf16()`, `utf16_col_to_byte()`, `range_byte_to_utf16()`.
    - **server.rs**: все 10 входящих вызовов (`symbol_at_position`, `variable_definition_at_position`, `detect_context`, `find_variable_references_at_position`, `infer_variable_type_at_position`) конвертируют `pos.character` через `utf16_col_to_byte()`. Все исходящие позиции (diagnostics, variable refs, rename edits, prepareRename ranges, reference locations) конвертируются через `range_byte_to_utf16()` / `Utf16LineIndex`.
  - **Затронутые файлы**: parser.rs, utf16.rs (новый), lib.rs, server.rs.
  - 141 unit тест, 16 e2e тестов, clippy clean.

- [x] **H-023** Go-to-definition и hover для method chains (`$er->createQueryBuilder()->orderBy()->addOrderBy()`) *(done 2026-03-18)*
  - **Проблема**: go-to-definition не работал для методов в цепочке вызовов. `$er->createQueryBuilder('s')->orderBy(...)` — `orderBy` не разрешался, потому что LSP не мог определить тип возвращаемый `createQueryBuilder()` и далее по цепочке.
  - **Причины**:
    1. Типы возвращаемых значений `self`/`static`/`$this` не обрабатывались — `resolve_member_type` и `try_resolve_object_type` передавали их в `resolve_class_name`, который не мог их разрешить.
    2. Hover handler использовал `symbol_at_position` (без resolver), поэтому не мог резолвить cross-file типы в цепочках.
    3. PHPDoc `@return` не использовался как fallback когда PHP-тип возврата отсутствует.
  - **Фикс**:
    - **resolve.rs**: в `try_resolve_object_type` для `member_call_expression` и `member_access_expression` — если return type = `self`/`static`/`$this`, возвращает FQN класса-владельца метода.
    - **server.rs**: `resolve_member_type` — аналогичная обработка `self`/`static`/`$this`. Hover handler переведён на `symbol_at_position_with_resolver` для поддержки цепочек.
    - **symbols.rs**: `extract_method` и `extract_function` — когда PHP return type отсутствует, берётся `@return` из PHPDoc как fallback.
  - **Тесты**: 3 новых теста — `test_resolve_method_chain_static_return_type`, `test_resolve_method_chain_phpdoc_return_this`, `test_resolve_method_chain_cross_class_return`.
  - 144 unit теста, 16 e2e тестов, clippy clean.

- [x] **H-024** Ложные срабатывания "Too few arguments" для функций с необязательными параметрами *(done 2026-03-18)*
  - **Проблема**: для `preg_replace_callback()`, `mb_strtolower()`, `file_get_contents()` и других функций выдавалась ошибка "Too few arguments", хотя вызовы были корректными.
  - **Причина**: расчёт `required` считал ВСЕ параметры без `default_value` как обязательные. Но в phpstorm-stubs (и в PHP) параметры, идущие после параметра с дефолтом, фактически необязательны, даже если у них нет явного `default_value`. Пример: `preg_replace_callback(..., int $limit = -1, &$count, ...)` — `&$count` не имеет default, но стоит после `$limit` (с дефолтом) и является необязательным в PHP.
  - **Фикс**: изменён расчёт `required` с подсчёта всех не-default параметров на "непрерывный обязательный префикс" — позиция первого параметра с `default_value` или `is_variadic`. Параметры после этой позиции считаются необязательными.
    - **semantic.rs**: два места — проверка аргументов обычных функций (~line 357) и конструкторов (~line 192). В обоих использован `.position()` вместо `.filter().count()`.
  - **Тесты**: 1 новый тест `test_no_false_positive_for_optional_params_after_default` — эмулирует сигнатуру `preg_replace_callback` с 6 параметрами (3 required, 1 default, 1 by-ref без default, 1 default), вызов с 3 аргументами не должен давать ошибку.
  - 145 unit тестов, 16 e2e тестов, clippy clean.

- [x] **H-025** Ложные "Too few arguments" для `mb_strtolower()`, `str_replace()` и аналогичных *(done 2026-03-18)*
  - **Проблема**: H-024 не закрыл все случаи. `mb_strtolower(string $string, ?string $encoding)` — оба параметра без default → required=2. `str_replace($search, $replace, $subject, &$count)` — все 4 без default → required=4. Но `$encoding` и `&$count` помечены `[optional]` в PHPDoc стабов, и фактически необязательны в PHP.
  - **Причина**: phpstorm-stubs помечают необязательные параметры через `@param ... [optional]` в PHPDoc, а не через `default_value` в сигнатуре. Парсер не учитывал эту аннотацию.
  - **Фикс**:
    - **symbols.rs**: новая функция `apply_phpdoc_to_signature()` — парсит PHPDoc, находит `@param` с `[optional]` в описании, и ставит синтетический `default_value = "null"` для соответствующих параметров сигнатуры. Вызывается из `extract_method` и `extract_function` (заменила дублированный код PHPDoc `@return` fallback).
    - **phpdoc.rs**: исправлен `parse_param_tag` — добавлена поддержка `&$name` (by-ref) в PHPDoc `@param`. Ранее `&$count` не распознавался как имя параметра.
  - **Тесты**: 2 новых теста — `test_phpdoc_optional_sets_default_value` (эмулирует `mb_strtolower`), `test_phpdoc_optional_on_byref_param` (эмулирует `str_replace` с `&$count`).
  - 147 unit тестов, 16 e2e тестов, clippy clean.

- [x] **H-026** Go-to-definition для promoted constructor properties (`$this->logger->debug()`) *(done 2026-03-18)*
  - **Проблема**: go-to-definition не работал для методов на свойствах, объявленных через constructor promotion (`protected readonly LoggerInterface $logger`). `$this->logger->debug(...)` не разрешался.
  - **Причина**: `extract_method` создавал `ParamInfo` с `is_promoted: true` для промоутнутых параметров, но НЕ создавал `SymbolInfo` с `kind: Property`. Поэтому при разрешении `$this->logger` поиск символа `Class::$logger` не находил ничего, и тип не определялся.
  - **Фикс**:
    - **symbols.rs**: в `extract_method` после создания Method символа добавлен проход по `property_promotion_parameter` нодам — для каждого создаётся дополнительный `SymbolInfo` с `kind: Property`, `fqn: Class::$name`, правильными visibility/modifiers и типом.
  - **Тесты**: 1 новый тест `test_promoted_constructor_params_emit_property_symbols` — проверяет что promoted параметры создают Property символы с правильным FQN, visibility, readonly модификатором и типом, а обычные параметры — нет.
  - 148 unit тестов, 16 e2e тестов, clippy clean.

---

## Этап v1 (4-6 недель после MVP)

### Signature Help

- [x] **V1-001** textDocument/signatureHelp *(done 2026-05-19)*
  - triggerCharacters: ['(', ',']
  - Показать параметры функции/метода
  - Подсветить текущий параметр
  - PHPDoc @param
  - Поддержать overload-like варианты из стабов/PHPDoc где возможно
  - Работать для функций, методов, конструкторов и статических вызовов

### Code Actions

- [x] **V1-002** textDocument/codeAction — quick-fix: добавить use *(done 2026-05-19)*
  - Диагностика "unknown class" + code action "Add use statement"
  - Вставка `use FQN;` в блок use-statements
  - Разрешение конфликтов alias/import
  - Поддержка `function`/`const` imports там, где применимо

- [x] **V1-003** textDocument/codeAction — organize imports *(done 2026-05-19)*
  - source.organizeImports
  - Сортировка use-statements алфавитно, удаление неиспользуемых
  - Группировка class/function/const imports
  - Сохранение комментариев рядом с use-блоком

- [x] **V1-004** textDocument/codeAction — добавить return type *(done 2026-05-19)*
  - Если есть PHPDoc @return но нет return type hint
  - Не предлагать несовместимые типы для текущей target PHP version

### Formatting

- [x] **V1-005** textDocument/formatting — внешний formatter *(done 2026-05-19)*
  - Интеграция: php-cs-fixer / phpcbf через subprocess
  - Конфигурация: phpLsp.formatting.provider + phpLsp.formatting.command
  - Возврат TextEdit без записи файла напрямую

- [x] **V1-006** textDocument/rangeFormatting *(done 2026-05-19)*

- [x] **V1-015** textDocument/onTypeFormatting *(done 2026-05-19)*
  - Автоформатирование после `;`, `}`, newline
  - Минимальные локальные edits без полного форматирования файла

### Semantic Tokens

- [x] **V1-007** textDocument/semanticTokens/full *(done 2026-05-19)*
  - Legend: token types + modifiers по таблице в PRD
  - Обход CST, маппинг нод в semantic tokens

- [x] **V1-008** textDocument/semanticTokens/full/delta *(done 2026-05-19)*
  - Инкрементальное обновление на основе previousResultId

### Navigation polish

- [x] **V1-016** textDocument/declaration *(done 2026-05-19)*
  - Переход к декларации symbol/type alias/import, когда она отличается от definition
  - Fallback к definition для PHP-символов без отдельной declaration

- [x] **V1-017** textDocument/typeDefinition *(done 2026-05-19)*
  - Для переменных/свойств/return values переходить к объявлению класса типа
  - Использовать PHPDoc `@var`, `@param`, `@return` как fallback

- [x] **V1-018** textDocument/documentHighlight *(done 2026-05-19)*
  - Подсветка всех occurrences символа в текущем документе
  - Отдельно Read/Write для переменных и свойств где возможно

- [x] **V1-019** textDocument/selectionRange *(done 2026-05-19)*
  - AST-based расширение выделения: identifier → expression → statement → block → class/function

- [x] **V1-020** textDocument/linkedEditingRange *(done 2026-05-19)*
  - Связанные edits для парных PHP constructs где применимо
  - Минимум: namespace/use-safe rename внутри одного syntactic construct

### Completion polish

- [x] **V1-021** Улучшить completion до привычного IDE-уровня *(done 2026-05-19)*
  - snippets для `class`, `interface`, `trait`, `enum`, `function`, control-flow constructs
  - `sortText`, `filterText`, `insertTextFormat`, `commitCharacters`
  - auto-import completion через `additionalTextEdits`
  - Не предлагать недоступные private/protected/static/instance members в неправильном контексте

### Workspace & configuration

- [x] **V1-022** workspace/didChangeWatchedFiles и переиндексация изменённых файлов *(done 2026-05-19)*
  - Обрабатывать create/change/delete PHP файлов
  - Обновлять индекс без полного restart сервера
  - Удалять символы удалённых файлов из индекса

- [x] **V1-023** workspace/didChangeConfiguration и применение настроек клиента *(done 2026-05-19)*
  - Реально использовать `phpVersion`, `diagnosticsMode`, `composerEnabled`, `indexVendor`, `stubExtensions`, `logLevel`
  - Поддержать изменение настроек без restart где возможно
  - Синхронизировать VS Code configuration schema и server initializationOptions

- [x] **V1-024** Workspace file operations *(done 2026-05-19)*
  - Поддержать will/did create/rename/delete files где клиент это отдаёт
  - Обновлять URI символов при rename/move
  - Инвалидировать кэш и diagnostics для удалённых/перемещённых файлов

### Diagnostics parity

- [x] **V1-025** Расширить базовые diagnostics *(done 2026-05-20)*
  - Undefined variables
  - Unused imports
  - Unused local variables/parameters
  - Duplicate symbols в workspace

- [x] **V1-026** Type/member diagnostics *(done 2026-05-20)*
  - Unknown method/property/class constant
  - Visibility violations: private/protected access
  - Static/instance misuse
  - Basic type compatibility для параметров, return values, property assignments
  - Override/signature compatibility для inheritance
  - PHP-version-specific diagnostics

### Disk Cache

- [x] **V1-009** Кэш индекса на диск *(done 2026-05-22)*
  - Формат: bincode
  - Путь: ~/.cache/php-lsp/{workspace-hash}/{namespace}/index.bin
  - Инвалидация: mtime + size файлов
  - Ускорение повторного запуска

### Performance

- [x] **V1-011** Lazy vendor indexing — оптимизация *(done 2026-05-22)*
  - Предзагрузка popular packages
  - LRU-кэш для vendor-файлов
  - Parsed Composer `vendor/composer/installed.json` metadata cache
  - Dedicated vendor disk cache namespace for lazy-indexed file symbols

### Documentation

- [x] **V1-012** docs/architecture.md — потоки данных, диаграммы *(done 2026-05-25 via PR-052)*
- [x] **V1-013** docs/lsp-features.md — таблица статусов LSP-фич *(done 2026-05-25 via PR-052)*
- [x] **V1-014** README.md — полный (установка, настройки, troubleshooting) *(done 2026-05-25 via PR-052)*

---

## Этап vNext (ongoing)

- [x] **VN-001** textDocument/inlayHint — типы параметров, return types inline *(done 2026-05-20)*
- [x] **VN-002** textDocument/prepareCallHierarchy + incoming/outgoing *(done 2026-05-20)*
- [x] **VN-003** textDocument/prepareTypeHierarchy + supertypes/subtypes *(done 2026-05-20)*
- [x] **VN-004** textDocument/implementation (interface → concrete) *(done 2026-05-20)*
- [x] **VN-005** Multi-root workspace поддержка *(done 2026-05-20)*
  - Использовать `workspaceFolders` вместо одного `rootUri`
  - Отдельные composer namespace maps и индексы по workspace folder
  - Корректные diagnostics/search/symbols across folders
- [x] **VN-006** Интеграция PHPStan — subprocess + маппинг output → Diagnostics *(done 2026-05-20)*
- [x] **VN-007** Интеграция Psalm — subprocess + маппинг output → Diagnostics *(done 2026-05-20)*
- [x] **VN-008** Code Lens — количество ссылок на класс/метод *(done 2026-05-20)*
  - references count
  - test/run/debug codelens где применимо
- [x] **VN-009** Folding Range — складывание функций, классов, PHPDoc *(done 2026-05-20)*
  - textDocument/foldingRange capability
  - Функции, методы, классы, namespaces, PHPDoc, массивы/blocks
- [x] **VN-010** Release pipeline — cross-platform VSIX сборка + публикация в Marketplace *(done 2026-02-15)*
  - `scripts/build-server.sh` — сборка Rust бинарника, копирование в `client/bin/`
  - `scripts/bundle-stubs.sh` — копирование phpstorm-stubs в `client/stubs/`
  - `.github/workflows/release.yml` — matrix build (6 VS Code platform directories) + universal VSIX package
  - `client/.vscodeignore` — включает только `bin/`, `stubs/`, `out/`, `package.json`
  - `extension.ts` — передаёт `stubsPath` в initializationOptions
  - `server.rs` — принимает `stubsPath` из initializationOptions для поиска стабов
  - Поддержанные published platform directories: linux-x64, linux-arm64, darwin-x64, darwin-arm64, win32-x64, win32-arm64; Alpine/musl не заявляется как published VSIX target
  - Локальная сборка VSIX: 2.56 MB (бинарник + стабы + клиент)

- [x] **VN-011** Make release target — сборка + тегирование + push в GitHub *(done 2026-03-05)*
  - `VERSION` файл — единый источник версии
  - `make release` — берёт версию из `VERSION`, патчит `client/package.json` и `server/Cargo.toml`, собирает `package-all`, создаёт force-теग `v<VERSION>` и пушит на GitHub

---

## Milestone: Production Readiness (6 недель)

**Срок:** 2026-05-21 → 2026-07-01  
**Цель:** довести текущий feature-complete LSP до состояния, где его можно уверенно ставить на крупные PHP/Composer проекты без постоянных false positives, долгих cold starts и зависаний тяжелых запросов.

### Exit criteria

- Cold start на проекте 5k-10k PHP файлов после первого запуска: < 5 секунд до готового индекса из disk cache.
- Первый полный индекс на проекте 5k-10k PHP файлов: измерен, задокументирован, без блокировки hover/completion.
- p95 latency для hover/completion/definition на прогретом индексе: < 50 мс на контрольной фикстуре.
- `references`, `rename`, `codeLens` работают через индекс ссылок или другой инкрементальный механизм, без полного reparse workspace на каждый запрос.
- `didChange` устойчив к быстрому набору: debounce, version ordering, отмена устаревших diagnostics.
- `$/cancelRequest` или эквивалентная отмена применяется к тяжелым операциям: indexing, references, rename, external analyzers.
- `phpLsp.phpVersion` влияет не только на syntax/type diagnostics, но и на built-in stubs/completion/definition.
- PHPDoc generics/callable/array-shape-like типы не ломают parser и используются в hover/completion/type inference best-effort.
- Release workflow собирает все заявленные платформы и готов к публикации VS Marketplace.
- Есть `docs/architecture.md`, `docs/lsp-features.md`, troubleshooting и публичный список known limitations.

### Неделя 1: Baseline, профилирование, план стабилизации (2026-05-21 → 2026-05-27)

- [x] **PR-001** Зафиксировать production baseline *(done 2026-05-21)*
  - Прогнать `cargo test --all`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all --check`.
  - Прогнать `npm run lint` и `npm run build` в `client/`.
  - Зафиксировать текущие числа: количество тестов, время CI локально, размер VSIX, размер server binary.
  - Сохранить baseline в `docs/production-baseline.md`.

- [x] **PR-002** Добавить perf harness для реальных проектов *(done 2026-05-21)*
  - Скрипт `scripts/profile-workspace.sh` или Rust test harness для замера cold start, indexing time, memory RSS.
  - Сценарии: small fixture, Symfony/Laravel-like project, vendor-heavy project.
  - Метрики: files/sec, symbols/sec, peak RSS, stubs load time, first diagnostics time.
  - Результаты складывать в `target/php-lsp-profile/*.json`.

- [x] **PR-003** Добавить latency benchmarks для LSP requests *(done 2026-05-22)*
  - Скрипт LSP-клиента для batch-запросов: hover, completion, definition, references, rename dry-run.
  - Измерять p50/p95/p99 на прогретом и холодном индексе.
  - Отдельно измерять открытый файл и неоткрытый файл.

- [x] **PR-004** Составить risk register production gaps *(done 2026-05-22)*
  - Документировать known bottlenecks: full workspace scan в references/rename/codeLens, sync file reads, stubs load, vendor resolve.
  - Для каждого риска указать mitigation и owner task.
  - Обновить `README.md` known limitations без завышения текущих возможностей.

### Неделя 2: Disk cache и индексирование (2026-05-28 → 2026-06-03)

- [x] **PR-010 / V1-009** Реализовать disk cache индекса *(done 2026-05-22)*
  - Формат: `bincode` или другой компактный бинарный формат.
  - Путь: `~/.cache/php-lsp/{workspace-hash}/workspace/index.bin`.
  - Хранить: `FileSymbols`, top-level maps, версию schema, версию php-lsp, PHP version, stub extensions, include/exclude paths.
  - Инвалидация: mtime + size + config hash + stubs hash.
  - На старте сначала грузить cache, затем фоново доиндексировать changed files.

- [x] **PR-011** Разделить project index, stub index и vendor index *(done 2026-05-22)*
  - Отдельные cache namespaces: `workspace`, `stubs`, `vendor`.
  - Не удалять stubs при workspace reindex.
  - Ускорить reload настроек, затрагивающих только stubs или только workspace.
  - `workspace`: `~/.cache/php-lsp/{workspace-hash}/workspace/index.bin`.
  - `stubs`: cache keyed by php-lsp version, PHP version, extension list and stubs hash; повторный smoke load: 85 stub files за 23.23 ms.
  - `vendor`: lazy-indexed vendor file symbols сохраняются в `vendor/index.bin` после первого парсинга vendor файла.

- [x] **PR-012 / V1-011** Оптимизировать lazy vendor indexing *(done 2026-05-22)*
  - Кэшировать parsed composer installed/autoload metadata.
  - Добавить LRU для vendor file symbols.
  - Предзагружать часто используемые vendor entrypoints после ready, без блокировки быстрых запросов.
  - `vendor/composer/installed.json` парсится в `VendorAutoloadMap` и переиспользуется до изменения metadata fingerprint.
  - LRU удерживает до 512 lazy-indexed vendor files в symbol index; вытесненные файлы могут быть восстановлены из `vendor/index.bin`.
  - После workspace ready фоново предзагружаются до 16 `autoload.files` entrypoints.

- [x] **PR-013** Сделать indexing реально параллельным *(done 2026-05-22)*
  - Заменить последовательный loop с semaphore на task queue / `JoinSet`.
  - Ограничить concurrency конфигурацией или CPU-aware default.
  - Стабильно агрегировать progress и errors.
  - Проверить отсутствие гонок в `WorkspaceIndex::update_file`.
  - `index_workspace()` теперь использует `JoinSet::spawn_blocking` для read/parse файлов.
  - Default concurrency: `available_parallelism()` capped at 8.
  - Progress payload включает `parseConcurrency` и `indexingErrors`.
  - Добавлен regression test на concurrent `WorkspaceIndex::update_file()`.
  - Smoke `parallel-pr013=test-fixtures/basic`: первый run indexed 4 files за 304.47 ms до ready; второй run из cache — 28.09 ms.

- [x] **PR-014** Добавить команду `Clear PHP LSP Cache and Restart` *(completed 2026-05-22)*
  - VS Code command: `phpLsp.clearCacheAndRestart`.
  - Удалять cache только для текущих workspace roots: `~/.cache/php-lsp/{workspace-hash}/`.
  - Чистить все namespaces: `workspace`, `stubs`, `vendor`.
  - После очистки перезапускать language server и показывать user-facing confirmation/error.
  - Не смешивать с обычным `Restart Language Server`, который должен продолжать использовать disk cache.
  - Реализовано в command palette и status quick pick: подтверждение, stop server, удаление cache dir, restart.
  - Добавлен client helper `cachePath.ts` с FNV-compatible hash/path logic и `npm run check:cache-path`.
  - Validation: `npm run check:cache-path`, `npm run lint`, `npm run build`, `git diff --check`.

### Неделя 3: Responsiveness, debounce, cancellation (2026-06-04 → 2026-06-10)

- [x] **PR-020** Очередь `didChange` с debounce и version ordering *(completed 2026-05-22)*
  - Хранить document version из LSP events.
  - Debounce diagnostics 150-250 мс.
  - Отменять устаревшие diagnostic tasks при новом изменении файла.
  - Гарантировать, что старый результат diagnostics не перетрет новый.
  - Добавлен `document_versions` для open documents и per-URI debounce task registry.
  - `didChange` игнорирует stale/duplicate versions, diagnostics публикуются после 180 мс debounce.
  - `publishDiagnostics` теперь отправляет document version и пропускает результат, если версия изменилась во время вычисления.
  - `didSave`/`didClose`/file delete/rename отменяют pending debounce tasks.
  - Regression: e2e проверяет, что broken version 2 не публикуется после fixed version 3.
  - Validation: `cargo fmt --all --check`, `cargo test -p php-lsp-server`, `cargo clippy -p php-lsp-server --all-targets -- -D warnings`.

- [x] **PR-021** Поддержать cancellation для тяжелых операций *(completed 2026-05-22)*
  - Ввести `CancellationToken`/task registry для indexing, references, rename, external analyzer runs.
  - Проверить `$/cancelRequest` для `references` на большой фикстуре.
  - Возвращать LSP `RequestCancelled` там, где клиент явно отменил запрос.
  - Добавлен общий `OperationCancellationToken` на базе `AtomicBool + Notify`.
  - Background indexing runs теперь отменяют предыдущий indexing/reindex batch; `index_workspace()` проверяет token на discovery/cache/parse этапах и abort'ит pending parse tasks.
  - External analyzer runs (`PHPStan`/`Psalm`) получают per-URI token; `didChange`/`didClose`/delete/rename отменяют активный subprocess, `kill_on_drop` завершает процесс.
  - `references` и `rename` получили cooperative yield points каждые 32 indexed files, чтобы `tower-lsp` успевал обработать `$/cancelRequest`.
  - Regression: e2e `test_cancel_request_cancels_references_request` проверяет `RequestCancelled` (`-32800`) для большого `references` запроса.
  - Validation: `cargo fmt --all --check`, `cargo test -p php-lsp-server`, `cargo clippy -p php-lsp-server --all-targets -- -D warnings`, `git diff --check`.

- [x] **PR-022** Убрать full reparse workspace из references/rename/codeLens *(completed 2026-05-22)*
  - Построить reference index или per-file lightweight occurrence index при индексации.
  - Инвалидировать occurrence данные при didChange/didSave/watched files.
  - `references` должен читать готовые occurrence данные и парсить только открытый dirty buffer.
  - `rename` должен использовать те же ranges и валидировать конфликт имен до WorkspaceEdit.
  - Добавлен `SymbolReference` и `WorkspaceIndex::file_references`; occurrence данные строятся при workspace indexing, lazy vendor/stub indexing, didOpen/didChange и file watcher reindex.
  - Disk cache schema v3 сохраняет/загружает per-file references вместе с `FileSymbols`.
  - `references`, `rename` и `codeLens` используют `file_references`; для open buffer ссылки пересобираются из текущего parser state, закрытые файлы не читаются и не парсятся заново.
  - Property rename использует сохраненный `starts_with_dollar`, чтобы корректно различать declaration/static `$prop` и object `->prop`.
  - Regression: parser unit на occurrence collection, cache roundtrip для references, e2e на references из закрытого indexed file.
  - Validation: `cargo fmt --all --check`, `cargo test --all`, `cargo clippy --all-targets -- -D warnings`.

- [x] **PR-023** Перевести sync file IO в тяжелых paths на blocking/background *(completed 2026-05-22)*
  - Не делать `std::fs::read_to_string` в async handler hot path.
  - Использовать `spawn_blocking` или отдельный file IO worker для bulk reads.
  - Добавить timeout/error telemetry для медленных файловых операций.
  - Добавлен общий `run_file_io_blocking()` с `spawn_blocking`, timeout 15s и warning telemetry для операций дольше 100ms.
  - На blocking pool переведены watched-file reindex, lazy PHP/vendor indexing, vendor cache load/save, vendor autoload metadata parse, call hierarchy disk reads, `codeLens` source read и `foldingRange` source read.
  - Прямые `read_to_string` в server hot paths заменены на blocking wrappers; оставшиеся sync reads находятся в sync helper'ах, которые вызываются через blocking pool, formatter helper'е уже внутри `spawn_blocking`, и startup composer discovery.
  - Validation: `cargo fmt --all --check`, `cargo test --all`, `cargo clippy --all-targets -- -D warnings`, `git diff --check`.

### Неделя 4: PHP version, stubs, PHPDoc (2026-06-11 → 2026-06-17)

- [x] **PR-030 / H-006** Version-aware stubs *(completed 2026-05-22)*
  - Парсить/учитывать `PhpStormStubsElementAvailable` и другие version-gated атрибуты из phpstorm-stubs.
  - Фильтровать built-in symbols/signatures под `phpLsp.phpVersion`.
  - При смене версии через `didChangeConfiguration` обновлять stubs cache и diagnostics без restart.
  - E2E: API доступен в PHP 8.2 и недоступен в PHP 8.1.
  - Добавлен version-aware symbol extraction для declaration-level и parameter-level `PhpStormStubsElementAvailable`.
  - Stubs loader передает текущий `phpLsp.phpVersion` в parser и сохраняет уже отфильтрованные symbols/signatures в stubs cache.
  - Cache schema поднята до v4, чтобы не переиспользовать старые unfiltered stubs snapshots.
  - Regression: parser unit tests для symbol/parameter filtering и e2e `test_php_version_filters_version_gated_stubs` на 8.2-only sodium API.
  - Validation: `cargo fmt --all --check`, `cargo test --all`, `cargo clippy --all-targets -- -D warnings`.

- [x] **PR-031 / H-008** Переписать PHPDoc type parser *(completed 2026-05-22)*
  - Поддержать `array<int, User>`, `list<User>`, `class-string<T>`, `(A&B)|null`, `callable(A): B`.
  - Не обрезать type expression через `first_word`.
  - Безопасно игнорировать malformed tags.
  - Добавить unit tests для пробелов внутри generics и nested скобок.
  - Implemented: top-level scanner для PHPDoc type expressions, variable token lookup и method return/name split.
  - Regression: unit tests для generics с пробелами, nested generics, callable return type, callable param variable lookup и malformed tags.
  - Validation: `cargo fmt --all --check`, `cargo test -p php-lsp-parser phpdoc`, `cargo test --all`, `cargo clippy --all-targets -- -D warnings`, `git diff --check`.

- [x] **PR-032 / H-009** Разделить модель `@property`, `@property-read`, `@property-write` *(completed 2026-05-22)*
  - Добавить access mode в `PhpDocProperty`.
  - Учесть read/write режим в completion, diagnostics и future rename guards.
  - Сохранить обратную совместимость сериализации cache schema через schema version.
  - Implemented: `PhpDocPropertyAccess::{ReadWrite, ReadOnly, WriteOnly}` с readable/writable helpers.
  - Parser: `@property`, `@property-read`, `@property-write` теперь дают разные access modes; virtual member UI usage оставлен для PR-033.
  - Cache: `CACHE_SCHEMA_VERSION` поднят до 5.
  - Regression: `test_parse_property_access_modes`.
  - Validation: `cargo fmt --all --check`, `cargo test -p php-lsp-parser phpdoc`, `cargo test --all`, `cargo clippy --all-targets -- -D warnings`, `git diff --check`.

- [x] **PR-033 / H-010 / H-012** PHPDoc virtual members в LSP UI *(completed 2026-05-22)*
  - Показывать `@throws`, `@var`, `@property*`, `@method` в hover и completion resolve.
  - Добавить virtual properties/methods в `$obj->` completion.
  - Definition по virtual member возвращает range в doc-comment.
  - Rename для virtual members: явно запретить или реализовать локальный doc-comment edit.
  - Implemented: class/member hover и completion resolve показывают `@throws`, `@var`, `@property*`, `@method`.
  - Completion: `$obj->` включает inherited PHPDoc virtual properties/methods с metadata для resolve.
  - Definition: unresolved PHPDoc virtual member ведет на имя в doc-comment tag.
  - Rename: PHPDoc virtual members явно запрещены в `rename`/`prepareRename`.
  - Regression: completion unit tests для direct/inherited virtual members; server unit tests для markdown sections и doc-comment range.
  - Validation: `cargo fmt --all --check`, targeted `phpdoc_virtual`/`phpdoc_extra` tests, `cargo test --all`, `cargo clippy --all-targets -- -D warnings`, `git diff --check`.

- [x] **PR-034 / H-013** E2E покрытие PHPDoc behavior *(completed 2026-05-22)*
  - Fixture-driven tests по `test-fixtures/lsp-cases/src/PhpDoc/*`.
  - Отдельно проверить hover, completionItem/resolve, definition, diagnostics no-crash.
  - Обновить `test-fixtures/lsp-cases/README.md` фактической матрицей поддержки.
  - Added: `VirtualMembers.php` fixture для usage sites class-level PHPDoc virtual members.
  - E2E: hover по class/method/virtual property, `$obj->` completion, `completionItem/resolve`, definition на doc-comment tag, rename guard, diagnostics no-crash для `EdgeCases.php`.
  - Docs: README обновлен PHPDoc behavior matrix.
  - Validation: `cargo fmt --all --check`, targeted `test_phpdoc_fixture_hover_completion_definition_and_diagnostics`, `cargo test --all`, `cargo clippy --all-targets -- -D warnings`, `git diff --check`.

### Неделя 5: Type engine и LSP polish (2026-06-18 → 2026-06-24)

- [x] **PR-040** Расширить `TypeInfo` для production PHPDoc/PHP типов *(completed 2026-05-22)*
  - Generic types, array/list shapes best-effort, callable signatures, class-string, literal scalar types.
  - Нормализация FQN внутри типов с учетом namespace/use.
  - Сохранить graceful fallback для неизвестных type forms.
  - Implemented: `Generic`, `ArrayShape`, `Callable`, `ClassString`, literal string/int/float/bool/null variants with stable `Display`.
  - Parser: PHPDoc generics, nested generics, array shapes, callable signatures, class-string and literal scalar types now produce structured `TypeInfo`.
  - Consumers: type definition, member return resolution, override normalization, return-type code actions and diagnostics handle new variants with conservative fallback.
  - Cache: `CACHE_SCHEMA_VERSION` поднят до 6.
  - Regression: parser tests for generic/class-string/callable/array-shape/literals and `TypeInfo` display tests.
  - Validation: `cargo fmt --all --check`, targeted PHPDoc/type display tests, `cargo test --all`, `cargo clippy --all-targets -- -D warnings`, `git diff --check`.

- [x] **PR-041** Улучшить inference для переменных и expressions *(completed 2026-05-22)*
  - Возврат методов через PHPDoc generics, iterable foreach value type, array access best-effort.
  - `instanceof` narrowing для positive/negative branches.
  - Типы свойств из constructor promotion, assignments и `@var` объединять с приоритетами.
  - Implemented: `VariableInference` сохраняет structured `TypeInfo`, чтобы использовать PHPDoc generics/array-shapes не только как display string.
  - Implemented: `foreach ($items as $item)` выводит тип `$item` из `array<TKey, TValue>`, `list<T>`, `iterable<TKey, TValue>`, collection-like generics.
  - Implemented: `$users[0]->...` и `$row['user']->...` резолвятся через generic element type / array-shape key; completion context сохраняет `$users[0]` и `$repo->findAll()[0]` как object expression.
  - Implemented: method return `@return array<int, User>` участвует в array-access inference без ложного класса `array<int, User>`.
  - Implemented: positive `if ($x instanceof Foo) { ... }` narrowing; negative guard narrowing с early exit сохранен regression-тестом.
  - Implemented: property `@var` становится property type source, если native/promoted type отсутствует; native/promoted type имеет приоритет, assignment fallback остается запасным путем.
  - Regression: parser tests для foreach generic value, generic method return array access, array shape access, completion-style `$users[0]`, positive/negative `instanceof`, property `@var`; completion context tests для array access object expr.
  - Validation: `cargo fmt --all --check`, `cargo test --all`, `cargo clippy --all-targets -- -D warnings`, `git diff --check`.

- [x] **PR-042** Снизить false positives diagnostics на framework-heavy коде *(completed 2026-05-22)*
  - Добавить regression corpus для Symfony/Laravel/PHPUnit patterns без project-specific hardcode.
  - Пересмотреть suppressions: они должны опираться на типы/наследование/known library metadata, а не имена конкретного проекта.
  - Ввести severity/category controls для noisy diagnostics.
  - Implemented: `phpLsp.diagnostics.severity` с категориями `unknownSymbols`, `unused`, `duplicateSymbols`, `members`, `typeCompatibility`, `overrideSignatures`, `phpVersion`; значения `off/error/warning/information/hint` применяются через initializationOptions и `didChangeConfiguration`.
  - Implemented: внутренние `php-lsp` diagnostics теперь получают category/code metadata и могут быть выключены/понижены по категории без отключения всего semantic mode.
  - Implemented: framework suppressions опираются на наследование/known library metadata: Symfony `AbstractController` helpers, Laravel Eloquent Model/Builder dynamic members, Doctrine repository descendants, классы с неиндексированным ancestor.
  - Regression: добавлен fixture `test-fixtures/lsp-cases/src/Diagnostics/FrameworkNoFalsePositive.php` и server tests для Symfony/Laravel patterns плюс severity controls.
  - Validation: `cargo fmt --all --check`, targeted `cargo test -p php-lsp-server compute_diagnostics_`, `cargo test --all`, `cargo clippy --all-targets -- -D warnings`, `npm run lint`, `npm run build`, `git diff --check`.

- [x] **PR-043** Закрыть LSP polish gaps *(completed 2026-05-22)*
  - Добавить `textDocument/semanticTokens/range` или явно документировать отсутствие.
  - Улучшить `workspace/symbol`: fuzzy scoring, container ranking, kind filters where possible.
  - Реализовать meaningful `willRenameFiles` для namespace/class path refactors или убрать завышенное ожидание из capabilities.
  - Проверить корректность UTF-16 ranges во всех новых edits.
  - Implemented: `textDocument/semanticTokens/range` capability и handler поверх существующего extractor; range response фильтрует absolute tokens и заново кодирует LSP relative token stream.
  - Implemented: `workspace/symbol` ищет по всем indexed file symbols, включая members, использует fuzzy scoring, container/FQN ranking и query kind filters (`class:`, `method:`, `function:`, `property:`, `const:` и т.п.).
  - Implemented: `workspace/symbol` конвертирует indexed byte ranges в UTF-16 ranges по open buffer или blocking file read fallback.
  - Implemented: `willRenameFiles` больше не advertised, пока сервер не возвращает meaningful namespace/class path refactor edits; `didRenameFiles` остается активным для index URI updates.
  - Regression: e2e для `semanticTokens/range`, initialize capabilities; unit tests для workspace symbol ranking/filtering и UTF-16 range conversion.
  - Validation: `cargo fmt --all --check`, targeted `workspace_symbol`/`semantic_tokens_range`/`test_initialize_and_shutdown`, `cargo test --all`, `cargo clippy --all-targets -- -D warnings`, `git diff --check`.

### Неделя 6: Release hardening, docs, acceptance (2026-06-25 → 2026-07-01)

- [x] **PR-050** Stress и soak testing *(completed 2026-05-25)*
  - 100 didChange за 1 секунду на файле с non-ASCII.
  - Одновременные hover/completion во время full indexing.
  - Cancel references/rename на большом workspace.
  - External analyzer timeout и malformed JSON без зависаний.
  - Implemented: e2e stress `test_stress_100_did_change_non_ascii_publishes_latest_version` — 100 full `didChange` events с кириллицей принимаются за 1 секунду, stale diagnostics не публикуются, финальная версия чистая.
  - Implemented: e2e responsiveness `test_hover_and_completion_respond_while_workspace_indexing_runs` — hover/completion по open buffer отвечают во время фоновой workspace indexing.
  - Implemented: e2e cancellation `test_cancel_request_cancels_rename_request`; существующий references cancellation покрывает вторую тяжелую операцию.
  - Implemented: analyzer soak unit tests для PHPStan/Psalm timeout и malformed JSON, обернутые в `tokio::time::timeout`, чтобы фиксировать отсутствие зависаний.
  - Validation: targeted stress/cancel/analyzer tests, `cargo fmt --all --check`, `cargo test --all`, `cargo clippy --all-targets -- -D warnings`, `git diff --check`.

- [x] **PR-051** Release workflow production-ready *(completed 2026-05-25)*
  - Синхронизировать `.github/workflows/release.yml` со всеми заявленными платформами.
  - Добавить `darwin-x64`, проверить linux musl/alpine решение или убрать из обещаний.
  - Включить Marketplace publish job за `VSCE_PAT`.
  - Добавить smoke test packaged VSIX: binary exists, stubs exist, extension activates.
  - Implemented: release matrix теперь включает `darwin-x64` на Intel `macos-15-intel`; `darwin-arm64` остается на arm64 `macos-14`.
  - Implemented: workflow_dispatch checkout/release uses requested tag, чтобы ручной release не публиковал default branch вместо tag.
  - Implemented: Marketplace publish gated by `VSCE_PAT`; без секрета job делает явный skip notice и не валит GitHub release.
  - Implemented: `scripts/smoke-vsix.sh` проверяет packaged VSIX: package metadata, extension bundle load/activation entrypoint, all platform binaries, bundled stubs, README/LICENSE.
  - Implemented: Alpine/musl убран из documented published target set; Linux release binaries documented как GNU/glibc (`*-unknown-linux-gnu`).
  - Validation: `actionlint` через `go run github.com/rhysd/actionlint/cmd/actionlint@latest .github/workflows/release.yml`, YAML syntax parse, `bash -n` для shell scripts, `npm run lint`, `npm run build`, local VSIX package через `npx @vscode/vsce package --no-dependencies -o /tmp/ht-php-lsp-local-smoke.vsix`, smoke test на реальном `linux-x64` VSIX, synthetic universal VSIX smoke, `git diff --check`.

- [x] **PR-052** Документация production-ready *(completed 2026-05-25)*
  - `docs/architecture.md`: data flow, indexing/cache model, diagnostics pipeline.
  - `docs/lsp-features.md`: таблица supported/partial/unsupported.
  - `docs/performance.md`: baseline, методика измерений, known bottlenecks.
  - README: установка, настройки, troubleshooting, external analyzers, formatter setup.
  - Implemented: `docs/architecture.md` описывает components, startup flow, workspace roots, open-file lifecycle, symbol index, cache namespaces, indexing, stubs/vendor, diagnostics, runtime config, cache clearing.
  - Implemented: `docs/lsp-features.md` фиксирует supported/partial/unsupported LSP capabilities и ограничения для heavy requests, formatter, file operations, hierarchies, semantic tokens.
  - Implemented: `docs/performance.md` описывает baseline sources, profiling/latency commands, cache interpretation, package smoke, acceptance targets and validation commands.
  - Implemented: README получил documentation links и troubleshooting для server startup, stale/slow indexing, noisy diagnostics, PHPStan/Psalm, formatting.
  - Validation: docs file existence checks, README/TASKS link references via `rg`, `git diff --check`.

- [x] **PR-053** Финальный acceptance прогон *(completed 2026-05-25)*
  - Все unit/e2e tests проходят.
  - Perf numbers внесены в docs.
  - Known limitations актуальны.
  - `TASKS.md` обновлен: завершенные PR tasks отмечены, оставшиеся перенесены в следующий milestone.
  - Implemented: acceptance refresh added to `docs/production-baseline.md` with current validation commands and Rust test breakdown (309 tests).
  - Implemented: `docs/production-risk-register.md` updated after PR-020..PR-052 so known limitations match current debounce, cancellation, reference index, version-aware stubs, PHPDoc/type model, and LSP polish state.
  - Implemented: old duplicate H/V documentation tasks marked done; remaining real Laravel profiling task moved to Next Milestone Backlog.
  - Validation: `cargo fmt --all --check`, `cargo test --all`, `cargo clippy --all-targets -- -D warnings`, `npm run lint`, `npm run build`, `actionlint` for CI/release workflows, `bash -n` for release/profile shell scripts, docs/link presence checks, stale-limitation search, `git diff --check`.

### Next Milestone Backlog

- [ ] **V1-010** Профилирование на Laravel проекте *(moved from vNext 2026-05-25; split into `PV-001`/`PV-002`)*
  - Замер: время индексации, память, latency hover/completion.
  - Оптимизация bottleneck'ов.
  - Нужен реальный Laravel workspace path; текущий acceptance покрывает fixture/profile harness и общие LSP checks.

## Milestone: Production Validation & GA Hardening (2 недели)

**Срок:** 2026-05-25 → 2026-06-07
**Цель:** перевести проект из "release candidate / beta" в честный production-ready LSP для крупных PHP/Composer workspace. Этот milestone не про добавление новых LSP фич, а про доказательство производительности, устойчивости и качества на реальных проектах, фиксацию обнаруженных bottleneck'ов и обновление публичных production claims.

### Exit criteria

- Есть минимум один реальный Laravel или Symfony workspace с 5k-10k PHP files, на котором прогнаны профилирование, latency benchmark, heavy request benchmark и dogfood сценарии.
- Warm start из disk cache до `phpLsp/indexingStatus phase=ready`: `< 5s` на выбранном large workspace.
- Warm p95 для `hover`, `completion`, `definition`: `< 50ms` на выбранном large workspace.
- `references`, `rename`, `codeLens` не блокируют параллельные `hover`/`completion`; если они остаются дорогими, есть измеренный лимит, mitigation task и честная документация.
- Peak RSS на large workspace измерен и не показывает неконтролируемый рост между cold/warm runs.
- 100 `didChange` за 1 секунду, external analyzer timeout/malformed JSON и cancel references/rename остаются covered tests.
- `docs/production-baseline.md`, `docs/production-risk-register.md`, `docs/performance.md`, `README.md`, `docs/lsp-features.md` обновлены по фактическим результатам.
- Перед релизом проходят: `cargo fmt --all --check`, `cargo test --all`, `cargo clippy --all-targets -- -D warnings`, `npm run lint`, `npm run build`, `actionlint`, `bash -n scripts/...`, `git diff --check`.

### Неделя 1: измерения на реальном проекте и фиксация bottleneck'ов (2026-05-25 → 2026-05-31)

- [x] **PV-001 / V1-010** Подготовить large PHP workspace для acceptance *(completed 2026-05-25)*
  - Найти локальный реальный проект Laravel или Symfony с 5k-10k PHP files. Если такого нет, выбрать ближайший большой Composer workspace и зафиксировать отклонение.
  - Known local test workspaces:
    - `large-laravel-crm`: `/home/apanov/ForTesting/laravel-crm/`
    - `large-symfony`: `/home/apanov/ForTesting/symfony`
    - `large-monica`: `/home/apanov/ForTesting/monica`
  - Записать абсолютный путь workspace в локальные run notes, но не коммитить приватные пути/данные в документацию. В docs писать anonymized label: `large-laravel`, `large-symfony`, `large-composer`.
  - Проверить, что workspace открывается без destructive действий: не запускать composer install/update, миграции, тесты проекта или форматтеры проекта без отдельного явного решения.
  - Зафиксировать счетчики: количество PHP files в project source, vendor files, общий размер workspace, наличие `composer.json`, `composer.lock`, `vendor/composer/installed.json`.
  - Если workspace содержит generated/cache dirs, настроить `phpLsp.excludePaths` для профиля и документировать список excludes.
  - Validation:
    - `find <workspace> -name '*.php' | wc -l` или эквивалентный script result.
    - `scripts/profile-workspace.sh --scenario large=<workspace> --timeout 300` стартует и пишет JSON.
  - Result 2026-05-25:
    - `large-laravel-crm`: 904 PHP files, 866 indexed files, 2401 symbols, 99M workspace, `composer.json`/`composer.lock` yes, no `vendor/composer/installed.json`, profile pass, ready 467.61 ms, peak RSS 76,668,928 bytes.
    - `large-symfony`: 10631 PHP files, 10575 indexed files, 72683 symbols, 482M workspace, `composer.json` yes, no `composer.lock`, no `vendor/composer/installed.json`, profile pass, ready 7460.54 ms, peak RSS 728,272,896 bytes. Primary large acceptance candidate.
    - `large-monica`: 1656 PHP files, 1330 indexed files, 7163 symbols, 169M workspace, `composer.json`/`composer.lock` yes, no `vendor/composer/installed.json`, profile pass, ready 620.68 ms, peak RSS 107,020,288 bytes.
    - Profile JSON outputs: `target/php-lsp-profile/large-laravel-crm.json`, `target/php-lsp-profile/large-symfony.json`, `target/php-lsp-profile/large-monica.json`.

- [x] **PV-002** Снять cold/warm indexing и cache baseline на large workspace *(completed 2026-05-25)*
  - Использовать isolated cache, чтобы измерения были воспроизводимыми:
    - `rm -rf target/php-lsp-profile/large-cache`
    - `XDG_CACHE_HOME="$PWD/target/php-lsp-profile/large-cache" scripts/profile-workspace.sh --scenario large=<workspace> --timeout 300`
    - повторить ту же команду второй раз для warm cache.
  - Снять и внести в `docs/production-baseline.md`:
    - time to `phpLsp/indexingStatus phase=ready`
    - indexed files, symbols, stubs files
    - cache files loaded/stale/missing
    - stubs load time
    - files/sec, symbols/sec если есть в JSON
    - peak RSS
    - cache path namespace stats: `workspace`, `stubs`, `vendor`
  - Сравнить warm start с exit criteria `< 5s`.
  - Если warm start не проходит:
    - найти top bottleneck по JSON/logs
    - создать follow-up `PV-FIX-*` task в этом milestone
    - не править вслепую без измерения до/после.
  - Validation:
    - JSON артефакты есть в `target/php-lsp-profile/`.
    - `docs/production-baseline.md` содержит large workspace cold/warm таблицу.
    - `docs/production-risk-register.md` обновлен по `R-001`, `R-003`, `R-008`.
  - Result 2026-05-25:
    - Isolated cache run on `large-symfony` completed.
    - Cold: 10575 indexed files, 72683 symbols, ready 7349.48 ms, stubs load 313.73 ms, peak RSS 730,419,200 bytes.
    - Warm: 10575 cache files loaded, 0 missing/stale, ready 3423.19 ms, stubs load 33.79 ms, peak RSS 625,729,536 bytes.
    - Warm start meets `<5s` large-workspace target.
    - Cache artifacts: `workspace/index.bin` 141,725,060 bytes; `stubs/index.bin` 4,351,427 bytes.
    - Updated `docs/production-baseline.md` and `docs/production-risk-register.md`.

- [x] **PV-003** Снять latency benchmark на real large workspace *(completed 2026-05-25)*
  - Запустить:
    - `scripts/benchmark-lsp-latency.sh --iterations 20 --timeout 300 --scenario large=<workspace>`
  - Проверить оба состояния benchmark, если script их поддерживает: `open` и `unopened`.
  - Внести в `docs/production-baseline.md` p50/p95/p99 для:
    - `textDocument/hover`
    - `textDocument/completion`
    - `textDocument/definition`
    - `textDocument/references`
    - rename dry-run
  - Сравнить warm p95 `hover/completion/definition` с `< 50ms`.
  - Если common request p95 не проходит:
    - локализовать path: parser/open buffer, index lookup, lazy vendor hit, disk read fallback, UTF-16 conversion, completion provider.
    - создать конкретный `PV-FIX-*` task с файлом/модулем и measured before/after target.
  - Validation:
    - `target/php-lsp-profile/*-latency.json` обновлен.
    - `docs/performance.md` и `docs/production-baseline.md` отражают real large numbers.
    - `docs/production-risk-register.md` обновлен по `R-002`, `R-004`, `R-005`.
  - Result 2026-05-25:
    - `scripts/benchmark-lsp-latency.sh --iterations 20 --timeout 300 --scenario large-symfony=<workspace>` passed.
    - Output: `target/php-lsp-profile/large-symfony-latency.json`.
    - Warm open p95: hover 3.562 ms, completion 6.556 ms, definition 2.855 ms.
    - Warm unopened p95: hover 0.206 ms, completion 0.248 ms, definition 0.338 ms.
    - Warm open heavy requests: references p95 72.527 ms, rename dry-run p95 73.529 ms.
    - Common interactive latency meets `<50ms` production target; heavy request concurrency remains for `PV-004`.
    - Updated `docs/production-baseline.md`, `docs/performance.md`, and `docs/production-risk-register.md`.

- [x] **PV-004** Проверить heavy requests под нагрузкой *(completed 2026-05-25)*
  - Цель: доказать, что `references`, `rename`, `codeLens`, incoming call hierarchy и file-operation refresh не блокируют быстрые `hover/completion`.
  - Добавить или расширить benchmark/script, если текущего `benchmark-lsp-latency.sh` недостаточно:
    - открыть PHP file из large workspace
    - запустить долгий `references` или rename dry-run
    - параллельно посылать `hover`/`completion` каждые 100-250ms
    - измерить p95/p99 быстрых ответов во время heavy request
  - Проверить `$/cancelRequest` для large `references` и `rename`, а не только fixture.
  - Если `codeLens` дорогой:
    - измерить число symbols в документе и reference-count latency
    - рассмотреть batch/count cache или lazy codelens resolve task.
  - Validation:
    - benchmark выводит отдельные цифры "while heavy request is running".
    - `docs/production-baseline.md` содержит таблицу heavy request responsiveness.
    - Если есть bottleneck, добавлен `PV-FIX-*` task с target p95/p99.
  - Implemented:
    - Added `--heavy-responsiveness` mode to `scripts/benchmark-lsp-latency.sh` / `.py`.
    - Added response buffering in the benchmark LSP client so out-of-order heavy/fast responses are not lost.
    - Heavy mode measures hover/completion while `references` or rename dry-run is outstanding and then checks `$/cancelRequest` for both heavy request types.
  - Result 2026-05-25:
    - `scripts/benchmark-lsp-latency.sh --heavy-responsiveness --iterations 20 --timeout 300 --scenario large-symfony=<workspace>` passed.
    - Output: `target/php-lsp-profile/large-symfony-heavy-responsiveness.json`.
    - While `references` outstanding: hover p95 6.067 ms, completion p95 5.851 ms.
    - While rename dry-run outstanding: hover p95 5.755 ms, completion p95 6.390 ms.
    - Normal heavy p95: references 76.329 ms, rename dry-run 82.806 ms.
    - Cancellation: references 20/20 cancelled, rename dry-run 20/20 cancelled; cancel p95 around 2.1-2.2 ms.
    - No `PV-FIX-*` task needed from this run; `codeLens` remains a dogfood watch item.

### Неделя 2: исправления, dogfooding, релизная готовность (2026-06-01 → 2026-06-07)

- [x] **PV-010** Закрыть measured bottlenecks из `PV-002`-`PV-004` *(completed 2026-05-25)*
  - Брать только bottleneck, подтвержденный измерением на large workspace или stress benchmark.
  - Для каждого исправления:
    - добавить regression/perf test или benchmark scenario
    - сохранить before/after числа в `docs/production-baseline.md`
    - обновить соответствующий риск в `docs/production-risk-register.md`
  - Возможные направления, если измерения подтвердят проблему:
    - reference count aggregation для `codeLens`
    - inverted reference index by FQN/member key для `references`/`rename`
    - дополнительное sharding/batching для `file_references`
    - устранение disk read fallback из hot request path
    - tuning vendor LRU/cache preload
  - Validation:
    - targeted benchmark показывает улучшение или документированное acceptable tradeoff.
    - `cargo test --all` и `cargo clippy --all-targets -- -D warnings` проходят после code changes.
  - Result 2026-05-25:
    - `PV-002`: warm cache ready 3423.19 ms on `large-symfony`, below `<5s` target.
    - `PV-003`: common warm p95 on `large-symfony` below `<50ms` target.
    - `PV-004`: hover/completion stay below ~6.4 ms p95 while references/rename are outstanding; cancellation succeeds 20/20.
    - No measured bottleneck from `PV-002`-`PV-004` requires a `PV-FIX-*` code task. Heavy references/rename remain documented as more expensive but acceptable for this acceptance pass.
    - Benchmark tooling was extended and validated with `python3 -m py_compile scripts/benchmark-lsp-latency.py`, `bash -n scripts/benchmark-lsp-latency.sh`, and large-workspace benchmark runs.

- [ ] **PV-011** Провести dogfood через packaged VSIX *(partial 2026-05-25; GUI dogfood pending)*
  - Собрать package:
    - `./scripts/build-server.sh`
    - `./scripts/bundle-stubs.sh`
    - `cd client && npm ci && npm run build && npx @vscode/vsce package --no-dependencies`
  - Проверить package:
    - `scripts/smoke-vsix.sh <path-to-vsix>`
    - установить VSIX в VS Code
    - открыть large workspace
    - проверить status popup, indexing status, clear cache command, restart command.
  - Dogfood checklist:
    - open/edit/save PHP file with non-ASCII text
    - hover/completion/definition on project symbol
    - references/rename on class/method/property
    - codeAction add use / organize imports on safe fixture or disposable branch
    - PHPStan/Psalm disabled and enabled modes, если tools есть в workspace
    - `PHP: Clear PHP LSP Cache and Restart` реально очищает cache dirs для workspace roots.
  - Validation:
    - `scripts/smoke-vsix.sh` passed.
    - В `docs/production-baseline.md` есть dogfood notes: VSIX name/version, workspace label, pass/fail summary, known issues.
  - Partial result 2026-05-25:
    - Host package built at `/tmp/ht-php-lsp-pv011.vsix` (3.99M).
    - `PHP_LSP_VSIX_PLATFORMS=linux-x64 scripts/smoke-vsix.sh /tmp/ht-php-lsp-pv011.vsix` passed.
    - `code --install-extension /tmp/ht-php-lsp-pv011.vsix --force` passed; `code --list-extensions` shows `hightemp.ht-php-lsp`.
    - Default universal smoke was not applicable to this host-only local package; universal six-platform package smoke remains covered by release workflow.
    - `npm ci` reported audit findings: 2 moderate, 1 high. Build/package passed; audit review remains a GA policy question.
    - Interactive GUI dogfood checks for status popup, command palette behavior, and opening large workspace are still pending.

- [x] **PV-012** Стабилизировать diagnostics false positives на real large workspace *(completed 2026-05-25)*
  - Прогнать diagnostics audit script или LSP client по выборке файлов из large workspace:
    - framework controllers/services/models/tests
    - files with PHPDoc generics/array shapes/callables
    - files with vendor-heavy types
  - Классифицировать diagnostics:
    - true positive
    - acceptable limitation
    - false positive
    - uncertain because project metadata missing
  - Для false positives:
    - добавить minimal fixture under `test-fixtures/lsp-cases/src/Diagnostics/`
    - исправить без project-specific hardcode
    - добавить unit/e2e regression.
  - Current fix 2026-05-25:
    - investigate Symfony `MapEntity::withDefaults(self $defaults, ...)` diagnostics.
    - fix variable type inference so `self`/`static` typed parameters resolve to the enclosing class before member diagnostics.
    - add regression coverage for promoted constructor properties accessed through a `self`-typed parameter.
  - Для acceptable limitations:
    - обновить `README.md` Known Limitations или `docs/lsp-features.md`.
  - Validation:
    - diagnostics audit summary добавлен в `docs/production-baseline.md`.
    - Regression tests added for fixed false positives.
  - Result 2026-05-25:
    - Release diagnostics samples:
      - `large-symfony-diagnostics-sample`: 500 files, 2800 diagnostics, 0 missing diagnostics, 0 request/stderr errors.
      - `large-laravel-crm-diagnostics-sample`: 500 files, 1006 diagnostics, 0 missing diagnostics, 0 request/stderr errors.
      - `large-monica-diagnostics-sample`: 500 files, 2124 diagnostics, 0 missing diagnostics, 0 request/stderr errors.
    - Top sample diagnostics are mostly classified as missing Composer/vendor metadata (`PHPUnit`, `Twig`, `Doctrine`, `Illuminate`, `Carbon`, `Sabre`, `Inertia`) or accepted dynamic framework limits (for example Eloquent relation members).
    - Fixed one confirmed real-project false positive: `self`/`static` typed parameters now resolve to the enclosing class before member diagnostics, so promoted constructor properties accessed via `withDefaults(self $defaults)` no longer produce unknown-property diagnostics.
    - Added regression coverage:
      - `test-fixtures/lsp-cases/src/Diagnostics/PromotedSelfDefaults.php`
      - `php-lsp-parser::resolve::tests::test_resolve_property_access_on_self_typed_parameter`
      - `php-lsp-server::server::tests::test_compute_diagnostics_allows_promoted_properties_on_self_typed_parameter`
    - Release targeted audit:
      - `fixture-promoted-self-diagnostics-release`: 1 file, 0 diagnostics.
      - `large-symfony-mapentity-diagnostics-release`: real Symfony `MapEntity.php`, 0 diagnostics.
    - Validation commands passed:
      - `cargo fmt --all --check`
      - `cargo test --all`
      - `cargo clippy --all-targets -- -D warnings`
      - `cargo build --release -p php-lsp-server`
      - `./scripts/build-server.sh`
      - `git diff --check`
    - Copied host VS Code binary was rechecked through `client/bin/linux-x64/php-lsp` on the new fixture and real Symfony `MapEntity.php`; both published 0 diagnostics.
    - Documentation updated in `docs/production-baseline.md`, `docs/production-risk-register.md`, `docs/lsp-features.md`, and `README.md`.

- [x] **PV-013** Закрыть или честно переоценить risk register *(completed 2026-05-25)*
  - Для каждого риска `R-001`-`R-010` в `docs/production-risk-register.md`:
    - обновить current evidence по результатам large workspace measurements
    - поменять status на `Mitigated`, `Accepted limitation`, или оставить `Partially mitigated` с новым owner task
    - для `High` risks нельзя оставлять `Partially mitigated` без конкретной следующей задачи и измеримого exit signal.
  - Обновить `README.md` Known Limitations:
    - если large validation прошла, убрать "Production hardening is still in progress" или заменить на точную формулировку.
    - если не прошла, явно написать какие thresholds не выполнены.
  - Обновить `docs/lsp-features.md`, если capability behavior изменился.
  - Validation:
    - `rg -n "Partially mitigated|Production hardening is still in progress|Large-project acceptance thresholds are still" README.md docs/production-risk-register.md docs/lsp-features.md` проверен и результат осознанно оставлен или устранен.
  - Result 2026-05-25:
    - High risks no longer remain `Partially mitigated`:
      - `R-001` disk cache maturity -> `Mitigated`
      - `R-002` references/rename/codeLens scale -> `Accepted limitation`
      - `R-003` parallel indexing acceptance -> `Mitigated`
      - `R-004` sync file IO in async/hot paths -> `Mitigated`
      - `R-005` heavy-request cancellation -> `Mitigated`
    - `R-008` remains `Partially mitigated` intentionally because installed-vendor first-hit scale still needs `PV-014`/GA acceptance evidence.
    - `R-009` is now `Accepted limitation` after the `PV-012` diagnostics audit and `self`/promoted-property false-positive fix.
    - README Known Limitations no longer says production hardening is generally in progress or that large-project acceptance thresholds are still unmeasured.
    - Validation query now only returns `R-008`, which is intentional and Medium severity.
  - Docs language cleanup 2026-05-25 *(completed)*:
    - keep `docs/` and `README.md` in English.
    - `rg -n "[А-Яа-яЁё]" docs README.md` returns no matches.

- [ ] **PV-014** Финальный GA acceptance прогон
  - Запустить:
    - `cd server && cargo fmt --all --check`
    - `cd server && cargo test --all`
    - `cd server && cargo clippy --all-targets -- -D warnings`
    - `cd client && npm run lint`
    - `cd client && npm run build`
    - `go run github.com/rhysd/actionlint/cmd/actionlint@latest .github/workflows/ci.yml .github/workflows/release.yml`
    - `bash -n scripts/build-server.sh scripts/bundle-stubs.sh scripts/profile-workspace.sh scripts/benchmark-lsp-latency.sh scripts/smoke-vsix.sh`
    - `git diff --check`
  - Проверить release package smoke на свежем VSIX.
  - Обновить `docs/production-baseline.md` финальным "GA Acceptance" блоком:
    - дата
    - git revision
    - version
    - команды
    - test breakdown
    - large workspace acceptance summary
  - Если все exit criteria выполнены:
    - отметить milestone tasks done
    - добавить релизную задачу на bump/publish.
  - Если нет:
    - не называть проект production ready; оставить `0.x beta/RC` и создать следующий measured milestone.

### Production Validation Dependencies

```
PV-001 ─→ PV-002 ─→ PV-003 ─→ PV-004
PV-002 ─→ PV-010
PV-003 ─→ PV-010
PV-004 ─→ PV-010
PV-010 ─→ PV-011 ─→ PV-012 ─→ PV-013 ─→ PV-014
PV-002 ─→ PV-013
PV-003 ─→ PV-013
PV-004 ─→ PV-013
```

### Production Readiness Dependencies

```
PR-001 ─→ PR-002 ─→ PR-010 ─→ PR-013
PR-002 ─→ PR-003 ─→ PR-020 ─→ PR-021 ─→ PR-022
PR-010 ─→ PR-011 ─→ PR-012
PR-030 ─→ PR-033
PR-031 ─→ PR-032 ─→ PR-033 ─→ PR-034
PR-040 ─→ PR-041 ─→ PR-042
PR-022 ─→ PR-043
PR-050 ─→ PR-051 ─→ PR-053
PR-052 ─→ PR-053
```

---

## Milestone: IDE Intelligence & Tooling Expansion (5 недель)

**Срок:** после закрытия `PV-014`, ориентир 2026-06-08 → 2026-07-12

**Цель:** после GA-стабилизации расширить сервер от "production-ready PHP LSP" к более сильному IDE-инструменту: больше практичных code actions, CLI/tooling режимы, проектная конфигурация, более глубокая типизация PHPDoc/PHPStan/Psalm и framework-aware интеллект без ухудшения latency/false-positive baseline.

### Scope rules

- Не начинать этот milestone до финального acceptance `PV-014`, если задача не является маленькой независимой UX-фичей.
- Не добавлять project-specific hardcode. Любая framework-aware логика должна быть оформлена как общий provider/adapter слой с тестовыми фикстурами.
- Все новые дорогие операции должны иметь cancellation/debounce или lazy resolve.
- Все новые публичные claims должны попадать в docs на английском: `README.md`, `docs/lsp-features.md`, `docs/architecture.md`, `docs/performance.md`.
- Для каждой фичи: unit/e2e tests, regression fixture, `cargo fmt --all --check`, targeted tests, затем `cargo test --all` и `cargo clippy --all-targets -- -D warnings`.
- После изменений клиента: `npm run lint`, `npm run build`, VSIX smoke если затронута упаковка/activation/lifecycle.

### Exit criteria

- `textDocument/documentLink` поддержан для `include`, `include_once`, `require`, `require_once`.
- `codeAction/resolve` внедрен для тяжелых code actions; обычный `textDocument/codeAction` остается быстрым на больших файлах.
- Есть минимум 8 новых production-useful code actions с тестами и documented behavior.
- Есть проектная конфигурация `.php-lsp.toml` с JSON schema и командой инициализации.
- Есть CLI `analyze` для batch diagnostics с table/json/github output и корректными exit codes.
- Type engine умеет template params, template bindings, type aliases, conditional return types и shape-aware completion минимум в best-effort режиме.
- Framework-aware слой реализован без bootstrapping приложения и без чтения базы данных.
- Laravel/Eloquent-like patterns покрыты фикстурами: relations, scopes, casts, accessors, builders, magic properties/methods.
- Blade-like templates либо поддержаны через virtual PHP + source map, либо явно оставлены как отдельный follow-up с documented limitation.
- На large workspaces из `PV-*` warm p95 `hover/completion/definition` остается `< 50ms`, а heavy/codeAction latency не блокирует быстрые запросы.

### Неделя 1: low-risk UX, protocol polish, конфигурация (2026-06-08 → 2026-06-14)

- [x] **IE-001** Добавить `textDocument/documentLink` для PHP include/require paths *(completed 2026-05-25)*
  - Реализовать capability `document_link_provider`.
  - AST-based найти `include`, `include_once`, `require`, `require_once`.
  - Поддержать только безопасные статические пути: string literal, `__DIR__ . '/file.php'`, `dirname(__FILE__) . '/file.php'` best-effort.
  - Резолвить относительно текущего файла и workspace root; не выполнять PHP-код.
  - Возвращать `DocumentLink` только если target exists.
  - Добавить e2e: valid include, missing include ignored, nested include inside function/class method.
  - Обновить `docs/lsp-features.md` на английском.
  - Implemented: server advertises `documentLinkProvider` without resolve; handler works for open and unopened PHP files.
  - Implemented: static path resolver supports string literals, concatenation, `__DIR__`, `__FILE__`, and `dirname(__FILE__)`.
  - Regression: e2e covers existing `require`, `include_once`, nested `require_once`, and ignored missing include path.
  - Docs: README feature list and `docs/lsp-features.md` updated in English.
  - Validation: `cargo fmt --all --check`, `cargo test -p php-lsp-server`, `cargo test --all`, `cargo clippy --all-targets -- -D warnings`, docs Cyrillic check for `docs/ README.md`, `git diff --check`.

- [x] **IE-002** Добавить команды версии и server diagnostics в VS Code client *(completed 2026-05-25)*
  - Команда `PHP: Show Language Server Version`.
  - Показывать `server_info.version`, resolved binary path, platform target, stubs path, cache root.
  - Добавить пункт в status quick pick.
  - Если сервер не стартовал, команда должна показывать last known binary resolution error.
  - Client tests или lightweight script validation для command registration.
  - Обновить README commands table на английском.
  - Implemented: new `phpLsp.showServerVersion` VS Code command with initialized server name/version, extension version, binary source/path, platform target, stubs path, cache roots, and last binary/start errors.
  - Implemented: status quick pick now includes a `Server version` action and the status tooltip shows the initialized server version when available.
  - Implemented: binary resolution now records missing custom/bundled binary errors before server startup fails.
  - Regression: added `npm run check:commands` lightweight validation for command contribution/registration drift.
  - Docs: root README commands table updated in English.
  - Validation: `npm run lint`, `npm run check:commands`, `npm run build`, docs Cyrillic check for `docs/ README.md client/README.md`, `git diff --check`.

- [x] **IE-003** Укрепить VS Code lifecycle и binary resolution *(completed 2026-05-25)*
  - При restart/clear-cache использовать serialized lifecycle queue, чтобы две команды не стартовали два сервера параллельно.
  - `stopLanguageClient()` должен иметь timeout и fallback kill для зависшего процесса, где доступен process handle.
  - Добавить PATH fallback: если bundled binary отсутствует и `phpLsp.serverPath` пустой, попробовать `php-lsp` из `PATH`, но явно логировать источник binary.
  - Не включать network auto-download в этот task.
  - Добавить output logs: start source, stop reason, restart reason, binary path.
  - Validation: `npm run lint`, `npm run build`, manual activation smoke.
  - Implemented: lifecycle queue serializes extension activation, restart, clear-cache restart, enable/disable changes, and deactivation.
  - Implemented: `stopLanguageClient()` now uses a 5s timeout and best-effort managed process termination when the language-client process handle is available.
  - Implemented: binary resolution now checks executable custom/bundled binaries and falls back to `php-lsp` from `PATH` when no custom path is configured and the bundled binary is missing.
  - Implemented: LSP output channel logs lifecycle reason, selected binary source/path, platform target, stop reason, start/restart reason, and removed cache directories.
  - Implemented: commands/config listener remain registered when `phpLsp.enable=false` on activation; server start is skipped until the setting is enabled.
  - Docs: README troubleshooting/config and `docs/architecture.md` updated in English.
  - Validation: `npm run lint`, `npm run check:commands`, `npm run build`, docs Cyrillic check for `docs/ README.md client/README.md`, `git diff --check`.
  - Manual VS Code activation smoke was not run from the shell session to avoid opening a user GUI window; run before release packaging.

- [x] **IE-004** Добавить проектную конфигурацию `.php-lsp.toml` *(completed 2026-05-25)*
  - Новый server config loader: project config рядом с `composer.json`, затем global config, затем VS Code/init options.
  - Определить precedence: VS Code settings override project config для editor-only настроек; project config задает shared tooling defaults.
  - Минимальные секции:
    - `[php] version`
    - `[diagnostics]` category toggles/severity
    - `[indexing] include/exclude/vendor/stubs`
    - `[formatting] provider/command/timeout`
    - `[phpstan] command/timeout/memory_limit`
    - `[psalm] command/timeout`
  - Добавить `config-schema.json` для TOML/JSON-schema compatible tooling.
  - Добавить server command/CLI `init-config`, который создает дефолтный `.php-lsp.toml` без перезаписи существующего.
  - Runtime reload: watched changes `.php-lsp.toml` должны применять config как `didChangeConfiguration`.
  - Обновить architecture/config docs на английском.
  - Implemented: TOML config loader with precedence `built-in defaults -> global config -> project .php-lsp.toml -> explicit VS Code/init options`.
  - Implemented: global config discovery via `PHP_LSP_CONFIG`, `$XDG_CONFIG_HOME/php-lsp/config.toml`, `$HOME/.config/php-lsp/config.toml`, and `$HOME/.php-lsp.toml`.
  - Implemented: project config discovery next to discovered `composer.json`, falling back to workspace-root `.php-lsp.toml`.
  - Implemented: sections `[php]`, `[diagnostics]`, `[diagnostics.severity]`, `[indexing]`, `[stubs]`, `[formatting]`, `[phpstan]`, `[psalm]`.
  - Implemented: `php-lsp init-config [--path <path>]` creates the default config without overwriting existing files.
  - Implemented: VS Code client watches `**/.php-lsp.toml` and sends explicit `didChangeConfiguration` payloads so default VS Code values do not mask project config.
  - Implemented: formatter timeout config and PHPStan `memory_limit` support.
  - Docs: added `docs/configuration.md`, root `config-schema.json`, README and architecture updates in English.
  - Regression: unit tests cover TOML normalization/merge; e2e covers project diagnostics config and watched-file reload.
  - Validation: `cargo fmt --all --check`, targeted config/e2e tests, `cargo test -p php-lsp-server`, CLI `init-config` smoke, `cargo test --all`, `cargo clippy --all-targets -- -D warnings`, `npm run lint`, `npm run check:commands`, `npm run build`, docs Cyrillic check, `git diff --check`.

- [x] **IE-005** Внедрить `codeAction/resolve` infrastructure *(done 2026-05-25)*
  - Advertise `code_action_provider.resolve_provider = true`.
  - Создать typed `CodeActionData` с `action_kind`, `uri`, `range`, `document_version`, `extra`.
  - Existing cheap actions могут оставаться eagerly computed.
  - Heavy actions должны возвращать lightweight action без `edit`; edit вычисляется в `codeAction/resolve`.
  - Добавить stale guard: если document version изменилась, resolve возвращает no-op или пересчитывает action на актуальном тексте.
  - Добавить e2e: unresolved class quickfix still works, heavy action resolves edit lazily, stale version guarded.
  - Implemented: `Add return type` action now returns lightweight `CodeAction` with typed resolve data and no eager `WorkspaceEdit`.
  - Implemented: `codeAction/resolve` computes the return-type edit on the current open document version and returns an empty edit for stale actions.
  - Implemented: quickfix import and organize-imports actions remain eagerly editable.
  - Docs: `docs/lsp-features.md` documents `codeAction/resolve` and lazy add-return-type behavior.
  - Regression: e2e covers advertised resolve capability, lazy return-type resolve, stale-version no-op resolve, PHP-version filtering, and existing import quickfix behavior.
  - Validation: `cargo fmt --all --check`, targeted e2e filters, `cargo test -p php-lsp-server`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all`, docs Cyrillic check, `git diff --check`.

### Неделя 2: production-useful code actions (2026-06-15 → 2026-06-21)

- [x] **IE-010** Code action: implement missing interface/abstract methods *(done 2026-05-25)*
  - Определить cursor context: concrete class body или class declaration.
  - Найти все abstract/interface methods из parents/interfaces/traits, которых нет в классе.
  - Сгенерировать method stubs с visibility, static, return type, params, by-ref/variadic/defaults.
  - Для unknown parameter default expression использовать safe placeholder или omit только если PHP позволяет.
  - Не генерировать duplicate methods.
  - Покрыть interface, abstract class, multiple interfaces, inherited already implemented method.
  - Implemented: quickfix action appears for concrete classes under cursor and returns lazy `CodeActionData`.
  - Implemented: resolve recomputes missing methods for the same document version and inserts stubs before the class closing brace.
  - Implemented: required-method collection covers interfaces, parent interfaces, abstract parent classes, and abstract trait methods; inherited concrete parent/trait methods satisfy requirements.
  - Implemented: stubs preserve visibility, `static`, params, by-ref, variadic, defaults, and native-safe parameter/return type hints.
  - Docs: `docs/lsp-features.md` documents the implement-missing-methods code action.
  - Regression: e2e covers interface methods, abstract parent methods, multiple interfaces, static/by-ref/variadic/default params, and inherited already implemented methods.
  - Validation: `cargo fmt --all --check`, targeted code-action e2e tests, `cargo test -p php-lsp-server`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all`, docs Cyrillic check, `git diff --check`.

- [x] **IE-010S** Code action: implement missing methods Supported coverage *(done 2026-05-26)*
  - Довести feature matrix для `textDocument/codeAction` implement missing methods до `Supported`.
  - Переносить meaningful PHPDoc контракт из interface/abstract method в generated stub.
  - Переносить method attributes из source declaration, когда это безопасно для concrete implementation.
  - Сохранять analyzer-specific generic/template param/return metadata, если native signature его не выражает.
  - Не генерировать invalid private abstract/interface methods; сохранять корректную public/protected visibility.
  - Добавить regression tests на PHPDoc/attributes, generic PHPDoc metadata, defaults/by-ref/variadic/static, diamond duplicate requirements, and no action when already implemented.
  - Implemented: generated stubs re-render inherited PHPDoc with target-class indentation.
  - Implemented: generated stubs preserve analyzer-specific contract tags such as `@template`, refined `@param`, refined `@return`, `@throws`, and `@phpstan-return`.
  - Implemented: generated stubs preserve method attributes attached to interface/abstract declarations.
  - Implemented: resolve reads declaration sources from open files or disk to extract attribute metadata while keeping stale-version no-op behavior.
  - Docs: `docs/lsp-features.md` now marks implement missing methods as `Supported`.
  - Regression: e2e covers interface/abstract PHPDoc and attributes, generic/analyzer metadata, native-safe signatures, static/by-ref/variadic/default params, inherited concrete method suppression, and no action when all methods are implemented.
  - Validation: `cargo fmt --all --check`, targeted implement-missing e2e, targeted code-action e2e tests, `cargo test -p php-lsp-server`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all`, docs Cyrillic check, `git diff --check`.

- [x] **IE-011** Code actions: generate constructor, getters, setters *(done 2026-05-26)*
  - Generate constructor для non-static properties без existing `__construct`.
  - Учитывать readonly/promoted properties и nullable/default values.
  - Generate getter/setter для property under cursor.
  - Bool property getter должен предлагать `isX()` и/или `getX()` по naming heuristic.
  - Static property генерирует static accessor methods.
  - Readonly property не получает setter.
  - Tests: class indentation, existing methods, private/protected/public, typed/untyped property.
  - Implemented: lazy `Generate constructor` refactor for concrete classes without direct `__construct`; static properties are excluded.
  - Implemented: constructor params preserve native-safe type hints and safe trailing property defaults; body assigns instance/readonly properties.
  - Implemented: lazy property getter/setter refactors under cursor, including bool `isX` naming, static accessors, readonly setter suppression, and existing accessor suppression.
  - Docs: `docs/lsp-features.md` documents generated constructor/getter/setter member actions.
  - Regression: e2e covers constructor generation, nullable default params, readonly/static properties, bool getter naming, setter generation, existing constructor suppression, and existing accessor suppression.
  - Validation: `cargo fmt --all --check`, targeted generate-members e2e, targeted code-action e2e tests, `cargo test -p php-lsp-server`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all`, docs Cyrillic check, `git diff --check`.

- [x] **IE-011S** Code actions: generate members Supported coverage *(done 2026-05-26)*
  - Довести feature matrix для `textDocument/codeAction` generate members до `Supported`.
  - Generate constructor должен сохранять richer PHPDoc/property metadata через generated constructor PHPDoc, когда native signature не может выразить generic/analyzer-specific types.
  - Generate getter/setter должен добавлять PHPDoc contract для refined property types, сохраняя native-safe signatures.
  - Учитывать attributes/PHPDoc on properties без unsafe переносов на параметры, но не терять type contract в generated members.
  - Поддержать untyped properties with `@var`, generic array/list/class-string/callable/array-shape PHPDoc types, static properties, readonly suppression, nullable/default values.
  - Добавить regression tests на constructor/getter/setter PHPDoc metadata, static/refined types, existing method suppression, and no action when constructor exists.
  - Implemented: constructor generation adds `@param` PHPDoc for property types that are richer than native PHP can express.
  - Implemented: getter generation adds `@return` PHPDoc for refined property types while keeping native-safe return hints.
  - Implemented: setter generation adds `@param` PHPDoc for refined property types while keeping native-safe parameter hints.
  - Implemented: generated members understand regular `@var`, `@phpstan-var`, and `@psalm-var` property tags, including descriptions.
  - Implemented: PHPDoc pseudo-types such as `positive-int`, generic arrays/lists, class-string lists, callable/array-shape-style contracts are kept in PHPDoc and never emitted as invalid native hints.
  - Docs: `docs/lsp-features.md` now marks generate members as `Supported`.
  - Regression: e2e covers refined constructor PHPDoc, getter/setter PHPDoc, analyzer var tags, native-safe hints, static properties, readonly setter suppression, bool getter naming, existing constructor/accessor suppression, nullable/default values, and static accessors.
  - Validation: `cargo fmt --all --check`, targeted generate-members e2e, targeted code-action e2e tests, `cargo test -p php-lsp-server`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all`, docs Cyrillic check, `git diff --check`.

- [x] **IE-012** Code actions: change visibility и promote constructor parameter *(done 2026-05-26)*
  - Change visibility для method/property/class constant/promoted property.
  - Предлагать только альтернативные visibility и сохранять modifiers order.
  - Promote constructor parameter:
    - найти property declaration
    - найти assignment `$this->prop = $param`
    - заменить constructor parameter на promoted property
    - удалить property declaration и assignment безопасно
  - Guard: не предлагать при complex assignment, multiple assignments, attributes/comments that cannot be moved safely.
  - Tests на простые и отказные сценарии.
  - Implemented: lazy visibility refactors for methods, properties, class constants, and promoted properties; resolve replaces existing visibility token or inserts one before modifiers.
  - Implemented: safe constructor property promotion for the simple pattern `property + constructor param + $this->prop = $param`.
  - Guards: promotion is suppressed for static properties, already promoted params, multi-property declarations, doc-comment/attribute properties, missing constructor param, missing exact assignment, and complex/multiple assignments.
  - Docs: `docs/lsp-features.md` documents visibility and constructor-promotion refactors.
  - Regression: e2e covers property/method/constant/promoted-property visibility changes, lazy promote resolve edits, and refusal for complex assignment.
  - Validation: `cargo fmt --all --check`, targeted IE-012 e2e, targeted code-action e2e tests, `cargo test -p php-lsp-server`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all`, docs Cyrillic check, `git diff --check`.

- [x] **IE-012S** Code actions: visibility and promotion Supported coverage *(done 2026-05-26)*
  - Довести feature matrix для `textDocument/codeAction` visibility and promotion refactors до `Supported`.
  - Change visibility должен учитывать interface/abstract/override contracts и не предлагать unsafe public/protected lowering.
  - Change visibility должен подавляться для interface members и abstract contract declarations, где visibility задает API contract.
  - Promote constructor parameter должен переносить safe property PHPDoc and attributes onto promoted constructor parameter instead of suppressing all documented properties.
  - Promote constructor parameter должен сохранять type contract metadata when property has richer PHPDoc than native parameter type.
  - Добавить regression tests на safe/unsafe visibility, interface/override refusal, promoted property metadata transfer, complex assignment refusal, and stale lazy resolve behavior.
  - Implemented: visibility actions are contract-aware for interface, abstract, and override methods; unsafe lowering below inherited/interface visibility is suppressed.
  - Implemented: standalone concrete methods, properties, constants, and promoted properties still support direct visibility edits with resolve-time revalidation.
  - Implemented: constructor promotion moves safe property PHPDoc and attributes onto the promoted parameter and deletes the full property metadata block plus the exact assignment.
  - Guard: promotion continues to refuse static properties, already promoted params, multi-property declarations, missing exact assignments, and complex/multiple assignments.
  - Docs: `docs/lsp-features.md` marks visibility and promotion refactors as `Supported`.
  - Regression: e2e covers standalone visibility changes, interface/override refusal, protected override widening, PHPDoc/attribute promotion transfer, simple promotion, promoted-property visibility, and complex assignment refusal.
  - Validation: `cargo fmt --all --check`, targeted IE-012S e2e, targeted code-action e2e tests, `cargo test -p php-lsp-server`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all`, docs Cyrillic check, `git diff --check`.

- [x] **IE-013** Code action: update PHPDoc from signature *(done 2026-05-26)*
  - Синхронизировать `@param` с actual params: add missing, remove stale, reorder.
  - Синхронизировать `@return`: add/update/remove redundant `void`.
  - Сохранять summary и unrelated tags.
  - Не удалять `@template`, `@throws`, `@deprecated`, `@property`, `@method`.
  - Учитывать promoted params, variadic, by-ref, nullable/union/intersection/generic display.
  - Tests на create new docblock и patch existing docblock.
  - Implemented: lazy `Update PHPDoc from signature` refactor for functions and methods, resolved through `codeAction/resolve`.
  - Implemented: creates a new docblock from typed signatures, including by-ref and variadic parameter tokens plus non-void native return types.
  - Implemented: patches existing docblocks by reordering actual params, adding missing params, removing stale params, and updating/removing native-return-driven `@return`.
  - Guard: does not add noisy `@param mixed` tags when a docblock only has return docs and the signature has no native parameter types.
  - Preserves: summaries and unrelated tags such as `@template`, `@throws`, `@deprecated`, `@property`, and `@method`.
  - Docs: `docs/lsp-features.md` documents PHPDoc signature sync.
  - Regression: e2e covers docblock creation, existing docblock patching, summary/unrelated tag preservation, stale param removal, redundant `void` return removal, by-ref/variadic params, and no action for already-current PHPDoc.
  - Validation: `cargo fmt --all --check`, targeted IE-013 e2e, targeted code-action e2e tests, `cargo test -p php-lsp-server`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all`, docs Cyrillic check, `git diff --check`.

- [x] **IE-013S** Code action: PHPDoc signature sync Supported coverage *(done 2026-05-26)*
  - Довести feature matrix для `textDocument/codeAction` PHPDoc signature sync до `Supported`.
  - Сохранять descriptions у `@param` и `@return` при обновлении типов/порядка.
  - Поддержать constructor/promoted params, by-ref, variadic, nullable/union/intersection/native types без потери tokens.
  - Удалять redundant PHPDoc только когда после sync не остается meaningful content.
  - Сохранять все unrelated tags, включая analyzer-specific `@phpstan-*` и `@psalm-*`.
  - Добавить regression tests на supported edge cases и отсутствие лишних actions.
  - Implemented: `@return` descriptions are preserved when return types are updated or re-rendered during param sync.
  - Implemented: existing richer analyzer-friendly PHPDoc types are preserved when they refine broad native types, e.g. `array<int, Foo>` for native `array`.
  - Implemented: sync detects and fixes parameter token drift such as missing `&` or `...`, not only type/name drift.
  - Implemented: constructor promoted parameters participate in PHPDoc sync without leaking visibility/modifier tokens into `@param`.
  - Implemented: redundant PHPDoc-only blocks are removed only when no summary or unrelated tags remain after sync.
  - Docs: `docs/lsp-features.md` now marks PHPDoc signature sync as `Supported`.
  - Regression: e2e covers return description preservation, generic precision preservation, analyzer-specific tag preservation, by-ref token correction, promoted constructor params, redundant docblock removal, and no action for already-current PHPDoc.
  - Validation: `cargo fmt --all --check`, targeted IE-013 e2e, targeted code-action e2e tests, `cargo test -p php-lsp-server`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all`, docs Cyrillic check, `git diff --check`.

- [x] **IE-014** Refactor actions: extract variable, extract constant, inline variable *(done 2026-05-26)*
  - Использовать `codeAction/resolve`, потому что selection analysis может быть дорогим.
  - Extract variable:
    - selection expression only
    - insert assignment before statement
    - replace selected expression with `$extracted`
  - Extract constant:
    - class scope only
    - literals only на первом этапе
    - insert `private const EXTRACTED = ...;`
  - Inline variable:
    - local variable with single assignment and safe usage count
    - не инлайнить across branches/closures.
  - Добавить prepare/refusal reasons в action title/detail где LSP позволяет.
  - Implemented: `refactor.extract` advertises lazy Extract variable and Extract constant actions.
  - Implemented: Extract variable accepts exact selected expressions, inserts `$extracted = ...;` before the enclosing statement, and replaces the selected expression.
  - Implemented: Extract constant accepts class-scope scalar literals, inserts `private const EXTRACTED = ...;`, and replaces the literal with `self::EXTRACTED`.
  - Implemented: `refactor.inline` advertises lazy Inline variable for local variables with one simple assignment and one same-block read.
  - Guard: inline suppresses branch/closure-crossing cases, multiple assignments/reads, compound assignments, self-referential RHS, and stale document versions.
  - Docs: `README.md` and `docs/lsp-features.md` document extract/inline refactors.
  - Regression: e2e covers extract variable, extract constant, inline variable, branch-crossing refusal, stale resolve no-op, and advertised `refactor.extract`/`refactor.inline` capabilities.
  - Validation: `cargo fmt --all --check`, targeted IE-014 e2e, targeted code-action e2e tests, initialize capability e2e, `cargo test -p php-lsp-server`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all`, docs Cyrillic check, `git diff --check`.

- [x] **IE-014S** Code actions: extract and inline refactors Supported coverage *(done 2026-05-26)*
  - Довести feature matrix для `textDocument/codeAction` extract and inline refactors до `Supported`.
  - Extract variable должен поддерживать selected expression in return/assignment/call/condition contexts, generate collision-free local variable names, and keep lazy stale-version no-op behavior.
  - Extract constant должен поддерживать scalar literal extraction inside classes, insert class constants in stable class-member position, avoid name collisions, and refuse non-literals/out-of-class selections.
  - Inline variable должен поддерживать one simple local assignment with one or more safe reads in the same straight-line block before any reassignment.
  - Inline variable должен parenthesize complex RHS where needed, delete the full assignment statement, and refuse branch/closure crossing, compound assignments, self-referential RHS, and unsafe usage counts.
  - Добавить regression tests на multiple safe reads, reassignment refusal, name-collision fallback, non-literal constant refusal, out-of-class refusal, and stale lazy resolve.
  - Implemented: feature matrix now marks `textDocument/codeAction` extract and inline refactors as `Supported`.
  - Implemented: Extract variable keeps lazy stale-version no-op behavior and generates collision-free local variable names such as `$extracted2`.
  - Implemented: Extract constant now inserts collision-free `private const` members in a stable class-member position with existing indentation.
  - Implemented: Inline variable now replaces one or more safe same-block reads from one simple assignment and deletes the full assignment statement.
  - Guard: inline still refuses branch/closure crossing, reassignment before/around reads, compound assignments, self-referential RHS, and unsafe usage counts.
  - Guard: extract constant refuses non-literal and out-of-class selections.
  - Regression: e2e covers multiple inline reads, reassignment refusal, extract variable/constant name-collision fallback, non-literal constant refusal, out-of-class refusal, and stale lazy resolve.
  - Docs: `README.md` and `docs/lsp-features.md` document the Supported extract/inline refactor behavior.
  - Validation: `cargo fmt --all --check`, targeted IE-014S e2e, targeted code-action e2e tests, `cargo test -p php-lsp-server`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all`, docs Cyrillic check, `git diff --check`.

- [x] **IE-015** Code actions для diagnostics и external analyzer findings *(done 2026-05-26)*
  - Remove unused import как explicit quickfix рядом с diagnostic.
  - Bulk "Remove all unused imports" через existing organize imports engine.
  - Replace deprecated call, если diagnostic содержит replacement metadata из attribute/PHPDoc/stub metadata.
  - Для PHPStan/Psalm diagnostics добавить extensible mapping:
    - ignore diagnostic locally
    - add missing `@throws`
    - add iterable value type in PHPDoc
    - fix obvious prefixed class name.
  - Любой analyzer quickfix должен быть opt-in и покрыт фикстурой с synthetic diagnostic.
  - Implemented: `php-lsp.unusedImport` diagnostics now offer a local `Remove unused import` quick fix.
  - Implemented: `Remove all unused imports` quick fix reuses the existing organize-imports edit.
  - Implemented: diagnostic `data.phpLsp.replacement` metadata produces a replacement quick fix for deprecated-call style diagnostics.
  - Implemented: opt-in `analyzerCodeActions.enabled` setting for PHPStan/Psalm quick fixes in `.php-lsp.toml`, VS Code settings, and initialization options.
  - Implemented: PHPStan/Psalm diagnostics can offer local ignore comments and metadata-driven `addThrows`, `addIterableValueType`, and `replacePrefixedClassName` fixes.
  - Regression: e2e covers unused import single/bulk fixes, deprecated replacement metadata, analyzer fixes disabled by default, and synthetic analyzer fixtures for ignore, `@throws`, iterable PHPDoc type, and prefixed class replacement.
  - Docs: `README.md`, `docs/configuration.md`, `docs/lsp-features.md`, `config-schema.json`, and VS Code setting contributions document the opt-in analyzer code actions.
  - Validation: `cargo fmt --all --check`, targeted code-action e2e tests, `cargo test -p php-lsp-server`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all`, `npm run lint`, `npm run build`, docs Cyrillic check, `git diff --check`.

- [x] **DOC-README-LSP-FEATURES** Sync README feature overview with current LSP feature matrix *(done 2026-05-26)*
  - Обновить README на английском, чтобы он отражал недавно добавленные LSP features из `docs/lsp-features.md`.
  - Не дублировать всю feature matrix, но явно перечислить основные Supported/Partial возможности по navigation, hierarchy, diagnostics, code actions, formatting, semantic tokens, workspace/file operations.
  - Проверить, что README остается кратким entrypoint и ссылается на `docs/lsp-features.md` для детальной матрицы.
  - Implemented: README feature overview now covers diagnostic severity controls, PHPDoc virtual-member hover/completion, inlay hints, semantic token modes, detailed navigation, symbols/hierarchies, code-action families, formatting, and workspace/file-operation behavior.
  - Validation: docs Cyrillic check and `git diff --check`.

### Неделя 3: CLI/tooling режимы и formatter strategy (2026-06-22 → 2026-06-28)

- [x] **IE-020** Добавить CLI `analyze` *(done 2026-05-26)*
  - Running binary without subcommand сохраняет LSP stdio behavior.
  - `php-lsp analyze [PATH] --project-root <DIR> --severity <all|hint|info|warning|error> --format <table|json|github>`.
  - Использовать тот же parser/index/diagnostics pipeline, что и LSP.
  - Exit codes:
    - `0` no diagnostics at requested severity
    - `1` execution/config error
    - `2` diagnostics found
  - Output:
    - table для локального запуска
    - JSON stable schema для scripts
    - GitHub workflow annotations для CI.
  - Tests: command parsing, JSON output shape, exit code behavior на fixture.
  - Implemented: `php-lsp analyze` subcommand while preserving stdio LSP behavior when no subcommand is provided.
  - Implemented: `[PATH]`, `--project-root`, `--severity`, and `--format` parsing with validation and `--help`.
  - Implemented: CLI analysis builds a workspace index from Composer/source/include paths and runs the existing parser/index/built-in diagnostics pipeline.
  - Implemented: severity filtering as a threshold (`error`, `warning`, `info`, `hint`, `all`) and exit codes `0`, `1`, `2`.
  - Implemented: table, stable JSON (`schemaVersion`, `summary`, `diagnostics`), and GitHub annotation output formats.
  - Regression: unit tests cover command parsing, JSON output shape, clean/error/diagnostics exit codes, and fixture diagnostics.
  - Docs: `README.md` and `docs/configuration.md` document CLI usage, config loading, formats, and exit codes.
  - Validation: `cargo fmt --all --check`, targeted analyze unit tests, `cargo test -p php-lsp-server`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all`, real `php-lsp analyze --help`, docs Cyrillic check, `git diff --check`.

- [x] **IE-021** Добавить CLI `fix --dry-run` для safe fixers *(done 2026-05-26)*
  - Первый набор rules: unused imports, organize imports, add return type from PHPDoc where safe.
  - `--dry-run` показывает changes без записи.
  - `--rule <RULE>` можно указывать несколько раз.
  - Без `--rule` запускать только preferred safe native fixers.
  - Не запускать formatter проекта внутри fix command без отдельного explicit flag.
  - Tests: idempotency, dry-run no write, JSON output.
  - Implemented: `php-lsp fix [PATH] --dry-run` subcommand while preserving stdio LSP behavior when no subcommand is provided.
  - Implemented: `--project-root`, repeated `--rule`, and `--format <table|json>` parsing with validation and `--help`.
  - Implemented: supported rules `unused-imports`, `organize-imports`, and `add-return-type`; default rules are preferred safe native fixers (`unused-imports`, `add-return-type`).
  - Implemented: dry-run-only execution that refuses to write files without `--dry-run`; no project formatter is invoked.
  - Implemented: table output and stable JSON (`schemaVersion`, `rules`, `summary`, `files`, `fixes`, `edits`).
  - Regression: unit tests cover command parsing, JSON output shape, dry-run no-write behavior, missing `--dry-run`, and idempotency after applying generated edits.
  - Docs: `README.md` and `docs/configuration.md` document CLI fix usage, rule selection, exit codes, and formatter behavior.
  - Validation: `cargo fmt --all --check`, targeted fix unit tests, `cargo test -p php-lsp-server`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all`, real `php-lsp fix --help`, real fixture dry-run, docs Cyrillic check, `git diff --check`.

- [x] **IE-022** Улучшить formatting strategy *(done 2026-05-26)*
  - Добавить auto-detect project tools из Composer metadata:
    - Laravel Pint
    - php-cs-fixer
    - phpcbf
  - Precedence:
    - explicit `phpLsp.formatting.*` / `.php-lsp.toml`
    - project tool detected in `require-dev`
    - current external provider fallback
    - optional built-in formatter task отдельно, если выбран parser/formatter dependency.
  - External tools должны иметь timeout и cancellation.
  - Range formatting должен оставаться conservative: не форматировать весь файл неожиданно.
  - Docs на английском: formatter resolution order и troubleshooting.
  - Implemented: `phpLsp.formatting.provider = "auto"` as the default provider and `.php-lsp.toml` starter default.
  - Implemented: Composer `require-dev`/`require` detection for `laravel/pint`, `friendsofphp/php-cs-fixer`, and `squizlabs/php_codesniffer` with precedence Pint → php-cs-fixer → phpcbf.
  - Implemented: explicit provider precedence for VS Code settings and `.php-lsp.toml`; `none` disables formatting, `custom` keeps command templates, and `pint` is now a supported explicit provider.
  - Implemented: external formatter execution through the shared timeout/cancellable subprocess runner; formatting runs are cancelled on document change/close/rename and when superseded by a newer formatting request.
  - Preserved: range formatting formats only the selected fragment through a temporary file and never silently formats the whole document.
  - Regression: unit tests cover Composer detection precedence and explicit `none`; e2e covers php-cs-fixer auto-detection from Composer metadata.
  - Docs: `README.md`, `client/README.md`, `docs/configuration.md`, `docs/lsp-features.md`, `docs/production-risk-register.md`, `config-schema.json`, and VS Code setting metadata document provider resolution, timeout, cancellation, and troubleshooting.
  - Validation: `cargo fmt --all --check`, targeted formatting tests, `cargo test -p php-lsp-server`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all`, `npm run lint`, `npm run build`, docs Cyrillic check, `git diff --check`.

- [x] **IE-023** Оценить и при необходимости добавить built-in formatter fallback *(done 2026-05-26)*
  - Research-only first step: выбрать реалистичный formatter backend или отказаться.
  - Если добавляем dependency:
    - измерить binary size impact
    - проверить PHP 7.4-8.4 syntax coverage
    - добавить `phpLsp.formatting.provider = "built-in"`
  - Если не добавляем:
    - закрыть task documented decision в `DECISIONS.md`
    - README должен честно говорить, что native formatter не поставляется.
  - Не смешивать с IE-022 auto-detect.
  - Decision: do not add `built-in` provider in this milestone.
  - Documented: ADR-017 in `DECISIONS.md` records the formatter-backend
    evaluation and revisit criteria.
  - Rationale: the current tree-sitter CST architecture is not a formatter AST;
    the realistic Rust-native candidate, Mago formatter, brings a separate
    parser/AST/tooling stack and currently requires Rust `1.95.0`, while
    php-lsp keeps workspace MSRV `1.85`.
  - Docs: `README.md`, `docs/configuration.md`, and `docs/lsp-features.md`
    explicitly state that native formatting is not shipped and no `built-in`
    formatter provider is advertised.
  - Validation: docs Cyrillic check for public English docs, `git diff --check`.

- [x] **IE-024** CI интеграция для CLI *(done 2026-05-26)*
  - Добавить docs на английском: пример GitHub Actions `php-lsp analyze`.
  - Добавить package smoke, что binary subcommands работают в release build.
  - Не включать CLI analyze в текущий project CI как required gate до стабилизации false positives.
  - Добавить scripts/examples для локального запуска на fixture и anonymized large workspace.
  - Implemented: `docs/cli-ci.md` with a report-only GitHub Actions example for
    `php-lsp analyze --format github`, including exit-code behavior and
    guidance to keep `continue-on-error` until diagnostics are stable.
  - Implemented: `scripts/smoke-cli.sh` to smoke an existing release binary
    through `--version`, top-level help, `analyze --help`, `fix --help`,
    `init-config`, real JSON `analyze`, and JSON `fix --dry-run`.
  - Implemented: `scripts/smoke-vsix.sh` now extracts the packaged
    `linux-x64` binary on Linux and runs the CLI smoke against the VSIX payload;
    the existing VS Code activation smoke now supplies `context.extension`.
  - Implemented: `scripts/examples/run-cli-analyze-fixture.sh` and
    `scripts/examples/run-cli-analyze-large-workspace.sh`; the large-workspace
    script writes an anonymized JSON report.
  - Preserved: current project CI does not run `php-lsp analyze` as a required
    gate.
  - Validation: shell syntax checks, release binary build, `scripts/smoke-cli.sh`
    on `test-fixtures/basic`, both example scripts, single-platform VSIX package
    smoke with CLI payload check, docs Cyrillic check, `cargo fmt --all --check`,
    `git diff --check`.

### Неделя 4: deep type intelligence (2026-06-29 → 2026-07-05)

- [x] **IE-030** Template model для classes/functions/methods *(done 2026-05-26)*
  - Расширить symbol model:
    - class-level template params with bounds
    - method/function-level template params
    - template variance metadata best-effort
    - template bindings from `@extends`, `@implements`, `@use`, `@mixin`
  - Type parser должен распознавать `@template`, `@template-covariant`, `@template-contravariant`.
  - Inheritance resolver должен подставлять generic args при lookup members.
  - Tests: generic repository, collection item type, inherited generic method return.
  - Implemented: `TemplateParam`, `TemplateVariance`, `TemplateBindingKind`, and
    `TemplateBinding` in shared types; `PhpDoc` and `SymbolInfo` now carry
    template metadata.
  - Implemented: PHPDoc parsing for `@template`, `@template-covariant`,
    `@template-contravariant`, plus PHPStan/Psalm-prefixed variants.
  - Implemented: PHPDoc generic bindings from `@extends`, `@implements`,
    `@use`, and `@mixin`, with target and argument type resolution in the
    declaring file context.
  - Implemented: inherited member lookup and `get_members` substitute template
    arguments through extends/implements/trait/mixin edges; method-local
    templates shadow inherited substitutions.
  - Implemented: cache schema version bump to invalidate older symbol snapshots
    without template metadata.
  - Regression: parser tests cover template params, variance, bindings, and
    symbol extraction; index tests cover generic repository and collection item
    inherited return type substitution.
  - Docs: `README.md`, `docs/lsp-features.md`, and
    `docs/production-risk-register.md` mention the new best-effort PHPDoc
    template support.
  - Validation: `cargo fmt --all --check`, `cargo test -p php-lsp-parser`,
    `cargo test -p php-lsp-index`, `cargo test --all`,
    `cargo clippy --all-targets -- -D warnings`, docs Cyrillic check,
    `git diff --check`.

- [x] **IE-031** Type aliases и imported aliases *(done 2026-05-26)*
  - Поддержать `@phpstan-type`, `@psalm-type`, `@phpstan-import-type`, `@psalm-import-type`.
  - Alias scope: class docblock, file-level docblock если поддерживается parserом.
  - Aliases должны участвовать в hover/completion/typeDefinition best-effort.
  - Guard cycles: detect recursive aliases и fallback to raw type without panic.
  - Tests: alias to array shape, imported alias, recursive alias ignored.
  - Implemented: PHPDoc parser support for PHPStan/Psalm local and imported
    type alias tags, including optional `=` syntax and `as` import aliases.
  - Implemented: `FileSymbols` stores file-level alias metadata; class
    docblock aliases remain class-scoped and are parsed from the indexed class
    docblock when materializing signatures.
  - Implemented: `WorkspaceIndex` expands aliases for indexed function/member
    signatures before template substitution, resolves imported aliases through
    the source class scope, and falls back to the raw alias on recursive cycles.
  - Implemented: cache schema version bump to invalidate symbol snapshots
    without file-level alias metadata.
  - Regression: parser tests cover alias/import parsing and file-level alias
    extraction; index tests cover class-scoped array-shape aliases, file-level
    function aliases, imported aliases, and recursive alias fallback.
  - Docs: `README.md`, `docs/lsp-features.md`, and
    `docs/production-risk-register.md` mention the new best-effort PHPDoc type
    alias support.
  - Validation: `cargo test -p php-lsp-parser`, `cargo test -p php-lsp-index`,
    `cargo test --all`, `cargo fmt --all --check`,
    `cargo clippy --all-targets -- -D warnings`, docs Cyrillic check,
    `git diff --check`.

- [x] **IE-032** Conditional return types и class-string templates *(done 2026-05-26)*
  - Parse PHPStan/Psalm conditional return syntax: `($arg is Foo ? A : B)`.
  - Resolve branch when call-site argument is:
    - literal string/int/bool/null
    - `ClassName::class`
    - variable with known literal/class-string type.
  - `class-string<T>` argument should bind `T` for return type.
  - Fallback: unresolved condition returns union of branches.
  - Tests: service locator `make(Foo::class): Foo`, conditional factory, fallback union.

- [x] **IE-032E** Conditional return types and `class-string<T>` call-site inference *(done 2026-05-26)*
  - Extend PHPDoc/type parsing so return types can preserve PHPStan/Psalm
    conditional syntax like `($name is class-string<T> ? T : object)`.
  - Bind `class-string<T>` template arguments from call-site expressions such
    as `Foo::class`, string literals, and variables with known class-string
    type where existing local inference can prove it.
  - Resolve conditional return branches when the tested argument is a known
    literal/class-string; otherwise expose a stable union fallback of both
    branches.
  - Thread the resolved call-site return type through hover/completion/inlay
    paths that already consume indexed function and method return `TypeInfo`.
  - Add parser/index/server e2e coverage for service-locator `make(Foo::class)`,
    conditional factories, and unresolved fallback unions.
  - Implemented: `TypeInfo` now models PHPStan/Psalm conditional return types
    and the PHPDoc parser preserves `($arg is Type ? A : B)` instead of
    flattening it into a simple string.
  - Implemented: PHPDoc `@param` types enrich indexed function/method
    signatures, so `class-string<T>` can bind `T` even when the PHP native
    parameter type is omitted or broader than the PHPDoc contract.
  - Implemented: call-site return resolution binds `class-string<T>` from
    `Foo::class`, string literals that resolve to indexed class symbols, and
    local variables with known PHPDoc `class-string` types.
  - Implemented: conditional returns choose a known branch when possible and
    expose a deterministic `if|else` union fallback when the subject argument
    is unresolved.
  - Implemented: local variable hover/inlay paths and member completion chains
    reuse the call-site return resolver, including
    `$locator->make(Widget::class)->...` completion.
  - Implemented: cache schema version bumped because indexed `TypeInfo`
    serialization gained a new variant.
  - Regression: parser tests cover conditional return parsing and PHPDoc param
    template metadata; e2e tests cover inlay hints, hover links, completion
    chains, service-locator factories, conditional factories, and fallback
    unions.
  - Docs: `README.md`, `docs/lsp-features.md`, and
    `docs/production-risk-register.md` mention the new call-site inference.
  - Validation: `cargo test -p php-lsp-parser`, targeted e2e inlay, hover,
    and completion tests, `cargo test --all`, `cargo fmt --all --check`,
    `cargo clippy --all-targets -- -D warnings`, docs Cyrillic check,
    `git diff --check`.

- [x] **IE-032A** Inlay hints: inferred local variable types *(done 2026-05-26)*
  - Extend `textDocument/inlayHint` with Rust-style `: Type` hints after local
    variable declarations/assignments where the server can infer a useful type.
  - Cover at minimum:
    - `$user = new User();`
    - `$item = $repo->find();` via indexed member return type resolver
    - inline/local PHPDoc `@var`
    - `foreach ($items as $item)` value inference where existing inference
      supports generic arrays/lists.
  - Avoid noisy hints for unknown/mixed/scalar-only cases unless inference has
    an explicit useful display type.
  - Keep hints range-aware, sorted with existing inlay hints, and guarded from
    duplicate hints on the same variable/range.
  - Add e2e coverage for new-expression, method-return, PHPDoc, and foreach
    variable type hints.
  - Update `README.md` / `docs/lsp-features.md` after implementation.
  - Implemented: `textDocument/inlayHint` now emits local variable `: Type`
    hints for simple assignments and foreach value variables when existing
    inference can produce a useful object/generic/shape-like type.
  - Implemented: hints reuse indexed member return type resolution, inline
    PHPDoc `@var`, generic/list foreach value inference, range filtering,
    sorting, and duplicate guards.
  - Implemented: scalar-only assignment hints such as `$count = 1` are
    suppressed to avoid noisy editor output.
  - Regression: e2e covers `new User()`, `$repo->find()`, inline PHPDoc
    `array<int, User>`, foreach `$item`, and scalar suppression.
  - Docs: `README.md` and `docs/lsp-features.md` mention inferred local
    variable type inlay hints.
  - Validation: `cargo test -p php-lsp-parser`,
    `cargo test -p php-lsp-server --test e2e test_inlay_hints`,
    `cargo test --all`, `cargo fmt --all --check`,
    `cargo clippy --all-targets -- -D warnings`.

- [x] **IE-032B** Inlay hints: real-project method-return types and clickable labels *(done 2026-05-26)*
  - Reproduce missing local variable type hints from
    `/home/apanov/Projects/bdpn-ui/app/src/Soap/Inbound/Handler/CdbHandler.php`.
  - Fix assignments such as `$portingProcess = $portingRequest->getPortingProcess();`
    where the RHS method return type is known through indexed project symbols
    but the local file fallback does not carry the full return `TypeInfo`.
  - Use `InlayHintLabelPart` for object-like local variable type hints when the
    target class can be resolved, so clients can navigate from the hint label to
    the type definition where supported.
  - Keep string labels for generic/shape/scalar-safe hints that do not have one
    unambiguous class location.
  - Add regression coverage using a fixture modeled after the real
    `PortingProcess` / `RecipientProcess` assignments.
  - Implemented: local variable assignment hints now prefer exact RHS
    expression return types from indexed function/method symbols before falling
    back to local variable hover inference.
  - Implemented: explicit scalar method returns such as `bool` can now be shown
    without re-enabling noisy literal assignment hints such as `$count = 1`.
  - Implemented: object-like type hints use `InlayHintLabelPart` with a
    definition `location` and label-part tooltip when the target type has one
    unambiguous indexed class/interface/trait/enum symbol.
  - Implemented: local variable type hint tooltips now include the concrete
    inferred type text or target FQN.
  - Regression: e2e covers `?PortingProcess` and `bool` method-return hints
    plus navigable label parts for `User` and `PortingProcess`.
  - Validation: `cargo test -p php-lsp-server --test e2e test_inlay_hints`,
    `cargo test --all`, `cargo fmt --all --check`,
    `cargo clippy --all-targets -- -D warnings`, `git diff --check`.

- [x] **IE-032C** Hover: inferred local variable return types and clickable type links *(done 2026-05-26)*
  - Reproduce hover missing type information on variables such as
    `$recipientProcess` and `$recipientProcessUpdated` after assignment from
    indexed method calls.
  - Reuse the same RHS method-return `TypeInfo` path that powers local variable
    inlay hints, including inherited methods and nullable object returns.
  - Render variable hover Markdown with concrete inferred type text for locals
    inferred from `new`, method/function calls, inline PHPDoc, and foreach where
    existing inference supports them.
  - Add clickable Markdown links for resolvable class/interface/trait/enum names
    in hover type text, while keeping scalar/generic text readable when a single
    definition target is unavailable.
  - Add e2e coverage for hovering `$recipientProcess`,
    `$recipientProcessUpdated`, and linked `PortingProcess` type text.
  - Implemented: local variable hover now uses the same indexed RHS
    method/function return `TypeInfo` path as local variable inlay hints,
    including current assignment hover on the left-hand variable.
  - Implemented: hover keeps scalar method returns such as `bool` visible and
    preserves PHPDoc hover context where available.
  - Implemented: hover adds a Markdown `Type` section with a clickable file link
    for resolvable class-like types, e.g. `?PortingProcess`.
  - Regression: e2e covers hover on `$recipientProcess` and
    `$recipientProcessUpdated`, including a linked `PortingProcess` type and
    scalar `bool` type section.
  - Validation: `cargo test -p php-lsp-server --test e2e hover`,
    `cargo test --all`, `cargo fmt --all --check`,
    `cargo clippy --all-targets -- -D warnings`, `git diff --check`.

- [x] **IE-032D** Inlay hints: explicit scalar casts and casted method returns *(done 2026-05-26)*
  - Reproduce missing local variable type hints in
    `/home/apanov/Projects/bdpn-ui/app/src/Soap/Inbound/Handler/CdbConfirmHandler.php`
    for assignments such as `$requestId = (string)(...)`.
  - Reproduce missing local variable type hints in
    `/home/apanov/Projects/bdpn-ui/app/src/Soap/Inbound/Handler/CdbHandler.php`
    for assignments such as `$currentPlace = (string)$donorProcess->getCurrentPlace();`.
  - Infer useful local variable inlay hints from explicit PHP cast expressions
    (`(string)`, `(int)`, `(float)`, `(bool)`, `(array)`, `(object)`) without
    re-enabling noisy scalar literal hints like `$count = 1`.
  - Keep hover behavior aligned with inlay hints because variable hover reuses
    the same RHS expression inference path.
  - Add e2e coverage for casted null-coalesce/stdClass property reads and
    casted method calls.
  - Implemented: local variable RHS inference recognizes `cast_expression` and
    maps PHP cast aliases to stable hint labels (`string`, `int`, `float`,
    `bool`, `array`, `object`).
  - Implemented: explicit cast hints are allowed for scalar/object PHP casts
    while scalar literal assignments remain suppressed.
  - Regression: e2e covers `(string)($message->NPRequestId ?? '')` and
    `(string)$donorProcess->getCurrentPlace()` producing local `: string`
    inlay hints.
  - Validation: `cargo test -p php-lsp-server --test e2e test_inlay_hints`,
    `cargo test -p php-lsp-server --test e2e hover`, `cargo test --all`,
    `cargo fmt --all --check`,
    `cargo clippy --all-targets -- -D warnings`, `git diff --check`.

- [x] **REL-060** Bump project version to 0.6.0 *(done 2026-05-26)*
  - Update all source-controlled package/version declarations for the Rust
    server, VS Code extension, release metadata, and documentation references
    from the current release version to `0.6.0`.
  - Keep generated build/cache artifacts out of the version bump unless they are
    intentionally source-controlled release metadata.
  - Refresh lockfiles where package metadata requires it.
  - Validate manifests/build metadata after the bump.
  - Updated: `VERSION`, Rust workspace version and lockfile package entries,
    VS Code extension `package.json`/`package-lock.json`, PRD status, and
    production baseline current-version reference.
  - Validation: no source-controlled project references to the previous release
    version remain outside external stubs/ignored artifacts; cargo metadata
    with `--locked` reports all `php-lsp-*` packages at `0.6.0`; npm package
    metadata reports `0.6.0`;
    `cargo test --all`, `cargo fmt --all --check`,
    `cargo clippy --all-targets -- -D warnings`, `npm run lint`,
    `npm run build`, `git diff --check`.

- [x] **IE-033** Shape-aware completion and definition *(done 2026-05-27)*
  - Array shape keys from PHPDoc/literal arrays should appear in `$arr['...']` completion.
  - Object shape properties should appear in `$obj->...` where modeled.
  - Go-to-definition for shape key should jump to PHPDoc/literal declaration where practical.
  - Preserve nested shapes and optional keys best-effort.
  - Tests: `array{foo: User, bar?: int}`, nested shape, list shape ignored for key completion.
  - Implemented: PHPDoc parser and shared `TypeInfo` now preserve
    `object{...}` shapes alongside existing `array{...}` shapes; cache schema
    was bumped because serialized type metadata changed.
  - Implemented: local inference keeps array-shape metadata from PHPDoc and
    literal array assignments, including nested literal arrays.
  - Implemented: completion inside `$arr['...']` returns PHPDoc/literal array
    shape keys, nested shape keys, and optional keys while ignoring list-like
    generics for key completion.
  - Implemented: completion after `$obj->` returns modeled `object{...}`
    properties without requiring an indexed class symbol.
  - Implemented: go-to-definition on shape keys jumps to the PHPDoc shape key
    or literal array key declaration where the source range can be recovered.
  - Regression: parser tests cover object-shape parsing, nested PHPDoc array
    shape inference, and literal array-shape inference; completion context
    tests cover array-key contexts; e2e covers PHPDoc/literal array key
    completion, object-shape property completion, nested shapes, list
    suppression, and definition targets.
  - Docs: `README.md`, `docs/lsp-features.md`, and
    `docs/production-risk-register.md` mention shape-aware
    completion/definition.
  - Validation: `cargo test --all`, `cargo fmt --all --check`,
    `cargo clippy --all-targets -- -D warnings`, docs Cyrillic check,
    `git diff --check`.

- [x] **IE-034** Closure, foreach, and collection callback inference *(done 2026-05-27)*
  - Infer closure/arrow-function params from callable parameter types.
  - Infer foreach key/value from `iterable<TKey,TValue>`, `array<TKey,TValue>`, `Generator<TKey,TValue,...>`.
  - Recognize common map/filter/reduce callback patterns through generic method signatures, not hardcoded project classes.
  - Ensure closure scopes do not leak variables into outer scope.
  - Tests: collection-like generic class fixture, array_map, foreach generator.
  - Implemented: untyped closure and arrow-function parameters infer from
    expected `callable(...)` parameter types at function/method call sites.
  - Implemented: callback parameter inference binds templates from actual call
    arguments such as `array<int, User>` and from receiver generics such as
    `Collection<User>`, so map/filter-style methods work through signatures
    instead of hardcoded collection classes.
  - Implemented: server hover, definition, diagnostics, completion, and inlay
    paths pass indexed callable-parameter resolution so callbacks work when
    helper functions or collection classes live in another indexed file.
  - Implemented: `foreach ($iterable as $key => $value)` now infers keys and
    values from `array<TKey,TValue>`, `iterable<TKey,TValue>`,
    `Generator<TKey,TValue,...>`, list-like types, collection-like generics,
    and array shapes best-effort.
  - Guard: closure parameter inference stays scoped to the closure/arrow
    function and does not leak inferred variables into the outer scope.
  - Regression: parser tests cover array-map-style helpers, generic collection
    map/filter callbacks, closure scope isolation, and generator key/value
    foreach inference.
  - Regression: server e2e covers callback hover and go-to-definition through
    indexed helper/collection signatures in separate files.
  - Docs: `README.md`, `docs/lsp-features.md`, and
    `docs/production-risk-register.md` mention callback and generator foreach
    inference.

- [x] **IE-035** Per-request expression type cache *(done 2026-05-27)*
  - Add request-local cache for expression/member type resolution.
  - Key by URI, document version, byte range, expected context.
  - Use only within one request or one diagnostics pass; do not persist until invalidation story is proven.
  - Benchmark before/after on completion, diagnostics, references-heavy fixture.
  - Must not change visible behavior except latency.
  - Implemented: added a `RequestTypeCache` scoped to a single request or
    diagnostics pass, with keys containing URI, document version, byte range,
    resolver context, and expected-context text.
  - Implemented: completion now reuses cached member types, member-chain
    results, variable type inference, shape type-info inference, and call-site
    member return resolution within one completion request.
  - Implemented: inlay hints and local variable hover helper paths reuse cached
    member type lookups, call-site argument type inference, callable target
    resolution, and local variable inlay type results within one request.
  - Implemented: semantic member/type diagnostics reuse cached
    symbol-at-position, member type, and simple expression type inference
    within one diagnostics pass.
  - Guard: cache is not stored on `PhpLspBackend`, is recreated per request or
    diagnostics pass, and negative lookups are cached only inside that scope.
  - Regression: server unit tests cover cache hits, negative-result caching, and
    separation by expected context and document version.
  - Benchmark: warmed before/after timings on targeted scenarios:
    completion `0.09s -> 0.08s`, diagnostics `0.09s -> 0.10s`, references
    `0.10s -> 0.10s`; no visible behavior change was observed.
  - Validation: `cargo test --all`, `cargo fmt --all --check`,
    `cargo clippy --all-targets -- -D warnings`.

### Неделя 5: framework-aware providers and template files (2026-07-06 → 2026-07-12)

- [ ] **IE-040** Ввести `VirtualMemberProvider` / framework adapter архитектуру
  - Общий trait/provider слой для synthetic methods/properties/string keys.
  - Providers получают readonly context: workspace root, composer metadata, indexed symbols, file content when already open/available.
  - Providers не должны bootstrapping приложение, подключать database или выполнять user code.
  - Results должны иметь source ranges или synthetic metadata for hover/completion/definition.
  - Cache/invalidation: per workspace config/composer fingerprint + watched relevant files.
  - Tests: provider ordering, duplicate merge, cache invalidation.

- [ ] **IE-041** Laravel/Eloquent-like model virtual properties
  - Detect model classes structurally: inheritance/interface/known framework symbols from Composer, not hardcoded project paths.
  - Sources:
    - `@property*` PHPDoc
    - `$casts` property and `casts()` method
    - legacy accessors `getXAttribute()`
    - modern accessors returning `Attribute<TGet,TSet>`
    - `$fillable`, `$guarded`, `$hidden`, `$visible` as weak mixed fallback.
  - Respect `__get`/`__set` and `reportMagicProperties`-like setting.
  - Tests: casts, accessors, property docs, magic property diagnostics.

- [ ] **IE-042** Laravel/Eloquent-like relations, scopes, builders
  - Infer relationship methods returning relation generics and expose related model properties where safe.
  - Add `*_count` virtual properties for known relationships.
  - Local scopes: `scopeActive($query)` exposes `active()` on builder-like chains.
  - Custom builder detection through return types/attributes/static factory methods where available.
  - Support fluent `query()->where()->first()` style chains via generics, not one-off method names.
  - Tests: belongs-to/has-many-like fixtures, local scope, custom builder, relation count.

- [ ] **IE-043** Framework string-key intelligence
  - Completion + definition for string keys where files are static and discoverable:
    - config keys
    - route names
    - translation keys
    - view/template names
  - Do not execute project code.
  - Use file parsers or conservative static scanners.
  - Add providers only in recognized project layout; otherwise no suggestions.
  - Tests: config tree, route declarations, nested translations, view file lookup.

- [ ] **IE-044** Blade-like template support via virtual PHP + source map
  - Add document selector for template language only if client packaging contributes/activates it safely.
  - Preprocess template into virtual PHP while preserving source map.
  - Map diagnostics, hover, completion, semantic tokens back to original template ranges.
  - Support first:
    - escaped/raw echo blocks
    - `@if`, `@foreach`, `@isset`, `@empty`
    - comments/directives as semantic tokens
  - Do not advertise full template support until diagnostics/source-map edge cases are stable.
  - Tests: source map range conversion, hover/completion in echo, diagnostics no false whole-file range.

- [ ] **IE-045** Final acceptance for intelligence milestone
  - Run full validation:
    - `cd server && cargo fmt --all --check`
    - `cd server && cargo test --all`
    - `cd server && cargo clippy --all-targets -- -D warnings`
    - `cd client && npm run lint`
    - `cd client && npm run build`
    - `git diff --check`
  - Re-run large workspace profile and latency baseline from `PV-*`.
  - Add new fixture audits for type/framework features.
  - Update English docs:
    - `README.md`
    - `docs/lsp-features.md`
    - `docs/architecture.md`
    - `docs/performance.md`
    - `docs/production-baseline.md` if metrics changed.
  - Update risk register with any remaining partial/accepted limitations.

### IDE Intelligence Dependencies

```
PV-014 ─→ IE-001
PV-014 ─→ IE-002
PV-014 ─→ IE-003
PV-014 ─→ IE-004
IE-005 ─→ IE-014
IE-005 ─→ IE-015
IE-004 ─→ IE-020
IE-004 ─→ IE-022
IE-020 ─→ IE-021 ─→ IE-024
IE-030 ─→ IE-031 ─→ IE-032
IE-030 ─→ IE-033
IE-030 ─→ IE-034
IE-035 ─→ IE-045
IE-040 ─→ IE-041 ─→ IE-042
IE-040 ─→ IE-043
IE-040 ─→ IE-044
IE-001 ─→ IE-045
IE-010 ─→ IE-045
IE-020 ─→ IE-045
IE-030 ─→ IE-045
IE-040 ─→ IE-045
```

---

## Зависимости между задачами

```
M-001 ──→ M-002 ──→ M-005 (scaffold → crates → LSP hello-world)
M-001 ──→ M-003          (scaffold → client)
M-001 ──→ M-004          (scaffold → CI)
M-002 ──→ M-006          (crates → parser)
M-006 ──→ M-007          (parser → doc sync)
M-006 ──→ M-008          (parser → diagnostics)
M-006 ──→ M-010          (parser → symbol extraction)
M-009 ──→ M-010          (index structs → symbol extraction)
M-010 ──→ M-012          (extraction → workspace indexing)
M-011 ──→ M-012          (composer → workspace indexing)
M-012 ──→ M-014          (indexing → hover)
M-012 ──→ M-015          (indexing → definition)
M-012 ──→ M-017          (indexing → completion)
M-012 ──→ M-019          (indexing → references)
M-016 ──→ M-014          (phpdoc → hover)
M-019 ──→ M-020          (references → rename)
M-013 ──→ M-014          (stubs → hover on built-ins)
V1-023 ─→ V1-004         (configuration → PHP-version-aware code actions)
V1-023 ─→ V1-026         (configuration → PHP-version-specific diagnostics)
V1-022 ─→ V1-009         (file watching → disk cache invalidation)
V1-022 ─→ V1-024         (watched file changes → file operation handling)
V1-002 ─→ V1-021         (add use quick-fix → auto-import completion)
V1-017 ─→ V1-026         (type definition → type/member diagnostics)
V1-025 ─→ V1-002         (unknown/unused diagnostics → quick-fixes)
V1-025 ─→ V1-003         (unused imports → organize imports)
V1-026 ─→ VN-002         (member/type model → call hierarchy)
V1-026 ─→ VN-003         (type model → type hierarchy)
V1-026 ─→ VN-004         (inheritance model → implementation)
V1-022 ─→ VN-005         (single-root file watching → multi-root support)
V1-023 ─→ VN-005         (single-root config → multi-root config)
```

---

## Research: Go-to-definition for promoted property member chains

- [x] **R-001** Analyze resolution chain for `$this->logger->debug()` *(done 2026-03-18)*
  - Read `resolve.rs`: `try_resolve_object_type`, `resolve_node`, variable type inference
  - Read `server.rs`: `resolve_member_type`, `goto_definition`, `symbol_at_position_with_resolver`
  - Read `symbols.rs`: symbol extraction for promoted constructor params
  - Read `workspace.rs`: `resolve_fqn`, `resolve_member`, `get_direct_members`
  - Identified root cause: promoted constructor params not emitted as Property symbols

- [x] **R-002** Оценить архитектуру и потенциальные проблемы проекта *(done 2026-05-04)*
  - Просмотреть структуру workspace, ключевые crate'ы и клиент VS Code
  - Проверить основные потоки: парсинг, индекс, completion, LSP server
  - Сформулировать риски по архитектуре, корректности, производительности и поддерживаемости

---

## LSP parity tracking checklist

Короткий список для отслеживания прогресса по возможностям, которые обычно есть в зрелых LSP серверах. Подробности по каждой задаче описаны в соответствующих `V1-*` / `VN-*` пунктах выше.

- [x] **LP-001 / V1-001** `textDocument/signatureHelp`
- [x] **LP-002 / V1-002** `textDocument/codeAction` — quick-fix: add use
- [x] **LP-003 / V1-003** `source.organizeImports`
- [x] **LP-004 / V1-004** `textDocument/codeAction` — add return type
- [x] **LP-005 / V1-005** `textDocument/formatting`
- [x] **LP-006 / V1-006** `textDocument/rangeFormatting`
- [x] **LP-007 / V1-015** `textDocument/onTypeFormatting`
- [x] **LP-008 / V1-007** `textDocument/semanticTokens/full`
- [x] **LP-009 / V1-008** `textDocument/semanticTokens/full/delta`
- [x] **LP-010 / V1-016** `textDocument/declaration`
- [x] **LP-011 / V1-017** `textDocument/typeDefinition`
- [x] **LP-012 / V1-018** `textDocument/documentHighlight`
- [x] **LP-013 / V1-019** `textDocument/selectionRange`
- [x] **LP-014 / V1-020** `textDocument/linkedEditingRange`
- [x] **LP-015 / V1-021** Completion polish: snippets, sorting, auto-imports, visibility-aware members
- [x] **LP-016 / V1-022** `workspace/didChangeWatchedFiles` and incremental reindex
- [x] **LP-017 / V1-023** `workspace/didChangeConfiguration` and real config application
- [x] **LP-018 / V1-024** Workspace file operations: create, rename, delete
- [x] **LP-019 / V1-025** Basic diagnostics parity: undefined/unused/duplicate symbols
- [x] **LP-020 / V1-026** Type/member diagnostics: unknown members, visibility, static misuse, type compatibility
- [x] **LP-021 / VN-001** `textDocument/inlayHint`
- [x] **LP-022 / VN-002** `textDocument/prepareCallHierarchy` + incoming/outgoing calls
- [x] **LP-023 / VN-003** `textDocument/prepareTypeHierarchy` + supertypes/subtypes
- [x] **LP-024 / VN-004** `textDocument/implementation`
- [x] **LP-025 / VN-005** Multi-root workspace support
- [x] **LP-026 / VN-006** PHPStan diagnostics integration
- [x] **LP-027 / VN-007** Psalm diagnostics integration
- [x] **LP-028 / VN-008** `textDocument/codeLens`
- [x] **LP-029 / VN-009** `textDocument/foldingRange`

---

## Текущие задачи

- [x] **T-2026-05-25** Добавить новый milestone IDE intelligence/tooling expansion без ссылок на внешние проекты.
- [x] **T-2026-05-25** Добавить Monica в список локальных проектов для production validation.
- [x] **T-2026-05-25** Добавить список локальных проектов для production validation.
- [x] **T-2026-05-25** Разбить production-ready gaps на новый milestone задач для Codex.
- [x] **T-2026-05-19** Добавить `.semantic-search` в ignore и проверить статус `server/data/stubs`.
- [x] **T-2026-05-19** Добавить release/downloads badge в README и перенести нижний счётчик наверх.
- [x] **T-2026-05-19** Дополнить README полным набором badge для GitHub и VS Marketplace.
- [x] **T-2026-05-19** Добавить Rust MSRV badge из `server/Cargo.toml` в README.
- [x] **T-2026-05-19** Перенести блок новых задач в конец `TASKS.md`.
- [x] **T-2026-05-19** Проанализировать отсутствующие LSP-возможности относительно обычных LSP серверов.
- [x] **T-2026-05-19** Добавить отсутствующие LSP-возможности в roadmap `TASKS.md`.
- [x] **T-2026-05-19** Добавить отдельный tracking checklist для LSP parity задач.
- [x] **T-2026-05-19** Реализовать `LP-001 / V1-001` `textDocument/signatureHelp`.
- [x] **T-2026-05-19** Реализовать `LP-002 / V1-002` quick-fix `Add use`.
- [x] **T-2026-05-19** Реализовать `LP-003 / V1-003` `source.organizeImports`.
- [x] **T-2026-05-19** Реализовать `LP-004 / V1-004` code action `Add return type`.
- [x] **T-2026-05-19** Реализовать `LP-005 / V1-005` `textDocument/formatting`.
- [x] **T-2026-05-19** Реализовать `LP-006 / V1-006` `textDocument/rangeFormatting`.
- [x] **T-2026-05-19** Реализовать `LP-007 / V1-015` `textDocument/onTypeFormatting`.
- [x] **T-2026-05-19** Реализовать `LP-008 / V1-007` `textDocument/semanticTokens/full`.
- [x] **T-2026-05-19** Реализовать `LP-009 / V1-008` `textDocument/semanticTokens/full/delta`.
- [x] **T-2026-05-19** Реализовать `LP-010 / V1-016` `textDocument/declaration`.
- [x] **T-2026-05-19** Реализовать `LP-011 / V1-017` `textDocument/typeDefinition`.
- [x] **T-2026-05-19** Реализовать `LP-012 / V1-018` `textDocument/documentHighlight`.
- [x] **T-2026-05-19** Реализовать `LP-013 / V1-019` `textDocument/selectionRange`.
- [x] **T-2026-05-19** Реализовать `LP-014 / V1-020` `textDocument/linkedEditingRange`.
- [x] **T-2026-05-19** Реализовать `LP-015 / V1-021` Completion polish.
- [x] **T-2026-05-19** Реализовать `LP-016 / V1-022` `workspace/didChangeWatchedFiles`.
- [x] **T-2026-05-19** Реализовать `LP-017 / V1-023` `workspace/didChangeConfiguration`.
- [x] **T-2026-05-19** Реализовать `LP-018 / V1-024` Workspace file operations.
- [x] **T-2026-05-20** Реализовать `LP-019 / V1-025` Basic diagnostics parity.
- [x] **T-2026-05-20** Реализовать `LP-020 / V1-026` Type/member diagnostics.
- [x] **T-2026-05-20** Реализовать `LP-021 / VN-001` `textDocument/inlayHint`.
- [x] **T-2026-05-20** Реализовать `LP-022 / VN-002` call hierarchy.
- [x] **T-2026-05-20** Реализовать `LP-023 / VN-003` type hierarchy.
- [x] **T-2026-05-20** Реализовать `LP-024 / VN-004` `textDocument/implementation`.
- [x] **T-2026-05-20** Реализовать `LP-025 / VN-005` multi-root workspace support.
- [x] **T-2026-05-20** Реализовать `LP-026 / VN-006` PHPStan diagnostics integration.
- [x] **T-2026-05-20** Реализовать `LP-027 / VN-007` Psalm diagnostics integration.
- [x] **T-2026-05-20** Реализовать `LP-028 / VN-008` `textDocument/codeLens`.
- [x] **T-2026-05-20** Реализовать `LP-029 / VN-009` `textDocument/foldingRange`.
- [x] **T-2026-05-20** Актуализировать `README.md` по статусу, возможностям и настройкам.
- [x] **T-2026-05-20** Убрать упоминание `TASKS.md` из `README.md`.
- [x] **T-2026-05-20** Исправить ложные diagnostics `Static method called as instance method` на instance setters.
- [x] **T-2026-05-20** Подтягивать parent interfaces/classes при lazy indexing для diagnostics.
- [x] **T-2026-05-20** Учитывать методы из `use Trait;` при member diagnostics.
- [x] **T-2026-05-20** Подтягивать class return types методов при lazy diagnostics indexing.
- [x] **T-2026-05-20** Сделать kind-aware member resolution для diagnostics.
- [x] **T-2026-05-20** Исправить completion после member access, чтобы методы шли первыми.
- [x] **T-2026-05-20** Исправить ложные diagnostics в PHPUnit mock chains и `::class` в `EmailNotifierTest.php`.
- [x] **T-2026-05-20** Исправить ложный `Undefined variable` для value-переменной в `foreach`.
- [x] **T-2026-05-20** Ускорить публикацию diagnostics при правках открытого файла.
- [x] **T-2026-05-20** Исправить ложные diagnostics в `ChangeUserPasswordCommandTest.php`.
- [x] **T-2026-05-20** Прогнать php-lsp diagnostics по файлам `/home/apanov/Projects/bdpn-ui/app/tests`.
- [x] **T-2026-05-20** Снизить false positive diagnostics, найденные полным прогоном `app/tests`.
- [x] **T-2026-05-20** Обновить `README.md` с учетом текущего статуса и `Makefile`.
- [x] **T-2026-05-20** Добавить VS Code status bar popup со статусом индексации и полезной информацией расширения.
- [x] **T-2026-05-20** Добавить настройку `phpLsp.excludePaths` и учитывать ее при индексации.
- [x] **T-2026-05-20** Проверить, что все `phpLsp.*` настройки объявлены, передаются серверу и реально используются.
- [x] **T-2026-05-20** Протестировать php-lsp diagnostics по файлам `/home/apanov/Projects/bdpn-ui/app/src` и найти неточности.
- [x] **T-2026-05-20** Исправить ложные diagnostics, найденные прогоном `/home/apanov/Projects/bdpn-ui/app/src`.
- [x] **T-2026-05-20** Убрать hardcode имен framework methods из suppress unused-parameter diagnostics.
- [x] **T-2026-05-20** Проверить production-код на project-specific hardcode и убрать найденное.
- [x] **T-2026-05-21** Исправить GitHub downloads badge в README.
- [x] **T-2026-05-21** Изучить код проекта и составить список недостающих работ для production LSP сервера.
- [x] **T-2026-05-21** Добавить milestone production-readiness с подробным расписанием задач.
- [x] **T-2026-05-22** Обновить submodule `server/data/stubs` из upstream.
- [x] **T-2026-05-23** Разобраться, почему `go to definition` не работает на `parent` в `AbstractObjectNormalizer.php`.
- [x] **T-2026-05-23** Прогнать LSP-сервер по `/home/hightemp/ForTesting/symfony` и отловить ошибки работы.
- [x] **T-2026-05-23** Убрать Symfony-specific hardcode `is_symfony_configurator_file` из diagnostics.
- [x] **T-2026-05-23** Разобраться, почему completion не показывает методы `ReflectionMethod` для `$reflMethod->` в Symfony `CallbackValidator.php`.
- [x] **T-2026-05-23** Протестировать autocomplete по `/home/hightemp/ForTesting/symfony` на падения и некорректные ответы.
- [x] **T-2026-05-23** Исправить completion `Blank::` внутри chained call в Symfony `BlankValidator.php`.
- [x] **T-2026-05-23** Расширить Symfony autocomplete-аудит проверкой полноты ожидаемых labels по разным контекстам.
- [x] **T-2026-05-23** Разобрать остаточные Symfony autocomplete label misses: external PHPUnit symbols, `parent::` в anonymous class/trait, nullable/member chains.
- [x] **T-2026-05-23** Исправить autocomplete для member-chain, начинающейся с `(new ClassName())`.
- [x] **T-2026-05-23** Исправить resolution/completion `parent::` внутри anonymous class.
- [x] **T-2026-05-23** Довести Symfony autocomplete/go-to-definition audit-fix-verify цикл до стабильного состояния.
  - [x] Свежий аудит `/home/hightemp/ForTesting/symfony` по completion и definition.
  - [x] Исправить воспроизводимые баги без Symfony-specific hardcode.
  - [x] Добавить regression tests на исправленные случаи.
  - [x] Прогнать `fmt`, `test`, `clippy`, `release build`, `git diff --check`.
- [x] **T-2026-05-24** Разобраться, почему member autocomplete в `CallbackValidator.php` показывает переменные после `$reflMethod->setAccessible`.
- [x] **T-2026-05-24** Разобраться, почему go-to-definition не работает для `$this`.
- [x] **T-2026-05-24** Исправить VS Marketplace badge в README под текущий `publisher`/`name` из `client/package.json`.
- [x] **T-2026-05-24** Добавить публикацию VS Code extension в Marketplace в release workflow.
- [x] **T-2026-05-24** Исправить CI clippy failure `clippy::question_mark` в completion member-chain inference.
- [x] **T-2026-05-24** Переименовать VS Code extension package `name` в `ht-php-lsp` для Marketplace publish.
- [x] **T-2026-05-24** Бампнуть release version до `0.5.4` для публикации обновленного Marketplace package.
- [x] **T-2026-05-24** Исправить completion и go-to-definition для переменных, объявленных output-аргументом `preg_match` (`$matches` в Symfony `DateValidator.php`).
- [x] **T-2026-05-24** Разобраться и исправить отсутствие `README.md` в опубликованном VS Code extension package.
- [x] **T-2026-05-24** Добавить в `README.md` badge с поддерживаемыми версиями PHP.
- [x] **T-2026-05-24** Подготовить предыдущий релиз с README в VSIX.
- [x] **T-2026-05-24** Исправить Marketplace badges в `README.md`, которые показывают `retired badge`.
- [x] **T-2026-05-25** Актуализировать всю документацию после повторного прохода по проекту.
