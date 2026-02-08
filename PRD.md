# PHP Language Server (LSP 3.17) на Rust — PRD + SRS

## Метаданные

| Поле | Значение |
|------|----------|
| Проект | php-lsp |
| Версия документа | 1.0 |
| Дата | 2026-02-08 |
| Лицензия | MIT |
| LSP версия | 3.17 |
| Транспорт | stdio (JSON-RPC 2.0) |

---

## 1. Цели и границы

### 1.1 Цели

1. Предоставить пользователям VS Code IDE-уровня базовых функций для PHP-проектов (7.4+), включая Composer-проекты с PSR-4 autoload.
2. Обеспечить быстрое реагирование: инкрементальный парсинг (tree-sitter), debounce изменений, фоновая индексация без блокировки UX.
3. Кроссплатформенность: Windows (x64/arm64), macOS (x64/arm64), Linux (x64/arm64, glibc + musl).
4. Устойчивость к ошибкам синтаксиса: сервер продолжает работу и выдаёт полезные подсказки даже на битом коде.

### 1.2 Не-цели (явный scope-out)

| # | Не-цель | Обоснование |
|---|---------|-------------|
| 1 | Полная совместимость с PhpStorm | Нереалистично; цель — покрыть 80% частых сценариев |
| 2 | Выполнение PHP-кода / интерпретатор | Не нужен для LSP; потребовал бы runtime |
| 3 | Статический анализ уровня PHPStan/Psalm | Можно интегрировать как внешний инструмент позднее (vNext) |
| 4 | Поддержка Blade/Twig/других шаблонизаторов | Возможно в будущих версиях через embedded languages |
| 5 | Debugger / Xdebug интеграция | Отдельный протокол (DAP), вне scope |
| 6 | Refactoring уровня IDE (Extract Method, Move Class) | Сложность слишком высока для MVP/v1 |

---

## 2. Поддерживаемые платформы и ограничения

### 2.1 PHP-версии

| Версия | Статус | Ключевые синтаксические особенности для парсинга |
|--------|--------|--------------------------------------------------|
| 7.4 | Полная поддержка | Typed properties, arrow functions `fn()`, null coalescing assignment `??=`, spread in arrays |
| 8.0 | Полная поддержка | Union types `A\|B`, named arguments, match expression, nullsafe operator `?->`, attributes `#[...]`, constructor promotion, `throw` expression |
| 8.1 | Полная поддержка | Enums, fibers (как символ), intersection types `A&B`, readonly properties, `never` return type, first-class callable syntax `strlen(...)` |
| 8.2 | Полная поддержка | Readonly classes, DNF types `(A&B)\|C`, `true`/`false`/`null` standalone types, constants in traits |
| 8.3 | Полная поддержка | Typed class constants, `#[\Override]`, dynamic class constant fetch `$class::{$const}` |
| 8.4+ | Best-effort | Парсинг без падений, но новые конструкции могут не индексироваться полностью |

Настройка `phpLsp.phpVersion` влияет на:
- Какие диагностики выдаются (например, enum на 7.4 = ошибка)
- Какие completion-подсказки предлагаются

### 2.2 VS Code

- Минимальная версия: 1.75.0 (для стабильной поддержки `vscode-languageclient` v9+)
- Поддержка: актуальные стабильные версии

### 2.3 Серверная часть

- Язык: Rust stable, edition 2021, MSRV 1.75
- Async runtime: tokio
- Целевые платформы сборки:

| Target | Тройка | Примечание |
|--------|--------|-----------|
| Windows x64 | `x86_64-pc-windows-msvc` | Основная |
| Windows ARM64 | `aarch64-pc-windows-msvc` | Опциональная |
| macOS x64 | `x86_64-apple-darwin` | Intel Mac |
| macOS ARM64 | `aarch64-apple-darwin` | Apple Silicon |
| Linux x64 (glibc) | `x86_64-unknown-linux-gnu` | Основная |
| Linux ARM64 (glibc) | `aarch64-unknown-linux-gnu` | Для ARM серверов |
| Linux x64 (musl) | `x86_64-unknown-linux-musl` | Alpine/Docker |

