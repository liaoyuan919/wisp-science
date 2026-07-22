# Chat activity flow design QA

## Evidence

- Reference: `/mnt/c/Users/XUZHOU~1/AppData/Local/Temp/wispterm-clipboard-1784677207100.png`
- Implementation: `/tmp/wisp-chat-flow-implementation.png`
- Side-by-side comparison: `/tmp/wisp-chat-flow-comparison.png`
- Viewport: 1588 × 1077 CSS px, device scale factor 1
- State: completed `STEPSDEMO` turn with commentary, reasoning, three tool calls, and a final answer; disclosures collapsed

## Review

- Hierarchy and spacing: commentary remains in the transcript, reasoning is a small collapsed row, and each adjacent tool batch has its own compact disclosure. The previous single oversized activity panel is gone.
- Typography and color: intentionally uses Wisp's existing system font, muted text, border, and surface tokens rather than copying Codex App branding.
- Controls: group and tool disclosures are native buttons with keyboard activation, visible focus treatment, and synchronized `aria-expanded` state.
- Content and assets: no new raster or placeholder assets are used. The mock copy differs from the reference so the test exercises a realistic scientific workflow.
- Runtime quality: the focused Playwright flow verifies event order, collapsed defaults, Enter/Space interaction, and an empty page-error/console-error log.

## Comparison history

1. The first comparison showed the intended three-layer hierarchy but exposed click-only `div` disclosures; these were replaced with accessible buttons.
2. Console instrumentation then exposed disposed-owner errors while transient streaming rows were replaced. Streaming assistant rows now use the lightweight renderer until the turn completes, and the mock keeps the turn active until its `Done` event.
3. The final focused comparison and interaction run passed with no P0, P1, or P2 findings.

Final result: passed.
