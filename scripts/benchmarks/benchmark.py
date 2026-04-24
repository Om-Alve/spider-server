#!/usr/bin/env python3
import argparse
import asyncio
import json
import os
import statistics
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import requests
from bs4 import BeautifulSoup


@dataclass
class RunResult:
    tool: str
    url: str
    ok: bool
    duration_s: float
    bytes_extracted: int
    link_count: int
    title: str | None
    error: str | None = None


def normalize_title(value: str | None) -> str | None:
    if value is None:
        return None
    normalized = " ".join(value.split()).strip()
    return normalized or None


def extract_title_from_html(html: str) -> str | None:
    soup = BeautifulSoup(html, "html.parser")
    if soup.title and soup.title.string:
        return normalize_title(soup.title.string)
    return None


def extract_link_count_from_html(html: str, base_url: str) -> int:
    soup = BeautifulSoup(html, "html.parser")
    count = 0
    for anchor in soup.find_all("a"):
        href = anchor.get("href")
        if href and href.strip():
            count += 1
    return count


def canonical_title_from_requests(url: str, timeout_s: int = 30) -> str | None:
    try:
        response = requests.get(url, timeout=timeout_s)
        response.raise_for_status()
        return extract_title_from_html(response.text)
    except Exception:
        return None


def normalize_proxy_entry(value: str) -> str | None:
    entry = value.strip()
    if not entry or entry.startswith("#"):
        return None

    if entry.startswith(("http://", "https://", "socks5://", "socks5h://")):
        return entry

    if "@" in entry and ":" in entry.split("@", 1)[1]:
        return f"http://{entry}"

    parts = entry.split(":")
    if len(parts) == 4:
        host, port, user, password = parts
        if host and port and user and password:
            return f"http://{user}:{password}@{host}:{port}"
    if len(parts) == 2:
        host, port = parts
        if host and port:
            return f"http://{host}:{port}"

    return None


def load_spider_proxies(
    proxy_list_url: str | None,
    proxy_file: str | None,
    timeout_s: int,
    max_items: int,
) -> list[str]:
    entries: list[str] = []

    if proxy_file:
        entries.extend(Path(proxy_file).read_text().splitlines())
    if proxy_list_url:
        response = requests.get(proxy_list_url, timeout=timeout_s)
        response.raise_for_status()
        entries.extend(response.text.splitlines())

    normalized: list[str] = []
    seen: set[str] = set()
    for raw in entries:
        proxy = normalize_proxy_entry(raw)
        if not proxy or proxy in seen:
            continue
        seen.add(proxy)
        normalized.append(proxy)
        if len(normalized) >= max_items:
            break
    return normalized


def run_spider_server(
    base_url: str,
    target_url: str,
    timeout_s: int,
    proxies: list[str] | None = None,
) -> RunResult:
    payload = {
        "url": target_url,
        "request_timeout_secs": min(10, timeout_s),
        "crawl_timeout_secs": min(20, timeout_s),
        "respect_robots_txt": True,
        "include_content": True,
        "max_content_chars": 20_000,
        "crawl_mode": "http",
        "anti_bot_profile": "camoufox_like",
        "response_format": "html",
    }
    if proxies:
        payload["proxies"] = proxies

    start = time.perf_counter()
    try:
        response = requests.post(
            f"{base_url.rstrip('/')}/scrape",
            json=payload,
            timeout=timeout_s,
        )
        elapsed = time.perf_counter() - start
        response.raise_for_status()
        data = response.json()

        page = data.get("page")
        page_content = (page or {}).get("content", "") if isinstance(page, dict) else ""
        title = normalize_title(extract_title_from_html(page_content)) if page_content else None
        links_extracted = int((page or {}).get("links_extracted", 0)) if isinstance(page, dict) else 0

        return RunResult(
            tool="spider-server",
            url=target_url,
            ok=True,
            duration_s=elapsed,
            bytes_extracted=len(page_content.encode("utf-8")),
            link_count=links_extracted,
            title=title,
        )
    except Exception as exc:
        elapsed = time.perf_counter() - start
        return RunResult(
            tool="spider-server",
            url=target_url,
            ok=False,
            duration_s=elapsed,
            bytes_extracted=0,
            link_count=0,
            title=None,
            error=str(exc),
        )


def run_crawl4ai_error(url: str, duration_s: float, message: str) -> RunResult:
    return RunResult(
        tool="crawl4ai",
        url=url,
        ok=False,
        duration_s=duration_s,
        bytes_extracted=0,
        link_count=0,
        title=None,
        error=message,
    )


