#!/usr/bin/env bash
# Audit dry-run for localhost, organized to mirror raw/audit.md section by
# section. Every check that curl/siege/cargo can verify runs against a real,
# live instance and prints PASS/FAIL with expected vs. actual. Anything that
# genuinely needs a human (browser devtools, a verbal source-code walkthrough)
# prints MANUAL with a one-line instruction instead of faking a result.
#
# Uses the real example.conf as shipped (ports 8080/8081/9090, sites
# "localhost"/"alt.local"/"solo.local", max_body 1M/512k/512k) plus a couple
# of throwaway configs built inline for the port-conflict checks. Boots and
# tears down its own server instances; you don't need one already running.
#
# Usage: bash tests/audit-dry-run.sh
#
# This is additive — it does not modify tests/integration.sh, example.conf,
# or tests/integration.conf.

set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

BIN="$ROOT/target/debug/localhost"
WORK="$(mktemp -d /tmp/localhost-audit-dryrun.XXXXXX)"
PASS=0
FAIL=0
MANUAL=0

# All server PIDs we start, so cleanup can stop everything even if a section
# exits early.
declare -a SERVER_PIDS=()

cleanup() {
  for p in "${SERVER_PIDS[@]:-}"; do
    if [[ -n "$p" ]] && kill -0 "$p" 2>/dev/null; then
      kill "$p" 2>/dev/null || true
      wait "$p" 2>/dev/null || true
    fi
  done
  # www/uploads only ever holds .gitkeep outside of test runs — sweep
  # anything this run (or a leftover DELETE-less run) left behind rather
  # than tracking every generated filename individually.
  find "$ROOT/www/uploads" -type f ! -name '.gitkeep' -delete 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

# ── helpers ──────────────────────────────────────────────
pass() { echo "  PASS  $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL  $1"; FAIL=$((FAIL + 1)); }
manual() { echo "  MANUAL  $1"; MANUAL=$((MANUAL + 1)); }

check() {
  local name="$1" expected="$2" actual="$3"
  if [[ "$actual" == "$expected" ]]; then
    pass "$name (expected='$expected' got='$actual')"
  else
    fail "$name (expected='$expected' got='$actual')"
  fi
}

check_contains() {
  local name="$1" haystack="$2" needle="$3"
  if echo "$haystack" | grep -qi -- "$needle"; then
    pass "$name (found '$needle')"
  else
    fail "$name (missing '$needle')"
  fi
}

start_server() {
  # start_server <conf> <logfile>  ->  prints pid on success, empty on failure
  local conf="$1" log="$2"
  "$BIN" "$conf" >"$log" 2>&1 &
  local pid=$!
  SERVER_PIDS+=("$pid")
  sleep 0.5
  if kill -0 "$pid" 2>/dev/null; then
    echo "$pid"
  else
    echo ""
  fi
}

stop_server() {
  local pid="$1"
  [[ -z "$pid" ]] && return
  kill "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
}

section() {
  echo ""
  echo "======================================================="
  echo "  $1"
  echo "======================================================="
}

echo "[build]"
cargo build -q 2>&1
if [[ ! -x "$BIN" ]]; then
  echo "binary missing: $BIN"
  exit 1
fi
mkdir -p "$ROOT/www/uploads"

# ─────────────────────────────────────────────────────────
section "Functional — audit.md § Functional"
# This whole section of audit.md is Socratic ("is the student able to
# justify...", "read the code with me") — nothing here is a pass/fail curl
# check by nature. Prep notes with real file/function citations for each
# question are in docs/audit-talking-points.md.
manual "How does an HTTP server work? -> docs/audit-talking-points.md § How does an HTTP server work?"
manual "Which function does I/O multiplexing, how does it work? -> § I/O multiplexing"
manual "Only one epoll instance for reads/writes? -> § Single epoll instance"
manual "Why one epoll, and how was it achieved? -> § Why one epoll instance"
manual "Trace epoll -> read/write of a client, one read/write per wake? -> § Tracing epoll_wait to a client read/write"
manual "Are I/O return values checked properly? -> § Are I/O return values checked?"
manual "If a socket errors, is the client removed? -> § If a socket errors, is the client removed?"
manual "Is reading/writing ALWAYS through epoll (incl. CGI)? -> § Is reading/writing ALWAYS through epoll?"

# ─────────────────────────────────────────────────────────
section "Configuration file — audit.md § Configuration file"
echo ""
echo "Booting example.conf (real config: sites 'localhost' on 127.0.0.1:8080/8081,"
echo "'alt.local' on 127.0.0.1:8080, 'solo.local' on 127.0.0.1:9090)"
MAIN_LOG="$WORK/example.log"
MAIN_PID=$(start_server "$ROOT/example.conf" "$MAIN_LOG")
if [[ -z "$MAIN_PID" ]]; then
  fail "server starts from example.conf"
  cat "$MAIN_LOG"
else
  pass "server starts from example.conf (pid=$MAIN_PID)"

  echo ""
  echo "-- single server, single port (solo.local, 127.0.0.1:9090) --"
  STATUS=$(curl -s -o /dev/null -w "%{http_code}" --max-time 3 http://127.0.0.1:9090/)
  check "GET solo.local:9090 -> 200" "200" "$STATUS"

  echo ""
  echo "-- multiple servers, different ports (8080 / 8081 / 9090) --"
  S8080=$(curl -s -o /dev/null -w "%{http_code}" --max-time 3 http://127.0.0.1:8080/)
  S8081=$(curl -s -o /dev/null -w "%{http_code}" --max-time 3 http://127.0.0.1:8081/)
  S9090=$(curl -s -o /dev/null -w "%{http_code}" --max-time 3 http://127.0.0.1:9090/)
  check "port 8080 responds" "200" "$S8080"
  check "port 8081 responds" "200" "$S8081"
  check "port 9090 responds" "200" "$S9090"

  echo ""
  echo "-- multiple servers, different hostnames, same ip:port (8080) --"
  echo "   'localhost' allows GET+POST+DELETE on /, 'alt.local' only allows GET"
  echo "   -- so a POST distinguishes which SiteBlock actually got picked."
  DEFAULT_POST=$(curl -s -o /dev/null -w "%{http_code}" --max-time 3 \
    -F "file=@$ROOT/example.conf;filename=audit_hostcheck_default.txt" http://127.0.0.1:8080/)
  ALT_POST=$(curl -s -o /dev/null -w "%{http_code}" --max-time 3 \
    --resolve alt.local:8080:127.0.0.1 \
    -F "file=@$ROOT/example.conf;filename=audit_hostcheck_alt.txt" http://alt.local:8080/)
  check "POST default Host (site 'localhost') on :8080 -> 201 (POST allowed)" "201" "$DEFAULT_POST"
  check "POST --resolve alt.local:8080 -> 405 (alt.local is GET-only)" "405" "$ALT_POST"

  echo ""
  echo "-- custom error pages (errpage 404 www/errors/404.html) --"
  BODY=$(curl -s --max-time 3 http://127.0.0.1:8080/does-not-exist.html)
  check_contains "404 body is the configured custom page" "$BODY" "Custom error page from www/errors/404.html"

  echo ""
  echo "-- client body limit (site 'localhost': max_body 1M = 1048576 bytes) --"
  head -c 1048576 /dev/urandom > "$WORK/at_limit.bin"
  head -c 1048577 /dev/urandom > "$WORK/over_limit.bin"
  AT_LIMIT=$(curl -s -o /dev/null -w "%{http_code}" --max-time 5 \
    -H "Content-Type: application/octet-stream" --data-binary @"$WORK/at_limit.bin" http://127.0.0.1:8080/)
  OVER_LIMIT=$(curl -s -o /dev/null -w "%{http_code}" --max-time 5 \
    -H "Content-Type: application/octet-stream" --data-binary @"$WORK/over_limit.bin" http://127.0.0.1:8080/)
  check "POST exactly 1048576 bytes (== max_body) -> not rejected" "201" "$AT_LIMIT"
  check "POST 1048577 bytes (max_body + 1) -> 413" "413" "$OVER_LIMIT"
  # save_raw() names the accepted at-limit upload from a timestamp (no
  # filename was supplied) — the EXIT trap sweeps www/uploads/ for it.

  echo ""
  echo "-- routes taken into account (/, /old, /cgi-bin) --"
  ROOT_STATUS=$(curl -s -o /dev/null -w "%{http_code}" --max-time 3 http://127.0.0.1:8080/)
  OLD_HDRS=$(curl -s -D - -o /dev/null --max-time 3 http://127.0.0.1:8080/old)
  OLD_STATUS=$(echo "$OLD_HDRS" | head -1 | tr -d '\r' | awk '{print $2}')
  CGI_BODY=$(curl -s --max-time 3 http://127.0.0.1:8080/cgi-bin/hello.py)
  check "GET / (root www) -> 200" "200" "$ROOT_STATUS"
  check "GET /old (redirect 301 /) -> 301" "301" "$OLD_STATUS"
  check_contains "GET /cgi-bin/hello.py routed to CGI" "$CGI_BODY" "REQUEST_METHOD=GET"

  echo ""
  echo "-- default file when path is a directory (index index.html) --"
  DIR_CTYPE=$(curl -s -D - -o /dev/null --max-time 3 http://127.0.0.1:8080/ | grep -i '^content-type' | tr -d '\r')
  DIR_BODY=$(curl -s --max-time 3 http://127.0.0.1:8080/)
  check_contains "GET / Content-Type is text/html" "$DIR_CTYPE" "text/html"
  check_contains "GET / body is www/index.html" "$DIR_BODY" "It works"

  echo ""
  echo "-- accepted methods per route (DELETE with/without permission) --"
  echo "hello from audit dry-run" > "$WORK/deleteme.txt"
  UP_STATUS=$(curl -s -o /dev/null -w "%{http_code}" --max-time 3 \
    -F "file=@$WORK/deleteme.txt;filename=audit_deleteme.txt" http://127.0.0.1:8080/)
  check "upload file to delete later -> 201" "201" "$UP_STATUS"
  DELETE_ALLOWED=$(curl -s -o /dev/null -w "%{http_code}" --max-time 3 -X DELETE http://127.0.0.1:8080/uploads/audit_deleteme.txt)
  check "DELETE /uploads/audit_deleteme.txt (route allows DELETE) -> 204" "204" "$DELETE_ALLOWED"
  DELETE_GONE=$(curl -s -o /dev/null -w "%{http_code}" --max-time 3 -X DELETE http://127.0.0.1:8080/uploads/audit_deleteme.txt)
  check "DELETE same file again -> 404" "404" "$DELETE_GONE"
  DELETE_DISALLOWED=$(curl -s -o /dev/null -w "%{http_code}" --max-time 3 -X DELETE http://127.0.0.1:8080/old)
  check "DELETE /old (route only allows GET) -> 405" "405" "$DELETE_DISALLOWED"
fi

# ─────────────────────────────────────────────────────────
section "Methods and cookies — audit.md § Methods and cookies"
if [[ -n "${MAIN_PID:-}" ]]; then
  echo ""
  echo "-- GET --"
  check "GET / -> 200" "200" "$(curl -s -o /dev/null -w "%{http_code}" --max-time 3 http://127.0.0.1:8080/)"
  check "GET /nope.html -> 404" "404" "$(curl -s -o /dev/null -w "%{http_code}" --max-time 3 http://127.0.0.1:8080/nope.html)"

  echo ""
  echo "-- POST (upload) --"
  echo "roundtrip-check-$$" > "$WORK/roundtrip.txt"
  POST_STATUS=$(curl -s -o /dev/null -w "%{http_code}" --max-time 3 \
    -F "file=@$WORK/roundtrip.txt;filename=audit_roundtrip.txt" http://127.0.0.1:8080/)
  check "POST multipart upload -> 201" "201" "$POST_STATUS"

  echo ""
  echo "-- DELETE --"
  check "DELETE /uploads/audit_roundtrip.txt -> 204" "204" "$(curl -s -o /dev/null -w "%{http_code}" --max-time 3 -X DELETE http://127.0.0.1:8080/uploads/audit_roundtrip.txt)"
  check "DELETE missing file -> 404" "404" "$(curl -s -o /dev/null -w "%{http_code}" --max-time 3 -X DELETE http://127.0.0.1:8080/uploads/audit_roundtrip.txt)"

  echo ""
  echo "-- wrong request, server still working after --"
  BAD_STATUS=$(python3 - <<'PY'
import socket
s = socket.create_connection(("127.0.0.1", 8080), 2)
s.sendall(b"NOT A REAL REQUEST\r\n\r\n")
data = s.recv(256)
s.close()
print(data.split()[1].decode() if data.startswith(b"HTTP/") else "000")
PY
)
  check "malformed request line -> 400" "400" "$BAD_STATUS"
  AFTER_BAD=$(curl -s -o /dev/null -w "%{http_code}" --max-time 3 http://127.0.0.1:8080/)
  check "server still answers a normal GET afterwards" "200" "$AFTER_BAD"

  echo ""
  echo "-- upload, get back, verify not corrupted --"
  head -c 65536 /dev/urandom > "$WORK/integrity.bin"
  curl -s -o /dev/null --max-time 5 -F "file=@$WORK/integrity.bin;filename=audit_integrity.bin" http://127.0.0.1:8080/ >/dev/null
  curl -s -o "$WORK/integrity_back.bin" --max-time 5 http://127.0.0.1:8080/uploads/audit_integrity.bin
  if cmp -s "$WORK/integrity.bin" "$WORK/integrity_back.bin"; then
    pass "uploaded file round-trips byte-for-byte (65536 random bytes)"
  else
    fail "uploaded file round-trips byte-for-byte (bytes differ after GET)"
  fi
  curl -s -o /dev/null --max-time 3 -X DELETE http://127.0.0.1:8080/uploads/audit_integrity.bin

  echo ""
  echo "-- sessions and cookies --"
  HDRS1=$(curl -s -D - -o /dev/null --max-time 3 http://127.0.0.1:8080/)
  check_contains "first response sets Set-Cookie session_id" "$HDRS1" "set-cookie: session_id="
  SID=$(echo "$HDRS1" | tr -d '\r' | grep -i '^set-cookie:' | head -1 | sed -n 's/.*session_id=\([^;]*\).*/\1/p')
  HITS1=$(echo "$HDRS1" | tr -d '\r' | grep -i '^x-session-hits:' | awk '{print $2}')
  check "first hit count is 1" "1" "$HITS1"
  HDRS2=$(curl -s -D - -o /dev/null --max-time 3 -H "Cookie: session_id=$SID" http://127.0.0.1:8080/)
  SID2=$(echo "$HDRS2" | tr -d '\r' | grep -i '^set-cookie:' | head -1 | sed -n 's/.*session_id=\([^;]*\).*/\1/p')
  HITS2=$(echo "$HDRS2" | tr -d '\r' | grep -i '^x-session-hits:' | awk '{print $2}')
  check "session id stable across requests" "$SID" "$SID2"
  check "hit count increments to 2" "2" "$HITS2"
fi

# ─────────────────────────────────────────────────────────
section "Interaction with the browser — audit.md § Interaction with the browser"
manual "Browser connects with no issues -> open http://127.0.0.1:8080/ in the audit browser"
manual "Request/response headers correct (devtools Network tab) -> inspect GET / on http://127.0.0.1:8080/"
manual "Wrong URL handled properly -> visit http://127.0.0.1:8080/does-not-exist in the browser, confirm the styled 404 page renders"
manual "Directory listing handled properly -> autoindex is 'on' for site 'localhost' path /; remove/rename www/index.html temporarily or browse a sub-dir with no index to see the listing"
manual "Redirected URL handled properly -> visit http://127.0.0.1:8080/old in the browser, confirm it lands on /"

if [[ -n "${MAIN_PID:-}" ]] && [[ -x /usr/bin/python3 ]]; then
  echo ""
  echo "-- CGI with unchunked (Content-Length) and chunked data (scripted, no browser needed) --"
  UNCHUNKED=$(curl -s -X POST -H "Content-Type: text/plain" --data "unchunked-payload" --max-time 3 http://127.0.0.1:8080/cgi-bin/hello.py)
  check_contains "CGI unchunked body" "$UNCHUNKED" "BODY=unchunked-payload"
  CHUNKED=$(python3 - <<'PY'
import socket
req = (
    b"POST /cgi-bin/hello.py HTTP/1.1\r\n"
    b"Host: localhost\r\n"
    b"Transfer-Encoding: chunked\r\n"
    b"Content-Type: text/plain\r\n"
    b"\r\n"
    b"7\r\nchunked\r\n0\r\n\r\n"
)
s = socket.create_connection(("127.0.0.1", 8080), 3)
s.sendall(req)
s.settimeout(3)
data = b""
try:
    while True:
        chunk = s.recv(4096)
        if not chunk:
            break
        data += chunk
except socket.timeout:
    pass
s.close()
print(data.decode(errors="replace"))
PY
)
  check_contains "CGI chunked body" "$CHUNKED" "BODY=chunked"
else
  manual "CGI chunked/unchunked check skipped (python3 not found) -> POST both ways to http://127.0.0.1:8080/cgi-bin/hello.py by hand"
fi

stop_server "${MAIN_PID:-}"

# ─────────────────────────────────────────────────────────
section "Port issues — audit.md § Port issues"

echo ""
echo "-- duplicate port -> server should find the error, not start --"
cat > "$WORK/dup_port.conf" <<'EOF'
site {
    bind 127.0.0.1:1;
    bind 0.0.0.0:1;
    name a;
    path / { methods GET; root www; }
}
EOF
DUP_LOG="$WORK/dup_port.log"
DUP_PID=$(start_server "$WORK/dup_port.conf" "$DUP_LOG")
if [[ -z "$DUP_PID" ]]; then
  pass "conflicting listen addresses on the same port rejected at startup"
  check_contains "error message names the conflict" "$(cat "$DUP_LOG")" "duplicate port"
else
  fail "conflicting listen addresses on the same port rejected at startup (server started anyway)"
  stop_server "$DUP_PID"
fi

echo ""
echo "-- multiple servers, common port, one configuration invalid --"
echo "   audit.md: \"If one of these configurations isn't valid... your server"
echo "   should continue to function for the other configurations.\""
echo ""
echo "   Two variants of 'invalid', tested separately (see docs/audit-talking-points.md"
echo "   § Port issues for why they differ):"
echo ""
echo "   (a) a directive that's syntactically fine but points at nothing on disk"
echo "       (root pointing to a directory that doesn't exist) — this is NOT"
echo "       caught by config validation (paths aren't stat()'d at load time),"
echo "       so it isn't actually a config error from the parser's point of view."
cat > "$WORK/shared_port_bad_path.conf" <<EOF
site {
    bind 127.0.0.1:19080;
    name good.local;
    path / {
        methods GET;
        root $ROOT/www;
        index index.html;
    }
}

site {
    bind 127.0.0.1:19080;
    name broken.local;
    path / {
        methods GET;
        root $ROOT/www/this/path/does/not/exist;
        index index.html;
    }
}
EOF
BADPATH_LOG="$WORK/shared_port_bad_path.log"
BADPATH_PID=$(start_server "$WORK/shared_port_bad_path.conf" "$BADPATH_LOG")
if [[ -z "$BADPATH_PID" ]]; then
  fail "(a) server starts with a nonexistent-root sibling (unexpectedly refused to start)"
  cat "$BADPATH_LOG"
else
  GOOD_STATUS=$(curl -s -o /dev/null -w "%{http_code}" --max-time 3 --resolve good.local:19080:127.0.0.1 http://good.local:19080/)
  check "(a) valid sibling ('good.local') still serves 200" "200" "$GOOD_STATUS"
  stop_server "$BADPATH_PID"
fi

echo ""
echo "   (b) a directive that's genuinely invalid per the config schema itself"
echo "       (a 'path' block with neither 'root' nor 'redirect' — verify::validate()"
echo "       explicitly rejects this). This is the case audit.md is actually about."
cat > "$WORK/shared_port_bad_schema.conf" <<EOF
site {
    bind 127.0.0.1:19081;
    name good.local;
    path / {
        methods GET;
        root $ROOT/www;
        index index.html;
    }
}

site {
    bind 127.0.0.1:19081;
    name broken.local;
    path / {
        methods GET;
    }
}
EOF
BADSCHEMA_LOG="$WORK/shared_port_bad_schema.log"
BADSCHEMA_PID=$(start_server "$WORK/shared_port_bad_schema.conf" "$BADSCHEMA_LOG")
if [[ -z "$BADSCHEMA_PID" ]]; then
  fail "(b) valid sibling ('good.local') stays up when its co-tenant has an invalid path block (whole process refused to start: $(cat "$BADSCHEMA_LOG"))"
else
  GOOD2_STATUS=$(curl -s -o /dev/null -w "%{http_code}" --max-time 3 --resolve good.local:19081:127.0.0.1 http://good.local:19081/)
  check "(b) valid sibling still serves 200 despite invalid co-tenant" "200" "$GOOD2_STATUS"
  stop_server "$BADSCHEMA_PID"
fi

# ─────────────────────────────────────────────────────────
section "Siege & stress test — audit.md § Siege & stress test"
if command -v siege >/dev/null 2>&1; then
  SIEGE_LOG="$WORK/siege.log"
  SIEGE_PID=$(start_server "$ROOT/example.conf" "$WORK/siege_server.log")
  if [[ -z "$SIEGE_PID" ]]; then
    fail "server starts for siege run"
  else
    echo ""
    echo "-- siege -b, ~8s, 25 concurrent (shorten/lengthen with -t/-c for a full audit run) --"
    siege -b -c 25 -t 8S http://127.0.0.1:8080/ > "$SIEGE_LOG" 2>&1
    cat "$SIEGE_LOG" | grep -E "availability|transactions|failed"
    AVAIL=$(grep -oE '"availability":[[:space:]]*[0-9.]+' "$SIEGE_LOG" | grep -oE '[0-9.]+$')
    if [[ -n "$AVAIL" ]] && (( $(echo "$AVAIL >= 99.5" | bc -l) )); then
      pass "siege availability >= 99.5% (got ${AVAIL}%)"
    else
      fail "siege availability >= 99.5% (got '${AVAIL:-unknown}%')"
    fi

    echo ""
    echo "-- no hanging connections after the burst --"
    sleep 0.5
    ESTABLISHED=$(ss -tan 2>/dev/null | grep ':8080' | grep -c ESTAB || true)
    check "no lingering ESTABLISHED sockets on :8080 (TIME-WAIT is fine)" "0" "$ESTABLISHED"

    stop_server "$SIEGE_PID"
  fi
else
  manual "siege not installed -> sudo apt install siege, then: siege -b -c 25 -t 30S http://127.0.0.1:8080/ (expect >=99.5% availability)"
fi

if command -v valgrind >/dev/null 2>&1; then
  echo ""
  echo "-- quick leak check (valgrind --leak-check=full, light traffic) --"
  VG_LOG="$WORK/valgrind.log"
  valgrind --leak-check=full --log-file="$VG_LOG" "$BIN" "$ROOT/example.conf" >"$WORK/valgrind_stdout.log" 2>&1 &
  VG_LAUNCHER_PID=$!
  SERVER_PIDS+=("$VG_LAUNCHER_PID")
  sleep 2
  if kill -0 "$VG_LAUNCHER_PID" 2>/dev/null; then
    for i in 1 2 3 4 5 6 7 8; do
      curl -s -o /dev/null --max-time 3 "http://127.0.0.1:8080/" || true
    done
    curl -s -o /dev/null --max-time 3 "http://127.0.0.1:8080/cgi-bin/hello.py" || true
    curl -s -o /dev/null --max-time 3 -X DELETE "http://127.0.0.1:8080/nope" || true
    sleep 0.3
    # SIGTERM, not SIGINT: bash sets SIGINT/SIGQUIT to be ignored by
    # asynchronous ("&") commands in a non-interactive shell (this script),
    # so a backgrounded valgrind here never even sees a `kill -INT` —
    # confirmed by direct reproduction. SIGTERM isn't subject to that and
    # valgrind reports the same leak summary on it before exiting.
    kill -TERM "$VG_LAUNCHER_PID" 2>/dev/null
    for i in $(seq 1 30); do
      kill -0 "$VG_LAUNCHER_PID" 2>/dev/null || break
      sleep 0.5
    done
    DEFINITELY_LOST=$(grep -oE 'definitely lost: [0-9,]+ bytes' "$VG_LOG" | head -1)
    if echo "$DEFINITELY_LOST" | grep -q "definitely lost: 0 bytes"; then
      pass "valgrind: $DEFINITELY_LOST"
    else
      fail "valgrind: ${DEFINITELY_LOST:-no LEAK SUMMARY found in $VG_LOG (process may not have shut down cleanly)}"
    fi
  else
    fail "server started under valgrind"
  fi
else
  manual "valgrind not installed -> sudo apt install valgrind, then: valgrind --leak-check=full ./target/debug/localhost example.conf (drive some load, Ctrl-C, check 'definitely lost: 0 bytes')"
fi

# ─────────────────────────────────────────────────────────
section "Unit Tests — audit.md § Unit Tests"
echo ""
TEST_OUTPUT=$(cargo test 2>&1)
TEST_EXIT=$?
TEST_SUMMARY=$(echo "$TEST_OUTPUT" | grep -E "^test result:" | tail -1)
echo "$TEST_SUMMARY"
if [[ $TEST_EXIT -eq 0 ]]; then
  pass "cargo test: all tests pass ($TEST_SUMMARY)"
else
  fail "cargo test: all tests pass ($TEST_SUMMARY)"
fi
check_contains "tests for HTTP request parsing exist (http::decode::tests)" "$TEST_OUTPUT" "http::decode::tests::parses_chunked_body"
check_contains "tests for config validation exist (settings::tests)" "$TEST_OUTPUT" "settings::tests::rejects_conflicting_addresses_on_same_port"
check_contains "tests for route matching exist (dispatch::tests)" "$TEST_OUTPUT" "dispatch::tests::longest_prefix_wins"
check_contains "tests for CGI routing exist (dispatch::tests)" "$TEST_OUTPUT" "dispatch::tests::cgi_extension_routes_to_handler"

# ─────────────────────────────────────────────────────────
section "General / bonus — audit.md § General"
echo ""
if grep -q 'cgi \.py' "$ROOT/example.conf" && grep -q 'cgi \.sh' "$ROOT/example.conf"; then
  pass "example.conf configures more than one CGI interpreter (.py and .sh)"
else
  fail "example.conf configures more than one CGI interpreter"
fi

MULTI_CGI_PID=$(start_server "$ROOT/example.conf" "$WORK/multicgi.log")
if [[ -n "$MULTI_CGI_PID" ]]; then
  PY_STATUS=$(curl -s -o /dev/null -w "%{http_code}" --max-time 3 http://127.0.0.1:8080/cgi-bin/hello.py)
  SH_STATUS=$(curl -s -o /dev/null -w "%{http_code}" --max-time 3 http://127.0.0.1:8080/cgi-bin/hello.sh)
  check "python3 CGI interpreter responds -> 200" "200" "$PY_STATUS"
  check "bash CGI interpreter responds -> 200" "200" "$SH_STATUS"
  stop_server "$MULTI_CGI_PID"
fi

if [[ -d "$ROOT/cpp" ]] && compgen -G "$ROOT/cpp/src/*.cpp" > /dev/null; then
  pass "second-language implementation present (cpp/src/*.cpp)"
  manual "repeat the practical tests above against the C++ build (cpp/Makefile) — separate build/runtime, out of scope for this Rust-focused dry run"
else
  fail "second-language implementation present (cpp/src/*.cpp)"
fi

# ─────────────────────────────────────────────────────────
section "Summary"
TOTAL=$((PASS + FAIL))
echo "  PASS:   $PASS"
echo "  FAIL:   $FAIL"
echo "  MANUAL: $MANUAL  (not counted toward pass/fail — see the lines above)"
echo "  ($TOTAL scripted checks total)"
if [[ "$FAIL" -eq 0 ]]; then
  echo "  All scripted checks passed."
else
  echo "  $FAIL scripted check(s) failed — see output above."
fi
echo ""
echo "Talking-point prep for the MANUAL items: docs/audit-talking-points.md"
echo ""

exit "$FAIL"
