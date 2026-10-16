# Аудит LSP-сервера php-lsp — 2026-10-15

Автор: Claude (Opus 5.5). Объект: `server/` (5 крейтов, ~92 тыс. строк без тестов, ~196 тыс. с тестами), коммит `8b5621f` + незакоммиченные изменения в рабочей копии.

## Как проводился аудит

- Полностью прочитаны `server.rs`, `lsp/diagnostics.rs`, `lsp/lifecycle.rs`, `main.rs`, `php-lsp-parser/src/parser.rs`, ключевые части `php-lsp-index/src/workspace.rs`, `config.rs`, обработчики rename/references/completion/formatting/watched files, загрузка стабов и кэш индекса. Остальные модули просмотрены выборочно (структура, горячие пути, рискованные паттерны).
- `cargo clippy --workspace --all-targets` — чисто, кроме 3 предупреждений в незакоммиченных файлах (см. P3).
- Гипотезы проверялись **на уже собранном** `target/debug/php-lsp` (без пересборки): `php-lsp analyze` на маленьких фикстурах и один короткий LSP-сеанс через stdio. Помеченное **[воспроизведено]** подтверждено запуском; остальное выведено из чтения кода.
- Чужие отчёты в `audit/` не читались.
- Второй проход: 7 параллельных проверок по участкам (вывод типов; символы и PHPDoc; code actions и CLI; completion, hover, inlay; навигация и форматирование; индексация и кэш; шаблоны и фреймворки), тоже без сборки. Добавили ≈85 находок (раздел D), два пункта первого прохода уточнены.

## Сводка

| # | Приоритет | Проблема | Статус |
|---|---|---|---|
| 1 | **P0** | Рассинхронизация документа: `ropey` считает U+2028/U+2029/U+0085/VT/FF переводами строк, а LSP и tree-sitter — нет | выполнено (2026-10-15), см. п. 1 |
| 2 | **P0** | Вся CPU-работа обработчиков выполняется в одной задаче-диспетчере tower-lsp — тяжёлый запрос блокирует весь сервер | выполнено (2026-10-16), см. п. 2 |
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
| D-NAV-1 | **P0** | Форматирование Blade/Twig заменяет шаблон отформатированным **виртуальным PHP** | воспроизведено |
| D-NAV-4 | **P1** | Внешние команды (phpstan/psalm/форматтер) наследуют stdin сервера и могут съесть JSON-RPC | по коду, подтверждено |
| D-ACT-1/2 | **P1** | Quickfix «Import» вставляет `use` перед `declare(strict_types=1)` или даже перед `<?php` | воспроизведено |
| D-ACT-3/4 | **P1** | `add-return-type` (fix/code action) берёт PHPDoc соседней функции и ломает реализации интерфейсов | воспроизведено |
| D-SYM-1/2 | **P1** | `const` в enum → ложная синтаксическая ошибка на весь файл; константы из `define()` (≈500 в стабах) не извлекаются | воспроизведено / подтверждено |
| D-INF-2/3/4/6 | **P1** | Вывод типов: instanceof перекрывает переприсваивание; by-ref out-параметры только у `preg_*`; `parse_str`; `$this` в трейте | воспроизведено |
| D-NAV-2/3 | **P1** | On-type форматирование меняет содержимое heredoc; rename параметра не трогает именованные аргументы, rename переменной — `compact()` | воспроизведено |
| D-IDX-1..4 | **P1** | LRU выталкивает autoload-`files` (пропадают `collect()`, `app()`); livelock перестройки агрегата; случайный порядок PSR-4 → полная переиндексация; watcher `DELETED` убивает открытый документ | по коду, частично подтверждено |
| D-TPL-1/2/4/5 | **P1** | Twig `~`, короткий `{% block %}`; квадратичный маппинг позиций шаблона; поиск Laravel-макросов читает весь проект с диска на каждый запрос | воспроизведено / по коду |
| D-* | P2/P3 | Ещё ≈60 находок по участкам | раздел D |

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

**Статус: выполнено (2026-10-15).** Исходное описание находки выше сохранено.

- **Исправлено:** общий `php-lsp-parser/src/line_index.rs` переводит полные пары строка/столбец между LSP (LF/CRLF/CR, UTF-16), байтовыми смещениями и Tree-sitter (только LF, байты). `FileParser` хранит индекс вместе с текстом/деревом; `InputEdit` считает только LF. У Ropey отключён `unicode_lines`, сохранён `cr_lines`; исходные байты не нормализуются. Входные запросы и исходящие диапазоны переведены на общий механизм, уже готовые LSP-диапазоны не конвертируются повторно. Шаблоны разделяют неизменяемые индексы original/virtual source; пакетные преобразования переиспользуют индекс. Старый кеш пересоздаётся по schema 39.
- **TDD:** 34 новых теста; сначала зафиксирован RED на рассинхронизации текста/CST и протокольных диапазонах, затем на найденных при review PHPDoc/refactor/lexical случаях и повторных построениях индекса. Проверяются все пять Unicode-разделителей, LF/CRLF/CR и смешанные окончания, emoji/границы UTF-16/EOF, последовательные и пакетные правки, close/reopen, навигация/rename/диагностика/completion/signature/inlay/иерархии/символы/lenses/tokens/folding/selection и шаблоны. Управляемые барьеры проверяют согласованность reader/edit/edit и template edit/reopen; кеш проверен на миграцию и повторную загрузку. Счётчики требуют одно построение индекса на каждое извлечение symbols/references/tokens из 1000 объявлений и на файл в выдаче 100 callers/subtypes, без построения UTF-16 карты для уже известных CST-координат вывода типов.
- **Проверки:** окончательный `cargo test --all` — 2004 PASS / 0 FAIL / 0 ignored; Clippy `--all-targets -- -D warnings`, Rustfmt и `git diff --check` — PASS. Все Rust-проверки последовательные (`CARGO_BUILD_JOBS=1`, `--test-threads=1`), с одним CPU и ограничением памяти. Повторный read-only Verifier (`gpt-6-astra`) — GO. Артефакты RED/GREEN и хеши проверенных исходников: `server/target/document-lines-2026-10-15/` (ignored).
- **Граница:** для файлов с одиночным CR отключены редактирующие quick fix внешних PHPStan/Psalm, включая legacy replacement/ignore: их соглашения о строках ещё не имеют проверенного адаптера. Внешняя диагностика продолжает публиковаться, но её CR-диапазоны не гарантированы. Встроенные функции и обычные LF/CRLF analyzer actions сохранены. Проверки выполнялись на Linux; это конечный набор сценариев/порядков событий, не доказательство отсутствия всех гонок. Большой performance baseline и проценты покрытия не перемерялись.
- **Документация:** актуализированы Position Model и правила AGENTS, feature/test-evidence matrices, описание analyzer quick fixes и текущей версии кеша. Регрессии: [`document_line_tests.rs`](../server/crates/php-lsp-parser/src/document_line_tests.rs), [`line_index_tests.rs`](../server/crates/php-lsp-parser/src/line_index_tests.rs), [`e2e_document_lines.rs`](../server/crates/php-lsp-server/tests/e2e_document_lines.rs), [`selection_ranges_tests.rs`](../server/crates/php-lsp-server/src/lsp/selection_ranges_tests.rs), [`hierarchy_position_tests.rs`](../server/crates/php-lsp-server/src/lsp/hierarchy_position_tests.rs).

