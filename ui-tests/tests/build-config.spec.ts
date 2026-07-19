import { expect, test } from "@playwright/test";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";

const repositoryRoot = resolve(__dirname, "../..");
const readRepositoryFile = (path: string) =>
  readFileSync(resolve(repositoryRoot, path), "utf8");

test("Tauri dev, UI tests, and release builds use isolated Trunk outputs", () => {
  const tauriConfig = JSON.parse(readRepositoryFile("src-tauri/tauri.conf.json"));
  const macosTauriConfig = JSON.parse(readRepositoryFile("src-tauri/tauri.macos.conf.json"));
  const devScript = readRepositoryFile("ui/dev.ps1");
  const buildScript = readRepositoryFile("ui/build.ps1");
  const playwrightConfig = readRepositoryFile("ui-tests/playwright.config.ts");

  expect(tauriConfig.build.devUrl).toBe("http://localhost:1421");
  expect(devScript).toContain("$devPort = 1421");
  expect(devScript).toContain("--dist dist-dev");
  expect(devScript).toContain("exit $LASTEXITCODE");
  expect(buildScript).toContain("trunk build --release --dist dist");
  expect(buildScript).toContain("exit $LASTEXITCODE");
  expect(macosTauriConfig.build.beforeDevCommand.script).toContain("node sync-vendor.mjs && trunk serve");
  expect(macosTauriConfig.build.beforeBuildCommand.script).toContain("node sync-vendor.mjs && trunk build");
  expect(playwrightConfig).toContain('UI_TEST_PORT ?? "1422"');
  expect(playwrightConfig).toContain("--dist dist-test");
  expect(playwrightConfig).toContain("--no-autoreload");
});

test("DOCX and PPTX import one pinned shared JSZip chunk", () => {
  const manifestBytes = readFileSync(resolve(repositoryRoot, "ui/vendor-src/office-build.json"));
  const manifest = JSON.parse(manifestBytes.toString("utf8"));
  const names = Object.keys(manifest.files).sort();

  expect(names).toEqual([
    "docx-preview.mjs",
    manifest.jszipChunk,
    "pptx-preview.mjs",
  ].sort());
  expect(manifest.jszipChunk).toMatch(/^office-chunks\/chunk-[A-Z0-9]+\.mjs$/);

  for (const name of names) {
    const bytes = readFileSync(resolve(repositoryRoot, "ui/vendor-src", name));
    expect(bytes.length).toBe(manifest.files[name].bytes);
    expect(createHash("sha256").update(bytes).digest("hex"))
      .toBe(manifest.files[name].sha256);
  }

  const sharedImport = `./${manifest.jszipChunk}`;
  expect(readRepositoryFile("ui/vendor-src/docx-preview.mjs")).toContain(sharedImport);
  expect(readRepositoryFile("ui/vendor-src/pptx-preview.mjs")).toContain(sharedImport);
});
