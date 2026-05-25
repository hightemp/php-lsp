import assert from "node:assert/strict";
import fs from "node:fs";

const packageJson = JSON.parse(fs.readFileSync("package.json", "utf8"));
const extensionSource = fs.readFileSync("src/extension.ts", "utf8");

const expectedCommands = [
  {
    id: "phpLsp.restartServer",
    title: "Restart Language Server",
  },
  {
    id: "phpLsp.clearCacheAndRestart",
    title: "Clear PHP LSP Cache and Restart",
  },
  {
    id: "phpLsp.showStatus",
    title: "Show Language Server Status",
  },
  {
    id: "phpLsp.showServerVersion",
    title: "Show Language Server Version",
  },
];

const contributedCommands = new Map(
  packageJson.contributes.commands.map((command) => [command.command, command]),
);

for (const command of expectedCommands) {
  const contribution = contributedCommands.get(command.id);
  assert.ok(contribution, `Missing package.json contribution for ${command.id}`);
  assert.equal(contribution.title, command.title);
  assert.equal(contribution.category, "PHP");
  assert.ok(
    extensionSource.includes(`"${command.id}"`),
    `Missing extension registration/reference for ${command.id}`,
  );
}

console.log(`command registration OK: ${expectedCommands.length} commands`);
