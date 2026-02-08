# PHP Language Server — Architecture Decision Records

Зафиксированные решения, принятые перед началом разработки.

---

## ADR-001: Имя проекта

**Решение:** `php-lsp`

- Crate имена: `php-lsp-server`, `php-lsp-parser`, `php-lsp-index`, `php-lsp-completion`, `php-lsp-types`
- VS Code extension ID: `php-lsp`
- Settings prefix: `phpLsp.*`
- Команды: `phpLsp.restartServer`, etc.

**Обоснование:** короткое, универсальное, понятное, не занято на crates.io.

---

## ADR-002: Rust edition и MSRV

**Решение:** Edition 2021, MSRV 1.75

**Обоснование:**
- Rust 1.75 стабилизировал `async fn in trait` — не нужен макрос `#[async_trait]`
- Широко доступна (вышла Dec 2023)
- tower-lsp-server v0.23 требует MSRV 1.77 — фактически MSRV будет определяться зависимостями
- Edition 2021 — стабильная, поддерживается всеми инструментами

---

## ADR-003: LSP-фреймворк — tower-lsp-server

**Решение:** `tower-lsp-server` v0.23+ (community fork)

**Рассмотренные альтернативы:**
- `tower-lsp` (оригинал) — заброшен с Aug 2023, 28 open issues
- `async-lsp` v0.2 — лучше ordering нотификаций, `&mut self`, но 145 stars, 3 контрибутора

**Обоснование:**
- Крупнейшая экосистема: Biome, Oxc, Harper, Veryl
- ~43K downloads/month, активная поддержка
- Простой API: `LanguageServer` trait → `LspService::new()` → `Server::serve()`
- Обновлённые lsp-types 0.97+

**Известное ограничение:** нотификации обрабатываются асинхронно (out-of-order).
**Mitigation:** собственная очередь для didChange через mpsc channel с ordering по version.

---

## ADR-004: PHP парсер — tree-sitter-php

**Решение:** `tree-sitter` v0.26 + `tree-sitter-php` v0.24 (grammar `php`, не `php_only`)

**Рассмотренные альтернативы:**
- `php-parser` crate (wudi) — 0.1.x, 2 месяца, нет инкрементального парсинга
- Mago parser — встроен в монорепо Mago, не extractable как standalone crate

**Обоснование:**
1. Инкрементальный парсинг — критически важен для LSP (<1мс при keystroke)
2. Проверенная error recovery (ERROR/MISSING ноды, остальное дерево валидно)
3. Боевая зрелость: GitHub, Neovim, Zed. 207 stars, 36 contributors, 642 commits
4. PHP 7.4–8.3 полностью покрыты
5. Grammar `php` поддерживает mixed PHP/HTML файлы

**Trade-off:** CST (не AST) — нужен слой маппинга CST → SymbolInfo. Одноразовая работа.

---

## ADR-005: Буфер документов — ropey::Rope

**Решение:** `ropey::Rope`

**Рассмотренные альтернативы:**
- `String` — O(n) при каждом edit, но достаточно для файлов <100KB
- `crop::Rope` — менее популярный, меньше зависимостей

**Обоснование:**
- O(log n) вставки/удаления
- Эффективен для больших файлов
- Поддержка byte/char/line-based индексации (нужно для tree-sitter InputEdit)
- Широко используется в текстовых редакторах на Rust (Helix, Lapce)

---

## ADR-006: Доставка phpstorm-stubs — git submodule

**Решение:** Git submodule в `server/data/stubs`

**Рассмотренные альтернативы:**
- Embed в бинарник (include_bytes! / build.rs) — +5-10MB к размеру
- Скачивать при первом запуске — нужен интернет, сложнее offline

**Обоснование:**
- Stubs обновляются редко (при обновлении сервера)
- Парсятся при первом запуске, кэшируются на диск
- Submodule легко обновить: `git submodule update --remote`
- Не увеличивает размер бинарника

---

## ADR-007: Vendor индексация — lazy (on-demand)

**Решение:** Lazy-индексация vendor в MVP

**Механизм:**
1. При resolve_fqn символ не найден в основном индексе
2. По namespace_map (из composer.json) определить файл в vendor/
3. Парсить файл on-demand
4. Добавить символы в индекс (кэш)

**Обоснование:**
- Полная индексация vendor (10K+ файлов) замедляет startup
- Lazy подход: пользователь не замечает задержки (hover/definition при первом обращении ~10мс)
- Конфигурируемо: `phpLsp.indexVendor`

---

## ADR-008: PHPDoc парсинг — свой мини-парсер

**Решение:** Собственный парсер для базовых PHPDoc тегов

**Поддерживаемые теги (MVP):**
- `@param Type $name [Description]`
- `@return Type [Description]`
- `@var Type`
- `@throws Type`
- `@deprecated [Description]`
- `@property Type $name`
- `@method ReturnType name(params)`

