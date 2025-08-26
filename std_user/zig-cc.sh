#!/usr/bin/env bash
exec zig cc -target riscv64-linux-musl -static "$@"
