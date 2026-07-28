#!/bin/sh
set -eu

. /run/liteos-agent/versions
APK='/sbin/apk.static --no-network --no-progress'
# init 启动的非 login shell 不读取 /etc/profile；这里与产品 login/PTY owner 发布同一标准
# PATH。缺少显式投影时 npm 已正确生成 /usr/local/bin 入口，bootstrap 却会误报 command not found。
PATH='/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin'
export PATH

# 1. 清除旧 Claude APK owner，再离线安装固定 Node/npm 与开发工具闭包。若保留旧 APK，
# `/usr/bin/claude` 会与 npm 的 `/usr/local/bin/claude` 形成两个可更新入口。
if $APK info -e claude-code; then
    $APK del claude-code
fi
$APK add /run/liteos-agent/apks/*.apk
$APK info -e 'nodejs=22.23.0-r0'
$APK info -e 'npm=11.6.4-r0'
$APK info -e 'bash=5.2.37-r0'
$APK info -e 'git=2.49.1-r0'
$APK info -e 'curl=8.14.1-r3'
$APK info -e 'ripgrep=14.1.1-r0'
echo LITEOS_AGENT_APKS_READY

# 2. 用官方 npm 命令从固定 cache 安装两个包。cache 已由 host 对照 registry SRI 和
# linux-arm64-musl optional package 验证；`--offline` 保证 Guest 不解析滚动版本。
mkdir -p /usr/share/liteos
rm -rf /run/liteos-agent/npm-cache
mkdir -p /run/liteos-agent/npm-cache
# 大型原生 package 在 ext3 上展开时可能合法地超过 runtime gate 的静默窗口。后台 owner 与
# `wait` 保留真实退出状态，固定 heartbeat 只证明 bootstrap 仍在推进，不放宽成功条件。
tar -xf /run/liteos-agent/npm-cache.tar -C /run/liteos-agent &
cache_pid=$!
while kill -0 "$cache_pid" 2>/dev/null; do
    echo LITEOS_AGENT_NPM_CACHE_EXTRACTING
    sleep 5
done
wait "$cache_pid"
echo LITEOS_AGENT_NPM_CACHE_READY
rm -rf /usr/local/lib/node_modules/@openai/codex
rm -rf /usr/local/lib/node_modules/@anthropic-ai/claude-code
rm -f /usr/local/bin/codex /usr/local/bin/claude
cat > /etc/npmrc <<'EOF'
prefix=/usr/local
registry=https://registry.npmjs.org/
EOF
npm install --global \
    --offline \
    --cache /run/liteos-agent/npm-cache \
    --os=linux \
    --cpu=arm64 \
    --libc=musl \
    --include=optional \
    --no-audit \
    --no-fund \
    "@openai/codex@$LITEOS_CODEX_VERSION" \
    "@anthropic-ai/claude-code@$LITEOS_CLAUDE_VERSION" &
npm_pid=$!
while kill -0 "$npm_pid" 2>/dev/null; do
    echo LITEOS_AGENT_NPM_INSTALLING
    sleep 5
done
wait "$npm_pid"
npm list --global --depth=0 \
    "@openai/codex@$LITEOS_CODEX_VERSION" \
    "@anthropic-ai/claude-code@$LITEOS_CLAUDE_VERSION"
test "$(command -v codex)" = /usr/local/bin/codex
test "$(command -v claude)" = /usr/local/bin/claude
echo LITEOS_AGENT_NPM_READY

# 3. 真实执行 npm launchers 与自举工具；任一个 Node/loader/syscall 契约失败都不发布 marker。
case "$(readlink /proc/self/exe)" in
    /*) ;;
    *) echo '/proc/self/exe did not resolve to an absolute executable path'; exit 1 ;;
esac
echo LITEOS_AGENT_CODEX_START
codex_version="$(codex --version)"
echo LITEOS_AGENT_CLAUDE_START
claude_version="$(claude --version)"
case "$codex_version" in
    *"$LITEOS_CODEX_VERSION"*) ;;
    *) echo "unexpected Codex version: $codex_version"; exit 1 ;;
esac
case "$claude_version" in
    *"$LITEOS_CLAUDE_VERSION"*) ;;
    *) echo "unexpected Claude version: $claude_version"; exit 1 ;;
esac
git --version
curl --version
bash --version
rg --version
echo "$codex_version"
echo "$claude_version"

cp /run/liteos-agent/stamp.json /usr/share/liteos/agent-development.json
chmod 0644 /usr/share/liteos/agent-development.json
cp /run/liteos-agent/normal.inittab /etc/inittab
rm -rf /run/liteos-agent
sync
echo LITEOS_AGENT_DEVELOPMENT_READY
while :; do sleep 1; done
