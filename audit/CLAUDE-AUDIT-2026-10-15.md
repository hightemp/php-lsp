# Аудит LSP-сервера php-lsp — 2026-10-15

Автор: Claude (Opus 5.5). Объект: `server/` (5 крейтов, ~92 тыс. строк без тестов, ~196 тыс. с тестами), коммит `8b5621f` + незакоммиченные изменения в рабочей копии.

## Как проводился аудит

- Полностью прочитаны `server.rs`, `lsp/diagnostics.rs`, `lsp/lifecycle.rs`, `main.rs`, `php-lsp-parser/src/parser.rs`, ключевые части `php-lsp-index/src/workspace.rs`, `config.rs`, обработчики rename/references/completion/formatting/watched files, загрузка стабов и кэш индекса. Остальные модули просмотрены выборочно (структура, горячие пути, рискованные паттерны).
- `cargo clippy --workspace --all-targets` — чисто, кроме 3 предупреждений в незакоммиченных файлах (см. P3).
- Гипотезы проверялись **на уже собранном** `target/debug/php-lsp` (без пересборки): `php-lsp analyze` на маленьких фикстурах и один короткий LSP-сеанс через stdio. Помеченное **[воспроизведено]** подтверждено запуском; остальное выведено из чтения кода.
- Чужие отчёты в `audit/` не читались.

## Сводка

| # | Приоритет | Проблема | Статус |
|---|---|---|---|
| 1 | **P0** | Рассинхронизация документа: `ropey` считает U+2028/U+2029/U+0085/VT/FF переводами строк, а LSP и tree-sitter — нет | воспроизведено |
| 2 | **P0** | Вся CPU-работа обработчиков выполняется в одной задаче-диспетчере tower-lsp — тяжёлый запрос блокирует весь сервер | по коду |
| 3 | **P1** | На каждое нажатие клавиши: 2× извлечение символов + 2× сбор ссылок + 2× запись в индекс синхронно, потом полный репарс в диагностике | по коду |
| 4 | **P1** | Горячий путь индекса: каждый lookup глубоко клонирует `FileSymbols` и заново парсит PHPDoc класса | по коду |
| 5 | **P1** | O(N²) при удалении файлов из индекса (перестройка, смена конфига, перезагрузка стабов) | по коду |
| 6 | **P1** | Диагностика дубликатов на каждом запуске обходит **все** файлы индекса для каждого символа | по коду |
| 7 | **P1** | Completion обрезается до 100 элементов, но отдаётся с `isIncomplete=false` | по коду |
| 8 | **P1** | Ложные диагностики: `__call`/`__callStatic`, protected-доступ из родителя, `Parent::method()` из наследника, игнор `strict_types` | воспроизведено |
| 9 | **P1** | Безопасность: `formatting.provider=auto` запускает `vendor/bin/pint` из недоверенного репозитория | по коду |
| 10 | P2 | Серверные file watchers не регистрируются → в не-VS Code клиентах индекс устаревает | по коду |
| 11 | P2 | Память: агрегированный индекс-копия, стабы на каждый корень, тяжёлый `SymbolInfo` | по коду |
| 12 | P2 | Rename по устаревшим ссылкам из индекса, без версий документов | по коду |
| 13 | P2/P3 | Прочие баги, качество кода, сборка (54 тестовых бинарника, `target/` = 128 ГБ) | см. ниже |

---

## P0 — критично

### 1. Модель документа на сервере расходится с клиентом [воспроизведено]

**Где:** `server/Cargo.toml:34` (`ropey = "1"` с фичами по умолчанию), `php-lsp-parser/src/parser.rs:63-125` (`apply_edit`).

У `ropey` по умолчанию включена фича `unicode_lines`: VT (`\x0B`), FF (`\x0C`), NEL (U+0085), LS (U+2028), PS (U+2029) и одиночный CR считаются переводами строк. В LSP переводы строк — только `\n`, `\r\n`, `\r`; tree-sitter и `Utf16LineIndex` считают только `\n`. В итоге `apply_edit` переводит LSP-строку N не в ту строку rope, и инкрементальная правка применяется не туда.

Воспроизведение (LSP по stdio): открыть `"<?php\n// a b\nfunction foo() {}\n"`, затем didChange — заменить `foo`→`bar` в строке 2, символы 9..12. Клиент видит `function bar()`, а `documentSymbol` сервера возвращает `['foo']`. После этого все функции (диагностика, rename, code actions с правками) работают с испорченным текстом, пока не придёт полный текст. U+2028 и FF встречаются в реальном коде (строки, копипаста из документации, старые файлы с `^L`).

