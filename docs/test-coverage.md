# Project-wide test coverage baseline

Snapshot: 2026-09-25. This is a **test-evidence matrix**, not a claim that every
possible combination works. A row identifies a user-visible function, an
important state/transition, and what executes on each platform. The test link
is evidence for the stated scenario on Linux; it does not imply that the same
test has run on macOS or Windows.

Platform marks: **T** = an automated test executes the behavior on Linux;
**M** = a Node helper test or mock-based VSIX test executes it;
**B** = the release workflow builds a binary for that OS, without executing
this behavior; **—** = no matching automated test or build evidence. Rust
protocol tests use an in-process LSP service; they do not exercise a real
VS Code Extension Host. Linux here means the current x86-64 development host
and Ubuntu CI, not every Linux architecture. The release build matrix has
Linux x64/arm64, macOS x64/arm64, and Windows x64/arm64 targets.

## Rust parser, index, and LSP

| Function | State / transition | Linux | macOS | Windows | Evidence and remaining gap |
|---|---|:---:|:---:|:---:|---|
| LSP lifecycle | initialize, initialized, shutdown | T | B | B | [initialize protocol tests](../server/crates/php-lsp-server/tests/e2e_initialize.rs), [indexing protocol tests](../server/crates/php-lsp-server/tests/e2e_indexing.rs). |
| Parser and symbol extraction | ordinary PHP, namespaces, PHPDoc | T | B | B | [parser tests](../server/crates/php-lsp-parser/src/parser_tests.rs), [symbol tests](../server/crates/php-lsp-parser/src/symbols_tests.rs); grammar-version compatibility still needs separate fixtures. |
| Parser incremental edits | UTF-16, past line end, chained edits | T | B | B | [incremental positions](../server/crates/php-lsp-server/tests/e2e_incremental_positions.rs), [parser tests](../server/crates/php-lsp-parser/src/parser_tests.rs). |
| Diagnostics | open before/during/after cold indexing | T | B | B | [diagnostics protocol tests](../server/crates/php-lsp-server/tests/e2e_diagnostics.rs). |
| Diagnostics | rapid unsaved changes, stale versions, close/reopen | T | B | B | [diagnostics protocol tests](../server/crates/php-lsp-server/tests/e2e_diagnostics.rs). |
| Diagnostics | syntax, semantic, PHP version, suppression budgets | T | B | B | [parser diagnostic tests](../server/crates/php-lsp-parser/src/diagnostics_tests.rs), [protocol tests](../server/crates/php-lsp-server/tests/e2e_diagnostics.rs). |
| External analyzers | trusted/untrusted PHPStan configuration, output location | T | B | B | [initialize tests](../server/crates/php-lsp-server/tests/e2e_initialize.rs), [server tests](../server/crates/php-lsp-server/src/server_tests.rs); real installed PHPStan/Psalm versions are not a compatibility matrix. |
| Completion | classes/functions/constants, import kind, deduplication | T | B | B | [completion protocol tests](../server/crates/php-lsp-server/tests/e2e_completion.rs), [provider tests](../server/crates/php-lsp-completion/src/provider_tests.rs). |
| Completion resolve | enriched item detail and stale/merged type state | T | B | B | [composite receiver tests](../server/crates/php-lsp-server/tests/e2e_composite_receivers.rs), [definition tests](../server/crates/php-lsp-server/tests/e2e_definition.rs). |
| Completion | incomplete member access and unsaved edit | T | B | B | [completion protocol tests](../server/crates/php-lsp-server/tests/e2e_completion.rs). |
| Completion | lazy vendor first hit and warm repeated hit | T | B | B | [vendor metadata tests](../server/crates/php-lsp-server/tests/e2e_vendor_metadata.rs), [vendor symlink tests](../server/crates/php-lsp-server/src/indexing/vendor_symlink_tests.rs); latency budget is unmeasured. |
| Hover and type inference | indexed, local, PHPDoc, composite receivers | T | B | B | [hover tests](../server/crates/php-lsp-server/tests/e2e_hover.rs), [composite receiver tests](../server/crates/php-lsp-server/tests/e2e_composite_receivers.rs). |
| Definition/declaration/type definition/implementation | local, cross-file, inheritance, vendor | T | B | B | [definition protocol tests](../server/crates/php-lsp-server/tests/e2e_definition.rs). |
| Signature help and inlay hints | call positions, inferred types | T | B | B | [completion tests](../server/crates/php-lsp-server/tests/e2e_completion.rs), [hover tests](../server/crates/php-lsp-server/tests/e2e_hover.rs). |
| References/highlights/code lens | open and indexed closed files | T | B | B | [references protocol tests](../server/crates/php-lsp-server/tests/e2e_references.rs). |
| Workspace symbols | indexed search, ranking, URI/range | T | B | B | [workspace symbol tests](../server/crates/php-lsp-server/src/server_tests.rs), [indexing protocol tests](../server/crates/php-lsp-server/tests/e2e_indexing.rs). |
| Rename | locals, imports, members, unsafe targets | T | B | B | [references protocol tests](../server/crates/php-lsp-server/tests/e2e_references.rs), [rename unit tests](../server/crates/php-lsp-server/src/lsp/rename_tests.rs). |
| Code actions | imports, organize imports, analyzer fixes | T | B | B | [code-action protocol tests](../server/crates/php-lsp-server/tests/e2e_code_actions.rs). |
| Code actions | generate/implement members, parent constructor | T | B | B | [code-action tests](../server/crates/php-lsp-server/tests/e2e_code_actions.rs), [constructor tests](../server/crates/php-lsp-server/tests/e2e_constructor.rs). |
| Code actions | extract/inline, stale resolve, PHP version | T | B | B | [code-action protocol tests](../server/crates/php-lsp-server/tests/e2e_code_actions.rs). |
| Formatting | configured command, auto-detection, range/on-type | T | B | B | [formatting protocol tests](../server/crates/php-lsp-server/tests/e2e_formatting.rs); platform shell/process differences are untested. |
| Symbols/folding/links/tokens | full, range, delta, UTF-16 | T | B | B | [symbol protocol tests](../server/crates/php-lsp-server/tests/e2e_symbols.rs), [range tests](../server/crates/php-lsp-server/tests/e2e_ranges.rs). |
| Selection and linked editing ranges | AST expansion and import aliases | T | B | B | [symbol protocol tests](../server/crates/php-lsp-server/tests/e2e_symbols.rs). |
| Static document links | include/require targets | T | B | B | [symbol protocol tests](../server/crates/php-lsp-server/tests/e2e_symbols.rs). |
| Semantic tokens | full → delta and range requests | T | B | B | [symbol protocol tests](../server/crates/php-lsp-server/tests/e2e_symbols.rs). |
| Call/type hierarchy | incoming/outgoing, super/subtypes | T | B | B | [hierarchy protocol tests](../server/crates/php-lsp-server/tests/e2e_hierarchy.rs). |
| Blade virtual PHP | hover/completion/diagnostics/source ranges | T | B | B | [template protocol tests](../server/crates/php-lsp-server/tests/e2e_templates.rs). |
| Twig virtual PHP and context | controller → caller → partial; open/change/delete/rename | T | B | B | [Twig context protocol tests](../server/crates/php-lsp-server/tests/e2e_twig_context.rs), [template tests](../server/crates/php-lsp-server/tests/e2e_templates.rs). |
| Twig context cache | cold/warm, overlays, dependency changes, cancellation | T | B | B | [Twig context unit tests](../server/crates/php-lsp-server/src/indexing/twig_context_tests.rs), [regressions](../server/crates/php-lsp-server/src/indexing/twig_context_regression_tests.rs). |
| Workspace index | cold scan, warm cache, source provenance | T | B | B | [index tests](../server/crates/php-lsp-index/src/workspace_tests.rs), [cache tests](../server/crates/php-lsp-index/src/cache_tests.rs); warm-start timing after schema 26 is unmeasured. |
| Workspace index | watch create/change/delete and file operations | T | B | B | [indexing protocol tests](../server/crates/php-lsp-server/tests/e2e_indexing.rs). |
| Workspace configuration | reconfigure parser/stubs/index, cancel old generation | T | B | B | [initialize tests](../server/crates/php-lsp-server/tests/e2e_initialize.rs), [indexing tests](../server/crates/php-lsp-server/tests/e2e_indexing.rs). |
| Pre-create/pre-delete file requests | advertised no-edit response | B | B | B | Implementation is documented in [LSP features](lsp-features.md); no direct protocol regression found for either response. |
| Workspace index | superseded run, shutdown, root removal, races | T | B | B | [run tests](../server/crates/php-lsp-server/src/indexing/run_tests.rs), [indexing protocol tests](../server/crates/php-lsp-server/tests/e2e_indexing.rs). |
| Multi-root isolation | duplicate names, settings, indexing run | T | B | B | [definition tests](../server/crates/php-lsp-server/tests/e2e_definition.rs), [initialize tests](../server/crates/php-lsp-server/tests/e2e_initialize.rs), [indexing tests](../server/crates/php-lsp-server/tests/e2e_indexing.rs). |
| Composer/vendor | PSR-4/PSR-0, invalid metadata, autoload-dev | T | B | B | [composer tests](../server/crates/php-lsp-index/src/composer_tests.rs), [vendor metadata tests](../server/crates/php-lsp-server/src/indexing/vendor_metadata_tests.rs). |
| Symlink traversal/watch | external targets, duplicate/cycle, future file | T | B | B | [walker tests](../server/crates/php-lsp-server/src/util/fs_walk_tests.rs), [symlink tests](../server/crates/php-lsp-server/src/indexing/symlinks_tests.rs); Windows symlink permissions/path semantics need native tests. |
| Cache and URI handling | corrupt/legacy cache, encoded path | T | B | B | [cache tests](../server/crates/php-lsp-index/src/cache_tests.rs), [URI tests](../server/crates/php-lsp-types/src/uri_tests.rs). |
| CLI analyze/fix | source parsing and local fixes | T | B | B | [analyze tests](../server/crates/php-lsp-server/src/analyze_tests.rs), [fix tests](../server/crates/php-lsp-server/src/fix_tests.rs); packaged-binary CLI invocation is not covered across OSes. |

