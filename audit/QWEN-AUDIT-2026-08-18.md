# PHP LSP Code Audit Report

**Date:** 2026-08-18  
**Auditor:** Qwen3.5-397B  
**Project:** php-lsp (PHP Language Server Protocol implementation)  
**Scope:** Full codebase review (Rust server, TypeScript client, PHP fixtures, documentation)

---

## Executive Summary

This audit identified **127 issues** across 6 categories:

| Category | Count | Severity Distribution |
|----------|-------|----------------------|
| Code Quality Issues | 42 | 15 High, 18 Medium, 9 Low |
| Test Coverage Gaps | 31 | 8 High, 14 Medium, 9 Low |
| Security Concerns | 12 | 5 High, 4 Medium, 3 Low |
| Performance Issues | 14 | 3 High, 7 Medium, 4 Low |
| Memory Safety | 8 | 2 High, 4 Medium, 2 Low |
| Documentation Gaps | 20 | 3 High, 10 Medium, 7 Low |

**Critical findings:**
- Zero test coverage for TypeScript client (37K+ lines)
- Unbounded caches in production code
- Command injection risk via project configuration
- Race conditions in async state management
- Missing security model documentation

> **Комментарий Codex (итог первичной верификации): сводка и число 127 не воспроизводятся из тела отчёта.** Подтверждено отсутствие client tests, но весь перечисленный TypeScript source занимает 1642 строки, а не 50K+/37K. «Unbounded» `RequestTypeCache` request-local, Twig order очищается; project commands закрыты explicit trust gate; заявленные async races защищены per-document entry/generation и lifecycle reconciliation; command trust уже документирован. Поэтому critical summary нельзя использовать как backlog без приведённых ниже постатейных поправок.

---

## 1. Code Quality Issues

### 1.1 Long Functions / Complex Logic

#### Rust Server

**File:** `server/crates/php-lsp-server/src/server.rs:1240-1378`
```rust
// ResolvedRuntimeConfiguration::from_settings() - 138 lines
// Excessive nested if-let statements
```
**Issue:** Function should be split into focused methods: `parse_formatting_config()`, `parse_phpstan_config()`, `parse_psalm_config()`.

> **Комментарий Codex (проверено 2026-08-18): частично подтверждено, severity — low.** `ResolvedRuntimeConfiguration::from_settings()` действительно длинная, но это линейное заполнение структуры без глубокой вложенности и без найденного дефекта. Вынос трёх парсеров может улучшить читаемость, однако это обычный рефакторинг, а не high-severity проблема.

**File:** `server/crates/php-lsp-server/src/lsp/diagnostics.rs:195-400+`
```rust
// parse_phpstan_json_diagnostics() and related functions
// Complex nested logic for filtering and path matching
```
**Issue:** Analyzer parsing functions are extremely long with complex nested logic.

> **Комментарий Codex: не подтверждено.** Актуальная `parse_phpstan_json_diagnostics()` занимает около 30 строк, а сложные операции уже вынесены в `AnalyzerTargetPathMatcher`, `phpstan_message_to_diagnostic()` и `parse_analyzer_output_off_runtime()`. Указанный диапазон строк объединяет несколько разных функций и создаёт ложное впечатление об одной большой функции.

**File:** `server/crates/php-lsp-server/src/lsp/completion_helpers.rs:200-600`
**Issue:** Multiple large helper functions with complex type resolution logic should be extracted into dedicated modules.

> **Комментарий Codex: наблюдение слишком общее.** В диапазоне находятся разные helpers, а не один монолит. Файл тематически уже отделён от LSP handler. Дальнейшее разбиение оправдано только по конкретной границе ответственности; само по себе количество строк дефектом не является.

#### TypeScript Client

**File:** `client/src/extension.ts:405-437`
```typescript
// getExtensionSnapshot() - 33 lines
// 20+ property extractions from config
```

> **Комментарий Codex: не считаю проблемой.** Функция декларативно собирает один `ExtensionSnapshot`; последовательные чтения полей конфигурации здесь прозрачнее разнесённых builders и не создают сложной логики.

**File:** `client/src/extension.ts:157-251`
```typescript
// showPopup() in PhpLspStatusController - 95 lines
// Builds array of 17 quick pick items
```

> **Комментарий Codex: частично подтверждено, severity — low.** Основной объём — статическое описание quick-pick items. Вынос фабрик может сократить метод, но добавит навигацию между простыми структурами; реального дефекта или существенной сложности ветвления здесь нет.

**File:** `client/src/extension.ts:899-958`
```typescript
// stopLanguageClient() - 60 lines
// Complex termination logic with multiple try-catch blocks
```

> **Комментарий Codex: устаревшая оценка.** Управление сигналами уже вынесено в `serverProcess.ts`; оставшиеся `try/catch/finally` разделяют stop, принудительное завершение и dispose и нужны для независимой best-effort очистки. Дополнительный вынос не устраняет риск.

**Recommendation:** Extract item builders and process termination into separate functions.

> **Комментарий Codex:** process termination уже вынесен. Builders для UI — допустимый косметический backlog, но не обязательное исправление.

### 1.2 Code Duplication

**File:** `server/crates/php-lsp-server/src/lsp/diagnostics.rs:8-15, 26-33`
```rust
// build_analyzer_shell_command() and build_phpstan_shell_command()
// Overlapping logic for template replacement
```

> **Комментарий Codex: подтверждено как небольшое дублирование.** Подстановка `{file}` совпадает с formatter helper; PHPStan-specific `{memory_limit}` остаётся отдельной логикой. Общий helper возможен, но выигрыш мал и это не функциональная ошибка.

**File:** `server/crates/php-lsp-server/src/lsp/formatting.rs:28-33`
```rust
// build_formatter_shell_command()
// Duplicates same template replacement pattern
```

> **Комментарий Codex: подтверждено, severity — low.** Это шесть одинаковых строк. Объединять стоит при следующем изменении command-template API, чтобы не расширять область текущих поведенческих fixes.

**File:** `client/src/extension.ts:1056-1067, 1118-1127`
```typescript
// Nearly identical "language server is disabled" status updates
if (!workspace.getConfiguration("phpLsp").get<boolean>("enable", true)) {
  statusController?.update({
    phase: "ready",
    message: "Language server is disabled",
  });
}
```

> **Комментарий Codex: частично подтверждено.** Текст статуса повторяется в нескольких пользовательских сценариях, но окружающие действия и уведомления различаются. Можно вынести маленький `showDisabledStatus()`, однако риск расхождения сейчас низкий.

**Recommendation:** Consolidate template replacement logic into shared helper function.

> **Комментарий Codex:** разумный low-priority рефакторинг; он не подтверждает заявленные суммарные high-severity оценки отчёта.

### 1.3 Inconsistent Coding Standards

#### PHP Fixtures

**Missing `declare(strict_types=1);`** in 12 files:
- `test-fixtures/basic/Test/Baz.php`
- `test-fixtures/lsp-cases/src/Diagnostics/FrameworkNoFalsePositive.php`
- `test-fixtures/lsp-cases/src/Diagnostics/PromotedSelfDefaults.php`
- `test-fixtures/vendor-resolve/src/BaseHandler.php`
- `test-fixtures/vendor-resolve/src/TimerService.php`
- `test-fixtures/vendor-resolve/src/ConcreteHandler.php`
- `test-fixtures/vendor-resolve/tests/SampleTest.php`
- `test-fixtures/vendor-resolve/vendor/fakevendor/framework/src/TestCase.php`
- `test-fixtures/vendor-resolve/vendor/fakevendor/framework/src/BaseAssert.php`
- `test-fixtures/vendor-resolve/vendor/fakevendor/framework/src/MockBuilder.php`
- `test-fixtures/vendor-resolve/vendor/fakevendor/framework/src/InvocationMocker.php`
- `test-fixtures/vendor-resolve/vendor/composer/autoload_psr4.php`

**Trailing whitespace:** `test-fixtures/basic/Test/Baz.php:8`

**Inconsistent property typing:** `test-fixtures/basic/Test/Baz.php:7`
```php
public $test = "This is a test."; // No type hint
```

> **Комментарий Codex: как проблема не подтверждено.** Эти файлы — входные fixtures, а не production PHP. Они намеренно представляют разные стили и версии PHP, поэтому единый `strict_types` и обязательные property types исказили бы покрываемые случаи. В `Baz.php:8` действительно есть trailing whitespace — это единственная подтверждённая косметическая находка в группе.

---

## 2. Potential Bugs

### 2.1 Unwrap Calls in Production Code

