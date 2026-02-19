#!/usr/bin/env bash
# Playwright E2E tests for Threshold Web Interface
# Requires: playwright-cli, cargo (for building e2e_server)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
PASS=0
FAIL=0
SKIP=0
TOTAL=0
SESSION="e2e-$$"
E2E_PID=""
TMPOUT=""

cleanup() {
    if [ -n "$E2E_PID" ] && kill -0 "$E2E_PID" 2>/dev/null; then
        kill "$E2E_PID" 2>/dev/null
        wait "$E2E_PID" 2>/dev/null || true
    fi
    playwright-cli -s="$SESSION" close 2>/dev/null || true
    [ -n "$TMPOUT" ] && rm -f "$TMPOUT"
}
trap cleanup EXIT

# Build the E2E server
echo "Building E2E server..."
cd "$PROJECT_ROOT"
cargo build --test e2e_server -p threshold-web --quiet 2>/dev/null

# Find binary
E2E_BIN=$(find "$PROJECT_ROOT/target/debug/deps" -maxdepth 1 -name "e2e_server*" -type f -perm +111 ! -name "*.d" ! -name "*.o" 2>/dev/null | head -1)
if [ -z "$E2E_BIN" ]; then
    echo "FATAL: Could not find e2e_server binary"
    exit 1
fi

# Start the server, capturing stdout to temp file for URL extraction
echo "Starting E2E server..."
TMPOUT=$(mktemp)
$E2E_BIN > "$TMPOUT" 2>&1 &
E2E_PID=$!
sleep 1.5

URL=$(grep "E2E_SERVER_URL=" "$TMPOUT" | head -1 | cut -d= -f2-)
if [ -z "$URL" ]; then
    echo "FATAL: Could not determine server URL"
    cat "$TMPOUT"
    exit 1
fi

echo "Server running at: $URL"

# Helper: take snapshot including YAML content, search for text (case-insensitive)
snapshot_contains() {
    local expected="$1"
    local snap
    snap=$(playwright-cli -s="$SESSION" snapshot 2>&1)
    # Read referenced YAML file for full snapshot content
    local yml
    yml=$(echo "$snap" | grep -oE '\.playwright-cli/[^ )]*\.yml' | tail -1 || true)
    if [ -n "$yml" ] && [ -f "$yml" ]; then
        snap="$snap
$(cat "$yml")"
    fi
    echo "$snap" | grep -qi "$expected"
}

# Helper: test that snapshot contains expected text
pw_test() {
    local desc="$1"
    local expected="$2"
    TOTAL=$((TOTAL + 1))
    if snapshot_contains "$expected"; then
        PASS=$((PASS + 1))
        echo "  PASS: $desc"
    else
        FAIL=$((FAIL + 1))
        echo "  FAIL: $desc (expected '$expected' in snapshot)"
    fi
}

# Helper: HTTP status test
http_test() {
    local desc="$1"
    local test_url="$2"
    local expected_code="$3"
    TOTAL=$((TOTAL + 1))
    local code
    code=$(curl -s -o /dev/null -w "%{http_code}" --max-time 5 "$test_url")
    if [ "$code" = "$expected_code" ]; then
        PASS=$((PASS + 1))
        echo "  PASS: $desc"
    else
        FAIL=$((FAIL + 1))
        echo "  FAIL: $desc (got $code, expected $expected_code)"
    fi
}

# Helper: skip a test
pw_skip() {
    local desc="$1"
    local reason="$2"
    TOTAL=$((TOTAL + 1))
    SKIP=$((SKIP + 1))
    echo "  SKIP: $desc ($reason)"
}

echo ""
echo "=== Threshold Web E2E Tests ==="
echo ""

# ── 1. Dashboard ──
echo "[1. Dashboard]"
playwright-cli -s="$SESSION" open "$URL" > /dev/null 2>&1
sleep 1

pw_test "Dashboard heading present" "Dashboard"
pw_test "Uptime card present" "Uptime"
pw_test "Conversations card present" "Conversations"
pw_test "Scheduled Tasks card present" "Scheduled Tasks"
pw_test "Scheduler status shown" "Scheduler"
pw_test "Discord status shown" "Discord"
pw_test "Web Interface status shown" "Web Interface"
pw_test "Quick Links section present" "Quick Links"
echo ""

# ── 2. Conversations ──
echo "[2. Conversations]"
playwright-cli -s="$SESSION" goto "$URL/conversations" > /dev/null 2>&1
sleep 0.5

pw_test "Conversations page loads" "Conversations"
pw_test "Shows empty state" "No conversations"
echo ""

