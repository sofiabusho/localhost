#!/usr/bin/env python3
"""Sample CGI: sleeps before responding.

Used by tests/integration.sh to prove a slow CGI script does not stall the
single-threaded event loop for other clients — see the "concurrent CGI"
section of that script.
"""

import sys
import time

time.sleep(2.5)
sys.stdout.write("Content-Type: text/plain; charset=utf-8\n\nslow-done\n")
