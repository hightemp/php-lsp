# Независимый аудит php-lsp — Codex

> **Автор отчёта:** Codex (OpenAI)<br>
> **Дата аудита:** 2026-08-26<br>
> **Версия проекта:** 0.7.0<br>
> **Статус:** завершённый независимый аудит исходного кода и проверок

## Важное примечание

Этот отчёт первоначально подготовлен Codex независимо. При исходном исследовании
и полном линейном Rust-проходе корневые отчёты `AUDIT-*`, `QWEN-AUDIT-*` и
`DEEPSEEK-AUDIT-*` не читались и не использовались как источник выводов.

27 августа 2026 года по прямому запросу пользователя файл
`DEEPSEEK-AUDIT-2026-08-19.md` был прочитан полностью (945/945 строк, 110
нумерованных находок). Каждый кандидат повторно проверен по текущему коду;
ниже импортированы только подтверждённые и не дублирующие уже существующие
группы ошибки. Остальные корневые отчёты при этой сверке не открывались.

Сторонний подмодуль `server/data/stubs` (phpstorm-stubs) не проверялся
построчно как собственный код проекта. Проверялись загрузка stubs, обход
файлов, кэширование, упаковка и взаимодействие stubs с LSP.

## Резюме

Архитектура проекта в целом зрелая: Rust-крейты разделены по назначению,
диапазоны byte/UTF-16 в основных LSP-путях обрабатываются дисциплинированно,
индекс поддерживает открытые документы и поколения snapshots, а запуск команд
из project config защищён явным trust gate. Тестовая база существенно шире,
чем у типичного молодого language server.

Критических дефектов уровня P0 не обнаружено. После первоначального аудита был
выполнен отдельный полный линейный проход по **107/107 отслеживаемым Rust-файлам
и 122 884/122 884 строкам**: каждый файл прочитан от первой строки до EOF,
включая unit-, integration- и E2E-тесты. После последующей сверки с DeepSeek
отчёт содержит 82 группы находок:

| Приоритет | Количество | Смысл |
|---|---:|---|
| P1 | 12 | Исправить до следующего публичного релиза |
| P2 | 51 | Ошибки корректности и устойчивости |
| P3 | 19 | Протокольные пробелы, производительность, CI и сопровождаемость |

Главные риски: локальный rename переменной пересекает вложенные callable
scopes, рекурсивная индексация выходит через symlink, старый reindex способен
дописать состояние после запуска нового, extract/inline refactors меняют
семантику программы, генерация конструктора может сломать наследование, Twig
context scan имеет комбинаторный worst case, настройки разных корней multi-root
workspace смешиваются, а vendor metadata не ограничена границами `vendor/`.

## Область проверки

Проверены все first-party области репозитория:

- Rust workspace и пять крейтов в `server/crates`;
- LSP lifecycle, indexing, diagnostics, completion, hover, definition,
  references, rename, formatting, hierarchy, semantic tokens и templates;
- VS Code client, lifecycle и управление server process;
- Composer/vendor/stubs/cache интеграции;
- CLI `analyze` и `fix`;
- конфигурационные схемы;
- CI и release workflows;
- shell/Python tooling и fixtures;
- документация и соответствие заявленным возможностям.

Инвентарь отслеживаемых файлов на момент аудита: 115 файлов в `server`, 21 в
`client`, 17 в `scripts`, 40 в `test-fixtures`, 7 в `docs` и 2 GitHub Actions
workflow.

## Методика

Использовались:

- полный последовательный просмотр каждого отслеживаемого `*.rs` от строки 1
  до EOF без структурного поиска и без выборочного пропуска функций или тестов;
- отдельный журнал покрытия с числом строк и SHA-256 каждого файла:
  [`CODEX-RUST-COVERAGE-2026-08-26.md`](CODEX-RUST-COVERAGE-2026-08-26.md);
- ручная проверка критических control/data flow после линейного чтения;
- Rustfmt, Clippy и Rust tests;
- TypeScript checks и production build;
- протокольный LSP-аудит на `test-fixtures/lsp-cases`;
- повторные запуски нестабильного E2E-теста;
- пост-аудитная построчная сверка всех 110 пунктов DeepSeek с текущим кодом и
  дедупликация относительно собственных находок Codex;
- отдельные воспроизведения symlink escape и PHP 8.5 syntax;
- `npm audit --omit=dev`;
- RustSec `cargo audit`.

### Карта полного покрытия Rust

| Область | Файлы | Строки | Статус |
|---|---:|---:|---|
| `php-lsp-completion` | 5/5 | 3 929/3 929 | Прочитано полностью |
| `php-lsp-index` | 9/9 | 5 518/5 518 | Прочитано полностью |
| `php-lsp-parser` | 25/25 | 22 713/22 713 | Прочитано полностью |
| `php-lsp-server/src` | 49/49 | 59 831/59 831 | Прочитано полностью |
| `php-lsp-server/tests` | 15/15 | 29 995/29 995 | Прочитано полностью |
| `php-lsp-types` | 4/4 | 898/898 | Прочитано полностью |
| **Итого** | **107/107** | **122 884/122 884** | **100%** |

Отдельно от линейного Rust-прохода сохранены проверки границ Rust ↔ TypeScript
client, конфигурации, file watchers, release и cache-path parity. Сторонний
подмодуль stubs в эти 107 first-party файлов не входит.

## P1 — высокий приоритет

### CODEX-P1-01. Рекурсивная индексация следует по symlink без защиты от циклов

> **Статус 2026-08-28:** исправлено. Внешние symlink остаются поддерживаемым
> поведением; исправлены небезопасный обход, дубли и live-update внешних целей.

> **Принятое решение 2026-08-28:** индексирование symlink-файлов и директорий,
> включая цели вне workspace, нужно сохранить — это поддерживает shared packages,
> monorepo-модули, Composer path repositories и смонтированные исходники. Дефектом
> считается не сам выход за границы workspace, а отсутствие cycle detection,
> дедупликации, управляемых exclusions и ограничений обхода.

Основной workspace walker использует `path.is_dir()`, который следует по
symlink, и не ведёт набор посещённых каталогов:

