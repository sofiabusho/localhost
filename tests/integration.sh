#!/usr/bin/env bash
# Exhaustive integration checks for localhost (audit-oriented).
#
# Usage (from repo root, Linux/WSL):
#   bash tests/integration.sh
#
# The script builds the binary, starts it on ports 18080–18082, runs curl/python
# checks, then stops the server. Do not point siege at hosts you do not own.

set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

CONF="$ROOT/tests/integration.conf"
BIN="$ROOT/target/debug/localhost"
HOST="http://127.0.0.1:18080"
HOST2="http://127.0.0.1:18081"
HOST3="http://127.0.0.1:18082"
PID=""
PASS=0
FAIL=0

cleanup() {
  if [[ -n "${PID}" ]] && kill -0 "$PID" 2>/dev/null; then
    kill "$PID" 2>/dev/null || true
    wait "$PID" 2>/dev/null || true
  fi
  rm -rf "$ROOT/tests/tmp"
}
trap cleanup EXIT

check() {
  local name="$1"
  local expected="$2"
  local actual="$3"
  if [[ "$actual" == "$expected" ]]; then
    echo "  PASS  $name"
    PASS=$((PASS + 1))
  else
    echo "  FAIL  $name (expected='$expected' got='$actual')"
    FAIL=$((FAIL + 1))
  fi
}

check_contains() {
  local name="$1"
  local haystack="$2"
  local needle="$3"
  if echo "$haystack" | grep -qi -- "$needle"; then
    echo "  PASS  $name"
    PASS=$((PASS + 1))
  else
    echo "  FAIL  $name (missing '$needle')"
    FAIL=$((FAIL + 1))
  fi
}

echo "======================================================="
echo "  localhost — integration suite"
echo "======================================================="

echo ""
echo "[build]"
cargo build -q
if [[ ! -x "$BIN" ]]; then
  echo "binary missing: $BIN"
  exit 1
fi

mkdir -p "$ROOT/tests/tmp/uploads"
rm -f "$ROOT/tests/tmp/uploads/"* 2>/dev/null || true

echo ""
echo "[start server]"
"$BIN" "$CONF" >/tmp/localhost-integration.log 2>&1 &
PID=$!
sleep 0.4
if ! kill -0 "$PID" 2>/dev/null; then
  echo "server failed to start; log:"
  cat /tmp/localhost-integration.log || true
  exit 1
fi

# ── Static ────────────────────────────────────────────────
echo ""
echo "[1] Static files"
STATUS=$(curl -s -o /dev/null -w "%{http_code}" "$HOST/")
check "GET / → 200" "200" "$STATUS"
HDRS=$(curl -s -D - -o /dev/null "$HOST/")
check_contains "Content-Type html" "$HDRS" "content-type: text/html"
check_contains "Content-Length present" "$HDRS" "content-length:"

STATUS=$(curl -s -o /dev/null -w "%{http_code}" "$HOST/style.css")
check "GET /style.css → 200" "200" "$STATUS"

# ── Errors ────────────────────────────────────────────────
echo ""
echo "[2] Error pages"
STATUS=$(curl -s -o /dev/null -w "%{http_code}" "$HOST/no-such-page.html")
check "missing file → 404" "404" "$STATUS"
BODY=$(curl -s "$HOST/no-such-page.html")
check_contains "custom 404 body" "$BODY" "404"

STATUS=$(curl -s -o /dev/null -w "%{http_code}" --path-as-is "$HOST/../../Cargo.toml")
check "traversal → 403" "403" "$STATUS"

# ── Methods ───────────────────────────────────────────────
echo ""
echo "[3] Method enforcement"
STATUS=$(curl -s -o /dev/null -w "%{http_code}" -X PUT "$HOST/")
check "PUT → 405" "405" "$STATUS"
STATUS=$(curl -s -o /dev/null -w "%{http_code}" -X PATCH "$HOST/")
check "PATCH → 405" "405" "$STATUS"

# ── Redirect ──────────────────────────────────────────────
echo ""
echo "[4] Redirect"
HDRS=$(curl -s -D - -o /dev/null "$HOST/old")
STATUS=$(echo "$HDRS" | head -1 | tr -d '\r' | awk '{print $2}')
check "GET /old → 301" "301" "$STATUS"
check_contains "Location /" "$HDRS" "location: /"

