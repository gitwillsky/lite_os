#!/bin/sh
set -eu

database=/root/sqlite-gate.db
rm -f "$database" "$database-shm" "$database-wal" "$database-journal"

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
        'INSERT INTO records(value) VALUES("writer-a");'
    sleep 2
    printf '%s\n' 'COMMIT;'
) | sqlite3 "$database" &
first=$!
sleep 1
sqlite3 "$database" 'PRAGMA busy_timeout=5000; INSERT INTO records(value) VALUES("writer-b");'
wait "$first"

# 3. integrity、持久化 row set 与同步边界在 guest 内完成断言。
[ "$(sqlite3 "$database" 'PRAGMA integrity_check;')" = ok ]
[ "$(sqlite3 "$database" 'SELECT count(*) FROM records;')" -eq 5 ]
cp /run/sqlite-recovery.inittab /etc/inittab
sync
echo LITEOS_SQLITE_APPLICATION_READY
reboot -f
