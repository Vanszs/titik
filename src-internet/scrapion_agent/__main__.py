"""
Headless one-shot entrypoint for scrapion_agent.

Usage:
    python -m scrapion_agent --json "<query or URL>"

stdout is always valid JSON (report on success, error object on failure).
All library debug output is redirected to stderr.
"""

import sys
import os
import json
import argparse

# Skip the Firefox auto-install check; titik --install-internet handles that separately.
os.environ["SCRAPION_SKIP_BROWSER_CHECK"] = "1"


def main():
    parser = argparse.ArgumentParser(
        description="Headless one-shot web search and scrape",
        prog="python -m scrapion_agent",
    )
    parser.add_argument("query", help="Search query or URL to scrape")
    parser.add_argument(
        "--json",
        action="store_true",
        default=True,
        help="Emit result as JSON (default and only output format)",
    )
    args = parser.parse_args()

    query = args.query

    # Save real stdout before redirecting.
    real_stdout = sys.stdout

    # Redirect stdout to stderr so all library debug prints go to stderr,
    # keeping real stdout clean for the final JSON payload.
    sys.stdout = sys.stderr

    report = None
    try:
        from scrapion_agent import Client
        report = Client(skip_browser_check=True).run(query)
    except Exception as exc:
        sys.stdout = real_stdout
        error_payload = json.dumps({"error": str(exc), "query": query}, ensure_ascii=False)
        real_stdout.write(error_payload)
        real_stdout.flush()
        sys.exit(1)
    finally:
        # Always restore stdout so subsequent code can write to it.
        sys.stdout = real_stdout

    real_stdout.write(report.to_json())
    real_stdout.flush()


if __name__ == "__main__":
    main()
