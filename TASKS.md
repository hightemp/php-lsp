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

---

## Этап v1 (4-6 недель после MVP)

### Signature Help

- [ ] **V1-001** textDocument/signatureHelp
  - triggerCharacters: ['(', ',']
  - Показать параметры функции/метода
  - Подсветить текущий параметр
  - PHPDoc @param

### Code Actions

- [ ] **V1-002** textDocument/codeAction — quick-fix: добавить use
  - Диагностика "unknown class" + code action "Add use statement"
  - Вставка `use FQN;` в блок use-statements

- [ ] **V1-003** textDocument/codeAction — organize imports
  - source.organizeImports
  - Сортировка use-statements алфавитно, удаление неиспользуемых

- [ ] **V1-004** textDocument/codeAction — добавить return type
  - Если есть PHPDoc @return но нет return type hint

### Formatting

- [ ] **V1-005** textDocument/formatting — внешний formatter
  - Интеграция: php-cs-fixer / phpcbf через subprocess
  - Конфигурация: phpLsp.formatting.provider + phpLsp.formatting.command

- [ ] **V1-006** textDocument/rangeFormatting

### Semantic Tokens

- [ ] **V1-007** textDocument/semanticTokens/full
  - Legend: token types + modifiers по таблице в PRD
  - Обход CST, маппинг нод в semantic tokens

- [ ] **V1-008** textDocument/semanticTokens/full/delta
  - Инкрементальное обновление на основе previousResultId

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
- [ ] **VN-006** Интеграция PHPStan — subprocess + маппинг output → Diagnostics
- [ ] **VN-007** Интеграция Psalm — subprocess + маппинг output → Diagnostics
- [ ] **VN-008** Code Lens — количество ссылок на класс/метод
- [ ] **VN-009** Folding Range — складывание функций, классов, PHPDoc
- [x] **VN-010** Release pipeline — cross-platform VSIX сборка + публикация в Marketplace *(done 2026-02-15)*
  - `scripts/build-server.sh` — сборка Rust бинарника, копирование в `client/bin/`
  - `scripts/bundle-stubs.sh` — копирование phpstorm-stubs в `client/stubs/`
  - `.github/workflows/release.yml` — matrix build (7 платформ) + `vsce package --target`
  - `client/.vscodeignore` — включает только `bin/`, `stubs/`, `out/`, `package.json`
  - `extension.ts` — передаёт `stubsPath` в initializationOptions
  - `server.rs` — принимает `stubsPath` из initializationOptions для поиска стабов
  - Поддержанные платформы: linux-x64, linux-arm64, alpine-x64, darwin-x64, darwin-arm64, win32-x64, win32-arm64
  - Локальная сборка VSIX: 2.56 MB (бинарник + стабы + клиент)

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
```
