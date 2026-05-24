import { copyFileSync, existsSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const clientDir = resolve(scriptDir, "..");
const repoDir = resolve(clientDir, "..");

const files = [
  ["README.md", "README.md"],
];

for (const [sourceName, targetName] of files) {
  const sourcePath = join(repoDir, sourceName);
  const targetPath = join(clientDir, targetName);

  if (!existsSync(sourcePath)) {
    throw new Error(`Missing package metadata source: ${sourcePath}`);
  }

  copyFileSync(sourcePath, targetPath);
  console.log(`Prepared ${targetName} for VSIX packaging`);
}
