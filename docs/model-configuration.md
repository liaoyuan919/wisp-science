# Model configuration

wisp-science calls remote LLM APIs through model profiles. Desktop users
configure these in **Settings -> Models**. Each row is a model profile with its
own display name, provider, API URL, model ID, advanced options, and API key.

The composer model picker binds the selected HTTP model to the current
conversation. Switching one populated conversation asks for confirmation and
does not change any other conversation. Empty conversations switch immediately
without a warning. The active profile in Settings remains the default for new
conversations.

Model profiles describe model access and capabilities for the **built-in Wisp
agent**. External coding agents (Codex / Claude via ACP) are configured under
**Settings → Models → ACP Agents** — see [ACP Agents](acp-agents.md). Do not put
an ACP launch command in an HTTP model profile.

For image workflows, mark an API profile as **Supports image input** and optionally **Use for image analysis**. Image attachments are sent directly to a visual input model. When the input model is non-visual, Wisp first calls the assigned vision model and passes its text observations to the input model. `view_image` and image reads use the assigned vision model in the same way. Raster image input supports PNG, JPEG, GIF, and WebP files up to 5 MiB.

## API providers

| Provider | Use when | Required fields |
| --- | --- | --- |
| OpenAI-compatible | DeepSeek, GLM, local gateways, or any `/chat/completions` compatible endpoint | API URL, Model ID, API key |
| OpenAI (Responses API) | OpenAI reasoning/tool-call models through `/v1/responses` | API URL, Model ID, API key |
| Anthropic | Claude API through `/v1/messages` | API URL, Model ID, API key |

API keys are stored in the OS keyring. They are not stored in SQLite.

The desktop app stores model profile metadata in `.wisp/wisp.sqlite`. Existing single-model installs are migrated into a `default` model profile the first time settings are loaded.

## Headless CLI

The `wisp-science` headless CLI uses environment variables and supports API providers:

```powershell
$env:WISP_PROVIDER = "openai"           # openai, openai_responses, or anthropic
$env:WISP_API_URL  = "https://api.deepseek.com"
$env:WISP_MODEL    = "deepseek-v4-pro"
$env:WISP_API_KEY  = "<your provider key>"
cargo run -p wisp-cli
```