---

## 3. LSP-функциональные требования

### 3.1 Жизненный цикл (все этапы)

| Метод/Нотификация | Направление | Этап | Описание |
|-------------------|-------------|------|----------|
| `initialize` | client→server | MVP | Обмен capabilities, возврат `ServerCapabilities` |
| `initialized` | client→server | MVP | Сигнал готовности; запуск фоновой индексации |
| `shutdown` | client→server | MVP | Остановка логики, подготовка к exit |
| `exit` | client→server | MVP | Завершение процесса (код 0 если был shutdown, иначе 1) |
| `$/cancelRequest` | bidirectional | MVP | Отмена запросов (возврат `RequestCancelled -32800`) |
| `window/logMessage` | server→client | MVP | Логирование |
| `window/showMessage` | server→client | MVP | Критические уведомления пользователю |
| `window/workDoneProgress/create` | server→client | MVP | Создание прогресс-бара индексации |
| `$/progress` | server→client | MVP | Обновление прогресса индексации |

### 3.2 MVP — обязательные LSP-методы

#### Синхронизация документов

| Метод | Описание | Детали реализации |
|-------|----------|-------------------|
| `textDocument/didOpen` | Документ открыт | Получить полный текст, распарсить tree-sitter (grammar `php` с поддержкой mixed HTML), обновить индекс файла, отправить диагностики |
| `textDocument/didChange` | Инкрементальные изменения | `TextDocumentSyncKind.Incremental (2)`. Применить дельты к буферу (ropey::Rope), перепарсить инкрементально через tree-sitter `parse(source, old_tree)`, обновить индекс файла, debounce диагностик (200мс) |
| `textDocument/didClose` | Документ закрыт | Освободить буфер открытого документа, переключиться на файловую версию |
| `textDocument/didSave` | Документ сохранён | `save.includeText = false`. Триггер для тяжёлых проверок |

#### Диагностика

| Метод | Описание | Детали реализации |
|-------|----------|-------------------|
| `textDocument/publishDiagnostics` | Отправка диагностик | `source: "php-lsp"`. Синтаксические ошибки от парсера (ERROR/MISSING ноды tree-sitter) + базовые семантические: неизвестный класс/функция/метод (если символ не найден в индексе), неразрешённый namespace/use. `severity`: Error для синтаксиса, Warning/Info для семантических |

#### Навигация

| Метод | Описание | Детали реализации |
|-------|----------|-------------------|
| `textDocument/hover` | Информация о символе | Тип/сигнатура + PHPDoc. Формат: `MarkupKind.Markdown`. Показать: FQN, параметры, return type, @param/@return из PHPDoc |
| `textDocument/definition` | Переход к определению | Класс → файл/строка определения. Функция/метод → определение. Property/const → определение. Поддержка: class, interface, trait, enum, function, method, property, class constant, global constant |
| `textDocument/references` | Поиск всех ссылок | Поиск по индексу workspace. Параметр `includeDeclaration`. Поддержка тех же символов, что и definition |
| `textDocument/rename` | Переименование символа | `prepareProvider: true` для валидации позиции. Возврат `WorkspaceEdit` с текстовыми правками во всех файлах. Проверки: имя не пустое, нет коллизий, позиция на переименовываемом символе |

#### Completion

