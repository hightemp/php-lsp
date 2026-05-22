import * as fs from "fs";
import * as os from "os";
import * as path from "path";

export function cacheBaseDir(env: NodeJS.ProcessEnv = process.env): string {
  if (env.XDG_CACHE_HOME) {
    return env.XDG_CACHE_HOME;
  }
  if (env.HOME) {
    return path.join(env.HOME, ".cache");
  }
  return os.tmpdir();
}

export function normalizeCachePath(value: string): string {
  try {
    return fs.realpathSync(value).replace(/\\/g, "/");
  } catch {
    return value.replace(/\\/g, "/");
  }
}

export function stableHashStrings(parts: string[]): string {
  let hash = 0xcbf29ce484222325n;
  const prime = 0x100000001b3n;
  const mask = 0xffffffffffffffffn;

  for (const part of parts) {
    for (const byte of Buffer.from(part, "utf8")) {
      hash ^= BigInt(byte);
      hash = (hash * prime) & mask;
    }
    hash ^= 0xffn;
    hash = (hash * prime) & mask;
  }

  return hash.toString(16).padStart(16, "0");
}

export function phpLspCacheDirForRoot(root: string): string {
  return phpLspCacheDirForRootWithBase(cacheBaseDir(), root);
}

export function phpLspCacheDirForRootWithBase(baseDir: string, root: string): string {
  const rootHash = stableHashStrings([normalizeCachePath(root)]);
  return path.join(baseDir, "php-lsp", rootHash);
}
