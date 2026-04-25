from __future__ import annotations

from .base import Scraper, ScrapeResult
from datetime import datetime
import asyncio
import os
from typing import Any

import requests


class SpiderServerScraper(Scraper):
    """Scrape via a running spider-server instance (this repository).

    The base URL can be overridden with `SPIDER_SERVER_URL`
    (default: http://127.0.0.1:8080). The anti-bot profile can be set via
    `SPIDER_SERVER_PROFILE` (default: camoufox_stealth).
    """

    def __init__(self) -> None:
        self.base_url = os.getenv("SPIDER_SERVER_URL", "http://127.0.0.1:8080").rstrip("/")
        self.profile = os.getenv("SPIDER_SERVER_PROFILE", "camoufox_stealth")
        self.timeout_s = int(os.getenv("SPIDER_SERVER_TIMEOUT_S", "45"))
        self.crawl_mode = os.getenv("SPIDER_SERVER_MODE", "http")
        self.response_format = os.getenv("SPIDER_SERVER_FORMAT", "html")
        # The analyzer special-cases `markdown` and strips markdown syntax. Our
        # text response is rendered by html2text (markdown-flavoured), so report
        # it as markdown to get clean tokenization downstream.
        self.report_format = "markdown" if self.response_format == "text" else self.response_format

    def check_environment(self) -> bool:
        try:
            r = requests.get(f"{self.base_url}/healthz", timeout=5)
            return r.status_code == 200
        except Exception:
            return False

    async def scrape(self, url: str, run_id: str) -> ScrapeResult:
        loop = asyncio.get_event_loop()
        return await loop.run_in_executor(None, self._scrape_sync, url, run_id)

    def _scrape_sync(self, url: str, run_id: str) -> ScrapeResult:
        payload: dict[str, Any] = {
            "url": url,
            "request_timeout_secs": min(20, self.timeout_s),
            "crawl_timeout_secs": self.timeout_s,
            "respect_robots_txt": False,
            "include_content": True,
            "max_content_chars": 1_000_000,
            "crawl_mode": self.crawl_mode,
            "anti_bot_profile": self.profile,
            "response_format": self.response_format,
        }
        created_at = datetime.now().isoformat()
        try:
            r = requests.post(
                f"{self.base_url}/scrape",
                json=payload,
                timeout=self.timeout_s + 10,
            )
            if r.status_code >= 500:
                return ScrapeResult(
                    run_id=run_id,
                    scraper="spider_server",
                    url=url,
                    status_code=r.status_code,
                    error=f"server returned {r.status_code}: {r.text[:200]}",
                    content_size=0,
                    format=self.report_format,
                    created_at=created_at,
                    content=None,
                )
            data = r.json()
            page = data.get("page") or {}
            content = page.get("content") or ""
            status_code = int(page.get("status_code") or r.status_code or 0)
            error = page.get("error")
            content_size = len(content.encode("utf-8")) if content else 0
            return ScrapeResult(
                run_id=run_id,
                scraper="spider_server",
                url=url,
                status_code=status_code,
                error=error if (status_code >= 400 or not content) else None,
                content_size=content_size,
                format=self.report_format,
                created_at=created_at,
                content=content or None,
            )
        except requests.exceptions.Timeout:
            return ScrapeResult(
                run_id=run_id,
                scraper="spider_server",
                url=url,
                status_code=408,
                error="Timeout: spider-server did not respond in time",
                content_size=0,
                format=self.report_format,
                created_at=created_at,
                content=None,
            )
        except Exception as e:
            return ScrapeResult(
                run_id=run_id,
                scraper="spider_server",
                url=url,
                status_code=500,
                error=f"{type(e).__name__}: {e}",
                content_size=0,
                format=self.report_format,
                created_at=created_at,
                content=None,
            )
