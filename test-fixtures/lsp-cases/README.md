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
- `src/PhpDoc/EdgeCases.php`:
  unsupported/malformed tags that should be ignored safely, bare `@deprecated`, multiline summary, inline `@var`, nullable and intersection type examples.

## Notes
- Some files are intentionally invalid while typing (especially completion fixtures).
- `RenameDisallowed.php` includes cases that should fail by design:
  - variable rename is unsupported,
  - built-in symbol rename is blocked (when stubs are loaded).
- "All possible cases" here means all cases supported by the current implementation.
