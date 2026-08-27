# DEEPSEEK AUDIT — PHP Language Server

**Дата:** 2026-08-19
**Область:** весь репозиторий (`server/` — 5 Rust-крейтов, `client/` — VS Code extension)
**Метод:** статический анализ исходников по крейтам/подсистемам, сверка с тестами и конвенциями из `AGENTS.md`.

---

## Сводка

| Severity | Кол-во | Комментарий |
|----------|--------|-------------|
| Critical | 1 | Утечка дочерних процессов при таймауте/отмене внешних команд |
| Major | 9 | Коррупция UTF-8, PSR-4 longest-prefix, O(n²) сбор файлов, блокирующий I/O в async |
| Minor | ~60 | Логика, производительность, надёжность, консистентность API |

Ниже — все находки, сгруппированные по подсистемам, с точными ссылками `file:line`.

> **Комментарий Codex (проверено 2026-08-19):** сводные количества нельзя принимать как число подтверждённых дефектов: ниже есть подтверждённые находки, частично верные замечания, намеренные компромиссы и опровергнутые пункты. Приоритет следует строить по моим комментариям под каждым пунктом, а не только по исходной severity.

---

## 1. Critical

### 1.1 Утечка дочерних процессов при таймауте/отмене внешних команд
`server/crates/php-lsp-server/src/lsp/external_command.rs:47-64`

`run_shell_command_with_timeout` запускает команду через `sh -c "<command>"` (или `cmd /C` на Windows) и полагается на `kill_on_drop(true)`. При срабатывании таймаута или отмены `wait` (владеющий `Child`) дропается, что убивает **только оболочку `sh`**, но не реальный процесс анализатора/форматтера (phpstan, psalm, pint и т.п.), который она породила. Процесс-внук осиротевает и продолжает потреблять CPU после того, как сервер уже сообщил «timed out»/«cancelled». Группа процессов не создаётся, внуки не забираются.

**Рекомендация:** запускать команду в собственной process group (`setsid`/`CREATE_NEW_PROCESS_GROUP`) и убивать всю группу по таймауту/отмене.

> **Комментарий Codex:** подтверждаю. `kill_on_drop(true)` управляет только запущенным `sh`/`cmd`, а отдельной process group/job object нет, поэтому потомки shell действительно могут пережить timeout или cancellation. Исправление должно быть кроссплатформенным и сопровождаться тестом с PID фонового дочернего процесса; `Critical` допустим для доверенных внешних команд, хотя по типу это прежде всего high resource-leak.

---

## 2. Major

### 2.1 Коррупция не-ASCII UTF-8 в `next_quoted_string`
`server/crates/php-lsp-server/src/framework.rs:3142,3145`

```rust
let ch = bytes[cursor] as char;          // 3142
value.push(bytes[cursor + 1] as char);   // 3145
```

Итерация по сырым байтам с приведением каждого байта к `char` разбивает любой многобайтовый UTF-8 символ (é, кириллица, CJK, emoji) на 2–4 мусорных символа. Функция питает `parse_array_string_values`, `parse_array_string_pairs`, `parse_named_call_string_args`, `parse_php_array_key_paths_in`, `parse_named_string_argument` — то есть ключи конфигов Laravel, ключи переводов, имена роутов и значения `$casts`. Любое не-ASCII строковое значение молча портится.

**Рекомендация:** итерировать `char_indices()` вместо байтов.

> **Комментарий Codex:** подтверждаю как дефект корректности. Одного механического перехода на `char_indices()` мало: нужно сохранить точные byte-границы и текущую обработку escape-последовательностей. Нужны Unicode-тесты для каждого потребителя строки — config/translation/route/casts, включая экранированные кавычки рядом с многобайтовым символом.

### 2.2 PSR-4: возвращаются пути для всех совпавших префиксов вместо longest-prefix-wins
`server/crates/php-lsp-index/src/composer.rs:28-50`

`resolve_class_to_paths` перебирает все записи `psr4` и добавляет путь для каждого префикса, являющегося строковым префиксом FQN. Семантика Composer — **побеждает самый длинный совпавший префикс**. При пересекающихся префиксах (типично: `"App\\" => "app/"` + `"App\\Tests\\" => "tests/"`) резолв `App\Tests\Foo` вернёт и `app/Tests/Foo.php` (неверно), и `tests/Foo.php` (верно). Анализатор (`analyze.rs:824`) пробует неверный путь первым и может выбрать не тот файл, если оба существуют.

**Рекомендация:** выбирать единственный longest-matching префикс.

> **Комментарий Codex:** подтверждаю. Вектор `psr4` формируется из `HashMap`, поэтому порядок совпавших префиксов не является надёжной частью поведения, а вызывающий действительно может принять первый существующий неверный путь. Выбирать надо самый длинный префикс, сохраняя все директории именно для него; PSR-0 следует обработать по его правилам отдельно.

### 2.3 O(n²) сбор файлов через `push_unique_path`
`server/crates/php-lsp-server/src/indexing/workspace.rs:887-891` (используется в `:824`, `:871`)

`push_unique_path` делает линейный `paths.iter().any(...)` на каждую вставку. `collect_php_files_recursive` вызывает его для каждого найденного PHP-файла, делая обнаружение файлов O(n²). Для проекта в 10k файлов это ~100M сравнений `PathBuf` при каждой индексации.

**Рекомендация:** аккумулятор на `HashSet`/`BTreeSet` → O(n log n).

> **Комментарий Codex:** подтверждаю проблему масштаба: `Vec::iter().any()` вызывается для каждого найденного файла. Практичный вариант — `HashSet<PathBuf>` для membership плюс `Vec<PathBuf>` для результата и финальная сортировка для детерминизма; `BTreeSet` тоже подходит, если допустима цена сравнений путей.

### 2.4 Блокирующий парсинг + извлечение символов inline в async-обработчиках
`server/crates/php-lsp-server/src/lsp/diagnostics.rs:519-588` (`lsp_did_open`), `:651-836` (`lsp_did_change`)

`parser.parse_full(text)`, `extract_file_symbols`, `collect_symbol_references_in_file` выполняются синхронно внутри async-обработчика (без `spawn_blocking`). Тот же паттерн в `references.rs:356` (`lsp_document_highlight`), `references.rs:452` (`lsp_references`), `rename.rs:28` (`lsp_rename`), `rename.rs:241` (`lsp_prepare_rename`), `semantic_tokens.rs:22-37`. Нарушает правило репозитория «no unbounded blocking work in async handlers» и стопорит Tokio runtime на больших файлах.

> **Комментарий Codex:** подтверждаю, но исправление нельзя свести к оборачиванию тела при удерживаемом `DashMap` guard. Нужно снять owned snapshot исходника/дерева и версии документа, выполнить CPU-работу в `spawn_blocking`, затем публиковать результат только при совпавшей generation/version. Это major latency-риск для больших файлов.

### 2.5 Синхронный полный скан воркспейса в async-обработчиках
`server/crates/php-lsp-server/src/lsp/references.rs:272-332` (`reference_scan_matches`)

Синхронная итерация по всему индексу (с повторным сбором ссылок для открытых файлов) вызывается напрямую из `lsp_references` (`:543`), `lsp_rename` (`:174`) и `lsp_code_lens` (через `reference_locations_for_symbol`). Только цикл результата делает `cooperative_heavy_request_yield`; сам скан блокирует runtime.

> **Комментарий Codex:** подтверждаю. Yield после полного скана не делает сам скан cooperative. Нужен один snapshot заранее вычисленных `file_references`, отдельный расчёт ссылок открытых документов вне runtime worker и периодическая проверка cancellation; для rename нельзя ослаблять требования к exact/type-resolved ссылкам.

### 2.6 Квадратичный скан ссылок в code lens
`server/crates/php-lsp-server/src/lsp/references.rs:600-637` (`lsp_code_lens`)

Для каждого class/method-символа в файле `reference_locations_for_symbol` → `reference_scan_matches` (`:272-332`) итерирует **все** индексированные файлы и заново собирает ссылки. O(symbols × files) с повторным `collect_symbol_references_in_file` на символ. На большом воркспейсе code lens крайне медленный.

> **Комментарий Codex:** подтверждаю. Code lens должен одним проходом построить счётчики сразу для всех целевых символов документа, используя индексированные ссылки и по одному snapshot каждого открытого файла. Это отдельная квадратичная проблема поверх пункта 2.5.

### 2.7 Полное чтение каждого файла при каждой загрузке/сборке кэша
`server/crates/php-lsp-index/src/cache.rs:483-491` (`file_metadata`)

`file_metadata` вызывает `fs::read(path)` для вычисления content-hash. `load_valid_cached_sources` (`:365`) вызывает его для каждого текущего файла при старте, а `build_cache_from_sources` (`:428`) — снова для каждого файла после того, как индекс уже построен парсингом тех же файлов. Для больших воркспейсов это O(total bytes) избыточного I/O на каждой загрузке/сохранении, что во многом сводит на нет смысл кэша (он избегает парсинга, но не чтения).

> **Комментарий Codex:** частично подтверждаю. Полное чтение на validation — намеренный backstop против неизменившихся mtime/size и его нельзя просто убрать; кэш всё равно экономит parsing и extraction, поэтому фраза «сводит на нет» завышена. Реальная оптимизация — передавать уже вычисленный hash исходника при построении кэша и не читать файл второй раз после индексации.

### 2.8 Блокирующий файловый I/O в async-обработчиках (framework)
`server/crates/php-lsp-server/src/framework.rs:1026-1072` (`laravel_macro_source`), `:3978-4000` (`hash_relevant_files`), `:3374,3404,3447,3538,3581` (`collect_*_keys`)

Синхронные `std::fs::read_to_string`/`std::fs::metadata` по всем индексированным файлам (кроме `vendor`) на async-пути completion/hover. Нарушает правило «no unbounded blocking filesystem work in async handlers».

> **Комментарий Codex:** частично подтверждаю. Сканирование framework string keys уже вызывается через `run_file_io_blocking`, и Twig disk scans также вынесены; эта часть отчёта устарела. Однако `laravel_macro_source` действительно читает индексированные файлы синхронно при разрешении virtual members, а `FrameworkProviderContext::fingerprint` синхронно вызывает `metadata` для relevant files — их нужно вынести или кэшировать отдельно.

### 2.9 Неограниченная рекурсия в `collect_classmap_php_files`
`server/crates/php-lsp-server/src/indexing/vendor.rs:581-607`

Рекурсивный обход classmap-директорий без ограничения глубины и без защиты от циклов по симлинкам. Classmap-запись, указывающая на директорию с симлинк-петлёй (или самоссылающуюся), даёт бесконечную рекурсию / переполнение стека. Контраст с `push_vendor_autoload_file_and_static_includes` (`:482-513`), где корректно используется `MAX_STATIC_INCLUDE_DEPTH`.

> **Комментарий Codex:** подтверждаю. `is_dir()` следует symlink, а рекурсивная функция не хранит visited set. Предпочтителен итеративный обход с `symlink_metadata`: либо вообще не следовать directory symlinks, либо канонизировать и дедуплицировать посещённые директории; depth limit остаётся дополнительным предохранителем.