**Исправить:**
- `ropey = { version = "1", default-features = false, features = ["simd"] }` (без `unicode_lines`; `cr_lines` — по решению, см. ниже).
- Точки tree-sitter (`Point.row`) вычислять так же, как tree-sitter — только по `\n`. Сейчас `start_point.row` берётся из строки rope. Одиночный `\r` LSP тоже считает переводом строки, а tree-sitter — нет, поэтому нужно одно правило преобразования позиций на весь сервер (желательно отдельный модуль `LineIndex`, построенный по правилам LSP и используемый всеми).
- Добавить регрессионные тесты с U+2028, FF, одиночным `\r` во всех трёх местах: `apply_edit`, `utf16_col_to_byte`, `Utf16LineIndex`.

### 2. Обработчики блокируют диспетчер tower-lsp

**Где:** `main.rs:55` (`Server::new(...).serve(service)`), все `lsp_*` в `src/lsp/`.

`tower-lsp-server` 0.23 опрашивает futures всех запросов и уведомлений внутри **одной** задачи через `buffer_unordered(4)` (`transport.rs:120`). Вся синхронная работа внутри обработчика (поиск ссылок по всему индексу, `get_members`, вывод типов, извлечение символов, inlay hints, code actions) не даёт этой задаче опрашивать другие futures. Пока идёт `references`/`rename` на большом проекте, стоят completion, hover и даже `didChange`. В `spawn_blocking` тяжёлую работу выносят только диагностика, форматирование и индексация; остальные ~35 обработчиков считают всё прямо на месте. Отмена (`$/cancelRequest`) синхронную работу тоже не прерывает.

Сопутствующий риск: два `didChange` — это два future в `buffer_unordered`, а `lsp_did_change` начинается с `request_context_for_uri(...).await` (tokio-мьютекс + кооперативный бюджет tokio). Порядок применения сейчас держится на справедливости мьютекса и порядке опроса, а не на явной гарантии. При этом `document_version_is_newer` (`server.rs`) отсекает только *старые* версии и не замечает **пропуск** (v3 поверх v1), который при инкрементальной синхронизации и есть повреждение текста.

**Исправить:**
- Тяжёлые обработчики (references, rename, implementation, call/type hierarchy, workspace symbol, code lens, inlay hints, semantic tokens, code action/resolve, completion) выполнять в `spawn_blocking` над неизменяемым снимком (`Arc<DocumentSnapshot>` + `Arc<WorkspaceIndex>`) с токеном отмены, который проверяется в циклах.
- Уведомления синхронизации документов (`didOpen/didChange/didClose/didSave`) пропускать через упорядоченную очередь на документ (mpsc + один воркер) и не зависеть от порядка опроса. Для инкрементальной правки, если версия перепрыгнула (`incoming != current + 1`, а клиент нумерует подряд), ставить флаг `documents_requiring_full_sync`, как это уже делается для перевёрнутых диапазонов.

---

## P1 — высокий приоритет

### 3. Лишняя работа на каждое нажатие клавиши

**Где:** `lsp/diagnostics.rs:747` (`lsp_did_change`), `server.rs:3079` (`synchronize_open_document_index_to_current_runtime`), `server.rs:301` (`open_document_snapshot_from_state_with_lock_hook_for_version`), `server.rs:3395+` (`schedule_fast_diagnostics`), `lsp/diagnostics.rs:1255`.

Один `didChange` сейчас делает:
1. под блокировкой шарда `DashMap` (`open_files.entry`), прямо в async-потоке: инкрементальный репарс → `extract_file_symbols_for_php_version` → `collect_symbol_references_in_file` → запись в агрегированный и корневой индексы (каждая запись берёт **эксклюзивный** глобальный `publication_barrier` индекса);
2. `synchronize_open_document_index_to_current_runtime` → снова снимок → **снова** извлечение символов и сбор ссылок → **снова** две записи в индекс;
3. через 180 мс дебаунса: ещё один снимок (третье извлечение символов, клонирование всего исходника) → `compute_source_diagnostics_blocking` **заново парсит весь файл с нуля** (`FileParser::new(); parse_full`), хотя дерево уже есть;
4. на каждую правку PHP-файла — `refresh_open_twig_contexts_and_republish_diagnostics`.

Кроме того, `FileParser::apply_edit` делает `rope.to_string()` на **каждое** изменение из `content_changes` и репарсит после каждого (`parser.rs:122`), а `FileParser::source()` при каждом вызове заново собирает `String` из rope.

`diagnostic_publish_request_is_current` (`server.rs:679`) вызывает `open_document_snapshot_from_state`, а это **полное извлечение символов файла** только ради сравнения версии и шаблона. Вызывается до 4 раз на одну публикацию, в том числе внутри `std::sync::Mutex` в `DiagnosticsPublisher::publish` (`server.rs:588`).