- [`server/crates/php-lsp-server/src/indexing/workspace.rs:843`](server/crates/php-lsp-server/src/indexing/workspace.rs#L843)
- [`server/crates/php-lsp-server/src/lsp/templates.rs:173`](server/crates/php-lsp-server/src/lsp/templates.rs#L173)
- [`server/crates/php-lsp-server/src/framework.rs:3581`](server/crates/php-lsp-server/src/framework.rs#L3581)
- [`server/crates/php-lsp-server/src/indexing/vendor.rs:580`](server/crates/php-lsp-server/src/indexing/vendor.rs#L580)

#### Подтверждение

В тестовой директории был создан symlink `workspace/linked` на внешний каталог.
Команда `php-lsp analyze workspace --project-root workspace --format json`
проанализировала внешний файл как `linked/Outside.php` и вернула его
диагностики.

#### Последствия

- случайная ссылка на слишком большое внешнее дерево может резко расширить
  индекс и раскрываемые через LSP имена;
- symlink-ветка может обходить ожидаемые разработчиком exclusions;
- повторный обход деревьев и потенциальные циклы;
- продолжающиеся фоновые задачи после timeout.

#### Что исправить

Создать единый общий filesystem visitor:

1. Проверять `DirEntry::file_type()`, явно распознавать symlink и продолжать
   обход доступной цели, даже если она находится вне workspace.
2. Хранить identity посещённых директорий: device/inode на Unix, platform
   file-id на Windows, canonical path как fallback; повторный identity не
   обходить.
3. Дедуплицировать один физический файл, доступный через несколько ссылок,
   сохраняя детерминированный первый логический URI для навигации.
4. Применять exclusions к видимому в проекте логическому пути, чтобы отдельную
   symlink-ветку можно было исключить без запрета внешних целей в целом.
5. Broken и недоступные ссылки пропускать с диагностическим log-сообщением, не
   прерывая остальную индексацию.
6. Проверять cancellation и traversal/file budgets во время внешнего обхода;
   не полагаться на фиксированную глубину как средство против циклов.
7. Применить одинаковую политику к workspace, templates, framework и vendor и
   учесть обновление внешних целей, которые VS Code watcher может не видеть.
8. Добавить тесты: внешний symlink индексируется, cycle завершается, несколько
   aliases не дублируют файл, exclusion работает, broken link безопасно
   пропускается.

#### Реализовано

- workspace, CLI `analyze`/`fix`, Twig, framework и vendor classmap используют
  единый итеративный filesystem visitor с platform file ID (`file-id`) и
  canonical fallback;
- каталоги и файлы дедуплицируются по физической identity, входные roots и
  записи обходятся в стабильном порядке, а первым URI остаётся
  детерминированный логический symlink-путь;
- exclusions применяются к логическому пути, broken/недоступные ссылки
  пропускаются, cancellation и deadline проверяются внутри blocking traversal;
- добавлены root-specific `indexing.maxFiles=100000` и
  `indexing.maxEntries=1000000`; `0` отключает cap только из trusted
  global/VS Code config, а project config может лишь уменьшить лимит;
- исчерпание лимита сохраняет частичный индекс и публикует `truncated`,
  `truncationReason`, `truncationLimit`, `visitedEntries` в indexing status;
- generation-aware реестр переводит события physical target обратно в
  logical URI, поддерживает alias promotion и multi-root isolation; для
  внешних roots используется стандартная LSP dynamic registration с
  `RelativePattern`, без custom protocol или polling;
- regressions покрывают внешний file/directory symlink, cycles, hardlinks,
  aliases, exclusions, broken links, limits/deadline/cancellation, stale
  generation, multi-root routing, Twig/framework/vendor adapters, cache/config
  parity, физический watched event и 10 000 файлов с линейным числом identity
  lookups.

### CODEX-P1-02. Нестабильный переход к унаследованному vendor-методу

> **Статус 2026-08-28:** исправлено. Находка сохранена как историческое
> обоснование изменения.

Тест
[`test_goto_definition_vendor_inherited_method`](../server/crates/php-lsp-server/tests/e2e_definition.rs#L1630)
иногда получает `null` вместо `BaseAssert::createStub`.

#### Подтверждение

- полный `make check` упал на этом тесте;
- тест запускался 15 раз изолированно;
- 2 из 15 запусков завершились ошибкой;
- ошибка повторилась и с отдельным пустым cache directory.

Таким образом, проблема не объясняется только старым дисковым кэшем.
Подозрительная граница — lazy member resolution и публикация цепочки родителей:

- прежний `resolve_member_lazy_matching_kinds`;
- прежний `lazy_index_class_dependencies_with_context`;
- прежний `lazy_index_parents_with_context`.

#### Последствия

Во время начальной или ленивой индексации `gotoDefinition` может временно не
находить существующий унаследованный метод.

#### Что исправить

- добавить single-flight загрузку класса и его dependency chain;
- повторять member lookup только после стабильной публикации всех родителей;
- добавить generation/version contract для lazy class load;
- сделать stress-тест с десятками/сотнями повторений;
- если запросы до `indexingStatus=ready` официально best-effort, явно
  документировать это и синхронизировать тест, но предпочтительнее сохранить
  работающий lazy fallback во время indexing.

#### Реализовано

- [`WorkspaceIndex`](../server/crates/php-lsp-index/src/workspace.rs#L28)
  публикует committed type/member snapshot через существующий per-URI
  generation barrier: member lookup больше не видит type до его direct-member
  locators и не смешивает поколения при replacement;
- [`VendorLazyLoadCoordinator`](../server/crates/php-lsp-server/src/indexing/vendor.rs#L190)
  объединяет конкурентные cold class и hierarchy loads по identity конкретного
  root index, Composer epoch и нормализованному FQN; отмена одного waiter не
  отменяет общую загрузку, а одинаковые FQN разных roots не объединяются;
- Composer metadata invalidation получает exclusive epoch barrier, дожидается
  старых loads и удаляет их результаты до допуска новых запросов; изменение
  project namespace map заменяет root index, поэтому старая task пишет только в
  detached поколение;
- class/hierarchy loads, Composer namespace checks и vendor entrypoint preload
  держат общий read epoch от parse autoload map до cache/index/LRU commit, так
  что stale map или entrypoint не могут вернуться после invalidation;
- hierarchy snapshot включает поколения class, traits, PHPDoc mixins, parents и
  interfaces; общий lazy member resolver выполняет bounded
  `load -> lookup -> validate` и повторяет lookup при смене поколения;
- definition/hover/completion/diagnostics используют общий стабильный index/lazy
  contract без ожидания `indexingStatus=ready` и без sleeps;
- deterministic unit-тесты фиксируют first-publish/replacement gaps,
  single-flight, cancellation и multi-root isolation; cold-cache E2E выполняет
  100 последовательных переходов к `BaseAssert::createStub`.

### CODEX-P1-03. Уязвимые npm-зависимости входят в extension bundle

> **Статус 2026-09-02:** исправлено. При сохранении VS Code 1.82 и
> `vscode-languageclient 9.0.1` lockfile обновлён до `minimatch 5.1.9` и
> `brace-expansion 2.1.4`; runtime-аудит добавлен в CI и release-сборку.

`npm audit --omit=dev` обнаружил две high transitive vulnerabilities:

| Пакет | Версия | Проблема |
|---|---:|---|
| `minimatch` | 5.1.6 | ReDoS / combinatorial backtracking |
| `brace-expansion` | 2.0.2 | CPU hang и memory exhaustion |

Версии зафиксированы в:

- [`client/package-lock.json:439`](client/package-lock.json#L439)
- [`client/package-lock.json:487`](client/package-lock.json#L487)

Они приходят через `vscode-languageclient 9.0.1` и присутствуют в собранном
`client/out/extension.js`.

Advisory: GHSA-3ppc-4f35-3m26, GHSA-7r86-cg39-jmmj,
GHSA-23c5-xmqv-rm74, GHSA-f886-m6hf-6m8v, GHSA-3jxr-9vmj-r5cp,
GHSA-mh99-v99m-4gvg и GHSA-rgw5-rvv9-x895.

#### Что исправить

- зафиксировать как минимум `minimatch >= 5.1.8`;
- зафиксировать `brace-expansion >= 2.1.4`;
- проверить overrides как промежуточное решение, сохраняющее VS Code 1.82;
- отдельно спланировать переход на новый `vscode-languageclient`;
- добавить `npm audit --omit=dev` в CI.

### CODEX-P1-04. Проект не поддерживает PHP 8.5

Доступные версии ограничены PHP 8.4:

- [`client/package.json:42`](client/package.json#L42)
- [`config-schema.json:15`](config-schema.json#L15)
- [`README.md:19`](README.md#L19)

PHP 8.5 официально выпущен 20 ноября 2025 года:
[PHP 8.5 Release Announcement](https://www.php.net/releases/8.5/en.php).

#### Подтверждение

CLI-анализ файла с pipe operator и clone-with:

```php
$slug = $title |> trim(...) |> strtolower(...);
return clone($this, ['alpha' => $alpha]);
```

вернул четыре ложные синтаксические ошибки.

#### Что исправить

- обновить tree-sitter PHP grammar;
- добавить `8.5` в client и JSON schema;
- обновить version-aware diagnostics и stubs;
- добавить тесты для pipe operator, clone-with, `#[NoDiscard]`, final promoted
  properties и новых вариантов asymmetric visibility;
- обновить README и конфигурационную документацию.

### CODEX-P1-05. Rename локальной переменной пересекает вложенные scopes

> **Статус 2026-08-28:** исправлено. Находка сохранена как историческое
> обоснование изменения.

До исправления [`find_variable_references_at_position`](../server/crates/php-lsp-parser/src/references.rs#L33)
выбирает ближайший scope, после чего
прежний `walk_variable_refs`
рекурсивно проходит все вложенные функции, closures и arrow functions без
scope barrier.

Например, rename внешней `$value` также изменит независимо объявленную
`$value` внутри вложенной closure/function. Обратный тест существует только
для cursor внутри вложенной функции; случай cursor во внешнем scope не покрыт.

Это особенно опасно, потому что результат сразу превращается в `WorkspaceEdit`
в [`lsp/rename.rs`](../server/crates/php-lsp-server/src/lsp/rename.rs#L6).

#### Что исправить

- прекращать обычный обход на границе nested callable;
- отдельно переносить только реальные closure captures (`use ($value)`) и
  auto-captures arrow function;
- различать declaration/read роль capture-token в зависимости от
  рассматриваемого scope;
- добавить E2E на shadowing во function/closure/arrow и на nested capture.

#### Реализовано

- [`walk_variable_binding_refs`](../server/crates/php-lsp-parser/src/references.rs#L1490)
  останавливается на независимых named functions/methods, обычных closures и
  class-member bodies, не смешивая local variable с одноимённым property;
- explicit `use ($value)`/`use (&$value)` и implicit arrow captures образуют
  направленную цепочку binding scopes, а одноимённый parameter её разрывает;
- same-name `global` в capture-connected callable консервативно запрещает
  rename всего binding component: runtime/control-flow alias нельзя безопасно
  представить лексическим диапазоном без global/cross-file symbol expansion;
- capture-token считается read внешнего binding, declaration filtering также
  покрывает variadic parameters, destructuring, catch и output arguments, но
  сохраняет reads в array keys/receivers и dynamic-variable names сложных write
  targets; foreach bindings определяются по CST target, а не delimiters текста;
- диапазоны сортируются и дедуплицируются до LSP-конвертации;
- parser и protocol regressions проверяют outer/inner cursor, nested captures,
  sibling shadows, anonymous classes, interpolation и UTF-16 edits.

### CODEX-P1-06. Конфигурации multi-root workspace смешиваются глобально

> **Статус 2026-09-02:** исправлено после повторной проверки. Находка сохранена
> как историческое обоснование изменения; дополнительно закрыты aggregate-index
> утечки import quick-fix и outside-workspace member resolution, а аварийный
> fallback конфигурационного I/O сохраняет настройки каждого workspace folder.

[`load_effective_configuration_settings`](../server/crates/php-lsp-server/src/indexing/workspace.rs#L915)
последовательно объединяет project config всех workspace roots в один JSON.
Последний root переопределяет PHP version, include/exclude, stubs, analyzers и
formatter для всех остальных roots.

На стороне VS Code используется `workspace.getConfiguration("phpLsp")` без
URI, хотя большинство настроек объявлены со scope `resource`. В итоге
workspace-folder-specific values также не передаются корректно.

#### Последствия

- файл одного проекта анализируется с PHP version другого;
- exclusions и include paths применяются к чужому root;
- formatter/analyzer config зависит от порядка workspace folders;
- trust одного root фактически влияет на общее runtime state.

#### Что исправить

Хранить `ResolvedRuntimeConfiguration` на каждый effective root, выбирать его
через longest containing root для URI и отправлять resource-scoped client
snapshot отдельно для каждой workspace folder.

#### Реализовано

- VS Code client отправляет versioned snapshot с отдельными explicit settings
  для каждого workspace folder и обновляет его при configuration/folder change;
- server независимо объединяет defaults → global config → project config →
  resource-scoped client settings и применяет trust gate в контексте того же
  root;
- исходный workspace folder, Composer effective root, namespace map и
  `ResolvedRuntimeConfiguration` публикуются одним root-context набором;
- document URI выбирает самый специфичный содержащий root; URI вне workspace не
  наследует первый root;
- diagnostics/PHP version, code actions, inlay hints, formatter, analyzers,
  framework namespace map, indexing/cache/include/exclude, vendor и stubs
  orchestration переведены на root-scoped configuration;
- каждый root владеет отдельным symbol/reference index; агрегированный index
  оставлен для workspace-wide выдачи и не участвует в document-scoped
  resolution, поэтому одинаковые FQN и `phpstub://` URI не перезаписывают
  соседний root;
- completion resolve и hierarchy items сохраняют URI исходного workspace,
  request целиком удерживает одну опубликованную generation, а stub source
  читается только из настроенного для этого root дерева;
- side effects конфигурации заменяют/reindex только изменившийся root и не
  удаляют nested workspace при снятии родительской папки;
- import quick-fix ищет классы, функции и константы только в request-owned
  index, а lazy member resolution вне workspace использует изолированный
  fallback index вместо workspace-wide aggregate;
- при timeout/error загрузки project/global config client-only fallback
  продолжает применять отдельные resource-scoped настройки каждого root;
- добавлены unit/client/E2E regressions на PHP version/diagnostics runtime
  update, одинаковые FQN/stub URI, includePaths, formatter commands, command
  trust, Composer selection, nested/shared effective roots, vendor/stub
  visibility, cross-root import candidates, outside member resolution и
  конфигурационный fallback.

### CODEX-P1-07. Auto formatter может исполнять код недоверенного workspace

Formatter provider по умолчанию имеет значение `auto`. Он читает
`composer.json`, обнаруживает Pint/php-cs-fixer/phpcbf и затем запускает
`vendor/bin/...` из workspace:

- [`lsp/formatting.rs:41`](server/crates/php-lsp-server/src/lsp/formatting.rs#L41)
- [`lsp/formatting.rs:154`](server/crates/php-lsp-server/src/lsp/formatting.rs#L154)

Trust gate очищает команды, пришедшие из `.php-lsp.toml`, но auto-detected
workspace executable не проходит через `allowProjectCommands`. Client также
не проверяет `workspace.isTrusted`.

Форматирование запускается по явному запросу пользователя, что уменьшает риск,
но сам факт выполнения workspace binary не соответствует заявленной модели
«project commands are untrusted by default».

#### Что исправить

- передавать VS Code Workspace Trust в initialization/configuration;
- блокировать auto-detected executables в untrusted workspace;
- либо требовать `allowProjectCommands` и для auto provider;
- показывать выбранный executable до первого запуска и запоминать решение по
  canonical workspace root.

### CODEX-P1-08. Старый reindex может дописать состояние после запуска нового

> **Статус 2026-09-03: исправлено (Codex).** На момент исправления часть
> исходной находки уже была закрыта: прежний token удалялся после
> post-processing, а symlink snapshots и diagnostics имели собственные
> generation-проверки. Эти проверки не обеспечивали общей линейной границы для
> всех записей одного reindex-run, поэтому проблема сохранялась для индекса,
> Composer/vendor state, caches и части post-processing.

В `reindex_workspaces` старый run отменяется только после discovery, смены
workspace-конфигурации и удаления индексированных файлов:

- удаление начинается в
  [`server.rs:2539`](server/crates/php-lsp-server/src/server.rs#L2539);
- новый cancellation token устанавливается лишь в
  [`server.rs:2607`](server/crates/php-lsp-server/src/server.rs#L2607).

Ещё опаснее, что run удаляется из `indexing_run` в
[`server.rs:2667`](server/crates/php-lsp-server/src/server.rs#L2667), **до**
refresh Twig-контекстов, повторного commit открытых документов и публикации
диагностик. Если в этот момент начинается новый reindex, его `start_indexing_run`
уже не видит и не отменяет старый token. Старый post-processing продолжает
работу и может публиковать snapshots, построенные с предыдущими roots/config.

#### Последствия

- устаревшие symbols/references поверх нового поколения индекса;
- Twig virtual documents и diagnostics от предыдущей конфигурации;
- гонка особенно вероятна при быстрых `didChangeConfiguration`, смене folders и
  Composer metadata.

Во время итоговой проверки первый `cargo test --all` дополнительно упал по
10-секундному ожиданию `ready` в
[`test_post_index_diagnostics_preresolve_vendor_imports_for_open_files`](server/crates/php-lsp-server/tests/e2e_diagnostics.rs#L257).
Сразу после этого тот же тест прошёл 20/20 изолированных запусков, а повторный
полный набор прошёл целиком. Это не доказывает конкретно описанную выше
последовательность commits, но независимо подтверждает зависимость перехода
индекса в `ready` от порядка/нагрузки и необходимость детерминированных
generation-barrier тестов.

#### Исправлено

- lifecycle вынесен в `indexing/run.rs`: монотонный `run_id`, отдельный
  координатор на каждый исходный workspace folder, RAII guard и клонируемая
  lease с атомарными `commit_if_current`/`commit_index_if_current`;
- run регистрируется до initial/reindex mutation и остаётся активным до
  завершения Twig refresh, повторного commit открытых документов, постановки
  diagnostics и финального `ready`; новый run немедленно отменяет и лишает
  права commit только старый run того же folder;
- workspace/stub/vendor данные и cache-файлы сначала строятся во временном
  staging state. Публикация в live index и atomic rename выполняются коротким
  guarded commit; stale staging автоматически отбрасывается;
- aggregate rebuild проверяет run snapshot и revisions всех участвующих root
  indexes под общей mutation barrier, поэтому изменения другого root во время
  построения также не теряются;
- symlink aliases, Twig documents/cache, semantic-token invalidation,
  open-document recommit, diagnostics и status защищены той же run identity;
  `DiagnosticPublishRequest` отличает повторные Composer reindex даже при
  одинаковом runtime generation;
- status и diagnostics publishers корректно останавливаются при shutdown:
  queued state очищается, workers abort/await-ятся, а coordinator invalidates
  все latest identities;
- добавлены детерминированные unit/integration/E2E регрессии для supersession,
  commit linearization, panic/abort/error cleanup, staged cache/vendor/stubs,
  Twig/diagnostics, удаления folder, быстрого Composer reindex, shutdown и
  независимости multi-root.

### CODEX-P1-09. Extract/inline variable могут менять семантику программы

[`extract_variable_plan`](server/crates/php-lsp-server/src/lsp/code_action.rs#L3602)
для любой однострочной выбранной expression вставляет assignment перед
enclosing statement. Проверок short-circuit, ternary, loop condition и числа
вычислений нет. Например, извлечение `expensive()` из
`$ready && expensive()` начнёт вызывать функцию даже при `$ready === false`, а
из условия цикла — один раз вместо каждой итерации.

[`inline_variable_plan`](server/crates/php-lsp-server/src/lsp/code_action.rs#L4204)
проверяет одно assignment и последующие reads в одном statement container, но
не проверяет чистоту RHS и изменения его зависимостей. `$x = nextId(); use($x,
$x)` превращается в два вызова `nextId()`, а `$x = $obj->value; $obj->mutate();
use($x)` начинает читать уже новое состояние.

#### Что исправить

- extract разрешать только при доказанном сохранении control-flow/evaluation
  count или ограничить безопасным whitelist pure expressions;
- inline по умолчанию разрешать только для одного непосредственного read и
  pure RHS;
- учитывать writes/calls/aliasing между assignment и usage;
- добавить negative E2E на `&&`, `||`, `?:`, loop condition, function calls,
  property reads и несколько usages.

### CODEX-P1-10. Generate constructor может сломать наследование

[`generate_constructor_edit`](server/crates/php-lsp-server/src/lsp/code_action.rs#L2681)
проверяет только наличие **direct** `__construct`, собирает свойства и генерирует
новый child constructor. Наличие унаследованного конструктора не проверяется,
`parent::__construct(...)` не добавляется.

В PHP объявление child constructor прекращает наследование parent constructor.
После применения action класс с обязательными parent dependencies может стать
невалидным или перестать инициализировать состояние родителя.

#### Что исправить

- находить effective parent constructor по hierarchy;
- если безопасный вызов нельзя синтезировать, не предлагать action;
- иначе переносить совместимые параметры и генерировать явный
  `parent::__construct(...)`;
- тестировать required/optional/variadic parent parameters и multi-level chain.

### CODEX-P1-11. Vendor autoload metadata не ограничена каталогом `vendor`

[`parse_vendor_autoload_map`](server/crates/php-lsp-server/src/indexing/vendor.rs#L147)
доверяет `install-path` и затем присоединяет к нему PSR-4/files/classmap paths.
Нет canonical containment относительно `vendor/`; абсолютный путь или лишние
`../` в локально подменённом `installed.json` направляет lazy index во внешний
каталог. Classmap fallback в
[`collect_classmap_php_files`](server/crates/php-lsp-server/src/indexing/vendor.rs#L581)
рекурсивно следует directory symlink и способен обойти внешнее дерево.

Там же dependency `autoload-dev` безусловно объединяется с runtime autoload
([`vendor.rs:174`](server/crates/php-lsp-server/src/indexing/vendor.rs#L174)),
хотя [официальная схема Composer](https://getcomposer.org/doc/04-schema.md#autoload-dev)
помечает `autoload-dev` как root-only. PSR-0, который Composer по-прежнему
поддерживает, вообще не загружается. Это уже закреплено тестом, ожидающим
dependency `dev-bootstrap.php`.

#### Что исправить

- canonicalize package/autoload paths и требовать containment в canonical
  vendor root;
- не следовать symlink в classmap traversal и ввести file/depth budget;
- не читать `autoload-dev` из installed dependencies;
- либо поддержать PSR-0, либо сначала читать сгенерированные Composer maps
  `autoload_psr4.php`, `autoload_namespaces.php`, `autoload_classmap.php` и
  `autoload_files.php` как authoritative данные;
- добавить hostile installed.json и symlink-cycle tests.

### CODEX-P1-12. Twig context scan имеет комбинаторный worst case

Для определения include-контекста сервер сначала сканирует до 2048 Twig-файлов,
а затем для каждого найденного caller вызывает
[`direct_twig_variable_types_for_template_state`](server/crates/php-lsp-server/src/lsp/templates.rs#L3602),
который способен заново прочитать и распарсить до 2048 PHP-файлов. Цикл виден в
[`templates.rs:3797`](server/crates/php-lsp-server/src/lsp/templates.rs#L3797) и
[`templates.rs:3845`](server/crates/php-lsp-server/src/lsp/templates.rs#L3845).

Worst case — около 4,2 млн чтений/парсов на один refresh одного template. Работа
запущена через `spawn_blocking`; timeout возвращает ошибку, но сам blocking task
не останавливается (см. P2-05). Повторные запросы способны накопить продолжающие
работу фоновые scans.

#### Что исправить

- построить инкрементальный reverse index `template -> render/include callers`;
- кэшировать parse/result на `(canonical path, content hash)`;
- исключить N×M rescans и обновлять только изменившиеся файлы;
- ограничить concurrent scans semaphore и проверять cancellation внутри walk;
- добавить performance regression с тысячами PHP/Twig файлов и верхней границей
  числа фактических reads/parses.

## P2 — корректность и устойчивость

### CODEX-P2-01. Неограниченная загрузка bincode-кэша

[`load_cache`](server/crates/php-lsp-index/src/cache.rs#L213) целиком читает
файл и вызывает `bincode::deserialize` без ограничения размера:

```rust
let bytes = fs::read(path)?;
Ok(bincode::deserialize(&bytes)?)
```

Повреждённый или локально подменённый кэш может вызвать очень большую
аллокацию до того, как deserialize вернёт обычную ошибку.

RustSec не нашёл известных уязвимостей в lockfile, но сообщил
`RUSTSEC-2025-0141`: `bincode 1.3.3` больше не поддерживается.

#### Что исправить

- проверять metadata size до чтения;
- использовать ограниченный decoder;
- перейти на поддерживаемый формат/версию;
- сохранить fail-soft schema migration;
- добавить тест на огромный length prefix и oversized cache file.

### CODEX-P2-02. Union/intersection сворачиваются к первому object type

[`resolve_phpdoc_var_type`](server/crates/php-lsp-parser/src/resolve.rs#L3396)
обрабатывает `Union` и `Intersection` одинаково и возвращает первый разрешимый
тип.

#### Подтверждение

Для аннотации `ArrayAccess&Countable` в
[`test-fixtures/lsp-cases/src/PhpDoc/EdgeCases.php:48`](test-fixtures/lsp-cases/src/PhpDoc/EdgeCases.php#L48)
completion вернул только методы `ArrayAccess` и не предложил
`Countable::count`.

#### Что исправить

- не превращать composite type преждевременно в один `String FQN`;
- для intersection объединять members всех составляющих;
- для union возвращать общие безопасные members либо явно помеченные uncertain
  candidates;
- использовать одну семантику в completion, hover, definition и diagnostics.

### CODEX-P2-03. Не выводится тип `clone` expression

В
[`PromotedSelfDefaults.php:15`](test-fixtures/lsp-cases/src/Diagnostics/PromotedSelfDefaults.php#L15)
переменная `$clone = clone $this` не получает тип текущего класса. Поэтому
completion для `$clone->objectManager` и `$clone->mapping` пуст.

В основном expression inference match
[`resolve.rs:4789`](server/crates/php-lsp-parser/src/resolve.rs#L4789) отсутствует
ветка `clone_expression`.

#### Что исправить

- возвращать тип operand для `clone expr`;
- сохранять `self`/`static` substitution текущего owner;
- поддержать PHP 8.5 clone-with;
- добавить parser unit и completion E2E tests.

### CODEX-P2-04. PHPStan diagnostics могут иметь неверный range или файл

[`phpstan_message_to_diagnostic`](server/crates/php-lsp-server/src/lsp/diagnostics.rs#L147)
не гарантирует LSP-инвариант `end >= start`, хотя Psalm range уже
нормализуется.

[`parse_phpstan_json_diagnostics`](server/crates/php-lsp-server/src/lsp/diagnostics.rs#L191)
при единственном элементе `files` принимает его независимо от пути. Кроме
того, relative analyzer paths canonicalize относительно CWD server process, а
не `current_dir` анализатора.

#### Последствия

- недопустимый LSP Range при malformed analyzer output;
- диагностика другого файла может быть опубликована на текущем документе;
- relative path в multi-file output может быть ошибочно отброшен.

#### Что исправить

- lexicographically clamp `(end_line, end_character)` к start;
- передавать workspace/analyzer cwd в path matcher;
- разрешать relative paths относительно этого cwd;
- убрать безусловный single-file fallback;
- добавить malformed/single-relative/multi-relative regression matrix.

### CODEX-P2-05. Timeout не ограничивает все потребляемые ресурсы

[`run_file_io_blocking`](server/crates/php-lsp-server/src/server.rs#L664)
оборачивает `spawn_blocking` в timeout. После timeout уже выполняющаяся
blocking task не отменяется и продолжает работать в фоне.

[`run_shell_command_with_timeout`](server/crates/php-lsp-server/src/lsp/external_command.rs#L13)
использует `wait_with_output`, который без лимита накапливает stdout и stderr.
`kill_on_drop` завершает shell process, но не гарантирует завершение всего
дерева потомков.

#### Что исправить

- добавить cooperative cancellation в рекурсивные обходы;
- ограничить число тяжёлых blocking operations semaphore;
- читать stdout/stderr потоково с жёстким byte limit;
- запускать Unix process group и завершать всю группу;
- использовать Windows Job Object для эквивалентного поведения;
- добавить тест на subprocess, создающий потомка и неограниченный output.

### CODEX-P2-06. Incremental edit не зажимается на конце строки

[`FileParser::utf16_position_to_byte`](server/crates/php-lsp-parser/src/parser.rs#L146)
проходит по `rope.line(line)`, содержащей line ending. Позиция с character
больше длины строки может пересечь `\n` и превратиться в начало следующей
строки. Общий [`utf16_col_to_byte`](server/crates/php-lsp-parser/src/utf16.rs#L130)
зажимает ту же позицию до содержимого строки, поэтому разные features
интерпретируют malformed/stale position по-разному.

Что исправить: исключать `\n` и `\r\n` из line slice при edit conversion,
централизовать UTF-16 → byte policy и добавить oversized/mid-surrogate E2E.

### CODEX-P2-07. Arrow function не является отдельным diagnostic scope

[`is_variable_scope`](server/crates/php-lsp-parser/src/semantic.rs#L1403)
не включает `arrow_function`. Его параметры и тело попадают во внешний scope.

Подтверждённый пример:

```php
$shadowed = 1;
return fn($shadowed) => $shadowed + 1;
```

CLI вернул 0 diagnostics, хотя внешняя `$shadowed` не используется. Чтение
одноимённого параметра arrow function ошибочно засчитывается внешней переменной.

Что исправить: отдельный arrow scope плюс явный учёт автоматических captures
при анализе внешнего scope.

### CODEX-P2-08. Argument-count diagnostics неверны для части допустимых вызовов

В function и constructor checks required count определяется позицией первого
параметра с default/variadic. Это неверно для legacy-signature с обязательным
параметром после optional: PHP трактует предшествующий optional как required.

Кроме того, один unpacked argument `...$args` считается ровно одним аргументом,
из-за чего возможны ложные `too few` и `too many` diagnostics.

Код находится в
[`semantic.rs:208`](server/crates/php-lsp-parser/src/semantic.rs#L208) и
[`semantic.rs:387`](server/crates/php-lsp-parser/src/semantic.rs#L387).

Что исправить: required boundary считать по последнему required parameter;
при unpack suppress/relax обе границы, пока cardinality неизвестна; отдельно
валидировать named arguments.

### CODEX-P2-09. `SymbolModifiers.is_deprecated` никогда не устанавливается

Поле объявлено и используется completion/document/workspace symbols, но в
production Rust нет ни одного присваивания `is_deprecated: true` или
`mods.is_deprecated = ...`. [`extract_modifiers`](server/crates/php-lsp-parser/src/symbols.rs#L2117)
обрабатывает только static/abstract/final/readonly.

Следствие: initial completion items и symbol tags не помечают `@deprecated` или
`#[Deprecated]`; completion resolve частично компенсирует это только после
дополнительного запроса.

Что исправить: вычислять deprecated из PHPDoc и version-aware Deprecated
attribute при извлечении всех поддерживаемых symbol kinds.

### CODEX-P2-10. File-level PHPDoc aliases протекают между namespace sections

`FileSymbols.type_aliases` и `type_alias_imports` не содержат namespace/range.
[`scoped_at_byte_position`](server/crates/php-lsp-types/src/lib.rs#L490)
фильтрует imports, но оставляет все aliases файла. Alias из первого bracketed
namespace может разрешиться во втором namespace с тем же именем.

Что исправить: хранить scope range/namespace у alias declarations/imports и
фильтровать их вместе с `use_statements`; добавить multi-namespace alias tests.

### CODEX-P2-11. WorkspaceIndex публикует обновление неатомарно

[`update_file_with_references_with_hook`](server/crates/php-lsp-index/src/workspace.rs#L133)
сначала удаляет старый file snapshot и top-level symbols, затем по отдельности
добавляет новые maps, file snapshot, references и member sources. Читатель не
берёт per-URI guard и может увидеть временно отсутствующий class/function или
смешанное поколение.

`file_update_generations` сериализует writers, но generation value нигде не
читается и не обеспечивает reader consistency. Это вероятный класс причин для
нестабильного lazy vendor lookup.

Дополнительно
[`direct_members_from_sources`](server/crates/php-lsp-index/src/workspace.rs#L621)
через `?`/`return None` отбрасывает **всех** direct members родителя, если хотя бы
один locator имеет неверный индекс или parent. При смешанном/повреждённом
snapshot локальная неконсистентность поэтому превращается в полную потерю
member resolution для типа.

Что исправить: immutable per-file generation snapshot + атомарная смена
authoritative generation; readers должны либо видеть old, либо new snapshot.

### CODEX-P2-12. Cache metadata может быть привязана к старому symbol snapshot

При сохранении кэша symbols берутся из index, а
[`file_metadata`](server/crates/php-lsp-index/src/cache.rs#L483) повторно читает
текущий файл позже. Если файл изменился после parse, но до cache build, кэш
получает старые symbols с hash/mtime нового содержимого. Следующий запуск
считает такую запись свежей.

Что исправить: переносить content hash из того же source buffer, который был
распарсен, и отбрасывать commit/cache, если metadata изменилась до публикации.

### CODEX-P2-13. Completion смешивает symbol kinds и создаёт дубликаты

- `provide_free_completions` вызывает `index.search`, который уже возвращает
  types, functions и constants, а затем повторно добавляет functions отдельным
  циклом: [`provider.rs:675`](server/crates/php-lsp-completion/src/provider.rs#L675).
- `UseStatement` context не хранит `class/function/const`, а provider всегда
  перечисляет только `index.types`; `use function` и `use const` получают
  неверные candidates.
- inherited members не dedup по PHP lookup identity, поэтому override и
  interface diamonds могут давать одинаковые labels несколько раз.

Что исправить: typed completion query, единый dedup key с kind-specific casing
и отдельные providers для class/function/const imports.

### CODEX-P2-14. Visibility completion неполно соответствует PHP

[`member_is_visible`](server/crates/php-lsp-completion/src/provider.rs#L948)
разрешает protected instance member только при `object_expr == "$this"`.
Внутри класса корректный `$other->protectedMember` поэтому скрывается.

Private members, пришедшие из используемого trait, имеют owner FQN trait и
скрываются в consuming class, хотя PHP внедряет их в класс.

Что исправить: проверять relationship current/receiver/declaring type, а trait
members материализовать с effective consuming owner или отдельной access model.

### CODEX-P2-15. Signature Help пропускает nullsafe calls и ошибается на comments

[`is_call_node`](server/crates/php-lsp-parser/src/signature_help.rs#L56) не
включает `nullsafe_member_call_expression`. `$service?->run(` не получает
signature help.

Active parameter считается raw-сканером, который знает строки и скобки, но не
comments, heredoc/nowdoc и некоторые PHP tokens. Запятая внутри comment может
сдвинуть `activeParameter`.

Что исправить: использовать CST `argument` boundaries и cursor position;
добавить nullsafe, comment, heredoc и incomplete-call tests.

### CODEX-P2-16. Document Symbols ломают файлы с несколькими namespaces

[`lsp_document_symbol`](server/crates/php-lsp-server/src/lsp/document_symbols.rs#L411)
хранит только один `namespace_sym`; последняя namespace declaration оборачивает
все classes/functions/constants файла, включая символы предыдущих namespaces.

Что исправить: группировать top-level symbols по `NamespaceScope`, сохранять
несколько namespace DocumentSymbol и корректно обрабатывать global sections.

### CODEX-P2-17. Linked Editing связывает одноимённые, но независимые names

[`collect_matching_name_ranges`](server/crates/php-lsp-server/src/lsp/document_symbols.rs#L630)
связывает все `name` nodes с одинаковым текстом внутри namespace/use construct.
В group use два разных import target с одинаковым terminal name могут начать
редактироваться одновременно. Аналогично повторяющиеся namespace segments не
обязательно представляют одну сущность.

Что исправить: связывать только AST-роли одной alias/import identity; для
неоднозначных group uses возвращать `None`.

### CODEX-P2-18. Incoming Call Hierarchy сопоставляет instance calls только по имени

[`incoming_call_hierarchy_for_file`](server/crates/php-lsp-server/src/lsp/hierarchy.rs#L461)
использует legacy `find_references_in_file`. Для `$obj->run()` тот walker
сравнивает только short member name и не проверяет receiver type. Вызов
`Unrelated::run` может появиться как incoming call для `Target::run`.

Scoped `self/static/parent` в legacy walker также принимаются без проверки
отношения текущего класса к target. Outgoing collector не включает nullsafe
member calls.

Что исправить: строить hierarchy из precomputed resolved references и
`SymbolReferenceReceiver`, применяя open-document overlay.

### CODEX-P2-19. Definition PHPDoc virtual member может перейти к чужому comment

[`phpdoc_virtual_member_location`](server/crates/php-lsp-server/src/lsp/definition.rs#L1004)
ищет `doc_comment` через `source.find(doc_comment)`. Если одинаковый PHPDoc text
повторён у нескольких classes, member второго класса ведёт к первому
совпадению.

Отдельный template shape path восстанавливает начало docblock как
`symbol.range.0 - line_count` в
[`symbol_doc_comment_start`](server/crates/php-lsp-server/src/lsp/templates.rs#L1059).
Атрибуты PHP 8 между docblock и declaration делают такой диапазон неверным.

Что исправить: хранить range PHPDoc owner/tag при extraction или искать только
в диапазоне конкретного owner symbol.

### CODEX-P2-20. Inlay/hover type owner выбирается как первый класс файла

В [`server_variable_type_info`](server/crates/php-lsp-server/src/lsp/inlay_hints.rs#L1468)
PHPDoc fallback записывает owner через `current_class_fqn(file_symbols)`, а эта
функция возвращает первый class-like symbol файла:
[`diagnostics.rs:4531`](server/crates/php-lsp-server/src/lsp/diagnostics.rs#L4531).

В multi-class file `self`, `static`, imports и links могут разрешиться в
контексте чужого класса.

Что исправить: всегда передавать variable/call-site range и использовать
`current_class_fqn_at_range`.

### CODEX-P2-21. Runtime-настройка `logLevel` фактически не работает

Server сохраняет `log_level` в mutex, но production-код больше нигде это поле
не читает. Tracing filter создаётся один раз из `RUST_LOG` в `main.rs`, а client
при configuration change только отправляет `didChangeConfiguration`, не
перезапускает server.

Что исправить: использовать reloadable tracing filter либо классифицировать
`logLevel` как restart-required setting и автоматически перезапускать client.

### CODEX-P2-22. Cancellation token допускает потерянное пробуждение

[`OperationCancellationToken::cancelled`](server/crates/php-lsp-server/src/server.rs#L751)
сначала читает atomic flag, а затем создаёт `Notify::notified()` future. Если
`cancel()` выполнится между этими действиями, `notify_waiters()` не сохраняет
permit для ещё не зарегистрированного waiter, и future может ждать следующего
уведомления до внешнего timeout.

Что исправить: создавать/пиновать `notified()` до проверки flag, использовать
`watch`/`CancellationToken` или другой примитив без lost-wakeup; добавить тест,
где token отменён до входа в `cancelled()` и на границе регистрации.

### CODEX-P2-23. PHPDoc способен заменить корректный native type

Комментарий к
[`apply_phpdoc_to_signature`](server/crates/php-lsp-parser/src/symbols.rs#L1457)
говорит о fallback, но каждый `@param` безусловно перезаписывает native
`ParamInfo.type_info` в строках 1468–1476. Ошибочный `@param Foo $id` над
`function f(int $id)` превращает параметр индекса в `Foo` для diagnostics,
signature help, hover и completion.

Дополнительно
[`symbol_effective_return_type`](server/crates/php-lsp-server/src/lsp/inlay_hints.rs#L2368)
выбирает PHPDoc return по абстрактному “specificity score”, а не по совместимости:
`@return Foo` может победить native `int` только потому, что object получает
больше баллов.

Что исправить: хранить native и PHPDoc типы раздельно; использовать PHPDoc как
уточнение только после проверки совместимости, а противоречие диагностировать,
не подменять молча.

### CODEX-P2-24. Квалифицированные PHP-имена иногда считаются абсолютными

[`resolve_type_name_relative_to_symbol`](server/crates/php-lsp-parser/src/resolve.rs#L3641)
считает `App\Foo` абсолютным, если первый segment совпал с первым segment текущей
namespace. В namespace `App\Sub` имя `App\Foo` по правилам PHP является
`App\Sub\App\Foo`, но код возвращает `\App\Foo`. Та же оптимизация повторена в
[`resolve_qualified_type_fqn_from_owner_or_index`](server/crates/php-lsp-server/src/lsp/inlay_hints.rs#L3618).

Следствие — неверные hover/completion/definition/template types без явного
leading `\`. Нужно удалить эвристику namespace root и разрешать qualified names
строго по PHP name-resolution rules.

### CODEX-P2-25. Local definition/type inference имеет неполные scope barriers

[`find_variable_definition_before`](server/crates/php-lsp-parser/src/resolve.rs#L6198)
ищет предыдущие definitions рекурсивно и может выбрать одноимённое assignment из
вложенной closure/function для последующего outer usage. Оно также неполно
обрабатывает by-ref/destructuring assignments.

Server-side [`local_variable_scope_node`](server/crates/php-lsp-server/src/lsp/inlay_hints.rs#L3372)
не считает `arrow_function` и `anonymous_function_creation_expression` корнем
scope. Для переменной внутри arrow поиск начинает с внешнего callable/program, а
внутренний collector затем отсекает сам arrow как boundary — локальный RHS
оказывается невидимым.

Что исправить: единая модель lexical variable scope для definition, rename,
hover и inlay; отдельные правила closure `use` и arrow auto-capture; тесты в обе
стороны каждой вложенной границы.

### CODEX-P2-26. Обычные `name` nodes превращаются в ссылки на global constants

[`push_constant_reference_if_plain_name`](server/crates/php-lsp-parser/src/references.rs#L701)
использует blacklist parent kinds. В нём нет ряда контекстов, включая части
method/constant declarations, attributes и некоторые nullsafe/member формы.
Неисключённое имя записывается как `GlobalConstant` и затем участвует в
references, rename, code lens и unused-import analysis.

Что исправить: заменить blacklist на whitelist реальных constant-expression
AST-ролей; проверять declaration/reference identity и добавить negative matrix
по method names, const elements, named arguments, attributes и nullsafe access.

### CODEX-P2-27. Conditional PHPDoc types разбираются и сопоставляются неверно

В [`parse_type_string`](server/crates/php-lsp-parser/src/phpdoc.rs#L738) union и
intersection разбираются до conditional type. Поэтому top-level `|`/`&` внутри
ветви выражения вроде `($x is Foo ? A|B : C)` может разрезать всю конструкцию до
вызова [`parse_conditional_type`](server/crates/php-lsp-parser/src/phpdoc.rs#L807).

При call-site specialization
[`type_pattern_matches_actual`](server/crates/php-lsp-server/src/lsp/inlay_hints.rs#L2932)
не считает literal string/int/bool экземпляром базового `string`/`int`/`bool`,
поэтому условие `$x is string` для `'value'` выбирает неверную ветвь. Parser-side
`conditional_template_names` также собирает обычные simple type names как будто
они template parameters.

Что исправить: распознавать conditional на корректном precedence level,
передавать реальный набор declared templates и реализовать subtype/literal
matching вместо точного сравнения enum variants.

### CODEX-P2-28. Post-guard `instanceof` narrowing не учитывает булеву логику

[`negative_instanceof_guard_for_var`](server/crates/php-lsp-parser/src/resolve.rs#L2298)
и [`if_then_branch_exits`](server/crates/php-lsp-parser/src/resolve.rs#L2382)
опираются на raw text/substring heuristics. После
`if (!($x instanceof Foo) && $flag) return;` сервер сужает `$x` до `Foo`, хотя
при `$flag === false` выполнение продолжается с любым `$x`. `return`/`throw` в
comments, strings или nested closure также способен повлиять на результат.

Что исправить: анализировать CST boolean expression и exits конкретной ветви;
поддерживать только логически доказанные guards, остальные оставлять unknown.

### CODEX-P2-29. Неизвестный array-shape key получает тип первого поля

[`array_shape_value_type`](server/crates/php-lsp-parser/src/resolve.rs#L5747)
после неудачного поиска key сначала берёт unkeyed item, а затем безусловно
`items.first()`. Поэтому `$shape['missing']` может получить тип первого
существующего поля и открыть ложные member completion/definition.

Кроме того,
[`normalize_shape_key_text`](server/crates/php-lsp-types/src/lib.rs#L220)
безусловно удаляет trailing `?`. Для уже распакованного literal key `"ready?"`
это меняет само имя и смешивает его с `ready`.

Что исправить: при известном отсутствующем key возвращать `None`; fallback
разрешать только для явного unkeyed/open item; optional marker удалять во время
парсинга синтаксиса, а не общей нормализацией значения key.

### CODEX-P2-30. Diagnostic pipeline повторно конвертирует UTF-16 и анализирует не тот snapshot

Parser уже возвращает syntax ranges в UTF-16:
[`parser/diagnostics.rs:8`](server/crates/php-lsp-parser/src/diagnostics.rs#L8).
Server затем трактует их как byte columns и конвертирует второй раз в
[`lsp/diagnostics.rs:1344`](server/crates/php-lsp-server/src/lsp/diagnostics.rs#L1344).
После non-ASCII текста range сдвигается влево.

Внешние PHPStan/Psalm запускаются по файлу на диске, а не по unsaved open
source, но их ranges/messages публикуются на текущую editor version. Наконец,
`has_syntax_errors` в
[`lsp/diagnostics.rs:4761`](server/crates/php-lsp-server/src/lsp/diagnostics.rs#L4761)
считает syntax error любой `php-lsp` diagnostic severity ERROR; если пользователь
повысил semantic category до ERROR, внешние analyzers неожиданно не запускаются.

Что исправить: маркировать coordinate domain в API, не делать вторую конверсию;
анализировать temp snapshot текущего buffer или явно не запускать analyzer на
dirty document; определять syntax errors по code/kind, а не severity/source.

### CODEX-P2-31. Undefined-variable diagnostics не моделируют control flow

Collector объединяет declarations/reads по порядку исходника, а не по CFG.
Assignment в одной ветви может подавить undefined read в несовместимой `else`
или после условного блока. Обратная проблема у `??=`: left operand помечается
только как isset-style probe, а `augmented_assignment_expression` не считается
declaration, поэтому последующий `$value` может остаться “undefined”, хотя
`$value ??= default()` гарантированно инициализирует переменную.

Отдельный arrow-scope defect уже описан в P2-07. Для исправления нужен хотя бы
must/may-defined dataflow по basic blocks и явная семантика `isset`, `empty`,
`??`, `??=`, destructuring и loop/catch variables.

### CODEX-P2-32. Framework/type helpers возвращают уверенные, но неверные типы

[`laravel_model_dynamic_method_return_type`](server/crates/php-lsp-server/src/framework.rs#L2238)
возвращает non-null model и для `find`, и для `first`; реальные Eloquent методы
могут вернуть `null`. То же повторено для relations в
[`framework.rs:2405`](server/crates/php-lsp-server/src/framework.rs#L2405).

Несколько builtin эвристик сравнивают только short function name: custom
namespaced `array_keys`, `array_values` или `preg_match` может получить поведение
глобального builtin. В hover relations
[`hover_direct_method_for_type`](server/crates/php-lsp-server/src/lsp/hover.rs#L1512)
сравнивает `candidate.name == method_name`, хотя PHP method names
case-insensitive, и теряет Implements/Overrides links при иной раскладке.

Что исправить: применять framework rules только после подтверждения owner/FQN,
разделить nullable/non-null terminal methods и использовать kind-aware PHP
casing helpers везде.

### CODEX-P2-33. Blade/Twig conversion повреждает часть корректных выражений

- `@{{ value }}` — стандартная escaped Blade echo — после пропуска `@` всё равно
  распознаётся как `{{ ... }}` в
  [`preprocess_blade_template`](server/crates/php-lsp-server/src/template.rs#L524).
- Twig float `1.5` проходит classifier и в
  [`convert_twig_expression_to_php`](server/crates/php-lsp-server/src/template.rs#L2052)
  превращается в `1->5`, потому что любой `.` считается member access.
- [`push_twig_for_fragment`](server/crates/php-lsp-server/src/template.rs#L1362)
  поддерживает только `for value in items`, но не `for key, value in items`.
- [`apply_text_change`](server/crates/php-lsp-server/src/template.rs#L2534)
  молча игнорирует invalid line, переставляет reversed range через min/max и не
  сообщает вызывающему коду о desync, хотя document version уже меняется.

Что исправить: grammar/token-aware preprocessing, явный result/error incremental
apply и переход в require-full-sync при любой невалидной template edit.

### CODEX-P2-34. Twig include context не соответствует наследованию переменных

[`twig_include_variable_types_for_template_state`](server/crates/php-lsp-server/src/lsp/templates.rs#L3740)
учитывает только include с явным `with { ... }`. Обычный
`{% include 'child.html.twig' %}` наследует caller context в Twig, но здесь не
передаёт ничего. Include `with {...}` без `only` также должен наследовать
остальные caller variables, а код переносит только map.

Open template считается authoritative только если в его текущем тексте всё ещё
найден нужный include (`open_template_uris.insert` выполняется после проверки).
Если unsaved editor удалил include, disk scan снова читает старую дисковую
копию и возвращает уже удалённый context.

Что исправить: хранить `only` flag, объединять inherited + explicit variables и
всегда исключать все open URIs из disk scan независимо от текущего match.

### CODEX-P2-35. Twig context merge теряет типы и безусловно добавляет Symfony globals

[`merge_twig_context_variable_type`](server/crates/php-lsp-server/src/lsp/templates.rs#L409)
игнорирует второй отличный non-null type: для двух render sites `user: User` и
`user: Admin` результат зависит от порядка обхода вместо `User|Admin`.

[`merge_symfony_twig_builtin_variable_types`](server/crates/php-lsp-server/src/lsp/templates.rs#L4364)
без проверки framework всегда добавляет `app`, `error`, `errors` и Symfony FQNs
в любой Twig workspace. Это создаёт ложные completion/hover даже в standalone
Twig или Laravel-проекте.

Что исправить: детерминированно объединять все типы, а framework globals
включать только после подтверждения Symfony/Twig bridge или явной config.

### CODEX-P2-36. Call hierarchy приписывает вызовы вложенных closures внешнему методу

[`collect_outgoing_call_hierarchy`](server/crates/php-lsp-server/src/lsp/hierarchy.rs#L518)
останавливается на nested named function/method, но не на anonymous/arrow
function. Вызовы из closure внутри метода попадают в outgoing calls метода.
Collector также не включает `nullsafe_member_call_expression`.

Incoming path использует receiver-неточный legacy walker (P2-18), поэтому
анонимные callable также получают неправильного containing caller. Нужно ввести
callable boundaries для всех пяти callable forms и строить обе стороны из
resolved reference graph.

### CODEX-P2-37. Completion теряет документацию и оставляет конфликтный short name

При конвертации completion items в
[`lsp/completion.rs:422`](server/crates/php-lsp-server/src/lsp/completion.rs#L422)
копируются label/detail/data, но `documentation` от provider отбрасывается.
Framework virtual item уже содержит Markdown; resolve затем для него может
вернуть item без восстановления, поэтому документация теряется окончательно.

[`build_completion_auto_import_edit`](server/crates/php-lsp-server/src/lsp/completion_helpers.rs#L2763)
при занятом alias просто возвращает `None`, но исходный completion с тем же short
label остаётся. Выбор `Service` при `use Other\Service` вставляет `Service` без
import/alias и фактически ссылается на другой symbol.

Что исправить: сохранять documentation при type conversion; при collision
генерировать безопасный alias + replacement text либо скрывать candidate.

### CODEX-P2-38. Reference UI имеет неверные write roles и квадратичный code lens

[`is_write_reference`](server/crates/php-lsp-server/src/lsp/references.rs#L704)
определяет роль по raw text одной строки. Foreach/catch/destructuring и closure
parameters могут стать READ, а token в необычно отформатированной declaration —
WRITE/READ ошибочно.

`lsp_code_lens` для **каждого** class/method вызывает
`reference_locations_for_symbol` в
[`lsp/references.rs:606`](server/crates/php-lsp-server/src/lsp/references.rs#L606),
то есть повторно сканирует workspace S раз. В файле с сотнями symbols это
O(symbols × all references) прямо в async request.

Что исправить: reference role хранить при CST extraction; code lens считать
одним проходом/group-by target и выполнять workspace-wide работу off-runtime с
cancellation.

### CODEX-P2-39. Symbol/selection responses нарушают реальные namespace и 1:1 contracts

`lsp_document_symbol` ожидает `PhpSymbolKind::Namespace` и хранит только один
`namespace_sym` в
[`document_symbols.rs:449`](server/crates/php-lsp-server/src/lsp/document_symbols.rs#L449),
но production extractor создаёт `NamespaceScope`, а Namespace `SymbolInfo` не
создаёт. Поэтому namespace wrapping branch фактически dead и multi-namespace
document symbols остаются плоскими. Старый unit test искусственно вручную
создаёт Namespace symbol и не проверяет production extraction.

Selection Range должен вернуть один элемент на каждую входную position в том же
порядке. Handler добавляет результат только при успешном построении в
[`document_symbols.rs:343`](server/crates/php-lsp-server/src/lsp/document_symbols.rs#L343);
invalid/unmapped position сокращает массив и сдвигает соответствие.

Что исправить: строить namespaces из `NamespaceScope`; валидировать весь request
или возвращать корректную chain на каждую position без filter-map поведения.

### CODEX-P2-40. Несколько code actions редактируют source по raw-тексту

[`find_visibility_token`](server/crates/php-lsp-server/src/lsp/code_action.rs#L2769)
ищет `public/protected/private` во всём диапазоне declaration, включая method
body. При implicit visibility первым совпадением может стать слово `public` в
строке/comment, и action испортит содержимое вместо modifier.

`remove_unused_import_edit` удаляет полный line span; однострочный group use или
несколько statements на одной строке могут потеряться целиком. Analyzer ignore
вставляется на diagnostic line, не на CST statement/declaration, поэтому marker
может попасть внутрь multiline expression. Implement-missing копирует
source-relative type/default/attribute text из parent namespace в child без
переноса imports; generated signature может ссылаться на другой класс.

Что исправить: edits строить только по CST token ranges, group-use менять на
уровне clause, ignore привязывать к statement, а inherited signatures сначала
нормализовать в FQN и затем безопасно импортировать/сокращать в target scope.

### CODEX-P2-41. `includePaths` разрешают выход из workspace

[`normalize_path`](server/crates/php-lsp-server/src/server.rs#L1674) не схлопывает
`ParentDir`, а сохраняет `..`. Затем
[`collect_php_files`](server/crates/php-lsp-server/src/indexing/workspace.rs#L806)
принимает absolute include path или `root.join("../../outside")` без containment
check и рекурсивно индексирует его.

В отличие от executable settings, project include paths не требуют trust. Таким
образом `.php-lsp.toml` способен заставить server читать PHP за пределами
workspace даже без symlink.

Что исправить: canonical containment для project-provided paths; внешние roots
разрешать только явной trusted client setting с отдельным предупреждением и
собственными exclusions/budgets.

### CODEX-P2-42. Сбор PHP-файлов имеет квадратичную сложность

> **Статус 2026-08-28:** исправлено вместе с `CODEX-P1-01`.

[`push_unique_path`](server/crates/php-lsp-server/src/indexing/workspace.rs#L887)
для каждой вставки линейно просматривает уже накопленный `Vec<PathBuf>` через
`paths.iter().any(...)`. Рекурсивный walker вызывает его для каждого найденного
PHP-файла в
[`collect_php_files_recursive`](server/crates/php-lsp-server/src/indexing/workspace.rs#L843).

Даже без единого дубля для `N` файлов выполняется
`N * (N - 1) / 2` сравнений путей. При 10 000 файлах это **49 995 000**
сравнений. Стоимость одного сравнения также зависит от длины/компонентов пути,
поэтому реальная верхняя граница хуже абстрактного `O(N²)` по числу файлов.

Повторные обходы не являются только теоретическим сценарием:
[`NamespaceMap::source_directories`](server/crates/php-lsp-index/src/composer.rs#L52)
добавляет PSR-4, PSR-0 и classmap directories без дедупликации, а
[`workspace_index_directories`](server/crates/php-lsp-server/src/indexing/workspace.rs#L779)
не удаляет уже присутствующие одинаковые или вложенные roots. Два обхода одного
набора из 10 000 файлов дают суммарно ровно **100 000 000** сравнений, а также
повторяют filesystem traversal. Лексически разные symlink-пути к одному файлу
при этом вообще не считаются дублями.

Путь используется при начальной/полной workspace index discovery
([`workspace.rs:1813`](server/crates/php-lsp-server/src/indexing/workspace.rs#L1813)),
а также CLI `analyze` и `fix`. Обычный incremental update одного файла полный
обход не запускает. Единственный прямой тест collection проверяет лишь
include/exclude correctness на двух файлах
([`server_tests.rs:6640`](server/crates/php-lsp-server/src/server_tests.rs#L6640))
и не имеет scale assertion.

#### Что исправить

- хранить membership в `HashSet<PathBuf>` при сохранении `Vec` для порядка либо
  собирать без линейной проверки и выполнять `sort_unstable` + `dedup` один раз;
- заранее нормализовать и дедуплицировать scan roots, удаляя дочерний root, если
  он уже покрыт родительским;
- не полагаться на лексический `PathBuf` для symlink identity: совместить fix с
  общей canonical containment/non-following policy;
- добавить regression/benchmark на 10–50 тысяч файлов, overlapping Composer
  mappings и ограничение количества path comparisons/directory visits.

#### Реализовано

`push_unique_path` больше не вызывается на каждый найденный PHP-файл.
Filesystem visitor хранит физические file/directory identities в `HashSet` и
группирует aliases через hash maps; overlapping Composer roots прекращают
обход после первого directory identity. Regression на 10 000 файлов получает
10 001 visited entry и 10 001 identity lookup (root + files), без
`N * (N - 1) / 2` сравнений путей. Та же линейная physical-дедупликация
используется при объединении workspace и CLI target lists.

### CODEX-P2-43. Framework string scanner повреждает Unicode

[`next_quoted_string`](server/crates/php-lsp-server/src/framework.rs#L3133)
проходит строку по `as_bytes()` и преобразует каждый byte через `as char`.
Многобайтовый UTF-8 символ поэтому превращается в несколько посторонних Unicode
code points; та же ошибка есть в обработке escaped byte. Результат остаётся
валидным `String`, поэтому повреждение не вызывает panic и незаметно проходит в
Laravel config/translation/route/cast key scanners.

Например, кириллический ключ больше не совпадает с исходным именем, из-за чего
completion/definition молча теряют корректный framework key.

Что исправить: отделить поиск ASCII delimiter/escape по byte offsets от
копирования значения, переносить целые UTF-8 slices либо идти по
`char_indices()`, сохраняя byte ranges; добавить Unicode + escaped-quote tests
для каждого потребителя.

### CODEX-P2-44. Composer resolver нарушает правила выбора autoload path

[`NamespaceMap::resolve_class_to_paths`](server/crates/php-lsp-index/src/composer.rs#L28)
добавляет пути для **всех** PSR-4 prefixes, являющихся строковым prefix FQN.
При `App\\ -> app/` и `App\\Tests\\ -> tests/` класс `App\\Tests\\Foo`
получает как неверный `app/Tests/Foo.php`, так и корректный `tests/Foo.php`.
Порядок записей пришёл из `HashMap`, поэтому первый существующий candidate не
является стабильным longest-prefix result. Сам API также не срезает ведущий
`\\`.

Lazy vendor fallback в
[`lazy_index_class_with_context`](server/crates/php-lsp-server/src/indexing/vendor.rs#L194)
запускается только когда workspace candidate list пуст. Если неверный или
устаревший workspace path был построен, но requested type там не найден, vendor
map уже не проверяется. Vendor resolver повторяет raw prefix matching и для
malformed prefix способен построить `.php` из пустого relative class.

Что исправить: нормализовать FQN, выбирать все directories только самого
длинного валидного PSR-4 prefix, отдельно реализовать PSR-0 rules, валидировать
границы prefix и запускать vendor fallback после фактического miss по requested
type, а не только после пустого списка путей.

### CODEX-P2-45. Position APIs повторно смешивают byte и UTF-16 координаты

Server callers заранее преобразуют LSP character в tree-sitter byte column и
передают его в `infer_variable_type_*_at_position`. Внутри
[`infer_variable_type_at_position_internal`](server/crates/php-lsp-parser/src/resolve.rs#L524)
то же значение корректно используется в `Point::new`, но затем
[`position_to_byte`](server/crates/php-lsp-parser/src/resolve.rs#L5776)
повторно трактует его как UTF-16 column через `utf16_col_to_byte`. После Unicode
текста usage offset смещается, поэтому completion/definition/type inference
могут анализировать не ту часть expression.

Общий server helper
[`lsp_position_to_byte`](server/crates/php-lsp-server/src/util/lsp_text.rs#L40)
дополнительно принимает несуществующую строку после EOF для source без trailing
newline: `position.line == source.lines().count()` возвращает `source.len()`.

Что исправить: типизировать/переименовать API как byte-column либо LSP-position,
убрать повторную конвертацию, разрешать EOF pseudo-line только при реальном
trailing newline и добавить Unicode/CRLF/nonexistent-line tests на всех
границах.

### CODEX-P2-46. Type/name identity и отображение применяют разные правила

- [`is_builtin_or_relative_class_name`](server/crates/php-lsp-parser/src/references.rs#L760)
  сравнивает `string/int/bool/...` регистрозависимо; допустимые `INT`/`String`
  могут превратиться в class references;
- [`symbol_fqn_eq`](server/crates/php-lsp-types/src/lib.rs#L540) сравнивает
  namespace exact-case, хотя namespace segments в PHP case-insensitive;
- [`WorkspaceIndex::search`](server/crates/php-lsp-index/src/workspace.rs#L563)
  использует Unicode `to_lowercase`, тогда как остальные PHP lookup keys
  намеренно ASCII-normalized;
- [`is_phpdoc_builtin_type`](server/crates/php-lsp-index/src/workspace.rs#L1304)
  не сохраняет распространённые pseudo-types вроде `array-key`,
  `non-empty-string`, `positive-int` и `non-empty-array`; alias expansion может
  квалифицировать их как классы текущей namespace.

Что исправить: единый classifier/normalizer типов и FQN с kind-specific casing,
полная таблица поддержанных PHPDoc pseudo-types и общие tests для регистра
builtins/namespaces/search.

### CODEX-P2-47. Completion context использует текст вне курсора и угадывает receiver

[`check_use_context`](server/crates/php-lsp-completion/src/context.rs#L427) при
multiline use fallback'ится на полный text CST node, включая clauses после
курсора. [`use_statement_prefix`](server/crates/php-lsp-completion/src/context.rs#L446)
не разбирает `as` alias и group-use braces, поэтому prefix загрязняется чужим
текстом.

Дополнительно:

- `prop++`/`prop--` считаются только Read в
  [`member_access_mode_after_cursor`](server/crates/php-lsp-completion/src/context.rs#L245),
  что нарушает фильтрацию `@property-read`/`@property-write`;
- если incomplete CST не дал object перед `->`,
  [`check_member_access`](server/crates/php-lsp-completion/src/context.rs#L202)
  без проверки method/static/global scope подставляет `$this`.

Что исправить: строить use prefix из активной CST clause, всегда обрезанной по
cursor offset; моделировать ReadWrite; разрешать `$this` fallback только в
non-static method текущего класса и добавить multiline group/alias/increment/
malformed-scope tests.

### CODEX-P2-48. Malformed target URI создаёт правдоподобную definition в чужом файле

При ошибке parsing target URI ветка class definition в
[`lsp_definition`](server/crates/php-lsp-server/src/lsp/definition.rs#L693)
подставляет URI текущего документа через `unwrap_or_else(|_| uri.clone())`, но
сохраняет range целевого symbol. Клиент получает Location, выглядящую валидной,
но указывающую на произвольный диапазон текущего файла.

Что исправить: malformed target candidate пропускать/возвращать `None` и
логировать причину; legacy/cache URI должен инвалидировать источник, а не
маскироваться текущим URI.

### CODEX-P2-49. Vendor LRU может удалить заново загруженное поколение файла

[`touch_vendor_file_lru`](server/crates/php-lsp-server/src/indexing/workspace.rs#L1614)
под lock обновляет LRU и получает список evicted URI, затем отпускает lock и
вызывает `index.remove_file`. Между этими действиями конкурентный lazy lookup
может снова загрузить тот же URI и вернуть его в индекс; старый eviction после
этого удалит уже новое поколение, а LRU и index разойдутся.

Что исправить: хранить generation/token в LRU entry и делать conditional remove
только ожидаемого поколения либо объединить publication/eviction в одну
транзакцию с явным lock order; добавить barrier-test на reload между `touch` и
`remove_file`.

### CODEX-P2-50. Composer/cache metadata может остаться свежей после смены содержимого

[`vendor_cache_hash`](server/crates/php-lsp-server/src/indexing/cache.rs#L177)
включает для `composer.json`, lock и generated maps только размер и mtime.
Перезапись тем же размером с сохранённым timestamp оставляет прежний config
hash. Per-PHP-file content hashes не защищают изменение **состава** autoload
files/directories, поэтому cache способен принять старую конфигурацию.

При выборе одного из нескольких nested Composer roots
[`find_composer_json`](server/crates/php-lsp-server/src/indexing/workspace.rs#L1423)
ищет raw substrings `"autoload"`/`"psr-4"` в любом JSON string и возвращает
первый candidate из filesystem order. Description/script может дать false
positive и выбрать другой проект.

Что исправить: хэшировать содержимое малых metadata files, parse JSON sections
структурно, детерминированно ранжировать candidates и включить выбранную
autoload model в cache identity.

### CODEX-P2-51. Activation promise VS Code extension может остаться необработанным

Activation запускает
[`enqueueLanguageClientReconciliation`](client/src/extension.ts#L1177) через
`void` без `.catch`. [`LifecycleCoordinator::enqueue`](client/src/lifecycle.ts#L62)
после logging повторно бросает ошибку и возвращает rejected `run`; внутренняя
queue восстанавливается, но внешний fire-and-forget promise способен дать
unhandled rejection при ошибке notification/callback/lifecycle operation.

Что исправить: завершать activation fire-and-forget явным `.catch(...)` с
status/error log; добавить тест, где reconciliation callback бросает, а очередь
после этого остаётся работоспособной без unhandled rejection.

## P3 — инфраструктура и сопровождаемость

### CODEX-P3-01. Часть клиентских проверок не входит в lint/CI

В [`client/package.json:398`](client/package.json#L398) определены
`check:cache-path` и `check:commands`, но строка `lint` их не запускает.
Обе проверки при аудите прошли, однако будущая регрессия не остановит CI.

#### Что исправить

Добавить обе команды в `npm run lint` или отдельные CI steps.

### CODEX-P3-02. Нет PR-проверок Windows/macOS и MSRV

Основной CI работает только на Ubuntu и Rust stable:
[`ci.yml:12`](.github/workflows/ci.yml#L12).

Cross-platform ветви URI, shell commands, executable detection и process
termination фактически проверяются только при release build после создания
tag.

#### Что исправить

- Windows smoke test для client/process/path logic;
- macOS compile/smoke check;
- отдельная сборка на заявленном Rust 1.85;
- оставить stable Clippy как дополнительную проверку;
- добавить `cargo audit`/`cargo deny` policy.

### CODEX-P3-03. Release workflow допускает ошибочную или разрушительную публикацию

[`Makefile:176`](Makefile#L176):

- semver regex не закреплён концом строки;
- ошибка `cargo update` скрывается через `|| true`;
- существующий tag force-перезаписывается;
- tag force-push отправляется в remote;
- версия tag не сверяется с `VERSION`, Cargo и package.json;
- `cross` и `vsce` устанавливаются без точной версии.

#### Что исправить

- строгая проверка `^[0-9]+\.[0-9]+\.[0-9]+$`;
- прекращать release при ошибке lockfile update;
- запрещать перезапись существующего tag;
- проверять согласованность всех четырёх версий;
- pin tool versions и GitHub Actions к commit SHA;
- по возможности формировать draft release до marketplace publish.

### CODEX-P3-04. Репозиторная гигиена и размер модулей

В Git отслеживается Python bytecode:
`scripts/__pycache__/audit-lsp-workspace.cpython-312.pyc`. В `.gitignore` нет
общих правил для `__pycache__` и `*.py[cod]`.

Крупнейшие production modules:

| Файл | Строк |
|---|---:|
| `lsp/code_action.rs` | 6701 |
| `parser/resolve.rs` | 6558 |
| `lsp/diagnostics.rs` | 4888 |
| `lsp/templates.rs` | 4646 |
| `lsp/inlay_hints.rs` | 4364 |

#### Что исправить

- удалить tracked `.pyc` и расширить `.gitignore`;
- добавить `shellcheck` и Python lint/compile checks;
- дальше делить модули по feature/domain, не смешивая с behavior fixes;
- вынести общие type-resolution и filesystem traversal policies в небольшие
  тестируемые компоненты.

### CODEX-P3-05. Несколько тяжёлых операций всё ещё выполняются на Tokio runtime

Наиболее существенные места:

- `index_workspace` синхронно вызывает cache load, повторное хеширование всех
  файлов, bincode serialization, write и `fsync`;
- [`open_document_snapshot_from_state`](server/crates/php-lsp-server/src/server.rs#L131)
  на каждом hover/completion/definition клонирует Tree/Rope source и заново
  запускает полный `extract_file_symbols`;
- lightweight freshness check diagnostics вызывает тот же тяжёлый snapshot и
  повторяет extraction до четырёх раз на одну публикацию; сбор references при
  commit выполняется ещё до проверки актуальности поколения;
- code actions строят refactors и при необходимости diagnostics прямо в async
  handler;
- folding, document links, semantic tokens и document symbols выполняют AST
  walk/parse в async handler, часть из них удерживает DashMap guard;
- incoming call hierarchy последовательно читает и парсит все indexed files.

Это не только performance debt: long-running runtime work задерживает
didChange, cancellation и быстрые interactive requests.

Что улучшить: versioned `OpenDocumentSnapshot` с готовыми symbols/references,
off-runtime cache load/save, bounded parallel source reads и cancellation в
workspace-wide requests.

### CODEX-P3-06. Semantic Tokens имеют ошибки классификации

- [`symbol_modifier_bits`](server/crates/php-lsp-parser/src/semantic_tokens.rs#L579)
  ищет `static`, `abstract`, `readonly` во всём тексте declaration. Обычный
  instance method с `static::foo()` в body получает modifier `static`.
- nullsafe member call/access отсутствуют в части classifier branches;
- `use function` и `use const` names классифицируются как type;
- отсутствует ряд PHP operators (`?->`, `**`, shifts, spaceship и PHP 8.5
  pipe);
- целый `encapsed_string` становится string token, поэтому вложенные variable
  nodes больше не обходятся.

Что улучшить: определять modifiers только по direct modifier children и
добавить semantic-token matrix по всем CST operator/member/import kinds.

### CODEX-P3-07. Built-in diagnostics намеренно имеют слишком широкие blind spots

Подтверждённые примеры из кода:

- [`check_use_statements`](server/crates/php-lsp-parser/src/semantic.rs#L111)
  подавляет любой unresolved aliased class import, даже обычную опечатку;
- [`should_check_class`](server/crates/php-lsp-parser/src/semantic.rs#L564)
  не проверяет ни один single-segment/global class name;
- PHPDoc usage scanner ищет `/**` raw-текстом по source, включая строки, а
  absolute `\Foo\Bar` может ошибочно засчитать import `Foo` использованным;
- [`method_has_override_attribute`](server/crates/php-lsp-parser/src/semantic.rs#L1532)
  ищет `#[Override` во всём method text, включая body/string/comment;
- [`if_then_branch_exits`](server/crates/php-lsp-parser/src/resolve.rs#L2382)
  делит raw source по подстроке `else` и ищет `return/throw/exit/die` внутри
  строк/comments, поэтому post-guard type narrowing получает ложные exits;
- foreach declaration helper ищет первый `{`/`:` raw-текстом; символ внутри
  string/выражения header может сломать классификацию переменной;
- by-ref output suppression считает любой short-name `preg_match` built-in,
  даже если namespaced project function shadowed it.

Эти эвристики снижают false positives, но создают плохо видимые false
negatives. Их нужно заменить CST/identity-aware проверками и явно
документировать оставшиеся ограничения режима `basic-semantic`.

### CODEX-P3-08. Template preprocessing имеет boundary и delimiter gaps

- [`TemplateSourceMap::original_to_virtual`](server/crates/php-lsp-server/src/template.rs#L325)
  использует inclusive end, тогда как virtual containment — half-open. На
  границе соседних segments может быть выбран предыдущий wrapper segment.
- Blade `{{ ... }}` и `{!! ... !!}` ищут close delimiter обычным `find`, не
  учитывая quoted `"}}"`/`'!!}'` внутри expression.
- template path lookup и Twig context walkers следуют symlink, включая ссылки
  за workspace root.
- при количестве открытых Twig документов больше refresh limit всегда
  обслуживаются первые URI после сортировки; остальные могут постоянно
  голодать.

Что улучшить: единая half-open source-map модель, quote-aware Blade delimiter
scanner, общий no-follow visitor и fair/rotating Twig refresh queue.

### CODEX-P3-09. CLI и локальные formatter/link helpers имеют неожиданные edge cases

- неизвестная top-level CLI-команда попадает в `_ => false`, после чего процесс
  запускает LSP и выглядит зависшим вместо exit 2;
- `init-config --path` без значения молча создаёт default path;
- CLI `fix` не выполняет ту же lazy vendor pre-resolution, что `analyze`;
- fix converter игнорирует `WorkspaceEdit.document_changes`;
- CLI `analyze`/`fix` применяют `String::from_utf8_lossy`; offsets и fixes
  строятся по изменённому source при невалидном UTF-8 вместо явной encoding
  error;
- две zero-width вставки в одной позиции не считаются конфликтом, а их порядок
  после сортировки/обратного применения не определён контрактом;
- on-type formatter считает braces внутри strings/comments;
- document link для `dirname(__FILE__, 2)` игнорирует второй аргумент;
- cache `top_level` сериализуется, но production load его не использует;
- при ошибке temp cache write частичный `.tmp` не очищается.

Это небольшие отдельные дефекты, но для CLI/CI tooling они заметно ухудшают
предсказуемость. Нужна отдельная edge-case test matrix.

### CODEX-P3-10. Нет общего ограничения глубины AST и размера source/output

Большинство parser/server walkers рекурсивны. Глубоко вложенный корректный или
malformed PHP способен исчерпать worker stack; 8 MiB stack только повышает
порог. Source files, analyzer output и некоторые cache/template buffers также
читаются целиком без единой quota policy.

Что улучшить: iterative walkers или depth budget на недоверенных AST,
максимальный source/cache/subprocess-output size, partial/degraded result и
стресс-тесты с generated nesting.

### CODEX-P3-11. Cross-platform и file-operation границы недостаточно согласованы

- client и Rust по-разному canonicalize Windows paths для cache hash; Rust
  может получить extended `\\?\` path, поэтому Clear Cache способен удалить не
  тот каталог, который использует server;
- client регистрирует несколько пересекающихся watchers: `**/*.php` уже
  включает часть специальных `vendor/composer/*.php` patterns;
- file-operation registration принимает только `File` с glob `**/*.php`;
  rename/delete целой директории не очищает descendants из index;
- PHP extension при recursive collection сравнивается case-sensitive (`php`),
  хотя URI helper проверяет расширение case-insensitive.

Что улучшить: shared cache-path fixtures для Windows, non-overlapping watchers,
folder operation handling и единая case policy для extension detection.

### CODEX-P3-12. Twig disk cache не инвалидируется при появлении нового caller

[`TwigContextDiskCache::evict_entries_for_source_uri`](server/crates/php-lsp-server/src/server.rs#L1525)
удаляет только cache entries, в которых source URI уже присутствовал среди
результатов. Если PHP-файл раньше не render-ил template, а новая unsaved правка
добавила первый `render('target.twig', ...)`, URI отсутствует в старом value и
cache остаётся “свежим”; новый context не появляется.

Cache key содержит только root + template name и не содержит index generation,
file list/content hashes или config. Нужен dependency/reverse index либо
консервативная инвалидация всех template entries root при изменении потенциального
PHP caller.

### CODEX-P3-13. Framework cache слишком короткоживущий, а scanners повторяют raw work

Completion helpers создают новый `FrameworkProviderCache::default()` при каждом
exact/candidate обращении:
[`completion_helpers.rs:135`](server/crates/php-lsp-server/src/lsp/completion_helpers.rs#L135)
и [`completion_helpers.rs:181`](server/crates/php-lsp-server/src/lsp/completion_helpers.rs#L181).
Кэш приносит пользу внутри одного вызова, но не между соседними hover/completion
requests.

Laravel macro и часть metadata providers синхронно перечитывают/перепарсивают
многие indexed files, а ряд route/model/form scanners анализирует raw strings.
Стоит сделать generation-aware shared caches на уровне workspace и
инвалидировать их по file update, одновременно заменяя raw scanners на CST.

### CODEX-P3-14. Часть тестов закрепляет approximation или пропускает реальную поломку

Полный проход по всем Rust unit-, integration- и E2E-тестам выявил характерные
пробелы:

- `test_type_compatibility_approximation_rules_are_explicit` ожидает, что
  значение типа `A` совместимо с `A&B`, тем самым закрепляя неверную
  intersection-семантику;
- PHPStan test ожидает single-file fallback даже когда единственный ключ —
  другой файл;
- базовый `test_goto_definition` проверяет URI только внутри `if !result.is_null()`,
  поэтому `null` проходит тест;
- важные stubs/vendor tests делают early return, если submodule/bundle не
  подготовлен, и CI может зелёно пропустить feature целиком;
- cancellation test отменяет command спустя 50 ms и не проверяет lost-wakeup до
  регистрации waiter;
- constructor/refactor happy-path tests не покрывают parent constructor,
  side-effecting RHS, short-circuit и loop contexts.

Что улучшить: negative regression suite для каждой находки, fail/explicit skip
policy для обязательных assets и запрет условных assertions, допускающих null.

### CODEX-P3-15. Version/resource configuration валидируется неполно

[`PhpVersion::parse`](server/crates/php-lsp-server/src/server.rs#L841) читает два
segment и игнорирует остальные: `8.2.garbage` принимается как 8.2. Formatter и
analyzer timeouts имеют минимум, но не максимум; diagnostic budget `0` снимает
ограничение; `PHP_LSP_WORKER_THREAD_STACK_SIZE` принимает произвольно большое
`usize` в [`main.rs:55`](server/crates/php-lsp-server/src/main.rs#L55) и способен
сорвать создание runtime.

Также Type Hierarchy объявлен только через `experimental`, хотя requests
реализованы; клиенты, ожидающие стандартную capability, могут его не включить.
Нужна строгая schema/semantic validation с диапазонами, предупреждением клиенту
и безопасными верхними пределами ресурсов.

### CODEX-P3-16. Несколько проходов остаются superlinear или повторяют одну работу

Помимо O(N²) file collection и code lens:

- [`check_unused_imports`](server/crates/php-lsp-parser/src/semantic.rs#L615)
  обходит CST заново для каждого import;
- multiline semantic token для каждой строки вызывает
  [`line_byte_len`](server/crates/php-lsp-parser/src/semantic_tokens.rs#L195),
  который каждый раз начинает `split(...).nth(line)` от начала source;
- удаление каждого top-level symbol ищет replacement полным проходом по всем
  [`file_symbols`](server/crates/php-lsp-index/src/workspace.rs#L318);
- [`discover_stub_extensions`](server/crates/php-lsp-index/src/stubs.rs#L130)
  полностью собирает файлы каждой extension лишь для проверки непустоты, после
  чего выбранные extensions обходятся снова;
- namespace completion сканирует весь type index даже для пустого prefix, а
  PHPDoc virtual member details повторно парсятся на запрос;
- vendor autoload cache не имеет single-flight/negative entry, поэтому
  concurrent miss повторно читает одни metadata.

Что улучшить: single-pass usage/token indexes, reverse ownership map для
top-level symbols, bounded `has_any_php_file`, prefix index/metadata snapshot и
per-vendor shared in-flight future. Добавить счётчики фактических visits/parses,
а не только wall-clock benchmark.

### CODEX-P3-17. Ошибки и отмена фоновой индексации отображаются как успех

Начальная загрузка stubs
([`workspace.rs:215`](server/crates/php-lsp-server/src/indexing/workspace.rs#L215))
и reload
([`server.rs:2482`](server/crates/php-lsp-server/src/server.rs#L2482))
превращают `JoinError`/panic `spawn_blocking` в `0` через `unwrap_or(0)`, после
чего публикуют успешный статус `Loaded 0`/`Reloaded 0`.

После `WorkDoneProgress::begin` некоторые cancellation returns в
[`index_workspace`](server/crates/php-lsp-server/src/indexing/workspace.rs#L1963)
выходят без явного `finish`, поэтому клиентский progress может остаться
активным; `abort_all` при этом не отменяет уже стартовавший `spawn_blocking`.

Что исправить: общий join-error handler с phase `error`, progress guard с
обязательным `end` для success/error/cancel и отдельный cooperative token для
blocking parse tasks.

### CODEX-P3-18. Status UI VS Code синхронно сканирует файловую систему

Каждое indexing-status notification вызывает `render`, а
[`render`](client/src/extension.ts#L263) синхронно строит полный
[`getExtensionSnapshot`](client/src/extension.ts#L405): ищет Composer roots,
cache directories, server binary и сканирует `PATH` через `existsSync`,
`statSync`, `readdirSync` и `readFileSync`. Частые progress notifications могут
подвешивать extension host.

Связанные подтверждённые проблемы запуска/портируемости:

- relative `phpLsp.serverPath` в
  [`resolveServerBinary`](client/src/extension.ts#L718) зависит от случайного CWD
  extension host;
- termination fallback читает private `_serverProcess` language client в
  [`managedServerProcess`](client/src/extension.ts#L523), что ломается при
  изменении внутренностей зависимости;
- [`cacheBaseDir`](client/src/cachePath.ts#L5) и Rust server учитывают только
  `XDG_CACHE_HOME`/`HOME`, игнорируя нормальную Windows home/cache location и
  падая в temp.

Что улучшить: кэшировать immutable snapshot до config/workspace change,
обновлять на progress только поля статуса, требовать/стабильно разрешать custom
server path, сохранять собственный public child handle и синхронно внедрить
platform cache policy в клиент и сервер.

### CODEX-P3-19. `path_to_uri` не схлопывает parent segments

[`path_to_uri`](server/crates/php-lsp-types/src/uri.rs#L36) делает путь
абсолютным, но используемая версия `url` сохраняет `..`:
`/tmp/../foo.php` превращается в `file:///tmp/../foo.php`. Один физический файл
может получить несколько URI и разные index/cache keys.

Исходное утверждение DeepSeek о том, что `file://localhost/path` отвергается на
Unix, отдельно проверено и **не подтверждено**: текущий `Url::to_file_path`
успешно возвращает `/path`, поэтому этот подпункт в находку не включён.

Что исправить: выполнять platform-aware lexical normalization без обязательного
разрешения symlink и добавить Unix/Windows round-trip tests для dot/parent
segments, localhost, UNC и percent encoding.

## Дополнительные улучшения

### Framework completion parity

Некоторые framework providers умеют подавлять ложные diagnostics для
динамических Symfony/Laravel members, но не перечисляют те же API через
`virtual_member_candidates`. Из-за этого допустимый вызов распознаётся после
ввода, но completion его заранее не предлагает.

Рекомендуется выровнять exact lookup и candidate enumeration для безопасного
набора известных helpers/scopes/properties.

### Точность audit harness

Протокольный аудит нашёл 13 completion label misses. После проверки часть из
них оказалась ожидаемыми unknown-symbol probes, а не дефектами сервера. Скрипту
стоит различать:

- известный symbol/member, который обязан присутствовать;
- framework dynamic candidate;
- намеренно неизвестное имя из diagnostic fixture.

Это уменьшит шум и позволит безопасно включить строгий completion audit в CI.

### Ограничение размера исходных файлов

Workspace indexing, CLI и некоторые template paths читают PHP-файлы целиком.
Стоит ввести документированный максимальный размер source file или режим
degraded parsing для очень больших файлов, чтобы случайный generated PHP не
приводил к резкому росту памяти и latency.

## Положительные результаты

- Основные byte-column/UTF-16 преобразования централизованы и хорошо покрыты.
- File URI создаются через общие helpers, включая percent encoding.
- Project commands не исполняются без явного trust gate.
- Открытые документы защищены от перезаписи disk snapshot во многих критических
  путях.
- Rename member references использует resolved receiver safety.
- Stubs walker уже имеет корректную non-following symlink policy; её можно
  переиспользовать для остальных обходов.
- Analyzer parsing вынесен с async runtime threads.
- Клиент имеет bounded restart policy и TERM/KILL escalation.
- Тестовая база охватывает Unicode, incremental edits, templates, diagnostics,
  references, rename и lazy vendor resolution.

## Рекомендуемый порядок работ

### Этап 1 — безопасность и красный CI

1. Исправить scope-safe rename локальных переменных.
2. Объединить symlink policy и закрыть containment workspace/vendor paths.
3. Закрыть auto formatter через Workspace Trust и обновить уязвимые npm
   dependencies.
4. Разделить runtime configuration по workspace roots.
5. Сделать reindex/lazy vendor publication generation-safe и устранить
   нестабильный переход в `ready`.
6. Ограничить extract/inline безопасными преобразованиями и учитывать parent
   constructor при генерации.
7. Устранить N×M Twig context scan и сделать blocking traversal отменяемым.
8. Добавить dependency/security checks в CI.

### Этап 2 — совместимость и устойчивость

1. Добавить PHP 8.5 grammar/config/stubs/tests.
2. Ограничить cache и external command memory usage.
3. Исправить lost-wakeup cancellation и унифицировать lifetime/generation
   фоновых операций.
4. Исправить UTF-16/range contracts built-in diagnostics, PHPStan и formatter.
5. Перейти на authoritative Composer autoload maps либо полностью реализовать
   правила PSR-0/PSR-4/files/classmap.
6. Закрыть scope/CFG-пробелы local definition, undefined-variable и narrowing.

### Этап 3 — качество типов и completion

1. Сохранить composite `TypeInfo`, native-signature precedence и array-shape
   semantics до feature-specific resolution.
2. Добавить clone inference и исправить qualified PHP names/conditional PHPDoc.
3. Выровнять framework diagnostics, hover и completion candidates.
4. Исправить template context/source mapping и call/symbol hierarchy contracts.
5. Добавить каждый подтверждённый случай в строгий regression baseline.

### Этап 4 — инженерная инфраструктура

1. Усилить release validation.
2. Добавить Windows/macOS/MSRV jobs.
3. Подключить все существующие client checks.
4. Почистить generated artifacts и постепенно разделить крупные модули.

## Результаты проверок

| Проверка | Результат |
|---|---|
| `cargo fmt --all --check` | PASS |
| `cargo clippy --all-targets -- -D warnings` | PASS |
| Полный `make check` | FAIL: flaky vendor inherited definition |
| Все Rust tests с исключённым flaky test | PASS |
| Flaky test, 15 изолированных запусков | 13 PASS / 2 FAIL |
| Первый `cargo test --all` после полного чтения | FAIL: timeout ожидания indexing `ready` |
| Упавший diagnostics E2E, 20 изолированных запусков | 20 PASS / 0 FAIL |
| Повторный `cargo test --all` | PASS |
| Client `npm run lint` | PASS |
| Client `npm run build` | PASS |
| `check:cache-path` | PASS |
| `check:commands` | PASS |
| LSP audit, 20 PHP files | Завершён, request errors: 0 |
| Definition probes | 64, expected misses: 0 |
| Completion probes | 67, подтверждены composite/clone gaps |
| `npm audit --omit=dev` | FAIL: 2 high vulnerabilities |
| RustSec `cargo audit` | 0 vulnerabilities, 1 unmaintained warning |
| Symlink escape reproduction | CONFIRMED |
| PHP 8.5 syntax reproduction | CONFIRMED |
| Arrow-function shadowing diagnostic | CONFIRMED: ошибочно 0 diagnostics |
| Полное линейное покрытие Rust | 107/107 файлов, 122 884/122 884 строки; журнал с SHA-256 |
| Production Rust subset | 69 046 строк, все 59 first-party файлов включены |
| DeepSeek post-audit reconciliation | 945/945 строк, все 110 numbered findings проверены |
| Finding groups | 82: P1=12, P2=51, P3=19 |
| `git diff --check` | PASS |

## Итог

Проект имеет хорошую основу и значительный объём защитных тестов, но следующий
релиз желательно не выпускать до устранения P1-группы. Наиболее опасная новая
находка глубокого прохода — destructive local rename через nested callable
scope. Полный линейный проход добавил не менее серьёзные проблемы: stale reindex
publication, меняющие семантику refactors, небезопасная генерация child
constructor, выход vendor metadata за containment и комбинаторный Twig scan.
Вместе с dependency security, multi-root isolation, Workspace Trust и PHP 8.5
это формирует минимальный release-blocking набор. Следом стоит исправить
атомарность snapshots, Composer resolution, Unicode/position contracts,
ограничения ресурсов и сохранение точных типов. Сверка DeepSeek дополнительно
подтвердила, что часть проблем масштабирования и клиентского lifecycle была
недостаточно явно представлена в исходной версии отчёта.

---

*Отчёт подготовлен Codex (OpenAI). Runtime-код проекта в рамках аудита не
изменялся.*