**File:** `server/crates/php-lsp-server/src/indexing/workspace.rs:1426, 1437`
```rust
candidates.into_iter().next().unwrap()
// Will panic if candidates is empty
// Safe due to length check but should use expect()
```

> **Комментарий Codex: panic path не подтверждён.** Обе ветки защищены `candidates.len()`: `1` и `_` после исключения `0`. Замена `unwrap()` на `expect()` меняет только сообщение для логически недостижимого нарушения инварианта и не является bug fix.

**File:** `server/crates/php-lsp-parser/src/resolve.rs:4754`
```rust
merged.pop().unwrap()
// Safe due to length check but should use expect() for clarity
```

> **Комментарий Codex: panic path не подтверждён.** `pop()` выполняется только при `merged.len() == 1`. `expect()` допустим как документация инварианта, но текущий код безопасен относительно входных данных.

**Recommendation:** Replace `unwrap()` with `expect()` containing descriptive error messages.

> **Комментарий Codex:** можно принять как style-only cleanup; повышать приоритет из-за этих двух мест оснований нет.

### 2.2 Potential Deadlocks

**File:** `server/crates/php-lsp-server/src/server.rs:2348-2349`
```rust
self.vendor_autoload_cache.lock().await.clear();
let evicted = self.vendor_file_lru.lock().await.clear();
```
**Issue:** Multiple mutex locks acquired in sequence without documented ordering. Risk of deadlock if another code path acquires locks in reverse order.

> **Комментарий Codex: не подтверждено.** Guard от `vendor_autoload_cache.lock().await.clear()` уничтожается в конце первого statement, до ожидания `vendor_file_lru`. Два mutex одновременно не удерживаются, поэтому обратный порядок в другом месте здесь deadlock не создаёт.

**File:** `server/crates/php-lsp-server/src/server.rs:2188-2191`
```rust
let diagnostics_mode = *self.diagnostics_mode.lock().await;
let diagnostic_severity = *self.diagnostic_severity.lock().await;
let diagnostic_budget = *self.diagnostic_budget.lock().await;
let php_version = *self.php_version.lock().await;
```
**Issue:** Pattern is fragile if any guards are held across await points.

> **Комментарий Codex: не подтверждено.** Каждое значение имеет `Copy`, guard живёт только до `;`, и между этими statement нет удерживаемого guard. Формулировка описывает гипотетическое будущее изменение, а не текущую ошибку.

**Recommendation:** Document and enforce consistent lock ordering for all multi-mutex operations.

> **Комментарий Codex:** общий lock-order документ полезен только для участков, действительно удерживающих несколько guard. Приведённые места к ним не относятся.

### 2.3 Race Conditions

**File:** `server/crates/php-lsp-server/src/server.rs:198-239`
```rust
// commit_open_document_index_snapshot_if_current_with_hook()
// Checks document state consistency but window between check and commit
```
**Issue:** Check-then-act pattern could race even with generation tokens.

> **Комментарий Codex: не подтверждено.** Commit получает `Occupied` entry `open_files` и удерживает его как per-document lock во время проверки generation/version/template и обновления индекса. Писатели используют ту же parser-entry boundary; соответствующие interleaving-тесты уже есть в `server_tests.rs`.

**File:** `server/crates/php-lsp-server/src/lsp/diagnostics.rs:432-470`
```rust
// Diagnostic publish worker drains pending requests
// Multiple checks for diagnostic_publish_request_is_current()
// with potential state changes between checks
```

> **Комментарий Codex: не подтверждено как race bug.** Publisher намеренно коалесцирует запросы, проверяет snapshot при постановке и повторно перед публикацией; generation/version/template служат optimistic-cancellation tokens. Смена состояния приводит к отбрасыванию stale результата, а не к его публикации.

**File:** `client/src/lifecycle.ts:66-85`
```typescript
// LifecycleCoordinator.enqueue() has potential reentrancy issue
this.operationDepth += 1;
try {
  await operation();
} finally {
  this.operationDepth = Math.max(0, this.operationDepth - 1);
}
```
**Issue:** If `operation()` throws, error is re-thrown but `this.queue` is already set to caught promise.

> **Комментарий Codex: не подтверждено.** Это намеренная семантика: вызывающий получает rejecting `run`, а внутренняя очередь хранит `run.catch(...)`, чтобы следующая операция не была навсегда заблокирована предыдущей ошибкой. `finally` корректно уменьшает depth.

**File:** `client/src/extension.ts:1048-1067`
```typescript
// restartLanguageClient() has race between stop and start
await stopLanguageClient("restart command");
if (!workspace.getConfiguration("phpLsp").get<boolean>("enable", true)) {
// Configuration could change between stop and this check
}
```

> **Комментарий Codex: не подтверждено.** Lifecycle-операции сериализованы, настройка перечитывается после `await stop`, а общий `reconcileLanguageClientState()` повторно сверяет состояние после каждого асинхронного start/stop. Это защита от описанной смены конфигурации.

### 2.4 Null/Undefined Access (TypeScript)

**File:** `client/src/extension.ts:848`
```typescript
const processToTerminate = managedServerProcess(currentClient);
// May return undefined, accessed later at lines 929, 947
```

> **Комментарий Codex: не подтверждено.** Значение передаётся в `childProcessIsRunning()`/`terminateManagedServerProcess()`, которые явно принимают `undefined`; PID в логах читается через `?.pid`.

**File:** `client/src/extension.ts:544`
```typescript
const failurePolicy = languageClientFailurePolicy(
  client,
  languageClient,  // Can be undefined
  languageClient !== undefined && stoppingClients.has(languageClient),
);
```

> **Комментарий Codex: не подтверждено.** Сигнатура `languageClientFailurePolicy(activeClient: object | undefined, failedClient: object | undefined, ...)` специально допускает отсутствие клиента и трактует его как stale/stopping event.

**File:** `client/src/serverProcess.ts:68`
```typescript
timeout = setTimeout(
  () => finish(!childProcessIsRunning(childProcess)),
  Math.max(0, timeoutMs),
);
// Timeout callback may race with exit event
```

> **Комментарий Codex: не подтверждено.** Общий `finish()` защищён флагом `settled`, очищает timeout и снимает listener. Дополнительная проверка после установки listener закрывает окно выхода процесса между первой проверкой и подпиской.

### 2.5 Missing Error Handling

**File:** `client/src/extension.ts:620-642`
```typescript
try {
  candidates = fs.readdirSync(root, { withFileTypes: true })
} catch {
  return undefined;  // Error swallowed without logging
}
```

> **Комментарий Codex: допустимый best-effort fallback.** Composer discovery необязателен; ошибка чтения корня означает «не найдено» и не мешает запуску расширения. Debug-лог мог бы помочь поддержке, но отсутствие лога не является потерей обязательной ошибки.

**File:** `client/src/cachePath.ts:18-19`
```typescript
try {
  return fs.realpathSync(value).replace(/\\/g, "/");
} catch {
  return value.replace(/\\/g, "/");  // Error swallowed
}
```

> **Комментарий Codex: не подтверждено как ошибка.** `realpathSync` ожидаемо падает для ещё не созданного cache path; лексическая нормализация — намеренный fallback, необходимый до создания каталога.

**File:** `server/crates/php-lsp-server/src/indexing/vendor.rs:189`
```rust
.await.ok().flatten()
// Swallows actual error, should log before discarding
```

> **Комментарий Codex: частично подтверждено, severity — low.** API намеренно возвращает `Option`, а `run_file_io_blocking()` уже логирует task failure/timeout. Явный лог в wrapper мог бы дать больше контекста, но обычный parse miss — штатный `None`, а не ошибка, которую следует всегда шумно логировать.

---

## 3. Security Issues

### 3.1 Command Injection Risk

**File:** `server/crates/php-lsp-server/src/lsp/external_command.rs:5-11`
```rust
fn shell_escape(value: &str) -> String {
    if cfg!(windows) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}
```
**Severity:** HIGH

**Issue:** Used with user-configurable command templates from `.php-lsp.toml`. If users load untrusted project configurations, command templates like `formatting.command`, `phpstan.command`, or `psalm.command` could execute arbitrary code.

> **Комментарий Codex: high-severity vulnerability не подтверждена.** Project `.php-lsp.toml` по умолчанию не может включить или подменить executable formatter/analyzer settings: server удаляет их через `sanitize_project_settings_for_command_trust()`, а project config не может сам выставить trust. Произвольная команда разрешается только после явного `phpLsp.allowProjectCommands`/global trust; для custom-command feature это ожидаемая семантика. Подставляемые сервером file path и memory limit проходят `shell_escape()`.