**Исправить:**
- Ввести кэш анализа документа на версию: `DocumentSnapshot { generation, version, source: Arc<str>, tree: Tree, line_index: Arc<LineIndex>, symbols: Arc<FileSymbols>, references: Arc<[SymbolReference]> }`, который создаётся один раз при коммите версии. Все запросы и проверки «актуальности» читают его, а не извлекают заново.
- Для проверки актуальности сравнивать только `OpenDocumentState` и id шаблона — без снимков.
- Диагностике передавать готовое дерево (`tree.clone()` дёшев), не репарсить.
- В `apply_edit` сначала применить все изменения к rope и `tree.edit`, а репарсить один раз; для парсинга использовать `parser.parse_with` по чанкам rope, без `to_string()`.
- Тяжёлую часть (извлечение символов/ссылок и коммит в индекс) делать вне блокировки `DashMap`: коммит текста под блокировкой, индексацию отдельно с проверкой версии (механизм проверки уже есть).

### 4. Горячий путь индекса: клонирование и повторный парсинг на каждый lookup

**Где:** `php-lsp-index/src/workspace.rs:1415` (`materialize_symbol`), `:1806` (`class_type_alias_for_name`), `:1877` (`file_type_alias_for_name`), `:1230` (`direct_members_from_sources`), `php-lsp-types/src/lib.rs:670` (`scoped_at_byte_position`).

`resolve_fqn`, `get_members` и `resolve_member*` вызывают `materialize_symbol` для каждого символа с сигнатурой. Он:
- клонирует `SymbolInfo` целиком;
- для **каждого простого имени типа** в параметрах и возвращаемом значении вызывает `type_alias_for_name` → `class_type_alias_for_name`, а тот:
  - делает `scoped_at_byte_position(..).into_owned()` — для `Cow::Borrowed` это **глубокая копия всего `FileSymbols`** файла (все символы с doc-комментариями и сигнатурами);
  - заново парсит doc-комментарий класса (`parse_phpdoc`);
  - затем `file_type_alias_for_name` делает ещё один `into_owned()`;
- считает `refinement_for_symbol` (ещё одно разрешение имён).

`direct_members_from_sources` на каждый вызов делает `Arc::new(symbol.clone())` для каждого члена. `get_members` собирает всю иерархию (трейты, миксины, родители, интерфейсы) с материализацией каждого члена, а вызывающие код часто делают `get_members(...).into_iter().find(...)` (например, `resolve_member_on_class_for_ref_kind`, `diagnostics.rs`) — то есть строят полный список ради одного элемента.

Для класса вроде Eloquent `Model` (сотни членов, глубокая иерархия) одна проверка обращения к члену в диагностике стоит сотен глубоких копий. Диагностика, completion, hover и inlay hints вызывают это постоянно.

**Исправить:**
- Хранить символы как `Arc<SymbolInfo>` внутри `FileSymbols` (`Vec<Arc<SymbolInfo>>`), чтобы `direct_members` раздавал `Arc::clone`, а не копии.
- Таблицы псевдонимов типов (`@phpstan-type`, `@psalm-type`, импорты) один раз разбирать при извлечении символов и хранить в индексе как `HashMap<(scope), Arc<TypeAliasTable>>`; не вызывать `parse_phpdoc` в lookup.
- `scoped_at_byte_position` должен возвращать лёгкий вид `NamespaceScopeView { namespace, uses: &[UseStatement] }`, а не клон файла; `into_owned()` в индексе убрать.
- Кэшировать материализованный символ по `(fqn, substitutions-hash, mutation_revision)`. Ревизия индекса уже есть (`revision_snapshot`).
- Добавить `find_member(class, name, kinds)` с ленивым обходом иерархии и заменить им шаблон `get_members(..).find(..)`.

### 5. O(N²) при массовом удалении файлов из индекса

**Где:** `php-lsp-index/src/workspace.rs:836` (`find_top_level_symbol_replacement`), вызывается из `remove_top_level_symbol` на каждый удаляемый верхнеуровневый символ. Массовые вызывающие: `server.rs` `clear_non_stub_symbols`, `rebuild_aggregate_index`, `indexing/stubs.rs:24` `remove_stub_symbols`, `replace_stub_symbols_from`, `replace_from_staged_if_sources_current`.

