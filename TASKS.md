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

- [ ] **H-006** Реально применить `phpVersion` из VS Code в сервере
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

- [ ] **H-008** PHPDoc parser: корректный разбор сложных типов и тегов
  - Проблема: текущий разбор `@param/@return/@var` обрезает тип до первого слова и неустойчив для `array<int, User>`, `(A&B)|null`, `callable(...)`, и похожих форм.
  - Что сделать:
    - Переписать извлечение type-expression из строки тега без `first_word` (учесть generic-скобки `<...>`, круглые скобки, `|`, `&`, `?`).
    - Сохранить текущую совместимость для простых случаев и malformed-тегов (не падать, игнорировать невалидное безопасно).
    - Расширить `parse_method_tag`: извлекать параметры и описание, а не только имя/return/static.
  - Критерии готовности:
    - Добавлены unit-тесты на сложные типы (включая пробелы внутри generic).
    - `@method` содержит распарсенные params (минимум имя + type если указан).
    - Все существующие тесты `phpdoc` проходят + новые regression-тесты.

- [ ] **H-009** PHPDoc model: разделить `@property`, `@property-read`, `@property-write`
  - Проблема: в модели нет различия read/write семантики virtual property; сейчас все варианты обрабатываются одинаково.
  - Что сделать:
    - Расширить структуру `PhpDocProperty` флагами доступа (`readable`/`writable`) или enum-kind.
    - Обновить parser и сериализацию типов.
    - Добавить миграционные правки по месту использования (hover/completion).
  - Критерии готовности:
    - Для трех тегов (`@property`, `@property-read`, `@property-write`) в тестах получаются разные значения access-mode.
    - Обратная совместимость: старые кейсы не ломаются.

- [ ] **H-010** UI для PHPDoc в LSP: показывать `@throws`, `@var`, `@property*`, `@method`
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

- [ ] **H-012** PHPDoc virtual members в completion/definition
  - Проблема: class-level `@property` и `@method` сейчас не участвуют в навигации/автодополнении как виртуальные члены.
  - Что сделать:
    - Подмешивать virtual members из PHPDoc класса в completion для `$obj->`.
    - Добавить go-to-definition/references на doc-объявление virtual member (минимальный MVP: definition на строку в doc-комментарии).
    - Для rename явно зафиксировать поведение (поддерживается/не поддерживается) и добавить guard.
  - Критерии готовности:
    - На `SupportedTags.php` видны `findById`/`label` в completion.
    - `Ctrl+Click` по virtual member возвращает definition в doc-comment.
    - Есть e2e/интеграционные тесты на completion + definition для virtual members.

- [ ] **H-013** E2E покрытие PHPDoc (fixture-driven)
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

- [ ] **V1-002** textDocument/codeAction — quick-fix: добавить use
  - Диагностика "unknown class" + code action "Add use statement"
  - Вставка `use FQN;` в блок use-statements
  - Разрешение конфликтов alias/import
  - Поддержка `function`/`const` imports там, где применимо

- [ ] **V1-003** textDocument/codeAction — organize imports
  - source.organizeImports
  - Сортировка use-statements алфавитно, удаление неиспользуемых
  - Группировка class/function/const imports
  - Сохранение комментариев рядом с use-блоком

- [ ] **V1-004** textDocument/codeAction — добавить return type
  - Если есть PHPDoc @return но нет return type hint
  - Не предлагать несовместимые типы для текущей target PHP version

### Formatting

- [ ] **V1-005** textDocument/formatting — внешний formatter
  - Интеграция: php-cs-fixer / phpcbf через subprocess
  - Конфигурация: phpLsp.formatting.provider + phpLsp.formatting.command
  - Возврат TextEdit без записи файла напрямую

- [ ] **V1-006** textDocument/rangeFormatting

- [ ] **V1-015** textDocument/onTypeFormatting
  - Автоформатирование после `;`, `}`, newline
  - Минимальные локальные edits без полного форматирования файла

### Semantic Tokens

- [ ] **V1-007** textDocument/semanticTokens/full
  - Legend: token types + modifiers по таблице в PRD
  - Обход CST, маппинг нод в semantic tokens

- [ ] **V1-008** textDocument/semanticTokens/full/delta
  - Инкрементальное обновление на основе previousResultId

### Navigation polish

- [ ] **V1-016** textDocument/declaration
  - Переход к декларации symbol/type alias/import, когда она отличается от definition
  - Fallback к definition для PHP-символов без отдельной declaration

