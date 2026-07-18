#!/usr/bin/env python3
"""Sample CGI: echoes method, PATH_INFO, query, and POST body."""

import os
import sys


def main() -> None:
    body = sys.stdin.read()
    lines = [
        "Content-Type: text/plain; charset=utf-8",
        "",
        f"REQUEST_METHOD={os.environ.get('REQUEST_METHOD', '')}",
        f"PATH_INFO={os.environ.get('PATH_INFO', '')}",
        f"QUERY_STRING={os.environ.get('QUERY_STRING', '')}",
        f"CONTENT_LENGTH={os.environ.get('CONTENT_LENGTH', '')}",
        f"BODY={body}",
    ]
    sys.stdout.write("\n".join(lines) + "\n")


if __name__ == "__main__":
    main()
