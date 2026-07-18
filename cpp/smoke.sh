#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
CONF=$(mktemp)
cat >"$CONF" <<'EOF'
site {
    bind 127.0.0.1:19080;
    name localhost;
    max_body 1M;
    path / {
        methods GET POST DELETE;
        root www;
        index index.html;
        autoindex on;
        upload www/uploads;
    }
    path /cgi-bin {
        methods GET POST;
        root www/cgi;
        cgi .py /usr/bin/python3;
        cgi .sh /bin/bash;
    }
}
EOF
./cpp/localhost_cpp "$CONF" >/tmp/cpp-smoke.log 2>&1 &
PID=$!
cleanup() { kill "$PID" 2>/dev/null || true; wait "$PID" 2>/dev/null || true; rm -f "$CONF"; }
trap cleanup EXIT
sleep 0.5
echo "GET=$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:19080/)"
echo "CGI=$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:19080/cgi-bin/hello.py)"
echo "COOKIE=$(curl -s -D - -o /dev/null http://127.0.0.1:19080/ | tr -d '\r' | grep -i set-cookie | head -1)"