**File:** `server/crates/php-lsp-server/src/lsp/diagnostics.rs:17-29`
```rust
// PHPStan command building includes memory limit substitution
// Could be exploited if memory limit value isn't validated
```

> **Комментарий Codex: не подтверждено.** `memory_limit` не интерпретируется отдельно, а shell-экранируется как один аргумент и к тому же не относится к executable project keys без trust. Формат вроде `1G` можно валидировать для UX, но command injection через это место не показан.

**Recommendation:**
- Add explicit validation for command templates
- Consider whitelisting allowed commands
- Implement command trust gate enforcement

> **Комментарий Codex:** trust gate уже реализован, документирован и покрыт unit/E2E, включая невозможность self-trust из project config. Whitelist противоречит поддержке явной custom command; разумнее сохранять текущую модель явного доверия и escaping подстановок.

### 3.2 Path Traversal

**File:** `server/crates/php-lsp-server/src/indexing/workspace.rs:1390-1440`
```rust
// find_composer_json() walks up directory tree
// Doesn't validate staying within workspace boundaries
```
**Severity:** MEDIUM

**Issue:** Could potentially read `composer.json` files outside intended workspace.

> **Комментарий Codex: не подтверждено.** `find_composer_json()` не «идёт вверх»: он проверяет переданный workspace root и только непосредственные дочерние каталоги. `DirEntry::file_type().is_dir()` не следует directory symlink, поэтому показанного выхода за workspace нет.

**File:** `client/src/extension.ts:617-642`
```typescript
// discoverComposerRoot() traverses directories without validation
candidates = fs.readdirSync(root, { withFileTypes: true })
  .filter((entry) => entry.isDirectory())
  .filter((entry) => !entry.name.startsWith(".") && !skipDirs.has(entry.name))
  .map((entry) => path.join(root, entry.name))
```
**Severity:** MEDIUM

**Issue:** No symlink following protection, could traverse into unexpected directories.

> **Комментарий Codex: не подтверждено.** Обход имеет глубину 1, а `Dirent.isDirectory()` для symbolic link возвращает false; рекурсивного следования по symlink здесь нет.

**File:** `client/src/extension.ts:718-789`
```typescript
// resolveServerBinary() trusts custom path from config
const customPath = config.get<string>("serverPath", "").trim();
if (customPath.length > 0) {
  const exists = fs.existsSync(customPath);
  const executable = exists && isExecutableFile(customPath);
}
```
**Severity:** MEDIUM

**Issue:** No validation that path is within expected directories. User could configure path to arbitrary executable.

> **Комментарий Codex: это намеренная функция, а не path traversal.** `phpLsp.serverPath` — явная пользовательская настройка для собственного server binary. Ограничение путём каталога сделало бы её бесполезной; код уже проверяет существование и executable bit.

### 3.3 Environment Variable Leaks

**File:** `client/src/extension.ts:598`
```typescript
function getServerEnvironment(logLevel: string): NodeJS.ProcessEnv {
  return {
    ...process.env,  // Passes ALL process env vars to server
    RUST_LOG: logLevel.trim() || "info",
  };
}
```
**Severity:** MEDIUM

**Issue:** Could leak sensitive environment variables to server process. Should whitelist only necessary variables.

> **Комментарий Codex: как утечка не подтверждено.** Дочерние процессы обычно наследуют environment, а сервер запускается локально от того же пользователя и нуждается как минимум в `PATH`, `HOME`/cache/config variables и platform-specific окружении. Whitelist может сломать Composer/analyzers. Отдельная минимизация окружения имеет смысл только при иной trust boundary для самого server binary.

### 3.4 Unsafe File Operations

**File:** `server/crates/php-lsp-server/src/lsp/formatting.rs:106-108`
```rust
// temp_format_dir() creates temp files with predictable names
// Based on PID and timestamp
```
**Severity:** LOW

**Recommendation:** Use `tempfile` crate's secure temp file creation.

> **Комментарий Codex: подтверждено как low-severity hardening.** PID+nanos делает коллизию маловероятной, но `create_dir_all` + обычный `write` не дают атомарной гарантии против заранее созданного symlink/path. `tempfile::TempDir` лучше выражает владение и cleanup; это не доказывает заявленный high-risk command injection.

**File:** `client/src/cachePath.ts:6-8`
```typescript
if (env.XDG_CACHE_HOME) {
  return env.XDG_CACHE_HOME;
}
if (env.HOME) {
  return path.join(env.HOME, ".cache");
}
```
**Severity:** LOW

**Issue:** Trusts environment variables without validation. Could be exploited via environment variable injection.

> **Комментарий Codex: не подтверждено как vulnerability.** `XDG_CACHE_HOME` и `HOME` — стандартные источники cache location; процесс, способный изменить environment расширения, уже управляет его файловым контекстом. Здесь нет повышения привилегий. Нормализация/создание безопасного подкаталога остаётся хорошей защитой от случайно некорректных значений.

---

## 4. Performance Issues

### 4.1 Unnecessary Clones

**File:** `server/crates/php-lsp-server/src/server.rs:148`
```rust
let tree = parser.tree()?.clone();
// Tree-sitter trees are expensive to clone
```

> **Комментарий Codex: не подтверждено.** Clone нужен, чтобы отпустить DashMap guard и работать с согласованным snapshot; tree-sitter предоставляет копирование дерева с разделяемыми внутренними поддеревьями. Удалять clone без нового механизма владения/замера нельзя.

**File:** `server/crates/php-lsp-server/src/server.rs:151-153`
```rust
template_documents.get(uri_str).map(|document| document.value().clone())
```

> **Комментарий Codex: частично подтверждено.** `TemplateDocument` содержит строки и source-map данные, поэтому clone не бесплатный, но он также нужен для coherent snapshot после снятия guard. Оптимизация через `Arc<TemplateDocument>` возможна только после профилирования template hot paths.

**File:** `server/crates/php-lsp-server/src/server.rs:220-221`
```rust
snapshot.file_symbols.clone()
```

> **Комментарий Codex: частично подтверждено, severity — low.** Это реальный clone `FileSymbols` при commit, обусловленный тем, что snapshot остаётся доступен. Можно исследовать передачу ownership/`Arc`, но отчёт не показывает, что этот участок доминирует по времени.

**File:** `server/crates/php-lsp-server/src/lsp/completion.rs:196`
```rust
self.namespace_map.lock().await.clone()
// Clones entire namespace map on every completion request with framework context
```

> **Комментарий Codex: частично подтверждено.** Clone выполняется не на каждом completion, а только при найденном framework string-key context. `NamespaceMap` обычно мал; `Arc` может убрать копию на больших Composer maps, если профилирование подтвердит стоимость.

### 4.2 Inefficient Data Structures

**File:** `server/crates/php-lsp-server/src/server.rs:1420-1566`
```rust
// FrameworkStringKeyCache and TwigContextDiskCache use HashMap + VecDeque for LRU
// touch() method does O(n) linear search:
if let Some(position) = self.order.iter().position(|existing| existing == &key) {
    self.order.remove(position);
}
```
**Severity:** MEDIUM

**Recommendation:** Use `IndexMap` or maintain separate HashMap for O(1) lookups.

> **Комментарий Codex: severity завышена.** Поиск действительно O(n), но cache capacities жёстко ограничены 32 и 64 элементами. `IndexMap::shift_remove` тоже требует учитывать порядок/сдвиги; усложнять структуру без измеримой нагрузки не стоит.

**File:** `server/crates/php-lsp-server/src/indexing/vendor.rs:110-120`
```rust
// VendorFileLru::touch() has same O(n) position lookup issue
```

> **Комментарий Codex: подтверждено как bounded O(n), но не medium issue.** Capacity равен 512, поэтому рост не неограничен. HashMap+linked ordering может помочь при очень частых vendor touches, но сначала нужен benchmark.

### 4.3 Blocking I/O in Async Context

**File:** `server/crates/php-lsp-server/src/indexing/workspace.rs:1430`
```rust
std::fs::read_to_string(c)  // Inside find_composer_json()
// Called in async context, wrapped in run_file_io_blocking() in some callers
// but direct calls may block async runtime
```

> **Комментарий Codex: текущий async violation не подтверждён.** LSP configuration/indexing callers проходят через `load_effective_configuration_settings_blocking()` или `discover_workspace_root_configs_blocking()`. Прямые sync callers находятся в CLI/tests или внутри уже вынесенной blocking closure.