async def run_crawl4ai_batch(
    urls: list[str], iterations: int, timeout_s: int
) -> list[RunResult]:
    from crawl4ai import AsyncWebCrawler, BrowserConfig, CacheMode, CrawlerRunConfig

    browser_config = BrowserConfig(headless=True, verbose=False)
    run_config = CrawlerRunConfig(
        page_timeout=timeout_s * 1000,
        cache_mode=CacheMode.BYPASS,
        only_text=False,
        check_robots_txt=False,
        semaphore_count=8,
        verbose=False,
    )

    results: list[RunResult] = []
    try:
        async with AsyncWebCrawler(config=browser_config) as crawler:
            for _ in range(iterations):
                for target_url in urls:
                    start = time.perf_counter()
                    try:
                        result = await crawler.arun(url=target_url, config=run_config)
                        elapsed = time.perf_counter() - start
                        if not getattr(result, "success", False):
                            results.append(
                                run_crawl4ai_error(
                                    target_url,
                                    elapsed,
                                    getattr(
                                        result, "error_message", "unknown crawl failure"
                                    ),
                                )
                            )
                            continue

                        html = getattr(result, "html", "") or ""
                        title = extract_title_from_html(html)
                        links = getattr(result, "links", None) or {}
                        internal = links.get("internal", []) if isinstance(links, dict) else []
                        external = links.get("external", []) if isinstance(links, dict) else []

                        results.append(
                            RunResult(
                                tool="crawl4ai",
                                url=target_url,
                                ok=True,
                                duration_s=elapsed,
                                bytes_extracted=len(html.encode("utf-8")),
                                link_count=len(internal) + len(external),
                                title=title,
                            )
                        )
                    except Exception as exc:
                        elapsed = time.perf_counter() - start
                        results.append(run_crawl4ai_error(target_url, elapsed, str(exc)))
    except Exception as exc:
        error_text = str(exc)
        for _ in range(iterations):
            for target_url in urls:
                results.append(run_crawl4ai_error(target_url, 0.0, error_text))

    return results


def run_firecrawl(target_url: str, timeout_s: int) -> RunResult:
    from firecrawl import FirecrawlApp

    api_key = os.getenv("FIRECRAWL_API_KEY")
    if not api_key:
        return RunResult(
            tool="firecrawl",
            url=target_url,
            ok=False,
            duration_s=0.0,
            bytes_extracted=0,
            link_count=0,
            title=None,
            error="FIRECRAWL_API_KEY not set",
        )

    start = time.perf_counter()
    try:
        app = FirecrawlApp(api_key=api_key, timeout=timeout_s)
        # firecrawl-py v4 in this environment only exposes parse(), not scrape_url().
        # Treat this as unavailable for URL benchmarks.
        _ = app
        elapsed = time.perf_counter() - start
        return RunResult(
            tool="firecrawl",
            url=target_url,
            ok=False,
            duration_s=elapsed,
            bytes_extracted=0,
            link_count=0,
            title=None,
            error="installed firecrawl client lacks URL scrape endpoint (parse-only)",
        )
    except Exception as exc:
        elapsed = time.perf_counter() - start
        return RunResult(
            tool="firecrawl",
            url=target_url,
            ok=False,
            duration_s=elapsed,
            bytes_extracted=0,
            link_count=0,
            title=None,
            error=str(exc),
        )


def ensure_spider_server(
    server_cmd: str, base_url: str, startup_wait_s: int
) -> subprocess.Popen[str]:
    process = subprocess.Popen(
        server_cmd,
        shell=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        text=True,
    )
    deadline = time.time() + startup_wait_s
    health_url = f"{base_url.rstrip('/')}/healthz"
    while time.time() < deadline:
        if process.poll() is not None:
            raise RuntimeError("spider-server process exited before readiness check")
        try:
            response = requests.get(health_url, timeout=1.0)
            if response.status_code == 200:
                return process
        except Exception:
            pass
        time.sleep(0.5)
    raise RuntimeError("timed out waiting for spider-server /healthz")


def run_sync_tool_batch(
    tool: str,
    urls: list[str],
    iterations: int,
    timeout_s: int,
    server_url: str,
    spider_proxies: list[str] | None = None,
) -> list[RunResult]:
    results: list[RunResult] = []
    for _ in range(iterations):
        for url in urls:
            if tool == "spider-server":
                results.append(
                    run_spider_server(
                        server_url, url, timeout_s, proxies=spider_proxies
                    )
                )
            elif tool == "firecrawl":
                results.append(run_firecrawl(url, timeout_s))
            else:
                results.append(
                    RunResult(
                        tool=tool,
                        url=url,
                        ok=False,
                        duration_s=0.0,
                        bytes_extracted=0,
                        link_count=0,
                        title=None,
                        error="unknown tool",
                    )
                )
    return results


def stop_process(process: subprocess.Popen[str] | None) -> None:
    if not process:
        return
    try:
        process.terminate()
        process.wait(timeout=5)
    except Exception:
        process.kill()


def score_quality(result: RunResult, canonical_title: str | None) -> dict[str, Any]:
    title_match = None
    if canonical_title and result.title:
        title_match = int(result.title.strip().lower() == canonical_title.strip().lower())

    return {
        "title_match": title_match,
        "has_content": int(result.bytes_extracted > 0),
        "has_links": int(result.link_count > 0),
    }


