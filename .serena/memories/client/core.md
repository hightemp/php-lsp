# Client Core

- VS Code extension lives in `client/`.
- TypeScript source entry: `client/src/extension.ts`; build output is `client/out/`.
- Use `npm ci` from `client/` for exact lockfile dependencies.
- Validation: `npm run lint` (`tsc --noEmit`) and `npm run build` (esbuild bundle).