### 2. Обработчики блокируют диспетчер tower-lsp

**Где:** `main.rs:55` (`Server::new(...).serve(service)`), все `lsp_*` в `src/lsp/`.

*Уточнение:* inlay hints, code lens и call hierarchy всё-таки вынесены в `spawn_blocking`, но через **файловый** семафор на 2 слота с таймаутом 15 с — см. раздел D, п. D-NAV-10 и D-CMP-5.

`tower-lsp-server` 0.23 опрашивает futures всех запросов и уведомлений внутри **одной** задачи через `buffer_unordered(4)` (`transport.rs:120`). Вся синхронная работа внутри обработчика (поиск ссылок по всему индексу, `get_members`, вывод типов, извлечение символов, inlay hints, code actions) не даёт этой задаче опрашивать другие futures. Пока идёт `references`/`rename` на большом проекте, стоят completion, hover и даже `didChange`. В `spawn_blocking` тяжёлую работу выносят только диагностика, форматирование и индексация; остальные ~35 обработчиков считают всё прямо на месте. Отмена (`$/cancelRequest`) синхронную работу тоже не прерывает.

Сопутствующий риск: два `didChange` — это два future в `buffer_unordered`, а `lsp_did_change` начинается с `request_context_for_uri(...).await` (tokio-мьютекс + кооперативный бюджет tokio). Порядок применения сейчас держится на справедливости мьютекса и порядке опроса, а не на явной гарантии. При этом `document_version_is_newer` (`server.rs`) отсекает только *старые* версии и не замечает **пропуск** (v3 поверх v1), который при инкрементальной синхронизации и есть повреждение текста.

**Исправить:**
- Тяжёлые обработчики (references, rename, implementation, call/type hierarchy, workspace symbol, code lens, inlay hints, semantic tokens, code action/resolve, completion) выполнять в `spawn_blocking` над неизменяемым снимком (`Arc<DocumentSnapshot>` + `Arc<WorkspaceIndex>`) с токеном отмены, который проверяется в циклах.
- Уведомления синхронизации документов (`didOpen/didChange/didClose/didSave`) пропускать через упорядоченную очередь на документ (mpsc + один воркер) и не зависеть от порядка опроса. Для инкрементальной правки, если версия перепрыгнула (`incoming != current + 1`, а клиент нумерует подряд), ставить флаг `documents_requiring_full_sync`, как это уже делается для перевёрнутых диапазонов.


**Статус (2026-10-16): выполнено через TDD.** Повторяемый read-only review Verifier на **gpt-6-astra: итоговый GO**. Исходное описание выше сохранено.