- [ ] **V1-017** textDocument/typeDefinition
  - Для переменных/свойств/return values переходить к объявлению класса типа
  - Использовать PHPDoc `@var`, `@param`, `@return` как fallback

- [ ] **V1-018** textDocument/documentHighlight
  - Подсветка всех occurrences символа в текущем документе
  - Отдельно Read/Write для переменных и свойств где возможно

- [ ] **V1-019** textDocument/selectionRange
  - AST-based расширение выделения: identifier → expression → statement → block → class/function

- [ ] **V1-020** textDocument/linkedEditingRange
  - Связанные edits для парных PHP constructs где применимо
  - Минимум: namespace/use-safe rename внутри одного syntactic construct

### Completion polish

- [ ] **V1-021** Улучшить completion до привычного IDE-уровня
  - snippets для `class`, `interface`, `trait`, `enum`, `function`, control-flow constructs
  - `sortText`, `filterText`, `insertTextFormat`, `commitCharacters`
  - auto-import completion через `additionalTextEdits`
  - Не предлагать недоступные private/protected/static/instance members в неправильном контексте

### Workspace & configuration

- [ ] **V1-022** workspace/didChangeWatchedFiles и переиндексация изменённых файлов
  - Обрабатывать create/change/delete PHP файлов
  - Обновлять индекс без полного restart сервера
  - Удалять символы удалённых файлов из индекса

- [ ] **V1-023** workspace/didChangeConfiguration и применение настроек клиента
  - Реально использовать `phpVersion`, `diagnosticsMode`, `composerEnabled`, `indexVendor`, `stubExtensions`, `logLevel`
  - Поддержать изменение настроек без restart где возможно
  - Синхронизировать VS Code configuration schema и server initializationOptions

- [ ] **V1-024** Workspace file operations
  - Поддержать will/did create/rename/delete files где клиент это отдаёт
  - Обновлять URI символов при rename/move
  - Инвалидировать кэш и diagnostics для удалённых/перемещённых файлов

### Diagnostics parity

- [ ] **V1-025** Расширить базовые diagnostics
  - Undefined variables
  - Unused imports
  - Unused local variables/parameters
  - Duplicate symbols в workspace

- [ ] **V1-026** Type/member diagnostics
  - Unknown method/property/class constant
  - Visibility violations: private/protected access
  - Static/instance misuse
  - Basic type compatibility для параметров, return values, property assignments
  - Override/signature compatibility для inheritance
  - PHP-version-specific diagnostics

### Disk Cache

- [ ] **V1-009** Кэш индекса на диск
  - Формат: bincode
  - Путь: ~/.cache/php-lsp/{workspace-hash}/index.bin
  - Инвалидация: mtime + size файлов
  - Ускорение повторного запуска

### Performance

- [ ] **V1-010** Профилирование на Laravel проекте
  - Замер: время индексации, память, latency hover/completion
  - Оптимизация bottleneck'ов

- [ ] **V1-011** Lazy vendor indexing — оптимизация
  - Предзагрузка popular packages
  - LRU-кэш для vendor-файлов

### Documentation

- [ ] **V1-012** docs/architecture.md — потоки данных, диаграммы
- [ ] **V1-013** docs/lsp-features.md — таблица статусов LSP-фич
- [ ] **V1-014** README.md — полный (установка, настройки, troubleshooting)

---

## Этап vNext (ongoing)

- [ ] **VN-001** textDocument/inlayHint — типы параметров, return types inline
- [ ] **VN-002** textDocument/prepareCallHierarchy + incoming/outgoing
- [ ] **VN-003** textDocument/prepareTypeHierarchy + supertypes/subtypes
- [ ] **VN-004** textDocument/implementation (interface → concrete)
- [ ] **VN-005** Multi-root workspace поддержка
  - Использовать `workspaceFolders` вместо одного `rootUri`
  - Отдельные composer namespace maps и индексы по workspace folder
  - Корректные diagnostics/search/symbols across folders
- [ ] **VN-006** Интеграция PHPStan — subprocess + маппинг output → Diagnostics
- [ ] **VN-007** Интеграция Psalm — subprocess + маппинг output → Diagnostics
- [ ] **VN-008** Code Lens — количество ссылок на класс/метод
  - references count
  - test/run/debug codelens где применимо
- [ ] **VN-009** Folding Range — складывание функций, классов, PHPDoc
  - textDocument/foldingRange capability
  - Функции, методы, классы, namespaces, PHPDoc, массивы/blocks
