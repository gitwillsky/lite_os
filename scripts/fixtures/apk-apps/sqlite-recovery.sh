#!/bin/sh
set -eu

database=/root/sqlite-gate.db
[ -f "$database" ]
integrity=$(sqlite3 "$database" 'PRAGMA integrity_check;')
count=$(sqlite3 "$database" 'SELECT count(*) FROM records;')
echo "LITEOS_SQLITE_RECOVERY_STATE integrity=$integrity count=$count"
[ "$integrity" = ok ]
[ "$count" -ge 1 ]
if [ -f /run/normal.inittab ]; then
    cp /run/normal.inittab /etc/inittab
    sync
fi
echo LITEOS_SQLITE_RECOVERY_READY
while :; do sleep 1; done
