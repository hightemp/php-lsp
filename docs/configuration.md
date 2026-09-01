# Configuration

php-lsp supports shared project configuration through `.php-lsp.toml`.

The VS Code extension also contributes `phpLsp.*` settings. Explicit VS Code
settings override `.php-lsp.toml`; default VS Code values do not mask project
configuration.

## Discovery

Configuration is applied in this order:

1. Built-in server defaults.
2. Global config from the first existing path in:
   - `PHP_LSP_CONFIG`
   - `$XDG_CONFIG_HOME/php-lsp/config.toml`
   - `$HOME/.config/php-lsp/config.toml`
   - `$HOME/.php-lsp.toml`
3. Project config:
   - `.php-lsp.toml` next to the discovered `composer.json`
   - otherwise `.php-lsp.toml` in the workspace root
4. Explicit VS Code `phpLsp.*` settings and initialization options.

The VS Code client watches `**/.php-lsp.toml`; changes are sent as
`workspace/didChangeWatchedFiles` and the server reloads effective
configuration without requiring a restart.

### Multi-root workspaces

The VS Code extension resolves explicit settings separately with
`workspace.getConfiguration("phpLsp", folder.uri)` for every workspace folder
and sends a versioned snapshot to the server. The precedence above is applied
independently per folder: project A's `.php-lsp.toml`, resource overrides, and
command trust never become inputs for project B merely because B appears later
in the workspace-folder list.

For file-backed requests, the most specific containing workspace folder owns
the request. This matters for nested folders. Composer effective roots,
include/exclude paths, cache identity, PHP version, diagnostics, vendor/stubs
policy, formatter, PHPStan/Psalm, and analyzer code actions come from that
owner. A file outside all configured roots uses only global/client fallback
settings instead of inheriting the first workspace folder.

## Command Trust

Project `.php-lsp.toml` is treated as untrusted for executable settings by
default. The server ignores project-provided analyzer and formatter commands
unless command trust is enabled outside the project:

- VS Code: `phpLsp.allowProjectCommands = true`
- Global php-lsp config: `allowProjectCommands = true`

The project file itself cannot opt in to command trust. Without trust, these
project settings are ignored:

- `[formatting] command`
- executable `[formatting] provider` values such as `pint`, `php-cs-fixer`, and
  `phpcbf`
- `[phpstan] enabled = true`
- `[phpstan] command`
- `[psalm] enabled = true`
- `[psalm] command`

Safe project settings such as PHP version, diagnostics mode/severity,
include/exclude paths, stubs, analyzer timeouts, and `formatting.provider =
"none"` still apply. Put commands in VS Code settings or global php-lsp config
for untrusted repositories.

## Create A Config

```bash
php-lsp init-config
```

This creates `.php-lsp.toml` in the current directory and never overwrites an
existing file. To write another path:

```bash
php-lsp init-config --path path/to/.php-lsp.toml
```

The JSON schema is available at [`config-schema.json`](../config-schema.json).

## Analyze From CLI

```bash
php-lsp analyze [PATH] --project-root <DIR> --severity warning --format json
```

The `analyze` command loads the same global and project configuration files as
the language server. It uses PHP version, diagnostic mode/severity, Composer
discovery, and include/exclude path settings when building its command-line
diagnostic report.

## Fix From CLI

```bash
php-lsp fix [PATH] --dry-run --project-root <DIR> --rule unused-imports --format json
```

The `fix` command uses the same configuration loading path as `analyze`. In
dry-run mode it reports safe native edits without writing files. Without
`--rule`, it previews unused-import cleanup and PHPDoc-derived native return
types that are valid for the configured PHP version. `--rule` can be repeated
with `unused-imports`, `organize-imports`, and `add-return-type`.

`php-lsp fix` does not run configured project formatters.

## Example

```toml
[php]
version = "8.3"

[diagnostics]
mode = "basic-semantic"
memberTypeNodeBudget = 512
partialAnalysisDiagnostic = true

[diagnostics.severity]
unknownSymbols = "warning"
unused = "hint"
members = "warning"

[indexing]
composer = true
vendor = true
include = ["src", "tests"]
exclude = ["var/cache", "storage/framework/cache"]
maxFiles = 50000
maxEntries = 500000

[stubs]
extensions = ["Core", "SPL", "standard", "PDO", "json", "mbstring"]

[formatting]
provider = "auto"
timeoutMs = 30000

[phpstan]
enabled = false
# Project analyzer commands require phpLsp.allowProjectCommands or global
# allowProjectCommands = true before they are executed.
# command = "vendor/bin/phpstan analyse --error-format=json --no-progress --no-interaction {file}"
timeoutMs = 30000
memory_limit = "1G"

[psalm]
enabled = false
# command = "vendor/bin/psalm --output-format=json --no-progress {file}"
timeoutMs = 30000

[analyzerCodeActions]
enabled = false
```

## Sections

