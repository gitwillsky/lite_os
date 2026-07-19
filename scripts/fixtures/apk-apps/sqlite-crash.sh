#!/bin/sh
set -eu

database=/root/sqlite-gate.db
ready=/run/sqlite-crash-writer-active
report_failure() {
    status=$?
    if [ "$status" -ne 0 ]; then
        echo "LITEOS_SQLITE_CRASH_FAILED status=$status"
    fi
}
trap report_failure EXIT
rm -f "$ready"

# 保持一个已写入但未提交的 WAL transaction，再精确杀死 sqlite process。
(
    printf '%s\n' \
        'PRAGMA journal_mode=WAL;' \
        'BEGIN IMMEDIATE;' \
        'INSERT INTO records(value) VALUES("uncommitted-crash");' \
        '.shell echo ready > /run/sqlite-crash-writer-active'
    sleep 30
    printf '%s\n' 'COMMIT;'
) | sqlite3 "$database" &
writer=$!
for _ in 1 2 3 4 5; do
    [ -f "$ready" ] && break
    sleep 1
done
[ -f "$ready" ]
kill -9 "$writer"
set +e
wait "$writer"
status=$?
set -e
[ "$status" -ne 0 ]

integrity=$(sqlite3 "$database" 'PRAGMA integrity_check;')
count=$(sqlite3 "$database" 'SELECT count(*) FROM records;')
echo "LITEOS_SQLITE_CRASH_STATE integrity=$integrity count=$count"
[ "$integrity" = ok ]
[ "$count" -eq 5 ]
sync
echo LITEOS_SQLITE_CRASH_RECOVERY_READY
trap - EXIT
while :; do sleep 1; done