Если удалённый символ был текущим владельцем ключа, индекс ищет замену полным перебором **всех файлов и всех их символов**. При перестройке удаляются все файлы по одному, поэтому выходит O(символы × все символы): стабы (~10⁴ символов) × проект (~10⁵) ≈ 10⁹ сравнений на смене `phpVersion`/`stubs`/`indexing.*`, на перезагрузке composer и при перестройке агрегата. Удаление выполняется под эксклюзивным `publication_barrier` — все читатели ждут.

**Исправить:**
- Вести вторичную карту `top_level_key → SmallVec<uri>` (все определения ключа). Замена — O(1), заодно это даёт честную модель дубликатов (см. п. 6).
- Для массовых операций добавить `remove_files_bulk(&[uri])` / `clear_where(pred)`: удалить всё, затем один раз пересчитать владельцев ключей.

### 6. Диагностика дубликатов обходит весь индекс

**Где:** `lsp/diagnostics.rs:4713` (`workspace_duplicate_symbol_diagnostics`).

Для каждого класса/функции/константы открытого файла выполняется `index.read().file_symbols().iter()` по **всем** файлам (включая тысячи стабов) с перебором всех их символов. Это происходит на каждом расчёте диагностики (после дебаунса каждой правки и на каждый publish). На больших проектах — десятки миллисекунд и больше на пустом месте, плюс удержание read-лизинга.

**Исправить:** использовать ту же карту `key → [uri]` из п. 5: дубликат = больше одного не-builtin uri.

### 7. Completion теряет результаты из-за `isIncomplete=false`

**Где:** `php-lsp-completion/src/provider.rs:722`, `:827` (`items.truncate(100)`), `lsp/completion.rs:191`, `:538` (`CompletionResponse::Array`).

Список классов/функций/неймспейсов обрезается до 100, а возвращается как `Array` (то есть `isIncomplete=false`). Клиент дальше фильтрует **эти 100 элементов локально** и не перезапрашивает при наборе следующих символов. Если по префиксу `Str` набралось больше 100 кандидатов, нужный `StreamedResponse` может не появиться вообще.

**Исправить:** при обрезке возвращать `CompletionResponse::List(CompletionList { is_incomplete: true, items })`; лимит вынести в настройку.

### 8. Ложные диагностики [воспроизведено]

Проверено через `php-lsp analyze` на маленьких фикстурах.

| Код | Сообщение сервера | Почему ложное |
|---|---|---|
| класс с `public function __call(...)`, вызов `$m->anything()` | `Unknown method: App\Magic::anything` | `is_dynamic_member_access` (`diagnostics.rs:2366`) проверяет только `__get` для свойств; `__call`/`__callStatic` в коде не обрабатываются нигде |
| `Magic::staticThing()` при `__callStatic` | `Unknown method` | то же |
| `__call(string $name, array $args) { return null; }` | `Unused parameter: $name`, `$args` | сигнатура магических методов (и абстрактных/интерфейсных реализаций) задана языком; переименовать в `$_` нельзя |
| `class A { function run(B $b) { $b->hook(); } }`, `class B extends A { protected function hook() }` | `Protected member is not accessible here: App\B::hook` | PHP разрешает доступ, если класс области видимости — предок **или** потомок корневого объявляющего класса (`zend_check_protected`); `class_can_access_protected_member` (`:2817`) проверяет только одно направление |
| в `B extends A`: `A::base()` (нестатический метод) | `Instance method called statically: App\A::base` | вызов `ParentClass::method()` из наследника — законный вызов родителя (как `parent::`); `static_instance_misuse_message` (`:2716`) разрешает только `self/static/parent` |
| `function takesInt(int $x)`; `takesInt("5")` в файле **без** `declare(strict_types=1)` | `Type mismatch ... expected int, got string` | в слабом режиме PHP приводит числовую строку; `strict_types` в коде не учитывается нигде (`grep` пусто) |
| `takesString(new Name())`, где `Name implements \Stringable` | `Type mismatch ... expected string, got App\Name` | в слабом режиме `Stringable` → `string` допустимо |

**Исправить:**
- В `is_dynamic_member_access` для `MethodCall` проверять `__call` в иерархии, для статического вызова — `__callStatic`; то же в composite receivers.
- Не выдавать `unusedParameter` для магических методов, для методов, переопределяющих или реализующих родительский/интерфейсный (это уже частично работает для интерфейсов), и для абстрактных.
- Protected: разрешать при `extends(current, declaring) || extends(declaring_root, current)`, где `declaring_root` — класс, впервые объявивший метод в иерархии.
- Статический вызов нестатического метода: разрешать, если `object_expr` — класс-предок текущего класса и вызов идёт из нестатического контекста.
- Совместимость типов: извлекать `declare(strict_types=1)` в `FileSymbols`; в слабом режиме принимать числовые строки для `int|float`, скаляры для `string`, `Stringable`-объекты для `string`, `int`/`float`/`string` для `bool`. В строгом оставить текущую логику (`int`→`float` разрешён в обоих режимах — это уже так).

