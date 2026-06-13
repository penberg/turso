#!/usr/bin/env bash
#
# Run the MySQL conformance suite against two targets:
#
#   1. A throwaway MySQL in Docker (the reference — should always be green).
#   2. The Turso MySQL front-end (turso-mysql-server) — work in progress.
#
# Both are driven through the same wire-protocol runner, so the comparison shows
# exactly where the front-end diverges from real MySQL.
#
# Usage:
#   ./run.sh [options] [TEST_PATH ...]
#
# Options:
#   --strict        Also fail the script if the Turso run has failures.
#                   (By default only the Docker MySQL run gates the exit code,
#                   since the front-end has known gaps.)
#   --mysql-only    Run against Docker MySQL only.
#   --turso-only    Run against the Turso front-end only.
#   --keep          Leave the Docker container and server running on exit.
#   -h, --help      Show this help.
#
# Environment overrides:
#   MYSQL_IMAGE   Docker image            (default: mysql:8.4)
#   MYSQL_PORT    host port for MySQL     (default: 3307)
#   TURSO_PORT    host port for Turso     (default: 3308)
#   CONTAINER     container name          (default: turso-mysql-conformance)
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

MYSQL_IMAGE="${MYSQL_IMAGE:-mysql:8.4}"
MYSQL_PORT="${MYSQL_PORT:-3307}"
TURSO_PORT="${TURSO_PORT:-3308}"
CONTAINER="${CONTAINER:-turso-mysql-conformance}"

strict=0
run_mysql=1
run_turso=1
keep=0
declare -a tests=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        --strict) strict=1 ;;
        --mysql-only) run_turso=0 ;;
        --turso-only) run_mysql=0 ;;
        --keep) keep=1 ;;
        -h|--help) sed -n '2,30p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
        --) shift; while [[ $# -gt 0 ]]; do tests+=("$1"); shift; done; break ;;
        -*) echo "unknown option: $1" >&2; exit 2 ;;
        *) tests+=("$1") ;;
    esac
    shift
done

