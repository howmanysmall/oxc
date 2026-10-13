import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join as pathJoin } from "node:path";

import { execa } from "execa";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { PACKAGE_ROOT_PATH } from "./utils.ts";

const CLI_PATH = pathJoin(PACKAGE_ROOT_PATH, "dist/cli.js");

// A JS plugin and JS config, as their in-memory representation differs between runs
// (e.g. JS plugin rule IDs depend on the order plugins load in), which must not invalidate the cache.
const CONFIG = `
export default {
  jsPlugins: ["./plugin.mjs"],
  rules: { "cache-plugin/no-debugger": "error" },
};
`;

const plugin = (message: string) => `
export default {
  meta: { name: "cache-plugin" },
  rules: {
    "no-debugger": {
      create(context) {
        return {
          DebuggerStatement(node) {
            context.report({ message: ${JSON.stringify(message)}, node });
          },
        };
      },
    },
  },
};
`;

interface CacheFile {
  settings: string;
  files: Record<string, { config: string; diagnostics?: { message: string }[] }>;
}

describe("--cache", () => {
  let dir: string;

  beforeEach(async () => {
    dir = await mkdtemp(pathJoin(tmpdir(), "oxlint-cache-"));
    await writeFile(pathJoin(dir, "oxlint.config.mts"), CONFIG);
    await writeFile(pathJoin(dir, "plugin.mjs"), plugin("Debugger statement"));
    await writeFile(pathJoin(dir, "index.js"), "debugger;\n");
  });

  afterEach(async () => {
    await rm(dir, { recursive: true, force: true });
  });

  const run = () =>
    execa("node", [CLI_PATH, "--cache", "--format", "unix"], { cwd: dir, reject: false });

  const readCache = async (): Promise<CacheFile> =>
    JSON.parse(await readFile(pathJoin(dir, ".oxlintcache"), "utf8"));

  // Replace cached messages, so the output shows whether cached results were replayed.
  const tamperCache = async () => {
    const cache = await readCache();
    for (const entry of Object.values(cache.files)) {
      for (const diagnostic of entry.diagnostics ?? []) diagnostic.message = "FROM CACHE";
    }
    await writeFile(pathJoin(dir, ".oxlintcache"), JSON.stringify(cache));
  };

  it("replays cached results of JS plugins", async () => {
    const cold = await run();
    expect(cold.exitCode).toBe(1);
    expect(cold.stdout).toContain("Debugger statement");
    const coldCache = await readCache();

    await tamperCache();
    const warm = await run();
    expect(warm.exitCode).toBe(1);
    expect(warm.stdout).toContain("FROM CACHE");

    const warmCache = await readCache();
    expect(warmCache.settings).toBe(coldCache.settings);
    expect(warmCache.files["index.js"].config).toBe(coldCache.files["index.js"].config);
  });

  it("lints again when a JS plugin changes", async () => {
    await run();
    await tamperCache();

    // A different size, so the change is detected even on file systems with coarse modification times.
    await writeFile(pathJoin(dir, "plugin.mjs"), plugin("Changed debugger statement"));
    const { stdout } = await run();
    expect(stdout).not.toContain("FROM CACHE");
    expect(stdout).toContain("Changed debugger statement");
  });

  it("lints again when settings in the JS config change", async () => {
    await writeFile(
      pathJoin(dir, "oxlint.config.mts"),
      CONFIG.replace("rules:", 'settings: { foo: "a" }, rules:'),
    );
    await run();
    await tamperCache();

    await writeFile(
      pathJoin(dir, "oxlint.config.mts"),
      CONFIG.replace("rules:", 'settings: { foo: "b" }, rules:'),
    );
    const { stdout } = await run();
    expect(stdout).not.toContain("FROM CACHE");
    expect(stdout).toContain("Debugger statement");
  });

  it("lints again when the JS config changes", async () => {
    await run();
    await tamperCache();

    await writeFile(pathJoin(dir, "oxlint.config.mts"), CONFIG.replace('"error"', '"warn"'));
    const { stdout } = await run();
    expect(stdout).not.toContain("FROM CACHE");
    expect(stdout).toContain("Debugger statement");
  });
});