| Section | Keys |
|---|---|
| `[php]` | `version` |
| `[diagnostics]` | `mode`, `memberTypeNodeBudget`, `partialAnalysisDiagnostic` |
| `[diagnostics.severity]` | `unknownSymbols`, `unused`, `duplicateSymbols`, `members`, `typeCompatibility`, `overrideSignatures`, `phpVersion` |
| `[indexing]` | `composer`, `vendor`, `include`, `exclude`, `maxFiles`, `maxEntries`, `stubs` |
| `[stubs]` | `path`, `extensions` |
| `[formatting]` | `provider`, `command`, `timeoutMs` |
| `[phpstan]` | `enabled`, `command`, `timeoutMs`, `memory_limit` |
| `[psalm]` | `enabled`, `command`, `timeoutMs` |
| `[analyzerCodeActions]` | `enabled` |

`diagnostics.memberTypeNodeBudget` limits the relevant syntax nodes inspected
by the expensive member-access and type-compatibility categories. The default is
`512`; `0` disables this cap. When the cap is reached,
`diagnostics.partialAnalysisDiagnostic` controls whether php-lsp publishes the
informational `partial-analysis` diagnostic in addition to logging the skipped
categories. The deprecated `diagnostics.memberTypeBudget` spelling remains an
accepted project-config alias for compatibility; new files should use
`memberTypeNodeBudget`.

## Filesystem traversal limits and symlinks

`indexing.maxFiles` defaults to `100000` unique matching files and
`indexing.maxEntries` defaults to `1000000` admitted entries per traversal.
VS Code exposes the same controls as `phpLsp.indexing.maxFiles` and
`phpLsp.indexing.maxEntries`. A trusted global or explicit VS Code value of `0`
disables the corresponding cap. Because project configuration is untrusted, a
project `.php-lsp.toml` may lower these limits but cannot raise or disable the
trusted baseline.

When a limit is reached, php-lsp retains the files found in deterministic path
order, marks the index as partial in `phpLsp/indexingStatus`, logs a warning,
and keeps the server usable. Changing either limit invalidates the workspace
cache and starts a clean reindex.

The `analyze` and `fix --dry-run` commands report configured limit truncation
as an explicit stderr warning. A recursive CLI traversal that reaches its
internal deadline fails with exit code `1` instead of silently presenting a
partial result as complete.

Directory children are admitted only after the directory batch is complete.
The visitor uses at most one unqueued iterator lookahead to distinguish an
exactly full directory from a truncated one; an incomplete batch is discarded.
This keeps the configured admitted-entry cap and the resulting partial index
deterministic even when the operating system returns directory entries in a
different order.

Filesystem traversal follows file and directory symlinks, including external
targets used by shared packages and Composer path repositories. Physical file
IDs prevent cycles and duplicate aliases; exclusions are evaluated against the
logical project path. VS Code clients supporting LSP 3.17 relative dynamic file
watchers receive live updates for discovered external targets. Other clients
still receive safe initial indexing and a warning that external changes require
a reindex.

## Stubs

The VS Code extension passes the bundled `client/stubs` directory to the
server automatically. A project or global config can override the source path
with `[stubs].path`.

When the server is run directly from the Rust workspace, fallback discovery also
checks the source checkout `server/data/stubs` directory relative to
`server/target/{debug,release}` binaries. This keeps CLI `analyze`/`fix` runs
from reporting PHP built-ins as unknown solely because no VS Code bundled stubs
path was provided.

`[stubs].extensions` has three distinct states:

- Omitted: load all available phpstorm-stubs extension directories from the
  selected stubs root.
- Non-empty array: load only the listed phpstorm-stubs extension directories.
- Empty array: disable stubs intentionally.

Startup logs distinguish an intentional empty extension list from missing or
uninitialized stubs paths. Development, CI, and release packaging use
`scripts/check-stubs.sh`/`make check-stubs` to fail when source or bundled stubs
are too small or missing required core files.

Executable keys in project config follow the command trust rules above.
Relative include/exclude paths are interpreted relative to the effective
workspace root. `phpstan.memory_limit` is added to the PHPStan command unless
the command already contains `--memory-limit`; `{memory_limit}` can be used in a
custom command template for explicit placement.

## Formatter Resolution

`[formatting] provider = "auto"` is the default. The formatter provider is
resolved in this order:

1. Explicit VS Code `phpLsp.formatting.*` settings, global php-lsp config, or
   trusted `.php-lsp.toml` `[formatting]` values.
2. Composer `require-dev`/`require` auto-detection:
   `laravel/pint`, `friendsofphp/php-cs-fixer`, then
   `squizlabs/php_codesniffer`.
3. No external formatter when no explicit provider or supported Composer tool is
   available.

Supported provider values are `auto`, `none`, `pint`, `php-cs-fixer`, `phpcbf`,
and `custom`. There is intentionally no `built-in` provider. Use `none` to
disable formatting. Use `custom` with `command` and the `{file}` placeholder
when a project has a wrapper script.

External formatter commands are timeout-bound by `timeoutMs` and are cancelled
when a document changes, closes, or a newer formatting request supersedes the
old request. Range formatting remains conservative: php-lsp formats only the
selected fragment via a temporary file and does not run whole-document
formatting for range requests.

Analyzer code actions are disabled by default. When
`analyzerCodeActions.enabled` is true, PHPStan/Psalm diagnostics can offer local
ignore comments and metadata-driven fixes such as missing `@throws`, iterable
PHPDoc value types, and obvious prefixed class-name replacements.
