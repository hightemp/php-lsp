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

- [ ] **M-001** Инициализация репозитория
  - git init, .gitignore (Rust + Node + VS Code)
  - LICENSE (MIT)
  - README.md (минимальный: что это, как собрать)

- [ ] **M-002** Cargo workspace
  - Корневой `server/Cargo.toml` (workspace)
  - Crate `php-lsp-types` — общие типы (SymbolKind, TypeInfo, Visibility)
  - Crate `php-lsp-parser` — tree-sitter-php обёртка
  - Crate `php-lsp-index` — заглушка
  - Crate `php-lsp-completion` — заглушка
  - Crate `php-lsp-server` — точка входа (main.rs)

- [ ] **M-003** VS Code extension scaffold
  - `client/package.json` (activationEvents, contributes.configuration, vscode-languageclient)
  - `client/tsconfig.json`
  - `client/esbuild.mjs`
  - `client/src/extension.ts` (activate/deactivate, LanguageClient с stdio)

- [ ] **M-004** GitHub Actions CI
  - Workflow: cargo clippy + cargo fmt --check + cargo test
  - Workflow: npm ci + npm run build (client)
  - Matrix: ubuntu-latest (основной)

- [ ] **M-005** LSP hello-world
  - `main.rs`: tokio::main, tower-lsp-server, stdio transport
  - `server.rs`: LanguageServer trait — initialize (возврат ServerCapabilities), shutdown, exit
  - Проверка: клиент запускает сервер, Output channel показывает initialized

- [ ] **M-006** Интеграция tree-sitter-php
  - `parser.rs`: FileParser struct (tree_sitter::Parser + ropey::Rope + Tree)
  - `parse_full(source)` — полный парсинг
  - `apply_edit(TextDocumentContentChangeEvent)` — инкрементальный
  - Unit-тесты: парсинг класса, функции, error recovery

### Неделя 2: Document Sync + Index Core + Diagnostics

- [ ] **M-007** didOpen / didChange / didClose / didSave
  - Менеджер открытых документов (DashMap<Url, FileParser>)
  - didOpen: parse_full → сохранить в map
  - didChange: apply_edit (incremental, TextDocumentSyncKind=2)
  - didClose: удалить из map
  - didSave: noop (пока)
  - Debounce didChange: 200мс перед diagnostics

- [ ] **M-008** Diagnostics (синтаксические)
  - Обход CST: найти ERROR и MISSING ноды tree-sitter
  - Маппинг в lsp_types::Diagnostic (range, severity=Error, source="php-lsp")
  - publishDiagnostics после debounce
  - Тесты: файл с ошибками → корректные диагностики

- [ ] **M-009** Индекс — структуры данных
  - `php-lsp-types`: SymbolInfo, SymbolKind, Visibility, SymbolModifiers, Signature, ParamInfo, TypeInfo
  - `php-lsp-index/workspace.rs`: WorkspaceIndex (DashMap-based)
  - API: update_file, remove_file, resolve_fqn, search, find_references
  - Unit-тесты CRUD

- [ ] **M-010** Symbol extraction из CST
  - `php-lsp-parser/symbols.rs`: обход CST tree-sitter
  - Извлечение: class, interface, trait, enum, function, method, property, class_constant, global constant
  - Извлечение: namespace, use statements
  - Извлечение: visibility, modifiers (static, abstract, readonly, final)
  - Извлечение: type hints (параметры, return, свойства)
  - Тесты на каждый тип символа

### Неделя 3: Composer + Hover + Definition + Stubs

- [ ] **M-011** Composer.json парсинг
  - `php-lsp-index/composer.rs`: парсинг composer.json (serde_json)
  - Извлечение autoload/autoload-dev: psr-4, psr-0, classmap, files
  - NamespaceMap: prefix → directory
  - Тесты на реальные composer.json

- [ ] **M-012** Workspace индексация (background)
  - При `initialized`: запуск фоновой задачи
  - Обход файлов workspace по composer namespace map
  - Парсинг каждого .php файла → extract_symbols → update_file
  - Progress reporting: window/workDoneProgress/create + $/progress
  - Семафор для ограничения параллелизма

- [ ] **M-013** phpstorm-stubs
  - Git submodule: server/data/stubs → JetBrains/phpstorm-stubs
  - Загрузка при старте: парсинг stubs для расширений из конфига
  - Добавление в индекс с модификатором defaultLibrary
  - Кэширование (опционально, можно в v1)

- [ ] **M-014** textDocument/hover
  - Определение символа под курсором (CST node → FQN)
  - Поиск в индексе: resolve_fqn
  - Формирование Hover: Markdown с FQN, сигнатурой, PHPDoc
  - Тесты: hover на классе, методе, built-in функции

- [ ] **M-015** textDocument/definition
  - Определение символа под курсором → FQN
  - Поиск в индексе → Location (uri + range)
  - Поддержка: class, interface, trait, enum, function, method, property, const
  - Тесты: cross-file definition

- [ ] **M-016** PHPDoc мини-парсер
  - `php-lsp-parser/phpdoc.rs`: парсинг doc-комментариев
  - Теги: @param, @return, @var, @throws, @deprecated, @property, @method
  - Тесты на различные форматы

### Неделя 4: Completion + References + Rename + Symbols + Polish

- [ ] **M-017** textDocument/completion
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

- [ ] **M-018** completionItem/resolve
  - Подгрузка PHPDoc, полной сигнатуры, deprecated
  - Тест

- [ ] **M-019** textDocument/references
  - Определение символа → FQN
  - Поиск по индексу references
  - Параметр includeDeclaration
  - Тест: все ссылки на класс в workspace

- [ ] **M-020** textDocument/rename + prepareRename
  - prepareRename: валидация позиции (возврат null на ключевых словах)
  - rename: собрать все ссылки + определение → WorkspaceEdit
  - Проверки: имя не пустое, нет коллизий
  - Тесты

- [ ] **M-021** textDocument/documentSymbol
  - Иерархический формат (DocumentSymbol[])
  - namespace → class → method/property/const
  - Тест

- [ ] **M-022** workspace/symbol
  - Fuzzy-match по query в глобальном индексе
  - Возврат WorkspaceSymbol[] с location
  - Тест

- [ ] **M-023** Vendor lazy indexing
  - При resolve_fqn не найден → проверить namespace_map → найти файл в vendor → парсить on-demand
  - Кэшировать распарсенные vendor-файлы
  - Тест

- [ ] **M-024** Семантические диагностики (базовые)
  - Неизвестный класс (не найден в индексе) — Warning
  - Неизвестная функция — Warning
  - Неразрешённый use — Warning
  - Тесты

- [ ] **M-025** Трейсинг и логирование
  - Поддержка trace из InitializeParams (off/messages/verbose)
  - $/logTrace при verbose
  - window/logMessage для важных событий
  - logLevel из конфига

- [ ] **M-026** End-to-end тестирование
  - In-process mock client тесты
  - Сценарии: open→diagnostics, hover, definition, completion, rename, shutdown
  - Golden tests для diagnostics/completion на fixtures

- [ ] **M-027** Тест-fixtures
  - test-fixtures/basic/ — минимальный PHP
  - test-fixtures/composer-psr4/ — PSR-4 с composer.json
  - test-fixtures/syntax-errors/ — битый код

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
- [ ] **VN-010** Release pipeline — cross-platform VSIX сборка + публикация в Marketplace

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
