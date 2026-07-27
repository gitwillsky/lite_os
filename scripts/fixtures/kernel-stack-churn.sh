#!/bin/sh

# 1. 每轮并发创建 16 个立即退出的 child，迫使不同 CPU 高频分配和回收 kernel stack。
# 2. wait 封闭本轮全部 child 生命周期，下一轮会立即复用刚释放的 virtual handle 与 frame。
# 3. 只有 100 轮全部完成才发布 marker；stale TTBR1 translation 会在此前触发 panic 或挂死。
i=0
while [ "$i" -lt 100 ]; do
    j=0
    while [ "$j" -lt 16 ]; do
        true &
        j=$((j + 1))
    done
    wait
    i=$((i + 1))
done
echo LITEOS_KERNEL_STACK_CHURN_42