# ── Upload + DELETE ───────────────────────────────────────
echo ""
echo "[5] Upload + DELETE"
printf 'hello-upload' > "$ROOT/tests/tmp/payload.txt"
STATUS=$(curl -s -o /dev/null -w "%{http_code}" \
  -F "file=@$ROOT/tests/tmp/payload.txt;filename=upload_test.txt" \
  "$HOST/")
check "multipart POST → 201" "201" "$STATUS"
if [[ -f "$ROOT/tests/tmp/uploads/upload_test.txt" ]]; then
  echo "  PASS  upload landed on disk"
  PASS=$((PASS + 1))
else
  echo "  FAIL  upload landed on disk"
  FAIL=$((FAIL + 1))
fi
GOT=$(cat "$ROOT/tests/tmp/uploads/upload_test.txt" 2>/dev/null || true)
check "upload bytes intact" "hello-upload" "$GOT"

# Place a deletable file under www for DELETE
printf 'bye' > "$ROOT/www/delete_me.txt"
STATUS=$(curl -s -o /dev/null -w "%{http_code}" -X DELETE "$HOST/delete_me.txt")
check "DELETE existing → 204" "204" "$STATUS"
if [[ ! -f "$ROOT/www/delete_me.txt" ]]; then
  echo "  PASS  file removed after DELETE"
  PASS=$((PASS + 1))
else
  echo "  FAIL  file removed after DELETE"
  FAIL=$((FAIL + 1))
  rm -f "$ROOT/www/delete_me.txt"
fi
STATUS=$(curl -s -o /dev/null -w "%{http_code}" -X DELETE "$HOST/delete_me.txt")
check "DELETE missing → 404" "404" "$STATUS"

# ── Body limit ────────────────────────────────────────────
echo ""
echo "[6] Body size limit"
dd if=/dev/zero bs=1024 count=8 2>/dev/null > "$ROOT/tests/tmp/too_big.bin"
STATUS=$(curl -s -o /dev/null -w "%{http_code}" \
  -H "Content-Type: application/octet-stream" \
  --data-binary @"$ROOT/tests/tmp/too_big.bin" \
  "$HOST/")
check "oversized POST → 413" "413" "$STATUS"

# ── Chunked ───────────────────────────────────────────────
echo ""
echo "[7] Chunked request body"
STATUS=$(python3 - <<'PY'
import socket
req = (
    b"POST / HTTP/1.1\r\n"
    b"Host: localhost\r\n"
    b"Transfer-Encoding: chunked\r\n"
    b"Content-Type: text/plain\r\n"
    b"\r\n"
    b"4\r\n"
    b"ping\r\n"
    b"0\r\n"
    b"\r\n"
)
s = socket.create_connection(("127.0.0.1", 18080), 2)
s.sendall(req)
data = s.recv(256)
s.close()
print(data.split()[1].decode() if data.startswith(b"HTTP/") else "000")
PY
)
check "chunked POST → 201" "201" "$STATUS"

# ── CGI ───────────────────────────────────────────────────
echo ""
echo "[8] CGI"
if [[ -x /usr/bin/python3 ]]; then
  STATUS=$(curl -s -o /dev/null -w "%{http_code}" "$HOST/cgi-bin/hello.py")
  check "GET CGI → 200" "200" "$STATUS"
  BODY=$(curl -s "$HOST/cgi-bin/hello.py?q=1")
  check_contains "CGI PATH_INFO" "$BODY" "PATH_INFO="
  check_contains "CGI QUERY" "$BODY" "QUERY_STRING=q=1"
  BODY=$(curl -s -X POST -H "Content-Type: text/plain" --data "hi" "$HOST/cgi-bin/hello.py")
  check_contains "CGI POST body" "$BODY" "BODY=hi"
  if [[ -x /bin/bash ]]; then
    STATUS=$(curl -s -o /dev/null -w "%{http_code}" "$HOST/cgi-bin/hello.sh")
    check "GET shell CGI → 200" "200" "$STATUS"
    BODY=$(curl -s "$HOST/cgi-bin/hello.sh?x=2")
    check_contains "shell CGI QUERY" "$BODY" "QUERY_STRING=x=2"
  else
    echo "  SKIP  /bin/bash not available"
  fi
