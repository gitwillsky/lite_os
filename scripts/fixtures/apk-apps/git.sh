#!/bin/sh
set -eu

origin="https://liteos-gate.test:$1/repo.git"
expected_commit="$2"
work=/tmp/git-gate
export GIT_PAGER=cat
. /run/apk-network-up.sh
start_gate_network
rm -rf "$work"
mkdir "$work"

# 1. 本地 object/index/ref/worktree mutation 必须覆盖 commit、branch、merge 与 tag。
git -C "$work" init -q -b main local
git -C "$work/local" config user.name 'LiteOS Gate'
git -C "$work/local" config user.email 'gate@liteos.invalid'
printf 'base\n' > "$work/local/file.txt"
git -C "$work/local" add file.txt
git -C "$work/local" commit -qm base
git -C "$work/local" checkout -qb feature
printf 'feature\n' >> "$work/local/file.txt"
git -C "$work/local" commit -qam feature
git -C "$work/local" checkout -q main
git -C "$work/local" merge -q --no-edit feature
git -C "$work/local" tag verified
[ -z "$(git -C "$work/local" status --porcelain)" ]
git -C "$work/local" show-ref --verify --quiet refs/tags/verified

# 2. 默认 TLS verification 下完成 dumb-HTTP clone 与 fetch，禁止 http 降级。
git clone -q "$origin" "$work/clone"
[ "$(git -C "$work/clone" rev-parse HEAD)" = "$expected_commit" ]
[ "$(cat "$work/clone/fixture.txt")" = 'git-over-https' ]
git -C "$work/clone" fetch -q origin gate-extra:refs/remotes/origin/gate-extra
git -C "$work/clone" show-ref --verify --quiet refs/remotes/origin/gate-extra

# 3. clone 后的 index/worktree 必须保持 clean。
[ -z "$(git -C "$work/clone" status --porcelain)" ]
echo LITEOS_GIT_APPLICATION_READY
while :; do sleep 1; done
