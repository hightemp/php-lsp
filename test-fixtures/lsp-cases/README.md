# LSP Fixture Pack

This fixture set is designed for manual and automated checks of `php-lsp`.

## Scope
- Syntax diagnostics (`Syntax error`, `Missing syntax`).
- Semantic diagnostics currently implemented in server:
  - `Unresolved use statement`
  - `Unknown class`
  - `Unknown function` (namespaced calls)
  - constructor argument count mismatch
- LSP requests: hover, go-to-definition, references, rename, completion, document symbols.

## Structure
- `src/Model`, `src/Service`: valid project-like code for indexing and cross-file navigation.
- `src/Diagnostics`: semantic positive/negative cases.
- `src/Syntax`: intentionally broken PHP files.
- `src/Lsp`: focused fixtures per LSP feature and edge case.
- `src/PhpDoc`: phpDoc parsing/extraction cases (supported tags and malformed edge cases).

## Coverage Map
- `src/Diagnostics/SemanticUnknowns.php`:
  unresolved class `use`, unknown class in `new`, type hints, inheritance, and unknown namespaced function.
- `src/Diagnostics/ArgumentCountMismatch.php`:
  constructor arity errors (too few/too many) and variadic no-false-positive.
- `src/Diagnostics/BuiltinNoFalsePositive.php`:
  built-in classes/functions and special type names that should not be reported.
- `src/Diagnostics/FrameworkNoFalsePositive.php`:
  Symfony controller helpers and Laravel Eloquent dynamic members that should not produce framework-heavy false positives.
- `src/Syntax/MissingParen.php`, `src/Syntax/MissingExpression.php`, `src/Syntax/BrokenMixedPhpHtml.php`:
  parser recovery and multiple syntax diagnostic forms.
- `src/Lsp/HoverDefinition.php`:
  hover + go-to-definition for classes/methods/functions across files.
- `src/Lsp/ReferencesRename.php`:
  references/rename for class, method, property, and class constant.
- `src/Lsp/ReferencesUnsupported.php`:
  current limitation check: variable references return no results.
- `src/Lsp/RenameDisallowed.php`:
  expected rename failures (variables, built-ins, invalid names).
- `src/Lsp/CompletionIncomplete.php`:
  completion contexts: variable, member, static, namespace, free keyword (with intentionally incomplete code).
- `src/Lsp/DocumentSymbols.php`:
  namespace tree with const/function/interface/trait/enum/class + members.
- `src/PhpDoc/SupportedTags.php`:
  supported phpDoc tags coverage (`@param`, `@return`, `@var`, `@throws`, `@deprecated`, `@property*`, `@method`) on class/method/property/function.
- `src/PhpDoc/VirtualMembers.php`:
  usage sites for class-level PHPDoc virtual properties/methods, used by e2e completion, hover, definition, and rename-guard checks.
- `src/PhpDoc/EdgeCases.php`:
  unsupported/malformed tags that should be ignored safely, bare `@deprecated`, multiline summary, inline `@var`, nullable and intersection type examples.

## PHPDoc Behavior Matrix
- Parser: `@param`, `@return`, `@var`, `@throws`, `@deprecated`, `@property`, `@property-read`, `@property-write`, `@method`.
- Type expressions: nested generics, nullable, union/intersection, parenthesized type groups, callable return syntax are preserved best-effort.
- Hover: summaries, params, return, throws, var, deprecated, class-level virtual properties/methods.
- Completion: `$obj->` includes real members plus inherited PHPDoc virtual properties/methods.
- Completion resolve: virtual members return PHPDoc markdown from their declaring class.
- Definition: unresolved PHPDoc virtual members jump to the declaring doc-comment tag name.
- Rename: PHPDoc virtual members are intentionally rejected instead of editing doc-comments implicitly.
- Diagnostics: malformed PHPDoc tags are ignored safely and must not crash diagnostics.

## Notes
- Some files are intentionally invalid while typing (especially completion fixtures).
- `RenameDisallowed.php` includes cases that should fail by design:
  - variable rename is unsupported,
  - built-in symbol rename is blocked (when stubs are loaded).
- "All possible cases" here means all cases supported by the current implementation.