else
  echo "  SKIP  python3 not available"
fi

# ── Sessions ──────────────────────────────────────────────
echo ""
echo "[9] Cookies / sessions"
HDRS=$(curl -s -D - -o /dev/null "$HOST/")
check_contains "Set-Cookie session_id" "$HDRS" "set-cookie: session_id="
SID=$(echo "$HDRS" | tr -d '\r' | grep -i '^set-cookie:' | head -1 | sed -n 's/.*session_id=\([^;]*\).*/\1/p')
HITS=$(echo "$HDRS" | tr -d '\r' | grep -i '^x-session-hits:' | awk '{print $2}')
check "first hit count" "1" "$HITS"
HDRS2=$(curl -s -D - -o /dev/null -H "Cookie: session_id=$SID" "$HOST/")
SID2=$(echo "$HDRS2" | tr -d '\r' | grep -i '^set-cookie:' | head -1 | sed -n 's/.*session_id=\([^;]*\).*/\1/p')
HITS2=$(echo "$HDRS2" | tr -d '\r' | grep -i '^x-session-hits:' | awk '{print $2}')
check "session id stable" "$SID" "$SID2"
check "second hit count" "2" "$HITS2"

# ── Multi-port + vhost ────────────────────────────────────
echo ""
echo "[10] Multiple ports + name-based vhost"
STATUS=$(curl -s -o /dev/null -w "%{http_code}" "$HOST/")
check "port 18080" "200" "$STATUS"
STATUS=$(curl -s -o /dev/null -w "%{http_code}" "$HOST2/")
check "port 18081" "200" "$STATUS"
STATUS=$(curl -s -o /dev/null -w "%{http_code}" \
  --resolve alt.local:18080:127.0.0.1 "http://alt.local:18080/")
check "Host alt.local on shared port" "200" "$STATUS"

# ── Autoindex ─────────────────────────────────────────────
echo ""
echo "[11] Directory listing"
BODY=$(curl -s "$HOST3/")
check_contains "autoindex links" "$BODY" "<a href"

# ── Bad request ───────────────────────────────────────────
echo ""
echo "[12] Malformed request"
STATUS=$(python3 - <<'PY'
import socket
s = socket.create_connection(("127.0.0.1", 18080), 2)
s.sendall(b"NOTHTTP\r\n\r\n")
data = s.recv(256)
s.close()
print(data.split()[1].decode() if data.startswith(b"HTTP/") else "000")
PY
)
check "garbage → 400" "400" "$STATUS"

# ── Bad config samples ────────────────────────────────────
echo ""
echo "[13] Bad configuration files"
BAD1=$(mktemp)
cat >"$BAD1" <<'EOF'
site {
    bind 127.0.0.1:1;
    bind 0.0.0.0:1;
    name a;
    path / { methods GET; root www; }
}
EOF
if timeout 2 "$BIN" "$BAD1" >/tmp/localhost-bad1.log 2>&1; then
  echo "  FAIL  conflicting ports rejected at startup"
  FAIL=$((FAIL + 1))
else
  echo "  PASS  conflicting ports rejected at startup"
  PASS=$((PASS + 1))
fi
rm -f "$BAD1"

BAD2=$(mktemp)
cat >"$BAD2" <<'EOF'
site {
    name only;
    path / { methods GET; root www; }
}
EOF
if timeout 2 "$BIN" "$BAD2" >/tmp/localhost-bad2.log 2>&1; then
  echo "  FAIL  missing bind rejected"
  FAIL=$((FAIL + 1))
else
  echo "  PASS  missing bind rejected"
  PASS=$((PASS + 1))
fi
rm -f "$BAD2"

# ── Summary ───────────────────────────────────────────────
TOTAL=$((PASS + FAIL))
echo ""
echo "======================================================="
echo "  Results: $PASS/$TOTAL passed"
if [[ "$FAIL" -eq 0 ]]; then
  echo "  All integration checks passed."
else
  echo "  $FAIL failure(s) — see /tmp/localhost-integration.log"
fi
echo "======================================================="
echo ""
echo "Stress (manual, own server only):"
echo "  siege -b -c 50 -t 30S http://127.0.0.1:18080/"
echo ""

exit "$FAIL"
