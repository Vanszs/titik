# src-internet

Vendored Python scraper for koma's researcher (opt-in internet access tier).
Uses Playwright/Firefox headless to search DuckDuckGo and scrape result pages,
returning a structured JSON report.

**Invocation:**
```
python -m scrapion_agent --json "<query or URL>"
```

Requires the venv and Firefox browser installed by `koma --install-internet`.
Run from this directory (or add it to `sys.path`) so `scrapion_agent` is importable.