## VS Code client and delivery

| Function | State / transition | Linux | macOS | Windows | Evidence and remaining gap |
|---|---|:---:|:---:|:---:|---|
| Client lifecycle | activation, start/stop, crash/restart, queued changes | M | — | — | [lifecycle check](../client/scripts/check-lifecycle.mjs), [packaged VSIX smoke](../scripts/smoke-vsix.sh); no real Extension Host test. |
| Server process selection | supported platform/architecture, executable resolution | M | — | — | [server-process check](../client/scripts/check-server-process.mjs); native packaged launch on macOS/Windows is untested. |
| Client configuration | defaults, scoped changes, trust | M | — | — | [configuration check](../client/scripts/check-configuration.mjs), [VSIX smoke](../scripts/smoke-vsix.sh). |
| Client status | run generation and stale notification handling | M | — | — | [indexing-status check](../client/scripts/check-indexing-status.mjs). |
| Cache path | workspace and platform-specific path choice | M | — | — | [cache-path check](../client/scripts/check-cache-path.mjs); this check is not part of `npm run lint`. |
| Commands | contributed commands versus registration | M | — | — | [command check](../client/scripts/check-commands.mjs); this check is not part of `npm run lint`. |
| TypeScript/build | type check and production bundle | T | — | — | [client CI](../.github/workflows/ci.yml); compilation alone does not exercise extension branches. |
| VSIX packaging | six binaries, stubs, activation with mocked VS Code | M | — | — | [release workflow](../.github/workflows/release.yml), [VSIX smoke](../scripts/smoke-vsix.sh). |
| Native extension host | activate, language features, restart, dispose | — | — | — | No automated VS Code Extension Host test on any OS. |

