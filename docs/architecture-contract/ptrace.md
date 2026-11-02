# ptrace 契约

本文领域位于 `system/linuxd/src/`；native 执行与内存机制见[执行域契约](execution.md)和[内存契约](memory.md)。

- `process_table::ptrace` 是逐 Thread tracing relation、stop event、resume 和 wait claim 的唯一 owner。
  `ProcessGraph.tracees` 按实际内部 TID 索引；每个 tracer Process 的 `ptracees` 只保存 membership，
  wait 扫描自己的目标集合，精确 TID 直接查找。没有目标时 signal generation 不扫描全局进程或 tracer。
  relation 与 immutable wait token 共用 `Arc<PidRegistration>`；token 捕获 UID/status/PID，
  即使另一个 tracer Thread 退出或 detach 删除 relation，wait copyout 也不再读取可消失的 graph node。
  claim 完成按 Arc identity 验证原 attachment，不能碰同 TID 的新 relation。真实 association 保留编号到 token 退休，禁止历史 Arc 导致 TID 被复用后误选新线程；
  relation 不持 TCB/MM/files。ATTACH 预留 relation、membership 和 PID link 后，取得目标 killable exec
  credential guard，按 ATTACH_REALCREDS 授权，graph 下复核 PF_EXITING/重复附加后一次发表。
  TRACEME 使用 parent 的 commoncap permitted/UserNS policy，不套用 ATTACH 的 UID/dumpability 条件。
- 信号产生时保留需要 tracer 观察的 provenance；ignored 信号仍能唤醒目标，不能借此绕过 PID1 的 SIGKILL
  豁免。目标在真实 signal delivery 处停止；post-switch handoff 完成后才发表 stopped，寄存器访问与
  resume/fatal signal 在同一 graph transaction 串行化。`ptrace::notification` 统一 stopped、Thread exit、Process exit 的通知事务；状态与 SIGCHLD 必须先同锁发表，
  才能在图外唤醒 tracer；否则 wait 已返回而迟到 SIGCHLD 可进入后续临时 mask，污染 sigwait 与 UID 配额。
  节点在图外准备，提交前按实际 tracer Arc/关系复查；复用 child notification owner 的 IGN/NOCLDSTOP 政策，
  未使用 preparation 在 wake 前退款，无新队列、锁或常驻状态。实际 ptrace fixture 在 wait 返回后立即
  交错 CONT/DETACH，再用零 timeout 读取 CLD_TRAPPED/CLD_EXITED，保护该顺序；成本沿用完整 musl 运行时。
  SIGCONT 不得解除 ptrace-stop；SIGKILL 不被 tracer
  延迟。tracer 在 pending 消费之后、handler/SA_RESETHAND 选择之前决定信号，失效 timer 先由原 dequeue owner
  验证/退休。resume 结果一次性移交 Thread 原 signal frame。stop signal 与可被 SETSIGINFO 修改的 si_signo 独立；
  注入号不同于 si_signo 时重建 tracer 的 SI_USER。若被目标屏蔽，归还 dequeue 捕获的原 private/shared 队列，
  不绕过 mask 直接调用 handler；重新解除屏蔽后的 pending 按原 signal-delivery 规则处理。
- wait4 对 ptrace-stop 不要求 WUNTRACED，ptrace 目标隐含 __WALL；waitid 按 WSTOPPED/WEXITED 选择，WNOWAIT
  释放观察 claim。real parent 在外部 tracer 消费最终 exit 前仍有 child，但不能提前 reap；最后一次真实
  retirement 将 leader 保留为 Traced，tracer 消费/退出后才沿原 parent notification/autoreap 路径发布。
  同一 real-parent/tracer group 只消费一次退出状态并聚合 child usage。tracer Thread 退出自动解除它的全部
  relation，停止中的 signal frame以 signal=0 恢复；不保留 stale waiter 或 MM。非 leader exec 同事务重指
  tracer TID、tracee PID registration 与 membership，传统附加在新 context 完整提交后产生 SIGTRAP。
  不具目标 UserNS SYS_PTRACE 的 tracer 不能利用 tracee 的 set-id/file-capability exec 增权。
- 被中断的管道 read/write 在唯一 I/O wait adapter 返回内部 ERESTARTSYS；普通 handler 决定 SA_RESTART/EINTR。
  ptrace-stop 在交付信号前先展示重绕后的 syscall PC/参数，restart record 只记录一次 presentation。
  CONT 后保持 tracer 修改的 PC/参数；仅 PC 仍在原指令时按实际 handler 决定恢复 EINTR 或 restart-block
  入口，不再整组覆盖已观察的寄存器。GET/SETREGSET 改 PC 的实际管道阻塞回归必须可恢复执行。
- syscall adapter 只拥有 raw request、iovec/siginfo user-copy 与 errno；architecture context codec 只暴露
  Linux NT_PRSTATUS，禁止修改 kernel metadata/特权状态。user-copy 在 graph 外，register transaction 在
  stopped graph guard 内；请求在 user-copy 前固定 attachment token，执行时复核同一 Arc，禁止 detach/re-attach 或 PID 复用更换目标。
  AArch64 和 RV64 使用同一静态 façade，无 runtime architecture dispatch。
  MM owner 的 debugger word access 复用实际 fault、canonical page、COW、dirty 与 instruction publication；
  private RX/PROT_NONE 不临时提高 VMA/PTE 权限，其他共享 mapping 仍须实际读写权限，设备不能强制访问。
- benchmark 决策：ATTACH、ptrace request、exec/exit 是冷路径；relation/membership/PID link 与其 Arc token 仅 ATTACH 分配。
  普通 signal 只增加空 registry 检查和目标 TID lookup，无新锁/atomic/分配；ignored pending 的 provenance
  检查有界于 64 个信号，复用原 pending owner，不维护第二份 pending mask。wait/exit 只访问 tracer 自身
  membership，精确 wait 不扫描其他 debugger；实际双架构 musl fixture 验证权限、RX/PROT_NONE COW、stop/wait、
  WNOWAIT、register validation、detach、线程独立停止、tracer exit 和 tracee exec/exit。