# ── 3. Schedules (no scheduler in test) ──
echo "[3. Schedules]"
playwright-cli -s="$SESSION" goto "$URL/schedules" > /dev/null 2>&1
sleep 0.5

# Test server has no scheduler_handle, so we expect the 503 error page
pw_test "Schedules shows scheduler status" "Scheduler"
pw_test "Error page has back link" "Dashboard"
echo ""

# ── 4. Audit ──
echo "[4. Audit]"
playwright-cli -s="$SESSION" goto "$URL/audit" > /dev/null 2>&1
sleep 0.5

pw_test "Audit page loads" "Audit"
pw_test "Shows empty state" "No audit"
echo ""

# ── 5. Logs ──
echo "[5. Logs]"
playwright-cli -s="$SESSION" goto "$URL/logs" > /dev/null 2>&1
sleep 0.5

pw_test "Logs page loads" "Log"
pw_test "Shows empty state" "No log"
echo ""

# ── 6. Config ──
echo "[6. Config]"
playwright-cli -s="$SESSION" goto "$URL/config" > /dev/null 2>&1
sleep 0.5

pw_test "Config page loads" "Configuration"
pw_test "Save button present" "Save"
pw_test "Restart warning present" "restart"
echo ""

# ── 7. Credentials (keychain may block in test env) ──
echo "[7. Credentials]"
# Test with curl + timeout since keychain may be interactive
CRED_CODE=$(curl -s -o /dev/null -w "%{http_code}" --max-time 3 "$URL/config/credentials" 2>/dev/null || echo "000")
if [ "$CRED_CODE" = "200" ]; then
    playwright-cli -s="$SESSION" goto "$URL/config/credentials" > /dev/null 2>&1
    sleep 0.5
    pw_test "Credentials page loads" "Credentials"
    pw_test "Breadcrumb shows Config link" "Config"
    pw_test "Secret store note present" "secret store"
    pw_test "Credential status shown" "Not set"
else
    pw_skip "Credentials page" "secret store not accessible in test environment (HTTP $CRED_CODE)"
    pw_skip "Breadcrumb" "skipped due to secret store"
    pw_skip "Secret store note" "skipped due to secret store"
    pw_skip "Credential status" "skipped due to secret store"
fi
echo ""

# ── 8. Responsive (375px mobile) ──
echo "[8. Responsive Mobile]"
playwright-cli -s="$SESSION" resize 375 812 > /dev/null 2>&1
playwright-cli -s="$SESSION" goto "$URL" > /dev/null 2>&1
sleep 0.5

pw_test "Dashboard loads at mobile width" "Dashboard"
pw_test "Nav still accessible at mobile" "Conversations"

playwright-cli -s="$SESSION" resize 1280 720 > /dev/null 2>&1
echo ""

# ── 9. Static Assets ──
echo "[9. Static Assets]"
http_test "htmx.min.js serves correctly" "$URL/static/htmx.min.js" "200"
http_test "pico.min.css serves correctly" "$URL/static/pico.min.css" "200"
http_test "style.css serves correctly" "$URL/static/style.css" "200"
echo ""

# ── 10. Error Handling ──
echo "[10. Error Handling]"
http_test "404 for nonexistent page" "$URL/nonexistent-page" "404"
http_test "Dashboard returns 200" "$URL/" "200"
http_test "Status JSON returns 200" "$URL/status" "200"
http_test "Schedules without scheduler returns 503" "$URL/schedules" "503"
echo ""

# ── 11. Status JSON ──
echo "[11. Status JSON API]"
TOTAL=$((TOTAL + 1))
STATUS_JSON=$(curl -s --max-time 5 "$URL/status")
if echo "$STATUS_JSON" | python3 -c "import json,sys; d=json.load(sys.stdin); assert 'uptime' in d and 'conversation_count' in d and 'scheduler_running' in d" 2>/dev/null; then
    PASS=$((PASS + 1))
    echo "  PASS: Status JSON has expected fields"
else
    FAIL=$((FAIL + 1))
    echo "  FAIL: Status JSON missing fields: $STATUS_JSON"
fi
echo ""

# ── Summary ──
echo "=== Results ==="
echo "Total: $TOTAL | Passed: $PASS | Failed: $FAIL | Skipped: $SKIP"
echo ""

# Close browser
playwright-cli -s="$SESSION" close > /dev/null 2>&1 || true
SESSION=""

if [ "$FAIL" -gt 0 ]; then
    echo "SOME TESTS FAILED"
    exit 1
else
    echo "ALL TESTS PASSED"
    exit 0
fi
