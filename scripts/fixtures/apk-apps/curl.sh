#!/bin/sh
set -eu

origin="https://liteos-gate.test:$1"
expected_sha256="$2"
work=/tmp/curl-gate
. /run/apk-network-up.sh
start_gate_network
rm -rf "$work"
mkdir "$work"

# 1. 默认 trust policy 必须拒绝 hostname mismatch，并接受注入的测试 CA 与 redirect。
if curl --fail --silent --show-error "https://10.0.2.2:$1/payload.bin" -o "$work/rejected"; then
    exit 71
fi
curl --fail --silent --show-error --location "$origin/redirect" -o "$work/payload"
printf '%s  %s\n' "$expected_sha256" "$work/payload" | sha256sum -c -

# 2. 多个并发 transfer 必须独立完成且内容一致。
for index in 1 2 3 4; do
    curl --fail --silent --show-error --max-time 30 "$origin/payload.bin" \
        -o "$work/payload.$index" &
    printf '%s\n' "$!" >> "$work/pids"
done
concurrent_status=0
for pid in $(cat "$work/pids"); do
    wait "$pid" || concurrent_status=$?
done
[ "$concurrent_status" -eq 0 ]
for index in 1 2 3 4; do
    cmp "$work/payload" "$work/payload.$index"
done

# 3. deadline 必须中断慢响应并返回 curl 标准 timeout status 28。
status=0
curl --fail --silent --show-error --max-time 1 "$origin/slow" -o "$work/slow" || status=$?
[ "$status" -eq 28 ]
echo LITEOS_CURL_APPLICATION_READY
while :; do sleep 1; done
