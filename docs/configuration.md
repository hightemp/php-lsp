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

[diagnostics.severity]
unknownSymbols = "warning"
unused = "hint"
members = "warning"

[indexing]
composer = true
vendor = true
include = ["src", "tests"]
exclude = ["var/cache", "storage/framework/cache"]

[stubs]
extensions = ["Core", "SPL", "standard", "PDO", "json", "mbstring"]

[formatting]
provider = "auto"
timeoutMs = 30000

[phpstan]
enabled = true
command = "vendor/bin/phpstan analyse --error-format=json --no-progress --no-interaction {file}"
timeoutMs = 30000
memory_limit = "1G"

[psalm]
enabled = false
command = "vendor/bin/psalm --output-format=json --no-progress {file}"
timeoutMs = 30000

[analyzerCodeActions]
enabled = false
```

## Sections

| Section | Keys |
|---|---|
| `[php]` | `version` |
| `[diagnostics]` | `mode` |
| `[diagnostics.severity]` | `unknownSymbols`, `unused`, `duplicateSymbols`, `members`, `typeCompatibility`, `overrideSignatures`, `phpVersion` |
| `[indexing]` | `composer`, `vendor`, `include`, `exclude`, `stubs` |
| `[stubs]` | `path`, `extensions` |
| `[formatting]` | `provider`, `command`, `timeoutMs` |
| `[phpstan]` | `enabled`, `command`, `timeoutMs`, `memory_limit` |
| `[psalm]` | `enabled`, `command`, `timeoutMs` |
| `[analyzerCodeActions]` | `enabled` |

Relative include/exclude paths are interpreted relative to the effective
workspace root. `phpstan.memory_limit` is added to the PHPStan command unless
the command already contains `--memory-limit`; `{memory_limit}` can be used in a
custom command template for explicit placement.

## Formatter Resolution

`[formatting] provider = "auto"` is the default. The formatter provider is
resolved in this order:

1. Explicit VS Code `phpLsp.formatting.*` settings or `.php-lsp.toml`
   `[formatting]` values.
2. Composer `require-dev`/`require` auto-detection:
   `laravel/pint`, `friendsofphp/php-cs-fixer`, then
   `squizlabs/php_codesniffer`.
3. No external formatter when no explicit provider or supported Composer tool is
   available.

Supported provider values are `auto`, `none`, `pint`, `php-cs-fixer`, `phpcbf`,
and `custom`. Use `none` to disable formatting. Use `custom` with `command` and
the `{file}` placeholder when a project has a wrapper script.

External formatter commands are timeout-bound by `timeoutMs` and are cancelled
when a document changes, closes, or a newer formatting request supersedes the
old request. Range formatting remains conservative: php-lsp formats only the
selected fragment via a temporary file and does not run whole-document
formatting for range requests.

Analyzer code actions are disabled by default. When
`analyzerCodeActions.enabled` is true, PHPStan/Psalm diagnostics can offer local
ignore comments and metadata-driven fixes such as missing `@throws`, iterable
PHPDoc value types, and obvious prefixed class-name replacements.