| Метод | Описание | Детали реализации |
|-------|----------|-------------------|
| `textDocument/completion` | Автодополнение | `triggerCharacters: ['$', '>', ':', '\\']`. Контексты: (1) после `->` / `?->` — методы/свойства по типу объекта (best-effort); (2) после `::` — статические методы/свойства/константы; (3) после `\` — namespace completion; (4) после `$` — локальные переменные; (5) свободный контекст — функции, классы, ключевые слова PHP. `resolveProvider: true` для ленивой подгрузки документации |
| `completionItem/resolve` | Детали элемента | Подгрузить PHPDoc, полную сигнатуру, deprecated-статус |

#### Символы

| Метод | Описание | Детали реализации |
|-------|----------|-------------------|
| `textDocument/documentSymbol` | Символы документа | Иерархический формат (`DocumentSymbol[]`): namespace → class → method/property/const. SymbolKind: Class(5), Method(6), Property(7), Constructor(9), Enum(10), Interface(11), Function(12), Variable(13), Constant(14), EnumMember(22) |
| `workspace/symbol` | Поиск символов workspace | Fuzzy-match по query. Возврат `WorkspaceSymbol[]` с location |

#### Трейсинг

- Сервер поддерживает параметр `trace` из `InitializeParams` (`off`/`messages`/`verbose`)
- При `verbose` — логировать полные JSON-RPC сообщения через `$/logTrace`
- Совместимость с `phpLsp.trace.server` настройкой в VS Code (стандартный механизм `vscode-languageclient`)

### 3.3 v1 — желательные LSP-методы

| Метод | Описание | Детали реализации |
|-------|----------|-------------------|
| `textDocument/signatureHelp` | Подсказка параметров | `triggerCharacters: ['(', ',']`, `retriggerCharacters: [',']`. Показать параметры функции/метода, подсветить текущий |
| `textDocument/codeAction` | Code actions | `codeActionKinds: ['quickfix', 'source.organizeImports']`. Quick-fix: добавить `use`, добавить return type. Organize imports: сортировка `use` statements |
| `textDocument/formatting` | Форматирование | Интеграция с внешним formatter (php-cs-fixer / phpcbf) через конфиг |
| `textDocument/rangeFormatting` | Форматирование диапазона | Аналогично formatting, но с передачей range |
| `textDocument/semanticTokens/full` | Семантическая подсветка | Полный набор токенов для файла |
| `textDocument/semanticTokens/full/delta` | Дельта семантических токенов | Инкрементальное обновление |

#### Semantic Tokens — стратегия для PHP

Типы токенов (legend):

| Индекс | Тип | PHP-применение |
|--------|-----|----------------|
| 0 | `namespace` | Namespace имена |
| 1 | `class` | Имена классов |
| 2 | `enum` | PHP enums |
| 3 | `interface` | Интерфейсы |
| 4 | `type` | Type aliases (будущее) |
| 5 | `typeParameter` | Generic (будущее) |
| 6 | `parameter` | Параметры функций |
| 7 | `variable` | Локальные переменные ($var) |
| 8 | `property` | Свойства классов |
| 9 | `enumMember` | Enum cases |
| 10 | `function` | Функции |
| 11 | `method` | Методы классов |
| 12 | `keyword` | Ключевые слова PHP |
| 13 | `comment` | Комментарии/PHPDoc |
| 14 | `string` | Строки |
| 15 | `number` | Числа |
| 16 | `operator` | Операторы |
| 17 | `decorator` | Атрибуты `#[...]` |

Модификаторы:

| Бит | Модификатор | Применение |
|-----|-------------|-----------|
| 0 | `declaration` | Места определений |
| 1 | `definition` | Определения |
| 2 | `readonly` | readonly свойства/классы |
| 3 | `static` | Статические методы/свойства |
| 4 | `deprecated` | @deprecated из PHPDoc |
| 5 | `abstract` | abstract класс/метод |
| 6 | `modification` | Присваивание переменной |
| 7 | `defaultLibrary` | Built-in PHP функции/классы |

### 3.4 vNext — перспективные LSP-методы

| Метод | Описание |
|-------|----------|
| `textDocument/inlayHint` | Подсказки типов параметров и return types inline |
| `textDocument/prepareCallHierarchy` + incoming/outgoing | Иерархия вызовов |
| `textDocument/prepareTypeHierarchy` + supertypes/subtypes | Иерархия типов |
| `textDocument/implementation` | Go to Implementation (interface → concrete classes) |
| Интеграция PHPStan/Psalm | Внешний процесс, маппинг вывода на Diagnostics |

---

## 4. Парсинг и AST

### 4.1 Решение: tree-sitter-php (основная стратегия)