## Measurement and interpretation

The quantitative baseline below uses source coverage for Rust production files,
excluding standalone test files. It measures the Linux test run only. A covered
line means one execution path reached it; it does not prove all branches,
protocol states, races, or platform behavior. The client checks compile
individual TypeScript modules into in-memory VM bundles; the client figures
below remap their V8 profiles to source using source maps from byte-identical
bundles. This covers five helper files, not the extension entrypoint.

Measured locally on Linux x86-64 at `df5e921` with `rustc 1.93.1`,
`cargo-llvm-cov 0.9.1`, and Node `24.13.0`. The full instrumented Rust
workspace test suite passed with one build job and one test thread. The report
contains 59 production Rust files. Stable instrumentation yielded **0 branch
counters**, so no branch percentage can be inferred from this run.

| Rust scope | Lines | Regions | Functions |
|---|---:|---:|---:|
| `php-lsp-completion` | 1,146 / 1,250 (91.7%) | 1,579 / 1,739 (90.8%) | 106 / 114 (93.0%) |
| `php-lsp-index` | 2,114 / 2,447 (86.4%) | 2,855 / 3,302 (86.5%) | 241 / 266 (90.6%) |
| `php-lsp-parser` | 11,714 / 13,510 (86.7%) | 16,578 / 19,353 (85.7%) | 937 / 1,066 (87.9%) |
| `php-lsp-server` | 41,907 / 49,472 (84.7%) | 57,316 / 68,792 (83.3%) | 3,856 / 4,420 (87.2%) |
| `php-lsp-types` | 224 / 269 (83.3%) | 381 / 454 (83.9%) | 41 / 46 (89.1%) |
| **Workspace** | **57,105 / 66,948 (85.3%)** | **78,709 / 93,640 (84.1%)** | **5,181 / 5,912 (87.6%)** |