### 9. Безопасность: автоопределение форматтера запускает код из репозитория

**Где:** `lsp/formatting.rs:60` (`detect_project_formatter_tool`), `server.rs:1502` (`command_template`), `lsp/external_command.rs`.

Проектные команды (phpstan/psalm/formatting.command) аккуратно закрыты флагом `allowProjectCommands`, и whitelist в `normalize_project_config_settings` не даёт обойти это плоскими ключами (проверено). Но значение по умолчанию `formatting.provider = "auto"`: если в `composer.json` репозитория есть `laravel/pint` (или php-cs-fixer/phpcs), сервер выполняет `sh -c "vendor/bin/pint --quiet <file>"` с `cwd` = корень проекта. `vendor/bin/pint` полностью контролируется содержимым репозитория (может быть закоммичен или подложен). При format-on-save это выполнение произвольного кода при сохранении файла в чужом проекте, без всякого доверия.

**Исправить:** в режиме `auto` без `allowProjectCommands` (или без workspace trust клиента) не запускать бинарники из проекта: либо только при явном доверии, либо только системные из `PATH` с логом-предупреждением. Задокументировать.

Попутно (Windows): `shell_escape` (`external_command.rs:12`) оборачивает путь в `"..."` и передаёт его в `cmd /C` через `Command::arg`, который ещё раз экранирует по правилам MSVCRT. Кавычки ломаются, `%VAR%` раскрывается, `&`/`^` в имени файла опасны. Лучше вообще не использовать shell: парсить шаблон в argv (`shell-words`) и подставлять `{file}` отдельным аргументом.

---

## P2 — средний приоритет

### 10. Watchers и события файловой системы

- **Регистрация watchers.** Сервер регистрирует `workspace/didChangeWatchedFiles` только для внешних симлинков (`indexing/symlinks.rs:251`); для `**/*.php`, `composer.json/lock`, `vendor/composer/installed.json`, `.php-lsp.toml` полагается на VS Code-клиент (`client/src/extension.ts:819`). В Neovim/Helix/Zed/Sublime индекс не узнает о `git checkout`, `composer install` и генерации кода. **Исправить:** при `dynamicRegistration` регистрировать эти глобы с сервера.
- **Нет пакетной обработки.** `lsp_did_change_watched_files` (`indexing/workspace.rs:608`) обрабатывает события строго по одному (`reindex_php_file(..).await` на каждое). После `git checkout` на 5 тыс. файлов это 5 тыс. последовательных репарсов с двойной записью в индекс и пересчётом twig-контекстов. **Исправить:** собирать события с коротким дебаунсом, парсить пакетом в `spawn_blocking` с ограниченным параллелизмом, коммитить пачкой; при большом пакете запускать инкрементальную переиндексацию через fingerprint-кэш.
- **`willCreate`/`willDelete` объявлены, но ничего не делают** (`lsp/lifecycle.rs:149-153`, обработчики возвращают `None`). Это блокирующие запросы: VS Code ждёт ответа перед операцией с файлом. Либо убрать из capabilities, либо реализовать полезное: `willCreateFiles` — каркас класса с неймспейсом по PSR-4; `willRenameFiles` (сейчас `None`) — обновление namespace/имени класса при перемещении файла.
- **`didChangeConfiguration` с `settings: null`** (`indexing/workspace.rs:675`): клиенты с pull-моделью (Neovim) присылают `null` и ждут `workspace/configuration`. Сервер записывает `null` в `client_settings` и теряет `initializationOptions` (в том числе `bundledStubsPath`). **Исправить:** при `null`/пустом значении запрашивать `workspace/configuration`, если клиент это поддерживает, иначе сохранять предыдущие настройки.

### 11. Память (на машине мало ОЗУ — это важно)

