#!/bin/sh
set -eu

. /run/liteos-agent/versions
APK='/sbin/apk.static --no-network --no-progress'

# 1. Claude 的官方签名 index 已由 host 验证，并以 rootfs trust-store 的 local key 重新签名
# 原始 control/data；全部 dependency 固定摘要后注入，禁止 Guest 在线解析滚动仓库。
$APK add /run/liteos-agent/apks/*.apk
$APK info -e "claude-code=$LITEOS_CLAUDE_VERSION"
$APK info -e 'bash=5.2.37-r0'
$APK info -e 'git=2.49.1-r0'
$APK info -e 'curl=8.14.1-r3'
$APK info -e 'ripgrep=14.1.1-r0'
echo LITEOS_AGENT_APKS_READY

# 2. Codex 官方 musl executable 由开发环境 owner 发布到 /usr/local/bin；缺少固定 owner 会
# 让 npm global cache 和 Node runtime 再次成为不可复现的第二条安装路径。
mkdir -p /usr/share/liteos

# 3. 真实执行两个原生 CLI 与自举工具；任一个 loader/syscall 契约失败都不会发布成功 marker。
case "$(readlink /proc/self/exe)" in
    /*) ;;
    *) echo '/proc/self/exe did not resolve to an absolute executable path'; exit 1 ;;
esac
echo LITEOS_AGENT_CODEX_START
codex_version="$(/usr/local/bin/codex --version)"
echo LITEOS_AGENT_CLAUDE_START
claude_version="$(/usr/bin/claude --version)"
case "$codex_version" in
    *"$LITEOS_CODEX_VERSION"*) ;;
    *) echo "unexpected Codex version: $codex_version"; exit 1 ;;
esac
case "$claude_version" in
    *"${LITEOS_CLAUDE_VERSION%-r1}"*) ;;
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
