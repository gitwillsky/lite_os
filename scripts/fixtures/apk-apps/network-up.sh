#!/bin/sh

start_gate_network() {
    /etc/init.d/network-service &
    gate_network_pid=$!
    trap 'kill "$gate_network_pid" 2>/dev/null || true' EXIT
    attempts=0
    until ifconfig eth0 | grep -q 'inet addr:10.0.2.'; do
        attempts=$((attempts + 1))
        [ "$attempts" -lt 30 ]
        sleep 1
    done
}