- **Агрегированный индекс — полная копия.** `self.index` (агрегат) хранит копии символов **и ссылок** всех workspace-корней (`update_aggregate_and_root_index`, `copy_non_stub_symbols`, `rebuild_aggregate_index` — `server.rs:2390`, `:2501`). Используется в нескольких местах (workspace symbols без конфигов, code lens, call hierarchy, часть definition). Это удвоение памяти проектных символов и ссылок. **Исправить:** заменить копию на федеративный вид поверх корневых индексов (запрос по каждому `Arc<WorkspaceIndex>` + слияние).
- **Стабы на каждый индекс.** phpstorm-stubs загружаются в fallback-индекс и в индекс **каждого** workspace-корня, копированием из staging (`indexing/stubs.rs`, `replace_stub_symbols_from`). При N корнях — N+1 копия. **Исправить:** один неизменяемый `Arc<StubIndex>` на `(stubs_path, extensions, php_version)`, общий для всех корней (слой «только чтение» под индексом).
- **Тяжёлый `SymbolInfo`.** У каждого члена класса свои `uri: String` (полный путь), `parent_fqn: String`, `fqn: String` с полным именем класса и сырой `doc_comment`. Верхнеуровневые символы ещё и клонируются в таблицы `types/functions/constants` (`Arc::new(sym.clone())`). **Исправить:** интернировать `uri`/`parent_fqn` (`Arc<str>`), в таблицах хранить локаторы `(Arc<FileSymbols>, idx)` (как уже сделано для членов), `doc_comment` хранить отдельно и лениво.
- **Снимки документа** клонируют исходник целиком (`parser.source()`), а для шаблонов — весь `TemplateDocument` (исходный и виртуальный текст плюс source map) на каждый запрос и проверку. Решается снимками на `Arc` из п. 3.
- **Дисковый кэш** читается целиком в память (`fs::read` + `bincode::deserialize`, `php-lsp-index/src/cache.rs:226`): пик = размер файла + десериализованная структура. Для vendor-кэша стоит перейти на шардирование (файл на пакет) или потоковое чтение.

### 12. Rename и references

- **Устаревшие ссылки из индекса** (`lsp/rename.rs`). Правки для закрытых файлов строятся по ссылкам, посчитанным при индексации. Если файл изменился на диске без события watcher'а (см. п. 10), диапазоны указывают не туда и rename портит код. **Исправить:** перед формированием правки сверять `SourceFingerprint` файла с диском (механизм уже есть) и при расхождении перепарсить.
- **Нет версий документов.** Ответ всегда идёт через `changes` (`rename.rs:90`, `:229`), даже если клиент поддерживает `documentChanges`. Для открытых документов нужно отдавать `TextDocumentEdit` с версией, чтобы клиент отклонил правку при гонке. Для `code_action` это уже сделано (`code_action.rs:6586`) — переиспользовать.
- **Переименование класса не переименовывает файл** по PSR-4 (`RenameFile` в `documentChanges`, если клиент поддерживает `resourceOperations`).
- `let _names = function_name_scope_from_index(...)` (`rename.rs:52`, `:276`) — вычисляется и выбрасывается, лишняя работа.
- `reference_scan_matches` синхронно сканирует все файлы, а `cooperative_heavy_request_yield` вызывается **после** скана, в цикле по уже готовому результату, — уступать управление там бессмысленно (см. п. 2).
- `lsp_prepare_rename`/`document_highlight` держат read-guard `open_files` (`DashMap::get`) на всё время тяжёлого разрешения символа. Сейчас взаимной блокировки нет, но любой будущий вызов `open_files.entry(...)` для того же шарда внутри этого кода её создаст. Сначала копировать `tree`+`source` (или брать `Arc`-снимок), потом отпускать guard.

### 13. Прочие функциональные замечания

