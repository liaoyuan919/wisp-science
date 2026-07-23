#!/usr/bin/env python3
"""Run one live, read-only MCP smoke check for every public bio-tools domain.

This script is intentionally a manual deployment check: it contacts real
third-party scientific databases and therefore must not run in offline CI.
Run it inside the wisp-server image, where the MCP Python dependencies and
vendored bio-tools catalog are already available.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import time
from pathlib import Path
from typing import Any

from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client


CASES: tuple[tuple[str, str, dict[str, Any]], ...] = (
    ("biomart", "list_marts", {}),
    ("biorxiv", "get_categories", {}),
    (
        "cancer-models",
        "cbioportal_list_studies",
        {"keyword": "breast", "max_records": 1},
    ),
    ("cellguide", "get_cell_type_info", {"cell_type": "T cell"}),
    (
        "chembl",
        "target_search",
        {"gene_symbol": "TP53", "organism": "Homo sapiens", "limit": 1},
    ),
    ("chemistry", "chebi_search", {"term": "aspirin", "max_results": 1}),
    (
        "clinical-genomics",
        "clingen_actionability",
        {"gene": "TP53", "context": "both"},
    ),
    (
        "clinical-trials",
        "search_trials",
        {"condition": "glioblastoma", "page_size": 1},
    ),
    ("drug-regulatory", "get_drug_statistics", {}),
    ("expression", "gtex_tissue_sites", {"dataset_id": "gtex_v8"}),
    ("genes-ontologies", "list_ontologies", {}),
    ("genomes", "ensembl_lookup", {"query": "TP53", "species": "homo_sapiens"}),
    ("human-genetics", "eqtl_list_datasets", {"max_records": 1}),
    (
        "literature",
        "openalex_search_works",
        {"query": "CRISPR", "max_records": 1},
    ),
    (
        "omics-archives",
        "arrayexpress_search_experiments",
        {"query": "TP53", "max_records": 1},
    ),
    ("protein-annotation", "search_interpro_entries", {"query": "p53"}),
    ("pubmed", "get_article_metadata", {"pmids": ["31452104"]}),
    ("regulation", "jaspar_list_collections", {}),
    ("research-resources", "get_antibody_registry_stats", {}),
    ("rna", "get_family", {"family": "RF00005"}),
    (
        "structures-interactions",
        "pdb_search_structures",
        {"uniprot_accession": "P04637", "max_rows": 1},
    ),
    ("variants", "dbsnp_get_rsids", {"rsids": ["rs28934578"]}),
    (
        "zinc",
        "zinc_search_by_id",
        {
            "zinc_ids": ["ZINC000000000012"],
            "max_results": 1,
            "timeout_s": 55.0,
        },
    ),
)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--launcher",
        type=Path,
        default=Path("/app/mcp-servers/bio-tools/run_server.py"),
    )
    parser.add_argument("--timeout-seconds", type=float, default=50.0)
    parser.add_argument(
        "--domain",
        action="append",
        dest="domains",
        help="run only this domain; repeat to select multiple domains",
    )
    return parser.parse_args()


async def run(args: argparse.Namespace) -> int:
    selected = set(args.domains or ())
    known = {domain for domain, _tool, _arguments in CASES}
    if unknown := selected - known:
        raise ValueError(f"unknown domains: {', '.join(sorted(unknown))}")
    cases = [
        case for case in CASES if not selected or case[0] in selected
    ]

    child_env = {
        "PATH": "/usr/local/bin:/usr/bin:/bin",
        "HOME": "/tmp",
        "TMPDIR": "/tmp",
        "PYTHONDONTWRITEBYTECODE": "1",
        "PYTHONUNBUFFERED": "1",
    }
    for parent_name, child_name in (
        ("WISP_NCBI_EMAIL", "NCBI_EMAIL"),
        ("WISP_NCBI_API_KEY", "NCBI_API_KEY"),
    ):
        if value := os.environ.get(parent_name):
            child_env[child_name] = value

    params = StdioServerParameters(
        command="python3",
        args=[str(args.launcher), "mcp_bio"],
        env=child_env,
    )
    results: list[dict[str, Any]] = []
    async with stdio_client(params) as (reader, writer):
        async with ClientSession(reader, writer) as session:
            await session.initialize()
            catalog_path = (
                args.launcher.parent / "lib" / "mcp_bio" / "domains.json"
            )
            catalog_domains = set(json.loads(catalog_path.read_text()))
            case_domains = {domain for domain, _tool, _arguments in CASES}
            if catalog_domains != case_domains:
                missing = sorted(catalog_domains - case_domains)
                stale = sorted(case_domains - catalog_domains)
                raise RuntimeError(
                    f"smoke coverage mismatch; missing={missing}, stale={stale}"
                )
            available_tools = {
                tool.name for tool in (await session.list_tools()).tools
            }
            unavailable = sorted(
                tool
                for _domain, tool, _arguments in cases
                if tool not in available_tools
            )
            if unavailable:
                raise RuntimeError(
                    f"selected tools are unavailable: {unavailable}"
                )
            for domain, tool, arguments in cases:
                started = time.monotonic()
                try:
                    response = await asyncio.wait_for(
                        session.call_tool(tool, arguments),
                        timeout=args.timeout_seconds,
                    )
                    elapsed = round(time.monotonic() - started, 2)
                    text = "".join(
                        getattr(item, "text", "") or ""
                        for item in (response.content or [])
                    )
                    is_error = bool(getattr(response, "isError", False))
                    ok = not is_error and bool(text.strip())
                    result: dict[str, Any] = {
                        "domain": domain,
                        "tool": tool,
                        "ok": ok,
                        "is_error": is_error,
                        "seconds": elapsed,
                        "bytes": len(text.encode()),
                    }
                    if not ok:
                        result["detail"] = text[:240].replace("\n", " ")
                except Exception as exc:
                    result = {
                        "domain": domain,
                        "tool": tool,
                        "ok": False,
                        "seconds": round(time.monotonic() - started, 2),
                        "detail": f"{type(exc).__name__}: {exc}"[:240],
                    }
                results.append(result)
                print(json.dumps(result, ensure_ascii=False), flush=True)

    failed = [result["domain"] for result in results if not result["ok"]]
    print(
        json.dumps(
            {
                "summary": {
                    "passed": len(results) - len(failed),
                    "total": len(results),
                    "failed": failed,
                }
            },
            ensure_ascii=False,
        ),
        flush=True,
    )
    return 1 if failed else 0


def main() -> None:
    raise SystemExit(asyncio.run(run(parse_args())))


if __name__ == "__main__":
    main()
