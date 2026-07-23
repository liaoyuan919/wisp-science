# Qingxiaoda deployment

`wisp-server` exposes the read-only Wisp Science agent through the
OpenAI-compatible API expected by Qingxiaoda.

## Public endpoints

- `GET /health` — unauthenticated liveness check, returns `ok`.
- `GET /v1/models` — Bearer-authenticated model list.
- `POST /v1/chat/completions` — Bearer-authenticated streaming or non-streaming
  chat completion.

Configure Qingxiaoda with:

- Base URL: `https://api.wispto.top/v1`
- Model: `wisp-science-v1`
- API key: the value of `WISP_SERVER_API_KEY`, **not** the model provider key.

The public process starts from an empty Wisp tool registry. It loads only tools
from the bundled `mcp_bio` aggregate whose MCP annotation contains
`readOnlyHint=true`. Tool schemas stay deferred behind `search_mcp_tools` and
`use_mcp_tool`. Local files, shell, Python/R, memory, skills, custom MCP
commands, and MCP App artifact writes are not registered.

## Container deployment

1. Copy `deploy/.env.example` to `deploy/.env`.
2. Set the GHCR image, service token, model provider settings, NCBI contact
   email, and ACME email. `WISP_NCBI_EMAIL` is passed only to the isolated
   read-only MCP process; model and service credentials are still excluded.
3. From `deploy/`, run:

   ```sh
   docker compose pull
   docker compose up -d
   docker compose ps
   ```

The application is only exposed to the internal Compose network. Caddy owns
ports 80 and 443 and preserves SSE streaming.

Do not build the Rust workspace on the 1 GiB server. The repository workflow
tests and builds inside Docker, publishes a Linux amd64 image to GHCR, and the
server only pulls that image.

## Smoke checks

```sh
curl -i https://api.wispto.top/health

curl -sS https://api.wispto.top/v1/models \
  -H "Authorization: Bearer $WISP_SERVER_API_KEY"

curl -N https://api.wispto.top/v1/chat/completions \
  -H "Authorization: Bearer $WISP_SERVER_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "wisp-science-v1",
    "stream": true,
    "messages": [{"role": "user", "content": "用一句话介绍你的能力"}],
    "max_tokens": 64
  }'
```

For resource observation:

```sh
docker stats --no-stream
docker compose logs --tail=100 wisp-server
```

Run one real, bounded read-only lookup in every public research domain:

```sh
docker compose run --rm --entrypoint python3 \
  wisp-server /app/python/mcp_domain_smoke.py
```

This manual check contacts third-party scientific databases and is deliberately
not part of offline CI. A non-zero exit identifies the failed domains without
printing API keys or full upstream payloads.

Keep the current 768 MiB container limit for the initial 1 GiB host. If catalog
startup or a representative single request approaches the limit, first split
the aggregate into lazily started domain processes; if it remains unstable,
upgrade the host to 2 GiB rather than removing scientific domains.

## BYOK boundary

The server intentionally does not expose `/setup` yet. Qingxiaoda's published
OpenAI-compatible contract does not provide a stable, trustworthy end-user
identifier, so separate users cannot safely be mapped to separate provider
keys. Until the platform supplies such an identifier, the container uses the
server-side provider configuration and enforces concurrency, request, tool,
output, and daily token limits.

Never use source IP, a user-supplied ID, or a marker hidden in chat content as
the BYOK identity.