- **Синтаксическая ошибка скрывает всю семантику** (`diagnostics.rs:1599`). При любой незакрытой скобке исчезают все предупреждения файла (unused, unknown class и т. д.), а при исправлении появляются снова — диагностика «мигает» во время набора. Варианты: показывать последнюю семантическую диагностику до восстановления; считать семантику для узлов вне поддеревьев `ERROR`.
- **Ленивая догрузка по тексту сообщения** (`lazy_resolvable_diagnostic_fqn`, `:2642`): FQN и вид ссылки извлекаются из `diagnostic.message` по префиксам `"Unknown class: "` и т. п. Любая правка текста или локализация сломает фильтрацию. **Исправить:** класть `{ fqn, ref_kind }` в `Diagnostic.data` (или во внутреннюю структуру до конвертации в LSP).
- **Приоритет `@mixin` выше родителя** (`php-lsp-index/src/workspace.rs:1114`, `:1290`): при поиске члена миксины обходятся раньше `extends`. В PHP настоящий метод родителя всегда выигрывает, а миксин работает только через `__call`. Если имя совпадает, получаем неверный символ и тип. Порядок должен быть: свои → трейты → родители → интерфейсы → миксины.
- **Проверка override только по прямым родителям** (`diagnostics.rs:4146`): несовместимость с методом деда или трейта не ловится. Не проверяются сужение видимости, `static`/не-`static`, переопределение `final`.
- **Захардкоженные эвристики PHPUnit** (`diagnostics.rs`: списки методов `assert*`/`createMock`/`willReturn*`, `fqn.ends_with("\\TestCase")`, трейты `*TestTrait` или из неймспейса `Tests`). Это противоречит правилу «без хардкода» из AGENTS.md и ломается на других фреймворках. Члены PHPUnit известны из vendor или стабов; надёжнее догружать `PHPUnit\Framework\TestCase` и `MockObject` лениво и проверять реальной иерархией.
- **Версия PHP не берётся из `composer.json`** (`require.php`, `config.platform.php`); по умолчанию 8.2. Для проектов на 7.4/8.0 диагностика версии языка молчит, пока нет конфига. Предлагаю выводить версию из ограничения composer как дефолт ниже явной настройки.
- **Type hierarchy только в `experimental`** (`lifecycle.rs:209`): стандартные клиенты, кроме собственного VS Code-клиента, не узнают о поддержке. В `ls_types` нет поля `type_hierarchy_provider`; можно сериализовать `ServerCapabilities` в JSON и добавить `typeHierarchyProvider` на верхний уровень.
- Путь `root.join("server/data/stubs")` (`indexing/stubs.rs:100`) — путь из разработки репозитория php-lsp, который проверяется в **каждом пользовательском проекте**. В чужом проекте с таким каталогом подхватятся чужие стабы. Оставить только под `cfg(debug_assertions)` или явной настройкой.
- `MAX_CONCURRENT_FILE_IO_TASKS = 2` (`server.rs:1013`) — глобальный семафор на весь файловый ввод-вывод запросов. Ленивое чтение vendor-файла в completion может ждать фоновые операции. Стоит развести пулы для интерактивных и фоновых операций.
- В первом stdio-сеансе процесс не завершился за 5 с после `shutdown`+`exit`, когда сразу после `didOpen`/`didChange` шла фоновая работа. В простое выход занимает 0.06 с. Вероятная причина: `Runtime` при выходе из `block_on` ждёт незавершённые `spawn_blocking`. Стоит явно вызывать `runtime.shutdown_timeout(...)` после `serve` и проверить отмену индексации в `lsp_shutdown`.

---

## P3 — качество кода и поддерживаемость

- **Размер модулей.** `code_action.rs` (7.5k строк), `resolve.rs` (7.4k), `diagnostics.rs` (5.2k), `server.rs` (4.9k), `templates.rs` (4.8k), `inlay_hints.rs` (4.3k; в нём живёт вывод типов локальных переменных, который используют hover и диагностика), `framework.rs` (4.2k). Разделить по ответственности: `diagnostics/{sync,publish,members,types,overrides,analyzers}.rs`, вывод типов локальных переменных — в парсерный крейт или `analysis/`.
- **Тестовые поля в продовом бэкенде.** В `PhpLspBackend` ~16 полей под `#[cfg(test)]` (`server.rs:2874+`: `stubs_path`, `php_version`, `diagnostics_mode`, ...) и отдельная `apply_resolved_configuration`, которая обновляет **их**, а не `runtime_state`. Тесты конфигурации идут не по продовому пути. Аналогично многочисленные `*_with_hook` / `*_with_lock_hook` в продовом коде. Предлагаю тестировать через `runtime_state` и инжектировать хуки трейтом/фичей `test-hooks`.
- **Дублирование:**
  - блок запуска индексации (`tokio::spawn` с `index_workspace` → `preload_vendor_entrypoints` → `postprocess_workspace_indexing_runs`) скопирован в `lsp_initialized` (`indexing/workspace.rs:441`) и `reindex_workspace_folders` (`server.rs`);
  - ветки `Occupied`/`Vacant` в `lsp_did_open` (`diagnostics.rs:451+`) почти идентичны (для шаблонов и для PHP);
  - ветки `Ok`/`Err(poisoned)` в `DiagnosticsPublisher::publish` (`server.rs:588`) дублируют одну логику — достаточно `unwrap_or_else(PoisonError::into_inner)`.
- **Взрыв API вывода типов:** `symbol_at_position`, `_with_resolvers`, `_with_full_resolvers`, `infer_variable_type_*_with_resolver(s)` и т. д. Свести к одной функции с `InferenceContext { resolvers..., budget }`. Типы ходят между слоями то как `String` (`MemberTypeResolver -> Option<String>`), то как `TypeInfo`, с повторным парсингом. Стоит договориться о `TypeInfo` везде.
- **Синхронизация:**
  - `runtime_state: Arc<tokio::Mutex<Arc<WorkspaceRuntimeState>>>` берётся почти в каждом запросе и для каждого элемента в воркере публикации диагностик — классический случай для `arc_swap::ArcSwap`;
  - `trace_level`, `work_done_progress_supported`, `workspace_root` в `tokio::Mutex` лучше сделать атомиками или `ArcSwap`;
  - в `twig_context.rs` `std::Mutex::lock().unwrap()` (16 мест) — не так, как в остальном коде (`PoisonError::into_inner`).
