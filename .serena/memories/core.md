# Core

- Monorepo: Rust LSP server under `server/`, VS Code client under `client/`, PHP fixtures under `test-fixtures/`, packaging helpers under `scripts/`.
- Server crate map and ownership details: `mem:server/core`.
- Client extension details: `mem:client/core`.
- Build/test commands: `mem:suggested_commands` and completion gate: `mem:task_completion`.
- Project-specific coding and architecture conventions: `mem:conventions`.
- Tech stack and toolchain notes: `mem:tech_stack`.
- `server/data/stubs/` is a phpstorm-stubs git submodule; tests/packaging must tolerate missing stubs unless explicitly initialized.