---

## 3. Minor — php-lsp-parser

### 3.1 Двойная конвертация UTF-16 → byte в `position_to_byte`
`server/crates/php-lsp-parser/src/resolve.rs:5776-5793`

`position_to_byte` вызывает `utf16_col_to_byte(source, line, character)`, трактуя `character` как UTF-16 колонку. Но все вызывающие (`infer_variable_type_at_position*`, `infer_variable_type_info_at_position*`) передают `byte_col` — уже результат `utf16_col_to_byte(...)` на серверном слое (`definition.rs:170`, `completion.rs:187`). Одно и то же значение используется как byte-колонка в `resolve.rs:536` (`Point::new(line, character)`) и как UTF-16 колонка в `resolve.rs:541`. Внутренне противоречиво и даёт неверные byte-оффсеты для файлов с не-ASCII текстом перед курсором, ломая инференс типов переменных. Соседний `byte_position_to_offset` (`resolve.rs:331`) корректно трактует аргумент как byte-колонку — подтверждает несоответствие.

> **Комментарий Codex:** подтверждаю; по влиянию это выше обычного minor. Публичный parser API и серверные callers фактически используют tree-sitter byte column, что видно и по `Point::new`. Нужно переименовать параметр в `byte_col`, считать абсолютный offset без UTF-16-конвертации и добавить regression-тест с кириллицей/emoji до переменной на той же строке.

### 3.2 `check_unused_imports` — O(imports × tree)
`server/crates/php-lsp-parser/src/semantic.rs:615,654`

Для каждого `use` `import_name_is_used` рекурсивно обходит весь CST. При многих импортах — квадратично по размеру файла. Один предварительный проход со сбором используемых имён сделал бы это линейным.

> **Комментарий Codex:** подтверждаю как parser performance debt. Собирать usage нужно один раз, но с сохранением различий class/function/const, alias и scope, иначе оптимизация изменит семантику unused-import diagnostics.

### 3.3 `line_byte_len` — O(line) на вызов
`server/crates/php-lsp-parser/src/semantic_tokens.rs:195` (используется в `:143-163`)

`source.split('\n').nth(line)` для каждой промежуточной строки многострочного токена. Многострочный комментарий/строка на N строк даёт O(N²). `Utf16LineIndex` уже хранит длины строк по байтам — можно переиспользовать.

> **Комментарий Codex:** подтверждаю. Стоит открыть byte lengths через метод `Utf16LineIndex` либо вычислять границы строк один раз при построении semantic tokens; это локальная низкорисковая оптимизация.

### 3.4 Смешение byte-оффсета и byte-колонки в PHPDoc-диапазонах
`server/crates/php-lsp-parser/src/symbols.rs:838-839,869`

`start_col = line_base_col + name_start`, где `name_start` — byte-оффсет внутри строки (`raw_line.find(...)`), а `line_base_col` — byte-колонка. Для не-ASCII PHPDoc-строк (например, `@method` с не-ASCII описанием перед именем метода) byte-оффсет не равен колонке, и диапазон неверен.

> **Комментарий Codex:** не подтверждаю. `str::find` возвращает byte offset, а `tree_sitter::Point.column` и хранимые `SymbolInfo` ranges используют byte columns; сумма byte column + byte offset корректна и при Unicode. Ошибка возникла бы только при преждевременной конвертации одного операнда в UTF-16, которой здесь нет.

### 3.5 `if_then_branch_exits` — наивный split по `"else"`
`server/crates/php-lsp-parser/src/resolve.rs:2384`

`text.split("else").next()` разбивает по подстроке `"else"`, поэтому `elseif`/`else if` обрабатываются неверно, а `then_text.contains("return")`/`"throw "`/`"exit"`/`"die("` дают ложные срабатывания, когда эти токены внутри строковых литералов или комментариев.

> **Комментарий Codex:** подтверждаю. Это semantic inference, поэтому raw text search здесь недостаточен: следует анализировать consequence node первого `if` и реальные statement kinds (`return_statement`, `throw_expression`/statement, exit), не текст ветви.

### 3.6 `is_foreach_header_declared_variable` — наивный скан `{`/`:`
`server/crates/php-lsp-parser/src/cst.rs:11-14`

Конец заголовка ищется через `foreach_text.find('{').or_else(|| find(':'))`. `{` или `:` внутри строкового литерала или массива в заголовке foreach (например, `foreach ($x as ['a' => $v])`) ошибочно принимается за терминатор заголовка.

> **Комментарий Codex:** подтверждаю общий дефект, но приведённый пример с `=>` сам по себе не содержит `:`. Ложное завершение реально для строк/выражений с `{` или `:` внутри header. Надёжнее проверять, что variable node находится внутри tree-sitter field `value`/`key` foreach, без поиска разделителя по сырому тексту.

### 3.7 Регистрозависимое сопоставление встроенных типов
`server/crates/php-lsp-parser/src/references.rs:760`, `resolve.rs:1032`

`is_builtin_or_relative_class_name` и `resolve_name_to_fqn` используют `matches!(name, "string" | "int" | ...)` с точным регистрозависимым сравнением. Имена типов PHP регистронезависимы, поэтому `String`, `INT`, `Bool` не распознаются как встроенные и резолвятся как имена классов — ложные «unknown class» диагностики или неверные цели ссылок.