def aggregate(results: list[RunResult], canonical_titles: dict[str, str | None]) -> dict[str, Any]:
    by_tool: dict[str, list[RunResult]] = {}
    for item in results:
        by_tool.setdefault(item.tool, []).append(item)

    summary: dict[str, Any] = {}
    for tool, tool_results in by_tool.items():
        successes = [r for r in tool_results if r.ok]
        quality = [score_quality(r, canonical_titles.get(r.url)) for r in tool_results if r.ok]

        summary[tool] = {
            "runs": len(tool_results),
            "successes": len(successes),
            "avg_latency_s": round(
                statistics.mean(r.duration_s for r in successes), 4
            )
            if successes
            else None,
            "p95_latency_s": round(
                statistics.quantiles([r.duration_s for r in successes], n=20)[-1], 4
            )
            if len(successes) >= 2
            else (round(successes[0].duration_s, 4) if successes else None),
            "avg_bytes": int(statistics.mean(r.bytes_extracted for r in successes))
            if successes
            else 0,
            "avg_links": round(statistics.mean(r.link_count for r in successes), 2)
            if successes
            else 0.0,
            "title_match_rate": round(
                statistics.mean(
                    q["title_match"] for q in quality if q["title_match"] is not None
                ),
                4,
            )
            if any(q["title_match"] is not None for q in quality)
            else None,
            "content_presence_rate": round(
                statistics.mean(q["has_content"] for q in quality), 4
            )
            if quality
            else None,
            "links_presence_rate": round(
                statistics.mean(q["has_links"] for q in quality), 4
            )
            if quality
            else None,
            "errors": [r.error for r in tool_results if not r.ok],
        }

    return summary


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Benchmark spider-server vs Crawl4AI/Firecrawl")
    parser.add_argument(
        "--targets",
        default="scripts/benchmarks/targets.txt",
        help="Path to newline-separated URL targets",
    )
    parser.add_argument(
        "--iterations",
        type=int,
        default=2,
        help="Number of runs per URL per tool",
    )
    parser.add_argument(
        "--timeout",
        type=int,
        default=60,
        help="Per-run timeout in seconds",
    )
    parser.add_argument(
        "--output",
        default="scripts/benchmarks/results.json",
        help="Output JSON report path",
    )
    parser.add_argument(
        "--server-url",
        default="http://127.0.0.1:8080",
        help="spider-server base URL",
    )
    parser.add_argument(
        "--server-cmd",
        default="cargo run --release",
        help="Command to start spider-server",
    )
    parser.add_argument(
        "--skip-server-start",
        action="store_true",
        help="Assume spider-server is already running",
    )
    parser.add_argument(
        "--startup-wait",
        type=int,
        default=120,
        help="Max seconds to wait for spider-server startup health check",
    )
    parser.add_argument(
        "--tools",
        default="spider-server,crawl4ai,firecrawl",
        help="Comma-separated tools to run",
    )
    parser.add_argument(
        "--spider-proxy-list-url",
        default=os.getenv("SPIDER_PROXY_LIST_URL"),
        help="Optional proxy list URL (newline-separated entries) for spider-server",
    )
    parser.add_argument(
        "--spider-proxy-file",
        default=None,
        help="Optional local proxy file for spider-server",
    )
    parser.add_argument(
        "--spider-proxy-timeout",
        type=int,
        default=20,
        help="Timeout (seconds) for downloading proxy list URL",
    )
    parser.add_argument(
        "--spider-proxy-max",
        type=int,
        default=120,
        help="Max proxy entries to pass to spider-server",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    target_file = Path(args.targets)
    urls = [line.strip() for line in target_file.read_text().splitlines() if line.strip()]
    tools = [item.strip() for item in args.tools.split(",") if item.strip()]

    canonical_titles = {url: canonical_title_from_requests(url) for url in urls}
    spider_proxies: list[str] | None = None
    if "spider-server" in tools:
        spider_proxies = load_spider_proxies(
            proxy_list_url=args.spider_proxy_list_url,
            proxy_file=args.spider_proxy_file,
            timeout_s=args.spider_proxy_timeout,
            max_items=args.spider_proxy_max,
        )
        if spider_proxies:
            print(f"Loaded {len(spider_proxies)} proxies for spider-server")

    server_process = None
    if "spider-server" in tools and not args.skip_server_start:
        server_process = ensure_spider_server(
            args.server_cmd, args.server_url, args.startup_wait
        )

    results: list[RunResult] = []
    try:
        for tool in tools:
            if tool == "crawl4ai":
                results.extend(asyncio.run(run_crawl4ai_batch(urls, args.iterations, args.timeout)))
            else:
                results.extend(
                    run_sync_tool_batch(
                        tool,
                        urls,
                        args.iterations,
                        args.timeout,
                        args.server_url,
                        spider_proxies=spider_proxies,
                    )
                )
    finally:
        stop_process(server_process)

    summary = aggregate(results, canonical_titles)
    report = {
        "meta": {
            "iterations": args.iterations,
            "targets": urls,
            "tools": tools,
            "generated_at_unix": time.time(),
        },
        "canonical_titles": canonical_titles,
        "summary": summary,
        "runs": [r.__dict__ for r in results],
    }

    output_path = Path(args.output)
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(json.dumps(report, indent=2))

    print(json.dumps(summary, indent=2))
    print(f"\nWrote full report to: {output_path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
