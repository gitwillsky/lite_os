#!/bin/sh
set -eu

inspect=/run/liteui-inspect

"$inspect" wait-ready
session=$($inspect pid session)
compositor=$($inspect pid compositor)
shell=$($inspect pid shell)
terminal=$($inspect pid terminal)
application=$($inspect pid application)

pointer_before=$($inspect pointer-samples)
echo LITEOS_DESKTOP_POINTER_ARMED
read -r phase
[ "$phase" = pointer ]
"$inspect" wait-pointer "$pointer_before"
echo LITEOS_DESKTOP_POINTER_OK

drag_before=$($inspect pointer-samples)
echo LITEOS_DESKTOP_DRAG_ARMED
read -r phase
[ "$phase" = drag ]
"$inspect" wait-pointer "$drag_before"
echo LITEOS_DESKTOP_DRAG_OK

release_before=$($inspect frames)
echo LITEOS_DESKTOP_RELEASE_ARMED
read -r phase
[ "$phase" = release ]
"$inspect" wait-frame "$release_before"
echo LITEOS_DESKTOP_RELEASE_OK

key_before=$($inspect frames)
echo LITEOS_DESKTOP_KEY_ARMED
read -r phase
[ "$phase" = key ]
"$inspect" wait-frame "$key_before"
echo LITEOS_DESKTOP_KEY_OK

resize_before=$($inspect resize-commits)
echo LITEOS_DESKTOP_RESIZE_ARMED
read -r phase
[ "$phase" = resize ]
"$inspect" wait-resize "$resize_before" 1280 720
[ "$($inspect pid session)" = "$session" ]
[ "$($inspect pid compositor)" = "$compositor" ]
[ "$($inspect pid shell)" = "$shell" ]
[ "$($inspect pid terminal)" = "$terminal" ]
[ "$($inspect pid application)" = "$application" ]
echo LITEOS_DESKTOP_RESIZE_OK

[ "$($inspect rss session)" -le 1024 ]
[ "$($inspect rss broker)" -le 2048 ]
[ "$($inspect rss compositor)" -le 12288 ]
[ "$($inspect rss shell)" -le 4096 ]
[ "$($inspect rss terminal)" -le 4096 ]
[ "$($inspect rss application)" -le 3072 ]
echo LITEOS_DESKTOP_RSS_OK

ticks_before=0
for role in session broker compositor shell terminal application; do
    value=$($inspect ticks "$role")
    ticks_before=$((ticks_before + value))
done
sleep 2
ticks_after=0
for role in session broker compositor shell terminal application; do
    value=$($inspect ticks "$role")
    ticks_after=$((ticks_after + value))
done
[ $((ticks_after - ticks_before)) -le 4 ]
echo LITEOS_DESKTOP_IDLE_OK

kill -9 "$shell"
new_shell=$($inspect wait-pid shell "$shell")
[ "$($inspect pid compositor)" = "$compositor" ]
[ "$($inspect pid terminal)" = "$terminal" ]
[ "$($inspect pid application)" = "$application" ]

kill -9 "$compositor"
new_compositor=$($inspect wait-pid compositor "$compositor")
[ "$new_compositor" != "$compositor" ]
[ "$($inspect pid session)" = "$session" ]
"$inspect" wait-ready
[ "$($inspect pid shell)" != "$new_shell" ]
[ "$($inspect pid terminal)" != "$terminal" ]
[ "$($inspect pid application)" != "$application" ]
echo LITEOS_DESKTOP_RECOVERY_OK
echo LITEOS_DESKTOP_RUNTIME_61