**Обоснование:**
1. **Инкрементальный парсинг** — критически важен для LSP. При каждом нажатии клавиши tree-sitter перепарсит только изменившееся поддерево за <1мс.
2. **Проверенная error recovery** — на битом коде CST содержит ERROR-ноды, но остальное дерево валидно.
3. **Боевая зрелость** — используется GitHub, Neovim, Zed. 207 stars, 36 контрибуторов, 642 коммита. Покрывает PHP 7.4–8.3.
4. Используется grammar `php` (не `php_only`) для поддержки mixed PHP/HTML файлов.

**Альтернатива (для мониторинга):** crate `php-parser` (wudi) — 0.1.x, 22x быстрее tree-sitter, нативный AST, fault-tolerant, но нет инкрементального парсинга, 1 автор, 2 месяца возраста. Также стоит мониторить парсер из Mago (2800 stars, JetBrains-спонсор).

### 4.2 Требования к парсеру

- Error recovery: частичный CST при синтаксических ошибках (ERROR/MISSING ноды)
- Стабильные позиции/диапазоны (byte offsets + row:col) для маппинга в LSP Range
- Быстрая обработка didChange: инкрементальный reparse через `tree.edit()` + `parser.parse(source, old_tree)`
- Буфер документа: `ropey::Rope` для O(log n) вставок/удалений

### 4.3 Поток данных парсинга

```
didChange(deltas)
  → apply_edits(Rope, deltas)
  → compute InputEdit (byte offsets + Point)
  → tree.edit(&input_edit)
  → parser.parse(rope_to_str, old_tree)  // инкрементально
  → new CST (Tree)
  → extract_symbols(CST) → обновить FileSymbols в индексе
  → extract_diagnostics(CST) → debounce → publishDiagnostics
```

---

## 5. Семантическая модель / индекс

### 5.1 Глобальный индекс символов

Центральная структура для hover, completion, definition, references, rename.

Хранит:
- **types**: FQN → SymbolInfo (классы, интерфейсы, трейты, enum)
- **functions**: FQN → SymbolInfo
- **constants**: FQN → SymbolInfo
- **file_symbols**: URI файла → список символов (для инкрементального обновления)
- **references**: FQN → список Location где символ используется
- **namespace_map**: маппинг из composer.json

Реализация: `DashMap` для lock-free concurrent access.

Стратегия инкрементального обновления:
1. При `didChange` → перепарсить файл → извлечь символы → `index.update_file(uri, new_symbols)`
2. `update_file` удаляет старые символы файла, добавляет новые
3. При `didClose` без сохранения → откатиться к дисковой версии
4. Новые файлы в workspace → `workspace/didChangeWatchedFiles` → парсить и добавить

Кэш на диск (v1):
- Формат: bincode, один файл на workspace
- Инвалидация: хэш (mtime + size) каждого файла
- Путь: `~/.cache/php-lsp/{workspace-hash}/index.bin`

### 5.2 Composer/autoload

Поддержка `composer.json`:
1. Парсинг `composer.json` в корне workspace + `vendor/composer/installed.json`
2. Извлечение `autoload` и `autoload-dev` секций
3. PSR-4 (основной): `App\\` → `src/` → `App\Service\Foo` ищется в `src/Service/Foo.php`
4. PSR-0: аналогично, но с underscore-маппингом
5. classmap: сканировать директории, построить map класс→файл
6. files: парсить как глобальные функции/константы

Vendor-зависимости (MVP): **lazy-индексация** — парсить vendor-файл по запросу при первом resolve неизвестного символа. Конфиг `phpLsp.indexVendor`.

### 5.3 Встроенные символы PHP (stubs)

Источник: **JetBrains phpstorm-stubs** (Apache-2.0, CC-BY 3.0 для PHPDoc)

Стратегия:
1. Git submodule в `server/data/stubs`
2. При первом запуске — парсить stubs tree-sitter, построить индекс built-in символов
3. Кэшировать результат на диск
4. Пометить символы модификатором `defaultLibrary`
5. Конфиг `phpLsp.stubs.extensions` — какие расширения подключить (по умолчанию ~30 основных)

### 5.4 PHPDoc парсинг

Свой мини-парсер (regex/nom) для базовых тегов:
- `@param Type $name Description`
- `@return Type Description`
- `@var Type`
- `@throws Type`
- `@deprecated [Description]`
- `@property Type $name`
- `@method ReturnType name(params)`