# Default to the bundled tests directory if no paths were given.
if [[ ${#tests[@]} -eq 0 ]]; then
    tests=("$SCRIPT_DIR/tests")
fi

# Locate a container runtime.
DOCKER="$(command -v docker || command -v podman || true)"
if [[ $run_mysql -eq 1 && -z "$DOCKER" ]]; then
    echo "error: neither docker nor podman found (needed for the MySQL target)" >&2
    echo "       use --turso-only to skip the Docker MySQL run" >&2
    exit 2
fi

cd "$SCRIPT_DIR"

# ---------------------------------------------------------------------------
# Build the binaries up front so timing/output is clean during the runs.
# ---------------------------------------------------------------------------
echo "==> building turso-mysql-server and turso-mysql-conformance"
cargo build -q -p turso-mysql-server -p turso-mysql-conformance

TARGET_DIR="$(cargo metadata --format-version 1 --no-deps \
    | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
SERVER_BIN="$TARGET_DIR/debug/turso-mysql-server"
RUNNER_BIN="$TARGET_DIR/debug/turso-mysql-conformance"

TURSO_PID=""
SERVER_LOG="$(mktemp -t turso-mysql-server.XXXXXX.log)"

cleanup() {
    if [[ $keep -eq 1 ]]; then
        echo "==> --keep set; leaving container '$CONTAINER' and server (pid ${TURSO_PID:-none}) running"
        return
    fi
    if [[ -n "$TURSO_PID" ]]; then
        kill "$TURSO_PID" 2>/dev/null || true
        wait "$TURSO_PID" 2>/dev/null || true
    fi
    if [[ $run_mysql -eq 1 && -n "$DOCKER" ]]; then
        "$DOCKER" rm -f "$CONTAINER" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT

# Runs the suite and returns the runner's exit code (0 = all passed).
run_suite() {
    local label="$1" url="$2"
    echo
    echo "======================================================================"
    echo "  $label   ($url)"
    echo "======================================================================"
    if "$RUNNER_BIN" --url "$url" "${tests[@]}"; then
        return 0
    else
        return $?
    fi
}

mysql_rc=0
turso_rc=0

# ---------------------------------------------------------------------------
# Target 1: Docker MySQL (reference).
# ---------------------------------------------------------------------------
if [[ $run_mysql -eq 1 ]]; then
    echo "==> starting $MYSQL_IMAGE on 127.0.0.1:$MYSQL_PORT (container: $CONTAINER)"
    "$DOCKER" rm -f "$CONTAINER" >/dev/null 2>&1 || true
    "$DOCKER" run --rm -d \
        --name "$CONTAINER" \
        -e MYSQL_ALLOW_EMPTY_PASSWORD=yes \
        -e MYSQL_DATABASE=conformance \
        -p "127.0.0.1:$MYSQL_PORT:3306" \
        "$MYSQL_IMAGE" >/dev/null

    echo -n "==> waiting for MySQL to accept connections"
    ready=0
    for _ in $(seq 1 90); do
        if "$DOCKER" exec "$CONTAINER" mysqladmin ping -h 127.0.0.1 --silent >/dev/null 2>&1; then
            ready=1; break
        fi
        echo -n "."; sleep 1
    done
    echo
    if [[ $ready -ne 1 ]]; then
        echo "error: MySQL did not become ready in time" >&2
        "$DOCKER" logs "$CONTAINER" 2>&1 | tail -20 >&2 || true
        exit 1
    fi

    run_suite "Docker MySQL" "mysql://root@127.0.0.1:$MYSQL_PORT/conformance" && mysql_rc=0 || mysql_rc=$?
fi

# ---------------------------------------------------------------------------
# Target 2: the Turso MySQL front-end.
# ---------------------------------------------------------------------------
if [[ $run_turso -eq 1 ]]; then
    echo
    echo "==> starting turso-mysql-server on 127.0.0.1:$TURSO_PORT (log: $SERVER_LOG)"
    RUST_LOG="${RUST_LOG:-warn}" "$SERVER_BIN" \
        --listen "127.0.0.1:$TURSO_PORT" --database :memory: >"$SERVER_LOG" 2>&1 &
    TURSO_PID=$!

    echo -n "==> waiting for the front-end to listen"
    ready=0
    for _ in $(seq 1 50); do
        if (exec 3<>"/dev/tcp/127.0.0.1/$TURSO_PORT") 2>/dev/null; then
            exec 3>&- 3<&-; ready=1; break
        fi
        echo -n "."; sleep 0.2
    done
    echo
    if [[ $ready -ne 1 ]]; then
        echo "error: turso-mysql-server did not start; log follows:" >&2
        cat "$SERVER_LOG" >&2 || true
        exit 1
    fi

    run_suite "Turso front-end" "mysql://root@127.0.0.1:$TURSO_PORT/" && turso_rc=0 || turso_rc=$?
fi

# ---------------------------------------------------------------------------
# Summary.
# ---------------------------------------------------------------------------
status() { [[ "$1" -eq 0 ]] && echo "PASS" || echo "FAIL"; }

echo
echo "======================================================================"
echo "  summary"
echo "======================================================================"
[[ $run_mysql -eq 1 ]] && printf "  %-18s %s\n" "Docker MySQL"    "$(status "$mysql_rc")"
[[ $run_turso -eq 1 ]] && printf "  %-18s %s\n" "Turso front-end" "$(status "$turso_rc")"
echo

# Exit code: the Docker MySQL run always gates (it validates the test files
# themselves). The Turso run only gates under --strict, because the front-end
# is a work in progress with known unimplemented statements.
rc=0
[[ $run_mysql -eq 1 && $mysql_rc -ne 0 ]] && rc=1
[[ $strict -eq 1 && $run_turso -eq 1 && $turso_rc -ne 0 ]] && rc=1

if [[ $run_turso -eq 1 && $turso_rc -ne 0 && $strict -eq 0 ]]; then
    echo "note: the Turso front-end has failures (expected — unimplemented statements)."
    echo "      run with --strict to make them fail the script."
fi

exit $rc
