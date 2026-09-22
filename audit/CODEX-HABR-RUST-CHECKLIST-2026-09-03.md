# Codex: проверка php-lsp по Rust-чек-листу статьи Habr 1035712

> Автор проверки: **Codex**  
> Дата: 2026-09-03  
> Источник: [«Я заставил LLM писать Rust полгода. Вот что они стабильно ломают»](https://habr.com/ru/articles/1035712/)  
> Снимок проекта: текущий рабочий каталог на момент проверки

## Итог

Семь категорий из статьи проверены на текущем first-party Rust-коде проекта.
Подтверждены четыре конкретных проблемы, все в области async cancellation и
RAII вокруг внешних побочных эффектов:

1. **High:** timeout/cancellation внешней команды не гарантирует завершение
   дерева процессов и полное reap дочернего процесса;
2. **Medium:** общий timeout над `spawn_blocking` не останавливает уже начатую
   работу, включая операции записи;
3. **Medium:** результат динамической регистрации внешних watcher'ов может
   стать неизвестным серверу после timeout/cancellation;
4. **Low:** временный каталог formatter не имеет RAII-владельца и может
   остаться после отмены или timeout записи.

Первые два пункта уже входят в более широкий открытый `CODEX-P2-05` основного
аудита. Третий пункт — отдельный найденный пробел. Четвёртый ранее упоминался в
аудитах как hardening, но остаётся актуальным.

Категории `unsafe`, lifetime laundering, ручные `Send`/`Sync`, публичные
blanket impl и большие стековые массивы в текущем коде **не подтверждены**.

## Покрытие

- Прочитаны целиком **114 из 114 Rust-файлов** проекта: production, unit,
  integration и E2E — **136 281 строка**.
- Отдельно перечислены и проверены все first-party вхождения:
  `unsafe`/`transmute`/raw-pointer operations, явные lifetime-параметры,
  `std::sync::Mutex`, `tokio::sync::{Mutex, RwLock}`, `Drop`, `tokio::spawn`,
  `spawn_blocking`, `select!`, `timeout`, generic `impl` и repeat-array
  expressions.
- Third-party исходники из Cargo registry не входят в область проверки;
  проверялись способы использования зависимостей проектом.
- Выполнен Clippy для всех targets с дополнительными обязательными lint:
  `clippy::await_holding_lock` и `clippy::large_stack_arrays`; замечаний нет.

## Подтверждённые проблемы

### HABR-CODEX-P1-01. Cancellation внешней команды не завершает всё дерево процессов

**Категории статьи:** Drop/RAII, async cancellation.  
**Приоритет:** High.  
**Пересечение с основным аудитом:** `CODEX-P2-05`.

[`run_shell_command_with_timeout`](../server/crates/php-lsp-server/src/lsp/external_command.rs#L13)
запускает пользовательскую командную строку через `sh -c` или `cmd /C`, включает
`kill_on_drop(true)`, превращает `Child` в `wait_with_output` и при timeout либо
cancellation немедленно возвращает ошибку
([`external_command.rs:20-63`](../server/crates/php-lsp-server/src/lsp/external_command.rs#L20)).

У этого поведения два незащищённых края:

- `kill_on_drop` относится к непосредственному процессу оболочки, а не является
  кроссплатформенной гарантией завершения всех порождённых ею процессов;
- при drop `Child` Tokio лишь инициирует kill и выполняет reap на best-effort
  основе. Код не вызывает явный `kill().await` + `wait().await` после timeout.

Поэтому PHPStan, Psalm или formatter, породивший дополнительный процесс, может
продолжить работу после сообщения `cancelled`/`timed out`; на Unix также нет
строгой гарантии своевременного reap. Это согласуется с официальными
[оговорками Tokio для `kill_on_drop`](https://docs.rs/tokio/latest/tokio/process/struct.Command.html#method.kill_on_drop).

#### Что исправить

- На Unix запускать команду в отдельной process group и завершать всю группу;
  на Windows использовать Job Object с kill-on-close.
- После timeout/cancellation явно инициировать завершение и дождаться reap.
- По возможности запускать известный executable с аргументами без промежуточной
  shell; shell-mode оставить только для явно доверенной произвольной команды.
- Добавить regression с shell, который порождает долгоживущего потомка: после
  cancel/timeout оба PID должны исчезнуть.

### HABR-CODEX-P2-01. Timeout над `spawn_blocking` не отменяет работу и её side effects

**Категория статьи:** async cancellation.  
**Приоритет:** Medium.  
**Пересечение с основным аудитом:** `CODEX-P2-05`.

Общий helper [`run_file_io_blocking`](../server/crates/php-lsp-server/src/server.rs#L959)
создаёт `spawn_blocking(op)` и оборачивает только ожидание `JoinHandle` в
15-секундный timeout
([`server.rs:968-983`](../server/crates/php-lsp-server/src/server.rs#L968)).
После timeout handle дропается, однако начавшая выполняться closure продолжает
работу. Это прямо зафиксировано в
[документации Tokio `spawn_blocking`](https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html):
начатую blocking task нельзя abort, а shutdown runtime ожидает такие задачи.

Для чистого чтения это в основном лишний расход ресурсов. Но helper также
используется для побочных эффектов. Например,
[`save_vendor_index_cache_blocking`](../server/crates/php-lsp-server/src/indexing/workspace.rs#L1712)
после timeout может поздно завершить atomic replacement cache-файла. Вызывающий
код к этому моменту уже считает операцию завершившейся ошибкой и может запустить
новую загрузку, очистку или запись. Аналогично timeout не является реальным
ограничением времени shutdown при зависшем filesystem syscall.

#### Что исправить

- Считать текущий timeout ограничением latency вызывающего, а не cancellation
  работы, и не повторять ту же side-effecting операцию, пока предыдущая не
  завершилась.
- Для обходов и CPU-циклов передавать cooperative token и проверять его внутри
  bounded частей работы.
- Ограничить общий concurrency тяжёлых blocking jobs семафором.
- Записи всегда выполнять как `prepare temporary artifact` в blocking worker,
  а publication/rename — только после возврата результата и повторной проверки
  актуальности на async-стороне. Не позволять timed-out worker заменять live
  cache.
- Добавить тест, который удерживает blocking write за barrier, дожидается
  timeout, публикует более новый cache и затем освобождает старый worker;
  актуальный файл не должен быть заменён.

### HABR-CODEX-P2-02. Timeout регистрации watcher'ов оставляет неизвестное внешнее состояние

**Категория статьи:** async cancellation.  
**Приоритет:** Medium.

[`ExternalSymlinkManager::refresh_registration`](../server/crates/php-lsp-server/src/indexing/symlinks.rs#L195)
сначала создаёт новый registration ID, затем ждёт
`client.register_capability(...)` под `tokio::time::timeout`
([`symlinks.rs:243-278`](../server/crates/php-lsp-server/src/indexing/symlinks.rs#L243)).
ID записывается в состояние сервера только после успешного ответа
([`symlinks.rs:282-285`](../server/crates/php-lsp-server/src/indexing/symlinks.rs#L282)).

LSP-request к этому моменту уже отправлен. Возможна последовательность:

1. клиент получает запрос и применяет регистрацию;
2. ответ задерживается дольше server-side timeout либо ожидающий future
   отменяется;
3. сервер возвращается без `commit_registration` и без помещения нового ID в
   `stale_registrations`;
4. следующий refresh создаёт другой ID и регистрирует тот же набор повторно.

Старые подтверждённые регистрации корректно сохраняются для повторного
unregister, но тест
[`failed_unregister_remains_pending_until_confirmation`](../server/crates/php-lsp-server/src/indexing/symlinks_tests.rs#L295)
проверяет только ошибку снятия уже известного ID. Случай «registration применена,
но ответ потерян/опоздал» не моделируется.

Последствия: дубли watcher'ов, повторные события, лишняя индексация и регистрация,
которую сервер уже не умеет снять при shutdown.

#### Что исправить

- До отправки запроса фиксировать ID как `pending/uncertain`.
- После error, timeout или cancellation сохранять этот ID в retryable cleanup
  set и пытаться unregister до подтверждения; unregister неизвестного клиенту ID
  обрабатывать идемпотентно.
- Разделить `desired`, `pending`, `confirmed` и `stale` состояния регистрации.
- Добавить deterministic test: клиент применяет регистрацию, задерживает ответ
  до timeout, затем запускается refresh/shutdown; итогом не должно быть двух
  активных регистраций и потерянного ID.

### HABR-CODEX-P3-01. Временный каталог formatter не защищён RAII

**Категории статьи:** Drop/RAII, async cancellation.  
**Приоритет:** Low.

Formatter вручную строит путь в системном temp через PID и timestamp
([`formatting.rs:101-107`](../server/crates/php-lsp-server/src/lsp/formatting.rs#L101)),
создаёт каталог в blocking closure, а удаляет его только в отдельной поздней
операции чтения
([`formatting.rs:109-147`](../server/crates/php-lsp-server/src/lsp/formatting.rs#L109)).

Обычный cooperative cancellation внешней команды проходит через cleanup, но
есть пути утечки:

- future `run_external_formatter` дропнут после записи и до строки cleanup;
- blocking write превысил общий timeout, вызывающий уже вернул ошибку, а worker
  позднее успешно создал каталог и файл;
- panic/abort между созданием каталога и cleanup.

#### Что исправить

- Использовать RAII tempdir с гарантированно уникальным созданием вместо ручной
  пары `temp_dir()`/`remove_dir_all`.
- Не позволять blocking writer пережить владельца cleanup: завершать/координировать
  запись до drop guard либо передавать cleanup-владение самому worker.
- Добавить тесты cancellation в каждой await-точке и timeout задержанной записи.

## Результат по всем категориям статьи

| Категория | Результат в php-lsp | Обоснование |
|---|---|---|
| Lifetime laundering | Не найдено | Долгоживущие index/cache структуры владеют `String`, `PathBuf` и `Arc`; найденные `HashMap<&str, ...>` локальны вычислению и не переживают входной source. Явные lifetimes в resolver/framework context описывают обычные borrowed views. |
| `Send`/`Sync`, sync mutex в async | Не найдено | Ручных `unsafe impl Send/Sync` нет. Production `std::sync::Mutex` используется в коротких очередях status/diagnostics; guard извлекает owned value и освобождается до `.await`. Длительные async-критические секции используют Tokio mutex/RwLock. Дополнительный `clippy::await_holding_lock` прошёл. |
| Drop order / RAII | Частично подтверждено | Собственные `PreparedCacheWrite`, `ObjectTypeResolveDepthGuard` и `IndexingRunGuard` корректно освобождают временный файл/depth/run. Проблемы остаются на внешней границе process/tempdir — `HABR-CODEX-P1-01` и `HABR-CODEX-P3-01`. DB transaction API в проекте нет. |
| `unsafe` | Не применимо сейчас | В first-party Rust-коде нет `unsafe`, `transmute`, raw allocation/read и ручных `Send`/`Sync`. Miri сейчас не даст содержательной дополнительной проверки этого класса. |
| Async cancellation | Подтверждено | Найдены `HABR-CODEX-P1-01`, `HABR-CODEX-P2-01`, `HABR-CODEX-P2-02` и `HABR-CODEX-P3-01`. При этом reindex pipeline уже имеет отдельные generation/run lease, staging и RAII-защиты. |
| Semver-конфликты blanket impl | Не найдено | В first-party crates нет trait blanket impl; два `impl<'a>` — inherent impl конкретных framework context/registry types. |
| Большие массивы на стеке | Не найдено | Нет крупных `[T; N]` и `Box::new([..; N])`; единственное динамическое заполнение `vec![0; configs.len()]` размещает элементы в heap. Дополнительный `clippy::large_stack_arrays` прошёл. |

## Технические неточности самой статьи

Чек-лист статьи полезен, но её примеры нельзя переносить в правила проекта без
проверки:

1. **Пример с `std::sync::Mutex` не держит guard через `.await`.** После
   `lock()` в теле вообще нет await-point; clone выполняется и guard освобождается
   в одном poll. Такой метод лучше сделать синхронным, но заявленный deadlock из
   показанного кода не следует. Более того,
   [документация Tokio Mutex](https://docs.rs/tokio/latest/tokio/sync/struct.Mutex.html)
   прямо допускает обычный mutex для простых данных, если guard не живёт через
   `.await`.
2. **Blanket-impl пример конфликтует сразу.** Если crate A уже содержит
   `impl<T: Display> Bar for T`, то для `MyType: Display` реализация `Bar`
   существует автоматически; явный `impl Bar for MyType` в crate B пересекается
   с ней уже сегодня. Это запрещает coherence, а не ломается лишь после будущего
   minor release. Сам общий semver-риск будущего blanket impl реален; неверна
   конкретная последовательность примера. См.
   [Rust Reference: overlapping implementations](https://doc.rust-lang.org/stable/reference/items/implementations.html#trait-implementation-coherence).
3. **`read_unaligned` исправляет только alignment.** Для чтения произвольных
   сетевых байтов как `Header` также нужны валидность bit pattern, layout,
   endian и отсутствие недопустимых полей. Официальный контракт
   [`ptr::read`](https://doc.rust-lang.org/core/ptr/fn.read.html) требует и
   выравнивание, и корректно инициализированное значение `T`; простая замена на
   `read_unaligned` не превращает произвольную `#[repr(C)]` структуру в wire
   format.
4. **Утверждение о блокирующем rollback SQLx слишком общее.** Поведение зависит
   от driver/version; `Drop` вызывает синхронный `start_rollback`, который у
   сетевых драйверов может поставить rollback в буфер для последующей отправки,
   а не обязательно выполнить блокирующий сетевой rollback и warning. Сам вывод
   «проверять Drop-контракт конкретной библиотеки» правильный, приведённый
   универсальный эффект — нет.

## Рекомендации процесса

- После закрытия четырёх находок добавить к CI явно названные
  `clippy::await_holding_lock` и `clippy::large_stack_arrays`, чтобы результат не
  зависел от будущего изменения lint groups.
- Пока проект не требует `unsafe`, рассмотреть `#![forbid(unsafe_code)]` в пяти
  first-party crates. Это сильнее ночного Miri для предотвращения случайного
  появления нового unsafe; если unsafe станет необходим, запрет можно осознанно
  снять вместе с documented safety invariants и Miri job.
- Для async API с побочными эффектами документировать linearization point,
  поведение при drop future и владельца cleanup. Комментарий `cancel-safe` сам по
  себе недостаточен — контракт должен закрепляться deterministic regression.

## Выполненная проверка

- `CARGO_BUILD_JOBS=1 cargo clippy --all-targets -- -D warnings -D clippy::await_holding_lock -D clippy::large_stack_arrays` — **passed**.
- `CARGO_BUILD_JOBS=1 cargo test -p php-lsp-server test_run_shell_command_with_timeout_respects_cancellation -- --test-threads=1` — **1 passed**; тест подтверждает быстрый возврат, но не проверяет process tree/reap.
- `CARGO_BUILD_JOBS=1 cargo test -p php-lsp-server server::indexing::symlinks::tests -- --test-threads=1` — **16 passed**; coverage неизвестного результата registration отсутствует.