- `WorkspaceRequestContext::index(&self, _aggregate)` (`server.rs:2263`) — параметр не используется, сигнатура вводит в заблуждение.
- Вывод типов устроен как обратный скан от позиции для каждого запроса, с бюджетами (`resolve_budget.rs`). Inlay hints и диагностика спрашивают тип для сотен узлов файла, и каждый вопрос заново сканирует предшествующие инструкции. Стратегически стоит один проход data-flow на функцию за версию документа (`variable_flow.rs` уже частично это делает), с кэшем `(scope, var, position) → type`.
- **Clippy** (только незакоммиченные файлы):
  - `lsp/code_action_phpdoc.rs:120` — одинаковые ветки `if` (`template*` и общий случай формируют одну и ту же строку);
  - `lsp/inherited_method_contracts.rs:536` — можно заменить на `?`;
  - `lsp/inherited_method_contracts_tests.rs:10` — поля `trait_uri`, `trait_source` не читаются.

## Сборка и инфраструктура (важно при нехватке памяти)

- **54 интеграционных теста = 54 бинарника.** Каждый файл в `crates/php-lsp-server/tests/*.rs` линкуется отдельно со всем серверным крейтом (~100 МБ debug каждый). Это основной источник пиков памяти при линковке и объёма `target/`. **Исправить:** собрать в один бинарник `tests/e2e/main.rs` с `mod completion; mod hover; ...` — линковка одна, память и время падают кратно.
- **`server/target` = 128 ГБ** (из них `debug/deps` 40 ГБ), рядом лежат ручные бэкапы `.rs` (`p2-24-marker-check-backup.rs`, `p2-30-server-before-mutation.rs`), сотни `.log`, каталоги `llvm-cov-target`, `flycheck0`. Рекомендации: `cargo clean` / `cargo sweep -t 7`; логи и бэкапы складывать в отдельный каталог вне `target`.
- **Профили** в `Cargo.toml` не заданы. Предлагаю:
  - `[profile.dev] debug = "line-tables-only"` (заметно меньше отладочной информации и памяти линкера);
  - `[profile.dev.package."*"] opt-level = 1` (зависимости вроде tree-sitter быстрее в тестах);
  - `[profile.release] lto = "thin", codegen-units = 1, strip = true`.
- Зависимости: `tree-sitter 0.24` / `tree-sitter-php 0.23` (синтаксис PHP 8.4 — property hooks, асимметричная видимость, `new X()->m()` — разбирается без ошибок, проверено). `bincode 1` в режиме сопровождения — при следующем изменении схемы кэша можно перейти на `bincode 2`/`postcard`.

## Что сделано хорошо (сохранить)

- Тщательная защита от гонок: поколения документов, токены перезагрузки закрытых файлов, лизинги индексации, проверка актуальности перед публикацией диагностики, атомарная запись кэша (tmp + `sync_all` + rename) с версионированием схемы.
- Доверие к проектным командам: whitelist нормализации TOML не даёт обойти `allowProjectCommands` плоскими ключами.
- Бюджеты вывода типов и диагностики защищают от патологических файлов.
- Проверки членов, переменных и override-вариантности на типичном коде дают точные результаты: на фикстурах с `extract`/`compact`/`global`/`static`/деструктуризацией/замыканиями по ссылке/`$$var`/`?->`/enum/readonly/first-class callable ложных срабатываний нет, настоящие ошибки находятся.
- Clippy чистый на закоммиченном коде.

## Предлагаемый порядок работ

1. **P0-1** (ropey + единый `LineIndex` + тесты) — небольшой изменение, устраняет порчу данных.
2. **P1-7** (`isIncomplete`), **P1-8** (`__call`, protected, `Parent::`, `strict_types`, unused params магических методов), **P1-9** (форматтер без доверия) — точечные исправления.
3. **P1-3 + P0-2**: снимок документа на `Arc`, упорядоченная очередь синхронизации, вынос тяжёлых обработчиков в `spawn_blocking` с отменой.
4. **P1-4/5/6**: карта `key → [uri]`, массовое удаление, `Arc<SymbolInfo>`, предразобранные псевдонимы типов, кэш материализации.
5. **P2-11**: федеративный агрегат и общий слой стабов (заметная экономия памяти).
6. Объединение интеграционных тестов в один бинарник и профили сборки — можно сделать в любой момент, сразу снизит нагрузку на память при `cargo test`.