Не поддерживаются в MVP: `@template`, `@psalm-*`, `@phpstan-*`, generics.

---

## 6. Архитектура проекта

### 6.1 Структура monorepo

```
php-lsp/
├── server/                          # Rust workspace
│   ├── Cargo.toml                   # workspace root
│   ├── crates/
│   │   ├── php-lsp-server/          # Главный бинарник — точка входа
│   │   │   ├── Cargo.toml
│   │   │   └── src/
│   │   │       ├── main.rs          # tokio::main, stdio transport
│   │   │       ├── server.rs        # LanguageServer trait impl
│   │   │       └── capabilities.rs  # ServerCapabilities формирование
│   │   │
│   │   ├── php-lsp-parser/          # Парсинг (tree-sitter wrapper)
│   │   │   ├── Cargo.toml
│   │   │   └── src/
│   │   │       ├── lib.rs
│   │   │       ├── parser.rs        # FileParser (tree-sitter + Rope)
│   │   │       ├── symbols.rs       # CST → SymbolInfo extraction
│   │   │       ├── diagnostics.rs   # CST → Diagnostic extraction
│   │   │       └── phpdoc.rs        # PHPDoc мини-парсер
│   │   │
│   │   ├── php-lsp-index/           # Индекс / семантическая модель
│   │   │   ├── Cargo.toml
│   │   │   └── src/
│   │   │       ├── lib.rs
│   │   │       ├── workspace.rs     # WorkspaceIndex
│   │   │       ├── symbols.rs       # SymbolInfo, TypeInfo, etc.
│   │   │       ├── resolver.rs      # Разрешение имён (FQN, use, aliasing)
│   │   │       ├── composer.rs      # Парсинг composer.json
│   │   │       └── stubs.rs         # Загрузка phpstorm-stubs
│   │   │
│   │   ├── php-lsp-completion/      # Completion engine
│   │   │   ├── Cargo.toml
│   │   │   └── src/
│   │   │       ├── lib.rs
│   │   │       ├── context.rs       # Определение контекста
│   │   │       └── providers.rs     # Провайдеры completion
│   │   │
│   │   └── php-lsp-types/           # Общие типы данных
│   │       ├── Cargo.toml
│   │       └── src/lib.rs
│   │
│   └── data/
│       └── stubs/                   # phpstorm-stubs (git submodule)
│
├── client/                          # VS Code extension (TypeScript)
│   ├── package.json
│   ├── tsconfig.json
│   ├── esbuild.mjs
│   └── src/
│       └── extension.ts             # activate/deactivate
│
├── test-fixtures/                   # Тестовые PHP-проекты
│   ├── basic/
│   ├── composer-psr4/
│   └── syntax-errors/
│
├── .github/workflows/
│   └── ci.yml
│
├── PRD.md
├── TASKS.md
├── DECISIONS.md
├── LICENSE
└── README.md
```

### 6.2 LSP-фреймворк: tower-lsp-server v0.23+

Community fork оригинального tower-lsp (оригинал заброшен с 2023).

Обоснование:
- Крупнейшая экосистема — используется Biome, Oxc, Harper, Veryl
- ~43K downloads/month, активная поддержка
- Простой API: `LanguageServer` trait → `LspService::new()` → `Server::serve()`
- Нативная поддержка tokio
- Обновлённые `lsp-types` 0.97+

Известное ограничение: нотификации обрабатываются асинхронно (возможен out-of-order). Решение: собственная очередь для didChange через `tokio::sync::mpsc` с ordering по `version`.

### 6.3 Конкурентность

Разделение "быстрого" и "тяжёлого" путей:

1. **Fast path** (hover, completion, definition, signatureHelp):
   - Inline в обработчике запроса
   - Читает snapshot индекса (lock-free через DashMap)
   - Целевая латентность: <50мс (p95)

2. **Medium path** (didChange, diagnostics, single-file index update):
   - didChange → debounce через tokio::time::sleep (200мс)
   - После debounce: reparse, update file_symbols, publishDiagnostics
   - Отмена предыдущего debounce при новом didChange

