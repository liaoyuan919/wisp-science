# syntax=docker/dockerfile:1.7

FROM rust:1.88-bookworm AS source
WORKDIR /src
COPY . .

FROM source AS test
RUN apt-get update \
    && apt-get install -y --no-install-recommends python3 python3-venv \
    && rm -rf /var/lib/apt/lists/*
RUN python3 -m venv /opt/wisp-venv \
    && /opt/wisp-venv/bin/pip install --no-cache-dir -r python/requirements-mcp.txt
ENV WISP_TEST_MCP_PYTHON=/opt/wisp-venv/bin/python
RUN cargo fmt --all -- --check
RUN cargo test -p wisp-server
RUN cargo test -p wisp-server bundled_mcp_catalog_is_read_only_and_license_gated -- --ignored

FROM test AS builder
RUN cargo build --locked --offline --release -p wisp-server

FROM python:3.12-slim-bookworm AS runtime
RUN pip install --no-cache-dir -r /dev/stdin <<'EOF'
mcp>=1.2
anyio>=4.0
requests>=2.31
httpx>=0.27
psutil>=5.9
lxml>=5.0
pandas>=2.0
EOF

RUN addgroup --system --gid 10001 wisp \
    && adduser --system --uid 10001 --ingroup wisp --home /nonexistent --no-create-home wisp
WORKDIR /app
COPY --from=builder /src/target/release/wisp-server /usr/local/bin/wisp-server
COPY mcp-servers/bio-tools /app/mcp-servers/bio-tools
COPY python/mcp_domain_smoke.py /app/python/mcp_domain_smoke.py

ENV WISP_BIND=0.0.0.0:8080 \
    WISP_RESOURCE_ROOT=/app \
    WISP_PYTHON=python3 \
    PYTHONDONTWRITEBYTECODE=1 \
    WISP_MAX_CONCURRENT=1 \
    WISP_REQUEST_TIMEOUT_SECS=110 \
    WISP_TOOL_TIMEOUT_SECS=60 \
    RUST_LOG=wisp_server=info

USER wisp
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 \
    CMD ["python3", "-c", "import urllib.request; assert urllib.request.urlopen('http://127.0.0.1:8080/health', timeout=3).read() == b'ok'"]
ENTRYPOINT ["/usr/local/bin/wisp-server"]