**File:** `server/crates/php-lsp-server/src/lsp/formatting.rs:66-77`
```rust
// detect_project_formatter_tool() uses std::fs::read_to_string() directly
// Async wrapper exists (line 80-90) but sync version is exposed
```

> **Комментарий Codex: не подтверждено.** Production async path вызывает `resolve_for_workspace_blocking()` → `detect_project_formatter_tool_blocking()`. Sync `resolve_for_workspace()` помечен `#[cfg(test)]`; сам sync helper используется внутри blocking wrapper.

### 4.4 Inefficient Operations (TypeScript)

**File:** `client/src/extension.ts:654-658`
```typescript
for (const folder of workspace.workspaceFolders ?? []) {
  roots.add(folder.uri.fsPath);
  const composerRoot = discoverComposerRoot(folder.uri.fsPath);
  if (composerRoot) {
    roots.add(composerRoot);
  }
}
return Array.from(new Set(Array.from(roots, phpLspCacheDirForRoot)));
// Redundant: roots is already a Set
```

> **Комментарий Codex: подтверждено, severity — low.** Внешний `new Set` избыточен, потому что исходные roots уже уникальны и hash одного root детерминирован. Можно заменить на `Array.from(roots, phpLspCacheDirForRoot)`.

**File:** `client/src/extension.ts:617`
```typescript
const skipDirs = new Set([...]);  // Recreated on every discoverComposerRoot() call
// Should be module-level constant
```

> **Комментарий Codex: подтверждено как микроптимизация.** Набор маленький, функция вызывается редко; module-level constant немного уменьшит allocations, но практический эффект минимален.

**File:** `client/src/cachePath.ts:27-37`
```typescript
// stableHashStrings() uses BigInt operations which are slower than necessary
let hash = 0xcbf29ce484222325n;
const prime = 0x100000001b3n;
// For cache directory naming, crypto.createHash('sha256') would be more efficient
```

> **Комментарий Codex: не подтверждено.** Отчёт не содержит benchmark; для нескольких коротких root strings простой FNV-1a без импорта crypto вполне уместен. SHA-256 даёт другие свойства, но не автоматически лучшую производительность для такого объёма.

**File:** `client/src/extension.ts:146-152`
```typescript
update(status: IndexingStatus): void {
  this.status = {
    ...this.status,
    ...status,
    lastUpdatedAt: Date.now(),
  };
  this.render();  // Called even if status hasn't meaningfully changed
}
```
**Recommendation:** Add shallow comparison to skip renders for identical status.

> **Комментарий Codex: частично подтверждено, severity — low.** Сейчас каждый status event обновляет timestamp и render. Сравнение может убрать лишние UI writes, но частота ограничена indexing/lifecycle notifications; оптимизировать стоит только с учётом того, должен ли `lastUpdatedAt` отражать повторный event.

---

## 5. Memory Leaks

### 5.1 Unbounded Caches

**File:** `server/crates/php-lsp-server/src/server.rs:512-632`
```rust
// RequestTypeCache uses RefCell<HashMap<...>> with NO capacity limit
string_values: RefCell<HashMap<RequestTypeCacheKey, Option<String>>>,
type_info_values: RefCell<HashMap<RequestTypeCacheKey, Option<php_lsp_types::TypeInfo>>>,
// ... no eviction policy
```
**Severity:** HIGH

**Issue:** For long-running LSP sessions with many files, this could grow unbounded.

> **Комментарий Codex: не подтверждено.** `RequestTypeCache` не хранится между запросами: новый экземпляр создаётся внутри completion/hover/definition/inlay/diagnostic operation и уничтожается вместе с ней. Он может расти только в пределах одного разбираемого документа/запроса, поэтому длительность сессии и число ранее открытых файлов его не накапливают. Это не production memory leak.

**File:** `server/crates/php-lsp-server/src/server.rs:1495-1566`
```rust
// TwigContextDiskCache has LRU but evict_entries_for_source_uri() (line 1525)
// only removes entries, doesn't update order VecDeque
// Causes order to grow unbounded even as entries are evicted
```
**Severity:** HIGH

> **Комментарий Codex: утверждение устарело/неверно для текущего кода.** `evict_entries_for_source_uri()` выполняет и `entries.retain(...)`, и `order.retain(...)`; отдельный regression test проверяет cleanup order. Capacity cache также ограничен 64.

### 5.2 Circular References

**File:** `server/crates/php-lsp-index/src/workspace.rs:17-24`
```rust
// DirectMemberSource contains Arc<FileSymbols>
// FileSymbols may contain references back to URIs that point to files holding these sources
```
**Severity:** MEDIUM

**Issue:** While Arc prevents use-after-free, circular Arc references can prevent cleanup.

> **Комментарий Codex: не подтверждено.** `DirectMemberSource` владеет `Arc<FileSymbols>`, но `FileSymbols` не владеет `DirectMemberSource` или workspace index. URI внутри symbols — строки, а не обратные owning references. Граф Arc ацикличен.

### 5.3 Missing Cleanup

**File:** `server/crates/php-lsp-server/src/server.rs:200-239`
```rust
// reload_tokens DashMap is cleaned up in commit_closed_php_index_if_current_with_hook() (line 289)
// But if function returns early (lines 267-277), token remains in map
```
**Severity:** MEDIUM

> **Комментарий Codex: leak не подтверждён.** Ранний возврат при несовпавшем token обязан сохранить более новый token; при reopen соответствующая ветка удаляет token, а error/close paths также делают cleanup. DashMap содержит максимум актуальную запись на URI, а не историю generations.

**File:** `client/src/extension.ts:51`
```typescript
const clientFileWatchers = new DisposableResourceRegistry<LanguageClient>();
// Module-level WeakMap, grows indefinitely
// disposeClientFileWatchers() (line 878) cleans up per-client
// but if clients are not properly disposed, registry could retain references
```

> **Комментарий Codex: не подтверждено.** Реестр использует `WeakMap`, который как раз не удерживает owner от garbage collection. Штатные stop/start-error paths дополнительно вызывают `disposeClientFileWatchers()`.

**File:** `client/src/extension.ts:46`
```typescript
// outputChannel is module-level and only disposed in deactivate()
// If extension is reloaded without full deactivation, channel could leak
```

> **Комментарий Codex: не подтверждено.** `deactivate()` явно вызывает `outputChannel?.dispose()`. VS Code lifecycle должен вызвать deactivate перед unload; сценарий «reload без deactivation» не является состоянием, которое расширение может корректно компенсировать module-level cleanup.

---

## 6. Error Handling Issues

### 6.1 Swallowed Errors

**File:** `server/crates/php-lsp-server/src/lsp/diagnostics.rs:108-109`
```rust
.map_err(|err| format!("invalid PHPStan JSON: {}", err))
// Loses context about which file/stage failed
```

> **Комментарий Codex: не является swallowed error.** Ошибка возвращается вызывающему, явно называет PHPStan JSON stage и сохраняет serde error. Target file уже известен `run_phpstan_for_file`; добавить его в сообщение можно для observability, но информация не теряется молча.

**File:** `server/crates/php-lsp-server/src/indexing/vendor.rs:189`
```rust
.await.ok().flatten()
// Returns Option but swallows actual error
```

> **Комментарий Codex: частично подтверждено, но продублировано из 2.5.** Infrastructure failure/timeout уже логируется `run_file_io_blocking`; `None` от parser является штатным отсутствием metadata. Wrapper мог бы логировать собственный path context, но это low-priority observability.

### 6.2 Generic Errors

**File:** `server/crates/php-lsp-server/src/lsp/external_command.rs:40, 66`
```rust
format!("failed to start {} command: {}", label, err)
// Too generic, should include command string, working directory, and environment
```

> **Комментарий Codex: рекомендация частично небезопасна.** Сообщение уже содержит tool label и исходный OS error. Working directory может быть полезен, но полный command и особенно environment могут содержать секреты; их нельзя безусловно добавлять в пользовательские логи.

**File:** `server/crates/php-lsp-server/src/server.rs:678-691`
```rust
// run_file_io_blocking() returns String errors
// Loses original std::io::Error type information
// Makes it hard to handle specific error cases (NotFound vs PermissionDenied)
```

> **Комментарий Codex: не подтверждено для заявленной функции.** `run_file_io_blocking()` оборачивает spawn/join/timeout и возвращает результат closure как generic `T`; filesystem helpers при необходимости сохраняют свой `std::io::Result`. String используется на orchestration boundary, где вызывающие логируют/делают fallback и не ветвятся по `ErrorKind`.

