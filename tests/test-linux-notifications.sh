#!/bin/sh
# Run the opt-in D-Bus contract test without contacting the user's desktop.
set -eu

if [ "${OCTOWATCHER_TEST_SESSION:-}" != 1 ]; then
    exec dbus-run-session -- env OCTOWATCHER_TEST_SESSION=1 "$0"
fi

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo_root"
notification_test_python=${OCTOWATCHER_TEST_PYTHON:-/usr/bin/python3}
notification_test_dir=$(mktemp -d)
"$notification_test_python" tests/notification-service.py > "$notification_test_dir/service.log" 2>&1 &
notification_service_pid=$!
cleanup() {
    notification_test_status=$?
    kill "$notification_service_pid" 2>/dev/null || true
    if [ "$notification_test_status" -ne 0 ]; then
        cat "$notification_test_dir/service.log"
    fi
    rm -rf "$notification_test_dir"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

notification_ready=0
for attempt in $(seq 1 100); do
    if "$notification_test_python" -c 'import pathlib, sys; sys.exit("ready\n" not in pathlib.Path(sys.argv[1]).read_text())' "$notification_test_dir/service.log"; then
        notification_ready=1
        break
    fi
    if ! kill -0 "$notification_service_pid" 2>/dev/null; then
        break
    fi
    sleep 0.1
done
if [ "$notification_ready" -ne 1 ]; then
    printf '%s\n' 'Notification test service did not start.' >&2
    exit 1
fi

timeout 60s cargo test --locked notifications::tests::linux_notification_service_contract -- --ignored --exact