3. **Heavy path** (workspace indexing, references, rename):
   - Background tasks через tokio::spawn
   - Workspace indexing: параллельный обход через семафор
   - `$/cancelRequest` через CancellationToken (tokio-util)

---

## 7. Конфигурация (VS Code Settings)

```jsonc
{
  "phpLsp.enable": {
    "type": "boolean", "default": true,
    "scope": "resource"
  },
  "phpLsp.phpVersion": {
    "type": "string", "default": "8.2",
    "enum": ["7.4", "8.0", "8.1", "8.2", "8.3", "8.4"],
    "scope": "resource"
  },
  "phpLsp.serverPath": {
    "type": "string", "default": "",
    "scope": "machine",
    "description": "Custom path to php-lsp binary (leave empty for bundled)"
  },
  "phpLsp.includePaths": {
    "type": "array", "items": {"type": "string"}, "default": [],
    "scope": "resource"
  },
  "phpLsp.stubs.path": {
    "type": "string", "default": "",
    "scope": "machine"
  },
  "phpLsp.stubs.extensions": {
    "type": "array",
    "default": ["Core", "SPL", "standard", "pcre", "date", "json",
      "mbstring", "ctype", "tokenizer", "dom", "SimpleXML", "PDO",
      "curl", "filter", "hash", "session", "Reflection", "intl",
      "fileinfo", "openssl", "phar", "xml", "xmlreader", "xmlwriter",
      "zip", "zlib", "bcmath", "gd", "iconv", "mysqli", "sodium"],
    "scope": "resource"
  },
  "phpLsp.composer.enabled": {
    "type": "boolean", "default": true, "scope": "resource"
  },
  "phpLsp.composer.path": {
    "type": "string", "default": "composer.json", "scope": "resource"
  },
  "phpLsp.indexVendor": {
    "type": "boolean", "default": true, "scope": "resource"
  },
  "phpLsp.diagnostics.mode": {
    "type": "string", "default": "basic-semantic",
    "enum": ["off", "syntax-only", "basic-semantic"],
    "scope": "resource"
  },
  "phpLsp.diagnostics.maxProblems": {
    "type": "number", "default": 100, "scope": "resource"
  },
  "phpLsp.formatting.provider": {
    "type": "string", "default": "none",
    "enum": ["none", "php-cs-fixer", "phpcbf"],
    "scope": "resource"
  },
  "phpLsp.trace.server": {
    "type": "string", "enum": ["off", "messages", "verbose"],
    "default": "off", "scope": "window"
  },
  "phpLsp.logLevel": {
    "type": "string", "enum": ["error", "warn", "info", "debug", "trace"],
    "default": "info", "scope": "window"
  }
}
```

---

## 8. Нефункциональные требования (SLO)

### 8.1 Производительность

| Метрика | Цель | Как измерять |
|---------|------|-------------|
| First index: 100 файлов | <2с | Таймер от `initialized` до завершения background indexing |
| First index: 1000 файлов | <10с | Аналогично |
| First index: 10000 файлов (Laravel) | <60с | Аналогично |
| Hover latency (p50) | <30мс | LSP trace log: timestamp запрос→ответ |
| Hover latency (p95) | <100мс | Аналогично |
| Completion latency (p50) | <50мс | Аналогично |
| Completion latency (p95) | <150мс | Аналогично |
| Definition latency (p95) | <50мс | Аналогично |
| didChange processing | <50мс | Внутренний таймер (parse + index update) |
| Diagnostics after edit | <500мс | Включая debounce 200мс |

### 8.2 Память

| Workspace | Целевой RSS | Примечание |
|-----------|-------------|-----------|
| 100 файлов | <50 MB | Мелкий проект |
| 1000 файлов | <200 MB | Средний проект |
| 10000 файлов | <800 MB | Крупный (Laravel + vendor) |
| + stubs | +30-50 MB | Фиксированная доплата |

### 8.3 Устойчивость