### 6.3 Missing Error Propagation

**File:** `server/crates/php-lsp-parser/src/references.rs:70`
```rust
find_variable_scope(node).unwrap_or(root)
// Silently falls back to root node when scope detection fails
// Could lead to incorrect reference results without diagnostic
```

> **Комментарий Codex: не подтверждено как missing propagation.** Root — корректная область для file-scope variables и консервативный fallback для incomplete syntax; public API возвращает список, а не `Result`. Чтобы считать это ошибкой, нужен воспроизводимый node, для которого есть более узкий scope, но `find_variable_scope()` его пропускает — отчёт такого случая не даёт.

**File:** `server/crates/php-lsp-server/src/lsp/completion_helpers.rs:58-99`
```rust
// phpdoc_virtual_member() returns Option and silently returns None on any parsing failure
// Should log warnings when PHPDoc parsing fails
```

> **Комментарий Codex: неверная модель API.** `parse_phpdoc()` — tolerant total parser, возвращающий `PhpDoc`, а не `Result`; malformed/unsupported tags намеренно игнорируются. `None` означает отсутствие подходящего virtual member, и warning на каждый miss засорил бы LSP logs.

---

## 7. Test Coverage Gaps

### 7.1 Critical Missing Coverage

#### TypeScript Client - ZERO Coverage

**Severity:** CRITICAL

**Files with no tests:**
- `client/src/extension.ts` (37,355 lines)
- `client/src/serverProcess.ts` (4,764 lines)
- `client/src/configuration.ts` (3,524 lines)
- `client/src/lifecycle.ts` (3,984 lines)
- `client/src/cachePath.ts` (1,247 lines)

**Impact:** Client-side bugs (LSP client initialization, configuration handling, server process management, cache path resolution) will not be caught until manual testing.

> **Комментарий Codex: основная находка подтверждена, численные данные неверны.** В `client/` действительно нет test files/test script, и lifecycle/config/process helpers заслуживают unit tests. Но отчёт перепутал байты/символы со строками: актуально `extension.ts` — 1189 строк, `serverProcess.ts` — 146, `configuration.ts` — 103, `lifecycle.ts` — 157, `cachePath.ts` — 47 (в сумме 1642, не 50K+). Поэтому «CRITICAL» завышено; я бы поставил high для process/lifecycle regressions и medium для остальных helpers.

**Recommended tests:**
1. `client/src/test/extension.test.ts` - Basic extension activation/deactivation
2. `client/src/test/serverProcess.test.ts` - Server process lifecycle
3. `client/src/test/configuration.test.ts` - Configuration handling
4. `client/src/test/lifecycle.test.ts` - Lifecycle state machine

> **Комментарий Codex:** направление верное, но первым слоем лучше тестировать уже выделенные pure/exported helpers (`serverProcess`, `configuration`, `lifecycle`, `cachePath`) без тяжёлого VS Code host. Activation/deactivation test можно добавить отдельно как integration smoke.

#### Rust Server - Missing Unit Tests

**`php-lsp-types` crate:**
- Missing tests for `TypeInfo::Display`, `TemplateVariance`, `PhpDocPropertyAccess`
- Missing tests for `ArrayShapeItem::Display`, `normalize_shape_key_text`
- Missing tests for `symbol_fqn_eq` edge cases

> **Комментарий Codex: в основном не подтверждено.** `lib_tests.rs` уже проверяет `TypeInfo::Display` и PHP-specific case rules `symbol_fqn_eq` для global constants, methods и properties. PHPDoc tests покрывают template variance и shape display через реальные tags. Отдельные direct tests для `ArrayShapeItem::Display`, `normalize_shape_key_text` и access predicates могут быть полезны, но список нельзя считать полностью missing.

**`php-lsp-server` crate:**
- `src/lsp/hierarchy.rs` - No dedicated unit tests (only e2e)
- `src/lsp/document_links.rs` - No dedicated unit tests
- `src/lsp/folding.rs` - No dedicated unit tests
- `src/lsp/external_command.rs` - No unit tests for timeout handling, cancellation
- `src/indexing/vendor.rs` - No unit tests (only e2e)
- `src/template.rs` - Limited coverage in `template_tests.rs`

> **Комментарий Codex: не подтверждено в заявленном масштабе.** Проект намеренно использует плоские `server_tests.rs`/`template_tests.rs` плюс split E2E. Hierarchy, folding и document links имеют содержательные protocol E2E; external commands уже покрыты cancellation, timeout и malformed-output tests; vendor LRU/cache/helpers проверяются в `server_tests.rs`; `template_tests.rs` содержит более 20 unit tests, а `e2e_templates.rs` — обширное protocol покрытие. Отсутствие теста внутри одноимённого production-файла не означает отсутствие покрытия.

### 7.2 Flaky Tests

**File:** `server/crates/php-lsp-server/tests/e2e_diagnostics.rs:58-165`
```rust
// test_open_file_diagnostics_are_syntax_only_while_workspace_indexing_runs
// Uses Duration::from_secs(3) and Duration::from_secs(10) timeouts
// May fail on slow CI
```

> **Комментарий Codex: риск частично подтверждён.** Тест специально создаёт 480 файлов, чтобы наблюдать промежуточную фазу, поэтому 3/10-second wall-clock границы зависят от CI. Это кандидат на более детерминированный indexing hook/barrier; один факт наличия timeout ещё не доказывает flaky history.

**File:** `server/crates/php-lsp-server/tests/e2e_diagnostics.rs:168-254`
```rust
// test_immediate_did_open_diagnostics_are_guarded_during_initialized_setup
// Same tight timeout issue
```

> **Комментарий Codex: частично подтверждено по той же причине.** Сценарий гонки важен, но лучше синхронизироваться с явным lifecycle event, чем рассчитывать на относительную скорость initialized/indexing.

**File:** `server/crates/php-lsp-server/tests/e2e_diagnostics.rs:558-629`
```rust
// test_did_change_debounces_diagnostics_and_ignores_stale_versions
// Uses Duration::from_millis(300) which is very tight
```

> **Комментарий Codex: не подтверждено в данной формулировке.** 300 ms используется как отрицательное окно «не должно быть публикации» для stale version, а не как deadline успешной операции. Увеличение окна сделает suite медленнее и не устранит scheduling flake; важнее привязка к debounce contract.

**File:** `server/crates/php-lsp-server/tests/e2e_indexing.rs`
```rust
// Tests depend on wait_for_indexing_phase() which may be non-deterministic
// with varying disk speeds
```

> **Комментарий Codex: частично подтверждено.** Helper ждёт конкретный `phpLsp/indexingStatus` event и сам детерминирован; filesystem speed влияет только на deadline отдельных тестов. Нужны конкретные failing tests/CI данные, а не blanket-оценка всего файла.

**File:** `server/crates/php-lsp-server/tests/e2e_foundation.rs:77-139`
```rust
// test_foundation_project_config_cannot_self_trust_executable_commands
// Uses unique_temp_dir() but cleanup may fail if test panics earlier
```

> **Комментарий Codex: подтверждено как test hygiene, не flakiness текущего assertion.** При panic каталог останется, но имя содержит PID+nanos и обычно не конфликтует со следующим запуском. RAII tempdir улучшит cleanup и упростит тесты.

### 7.3 Missing Edge Case Coverage

#### Parser Tests

**Missing:**
- Malformed PHPDoc with unclosed tags
- Extremely long PHPDoc comments
- PHPDoc with mixed ASCII/Unicode in parameter names
- `@template-covariant` / `@template-contravariant` variance
- Deeply nested namespace declarations (3+ levels)
- Anonymous classes within anonymous classes
- Traits using other traits with complex inheritance
- References in eval'd code contexts
- References within closure `use()` clauses with by-ref captures

> **Комментарий Codex: список частично устарел.** Уже есть malformed-tag PHPDoc test, `@template-covariant`/`@template-contravariant`, eval-context semantic test и closure `use (&$var)` diagnostics/reference coverage. «Deeply nested namespace declarations» не является валидной PHP-конструкцией. Длинные/Unicode PHPDoc stress cases, nested anonymous classes и сложные trait chains действительно можно расширить.

#### Completion Tests

**Missing:**
- Completion inside heredoc/nowdoc strings
- Completion within attribute arguments
- Completion after `::class` constant access
- Completion in match expression arms
- Completion context in nested closures with `use` clauses
- Completion context within conditional types

