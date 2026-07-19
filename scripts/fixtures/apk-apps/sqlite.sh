#!/bin/sh
set -eu

database=/root/sqlite-gate.db
lock_ready=/run/sqlite-writer-a-locked
report_failure() {
    status=$?
    if [ "$status" -ne 0 ]; then
        echo "LITEOS_SQLITE_APPLICATION_FAILED status=$status"
    fi
}
trap report_failure EXIT
rm -f "$database" "$database-shm" "$database-wal" "$database-journal"
rm -f "$lock_ready"

# 1. rollback journal transaction 必须原子创建 schema 与首批 rows。
sqlite3 "$database" <<'SQL'
PRAGMA journal_mode=DELETE;
BEGIN IMMEDIATE;
CREATE TABLE records(id INTEGER PRIMARY KEY, value TEXT NOT NULL);
INSERT INTO records(value) VALUES('rollback-a'),('rollback-b');
COMMIT;
SQL

# 2. WAL 与 POSIX blocking record lock 必须让两个 writer 串行提交而不丢数据。
sqlite3 "$database" 'PRAGMA journal_mode=WAL; INSERT INTO records(value) VALUES("wal-a");'
(
    printf '%s\n' 'PRAGMA busy_timeout=5000;' 'BEGIN IMMEDIATE;' \
        'INSERT INTO records(value) VALUES("writer-a");' \
        '.shell echo ready > /run/sqlite-writer-a-locked'
    sleep 2
    printf '%s\n' 'COMMIT;'
) | sqlite3 "$database" &
first=$!
for _ in 1 2 3 4 5; do
    [ -f "$lock_ready" ] && break
    sleep 1
done
[ -f "$lock_ready" ]
sqlite3 "$database" 'PRAGMA busy_timeout=5000; INSERT INTO records(value) VALUES("writer-b");'
wait "$first"

# 3. integrity、持久化 row set 与同步边界在 guest 内完成断言。
[ "$(sqlite3 "$database" 'PRAGMA integrity_check;')" = ok ]
[ "$(sqlite3 "$database" 'SELECT count(*) FROM records;')" -eq 5 ]
cp /run/sqlite-recovery.inittab /etc/inittab
sync
echo LITEOS_SQLITE_APPLICATION_READY
trap - EXIT
while :; do sleep 1; done
