import { test, expect } from "@playwright/test";
import { tauriMock } from "./mock-tauri";

test.beforeEach(async ({ page }) => {
  // Install the Tauri bridge mock before the page's wasm runs.
  await page.addInitScript(tauriMock);
});

test("opens a bundled demo as a read-only transcript", async ({ page }) => {
  await page.goto("/");
  await expect(page.getByText("Open demo")).toBeVisible();

  await page.getByRole("button", { name: "Open demo" }).click();
  await expect(page.getByText("Open a demo session")).toBeVisible();
  await page.getByText("Design a genome-wide CRISPR").click();

  // The demo request renders as the user turn…
  await expect(page.getByText("Design a genome-wide CRISPR knockout screen targeting all kinases.")).toBeVisible();
  // …and the agent's final report renders as the assistant turn.
  await expect(page.getByText("Human Kinome CRISPR-KO Screen")).toBeVisible();
});

test("send streams a mocked assistant reply", async ({ page }) => {
  await page.goto("/");
  await page.getByPlaceholder(/Ask wisp-science/i).fill("hello there");
  await page.getByRole("button", { name: "Send" }).click();
  // Deltas "Hello " + "from mock wisp-science." accumulate into one assistant bubble.
  await expect(page.getByText("Hello from mock wisp-science.")).toBeVisible({ timeout: 10_000 });
});

test("settings modal shows the saved provider", async ({ page }) => {
  await page.goto("/");
  await page.getByRole("button", { name: "Settings" }).click();
  await expect(page.locator("select")).toHaveValue("openai");
  await page.getByRole("button", { name: "Cancel" }).click();
});

test("settings normalizes a blank stored provider to openai", async ({ page }) => {
  await page.goto("/");
  await page.getByRole("button", { name: "Settings" }).click();
  await expect(page.locator("select")).toHaveValue("openai");
  await page.getByRole("button", { name: "Valid" }).click();
  await expect(page.locator(".settings-status")).toHaveText("Validated openai with deepseek-v4-pro");
});

test("editing API URL keeps provider state and display aligned", async ({ page }) => {
  await page.goto("/");
  await page.getByRole("button", { name: "Settings" }).click();
  await page.getByLabel("API URL").fill("https://api.deepseek.com");
  await expect(page.locator("select")).toHaveValue("openai");
  await page.getByRole("button", { name: "Valid" }).click();
  await expect(page.locator(".settings-status")).toHaveText("Validated openai with deepseek-v4-pro");
});

test("settings can validate current API config", async ({ page }) => {
  await page.goto("/");
  await page.getByRole("button", { name: "Settings" }).click();
  await page.getByRole("button", { name: "Valid" }).click();
  await expect(page.locator(".settings-status")).toHaveText("Validated openai with deepseek-v4-pro");
});

test("settings validation rejects blank required fields", async ({ page }) => {
  await page.goto("/");
  await page.getByRole("button", { name: "Settings" }).click();
  await page.getByLabel("API URL").fill("");
  await page.getByRole("button", { name: "Valid" }).click();
  await expect(page.locator(".settings-status")).toHaveText("Validation failed: API URL is required.");
});

test("provider switch fills current API defaults", async ({ page }) => {
  await page.goto("/");
  await page.getByRole("button", { name: "Settings" }).click();
  await page.locator("select").selectOption("openai_responses");
  await expect(page.getByLabel("API URL")).toHaveValue("https://api.openai.com/v1");
  await expect(page.getByLabel("Model")).toHaveValue("gpt-5.5");
  await page.locator("select").selectOption("anthropic");
  await expect(page.getByLabel("API URL")).toHaveValue("https://api.anthropic.com");
  await expect(page.getByLabel("Model")).toHaveValue("claude-sonnet-5");
});