> **Комментарий Codex:** подтверждаю для указанных путей: `references.rs` сравнивает исходное имя без нормализации. Нужно один раз убрать ведущий `\` и применить `to_ascii_lowercase`/`eq_ignore_ascii_case`; Unicode case folding для PHP identifier semantics здесь не нужен.

### 3.8 `Utf16LineIndex::new` разбивает только по `\n`
`server/crates/php-lsp-parser/src/utf16.rs:29`

Индекс разбивает по `'\n'` и оставляет `\r` в тексте строки для CRLF-файлов. Если `Point.column` tree-sitter исключает `\r` (или наоборот), `byte_col_to_utf16` будет ошибаться на единицу на Windows-переводах строк. Стоит сверить с семантикой CRLF-колонок tree-sitter.

> **Комментарий Codex:** как дефект не подтверждаю. Tree-sitter считает byte column относительно байта после `\n`; `\r` является реальным байтом предыдущей строки, и сохранение его в индексе согласовано с исходником. Полезен отдельный CRLF regression-тест на position перед `\r`, но менять split только из этой гипотезы нельзя.

### 3.9 `utf16_position_to_byte` — перелёт на границе суррогатной пары
`server/crates/php-lsp-parser/src/parser.rs:146`

Цикл прерывается при `utf16_offset >= utf16_char`, но для символа с `len_utf16() == 2` (emoji) `utf16_char`, указывающий на вторую половину суррогатной пары, не детектируется — потребляется полный 2-юнитовый символ, и возвращаемый byte-оффсет перелетает на один code unit. Сдвигает позиции правок в строках с emoji.

> **Комментарий Codex:** частично подтверждаю наблюдение, но не формулировку как обычный валидный edit. Между двумя UTF-16 code units одного scalar value нет UTF-8 char boundary, поэтому точного byte offset не существует; текущий код clamp'ит вправо. Нужно явно выбрать и протестировать policy (обычно отклонить такую позицию либо clamp'ить влево), а не пытаться разрезать UTF-8 символ.

### 3.10 O(properties × symbols) проверки дубликатов
`server/crates/php-lsp-parser/src/symbols.rs:628-632,685-690`

`extract_phpdoc_virtual_properties` и `extract_phpdoc_virtual_methods` делают `result.symbols.iter().any(...)` для каждого PHPDoc-члена. Для классов со многими членами — квадратично; `HashSet` существующих FQN сделал бы линейным.

> **Комментарий Codex:** подтверждаю как низкоприоритетную оптимизацию. Set лучше строить на нормализованном ключе с правильной case-sensitivity для метода/свойства и обновлять при добавлении virtual member, чтобы не изменить duplicate suppression.

---

## 4. Minor — php-lsp-index

### 4.1 `resolve_class_to_paths` не срезает ведущий `\`
`server/crates/php-lsp-index/src/composer.rs:28`

`fqn.strip_prefix(prefix)` не срабатывает, когда `fqn` начинается с `\` (например, `\App\Service\Foo`), поэтому полностью квалифицированные имена с ведущим бэкслешем никогда не совпадают с PSR-4/PSR-0 префиксами. Вызывающие непоследовательны: `vendor.rs:198` срезает, а `analyze.rs:814` передаёт `class_fqn` без среза.

> **Комментарий Codex:** подтверждаю как API-hardening: сам resolver должен нормализовать ведущий `\`, а не полагаться на каждого caller. Основной parser обычно хранит FQN без него, поэтому фактический impact ниже, чем у longest-prefix ошибки.

### 4.2 PSR-4 сопоставление — сырой строковый префикс, не по сегментам
`server/crates/php-lsp-index/src/composer.rs:32`

`strip_prefix` сопоставляет по сырым байтам. Некорректный/легаси префикс без завершающего `\` (например, `"App"`) совпадёт и с `Application\Foo`, давая `lication/Foo.php`. Composer требует, чтобы префиксы заканчивались на `\`; валидации здесь нет.

> **Комментарий Codex:** подтверждаю только для malformed Composer metadata. Валидный PSR-4 prefix обязан оканчиваться `\`; при parsing стоит отклонять/логировать неверную запись или безопасно нормализовать её. Это hardening, не ошибка разрешения валидного `composer.json`.

### 4.3 `source_directories` трактует classmap-файлы как директории
`server/crates/php-lsp-index/src/composer.rs:65-67`

`classmap` может содержать отдельные `.php`-файлы (не только директории), но `source_directories` возвращает их как корни сканирования. Нижележащий обход директорий попытается `read_dir` файл.

> **Комментарий Codex:** не подтверждаю. Имя `source_directories` вводит в заблуждение, но `collect_php_files` явно различает root: для directory вызывает recursive scan, а для `.php` file добавляет сам файл. Переименование API полезно, функционального дефекта в текущем downstream нет.

### 4.4 Per-URI «write barrier» — на деле per-shard блокировка, удерживаемая через пользовательский хук
`server/crates/php-lsp-index/src/workspace.rs:145-211`

`generation_guard` — это `DashMap` `RefMut` (блокировка записи на уровне шарда), удерживаемая на протяжении всего тела `update_file_with_references_with_hook`, включая хук `before_direct_member_publish()` (`:200`). Шарды DashMap не per-URI, поэтому два разных URI, попавших в один шард, сериализуются. Хуже: если хук когда-либо обратится к индексу (например, `get_direct_members`), будет deadlock. Сейчас прод-хук — `|| {}` (`:130`), так что латентно, но дизайн хрупкий.

> **Комментарий Codex:** частично подтверждаю. Guard действительно shard-level и расширяет critical section, но hook приватный и сейчас нужен тестам, а production передаёт пустую closure. Это low design debt: hook нельзя считать произвольным пользовательским кодом; лучше заменить barrier на явный per-URI mutex/generation protocol и не держать DashMap guard во время публикации.

### 4.5 Счётчики поколений — мёртвый код в проде
`server/crates/php-lsp-index/src/workspace.rs:97-100,151-153,211`

`file_update_generations` и `next_file_symbol_generation` пишутся, но никогда не читаются вне тестов (`workspace_tests.rs:355`). Значение поколения служит только блокировкой; монотонный счётчик — чистый оверхед.

> **Комментарий Codex:** подтверждаю: production читает не числовое значение, а использует сам occupied entry как barrier. Либо generation следует включить в проверку согласованности snapshots, либо заменить значение на `()`/отдельный lock и удалить атомарный счётчик.

### 4.6 `find_top_level_symbol_replacement` — O(total symbols) на удаляемый символ
`server/crates/php-lsp-index/src/workspace.rs:318-333`

При каждом удалении top-level символа сканируются все `file_symbols` и все их символы. Удаление файла с K top-level символами — O(K × total_symbols). Удаление/исключение большого файла или корня даёт квадратичную работу.

> **Комментарий Codex:** подтверждаю. Нужен reverse index ключа top-level symbol к кандидатам/владельцам либо групповой rebuild затронутых ключей одним проходом после удаления файла; важно сохранить поддержку дубликатов FQN из разных файлов.

### 4.7 `discover_stub_extensions` рекурсивно обходит каждую директорию ради проверки непустоты
`server/crates/php-lsp-index/src/stubs.rs:152`

`collect_stub_files(&path).is_empty()` полностью рекурсирует каждую top-level директорию, чтобы решить, включать ли её, затем файлы собираются снова. Избыточный полный обход дерева стабов.

> **Комментарий Codex:** подтверждаю повторный обход. После недавней защиты от symlink entries бесконечного цикла здесь уже нет, но I/O удваивается. Discovery может вернуть одновременно extension и уже собранные файлы либо использовать bounded `has_any_php_file` с ранним выходом.

### 4.8 `search` использует Unicode `to_lowercase` вместо ASCII
`server/crates/php-lsp-index/src/workspace.rs:565-581`

`query.to_lowercase()` / `name.to_lowercase()` Unicode-aware, в отличие от `to_ascii_lowercase` везде для PHP-идентификаторов. Безвредно для ASCII, но непоследовательно и чуть медленнее.

> **Комментарий Codex:** подтверждаю непоследовательность и лишние allocation, но не только микропроизводительность: Unicode folding может склеить идентификаторы, для которых проект в остальных местах применяет ASCII semantics. Следует использовать уже принятые ASCII-normalized keys.

### 4.9 64-битный FNV content-hash как единственный backstop корректности кэша
`server/crates/php-lsp-index/src/cache.rs:474-481,504`

`content_hash` использует FNV-1a 64-bit. Коллизия сделает изменённый файл «неизменным» и отдаст устаревшие символы. Вероятность низкая, но это единственный backstop корректности, когда mtime/size не изменились (например, перезапись того же размера в пределах гранулярности mtime). Стоит рассмотреть более сильный хэш (128-bit).

> **Комментарий Codex:** теоретически верно, практически very-low risk для неадверсариального локального кэша. Если hash остаётся единственным content backstop, 128/256-bit быстрый hash усилит гарантию; это hardening, а не наблюдаемый cache bug.

### 4.10 `replace_cache_file` remove-then-rename не атомарен
`server/crates/php-lsp-index/src/cache.rs:245-270`

На платформах, где `rename` падает поверх существующего файла, делается `remove_file(path)` затем `rename(tmp, path)`. Крэш между ними теряет кэш, а конкурентный писатель может вклиниться. Именование temp-файла (`pid` + счётчик) различает только внутри одного процесса, не между процессами, пишущими в один путь кэша.

> **Комментарий Codex:** частично подтверждаю. Окно remove→rename реально на соответствующих платформах, но потеря rebuildable cache восстанавливается полным reindex, а PID уже разделяет одновременные процессы. Стоит использовать платформенный atomic replace или lock file; severity низкая и речь не о потере пользовательских данных.

### 4.11 `relative_cache_path` падает на абсолютный путь вне корня
`server/crates/php-lsp-index/src/cache.rs:599-604`

`strip_prefix(root).unwrap_or(path)` даёт абсолютный путь как «относительный» ключ, когда файл вне корня (симлинк/монтирование). Вводит в заблуждение ключ кэша и может коллидировать/не совпадать между корнями.

> **Комментарий Codex:** частично подтверждаю только проблему контракта/портируемости. Абсолютный fallback сам по себе сохраняет identity и не создаёт очевидной коллизии; для внешних include paths он может быть намеренным. Формат ключа следует сделать tagged (`relative:`/`absolute:`) и тестировать migration, а не молча отбрасывать такие файлы.

### 4.12 `normalized_path_string` канонизирует — симлинкованные корни дают разные идентичности кэша
`server/crates/php-lsp-index/src/cache.rs:606-611`

`fs::canonicalize` резолвит симлинки; один и тот же воркспейс через симлинк и через реальный путь даёт разные `workspace_hash`/`workspace_root` — промахи кэша (не порча, но лишние пересборки).

> **Комментарий Codex:** не подтверждаю причинно-следственную связь: canonicalize как раз сводит symlink path и real path к одной identity, если обе цели существуют. Промах возможен при неуспешной/меняющейся canonicalization, но заголовок утверждает обратное текущему поведению.

### 4.13 `direct_members_from_sources` молча роняет ВСЕ члены при неконсистентности одного источника
`server/crates/php-lsp-index/src/workspace.rs:621-646`

Один out-of-bounds `symbol_index` или несовпадение родителя (`?` / `return None`) прерывает всю функцию, возвращая пустые члены для родителя, хотя другие источники валидны. Повреждённый/устаревший источник скрывает каждый член этого типа.

> **Комментарий Codex:** подтверждаю fail-closed поведение, но источник содержит `Arc<FileSymbols>` того же publish и при нормальном API должен быть согласован. Для robustness лучше пропускать и логировать конкретный повреждённый locator; ещё лучше — при обнаружении инварианта перестроить parent entry, чтобы не смешивать snapshots неизвестного поколения.

### 4.14 `template_substitutions_for_edge` молча обрезает несовпадающие количества
`server/crates/php-lsp-index/src/workspace.rs:716-721`

`templates.iter().zip(args.iter())` отбрасывает лишние шаблоны или аргументы без предупреждения, давая неверные подстановки типов при несовпадении арности.

> **Комментарий Codex:** частично подтверждаю. `zip` корректно связывает доступные аргументы, а отсутствующие template args естественно остаются неподставленными; лишние аргументы не имеют соответствующего template. Реальный недостаток — отсутствие явной политики/диагностики malformed PHPDoc, но автоматически считать полученные пары неверными нельзя.

### 4.15 `is_phpdoc_builtin_type` пропускает многие распространённые builtin
`server/crates/php-lsp-index/src/workspace.rs:1304-1329`

Нет `class-string`, `array-key`, `non-empty-string`, `positive-int`, `numeric`, `key-of`, `value-of`, `int-mask`, `non-empty-array`, `non-empty-list` и др. Type alias с одним из них трактуется как имя класса и резолвится неверно.

> **Комментарий Codex:** частично подтверждаю. `class-string` и некоторые generic/shape формы имеют отдельные варианты `TypeInfo` и не обязательно попадают в этот helper, но список scalar/pseudo-types действительно неполон для alias preservation. Расширять надо через единый parser/type classifier с тестами, а не бесконечно дублировать строковые списки по крейтам.

### 4.16 `remove_file` — избыточные идентичные ветки
`server/crates/php-lsp-index/src/workspace.rs:215-228`

Ветки `Occupied` и `Vacant` различаются только `entry.remove()` vs `drop(entry)`; обе вызывают `remove_file_snapshot` + `remove_direct_member_sources`. Ветка `Vacant` выполняет полную очистку символов для URI, который никогда не индексировался (защитно, но мёртво), а дублирование — риск сопровождения.

> **Комментарий Codex:** подтверждаю дублирование, но `Vacant` cleanup не обязательно мёртв: он может чинить частично рассинхронизированные auxiliary maps. Это refactor/readability, не пользовательский дефект; общую очистку можно вынести, сохранив удаление generation entry только для Occupied.

### 4.17 `parse_composer_json` использует `unwrap_or(Path::new("."))` для родителя
`server/crates/php-lsp-index/src/composer.rs:130`

Для голого относительного `composer.json` `parent()` — `""`; fallback на `"."` срабатывает только для корневого пути. Пути затем джойнятся к пустой/относительной базе, давая неабсолютные `PathBuf`, которые нижележащий код должен пере-резолвить (`analyze.rs:825`). Хрупко, но обрабатывается вызывающими.

> **Комментарий Codex:** подтверждаю как слабость standalone API, не как текущую production-поломку: реальные discovery paths обычно абсолютны, а caller умеет резолвить relative paths. Лучше сначала absolutize входной composer path или явно принимать `base_dir`, без `unwrap_or(".")`.

---

## 5. Minor — php-lsp-completion

### 5.1 Дублирующиеся элементы завершения функций в свободном контексте
`server/crates/php-lsp-completion/src/provider.rs:707,725-739`

`provide_free_completions` вызывает `index.search(prefix)`, который (по `workspace.rs:564-585`) возвращает типы **и функции и константы** через substring `contains`. Затем отдельно итерирует `index.functions` и добавляет каждую функцию с `starts_with(prefix)`. Так как `starts_with` влечёт `contains`, каждая функция с совпавшим префиксом эмитится дважды — раз с rank `0300_…` (из `search`) и раз с `0200_…` (из явного цикла). Дедuplication до `sort_completion_items` отсутствует.

> **Комментарий Codex:** подтверждаю. Исправлять лучше не финальным label-only dedup, а разделить поиск типов/констант и явный function path либо дедуплицировать по `(kind, normalized FQN)`, сохранив более специфичный rank и callable metadata.

### 5.2 `check_use_context` использует полный текст ноды (включая текст после курсора)
`server/crates/php-lsp-completion/src/context.rs:433-437`

Когда `text_before.trim_start()` не начинается с `use` (многострочный group use, или `use` не в начале строки), падает на `node_text` — всю ноду `namespace_use_declaration`/`namespace_use_clause` — так что возвращаемый префикс включает текст **после** курсора. Префикс становится слишком длинным, фильтрация неверна.

> **Комментарий Codex:** подтверждаю. Fallback должен срезать source строго до `cursor_offset`, а затем вычислять активную clause/group prefix по CST; использование полного node text нарушает базовую cursor-local семантику completion.

### 5.3 `use_statement_prefix` не обрабатывает `as`-алиасы и group use
`server/crates/php-lsp-completion/src/context.rs:446-456`

`use App\Models\User as U;` даёт префикс `App\Models\User as U` (алиас `as U` не срезается), а group-use `use Foo\{Bar, Baz}` не обрабатывается. Алиас/фигурные скобки загрязняют префикс.

> **Комментарий Codex:** подтверждаю. Нужен контекст активной `namespace_use_clause`, base namespace group use и отдельное поведение после `as`; raw trim строки здесь недостаточен. Добавить тесты для `use function`, `use const`, multiline groups и курсора в каждой clause.

### 5.4 `member_access_mode_after_cursor` пропускает `++`/`--` как запись
`server/crates/php-lsp-completion/src/context.rs:245-275`

`$this->prop++` (или `--`) классифицируется как `Read`, потому что `starts_assignment_operator` проверяет только `=`/compound-assign, не инкремент/декремент. Влияет на read/write-фильтрацию PHPDoc `@property-read`/`@property-write`.

> **Комментарий Codex:** подтверждаю. Инкремент/декремент являются read-modify-write, а текущая enum умеет только Read/Write; минимум — считать их Write для фильтра, точнее — добавить ReadWrite и определить доступность `@property-read`/`@property-write` явно.

### 5.5 `check_variable_access` не детектирует строковый контекст
`server/crates/php-lsp-completion/src/context.rs:373-401`

Проверяется только один символ перед `$`. Внутри строки в двойных кавычках (`"foo $bar"`) или после `{` в `"{$bar}"` `$` всё ещё трактуется как триггер завершения переменной, даже где это неуместно. Комментарий утверждает «Make sure $ is not part of a string», но проверка гораздо слабее.

> **Комментарий Codex:** частично подтверждаю. Variable completion внутри interpolated double-quoted string может быть полезным и валидным PHP-поведением; внутри single-quoted string/комментария — нет. Решение должно опираться на CST node kinds и сознательно поддерживать interpolation, а не полностью запрещать string context.

### 5.6 `is_type_hint_position` пропускает return-type / type-list
`server/crates/php-lsp-completion/src/context.rs:563-578`

Распознаются только `named_type`, `optional_type`, `union_type`, `intersection_type`, `simple_parameter`, `property_declaration`. `return_type`, `type_list` и `catch`-позиции не детектируются, поэтому завершение типов с пустым префиксом там не предлагается.

> **Комментарий Codex:** частично подтверждаю: при уже существующем `named_type` ancestor срабатывает, но пустая/incomplete позиция после `:` или в catch/type-list может иметь только container/error node и выпадает. Нужны CST fixtures для пустого return type, union/intersection gap, catch и malformed PHP, после чего расширять по field/ancestor, а не по одному списку names.

### 5.7 `class_pseudo_constant_completion_item` всегда эмитится независимо от префикса
`server/crates/php-lsp-completion/src/provider.rs:505-508`

`Foo::xyz` всё ещё предлагает псевдоконстанту `class` (она лишь понижается через rank `1000`, но не фильтруется). Несогласованно с ожиданием, что несовпадающие члены подавляются.

> **Комментарий Codex:** подтверждаю как UX bug. `class` следует добавлять только при empty prefix или совпадении prefix без учёта регистра, аналогично остальным candidates.

### 5.8 Несогласованность границ `byte_range_contains` vs `byte_range_contains_cursor`
`server/crates/php-lsp-completion/src/provider.rs:1044-1053`

`byte_range_contains` использует инклюзивный конец (`<=`), а `byte_range_contains_cursor` — эксклюзивный (`<`). Курсор ровно на конечном байте символа «внутри» для детекции класса, но «снаружи» для детекции callable. Вероятно намеренно, но хрупко и не задокументировано.

> **Комментарий Codex:** не считаю это подтверждённым дефектом. Symbol range containment и point-at-cursor имеют разные естественные контракты: ranges half-open, а внутренний диапазон может заканчиваться ровно с outer. Нужно переименовать/задокументировать helpers и добавить boundary tests; унификация операторов без разбора сломает вложенные ranges.

### 5.9 Странный `filter_text` для PHPDoc-виртуальных членов
`server/crates/php-lsp-completion/src/provider.rs:412,441`

`filter_text` — `"{label} {owner_fqn}::{name}"` (например, `foo App\Models\User::foo`), что не то, что печатает пользователь, и может ломать клиентскую fuzzy-фильтрацию.

> **Комментарий Codex:** не подтверждаю как дефект. LSP `filterText` не обязан совпадать со вставляемым текстом; добавление owner позволяет искать и различать virtual members. Возможен UX-тюнинг по реальному VS Code matching, но утверждение о поломке требует воспроизводимого client test.

### 5.10 `find_object_in_cst` fallback молча предполагает `$this`
`server/crates/php-lsp-completion/src/context.rs:222`

Когда `before_arrow` пуст и нет предка `member_access_expression`/`member_call_expression`, молча предполагается `$this`, что даёт неверные завершения членов в не-`$this` контекстах.

> **Комментарий Codex:** частично подтверждаю для malformed/incomplete CST: fallback может приписать доступ `$this` глобальному/static scope. Его нужно разрешать только когда cursor действительно находится в non-static method текущего класса; иначе возвращать `None`, не угадывать receiver.

### 5.11 `namespace_completion_match_rank` аллоцирует три lowercased строки на тип
`server/crates/php-lsp-completion/src/provider.rs:669-692`

`name.to_lowercase()`, `fqn.to_lowercase()`, `prefix.to_lowercase()` аллоцируются на каждой итерации по `index.types` при непустом префиксе.

> **Комментарий Codex:** подтверждаю micro-optimization. `prefix_lower` следует вычислять один раз за запрос, а name/FQN — сравнивать через ASCII-normalized index keys или allocation-free helper.

### 5.12 `provide_namespace_completions_with_options` сканирует весь индекс типов
`server/crates/php-lsp-completion/src/provider.rs:639-661`

Для пустого префикса (например, после голого `\`) посещается каждый тип в индексе и ранжируется, затем обрезается до 100. Нет ранней границы или индексного поиска по префиксу.

> **Комментарий Codex:** подтверждаю scalability concern. Простая ранняя остановка на DashMap iteration сделает результат недетерминированным; нужен namespace/prefix index либо deterministic bounded selection.

### 5.13 Повторный парсинг PHPDoc на каждый уровень иерархии
`server/crates/php-lsp-completion/src/provider.rs:321-325,362-366`

`parse_phpdoc` вызывается для каждого типа в иерархии на каждый запрос завершения; результаты не кэшируются.

> **Комментарий Codex:** подтверждаю повторную CPU-работу. Лучшее место кэша — parsed metadata в symbol extraction/index snapshot с естественной invalidation по обновлению файла, а не глобальный cache по строке без lifecycle.

---

## 6. Minor — php-lsp-types

### 6.1 `uri_to_path` отклоняет форму `file://localhost/...`
`server/crates/php-lsp-types/src/uri.rs:50-56`

`Url::to_file_path()` возвращает `Err` для непустого хоста на не-Windows. RFC 8089 разрешает `file://localhost/path`, и некоторые клиенты его эмитят, поэтому такие URI молча возвращают `None` вместо декодирования. Нет fallback для случая хоста `localhost`.

> **Комментарий Codex:** подтверждаю interoperability edge case на Unix. Безопасный fallback должен принимать только host `localhost` (case-insensitive) и отвергать произвольные remote hosts; нужны platform-gated URI tests.

### 6.2 `path_to_uri` не нормализует `.`/`..` и не резолвит симлинки
`server/crates/php-lsp-types/src/uri.rs:36-48`

`Url::from_file_path` только percent-кодирует; не схлопывает `..`. `path_to_uri("/tmp/../foo.php")` даёт `file:///tmp/../foo.php`, так что один файл может быть представлен несколькими разными URI, ломая ключевание/дедупликацию индекса.

> **Комментарий Codex:** частично подтверждаю dot-segment identity risk, но рекомендация резолвить symlinks опасна: это меняет видимый пользователю путь, требует существования файла и может смешать намеренно разные symlink roots. Нужна lexical normalization `.`/`..` с корректной обработкой root/prefix; canonicalization — только там, где контракт явно требует physical identity.

### 6.3 `FileUriError.message` приватный без аксессора
`server/crates/php-lsp-types/src/uri.rs:5-8`

Доступен только `path()`; поле `message` недостижимо, поэтому вызывающие не могут показать причину (например, текст ошибки `current_dir`) в диагностике.

> **Комментарий Codex:** не подтверждаю. `FileUriError` реализует `Display`, который включает и path, и private `message`; callers уже используют `err.to_string()`. Отдельный accessor нужен только для структурированной обработки, которой сейчас нет.

### 6.4 `symbol_fqn_eq` трактует `Namespace` регистрозависимо
`server/crates/php-lsp-types/src/lib.rs:557`

```rust
PhpSymbolKind::Namespace => left == right,
```

Имена namespace в PHP регистронезависимы (как имена классов/функций). Ветка должна использовать `eq_ignore_ascii_case`, как ветка `Class`/`Interface`/`Trait`/`Enum`/`Function` выше. Сейчас `Foo\Bar` и `foo\bar` считаются разными namespace.

> **Комментарий Codex:** подтверждаю. Namespace segment comparison должен следовать ASCII case-insensitive семантике PHP, при этом global constant terminal name остаётся отдельно case-sensitive.

### 6.5 `normalize_shape_key_text` срезает `?` до среза кавычек
`server/crates/php-lsp-types/src/lib.rs:220-226`

```rust
key.trim()
    .trim_end_matches('?')
    .trim()
    .trim_matches(|ch| ch == '\'' || ch == '"')
```

Завершающий `?` (маркер optional) удаляется **до** среза кавычек, поэтому ключ в кавычках, чьё имя легитимно заканчивается на `?`, портится: `'foo?'` → `foo` (`?` внутри кавычек теряется). Срез кавычек должен идти первым, затем проверка `?`.

> **Комментарий Codex:** не подтверждаю: для raw `'foo?'` последним символом до снятия кавычек является `'`, поэтому `trim_end_matches('?')` ничего не удаляет и результат будет `foo?`. Более того, текущий порядок различает `'foo?'` и optional `'foo?'?`; предложенная перестановка как раз рискует удалить `?` из имени. Нужны тесты, менять порядок не следует.

### 6.6 `TypeInfo::LiteralString` Display выводит значение без кавычек
`server/crates/php-lsp-types/src/lib.rs:145`

`write!(f, "{}", value)` рендерит литеральный строковый тип вроде `'foo'` как `foo`, что визуально неотличимо от имени класса/типа в hover/completion/inlay. Литеральные строки следует кавычить (или иначе различать) в `Display`.

> **Комментарий Codex:** подтверждаю semantic display ambiguity. Следует выводить валидный type syntax с кавычками и escaping, а затем проверить consumers, которые могли ошибочно полагаться на unquoted значение как insertion text.

### 6.7 `SymbolInfo.visibility`/`modifiers` без `#[serde(default)]`
`server/crates/php-lsp-types/src/lib.rs:396-397`

Оба `Visibility` и `SymbolModifiers` реализуют `Default`, и структура уже использует `#[serde(default)]` на новых полях (`attributes`, `extends`, `implements`, `traits`, `templates`, `template_bindings`). Если `visibility`/`modifiers` были добавлены после начального формата кэша, старые записи без них не десериализуются вместо fallback на дефолты — риск миграции кэша, несогласованный с окружающими полями.

> **Комментарий Codex:** не подтверждаю как cache migration bug: binary cache имеет `CACHE_SCHEMA_VERSION`, и несовместимые старые записи должны инвалидироваться/rebuild, а bincode field evolution всё равно не гарантируется одним `serde(default)`. Атрибуты можно добавить для JSON/API tolerance, но это отдельный контракт.

### 6.8 `SymbolInfo` без `PartialEq`/`Eq`/`Hash`
`server/crates/php-lsp-types/src/lib.rs:381`

`SymbolReference` (`:617`) реализует `PartialEq, Eq`, а `SymbolInfo` — только `Debug, Clone, Serialize, Deserialize`. Несогласованность делает невозможными проверки равенства `SymbolInfo` в тестах/потребителях без ручного сравнения полей.

> **Комментарий Codex:** не считаю это дефектом. Trait derivation должен следовать реальной потребности, а `SymbolInfo` не используется как hash key и содержит много полей. Для тестов field assertions часто дают более полезную диагностику; добавлять `Eq`/`Hash` ради симметрии не нужно.

### 6.9 Кортежные диапазоны `(u32,u32,u32,u32)` нетипизированы
`server/crates/php-lsp-types/src/lib.rs:392-394,374,453,463,622`

`range`/`selection_range` на `SymbolInfo`, `SymbolAttribute.range`, `UseStatement.range`, `NamespaceScope.range`, `SymbolReference.range` используют голый 4-кортеж. Нет типового различия между byte-колонками (`SymbolInfo`) и UTF-16 (`SymbolReference`), а порядок `(line, col, line, col)` легко перепутать. Именованные `Range`/`Position` предотвратили бы путаницу byte-vs-UTF-16, о которой предупреждает документация крейта.

> **Комментарий Codex:** подтверждаю как существенный архитектурный долг, хотя migration широкая и не minor по объёму. Нужны разные newtypes (`ByteRange`, `LspRange`), а не один общий `Range`; внедрять по границам крейтов с compile-time conversion helpers.

---

## 7. Minor — php-lsp-server (core)

### 7.1 `diagnostic_publish_request_is_current` повторно запускает `extract_file_symbols`
`server/crates/php-lsp-server/src/server.rs:399-423,153`

`diagnostic_publish_request_is_current` вызывает `open_document_snapshot_from_state`, который вызывает `extract_file_symbols(&tree, &source, uri_str)` (`:153`) — полное извлечение символов. Вызывается до двух раз в `DiagnosticsPublisher::publish` (`:353-364`) и ещё дважды в publish worker (`:452,463`), т.е. ~4 полных извлечения символов на публикацию диагностик, даже когда запрос устарел. Проверке «is current» нужны только `document_state`/`template_document`, не `file_symbols`.

> **Комментарий Codex:** подтверждаю. Freshness check должен брать lightweight state/template snapshot под тем же per-document synchronization contract, не строить `OpenDocumentSnapshot` с symbols. Это заметная CPU-экономия на частых edits.

### 7.2 `run_file_io_blocking` таймаут не отменяет блокирующую задачу
`server/crates/php-lsp-server/src/server.rs:664-703`

```rust
let task = tokio::task::spawn_blocking(op);
let result = match tokio::time::timeout(..., task).await { ... Err(_) => return Err(...) };
```

При таймауте `spawn_blocking`-задача дропается, но продолжает выполняться в фоне (tokio не может отменить блокирующие задачи). Зависшая файловая операция продолжит работать и держать ресурсы после того, как вызывающий уже сдался и, возможно, повторил.

> **Комментарий Codex:** подтверждаю ограничение. Tokio уже запущенную blocking closure отменить не может; следует ограничить concurrency/повторные операции и, где возможно, делать операции дробными с cooperative cancellation. Сам timeout полезен для latency caller, но не является resource cancellation.

### 7.3 `AnalyzeSeverity::includes` делает `Hint` идентичным `All`
`server/crates/php-lsp-server/src/analyze.rs:61-87`

`Self::Hint` матчит `ERROR | WARNING | INFORMATION | HINT`, т.е. всё. `--severity hint` ведёт себя ровно как `--severity all`, что удивительно, учитывая, что CLI рекламирует `hint` как отдельный уровень. Либо `Hint` должен значить «только hints», либо избыточность задокументировать/убрать.

> **Комментарий Codex:** не подтверждаю. Здесь `--severity` реализует minimum threshold: `warning` включает warning+error, поэтому `hint` закономерно включает все четыре явные severity. Он также не идентичен `All`: `All` принимает diagnostic с `severity: None`, `Hint` — нет. Можно яснее назвать option `minimum-severity`, но логика последовательна.

### 7.4 `lsp_position_to_byte` возвращает валидный оффсет для несуществующей хвостовой строки
`server/crates/php-lsp-server/src/util/lsp_text.rs:40-56`

Fallback `if position.line as usize == source.lines().count() { Some(source.len()) }` возвращает `Some(source.len())` для индекса строки, которого нет, когда файл без хвостового перевода строки (например, `"a\nb"` имеет `lines().count() == 2`, так что `position.line == 2` даёт `Some(len)`, хотя строка 2 вне диапазона). Также `byte_col.min(row.len())` клампит по `row`, включающему `\n`, так что позиция конца строки может попасть на байт перевода строки.

> **Комментарий Codex:** подтверждаю. EOF pseudo-line допустима только когда исходник реально оканчивается newline; внутри найденной строки clamp должен быть до длины без `\r?\n`. Нужны тесты для empty source, trailing/non-trailing newline и CRLF.

### 7.5 `normalize_path` не резолвит `..`
`server/crates/php-lsp-server/src/server.rs:1674-1686`

`Component::ParentDir` пушится как есть, так что `a/../b` остаётся `a/../b`, а `../x` — `../x`. Эти ненормализованные пути питают `include_paths`/`exclude_paths` и позже используются в prefix/`starts_with`-сопоставлении, что даёт неверные решения включения/исключения (и лёгкую проблему гигиены path traversal).

> **Комментарий Codex:** подтверждаю path-matching defect. Нужна lexical normalization, которая удаляет предшествующий normal component, но сохраняет ведущие relative `..` и не пересекает root/prefix; это не security traversal само по себе, пока пути не используются для недоверенной записи.

### 7.6 `reload_configured_stubs` глотает панику как `0`
`server/crates/php-lsp-server/src/server.rs:2482-2493`

```rust
let loaded = tokio::task::spawn_blocking(...).await.unwrap_or(0);
```

Если блокирующая задача паникует, сервер молча сообщает «Reloaded 0 stub files» без сигнала ошибки, маскируя реальный сбой.

> **Комментарий Codex:** подтверждаю observability bug. `JoinError` нужно логировать и отправлять indexing status `error`; значение `0` оставлять только для успешной загрузки без файлов. Аналогичный код есть в initial stub loading (9.9), исправлять общим helper.

### 7.7 `PhpVersion::parse` не тримит и молча обрезает
`server/crates/php-lsp-server/src/server.rs:841-846`

`raw.split('.')` без `trim()`, так что `" 8.2"` не парсится; а `"8.2.3"` молча становится `8.2` (лишние сегменты игнорируются).

> **Комментарий Codex:** частично подтверждаю. `trim()` нужен; принятие patch version может быть намеренным, поскольку модель хранит только major/minor. Следует явно принять `major.minor[.patch]` с валидацией всех сегментов или документировать строгий `major.minor`, но не молча игнорировать произвольный хвост.

### 7.8 `apply_fix_edits` не детектирует нулевые вставки в одной позиции
`server/crates/php-lsp-server/src/fix.rs:623-656`

Проверка перекрытия `window[0].1 > window[1].0` трактует две вставки в одном оффсете (`[5,5]` и `[5,5]`) как непересекающиеся, затем применяет их в обратном порядке через `replace_range(5..5, ...)`, давая порядок-зависимый (и потенциально неверный) вывод.

> **Комментарий Codex:** подтверждаю неоднозначность. Нужно либо сохранять и документировать стабильный source order для co-located inserts, либо заранее объединять их, либо отклонять конфликт; текущая unstable sort/обратное применение не задаёт надёжного порядка.

### 7.9 `line_col_for_offset` возвращает byte-колонки, используемые как LSP-позиции
`server/crates/php-lsp-server/src/framework.rs:3891-3904`

`line_col_for_offset` вычисляет `target.saturating_sub(line_start)` (byte-колонка) и кладёт в `VirtualMemberSource::SourceRange.range` / `StaticStringKey.range`. По конвенциям репозитория это byte-колонки, но они позже всплывают как source locations; если потребитель трактует их как UTF-16 (как `template.rs:90-97` для `TemplateShapeKeyDefinition.range`), позиции неверны для файлов с не-ASCII текстом до локации.

> **Комментарий Codex:** не подтверждаю для указанных consumers. `framework_virtual_member_location` вызывает `range_byte_to_utf16`, а string-key location — `range_from_byte_range`, то есть byte range конвертируется перед LSP. `TemplateShapeKeyDefinition.range` — другой тип/путь. Комментарий о единицах в enum всё равно стоит добавить.

### 7.10 `String::from_utf8_lossy` молча портит невалидный UTF-8
`server/crates/php-lsp-server/src/analyze.rs:647`, `fix.rs:833`

`parse_analyze_file` и `parse_fix_file` читают байты и lossy-конвертируют в `String`. Невалидные UTF-8 байты становятся U+FFFD, меняя длины байтов; tree-sitter затем парсит lossy-строку, так что все byte-оффсеты относительны строки, не совпадающей с файлом на диске. Для CLI, сообщающего line/column, это даёт тонко неверные локации.

> **Комментарий Codex:** подтверждаю. Для LSP/PHP source ожидается UTF-8; CLI должен вернуть явную per-file encoding error (либо иметь отдельный документированный lossy mode), а fix особенно не должен строить edits по изменённому представлению байтов.

### 7.11 `std::process::exit` внутри async-контекста
`server/crates/php-lsp-server/src/main.rs:78,88,103`

`handle_cli_command` вызывает `std::process::exit` из runtime `async_main`. Пропускает деструкторы и любую незавершённую async-очистку. Приемлемо для CLI, но обходит нормальное завершение.

> **Комментарий Codex:** не считаю это практическим дефектом текущих CLI-веток: до старта LSP service нет фоновых ресурсов, требующих graceful shutdown, а exit code нужен немедленно. Возврат кода из `async_main` был бы чище и тестируемее, но это refactor.

### 7.12 Жадный сбор ссылок до проверки «is current»
`server/crates/php-lsp-server/src/server.rs:195-197`

```rust
let references = snapshot.template_document.is_none().then(|| {
    collect_symbol_references_in_file(&snapshot.tree, &snapshot.source, &snapshot.file_symbols)
});
```

`collect_symbol_references_in_file` выполняется до проверок document-version/template (`:203-219`), так что устаревшие снапшоты всё равно платят полную стоимость сбора ссылок.

> **Комментарий Codex:** подтверждаю wasted work. Сначала нужно проверить snapshot generation/version, затем собирать references вне lock и повторно проверить generation перед commit, чтобы ранняя проверка не создала race.

---

## 8. Minor — php-lsp-server (lsp handlers)

### 8.1 Молчаливый fallback URI при ошибке парсинга
`server/crates/php-lsp-server/src/lsp/definition.rs:705`

`target_uri.parse::<Uri>().unwrap_or_else(|_| uri.clone())` молча подставляет URI текущего документа, когда целевой URI не парсится, давая definition, указывающий на неверный файл, вместо `None`.

> **Комментарий Codex:** подтверждаю correctness bug. При malformed target URI нужно пропустить candidate/вернуть `None` с debug/warn, а не создавать правдоподобную location в другом файле.

### 8.2 Trait маппится в `INTERFACE`
`server/crates/php-lsp-server/src/lsp/conversions.rs:9`

`PhpSymbolKind::Trait => SymbolKind::INTERFACE`. В LSP нет `TRAIT`, так что это намеренная аппроксимация, но traits неотличимы от interfaces в document/workspace symbols и hierarchy.

> **Комментарий Codex:** не считаю это дефектом: LSP `SymbolKind` действительно не имеет Trait, а `INTERFACE` — стандартная ближайшая аппроксимация. Отличие можно передавать в `detail`/иконке клиента, если протокол конкретного ответа позволяет.

### 8.3 Начало doc-комментария не учитывает атрибуты
`server/crates/php-lsp-server/src/lsp/templates.rs:1059-1066` (`symbol_doc_comment_start`)

Вычисляет строку doc-комментария как `symbol.range.0 - line_count`, предполагая, что PHPDoc сразу над символом. PHP 8 атрибуты между docblock и объявлением сдвигают вычисленный диапазон, давая неверные локации shape-definition.

> **Комментарий Codex:** подтверждаю. Range docblock нужно сохранять при symbol extraction либо искать конкретный `doc_comment` относительно declaration/attribute CST, а не восстанавливать по числу строк. Добавить PHP 8 attribute + Unicode regression test.

### 8.4 Наивный подсчёт скобок в on-type форматировании
`server/crates/php-lsp-server/src/lsp/formatting.rs:248-266` (`brace_delta` / `brace_depth_before_line`)

Считает `{`/`}` без пропуска строковых литералов, комментариев или heredoc, так что глубина отступа неверна для строк со скобками внутри строк (например, `$s = "}";`).

> **Комментарий Codex:** подтверждаю. Для on-type formatting нужен CST-derived nesting или хотя бы lexer state через строки/comments/heredoc; посимвольный brace count по raw line неизбежно даёт ложные изменения indentation.

### 8.5 Блокирующее чтение файла в Twig context resolver
`server/crates/php-lsp-server/src/lsp/templates.rs:54`

`TwigContextPhpSourceResolver::resolve_symbol_source` вызывает `std::fs::read_to_string(path)` синхронно (ограничено только `allow_disk_read`) — блокирующее чтение внутри пути вычисления Twig-контекста.

> **Комментарий Codex:** для текущего async пути не подтверждаю: resolver с `allow_disk_read` вызывается внутри `collect_cached_twig_context_file_variables`, который целиком запускается через `run_file_io_blocking`; open/include scans также обёрнуты. Сам sync helper допустим внутри blocking closure, но контракт стоит задокументировать, чтобы его не вызвали inline позже.

### 8.6 Дублированные реализации `full_document_range`
`server/crates/php-lsp-server/src/lsp/inlay_hints.rs:121-128` и `formatting.rs:7-24`

Одна и та же логика UTF-16 full-document range реализована независимо (одна через подсчёт байтов, другая через итерацию char). Риск расхождения; стоит вынести в общий хелпер.

> **Комментарий Codex:** подтверждаю maintainability debt. Общий helper логично разместить в `util/lsp_text` и покрыть non-ASCII/CRLF/trailing newline tests; текущего воспроизводимого расхождения в отчёте не показано.

### 8.7 Мёртвый код / неиспользуемые параметры
- `completion.rs:994` — `member.split('(').next().unwrap_or(member)`: `split` всегда даёт хотя бы один элемент, так что `unwrap_or` недостижим.
- `completion.rs:194` — `self.namespace_map.lock().await.clone()` вычисляется и передаётся как `_namespace_map` (не используется) в `framework_string_key_items` (`definition.rs:1082-1101`) — лишний захват блокировки + клон на пути framework-string-key completion.

> **Комментарий Codex:** подтверждаю оба cleanup-пункта. `split(...).next()` всегда `Some`, а clone namespace map действительно бесполезен из-за `_namespace_map`; удалить argument/call-site lock, не смешивая это с изменением completion behavior.

### 8.8 Особенность end-позиции `lsp_code_lens`
`server/crates/php-lsp-server/src/lsp/references.rs:609-613`

Для многострочных selection ranges `end` схлопывается в `start` (нулевая ширина), а однострочные сохраняют реальный конечный символ. Вероятно намеренно (однострочный lens), но неочевидно и не задокументировано.

> **Комментарий Codex:** не подтверждаю как bug: CodeLens range служит anchor, и zero-width на declaration start допустим. Непоследовательность стоит упростить до одного документированного anchor и закрепить protocol test, но клиентская семантика не требует полного selection range.

---

## 9. Minor — php-lsp-server (indexing)

### 9.1 Vendor fallback пропускается при несуществующем пути namespace_map
`server/crates/php-lsp-server/src/indexing/vendor.rs:203-221`

Vendor-резолв выполняется только когда `all_paths.is_empty()`. Если `namespace_map.resolve_class_to_paths` вернёт путь, которого нет на диске, цикл на `:223` попытается его распарсить, упадёт, и никогда не упадёт на vendor autoload map — хотя класс живёт в `vendor/`.

> **Комментарий Codex:** подтверждаю. Fallback надо запускать не только при пустом candidate list, а после того, как workspace candidates не дали requested type; при этом сначала исправить longest-prefix, чтобы не маскировать ошибочный путь лишним vendor scan.

### 9.2 `resolve_vendor_paths_from_map` — PSR-4 префикс без проверки границы
`server/crates/php-lsp-server/src/indexing/vendor.rs:419-427`

`normalized_fqn.strip_prefix(mapping.prefix.as_str())` полагается на то, что composer-префикс всегда заканчивается на `\`. Некорректный префикс без хвостового бэкслеша (например, `"App"`) совпадёт с `"Application\Foo"` и даст неверный путь. Также FQN, ровно равный префиксу, даёт `relative_path == ".php"`.

> **Комментарий Codex:** подтверждаю как malformed-metadata hardening, аналогично 4.2. Composer map следует валидировать при parsing, а resolver — не строить `.php` для пустого relative class; валидные PSR-4 prefixes этой проблеме не подвержены.

### 9.3 `lazy_index_parents_with_context` — нет детекции циклов / избыточный обход
`server/crates/php-lsp-server/src/indexing/vendor.rs:297-324`

`MAX_DEPTH = 10` ограничивает рекурсию, но нет visited-set. Циклическое наследование (A↔B) и ромбовидное наследование заставляют одного родителя пере-индексироваться многократно, каждый раз с полным `lazy_index_class_with_context` + файловой работой.

> **Комментарий Codex:** частично подтверждаю. Бесконечной рекурсии нет из-за `MAX_DEPTH`, а уже загруженный type заставляет `lazy_index_class_with_context` быстро выйти; однако parent traversal в циклах/ромбах повторяется до depth cap. Передавать normalized visited set правильнее и дешевле.

### 9.4 `lazy_index_member_return_types_with_context` — нет дедупликации return FQN
`server/crates/php-lsp-server/src/indexing/vendor.rs:326-345`

Вектор `return_fqns` не дедуплицируется, так что один и тот же return type lazy-индексируется (и его родители обходятся) по разу на каждый член, который на него ссылается.

> **Комментарий Codex:** подтверждаю небольшую избыточность. Дедуплицировать следует по ASCII-normalized FQN до awaits, сохраняя deterministic order; основной class load часто уже дешёвый из-за `contains_type`, но parent walk повторяется.

### 9.5 `lsp_did_change_watched_files` — отслеживается только последний composer-путь
`server/crates/php-lsp-server/src/indexing/workspace.rs:630-647`

`composer_metadata_changed: Option<PathBuf>` схлопывает несколько composer-изменений в батче в один путь. Инвалидация корректна (чистит все vendor metadata), но лог и решение `reindex_workspace` управляются только последним путём. `composer_requires_workspace_reindex` только выставляется в `true`, никогда не сбрасывается, так что порядок важен для лога, но не для корректности.

> **Комментарий Codex:** не подтверждаю пользовательский дефект: итоговые invalidate/reindex flags аккумулируются корректно. Сохраняется только последняя path для сообщения, поэтому максимум теряется точность лога; можно хранить set изменённых metadata paths.

### 9.6 `cached_vendor_autoload_map` — TOCTOU гонка двойного парсинга
`server/crates/php-lsp-server/src/indexing/vendor.rs:636-657`

Lock → check → release → `parse_vendor_autoload_map_blocking` (блокирующий) → lock → insert. Два конкурентных вызывающих оба промахиваются по кэшу и оба парсят `installed.json`. Безвредно (идемпотентно), но избыточная файловая работа; также negative-cache `remove` на `:648` означает, что отсутствующий `installed.json` пере-парсится на каждый lazy lookup.

> **Комментарий Codex:** подтверждаю cache stampede/negative-miss проблему, но lock через await держать нельзя. Подходит per-vendor in-flight `OnceCell`/shared future и negative entry с metadata-based invalidation или коротким TTL.

### 9.7 `touch_vendor_file_lru` — выселение не атомарно с удалением из индекса
`server/crates/php-lsp-server/src/indexing/workspace.rs:1614-1630`

LRU-блокировка отпускается после того, как `touch()` вернёт выселенные URI, затем `index.remove_file` выполняется без блокировки. Конкурентный lazy-index выселенного файла может пере-вставить его между двумя шагами, оставляя LRU и индекс несогласованными.

> **Комментарий Codex:** подтверждаю race. Удерживать Tokio mutex во время sync index removal тоже нежелательно; нужен generation/token у LRU entry и conditional remove, либо единая транзакционная функция с согласованным lock order.

### 9.8 Блокирующие файловые вызовы в async-контексте
`server/crates/php-lsp-server/src/indexing/workspace.rs:1720` (`is_file()`), `vendor.rs:211,717` (`is_dir()`)

Прямые `is_file()`/`is_dir()` внутри async-циклов вместо `run_file_io_blocking`/`spawn_blocking`. Ограничены и быстры, но нарушают правило async/IO репозитория.

> **Комментарий Codex:** подтверждаю формальное нарушение правила, impact мал и bounded одним stat на candidate. Лучше включить проверки в уже существующие blocking parse/resolve operations, а не создавать `spawn_blocking` на каждый `is_dir` отдельно.

### 9.9 `lsp_initialized` — паника spawn_blocking молча глотается
`server/crates/php-lsp-server/src/indexing/workspace.rs:215-226`

`tokio::task::spawn_blocking(...).await.unwrap_or(0)` превращает `JoinError` (панику задачи) в молчаливый `0` счётчик стабов. Паника внутри `load_configured_stubs` неотличима от «стабов нет» и не логируется.

> **Комментарий Codex:** подтверждаю; это второй call site той же проблемы, что 7.6. Исправить общим join-error handling и status/error logging, чтобы initial и reload paths не разошлись.

### 9.10 `discover_workspace_root_configs_blocking` — namespace map молча роняется при ошибке
`server/crates/php-lsp-server/src/indexing/workspace.rs:1249-1262`

При ошибке `run_file_io_blocking` fallback строит конфиги с `namespace_map: None`, молча деградируя с PSR-4 сканирования директорий на полное сканирование дерева (и теряя composer `files`). Только `tracing::warn!`.

> **Комментарий Codex:** частично подтверждаю degradation, но не «молча»: есть `tracing::warn!`. Fallback намеренно сохраняет доступность и root scan обычно включает PHP helper files; следует отразить degraded phase в client status и не выдавать его за нормальную Composer discovery.

### 9.11 `push_metadata_hash_part` — config hash по mtime+size, не по содержимому
`server/crates/php-lsp-server/src/indexing/cache.rs:193-217`

`vendor_cache_hash` и `stubs_cache_hash` выводятся из `metadata.len()` + `modified`. Файл, чьё содержимое изменилось, но сохранило size и mtime, даёт неизменный config hash. Per-file свежесть всё ещё охраняется `content_hash` в `load_valid_cached_sources`, так что это вторичный риск.

> **Комментарий Codex:** подтверждаю теоретический stale-config риск. Per-file content hash защищает PHP sources, но Composer/stub metadata, определяющие состав файлов, могут остаться незамеченными; для небольших config files разумно хэшировать содержимое.

### 9.12 FNV-1a 64-bit для `workspace_hash`/`config_hash`
`server/crates/php-lsp-server/src/indexing/cache.rs:461-472,594-597`

`stable_hash_strings` — некриптографический FNV-1a. `workspace_hash` использует его для имени on-disk директории кэша; коллизия двух разных корней заставит их делить файл кэша (смягчается только проверкой строки `workspace_root` внутри кэша, которая затем форсирует полный reindex — не порча, но риск корректности/доступности).

> **Комментарий Codex:** частично подтверждаю только very-low availability risk. Embedded `workspace_root` не позволит принять чужой cache как валидный, поэтому correctness corruption нет; более длинный hash уменьшит взаимные cache misses при теоретической коллизии.

### 9.13 `workspace_index_cache_config` — неиспользуемый `_root`
`server/crates/php-lsp-server/src/indexing/cache.rs:13-36`

Аргумент `_root` мёртв. Идентичность корня обеспечивается отдельно через `cache.workspace_root` в `cache_miss_reason`, так что безвредно, но вводит в заблуждение.

> **Комментарий Codex:** подтверждаю cleanup. Удалить параметр из функции и callers либо реально включить root в configuration hash, если это требуется контрактом; underscore уже явно сообщает, что сейчас он не используется.

### 9.14 `index_workspace` — `all_files.contains(&abs)` внутри цикла
`server/crates/php-lsp-server/src/indexing/workspace.rs:1831-1844`

`all_files.contains` — O(n) на composer `files`-запись (O(n·m) всего). Ограничено на практике, потому что `ns_map.files` мал, но избегаемо через set.

> **Комментарий Codex:** подтверждаю micro-scalability issue. Тот же membership set, который нужен для 2.3, должен использоваться и здесь, чтобы не плодить отдельные структуры.

### 9.15 `index_workspace` — отмена оставляет work-done progress незавершённым
`server/crates/php-lsp-server/src/indexing/workspace.rs:1964-1973,2030-2037`

При отмене `parse_tasks.abort_all()` и функция возвращает `Ok(())` без `p.finish_with_message(...)`. `ongoing` progress handle дропается без явного finish, что может оставить застрявший индикатор прогресса в клиенте (зависит от `Drop` tower-lsp).

> **Комментарий Codex:** подтверждаю protocol/UX risk. Все early returns после `begin` должны идти через guard/finalizer и отправлять `end` с `Cancelled`; `abort_all` для уже стартовавших blocking tasks всё равно не отменяет их и это также надо учитывать.

### 9.16 `path_is_excluded` — относительное сравнение использует ненормализованный `exclude_path`
`server/crates/php-lsp-server/src/indexing/workspace.rs:773-775`

`relative_path` нормализуется (`normalize_path`), но сырой `exclude_path` сравнивается напрямую. Exclude-записи с `./`, `../` или бэкслешами не совпадут со своим нормализованным относительным аналогом (абсолютная ветка на `:768-771` всё ещё ловит большинство случаев, но только когда путь под `root`).

> **Комментарий Codex:** подтверждаю, хотя `absolute_exclude` спасает обычные случаи. Нормализовать root, candidate и exclude один раз общей lexical функцией; Windows separator/case semantics нужно тестировать отдельно.

### 9.17 `remove_indexed_file_symbols` — `unwrap_or(true)` удаляет файлы с нерезолвимыми URI
`server/crates/php-lsp-server/src/indexing/workspace.rs:1327-1334`

Когда `uri_to_path` падает, файл трактуется как «не под vendor» и удаляется. Безопасный дефолт, но некорректная/легаси URI-запись молча роняется из индекса вместо сохранения или логирования.

> **Комментарий Codex:** не считаю удаление дефектом: malformed/legacy cache URI по правилам проекта должна инвалидироваться/rebuild, а сохранение неразрешимого entry опаснее. Стоит добавить log/counter, но fail-safe default правильный.

### 9.18 `find_composer_json` — детекция autoload по подстроке
`server/crates/php-lsp-server/src/indexing/workspace.rs:1429-1435`

Детектирует «кандидата с autoload» проверкой `content.contains("\"autoload\"") || content.contains("\"psr-4\"")`. `composer.json`, лишь упоминающий эти строки в комментарии/описании (или имеющий только `autoload-dev`), выбирается неверно.

> **Комментарий Codex:** подтверждаю substring heuristic для нескольких candidates. JSON comments невалидны, но строки в `description`/scripts дают false positive; `autoload-dev` с `psr-4` наоборот релевантен. Нужно parse JSON object и ранжировать фактические `autoload`/`autoload-dev` entries детерминированно.

---

## 10. Minor — VS Code client

### 10.1 Синхронное сканирование ФС на extension host при каждом обновлении статуса
`client/src/extension.ts:263-270` (`render()`), `405-436` (`getExtensionSnapshot`), `646-657` (`currentWorkspaceCacheDirs`), `602-644` (`discoverComposerRoot`), `718-789` (`resolveServerBinary`)

`PhpLspStatusController.update()` вызывается на каждое уведомление `phpLsp/indexingStatus` (`extension.ts:975-978`), и каждый `update()` вызывает `render()` → `snapshotProvider()` → `getExtensionSnapshot()`. Этот путь выполняет `currentWorkspaceCacheDirs()` → `discoverComposerRoot()` (блокирующие `readdirSync`/`existsSync`/`readFileSync` на workspace folder) плюс `resolveServerBinary()` (блокирующие `existsSync`/`statSync` и полный скан `PATH`). Во время индексации это срабатывает многократно и может стопорить extension host на больших воркспейсах. Снапшот следует кэшировать и пересчитывать только при изменении конфига/воркспейса.

> **Комментарий Codex:** подтверждаю. Status render должен быть pure/cheap: binary/cache/workspace snapshot кэшируется и инвалидируется на configuration/workspace change или явный refresh, а frequent indexing notifications меняют только status fields.

### 10.2 Опора на приватное поле `vscode-languageclient`
`client/src/extension.ts:524-526`

`managedServerProcess()` читает `(languageClient as unknown as { _serverProcess?: ChildProcess })._serverProcess` — недокументированное приватное поле (подтверждено в `node_modules/vscode-languageclient/lib/node/main.js:151,303`), так что весь termination-fallback хрупок при обновлении библиотеки. Работает для закреплённого `^9.0.1`, но не API-стабильно.

> **Комментарий Codex:** подтверждаю хрупкость. Это best-effort fallback после timeout, но зависимость следует изолировать, version-test'ить и по возможности заменить собственным `ServerOptions` factory, который сохраняет публичный `ChildProcess` handle при spawn.

### 10.3 `cacheBaseDir` игнорирует Windows `USERPROFILE`
`client/src/cachePath.ts:5-13`

Проверяются только `XDG_CACHE_HOME` и `HOME`, fallback на `os.tmpdir()`. На Windows `HOME` часто не задан, так что кэш попадает в temp-директорию (стирается при перезагрузке). Серверный `default_cache_base_dir` (`server/crates/php-lsp-index/src/cache.rs:584-592`) имеет то же ограничение только-`HOME`, так что оба согласованы, но оба субоптимальны на Windows. `os.homedir()` был бы корректнее.

> **Комментарий Codex:** подтверждаю cross-platform issue. Клиенту подходит `os.homedir()`/platform cache convention, серверу — соответствующий Rust helper/`LOCALAPPDATA`; алгоритм и hash path должны оставаться паритетными между ними.

### 10.4 `discoverComposerRoot` ищет только один уровень директорий
`client/src/extension.ts:618-624`

Проверяются только прямые дети корня воркспейса. В монорепо (`packages/*/composer.json`) composer root не находится, так что `currentWorkspaceCacheDirs()` пропускает эти под-корневые директории кэша, и «Clear cache» их не удалит.

> **Комментарий Codex:** подтверждаю ограничение команды clear cache, но depth-1 соответствует текущей server discovery policy. Если поддерживается nested monorepo, discovery нужно расширять согласованно на клиенте и сервере с bounds/excludes, а не делать отдельный бесконечный scan только в extension.

### 10.5 Мёртвый код в `LifecycleCoordinator`
`client/src/lifecycle.ts:58-60`

Геттер `active` (и `operationDepth`) нигде не читается. Безвредно, но не используется.

> **Комментарий Codex:** подтверждаю dead code. Удалить getter и counter целиком, если status не планирует их использовать; queue serialization от них не зависит.

### 10.6 Мёртвый код в `waitForChildProcessExit`
`client/src/serverProcess.ts:63-65`

`if (settled) return;` никогда не может быть истинным в этой точке (единственный синхронный `finish` — финальная проверка `!childProcessIsRunning(...)` на строке 73, которая выполняется после). Гонка, которую он охраняет, уже обработана финальной проверкой.

> **Комментарий Codex:** подтверждаю при текущей event semantics: callback `exit` не выполняится синхронно внутри `once`. Проверка лишняя, а финальный liveness check действительно закрывает окно установки listener.

### 10.7 Необработанное отклонение промиса при активации
`client/src/extension.ts:1178`

`void enqueueLanguageClientReconciliation(...)` отбрасывает возвращённый промис. `LifecycleCoordinator.enqueue` возвращает `run`, который отклоняется, если операция бросает (`lifecycle.ts:75-77`). Хотя `startLanguageClient`/`stopLanguageClient` глотают свои ошибки, `notifyServerConfigurationChanged` (`extension.ts:888-897`) и колбэки `onDisabled`/`onRunning` могут бросать, давая необработанное отклонение. `void` следует заменить на `.catch(...)`.

> **Комментарий Codex:** подтверждаю. Activation fire-and-forget promise должен заканчиваться `.catch(error => lifecycleLog(...))`; внутренний queue уже останется usable благодаря `this.queue = run.catch(...)`, но process-level unhandled rejection нужно исключить.

### 10.8 Записи `stoppingClients` никогда явно не удаляются
`client/src/extension.ts:49,912`

`stoppingClients.add(currentClient)` никогда не парный с `.delete()`. Полагается на GC `WeakSet`, что корректно на практике, но флаг «stopping» висит на время жизни (уже утилизированного) объекта клиента.

> **Комментарий Codex:** не подтверждаю проблему. `WeakSet` не удерживает объект, а конкретный остановленный client никогда не должен снова считаться active; постоянная метка правильно подавляет его поздние callbacks. `delete` после stop может, наоборот, открыть race со stale close event.

### 10.9 Лимит рестартов сбрасывается при каждом ручном рестарте
`client/src/extension.ts:537`

`createClientErrorHandler` создаёт свежий `BoundedRestartTracker` на клиент, а `createLanguageClient` выполняется на каждый `startLanguageClient`. Так что защита «4 крэша за 3 минуты» (`lifecycle.ts:23-48`) действует только в пределах одного экземпляра клиента; ручной рестарт сбрасывает счётчик, ослабляя защиту от crash-loop.

> **Комментарий Codex:** не подтверждаю для автоматического crash-loop: `CloseAction.Restart` перезапускает соединение того же `LanguageClient`, поэтому tracker сохраняется. Ручной restart — явное действие пользователя и разумно сбрасывает автоматический breaker; менять это можно только как продуктовую политику.

### 10.10 `getServerEnvironment` затирает пользовательский `RUST_LOG`
`client/src/extension.ts:595-600`

Безусловно выставляет `RUST_LOG` из `logLevel`, перезаписывая любой существующий `RUST_LOG` в окружении. Отбрасывает намерение пользователя.

> **Комментарий Codex:** не подтверждаю как bug: явная настройка расширения `phpLsp.logLevel` должна детерминированно управлять дочерним сервером, а наследованный ambient `RUST_LOG` может быть случайным. Если нужен advanced filter, это отдельная настройка/документация, не автоматический приоритет окружения.

### 10.11 Несогласованность дефолта `diagnosticsSeverity`
`client/src/configuration.ts:47`

Клиентский дефолт — `{}`, а `package.json:116-225` объявляет богатый 7-ключевой дефолтный объект. Поскольку значение пересылается только при явной установке, функционально безвредно, но два дефолта несогласованы и могут разойтись.

> **Комментарий Codex:** не подтверждаю несогласованность: `{}` здесь означает «нет explicit overrides», чтобы сервер/проектный config применил собственные defaults; materializing package defaults в payload как раз замаскирует project settings. Контракт стоит прокомментировать и тестировать reset-to-default.

### 10.12 Относительный `serverPath` резолвится против неверного CWD
`client/src/extension.ts:720-731`

Относительный `phpLsp.serverPath` передаётся в `fs.existsSync`/`isExecutableFile` без резолва против воркспейса или директории расширения, так что резолвится против CWD процесса extension host. Нет и раскрытия `~`/env-переменных.

> **Комментарий Codex:** подтверждаю. Нужно либо требовать absolute path и показать validation error, либо определить стабильную базу (single workspace folder) и явно резолвить; `~` можно поддержать отдельно, env expansion требует чётких escaping/security правил.

### 10.13 `debug` server options передают непроверенный флаг `--debug`
`client/src/extension.ts:834-841`

Debug transport добавляет `args: ["--debug"]` без подтверждения, что bundled binary его принимает. Если сервер не распознаёт флаг, он может не стартовать в debug-режиме.

> **Комментарий Codex:** не подтверждаю заявленный failure: текущий `handle_cli_command` для неизвестного первого аргумента возвращает `false`, после чего сервер штатно запускает LSP, то есть `--debug` просто игнорируется. Это всё же misleading option — флаг нужно либо реализовать/документировать, либо убрать из debug config.

---

## 11. Что улучшить (приоритизированный план)

### P0 — корректность и ресурсы (делать первыми)
1. **Утечка процессов** (`external_command.rs`) — process group + kill всей группы.
2. **Коррупция UTF-8** (`framework.rs:3142`) — `char_indices()`.
3. **PSR-4 longest-prefix** (`composer.rs:28`) — единственный longest match.
4. **Двойная UTF-16 конвертация** (`resolve.rs:5776`) — единая семантика byte-колонки.
5. **O(n²) сбор файлов** (`indexing/workspace.rs:887`) — `HashSet`.
6. **Неограниченная рекурсия classmap** (`indexing/vendor.rs:581`) — depth limit + cycle detection.

> **Комментарий Codex:** P0 в целом обоснован, но я бы назвал process leak/high и поднял двойную UTF-16 конвертацию из исходного Minor. Для PSR-4 нужно сохранять все directories longest prefix; для classmap основная защита — не следовать directory symlinks/visited identity, а depth limit является вторичным предохранителем.

### P1 — производительность async-путей
7. Вынести блокирующий парсинг/извлечение символов в `spawn_blocking` (`diagnostics.rs`, `references.rs`, `rename.rs`, `semantic_tokens.rs`).
8. Устранить квадратичные сканы ссылок в code lens / references (`references.rs:272,600`).
9. Убрать избыточное полное чтение файлов в кэше (`cache.rs:483`) — переиспользовать уже распарсенный контент.
10. Кэшировать снапшот статуса в клиенте (`extension.ts:263`) и убрать блокирующий ФС-скан из async-обработчиков framework.

> **Комментарий Codex:** P1 подтверждаю с уточнением: перенос CPU в `spawn_blocking` требует versioned snapshot/commit, а reference scan лучше устранить batch-indexing, не просто переместить. Framework string-key/Twig scans уже обёрнуты; переносить осталось конкретные virtual-member/fingerprint пути.

### P2 — консистентность и надёжность
11. Регистронезависимые встроенные типы и namespace (`references.rs:760`, `resolve.rs:1032`, `types/lib.rs:557`).
12. `normalize_shape_key_text` — порядок среза кавычек/`?` (`types/lib.rs:220`).
13. Именованные `Range`/`Position` вместо кортежей (`types/lib.rs`).
14. `#[serde(default)]` на `visibility`/`modifiers` (`types/lib.rs:396`).
15. Убрать мёртвый код и неиспользуемые параметры (`completion.rs:994,194`, `lifecycle.ts:58`, `serverProcess.ts:63`).
16. `.catch(...)` на `enqueueLanguageClientReconciliation` (`extension.ts:1178`).

> **Комментарий Codex:** P2 требует пересмотра: пункты 11, 13, 15 и 16 валидны, но предложенное изменение `normalize_shape_key_text` (12) неверно, а `serde(default)` (14) не решает bincode cache migration. Их не следует включать в implementation backlog в текущем виде.

### P3 — полировка
17. `AnalyzeSeverity::Hint` семантика (`analyze.rs:61`).
18. `normalize_path` резолв `..` (`server.rs:1674`).
19. `PhpVersion::parse` trim + строгая валидация (`server.rs:841`).
20. `os.homedir()` в `cachePath.ts` и серверном `default_cache_base_dir`.
21. Общий хелпер `full_document_range` (`inlay_hints.rs`/`formatting.rs`).
22. Документировать/убрать приватное поле `_serverProcess` (`extension.ts:524`).

> **Комментарий Codex:** P3 частично верен. `Hint` уже работает как minimum threshold и отличается от `All`; максимум нужно переименовать/документировать option. Остальные пункты разумны, причём path normalization и Windows cache path важнее косметической полировки.

---

## 12. Замечания о том, что проверено и корректно

- Паритет кэш-хэша клиент/сервер: `stableHashStrings` (`cachePath.ts:23-38`, FNV-1a 64-bit, offset `0xcbf29ce484222325`, prime `0x100000001b3`, разделитель `0xff`) точно совпадает с серверным `stable_hash_strings` (`cache.rs:461-472`), а `normalizeCachePath` (`realpathSync`) — с серверным `canonicalize`. Расхождений нет.

  > **Комментарий Codex:** подтверждаю алгоритмический паритет FNV и canonical/real path intent; при изменении Windows base directory клиент и сервер нужно менять синхронно.

- `DiagnosticsPublisher` шардирование через `DefaultHasher` детерминировано (фиксированные SipHash keys) — не баг.

  > **Комментарий Codex:** подтверждаю в рамках одного процесса: `DefaultHasher::new()` даёт согласованное распределение для producer/worker; стабильность между версиями Rust здесь не требуется.

- Канал `wake` (capacity 1) + drain-until-empty worker не теряет запросы.

  > **Комментарий Codex:** подтверждаю по текущему pending-map + wake protocol: `Full` безопасен, поскольку уже ожидающий wake заставит worker осушить map.

- `fix.rs:418,460` `.expect("parsed file has a tree")` безопасны: `parse_fix_file` уже возвращает `Err` при `None`.

  > **Комментарий Codex:** подтверждаю указанные invariants; это не оправдывает другие `expect`, но эти два не зависят от malformed PHP input после успешного wrapper result.

- `template.rs` байтовая индексация охраняется `starts_with`/`offset < len` и продвижением по char-границам — паник нет.

  > **Комментарий Codex:** для проверенных helper paths подтверждаю bounds discipline; это не формальная гарантия всего модуля, поэтому формулировку лучше не расширять на будущий код.

- `config.rs` trust gate `allowProjectCommands` корректно читается и из top-level, и из `security.allowProjectCommands`.

  > **Комментарий Codex:** подтверждаю: project-provided shell commands остаются за явным trust gate, что соответствует `AGENTS.md`.

- Хелперы коммитов (`commit_renamed_open_document_with_hook`, `commit_disk_php_index_if_closed`, `commit_workspace_disk_file_preserving_open`) хорошо спроектированы для атомарности; тесты подтверждают race-free поведение.

  > **Комментарий Codex:** подтверждаю покрытые race-сценарии, но это не переносится автоматически на LRU eviction из 9.7 и другие отдельные multi-structure commits.

- В `php-lsp-parser`/`php-lsp-completion`/`php-lsp-types` не найдено `unwrap`/`expect`/непроверенных срезов на некорректном вводе — все fallible операции используют `?`/`.ok()?`/`unwrap_or`/`let ... else` с предварительными проверками длины.

  > **Комментарий Codex:** формулировка буквально неверна: в production-коде есть как минимум `parser.rs` `.expect("Failed to set tree-sitter PHP language")` и guarded `merged.pop().unwrap()` в `resolve.rs`. Они выглядят как внутренние invariants, а не panic от malformed PHP, но утверждать полное отсутствие `unwrap`/`expect` нельзя.