| Требование | Acceptance criteria |
|-----------|---------------------|
| Не падает на битом коде | Файл с 50 синт. ошибками → сервер работает, hover на валидных участках |
| Не падает при быстром наборе | 100 didChange за 1с → нет OOM, нет hang |
| Ошибки логируются | Паники перехвачены через catch_unwind, логируются |
| Graceful shutdown | shutdown → exit за <1с |
| Некорректный JSON-RPC | Возврат ParseError (-32700), сервер продолжает |

---

## 9. Тестирование и качество

### 9.1 Unit-тесты

| Модуль | Что тестируется |
|--------|-----------------|
| php-lsp-parser | Парсинг PHP → CST → символы; Error recovery; Инкрементальный edit |
| php-lsp-index | WorkspaceIndex CRUD; Composer parsing; Name resolution |
| php-lsp-completion | Контекст-определение; Провайдеры |
| php-lsp-types | TypeInfo parsing; PHPDoc парсинг |

### 9.2 Integration-тесты LSP

In-process mock client (без spawn процесса):

| Сценарий | Шаги |
|----------|------|
| Open → Diagnostics | didOpen файл с ошибкой → publishDiagnostics |
| Open → Hover | didOpen → hover на классе → FQN + PHPDoc |
| Open → Definition | didOpen два файла → definition → Location |
| Change → Diagnostics | didOpen → didChange (ввести ошибку) → новые диагностики |
| Completion members | didOpen → completion после `$this->` → методы/свойства |
| Rename | didOpen → rename → WorkspaceEdit |
| Cancel | references + cancelRequest → RequestCancelled |
| Shutdown | shutdown → exit → код 0 |

### 9.3 Golden tests

- Директория `test-fixtures/golden/`
- Каждый тест: `input.php` + `expected_*.json`
- CI сравнивает фактический вывод с ожидаемым

### 9.4 Тест-проекты

| Проект | Цель |
|--------|------|
| `basic/` | Минимальный PHP файл |
| `composer-psr4/` | PSR-4 autoload, cross-file |
| `syntax-errors/` | Намеренно битый код, error recovery |

---

## 10. Сборка, релизы, доставка

### 10.1 CI Pipeline (GitHub Actions)

1. lint: `cargo clippy` + `cargo fmt --check` + eslint (client)
2. test: `cargo test` + client tests
3. build: matrix для всех target-платформ
4. package: `vsce package --target <platform>` (на release)
5. publish: `vsce publish` (на release tag)

### 10.2 Доставка бинарника

Platform-specific VSIX:
- Каждый VSIX содержит один бинарник под свою платформу в `bin/`
- Marketplace отдаёт нужный VSIX автоматически
- Fallback: `phpLsp.serverPath` для пользовательского бинарника

### 10.3 Обновление

- VS Code обновляет расширения автоматически → новый VSIX = новый бинарник
- `window/showMessage` при первом запуске новой версии

---

## 11. Acceptance Criteria — чек-лист

### Автоматические (CI)

- [ ] `cargo clippy --all-targets` — 0 warnings
- [ ] `cargo test --all` — 100% passed
- [ ] `npm test` (client) — 100% passed
- [ ] Golden tests — все совпадают
- [ ] Build на всех платформах

### Ручные сценарии в VS Code (MVP)

- [ ] **S1 Установка**: расширение устанавливается, Output channel "PHP Language Server" показывает initialized
- [ ] **S2 Ошибки**: файл с `function foo( { }` → подчёркнутая ошибка; исправить → исчезает за <1с
- [ ] **S3 Hover**: hover на классе → FQN + PHPDoc; на `strlen` → сигнатура из stubs
- [ ] **S4 Definition**: Ctrl+Click на класс → переход к определению; на `strlen` → stub
- [ ] **S5 Completion**: `$this->` → методы/свойства; `Foo::` → статики; `$` → переменные; `array_` → built-in
- [ ] **S6 References**: Find All References на классе → все использования
- [ ] **S7 Rename**: F2 на методе → правки во всех файлах; на ключевом слове → отказ
- [ ] **S8 Symbols**: Ctrl+Shift+O → иерархия; Ctrl+T → workspace search
- [ ] **S9 Composer**: PSR-4 проект → cross-file навигация работает
- [ ] **S10 Устойчивость**: 50 ошибок → работает; быстрый набор → нет зависаний