Among files with at least 50 coverable lines, the clearest low-coverage areas
are [server `main.rs`](../server/crates/php-lsp-server/src/main.rs) at 9/101
(8.9%), [LSP conversions](../server/crates/php-lsp-server/src/lsp/conversions.rs)
at 29/67 (43.3%), [hierarchy](../server/crates/php-lsp-server/src/lsp/hierarchy.rs)
at 710/1,074 (66.1%), and [definition](../server/crates/php-lsp-server/src/lsp/definition.rs)
at 849/1,203 (70.6%). By absolute uncovered lines, the largest targets are
[parser resolution](../server/crates/php-lsp-parser/src/resolve.rs) (1,059),
[code actions](../server/crates/php-lsp-server/src/lsp/code_action.rs) (881),
and [framework inference](../server/crates/php-lsp-server/src/framework.rs)
(729). These counts identify where execution is missing; the JSON summary
does not establish which missing lines correspond to a particular failure mode.

All seven available client checks passed (`lint` runs five, and cache-path plus
commands were run explicitly); `npm run build` passed. Source-map-remapped c8
coverage of the **five exercised TypeScript helper files** is 655/701 lines
(93.4%), 129/158 branches (81.6%), and 49/56 functions (87.5%). The raw V8
profiles contain 79/88 executed function entries in their transpiled bundles;
those entries have a different denominator from mapped TypeScript functions.

| Client helper | Mapped lines | Mapped branches | Mapped functions |
|---|---:|---:|---:|
| `cachePath.ts` | 37/47 (78.7%) | 6/6 (100%) | 5/10 (50%) |
| `configuration.ts` | 178/178 (100%) | 20/20 (100%) | 8/8 (100%) |
| `indexingStatus.ts` | 151/173 (87.3%) | 38/54 (70.4%) | 10/10 (100%) |
| `lifecycle.ts` | 150/157 (95.5%) | 33/39 (84.6%) | 15/15 (100%) |
| `serverProcess.ts` | 139/146 (95.2%) | 32/39 (82.1%) | 11/13 (84.6%) |

`extension.ts` is 1,203 physical source lines, compared with 701 across the
five measured helpers, and has **no source-level coverage figure**. The
helper-only percentage cannot be generalized to the whole client.
`check:commands` tests package metadata and contributes no TypeScript VM
bundle. The release VSIX smoke exercises extension activation with mocks, but
does not produce a source-level profile or use a real Extension Host. The c8
line denominator equals the physical line count of these source-mapped files;
its line percentage should not be compared directly with LLVM's Rust line
percentage.

The matrix has 48 scenario rows: on Linux 39 have executed tests, seven have
mock-based checks, one has build-only evidence, and one (native Extension Host)
has no automated test or build evidence. The 39 Rust rows have macOS/Windows
build evidence but no native runtime test; one of those Rust rows lacks a
direct Linux protocol regression.
The nine client rows have no native macOS/Windows runtime test. These are
**scenario-evidence counts**, not a percent of all possible feature/state
combinations; the table deliberately lists high-value states rather than a
Cartesian product of every configuration and event sequence.

## Reproduction

Run sequentially on a machine with Rust `llvm-tools-preview` and
`cargo-llvm-cov 0.9.1` installed (`rustup component add llvm-tools-preview`;
`cargo install cargo-llvm-cov --version 0.9.1 --locked -j1`). The Rust command
limits both compilation and test execution to one thread. The generated JSON
belongs in ignored `server/target/`; do not commit it as source.

