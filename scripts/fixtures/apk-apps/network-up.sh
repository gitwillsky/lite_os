#!/bin/sh

# 接口由原生 netcfg 在启动时经 DHCP 配置，与 Linux 用户态无关；这里只等待租约生效。
start_gate_network() {
    attempts=0
    until ifconfig eth0 | grep -q 'inet addr:10.0.2.'; do
        attempts=$((attempts + 1))
        [ "$attempts" -lt 30 ]
        sleep 1
    done
}
