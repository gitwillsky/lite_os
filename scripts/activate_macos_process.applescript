(*
等待指定 PID 注册为 Cocoa application，然后将其切到前台。

参数：唯一参数是即将由 exec 保持不变的进程 PID。
返回：目标进程成为 frontmost 后正常退出。
错误：参数非法或进程未在期限内注册时抛出。
*)
on run arguments
    -- 1. PID 是 Shell 与 exec 后 QEMU 共享的 identity；缺失时可能错误激活无关应用。
    if (count of arguments) is not 1 then error "expected one process ID"
    set processId to item 1 of arguments as integer
    if processId is less than 1 then error "expected a positive process ID"

    tell application "System Events"
        -- 2. QEMU 在 Cocoa 初始化后才注册 application process；立即查询会稳定得到空结果。
        repeat 60 times
            if exists first application process whose unix id is processId then
                -- 3. makeKeyAndOrderFront 只改变 QEMU 内部窗口顺序；这里负责跨应用激活。
                set frontmost of first application process whose unix id is processId to true
                return
            end if
            delay 0.05
        end repeat
    end tell
    error "process " & processId & " did not register as a Cocoa application"
end run