> **Комментарий Codex: частично подтверждено.** `::class` completion и nested closure/use scope уже покрыты provider/E2E, включая by-reference variadic, arrow/closure nesting и UTF-16 cursor cases. Явного покрытия heredoc/nowdoc, attribute-argument и match-arm contexts поиск не показал; conditional types относятся скорее к type inference, чем к синтаксическому completion context.

#### Index Tests

**File:** `server/crates/php-lsp-index/src/composer_tests.rs:3-170`
```rust
// Tests only cover basic PSR-4 autoloading
```
**Missing:** PSR-0 autoloading, classmap autoloading, files autoloading, exclude patterns from autoload

> **Комментарий Codex: утверждение в основном неверно.** `composer_tests.rs` уже содержит PSR-0 (namespaced и PEAR-style), classmap и files tests, а также autoload-dev/multiple directories. Не покрыта/не реализована Composer `exclude-from-classmap`; это отдельный feature gap, а не доказательство «только basic PSR-4».

**Missing:**
- Stub extensions with version constraints
- Conflicting stub definitions across extensions
- Cache migration from older schema versions
- Corrupted cache file recovery

> **Комментарий Codex: список частично неверен.** Cache tests уже проверяют schema-version invalidation и malformed bincode как cache miss; stubs symlink/missing-extension cases тоже есть. Version constraints для stub extensions не являются моделью текущего API. Конфликты определений и дополнительные cache migration fixtures могут быть полезны, если сначала определить ожидаемую policy.

### 7.4 Tests That Don't Assert Enough

**File:** `server/crates/php-lsp-server/tests/e2e_completion.rs:5-68`
```rust
assert!(
    !result.is_null(),
    "completion should return results for variable context"
);
// Only asserts non-null result, doesn't verify actual completion items
```

> **Комментарий Codex: подтверждено для этого одного smoke test.** Его стоит усилить проверкой `$name`/`$count` либо удалить как дублирующий: сразу после него находятся намного более точные scope/completion tests. Это не характеризует весь `e2e_completion.rs`.

**File:** `server/crates/php-lsp-server/tests/e2e_hover.rs:1199-1378`
```rust
// test_hover_callsite_doctrine_repository_resolved_returns()
// Uses generic assertions like contains("**Resolved returns:**")
// Doesn't assert exact type resolution format or edge cases
```

> **Комментарий Codex: не согласен с оценкой.** Тест проверяет не только заголовок, но конкретные `EntityRepository<App\Entity\RequestStatus>`, `list<App\Entity\RequestStatus>` и source links для четырёх calls. `contains` выбран для Markdown, где полный snapshot был бы хрупким; дополнительные edge cases возможны, но assertion содержательный.

### 7.5 Missing Integration Tests

**Missing scenarios:**
1. Completion + Diagnostics interaction (async diagnostic computation)
2. Rename + Indexing (during active workspace indexing)
3. Hover + Vendor lazy-loading (before lazy-loading completes)
4. Multiple workspace folders (cross-workspace symbol resolution)
5. File operations + Open editors (rename/move while open)

> **Комментарий Codex: список преимущественно устарел.** Уже есть hover+completion во время indexing, multi-root workspace E2E, watched/create/rename/delete file operations и open PHP→Blade atomic rename. Отдельного rename-request во время indexing и точной completion+diagnostics комбинации не видно; их можно оставить как targeted gaps. Vendor lazy loading покрывается множеством definition/hover/completion E2E, хотя не каждый timing interleaving выделен отдельно.

**External Tool Integration:**
- PHPStan JSON output parsing
- Psalm JSON output parsing
- Laravel Pint formatting
- php-cs-fixer formatting
- phpcbf formatting
- Timeout scenarios
- Command cancellation

> **Комментарий Codex: частично неверно.** PHPStan/Psalm запускаются через fake scripts и проверяются для mapping, timeout и malformed JSON; external command cancellation имеет unit test; php-cs-fixer auto-detection/formatting имеет E2E. Отдельные provider paths Pint/phpcbf и formatter cancellation можно добавить, но запуск реальных third-party binaries в CI менее стабилен, чем существующие contract fakes.

### 7.6 Missing Error Scenario Tests

**LSP Protocol:**
- Invalid JSON-RPC requests (malformed JSON, missing required fields)
- Unknown request methods
- Request cancellation (helper exists but no tests use it)
- Concurrent conflicting requests (rename + completion + hover simultaneously)

> **Комментарий Codex: частично подтверждено.** `$/cancelRequest` уже покрыт для references и rename, а concurrent hover+completion и concurrent document notifications тоже тестируются. Malformed transport JSON обычно является ответственностью `tower-lsp-server`; unknown-method protocol smoke и более широкий mixed-request stress test действительно отсутствуют.

**File System:**
- Permission denied
- Disk full (cache write failures)
- Symlink loops
- Network file systems (slow/unresponsive)
- File locked by another process

> **Комментарий Codex: частично подтверждено.** Symlink loops уже имеют Unix regressions в `stubs_tests.rs`, missing stubs также проверяются. Permission/disk-full/locked-file fallbacks почти не покрыты и лучше тестируются через инъекцию filesystem failures. Network filesystem и настоящий disk-full плохо подходят для детерминированного CI.

**PHP-Specific:**
- Invalid UTF-8 in PHP source
- Extremely large files (10MB+)
- Recursion limits (1000+ levels)
- Memory exhaustion
- Invalid composer.json (malformed JSON, missing required fields)

> **Комментарий Codex: частично подтверждено.** LSP JSON source всегда валидный UTF-8, а disk PHP читается lossy, поэтому «invalid UTF-8 in LSP source» неприменим в буквальном виде. Systematic large/deep-source limits и malformed Composer coverage стоит добавить; memory exhaustion как тестовый сценарий опасен и обычно заменяется явными budgets/fuzz/property tests.

**Configuration:**
- Invalid `.php-lsp.toml` (malformed TOML, unknown keys)
- Conflicting settings (`diagnostics.mode = "off"` but `phpstan.enabled = true`)
- Non-existent paths (`stubsPath` pointing to missing directory)
- Invalid PHP version (`phpVersion = "9.9"`)

> **Комментарий Codex: частично подтверждено.** Project-config trust/reload, timeout aliases, default reset и missing stubs paths уже покрыты. Malformed TOML, invalid enum/version values и unknown-key policy заслуживают явных regressions. Сочетание diagnostics off + external analyzer должно сначала иметь документированную semantics, прежде чем называться «conflicting».

---

## 8. LSP Protocol Compliance Issues

**File:** `client/src/extension.ts:842-853`
```typescript
documentSelector: [
  { scheme: "file", language: "php" },
  { scheme: "untitled", language: "php" },
  { scheme: "file", language: "blade" },
  { scheme: "untitled", language: "blade" },
  { scheme: "file", language: "twig" },
  { scheme: "untitled", language: "twig" },
],
```
**Issue:** LSP spec defines standard language IDs; `blade` and `twig` are custom. Must ensure server registers corresponding grammar support.

> **Комментарий Codex: compliance issue не подтверждён.** LSP `languageId` — строка, расширения вправе вводить свои IDs. Клиент регистрирует language contributions `blade`/`twig`, а сервер явно распознаёт `blade|laravel-blade` и `twig|html-twig`, строит virtual PHP и имеет `e2e_templates`/range tests.

**File:** `client/src/extension.ts:888-895`
```typescript
await client.sendNotification("workspace/didChangeConfiguration", {
  settings: buildExplicitClientSettings(config, getStubsPath(context)),
});
```
**Issue:** LSP spec says this notification should be sent automatically by the client. Manual sending may conflict with built-in synchronization.

> **Комментарий Codex: не подтверждено.** `vscode-languageclient` автоматически синхронизирует только заданный `synchronize.configurationSection`; здесь configured лишь `fileEvents`. Ручная отправка нужна для custom payload из explicit-only settings и не дублирует built-in notification.

**File:** `client/src/extension.ts:917`
```typescript
currentClient.stop(STOP_TIMEOUT_MS)
// timeout parameter - vscode-languageclient stop() method signature may not accept timeout
```

> **Комментарий Codex: фактически неверно.** У установленной версии `vscode-languageclient` сигнатуры `stop(timeout?: number)` и `dispose(timeout?: number)` объявлены в `lib/common/client.d.ts` и `lib/node/main.d.ts`.

**File:** `client/src/extension.ts:536-594`
```typescript
// Custom error handler implementation
// LSP spec recommends specific error handling patterns
// CloseAction.Restart at line 588 may conflict with client's built-in restart logic
```

