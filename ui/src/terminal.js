import { Terminal } from "/vendor/xterm.mjs";
import { FitAddon } from "/vendor/xterm-addon-fit.mjs";

const params = new URLSearchParams(window.location.search);
const sessionId = params.get("session");
const embedded = params.get("embedded") === "1";
// WebView2 does not consistently inject Tauri's initialization script into a
// same-origin child frame. The parent app always has the bridge, so embedded
// terminals use it as a fallback instead of rendering a disconnected xterm.
const tauri = window.__TAURI__ ?? (embedded && window.parent !== window ? window.parent.__TAURI__ : undefined);
const core = tauri?.core;
const currentWindow = tauri?.window?.getCurrentWindow?.();
document.body.classList.toggle("embedded", embedded);
const title = document.getElementById("terminal-title");
const context = document.getElementById("terminal-context");
const cwd = document.getElementById("terminal-cwd");
const status = document.getElementById("terminal-status");
const errorBox = document.getElementById("terminal-error");
const terminateButton = document.getElementById("terminal-terminate");
const pinButton = document.getElementById("terminal-pin");

const dark = !window.matchMedia("(prefers-color-scheme: light)").matches;
const terminal = new Terminal({
  cursorBlink: true,
  cursorStyle: "bar",
  fontFamily: "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace",
  fontSize: 13,
  lineHeight: 1.2,
  scrollback: 10_000,
  allowProposedApi: false,
  theme: dark
    ? { background: "#151614", foreground: "#ebe8e2", cursor: "#4cc5b5", selectionBackground: "#315d57" }
    : { background: "#faf9f6", foreground: "#171714", cursor: "#0f766e", selectionBackground: "#b8ddd7" },
});
const fit = new FitAddon();
terminal.loadAddon(fit);
terminal.open(document.getElementById("terminal-container"));

function showError(value) {
  const message = value instanceof Error ? value.message : String(value);
  errorBox.textContent = message;
  errorBox.hidden = false;
  status.classList.add("error");
}

function decodeBase64(value) {
  const binary = atob(value);
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) {
    bytes[index] = binary.charCodeAt(index);
  }
  return bytes;
}

function applySummary(summary) {
  title.textContent = summary.title || "Terminal";
  context.textContent = summary.contextId || "";
  cwd.textContent = summary.displayCwd || "";
  document.title = summary.title || "Wisp Terminal";
  status.classList.toggle("exited", !summary.running);
  terminateButton.disabled = !summary.running;
}

let pendingInput = "";
let inputFlushScheduled = false;
let inputChain = Promise.resolve();

function queueInput(data) {
  pendingInput += data;
  if (inputFlushScheduled) return;
  inputFlushScheduled = true;
  queueMicrotask(() => {
    inputFlushScheduled = false;
    const data = pendingInput;
    pendingInput = "";
    inputChain = inputChain
      .then(() => core.invoke("write_terminal", { sessionId, data }))
      .catch(showError);
  });
}

let resizeTimer;
function resizePty({ rows, cols }) {
  if (!rows || !cols) return;
  clearTimeout(resizeTimer);
  resizeTimer = setTimeout(() => {
    core.invoke("resize_terminal", { sessionId, rows, cols }).catch(showError);
  }, 30);
}

let fitFrame;
function scheduleFit(focus = false) {
  cancelAnimationFrame(fitFrame);
  fitFrame = requestAnimationFrame(() => {
    // A second frame lets the iframe finish applying a newly selected tab's
    // display/height before FitAddon measures its character grid.
    fitFrame = requestAnimationFrame(() => {
      const container = document.getElementById("terminal-container");
      if (container.clientWidth === 0 || container.clientHeight === 0) return;
      try {
        fit.fit();
        if (focus) terminal.focus();
      } catch (error) {
        showError(error);
      }
    });
  });
}

async function start() {
  if (!core || !sessionId) {
    throw new Error("Terminal session bridge is unavailable.");
  }
  terminal.onData(queueInput);
  terminal.onResize(resizePty);
  const container = document.getElementById("terminal-container");
  new ResizeObserver(() => scheduleFit(true)).observe(container);
  window.addEventListener("resize", () => scheduleFit(false));
  window.addEventListener("focus", () => scheduleFit(true));
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden) scheduleFit(true);
  });
  container.addEventListener("pointerdown", () => terminal.focus());

  const onEvent = new core.Channel();
  onEvent.onmessage = (message) => {
    if (message.event === "output") {
      terminal.write(decodeBase64(message.data.base64));
    } else if (message.event === "exit") {
      status.classList.add("exited");
      terminateButton.disabled = true;
      terminal.write(`\r\n\x1b[90m[process exited with code ${message.data.exitCode}]\x1b[0m\r\n`);
    } else if (message.event === "error") {
      showError(message.data.message);
    }
  };
  const summary = await core.invoke("attach_terminal", { sessionId, onEvent });
  applySummary(summary);
  scheduleFit(true);
}

terminateButton.addEventListener("click", async () => {
  if (!window.confirm("Terminate this terminal session?")) return;
  terminateButton.disabled = true;
  try {
    await core.invoke("terminate_terminal", { sessionId });
  } catch (error) {
    terminateButton.disabled = false;
    showError(error);
  }
});

pinButton.addEventListener("click", async () => {
  if (!currentWindow) return;
  const pinned = pinButton.getAttribute("aria-pressed") !== "true";
  try {
    await currentWindow.setAlwaysOnTop(pinned);
    pinButton.setAttribute("aria-pressed", String(pinned));
    pinButton.classList.toggle("active", pinned);
  } catch (error) {
    showError(error);
  }
});

start().catch(showError);