**Не поддерживаются (MVP):**
- `@template`, `@extends`, `@implements` — generics
- `@psalm-*`, `@phpstan-*` — vendor-specific
- Сложные типы: `array<string, int>`, `Closure(int): string`

**Обоснование:**
- Покрывает ~90% реальных PHPDoc
- Библиотек для PHPDoc на Rust нет
- Полный парсер PHPDoc (с generics) — значительный объём работы, отложен на v1/vNext

---

## ADR-009: LSP integration тесты — in-process mock client

**Решение:** Тесты без spawn процесса, напрямую через `LspService`

**Механизм:**
- Создать `LspService` в тесте
- Отправлять JSON-RPC запросы/нотификации напрямую
- Проверять ответы

**Обоснование:**
- Быстро: нет overhead на spawn/stdio
- Детерминированно: нет race conditions транспорта
- Достаточно для CI

**Дополнительно (v1):** smoke-тесты с реальным subprocess для проверки stdio.

---

## ADR-010: Multi-root workspace — не в MVP

**Решение:** MVP поддерживает один workspace root

**Обоснование:**
- Один root = один composer.json = один индекс — значительно проще
- Multi-root потребует: отдельный индекс на каждый root, сложный resolve cross-root, маппинг URI → root
- Добавить в v1/vNext когда архитектура стабилизируется

---

## ADR-011: Версионирование — SemVer + trunk-based

**Решение:**
- SemVer: v0.1.0, v0.2.0, ..., v1.0.0
- Branching: trunk-based (main = trunk, feature branches краткоживущие, release tags)

**Обоснование:**
- Стандарт для crates.io и npm
- Trunk-based подходит для малой команды, минимум overhead
- Pre-1.0: breaking changes допустимы с minor bump

---

## ADR-012: Порядок разработки — Transport → Parser → Index

**Решение:** Начать со scaffold + LSP hello-world, затем парсер, затем индекс

**Обоснование:**
- Можно демонстрировать прогресс рано (сервер запускается в VS Code)
- Каждый шаг тестируем отдельно
- Зависимости: transport нужен для отправки diagnostics, parser нужен для extraction, index нужен для hover/definition

---

## ADR-013: Лицензия — MIT

**Решение:** MIT

**Совместимость зависимостей:**
- tree-sitter: MIT
- tree-sitter-php: MIT
- tower-lsp-server: MIT / Apache-2.0
- phpstorm-stubs: Apache-2.0 (совместим)
- ropey: MIT
- tokio: MIT

---

## ADR-014: Client tooling — TypeScript + esbuild

**Решение:** TypeScript + esbuild для VS Code extension

**Обоснование:**
- esbuild: быстрая сборка (<1с), простая конфигурация
- Стандартный подход для VS Code extensions
- Меньше зависимостей чем webpack

---

## ADR-015: CI — GitHub Actions с первого дня

**Решение:** CI pipeline сразу в MVP

**Содержание:**
- cargo clippy + cargo fmt --check
- cargo test --all
- npm ci + npm run build (client)
- Matrix: ubuntu-latest (расширить позже)

**Обоснование:**
- Ловит проблемы рано
- Дисциплинирует: код всегда компилируется, тесты проходят
- Минимальный overhead: один yml файл

---

## ADR-016: Mixed PHP/HTML — grammar `php`

**Решение:** Использовать tree-sitter-php grammar `php` (с поддержкой `<?php` тегов)

**Альтернатива отвергнута:** `php_only` — не поддерживает mixed файлы, сломает legacy проекты

**Обоснование:**
- Многие PHP-проекты содержат mixed PHP/HTML файлы
- Grammar `php` обрабатывает `<?php` / `<?=` / `?>` теги
- Индексируются только PHP-части (class/function определения)

---

## Риски и компромиссы

| # | Риск | Вероятность | Влияние | Mitigation |
|---|------|-------------|---------|------------|
| 1 | Динамическая типизация PHP | Высокая | Среднее | Best-effort: PHPDoc + type hints. Не пытаться быть PHPStan |
| 2 | Magic methods (__get, __call) | Высокая | Среднее | PHPDoc @property, @method. Laravel: пользователь подключает ide-helper stubs |
| 3 | CST ≠ AST (tree-sitter) | Средняя | Среднее | Явный модуль symbols.rs для маппинга. Одноразовая работа |
| 4 | tower-lsp notification ordering | Средняя | Высокое | mpsc channel + ordering по document version |
| 5 | Память на крупных проектах | Средняя | Среднее | Lazy vendor indexing. Compact symbol representation |
| 6 | PHPDoc parsing сложность | Средняя | Среднее | MVP: базовые теги. Расширенные — vNext |
| 7 | phpstorm-stubs размер (~50MB) | Низкая | Низкое | Парсить при первом запуске, кэшировать (~5-10MB compact) |
| 8 | tree-sitter-php лаг новых фич PHP | Низкая | Низкое | Грамматика активно поддерживается. При необходимости — PR upstream |