```sh
cd server
CARGO_BUILD_JOBS=1 CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 \
  cargo llvm-cov --workspace --json --summary-only \
  --output-path target/coverage-summary.json \
  --ignore-filename-regex '/tests/|_tests\.rs$' -- --test-threads=1
```

For the client baseline, run from `client/` (using a fresh directory on each
run). First collect raw V8 profiles:

```sh
client_cov_dir=$(mktemp -d)
mkdir -p "$client_cov_dir/raw"
NODE_V8_COVERAGE="$client_cov_dir/raw" npm run lint
NODE_V8_COVERAGE="$client_cov_dir/raw" npm run check:cache-path
NODE_V8_COVERAGE="$client_cov_dir/raw" npm run check:commands
npm run build
```

The test scripts name their in-memory VM bundles without a filesystem path.
Build byte-identical copies with source maps in ignored `client/out/coverage/`,
then point a copy of the raw profiles at those files. The equality assertion
guards against remapping a profile with a different bundle:

```sh
node --input-type=module - "$client_cov_dir" <<'JS'
import * as esbuild from 'esbuild';
import fs from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
const base = process.argv[2];
const mapped = path.join(base, 'mapped');
const bundles = path.resolve('out/coverage');
fs.mkdirSync(mapped, { recursive: true });
fs.mkdirSync(bundles, { recursive: true });
const entries = {
  'cachePath.bundle.cjs': 'cachePath.ts',
  'configuration.bundle.cjs': 'configuration.ts',
  'indexing-status.bundle.cjs': 'indexingStatus.ts',
  'lifecycle.bundle.cjs': 'lifecycle.ts',
  'server-process.bundle.cjs': 'serverProcess.ts',
};
for (const [name, entry] of Object.entries(entries)) {
  const options = { entryPoints: ['src/' + entry], bundle: true,
    format: 'cjs', platform: 'node', write: false, logLevel: 'silent' };
  const plain = await esbuild.build(options);
  const withMap = await esbuild.build({ ...options, sourcemap: 'external',
    sourcesContent: true, outfile: path.join(bundles, name) });
  const js = withMap.outputFiles.find(file => file.path.endsWith('.cjs'));
  const map = withMap.outputFiles.find(file => file.path.endsWith('.map'));
  if (js.text !== plain.outputFiles[0].text) {
    throw new Error('bundle mismatch: ' + name);
  }
  fs.writeFileSync(js.path, js.text + `//# sourceMappingURL=${name}.map\n`);
  fs.writeFileSync(map.path, map.text);
}
for (const name of fs.readdirSync(path.join(base, 'raw'))) {
  if (!name.endsWith('.json')) continue;
  const profile = JSON.parse(fs.readFileSync(path.join(base, 'raw', name)));
  for (const script of profile.result) {
    if (Object.hasOwn(entries, script.url)) {
      script.url = pathToFileURL(path.join(bundles, script.url)).href;
    }
  }
  fs.writeFileSync(path.join(mapped, name), JSON.stringify(profile));
}
JS
npm exec --yes --package=c8@12.0.0 -- c8 report \
  --temp-directory "$client_cov_dir/mapped" --reporter=text \
  --exclude='node_modules/**' --exclude='scripts/**' --exclude=''
```

The explicit empty final c8 exclude overrides its default output-directory
exclusion; the preceding excludes remove the check scripts and dependencies
from the denominator. This source-map technique is specific to the five
single-module test bundles; it does not instrument `extension.ts`.

`cargo llvm-cov` does not include doctest coverage by default. The denominator
does not include the VS Code client, bundled PHP stubs, PHP test fixtures, or
third-party dependencies. Coverage is a baseline for prioritizing tests, not a
release gate or a measure of semantic correctness.

## Next test additions

The first useful additions are a real Extension Host activation/restart test
with source coverage for `extension.ts`, a native Windows test for path,
symlink, and command-process behavior, and a native macOS counterpart. Add a
packaged-binary CLI/LSP startup test for `main.rs` and dedicated protocol
regressions for pre-create/pre-delete responses. After those integration gaps,
target uncovered hierarchy/definition branches and measure warm-cache and
first-hit vendor latency on the current schema. These are priority test areas,
not an estimate of how many individual test cases would make the project
"fully covered"; that number depends on the chosen behavior and risk model.
