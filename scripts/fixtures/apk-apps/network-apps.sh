#!/bin/sh
set -eu

port="$1"
expected_sha256="$2"
expected_commit="$3"
origin="https://liteos-gate.test:$port"
curl_work=/tmp/curl-gate
git_work=/tmp/git-gate
export GIT_PAGER=cat
. /run/apk-network-up.sh
start_gate_network
echo LITEOS_APK_NETWORK_READY

rm -rf "$curl_work"
mkdir "$curl_work"

# 1. 默认 trust policy 必须拒绝 hostname mismatch，并接受注入的测试 CA 与 redirect。
if curl --fail --silent --show-error "https://10.0.2.2:$port/payload.bin" \
    -o "$curl_work/rejected"; then
    exit 71
fi
curl --fail --silent --show-error --location "$origin/redirect" -o "$curl_work/payload"
printf '%s  %s\n' "$expected_sha256" "$curl_work/payload" | sha256sum -c -

# 2. 单个真实 curl 以四个独立 easy handle 并发 transfer；process/vfork 由专门 gate 覆盖。
curl --fail --fail-early --silent --show-error --max-time 30 \
    --parallel --parallel-max 4 \
    -o "$curl_work/payload.1" "$origin/payload.bin" \
    -o "$curl_work/payload.2" "$origin/payload.bin" \
    -o "$curl_work/payload.3" "$origin/payload.bin" \
    -o "$curl_work/payload.4" "$origin/payload.bin"
for index in 1 2 3 4; do
    cmp "$curl_work/payload" "$curl_work/payload.$index"
done

# 3. deadline 必须中断慢响应并返回 curl 标准 timeout status 28。
status=0
curl --fail --silent --show-error --max-time 1 "$origin/slow" \
    -o "$curl_work/slow" || status=$?
[ "$status" -eq 28 ]
echo LITEOS_CURL_APPLICATION_READY

rm -rf "$git_work"
mkdir "$git_work"

# 4. 本地 object/index/ref/worktree mutation 必须覆盖 commit、branch、merge 与 tag。
git -C "$git_work" init -q -b main local
git -C "$git_work/local" config user.name 'LiteOS Gate'
git -C "$git_work/local" config user.email 'gate@liteos.invalid'
printf 'base\n' > "$git_work/local/file.txt"
git -C "$git_work/local" add file.txt
git -C "$git_work/local" commit -qm base
git -C "$git_work/local" checkout -qb feature
printf 'feature\n' >> "$git_work/local/file.txt"
git -C "$git_work/local" commit -qam feature
git -C "$git_work/local" checkout -q main
git -C "$git_work/local" merge -q --no-edit feature
git -C "$git_work/local" tag verified
[ -z "$(git -C "$git_work/local" status --porcelain)" ]
git -C "$git_work/local" show-ref --verify --quiet refs/tags/verified
echo LITEOS_GIT_LOCAL_READY

# 5. 默认 TLS verification 下完成 dumb-HTTP clone 与 fetch，禁止 http 降级。
git clone -q "$origin/repo.git" "$git_work/clone"
[ "$(git -C "$git_work/clone" rev-parse HEAD)" = "$expected_commit" ]
[ "$(cat "$git_work/clone/fixture.txt")" = 'git-over-https' ]
git -C "$git_work/clone" fetch -q origin gate-extra:refs/remotes/origin/gate-extra
git -C "$git_work/clone" show-ref --verify --quiet refs/remotes/origin/gate-extra
echo LITEOS_GIT_REMOTE_READY

# 6. clone 后的 index/worktree 必须保持 clean。
[ -z "$(git -C "$git_work/clone" status --porcelain)" ]
echo LITEOS_GIT_APPLICATION_READY
while :; do sleep 1; done