> **Комментарий Codex: не подтверждено.** Возврат `CloseAction.Restart` — штатный extension point built-in client. Handler ограничивает restart burst через `BoundedRestartTracker` и возвращает `DoNotRestart` для stale/stopping clients, то есть координирует, а не запускает параллельный restart самостоятельно.

**File:** `client/src/lifecycle.ts:130-157`
```typescript
// reconcileLanguageClientState() implements custom state management
// May conflict with vscode-languageclient's built-in lifecycle management
```

> **Комментарий Codex: не подтверждено.** Coordinator сериализует пользовательские enable/restart/config transitions вокруг публичных `start/stop`; transport lifecycle остаётся у `vscode-languageclient`. Повторная проверка `isEnabled()/hasClient()` после await закрывает конфигурационные гонки.

---

## 9. Documentation Gaps

### 9.1 Missing Documentation Files

**Critical missing files:**

1. **Security Model Documentation**
   - Command trust boundaries
   - What makes a workspace "trusted"
   - Security implications of `allowProjectCommands`
   - Safe vs dangerous configuration options

> **Комментарий Codex: отдельного файла нет, но «critical missing» неверно.** Command trust boundary, невозможность project self-trust, опасные keys и `allowProjectCommands` подробно описаны в README, `docs/configuration.md`, `docs/architecture.md`, schema/package descriptions и закреплены E2E. Можно собрать это в отдельную security page для discoverability.

2. **API Reference for Server Binary**
   - `php-lsp analyze` command flags in detail
   - `php-lsp fix` command flags and supported rules
   - `php-lsp init-config` output format
   - Exit codes beyond basic table

> **Комментарий Codex: в основном не подтверждено.** README и `docs/cli-ci.md` документируют analyze/fix flags, supported rules, formats и exit codes 0/1/2; `docs/configuration.md` описывает `init-config`. Полный generated CLI reference из `--help` был бы улучшением, но базовая документация существует.

3. **Template Documents Guide**
   - How Blade/Twig virtual PHP works
   - What expressions are supported/unsupported
   - How to debug template context inference issues
   - Limitations of static template analysis

> **Комментарий Codex: не подтверждено как missing.** `docs/lsp-features.md` подробно перечисляет поддержанные/неподдержанные Blade/Twig expressions, а `docs/architecture.md` описывает virtual PHP, source maps, context inference и ограничения. Отдельный troubleshooting guide можно добавить, но информация уже объёмная.

4. **Type Inference Guide**
   - PHPDoc generic inheritance
   - PHPStan/Psalm type aliases
   - Conditional return types
   - Shape type inference
   - When type inference fails and why

> **Комментарий Codex: частично подтверждено.** Архитектура и feature matrix описывают generics, aliases, shapes, conditional returns и ограничения, но материал распределён и ориентирован на разработчика. Единый user-facing guide повысил бы доступность; это documentation enhancement, не critical gap.

5. **Contributing Guide** (`CONTRIBUTING.md`)
   - How to run tests
   - How to add new LSP features
   - Code style guidelines beyond basic naming
   - How to add documentation

> **Комментарий Codex: подтверждено.** `CONTRIBUTING.md` отсутствует. Значительная часть инструкций есть в `AGENTS.md`, но внешний contributor не должен зависеть от agent-specific документа; стоит добавить короткий guide со ссылками на architecture/test selection.

6. **CHANGELOG.md**
   - No changelog tracking version changes
   - Users must read git history or release notes

> **Комментарий Codex: подтверждено.** В репозитории нет `CHANGELOG.md`. Если GitHub/Marketplace release notes являются каноническими, это следует явно указать; иначе changelog полезен для обновлений.

### 9.2 Outdated Information

**File:** `AGENTS.md:88-107`
- Missing `make server-all`, `make package-all`, `make clean` targets

> **Комментарий Codex: подтверждено как небольшая неполнота.** Makefile targets существуют, а раздел называет только «shortcuts». Добавление build/package/clean команд улучшит discoverability, но текущие перечисленные команды корректны.

**File:** `AGENTS.md:163-227` ("Where To Look" section)
- Missing references to:
  - `src/lsp/lifecycle.rs`
  - `src/lsp/inlay_hints.rs`
  - `src/indexing/cache.rs`
  - `src/util/uri.rs` and `src/util/lsp_text.rs`
- Incomplete test file references:
  - Missing `e2e_initialize.rs`
  - Missing `e2e_indexing.rs`
- Missing `server_tests.rs` reference

> **Комментарий Codex: устаревшее утверждение.** Текущий `AGENTS.md` уже содержит `lifecycle.rs`, `inlay_hints.rs`, `indexing/cache.rs`, `util/uri.rs`, `util/lsp_text.rs`, `e2e_initialize.rs`, `e2e_indexing.rs` и прямую ссылку на `server_tests.rs` в Test Selection.

**File:** `DECISIONS.md:11-27` (ADR-001)
- States extension ID is `php-lsp` but Marketplace package is `hightemp.ht-php-lsp`

> **Комментарий Codex: не является устаревшей информацией.** ADR сохраняет исходное решение, а непосредственно под ним `Текущий статус` явно фиксирует Marketplace package `hightemp.ht-php-lsp`, package name и неизменившиеся command/settings IDs.

**File:** `DECISIONS.md:30-42` (ADR-002)
- Title says "MSRV 1.75" but current status says "Rust 1.85+"

> **Комментарий Codex: не является противоречием.** ADR отделяет историческое решение от текущего статуса и прямо указывает workspace MSRV 1.85. Переписывать решение задним числом нельзя; при желании ADR можно формально пометить Superseded.

**File:** `DECISIONS.md:88-105` (ADR-004)
- Decision says `tree-sitter` v0.26 + `tree-sitter-php` v0.24
- Current status says v0.24 and v0.23

> **Комментарий Codex: та же ошибочная трактовка ADR.** Current status совпадает с `server/Cargo.toml`; первоначально выбранные версии сохранены как история решения. Улучшение — status/superseded metadata, а не замена текста решения.

**File:** `docs/production-baseline.md:3-6`
- States version `0.7.0` and revision `7be9e21`
- These will become stale quickly

> **Комментарий Codex: это point-in-time baseline по назначению.** Документ содержит даты и checked revisions; его задача — сохранять воспроизводимую историческую acceptance точку, а не всегда показывать HEAD. Обновлять следует новым dated block, не удаляя старый контекст.

### 9.3 Incomplete Documentation

**File:** `docs/architecture.md:13-16`
- Missing `php-lsp-completion` crate in component table
- Missing description of `framework.rs` and `template.rs`

> **Комментарий Codex: фактически неверно.** Component table содержит `Completion`, а server layout явно содержит `framework.rs` и `template.rs` с описаниями.

**File:** `docs/architecture.md:33-69`
- Missing files in server layout:
  - `src/lsp/hierarchy.rs`
  - `src/lsp/inlay_hints.rs`
  - `src/lsp/conversions.rs`
  - `src/util/` directory

> **Комментарий Codex: фактически неверно.** Все перечисленные пути присутствуют в актуальном layout block, включая `hierarchy.rs`, `inlay_hints.rs`, `conversions.rs`, `util/uri.rs` и `util/lsp_text.rs`.

**File:** `docs/configuration.md:139-152`
- Missing `[diagnostics.severity]` sub-keys documentation
- Should list all severity categories

> **Комментарий Codex: фактически неверно.** Таблица перечисляет `unknownSymbols`, `unused`, `duplicateSymbols`, `members`, `typeCompatibility`, `overrideSignatures`, `phpVersion`; README также документирует допустимые severity values.

**File:** `docs/configuration.md:154-158`
- Mentions `diagnostics.memberTypeBudget` as deprecated
- Missing `formatting.timeout`, `phpstan.timeout`, `psalm.timeout` as deprecated

> **Комментарий Codex: частично подтверждено.** Канонические `timeoutMs` документированы, а legacy `timeout` aliases принимаются кодом, но не отмечены в тексте как deprecated. Стоит либо документировать migration note, либо удалить aliases в следующем breaking release; это low-priority docs consistency.

**File:** `README.md:483-484`
- Missing `phpLsp.logLevel` in configuration table
- Environment variable `PHP_LSP_WORKER_THREAD_STACK_SIZE` documented separately

> **Комментарий Codex: неверно.** `phpLsp.logLevel` есть в таблице со значениями, а отдельная таблица runtime environment для `PHP_LSP_WORKER_THREAD_STACK_SIZE` — осознанное и более точное разделение setting и environment variable.

### 9.4 Language Inconsistency

**File:** `AGENTS.md:248-254`
- TASKS.md section is in Russian while rest is English