- **Реализовано:** production stdio использует [`dispatch::serve`](../server/crates/php-lsp-server/src/dispatch.rs). Вызов обычного обработчика и каждый опрос его future выполняются в ограниченном blocking-пуле; ожидание I/O освобождает CPU-слот. Приём сообщений и отмена продолжаются во время тяжёлого опроса. Уведомления документа получают последовательные commit-fences при поступлении, а последующие запросы ждут уже принятые изменения, включая зависимости в других файлах. Медленные диагностика, восстановление диска и Twig post-processing отпускают fence после обязательной подготовки/применения состояния. Epoch-проверки отклоняют ответы после изменения или close/reopen целевого документа; существующие проверки snapshot/runtime/index остаются обязательными.
- **Отмена и ресурсы:** обходы символов, ссылок и semantic tokens, а также бюджеты вывода типов проверяют отмену read-only запроса. CPU-слот удерживается до фактического завершения опроса; admission — также до получения/удаления ответа. Защищены повторное использование ID, отмена до первого опроса, потерянные пробуждения, panic и shutdown/exit. Лимиты запросов, уведомлений и удерживаемого JSON описаны в [архитектуре](../docs/architecture.md#transport-and-handler-execution). При перегрузке обычный запрос получает server busy; переполнение обязательных уведомлений завершает сессию, требуя новой синхронизации, вместо молчаливой потери правки. Stdio-процесс завершает runtime с ограниченным ожиданием, даже если клиент оставляет stdin открытым.
- **Уточнение исходного предложения:** проверка `incoming == current + 1` не добавлена: [LSP допускает непоследовательные номера версий](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#versionedTextDocumentIdentifier). Порядок гарантируется для реально принятых уведомлений; тесты включают переход 2 → 7. Решение изолирует опрос обработчиков на общей границе транспорта, сохраняя существующие снимки и проверки актуальности; полной переработки `DocumentSnapshot` из соседнего пункта 3 здесь нет.
- **TDD и проверки:** добавлено 36 тестов: 32 серверных, 3 парсерных и 1 реального stdio-процесса. Первые четыре транспортные регрессии воспроизвели блокировку лёгкого запроса, отмены и очереди правок; дополнительные RED-проверки выявили deadlock на ответах клиента при насыщении, преждевременное освобождение ресурсов, ID-reuse cleanup, устаревший resolve, непрерываемые обходы и зависание завершения с открытым stdin. Управляемые барьеры покрывают edit/reopen, cross-file definition и lazy code-action resolve. Полный `cargo test --all`: **2040 PASS, 0 FAIL, 0 ignored**, 69 result blocks; Clippy `-D warnings`, Rustfmt и diff check — PASS. Сборки последовательные (`CARGO_BUILD_JOBS=1`), тесты с `--test-threads=1`; один CPU, MemoryMax 1536 MiB / MemorySwapMax 256 MiB. Схема кеша 39 не менялась. Артефакты: `server/target/dispatcher-2026-10-16/` (ignored).
- **Границы проверки:** Linux; без нового замера большого проекта или запуска VS Code Extension Host. Это управляемые порядки событий, не доказательство всех возможных гонок. Кодек/JSON на границе транспорта и неделимые нативные вызовы не получили принудительного прерывания; лимиты не являются общей границей RSS. Epoch-защита целевого URI не заменяет feature-specific проверки поздних изменений внешних зависимостей.

Тесты: [`dispatch_tests.rs`](../server/crates/php-lsp-server/src/dispatch_tests.rs), [`dispatch_backend_tests.rs`](../server/crates/php-lsp-server/src/dispatch_backend_tests.rs), [`control_tests.rs`](../server/crates/php-lsp-parser/src/control_tests.rs), [`e2e_dispatch.rs`](../server/crates/php-lsp-server/tests/e2e_dispatch.rs).

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
- ~~`let _names = function_name_scope_from_index(...)` — лишняя работа~~ **Поправка:** это RAII-guard, который устанавливает thread-local `FunctionNameScope` на время запроса (`php-lsp-parser/src/function_names.rs`). Удалять нельзя. Стоит переименовать в `_scope_guard` и добавить комментарий, чтобы его не приняли за мёртвый код.
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

## D. Дополнительные находки по участкам (второй проход, 7 субагентов)

Второй проход вели 7 параллельных проверок: у каждой свой участок, без сборки. Каждая могла сделать до 3 прогонов уже собранного бинарника на крошечных фикстурах (`analyze`, `fix --dry-run`, короткие LSP-сеансы по stdio). Ключевые выводы я выборочно перепроверил по коду (stdin, форматирование шаблонов, `HashMap` в composer, `remove_php_file`, `define`, поиск PHPDoc). Нумерация: `D-<участок>-<n>`.

### D-NAV. Навигация, форматирование, внешние процессы

| # | Пр. | Где | Суть и сценарий | Статус | Исправление |
|---|---|---|---|---|---|
| D-NAV-1 | **P0** | `lsp/formatting.rs:349-394`, `:396`, `:470` | Для Blade/Twig в `open_files` лежит парсер **виртуального** PHP; форматтер получает виртуальный текст, а правка `full_document_range` применяется к оригиналу. `<div>{{ $name }}</div>` превращается в `<?php echo  $name ; ?>`. В Laravel срабатывает без настройки (`auto` находит pint). В `formatting.rs` нет ни одного обращения к `template_documents` | воспроизведено | Для шаблонов форматирование отключить или использовать отдельный форматтер; виртуальный текст никогда не форматировать |
| D-NAV-2 | P1 | `lsp/formatting.rs:290-320` | On-type отступ = число открытых `{` без учёта строк, комментариев, heredoc, `(`, `[`. Enter в heredoc меняет **значение литерала** (`"  SELECT 1"` → `"    SELECT 1"`); в докблоке ломается выравнивание `*`; `;` у цепочки сбрасывает отступ продолжения | воспроизведено | Отступ по CST; не трогать строки внутри `comment`/строк/heredoc; либо оставить только триггер `}` |
| D-NAV-3 | P1 | `lsp/rename.rs:51-92` | Rename переменной чисто локальный: `compact('bar')` не обновляется; rename параметра не трогает именованные аргументы `g(limit: 5)` → Fatal «Unknown named parameter» | воспроизведено | Обновлять named args через индекс (или отказывать в rename); переименовывать строки в `compact()`; отказывать при `extract`/`$$` |
| D-NAV-4 | **P1** | `lsp/external_command.rs:66-96` | stdin дочернего процесса не задан → наследуется stdin LSP. Phpstan/composer-скрипт/интерактивный prompt может съесть байты JSON-RPC | по коду, подтверждено | `.stdin(Stdio::null())` |
| D-NAV-5 | P2 | `lsp/definition.rs:38-107`, `lsp/hierarchy.rs:236-362` | Go to Implementation: абстрактные классы отбрасываются; реализации через `use Trait` не ищутся; для трейта не находятся классы-пользователи | воспроизведено | Обходить трейты подтипа; не исключать abstract для методов |
| D-NAV-6 | P2 | `lsp/hierarchy.rs:236`, `:271` | `direct_type_subtypes` перебирает все типы на каждом уровне, `direct_method_by_fqn` — все файлы и символы на каждый подтип. Для `Countable`/`Throwable` это O(подтипы × индекс) в диспетчере | по коду | Обратная карта `parent → subtypes`; `get_direct_members` |
| D-NAV-7 | P2 | `lsp/folding.rs:30-66` | `lineFoldingOnly` игнорируется: свёрнутый `if` прячет строку `} else {`. Нет фолдинга `use`-групп, `switch`/`match`, подряд идущих `//`, `#region` | воспроизведено | `end_line - 1`, если после `}` на строке есть код; добавить виды фолдинга |
| D-NAV-8 | P2 | `folding.rs:121`, `document_symbols.rs:359`, `document_links.rs:304`, `rename.rs:18` | Для Blade/Twig диапазоны считаются по виртуальному тексту; фолды, ссылки и правки попадают на чужие строки | по коду | Маппить через `TemplateDocument` или возвращать `None` |
| D-NAV-9 | P1 | `lsp/code_lens.rs:221-338`, `lsp/call_hierarchy_graph.rs:451+` | `index.read()` держится на весь скан ссылок проекта; запись в индекс (`didChange`) ждёт блокирующий `parking_lot`-лок в диспетчере — сервер стоит до 15 с | по коду | Короткие лизинги, `Arc<[SymbolReference]>` для дешёвых снимков |
| D-NAV-10 | P2 | `code_lens.rs:487`, `call_hierarchy_graph.rs:374/434/451` | CPU-сканы идут через файловый семафор (2 слота, 15 с). Если скан длиннее 15 с, линзы и incoming calls молча возвращают `None`; две линзы блокируют чтение файлов для hover/definition. Incoming calls перепарсивают каждый закрытый файл с методом того же имени | по коду | Отдельный CPU-пул с отменой без файлового таймаута |
| D-NAV-11 | P2 | `code_lens.rs:63-74`, `:497` | Любое изменение ревизии индекса (набор текста, индексация) прерывает расчёт → `None` → линзы мигают; `workspace/codeLens/refresh` не отправляется | по коду | Отдавать последний результат, пересчитывать в фоне + refresh |
| D-NAV-12 | P2 | `lsp/semantic_tokens.rs:259-268`, `php-lsp-parser/src/semantic_tokens.rs:204-210` | Range-запрос считает весь файл дважды; многострочные токены вызывают `split('\n').nth(line)` на каждую строку → O(n²); при CRLF в длину попадает `\r` | по коду | Убрать лишний вызов; длины строк из `LineIndex` |
| D-NAV-13 | P3 | `lsp/document_links.rs:204`, `:167-179` | Синхронный `is_file()` под guard'ом `open_files` в async-обработчике; второй аргумент `dirname(__DIR__, 2)` игнорируется | по коду | Проверку существования в `spawn_blocking`; поддержать `levels` |
| D-NAV-14 | P3 | `util/fs_walk.rs:383-392` | `canonicalize` на каждый файл (lstat на каждый компонент) — сотни тысяч лишних syscalls на vendor | по коду | Канонизировать только пути под симлинками |

Также: `call_hierarchy_graph.rs:84-96` (`Resolver::method`) ставит `@mixin` перед `extends` — та же ошибка, что в п. 13, но в отдельной реализации.

### D-ACT. Code actions, генерация кода, CLI `fix`/`analyze`

| # | Пр. | Где | Суть и сценарий | Статус | Исправление |
|---|---|---|---|---|---|
| D-ACT-1 | **P1** | `lsp/code_action.rs:336` (`find_use_insert_line`), `:5912` | В файле без namespace и use «Import X» вставляет `use` в строку 1, то есть **перед** `declare(strict_types=1)` → Fatal «strict_types declaration must be the very first statement». В completion есть защита (`safe_import_location`), в code action нет | воспроизведено | Точку вставки искать по AST, переиспользовать `safe_import_location` |
| D-ACT-2 | **P1** | там же | Если первая строка не ровно `<?php` (`<?php declare(...);`, shebang, `<?php // x`), `use` вставляется в 0:0 — **перед** `<?php` | воспроизведено | Вставлять после `php_tag`/`declare` по AST |
| D-ACT-3 | **P1** | `php-lsp-parser/src/return_type.rs:93-105` (`find_doc_comment_node`) | Цикл `prev_sibling` пропускает узлы, которые не являются комментариями (в том числе предыдущие функции), и берёт `@return` **соседней** функции. `fix --rule add-return-type` предлагает `b(): int` для `return 'x'` → TypeError. Затрагивает fix, code action и inlay hints. Нашли независимо два участка | воспроизведено, подтверждено по коду | Брать только непосредственно предыдущий комментарий; на именованном узле останавливаться (как в `resolve.rs:4099`) |
| D-ACT-4 | **P1** | `return_type.rs:60`, `fix.rs:606`, `code_action.rs:953` | Тип добавляется в методы интерфейсов и абстрактные методы и в методы, у которых есть переопределения без типа → Fatal «must be compatible» во всех реализациях | кандидат воспроизведён | Пропускать interface/abstract/переопределяемые; проверять контракт родителя |
| D-ACT-5 | P2 | `code_action.rs:2377`, `:2460`, `:2541` | «Generate constructor» и property hooks PHP 8.4: `=>` из хука принимается за значение по умолчанию → `string $label = > strtoupper(...)`; присваивание в get-only (виртуальное) свойство → Error | воспроизведено | Значение по умолчанию из AST (`property_initializer`); виртуальные свойства пропускать |
| D-ACT-6 | P2 | `code_action.rs:2386` | `declaration.find("$a")` находит `$ab`: `public int $ab = 1, $a = 2;` → у `$a` подставляется `= 1` | воспроизведено | Разбор по AST |
| D-ACT-7 | P3 | `code_action.rs:726`, `:2046`, `:5912` | В CRLF-файлах генерируемый текст всегда с `\n` → смешанные переводы строк | воспроизведено | Определять EOL документа один раз (как уже сделано в `analyzer_ignore_edit`) |
| D-ACT-8 | P2 | `code_action.rs:7069-7107` | Кандидаты на импорт: полный перебор типов, алфавит, `truncate(5)` — нужный может не попасть; для `new Sub\Bar()` импортируется любой `Bar`, а `Sub\Bar` заменяется на `Bar` | по коду | Ранжировать по близости неймспейса; для квалифицированного имени импортировать первый сегмент |
| D-ACT-9 | P3 | `code_action.rs:2019` | Сгенерированные члены всегда с отступом в 4 пробела; в файлах с табами отступы смешиваются | по коду | Единицу отступа брать из существующих членов |
| D-ACT-10 | P3 | `code_action.rs:~2735` | Сеттер всегда `: void`, без учёта `phpVersion` (< 7.1) | по коду | Учитывать версию |
| D-ACT-11 | P3 | `code_action.rs:2814`, `:1947`; `code_action_source.rs:12`, `:146` | Полный `Utf16LineIndex` и `line_start_offsets` на каждую правку; `lines().nth()`; повторный парсинг файла, хотя дерево уже есть | по коду | Передавать готовые дерево и индекс строк |
| D-ACT-12 | P3 | `fix.rs:438`, `:905`; `code_action.rs:6115` | Один нечитаемый файл прерывает весь `fix`; не-UTF-8 файлы читаются через `from_utf8_lossy` (опасно, если появится запись на диск); «Remove all unused imports» на деле делает organize imports (пересортировка) | по коду | Пропускать с предупреждением; не-UTF-8 пропускать; разделить действия |
| D-ACT-13 | P3 | `code_action.rs:4081` | Extract variable вставляет `$v = expr; ` в ту же строку перед инструкцией | по коду | Отдельная строка с отступом |

### D-INF. Вывод типов и переменные (`php-lsp-parser`)

| # | Пр. | Где | Суть и сценарий | Статус | Исправление |
|---|---|---|---|---|---|
| D-INF-2 | **P1** | `resolve.rs:2570` (`positive_instanceof_branch_inference`), вызов из `:2230` | Внутри `if ($x instanceof A)` тип всегда `A`, переприсваивание в теле не учитывается: `$x = new B(); $x->bOnly();` → ложное `Unknown method: A::bOnly`; неверные hover и completion | воспроизведено | Сканировать тело до точки использования, сужение — только стартовое значение |
| D-INF-3 | **P1** | `cst.rs:231`, `variable_flow.rs:426-436`, `function_names.rs:72-84` | By-ref out-параметры распознаются только по захардкоженному списку (`preg_match*`). `exec('ls', $lines)`, `str_replace(..., $count)`, `fsockopen`, `similar_text`, пользовательские `function f(&$r)` и методы → ложное «Undefined variable» | воспроизведено | Брать `ParamInfo.is_by_ref` из сигнатуры (стабы и индекс уже это знают) |
| D-INF-4 | **P1** | `variable_flow.rs:456-461` | `parse_str` в одном списке с `extract`/`eval`: ложное `Undefined variable: $parsed` **и** `state.unknown = true` до конца функции — дальнейшие undefined-проверки молча выключаются | воспроизведено | Убрать из Unknown-списка, второй аргумент — by-ref цель |
| D-INF-5 | **P1** | `semantic.rs:1730-1762` | «Unused parameter» на by-ref параметре, в который пишут (`function out(?array &$r) { $r = [1]; }`, `$out[] = 1`). Нашли два участка | воспроизведено | By-ref параметр с записью считать использованным |
| D-INF-6 | **P1** | `resolve.rs:7270` (`find_parent_class_fqn`) | `$this` в трейте резолвится в сам трейт: `trait T { function run() { $this->hostMethod(); } }` → ложное `Unknown method: T::hostMethod` (типичный паттерн) | воспроизведено | Искать в абстрактных методах трейта и в классах-пользователях; если не нашли — не ругаться |
| D-INF-7 | P2 | `function_names.rs:26-58`, `:127`; `resolve_foreach.rs:74` | Без активного `FunctionNameScope` каждый вызов делает `extract_root_symbols` — полное извлечение символов файла на каждый аргумент или lookup переменной → O(аргументы × файл) | по коду | Строить факты лениво или гарантировать один scope на запрос (assert в debug) |

### D-SYM. Символы, PHPDoc, ссылки, семантика (`php-lsp-parser`, `php-lsp-types`)

| # | Пр. | Где | Суть и сценарий | Статус | Исправление |
|---|---|---|---|---|---|
| D-SYM-1 | **P1** | грамматика `tree-sitter-php 0.23.11` (в `enum_declaration_list` нет `const_declaration`) | `enum Suit: string { case S='s'; const Wild = self::S; }` → «Missing }»/«Syntax error» до конца файла, а из-за правила «синтаксическая ошибка скрывает семантику» файл теряет все диагностики. (Уточнение к разделу «Зависимости»: PHP 8.4 разбирается, а const в enum — нет) | воспроизведено | Обновить или пропатчить грамматику; регрессионный тест |
| D-SYM-2 | **P1** | `symbols.rs:1494` (`extract_global_constants`) | `define('X', ...)` не извлекается вообще (`"define"` в парсере не встречается). В стабах так объявлено ≈500 констант (`E_ALL`, `PHP_EOL`, `JSON_THROW_ON_ERROR`, `SORT_*`) — нет hover, definition, completion | подтверждено по коду | Распознавать `define` со строковым литералом → `GlobalConstant` |
| D-SYM-3 | P2 | `symbols.rs:307` | Функции, классы и константы, объявленные внутри тела функции (в том числе `if (!function_exists('x')) { function x(){} }` в bootstrap-функциях), не индексируются → «Unknown function» | воспроизведено | Обходить тело (кроме замыканий и классов) |
| D-SYM-4 | P2 | `semantic.rs:1928`, `:1661` | «Unused variable» на записи в `global $x`, `static $cache` и ссылку `$ref = &$arr['k']; $ref = 5;` — у всех трёх есть внешний эффект | воспроизведено | Не выдавать unused для связываний через global/static/`=&`/foreach по ссылке |
| D-SYM-6 | P2 | `phpdoc.rs:818` (`parse_type_string`) | Рекурсия без ограничения глубины и O(n²) по длине типа: глубоко вложенный тип в docblock (сгенерированный или вредоносный файл в vendor) → переполнение стека = **abort процесса** | по коду | Счётчик глубины (например, 64 → `Mixed`), лимит длины строки типа |
| D-SYM-7 | P3 | `php-lsp-parser/src/diagnostics.rs:37` | Синтаксические ошибки собираются рекурсивно внутри `ERROR` (дубли), без лимита — на повреждённых файлах тысячи диагностик | по коду | Не спускаться в `is_error()`, лимит ~100, итеративный обход |
| D-SYM-8 | P3 | `utf16.rs:84`, `:114`, `:125` | Разовые конвертеры делают `split('\n').nth(line)` и вызываются в циклах (`rename.rs:76`, `references.rs:562`, `document_symbols.rs:336`, `definition.rs:666`) → O(ссылки × файл) | по коду | В циклах использовать `Utf16LineIndex` |
| D-SYM-9 | P3 | `semantic.rs:871` | Импорт, используемый только в `@see`/`{@link}`/`@uses`/`@covers`, помечается неиспользуемым; organize imports ломает PHPDoc-ссылку | воспроизведено | Разбирать первый токен этих тегов как имя класса |
| D-SYM-10 | P3 | `semantic.rs:1764` | Неиспользуемая переменная в `use (...)` замыкания не диагностируется (пропуск) | воспроизведено | Hint «Unused closure variable» |

### D-CMP. Completion, hover, inlay hints, signature help

| # | Пр. | Где | Суть и сценарий | Статус | Исправление |
|---|---|---|---|---|---|
| D-CMP-1 | P2 | `php-lsp-parser/src/signature_help.rs:140`, `completion_helpers.rs:2811` | Активный параметр считается по запятым: `f(b: 1, a: |)` подсвечивает `$b`; spread не учитывается; при переполнении без variadic подсвечивается последний | воспроизведено | По имени для named args; после `...` — None; `LabelOffsets` |
| D-CMP-2 | P2 | `inlay_hints.rs:551` | `f(...$args)` → подсказка `a:` перед spread; следующие позиционные подсказки смещаются | воспроизведено | Пропускать spread и прекращать позиционное сопоставление |
| D-CMP-3 | P3 | `inlay_hints.rs:551` + `signature_param_for_arg` | Variadic подписывается у каждого аргумента (`rest:`, `rest:`); шум `a: $a` при совпадении имени | воспроизведено | Только первый variadic; пропускать совпадающие имена |
| D-CMP-4 | P2 | `inlay_hints.rs:479-515` | Callable разрешается для **всех** вызовов файла, а попадание в `requested_range` проверяется только потом; для шаблонов диапазон = весь документ | по коду | Отсекать поддеревья вне диапазона до разрешения |
| D-CMP-5 | P2 | `inlay_hints.rs:63`, `server.rs:1013` | CPU-расчёт inlay hints занимает один из 2 слотов **файлового** семафора; два запроса блокируют весь файловый ввод-вывод запросов; при 15 с — молча пустой список. `hydrate_inlay_hint_label_locations` последовательно ждёт `await` на каждой метке | по коду | Отдельный CPU-пул; локации — в `inlayHint/resolve` |
| D-CMP-6 | P3 | `completion.rs:~615`, `provider.rs:882` | После `completionItem/resolve` detail свойства `string` превращается в `(): string`; печатается `...&$x` вместо `&...$x`; detail до и после resolve разный | воспроизведено | Одна функция форматирования сигнатур |
| D-CMP-7 | P2 | `php-lsp-completion/src/visibility.rs:261` vs `diagnostics.rs:2817` | Две реализации видимости: completion (правильная, двусторонняя) предлагает `hook()`, а диагностика после вставки ругается | по коду | Общий модуль видимости для всех функций |
| D-CMP-8 | P2 | `provider.rs:910`, `:933`, `:948` | Для каждого виртуального `@property`/`@method` весь docblock класса разбирается 2–3 раза → O(N²): ~100 `@property` дают ~300 разборов на одно `->` | по коду | Хранить вид доступа и тип виртуального члена в `SymbolInfo` |
| D-CMP-9 | P3 | `provider.rs:~860`, `:707`, `:810` | `filterText = "{name} {fqn}"` и для членов: `$user->us` совпадает со всеми членами `App\User` | по коду | Для членов фильтровать только по имени |
| D-CMP-10 | P3 | `php-lsp-completion/src/context.rs:377`, `:206`, `:346`, `:282` | Completion переменных внутри строк в одинарных кавычках и nowdoc (`'cost $|`) — контекст определяется по тексту строки, а не по CST | воспроизведено | Для `string`/`nowdoc`/`comment` → `None` (кроме интерполируемых строк) |
| D-CMP-11 | P3 | `completion.rs:~410` | Для composite receivers список полностью заменяется: теряются framework virtual members и object shape (`Model|Builder`) | по коду | Сливать списки |
| D-CMP-12 | P3 | `completion.rs:207-210` | `framework_string_key_items` и остальной вывод типов считаются, даже когда результат будет отброшен | по коду | Ранний выход |

### D-IDX. Индексация, vendor, кэш, конфигурация

| # | Пр. | Где | Суть и сценарий | Статус | Исправление |
|---|---|---|---|---|---|
| D-IDX-1 | **P1** | `indexing/workspace.rs:2128-2138`, `:1970-1973`, `indexing/vendor.rs:1745-1780` | Autoload-`files` (`helpers.php`, полифиллы) попадают в LRU на 512 файлов и вытесняются первыми; ленивая догрузка ищет только классы. После нескольких сотен hover в Laravel `collect()`, `app()`, `dd()` превращаются в «Unknown function» | по коду | Не трекать entrypoint-файлы в LRU (закрепить) |
| D-IDX-2 | **P1** | `indexing/workspace.rs:2252-2330`, `server.rs` `invalidate_composer_metadata`, `server.rs:3079+` | Перестройка агрегата держит `aggregate_rebuild` на всех повторах; любая запись меняет ревизию → повтор с `yield_now` (каждый раз полная копия + O(N²) из п. 5). Если пользователь печатает во время переиндексации — livelock, `didChange` занимают все 4 слота диспетчера, сервер перестаёт отвечать | по коду | Инкрементальная перестройка, лимит повторов с backoff, не брать мьютекс в `didChange` (или убрать агрегат, п. 11) |
| D-IDX-3 | **P1** | `php-lsp-index/src/composer.rs:99-116`, `:151-161`; `server.rs:3758` | `NamespaceMap` строится из `HashMap` (случайный порядок), а сравнивается как `Vec`. При ≥2 префиксах (Laravel) любой `didChangeConfiguration` или сохранение `.php-lsp.toml` даёт ложное «изменение» → перезагрузка стабов и **полная переиндексация** | подтверждено по коду | `BTreeMap`/`IndexMap` или сравнение отсортированного |
| D-IDX-4 | **P1** | `indexing/workspace.rs:654` → `remove_php_file` (`:3466+`), `lsp_did_delete_files` | Событие watcher'а `DELETED` удаляет запись из `open_files`/`document_versions`/`template_documents` открытого документа; следующий `didChange` игнорируется. `git checkout`/`git stash` для открытого файла — вкладка «мёртвая» до переоткрытия | подтверждено по коду | Для открытых документов убирать только дисковый снимок |
| D-IDX-5 | P2 | `server.rs:259`, `indexing/workspace.rs:908-941`, `:2703-2709` | Обход файлов жёстко ограничен ~15 с; при превышении индекс не строится вообще (WSL `/mnt/c`, NFS, Docker на macOS) | по коду | Только отмена или настраиваемый дедлайн; частичный индекс с пометкой truncated |
| D-IDX-6 | P2 | `indexing/workspace.rs:2679`, `:3127` | Work-done progress завершается только на успешном пути; около 10 ранних выходов → вечный спиннер «Indexing PHP workspace» | по коду | RAII-обёртка, отправляющая `end` в `Drop` |
| D-IDX-7 | P2 | `indexing/workspace.rs:2997-3005`, `:2802-2812`; `php-lsp-index/src/cache.rs:622-630` | После разбора каждый файл перечитывается и хэшируется последовательно в `join_next` через семафор на 2 слота — фактически однопоточно. На тёплом старте весь проект читается дважды | по коду | Валидация внутри задачи разбора; быстрый путь `(size, mtime)` |
| D-IDX-8 | P2 | `indexing/workspace.rs:2780`, `:2856-2875`, `:3020-3027`, `:3132` | Пик памяти при индексации — до 3 копий (`disk_index` + live + агрегат + `IndexCache` перед сериализацией) | по коду | Разделять `Arc<FileSymbols>`, сериализовать потоково |
| D-IDX-9 | P2 | `php-lsp-index/src/cache.rs:483-485`, `indexing/cache.rs` | Валидность кэша включает mtime (после `git checkout`/копирования всё перепарсивается) и не зависит от сборки (изменения логики извлечения без ручного bump схемы оставляют устаревшие символы) | по коду | Валидность по `(size, hash)`; build-id в config hash |
| D-IDX-10 | P2 | `vendor.rs:382,1598`, `workspace.rs:2046,2077`, `diagnostics.rs:1207`, `composer.rs` | `vendor` захардкожен: игнорируются `config.vendor-dir`, `COMPOSER_VENDOR_DIR`, `exclude-from-classmap`, сгенерированные `autoload_psr4.php`/`autoload_classmap.php` | по коду | Читать `vendor-dir`, учитывать exclude, использовать сгенерированные карты |
| D-IDX-11 | P2 | `indexing/workspace.rs:1076-1083`, `client/package.json:253-256,294-304` | Граница доверия обходится через `.vscode/settings.json` репозитория: `phpLsp.allowProjectCommands`, `phpstan.command`, `formatting.command` имеют `scope: "resource"`, `restrictedConfigurations` не объявлены. Дополняет п. 9 | по коду | `scope: machine`/`restrictedConfigurations`; сервер не доверяет workspace-уровню |
| D-IDX-12 | P3 | `php-lsp-index/src/stubs.rs:131-163` | По умолчанию грузятся все ~100+ расширений стабов (`swoole`, `redis`, …) в каждый индекс — десятки МБ, «Unknown class» не срабатывает для неустановленных расширений | по коду | Базовый набор + `ext-*` из `composer.json` |
| D-IDX-13 | P3 | `indexing/workspace.rs:794-815` | При наличии `composer.json` индексируются только autoload-каталоги; функции из `bootstrap/*.php`, `routes/`, `config/`, подключённые через `require`, — «Unknown function» | по коду | Индексировать корень как источник низкого приоритета |
| D-IDX-14 | P3 | `php-lsp-index/src/cache.rs:723-736` | Кэш `~/.cache/php-lsp/<hash>` никогда не чистится (клоны, worktree, временные проекты) | по коду | Чистка по возрасту и для несуществующих корней; `php-lsp cache clean` |
| D-IDX-15 | P3 | `logging.rs:101-103` | `logLevel: debug` включает debug для **всех** крейтов (tower-lsp-server логирует каждое JSON-RPC сообщение целиком, включая тексты документов) | по коду | Фильтр по таргетам `php_lsp*`, остальные — `warn` |

Проверено и снято: симлинки в `vendor/` дублей не создают (прогон `analyze`).

### D-TPL. Шаблоны Blade/Twig и фреймворки

| # | Пр. | Где | Суть и сценарий | Статус | Исправление |
|---|---|---|---|---|---|
| D-TPL-1 | **P1** | `template.rs:1038-1050`, `:1080` | Модификатор пробелов Twig `~` (`{%~ if x ~%}`) не срезается → «Missing Twig tag name», закрывающие теги не распознаются | воспроизведено | Срезать `-` и `~`, в том числе в комментариях |
| D-TPL-2 | **P1** | `template.rs:979` | Короткая форма `{% block title "Hello" %}` → «Unclosed Twig block» | воспроизведено | Как у `set`: с выражением блок не открывается |
| D-TPL-3 | P2 | `template.rs:940-960`, `:1007-1028` | Захардкоженный список блочных тегов: `{% else %}` внутри `guard` (Twig 3.15) и тегов расширений → «Unexpected else» | воспроизведено | Добавить `guard`; неизвестные пары не ругать; пары в конфигурацию |
| D-TPL-4 | **P1** | `template.rs:2692-2716`, `:2800`, `:345-385` | Каждая конвертация позиции заново строит `line_start_offsets` и `Utf16LineIndex`; сегменты ищутся линейно → semantic tokens шаблона O(T·n), ~10⁹ операций на большом шаблоне | по коду | Готовые `LineIndex` в `TemplateDocument`, бинарный поиск сегментов |
| D-TPL-5 | **P1** | `framework.rs:1125-1186`, вызовы `:742`, `:824` | На каждый неразрешённый метод `Macroable`-класса (`Collection`, `Builder`, `Str`, …) синхронно читаются с диска **все** не-vendor файлы проекта (в async-потоке, без кэша между запросами, без учёта несохранённых буферов) | по коду | Собирать `X::macro(...)` при индексации |
| D-TPL-6 | P2 | `template.rs:540-630`, `:2487` | `@verbatim…@endverbatim` (Vue/Alpine) анализируется как PHP; `@php…@endphp` наоборот не анализируется | по коду | Пропускать verbatim; маппить `@php` как PHP |
| D-TPL-7 | P2 | `template.rs:575-607` | Конец `{{ }}`/`{!! !!}` ищется через `find` без учёта кавычек: `{{ $a ? '}}' : 'b' }}` режется посередине | по коду | Сканер с учётом строк, как в Twig |
| D-TPL-8 | P2 | `twig_context.rs:780-830`, `templates.rs:60-75`, `:95-120` | Каталоги захардкожены (`src`/`app`/`tests`, `templates`/`resources/views`); не учитываются `twig.yaml`, PSR-4, Drupal/Craft. Шаблоны вне этих каталогов именуются по basename → контексты одноимённых шаблонов склеиваются | по коду | Корни из конфигурации/twig.yaml/PSR-4; без совпадения не выводить имя |
| D-TPL-9 | P3 | `templates.rs:277-282` | Рендер определяется только по имени метода (`->render(...)` у PDF или mailer тоже считается Twig) | по коду | Проверять тип получателя |
| D-TPL-10 | P3 | `template.rs:149-154` | Для Blade нет синтаксической диагностики (несбалансированные `@if`/`@endif` молча отфильтровываются) | по коду | Стек директив |
| D-TPL-11 | P3 | `framework.rs:1093-1095`, `:1112-1114` | Тип результата Laravel-макроса всегда = владелец (предполагается fluent) | по коду | Тип возврата замыкания или `mixed` |
| D-TPL-12 | P3 | `twig_context_merge.rs:62-64` | Строится `?(A&B)` — недопустимый тип, при повторном разборе он падает в `mixed` | по коду | `(A&B)\|null` |
| D-TPL-13 | P3 | `template.rs:269-280`, `:65-69` | Каждое изменение пересобирает и перепарсивает весь шаблон; `has_same_source_and_twig_context` сравнивает исходники целиком в каждой проверке актуальности | по коду | Одна сборка на пакет изменений; сравнение по поколению |
| D-TPL-14 | P3 | `template.rs:525-526` | `TOKEN_KEYWORD = 11`, `TOKEN_COMMENT = 13` захардкожены отдельно от легенды `SEMANTIC_TOKEN_TYPES` | по коду | Вычислять индекс из легенды |

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
- Зависимости: `tree-sitter 0.24` / `tree-sitter-php 0.23` (синтаксис PHP 8.4 — property hooks, асимметричная видимость, `new X()->m()` — разбирается без ошибок, проверено; но `const` внутри `enum` грамматика 0.23 не поддерживает — см. D-SYM-1, обновление грамматики нужно). `bincode 1` в режиме сопровождения — при следующем изменении схемы кэша можно перейти на `bincode 2`/`postcard`.

## Что сделано хорошо (сохранить)

- Тщательная защита от гонок: поколения документов, токены перезагрузки закрытых файлов, лизинги индексации, проверка актуальности перед публикацией диагностики, атомарная запись кэша (tmp + `sync_all` + rename) с версионированием схемы.
- Доверие к проектным командам: whitelist нормализации TOML не даёт обойти `allowProjectCommands` плоскими ключами.
- Бюджеты вывода типов и диагностики защищают от патологических файлов.
- Проверки членов, переменных и override-вариантности на типичном коде дают точные результаты: на фикстурах с `extract`/`compact`/`global`/`static`/деструктуризацией/замыканиями по ссылке/`$$var`/`?->`/enum/readonly/first-class callable ложных срабатываний нет, настоящие ошибки находятся.
- Clippy чистый на закоммиченном коде.

## Предлагаемый порядок работ

1. **Порча данных:**
   - P0-1 (ropey + единый `LineIndex` + тесты);
   - D-NAV-1 (не форматировать шаблоны);
   - D-NAV-4 (`stdin(Stdio::null())`);
   - D-NAV-2 (on-type форматирование в heredoc и комментариях);
   - D-ACT-1/2/3/4 (вставка `use`, add-return-type);
   - D-IDX-4 (не удалять открытые документы по событию watcher'а).

   Всё это небольшие правки, которые устраняют порчу пользовательского кода.
2. **Точечные исправления:**
   - P1-7 (`isIncomplete`);
   - P1-8 и D-INF-2..6 (ложные диагностики: `__call`, protected, `Parent::`, `strict_types`, by-ref, `parse_str`, `$this` в трейте, instanceof);
   - D-SYM-1/2 (const в enum, `define`);
   - D-IDX-1/3 (LRU `files`, порядок PSR-4);
   - D-TPL-1/2;
   - P1-9 и D-IDX-11 (доверие к командам).
3. **P1-3 + P0-2**: снимок документа на `Arc`, упорядоченная очередь синхронизации, вынос тяжёлых обработчиков в `spawn_blocking` с отменой.
4. **P1-4/5/6**: карта `key → [uri]`, массовое удаление, `Arc<SymbolInfo>`, предразобранные псевдонимы типов, кэш материализации.
5. **P2-11**: федеративный агрегат и общий слой стабов (заметная экономия памяти).
6. Объединение интеграционных тестов в один бинарник и профили сборки — можно сделать в любой момент, сразу снизит нагрузку на память при `cargo test`.
