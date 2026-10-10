# Pipe 与 readiness 契约

Socket 创建与 accept 的准备事务：PreparedSocket 唯一持 Arc<MaybeUninit<Socket>>，
尚无 backend/drop lifecycle。PreparedSocketDescription 在 backend 创建或 accept 前准备实际 OFD
allocation 和 socket inode/dentry；metadata identity 只移交实际 OpenedFile，不留第二份 facade cache。
accept 的 backend 只有准备完整才能消费连接，随后 facade/OFD/Weak root publication 均不分配；
EAGAIN 归还同一准备对象重试。syscall 先在原 FileTable 上预留不可见 fd，peer address copyout
完成后再 publish。socketpair 与 pipe2 使用同一 N-slot reservation owner，无分配在一次 table
acquisition 中成对发布；copyout/第二 slot OOM 只取消原表的不可见 slots，不通过替换后的 Thread handle 回滚。
socketpair 依固定 __sys_socketpair 在 socket 创建前 copyout reserved numbers；普通 socket 的
SocketRequest 先验证组合再准入 fd，只有这个 enum 决定具体 backend，创建路径不再重判 raw tuple。
新增 guest 用例覆盖 EMFILE 后 pending connection 仍可 accept、address EFAULT 后 fd reservation
归还、null socketpair 输出 EFAULT，以及 native ancillary 的空/short-int/trailing padding 行为。

SocketWaitSource 是唯一 opaque notification capability；旧 Data/Pipe direction 分支已移除。file readiness 与 transient poll 只把实际 edge Pipe 登记为 read interest，不继承 userspace event mask。stream::StreamPollState 只投影真实 I/O 四个 facts，不创
建无意义的 Pipe capacity/rendezvous。scope owner/new interface 基线在整批实现后统一生成。