- [x] **VN-010** Release pipeline — cross-platform VSIX сборка + публикация в Marketplace *(done 2026-02-15)*
  - `scripts/build-server.sh` — сборка Rust бинарника, копирование в `client/bin/`
  - `scripts/bundle-stubs.sh` — копирование phpstorm-stubs в `client/stubs/`
  - `.github/workflows/release.yml` — matrix build (7 платформ) + `vsce package --target`
  - `client/.vscodeignore` — включает только `bin/`, `stubs/`, `out/`, `package.json`
  - `extension.ts` — передаёт `stubsPath` в initializationOptions
  - `server.rs` — принимает `stubsPath` из initializationOptions для поиска стабов
  - Поддержанные платформы: linux-x64, linux-arm64, alpine-x64, darwin-x64, darwin-arm64, win32-x64, win32-arm64
  - Локальная сборка VSIX: 2.56 MB (бинарник + стабы + клиент)

- [x] **VN-011** Make release target — сборка + тегирование + push в GitHub *(done 2026-03-05)*
  - `VERSION` файл — единый источник версии
  - `make release` — берёт версию из `VERSION`, патчит `client/package.json` и `server/Cargo.toml`, собирает `package-all`, создаёт force-теग `v<VERSION>` и пушит на GitHub

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
- [ ] **LP-002 / V1-002** `textDocument/codeAction` — quick-fix: add use
- [ ] **LP-003 / V1-003** `source.organizeImports`
- [ ] **LP-004 / V1-004** `textDocument/codeAction` — add return type
- [ ] **LP-005 / V1-005** `textDocument/formatting`
- [ ] **LP-006 / V1-006** `textDocument/rangeFormatting`
- [ ] **LP-007 / V1-015** `textDocument/onTypeFormatting`
- [ ] **LP-008 / V1-007** `textDocument/semanticTokens/full`
- [ ] **LP-009 / V1-008** `textDocument/semanticTokens/full/delta`
- [ ] **LP-010 / V1-016** `textDocument/declaration`
- [ ] **LP-011 / V1-017** `textDocument/typeDefinition`
- [ ] **LP-012 / V1-018** `textDocument/documentHighlight`
- [ ] **LP-013 / V1-019** `textDocument/selectionRange`
- [ ] **LP-014 / V1-020** `textDocument/linkedEditingRange`
- [ ] **LP-015 / V1-021** Completion polish: snippets, sorting, auto-imports, visibility-aware members
- [ ] **LP-016 / V1-022** `workspace/didChangeWatchedFiles` and incremental reindex
- [ ] **LP-017 / V1-023** `workspace/didChangeConfiguration` and real config application
- [ ] **LP-018 / V1-024** Workspace file operations: create, rename, delete
- [ ] **LP-019 / V1-025** Basic diagnostics parity: undefined/unused/duplicate symbols
- [ ] **LP-020 / V1-026** Type/member diagnostics: unknown members, visibility, static misuse, type compatibility
- [ ] **LP-021 / VN-001** `textDocument/inlayHint`
- [ ] **LP-022 / VN-002** `textDocument/prepareCallHierarchy` + incoming/outgoing calls
- [ ] **LP-023 / VN-003** `textDocument/prepareTypeHierarchy` + supertypes/subtypes
- [ ] **LP-024 / VN-004** `textDocument/implementation`
- [ ] **LP-025 / VN-005** Multi-root workspace support
- [ ] **LP-026 / VN-006** PHPStan diagnostics integration
- [ ] **LP-027 / VN-007** Psalm diagnostics integration
- [ ] **LP-028 / VN-008** `textDocument/codeLens`
- [ ] **LP-029 / VN-009** `textDocument/foldingRange`

---

## Текущие задачи

- [x] **T-2026-05-19** Добавить `.semantic-search` в ignore и проверить статус `server/data/stubs`.
- [x] **T-2026-05-19** Добавить release/downloads badge в README и перенести нижний счётчик наверх.
- [x] **T-2026-05-19** Дополнить README полным набором badge для GitHub и VS Marketplace.
- [x] **T-2026-05-19** Добавить Rust MSRV badge из `server/Cargo.toml` в README.
- [x] **T-2026-05-19** Перенести блок новых задач в конец `TASKS.md`.
- [x] **T-2026-05-19** Проанализировать отсутствующие LSP-возможности относительно обычных LSP серверов.
- [x] **T-2026-05-19** Добавить отсутствующие LSP-возможности в roadmap `TASKS.md`.
- [x] **T-2026-05-19** Добавить отдельный tracking checklist для LSP parity задач.
- [x] **T-2026-05-19** Реализовать `LP-001 / V1-001` `textDocument/signatureHelp`.