> **Комментарий Codex: факт подтверждён, проблема не установлена.** Это repository-local инструкции команды; язык совпадает с рабочим процессом пользователя. Перевод нужен только при принятой English-only documentation policy.

**File:** `DECISIONS.md:1-8`
- Header is in Russian while rest is English

> **Комментарий Codex: формулировка отчёта неточна.** ADR в значительной части русскоязычный, а `Текущий статус` добавлялся на английском. Унификация полезна для внешних contributors, но не влияет на runtime quality.

---

## 10. Recommendations

### 10.1 High Priority (Fix Immediately)

1. **Security:**
   - Add command template validation and whitelisting
   - Implement path traversal protection in `discoverComposerRoot()` and `find_composer_json()`
   - Whitelist environment variables passed to server process
   - Document security/trust model

> **Комментарий Codex:** немедленный security блок в этой форме отклоняю: trust gate уже реализован и документирован, Composer discovery не выходит через symlink, custom server path — явная настройка, inherited env не показан как leak. Реальный low-priority hardening из отчёта — заменить formatter temp directory на secure RAII tempdir.

2. **Memory Safety:**
   - Add capacity limits to `RequestTypeCache` with LRU eviction
   - Fix `TwigContextDiskCache::evict_entries_for_source_uri()` to update `order` VecDeque
   - Replace `unwrap()` with `expect()` or proper error handling

> **Комментарий Codex:** первые два пункта основаны на неверном чтении lifetime/cache cleanup; оба `unwrap` защищены length invariants. Это не high-priority memory-safety work.

3. **Testing:**
   - Add basic TypeScript client tests (extension activation, configuration handling)
   - Add unit tests for `external_command.rs` (timeout, cancellation)
   - Add error scenario tests for LSP protocol violations

> **Комментарий Codex:** client unit tests — реальный приоритет. External command timeout/cancellation уже покрыты в flat `server_tests.rs`; protocol error coverage можно расширить точечно, учитывая границу ответственности `tower-lsp-server`.

4. **Documentation:**
   - Update `AGENTS.md` "Where To Look" section with missing files
   - Update `docs/architecture.md` server layout
   - Add security model documentation

> **Комментарий Codex:** `AGENTS.md` Where To Look и `docs/architecture.md` layout уже содержат заявленные missing paths; security/trust описан в нескольких канонических документах. Полезны лишь отдельная security landing page и перечисление дополнительных Make targets.

### 10.2 Medium Priority (Fix Within 1 Month)

1. **Performance:**
   - Replace O(n) LRU cache lookups with `IndexMap` or HashMap index
   - Audit blocking I/O calls and wrap in `spawn_blocking()`
   - Remove unnecessary clones in hot paths

> **Комментарий Codex:** оставить как profiling backlog, не месячный обязательный fix. LRU bounded малыми capacities, sync IO уже вынесен, а clones обеспечивают снятие guards/coherent snapshots. Кандидаты для измерения — `Arc<TemplateDocument/FileSymbols/NamespaceMap>` и vendor LRU touch.

2. **Error Handling:**
   - Add structured error types preserving context
   - Log swallowed errors before discarding
   - Add error propagation for scope detection failures

> **Комментарий Codex:** blanket conversion не обоснован. Можно улучшить path/cwd context без логирования command/env secrets; vendor wrapper — low observability. Root scope fallback и tolerant PHPDoc parser не являются потерянными errors.

3. **Testing:**
   - Add integration tests for external tools (PHPStan, Psalm, Pint)
   - Add tests for diagnostic fixtures without coverage
   - Add PHP 8.2/8.3 feature fixtures

> **Комментарий Codex:** первая рекомендация частично уже выполнена fake-tool contract tests; реальные third-party executables не обязательны. «Fixtures without coverage» и PHP-version gaps требуют coverage report/конкретного feature matrix, которого этот аудит не предоставил.

4. **Documentation:**
   - Add VS Code settings cross-reference to `docs/configuration.md`
   - Add ADRs for major features (templates, cache, type system)
   - Translate Russian sections to English

> **Комментарий Codex:** settings cross-reference уже есть в README→`docs/configuration.md`. ADRs для крупных подсистем могут быть полезны; перевод — policy choice, не исправление дефекта.

### 10.3 Low Priority (Fix Within 3 Months)

1. **Code Quality:**
   - Extract long functions (>50 lines) into smaller helpers
   - Consolidate duplicated template replacement logic
   - Add consistent `declare(strict_types=1);` to PHP fixtures

> **Комментарий Codex:** первые два пункта допустимы только адресно; порог 50 строк сам по себе не метрика качества. Единый `strict_types` для fixtures отклоняю, потому что fixtures должны покрывать разнообразный и legacy PHP.

2. **Testing:**
   - Add property-based tests for UTF-16/byte conversion edge cases
   - Add assertion helpers for common patterns
   - Replace tight timeouts with adaptive timeouts

> **Комментарий Codex:** property tests для UTF-16 полезны, но уже есть matrix tests сравнения indexed/one-off conversion; можно расширить генеративно. Timeout следует заменять deterministic barriers там, где это возможно, а не просто «adaptive» ожиданием.

3. **Documentation:**
   - Add `CONTRIBUTING.md` and `CHANGELOG.md`
   - Add troubleshooting for template-specific issues
   - Document client/README.md regeneration process

> **Комментарий Codex:** `CONTRIBUTING.md` и changelog действительно отсутствуют; template troubleshooting полезен. Процесс `client/README.md` уже описан в `docs/architecture.md`: это generated packaging mirror, обновляемый prepublish script.

---

## 11. Conclusion

The PHP LSP codebase demonstrates solid architectural foundations with comprehensive e2e test coverage for the Rust server. However, several critical issues require immediate attention:

**Critical risks:**
- Zero TypeScript client test coverage (37K+ lines)
- Unbounded caches in production code
- Command injection vulnerability via project configuration
- Missing security model documentation

**Strengths:**
- Extensive e2e test coverage (29K+ lines across 14 test files)
- Well-structured crate organization
- Comprehensive LSP feature implementation
- Good PHPDoc/type system support

**Overall assessment:** The codebase is production-ready for trusted workspaces but requires security hardening and test coverage expansion before recommending for untrusted project configurations.

> **Комментарий Codex:** итог в части untrusted workspaces не следует из проверенных находок: именно project executable settings уже fail-closed без внешнего trust и имеют E2E. Наиболее убедительный gap отчёта — отсутствие TypeScript tests; дополнительно подтверждены secure tempdir hardening, несколько low-priority refactors/docs gaps и отдельные edge-test opportunities. Формулировки про command injection, unbounded production caches и async races следует считать опровергнутыми до появления воспроизводимого сценария.

---

## Appendix A: Files Reviewed

### Rust Server
- `server/crates/php-lsp-server/src/server.rs`
- `server/crates/php-lsp-server/src/lsp/*.rs` (14 files)
- `server/crates/php-lsp-server/src/indexing/*.rs` (4 files)
- `server/crates/php-lsp-parser/src/*.rs` (8 files)
- `server/crates/php-lsp-index/src/*.rs` (4 files)
- `server/crates/php-lsp-completion/src/*.rs` (2 files)
- `server/crates/php-lsp-types/src/lib.rs`

### TypeScript Client
- `client/src/extension.ts` (37,355 lines)
- `client/src/serverProcess.ts` (4,764 lines)
- `client/src/configuration.ts` (3,524 lines)
- `client/src/lifecycle.ts` (3,984 lines)
- `client/src/cachePath.ts` (1,247 lines)

### PHP Test Fixtures
- `test-fixtures/basic/` (5 files)
- `test-fixtures/lsp-cases/` (20+ files)
- `test-fixtures/vendor-resolve/` (8 files)
- `test-fixtures/composer-psr4/` (1 file)

### Documentation
- `README.md`
- `AGENTS.md`
- `DECISIONS.md`
- `docs/architecture.md`
- `docs/configuration.md`
- `docs/lsp-features.md`
- `docs/cli-ci.md`
- `docs/performance.md`
- `docs/production-baseline.md`
- `docs/production-risk-register.md`
- `config-schema.json`

> **Комментарий Codex о scope:** заявленный «full codebase review» не подтверждается точностью ссылок: размеры TS приняты за строки, flat test modules и актуальные docs пропущены, а часть находок уже исправлена в текущем дереве. Appendix перечисляет категории файлов, но не даёт commit hash и методику подсчёта 127 issues; для повторяемого аудита это нужно фиксировать.

---

*Report generated by Qwen3.5-397B on 2026-08-18*
