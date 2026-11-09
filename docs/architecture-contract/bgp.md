# BGP 契约

本文件拥有 BGP 的格式边界与实现限制；固定来源见[网络规范基线](../standards-baseline/network.md#bgp)。
当前已有编解码、system/bgp 会话/UPDATE 裁决、属性驻留、Adj-RIB-In、选择核心与 Loc-RIB 预备/提交；原生 actor、政策/NHT 输入和来源发布尚未装配，不能据此声称能建立真实邻居或转发 BGP 路由。

## 格式所有权

`libraries/packet-wire::bgp` 唯一拥有消息头、OPEN/UPDATE 与双栈普通 NLRI 的字节布局。
只依赖 core，无堆分配、锁、全局状态、计时器、设备/RIB 权限或 Linux ABI；协议进程不能再复制字段 codec。

1. Header 检查十九字节共同头的 marker、类型及长度，完整正文缓冲分配前即可校验上限。
   HeaderError 区分截断、Marker、Length、Type，供会话 owner 选择对应 NOTIFICATION，不要求它重新实现头解析。
   Frame::read 借用流缓冲的首个完整帧及未消费后缀，不完整返回 None；非法完整头立即失败，不扫描 marker 猜测重新同步。
   Frame::parse 面向独立帧，额外尾部拒绝。两者共用同一 Header，不另建串流解析状态或复制完整消息。
   extended 是调用方向已授权的接收/发送能力，不能因本地实现支持便设为 true；普通上限 4,096，扩展上限 65,535。
   OPEN 始终受 4,096 限制，KEEPALIVE 必须为十九字节；UPDATE/NOTIFICATION 保留各自最低长度。
   当前 ROUTE-REFRESH 仅固定四字节正文，保留 AFI/SAFI/subtype；不支持 ORF 的可变长消息，不据 subtype 宣称已协商增强刷新。
2. OPEN 的版本、两字节 My AS、hold time 与 Identifier 保留原值；格式捕获不替代版本、AS_TRANS、对端身份和计时协商。
   RFC 9072 由非零 Non-Ext OP Len 后的类型 255 识别扩展；识别后忽略前者数值，参数总长和每参数长度改用 u16。
   普通总长度恰为 255 不等于使用扩展；普通/扩展参数沿同一 owner 解码，保留未知/重复参数。
   参数类型 2 才可解释 capability TLV；参数 framing、capability framing 和已知 capability 值宽度分别检查。
   当前类型化 MP、Route Refresh、Extended Message 与四字节 AS；未知 capability 保留，不能由此接纳 ADD-PATH 等能力。
   Capabilities::emit 清 MP 保留字节，Open::emit 写版本 4、拒绝 1/2 秒 hold time、必要时选扩展参数布局，均先检查全部范围再写输出。
3. UPDATE 先捕获撤回、属性和基础 IPv4 NLRI 的独立区间；一个属性的内容错误不影响其他区间读取。
   Attributes::parse 提供完整验证视图；scan/Update::scan_attributes 顺序交出有效前缀，首次结构错误报告一次并停止。
   后者供 RFC 7606 保留先到的 MP NLRI，不能跳过错误裁决或按猜测的下一属性位置继续扫描。
   属性短/扩展长度均检查；未知/重复项、原 flags 保留，必需属性、重复/flags 处置仍归协议 owner。
   发送按值长度选择长度字段，清 flags 低四位；接收不把这些应忽略位误判为错误。
   ORIGIN/AS_PATH/NEXT_HOP/MED/LOCAL_PREF/ATOMIC_AGGREGATE/AGGREGATOR、三类 community、反射字段与 MP 属性可独立检查内容。
   列表宽度和非空要求、固定值宽度/枚举检查不等于属性传播策略、团体匹配或最佳路径已实现。
4. AS_PATH 的两/四字节宽度由所属方向协商给定，不按长度猜测；AS4_PATH/AS4_AGGREGATOR 固定四字节宽度。
   空路径合法，零项/未知类型/截断段非法；set、sequence、联盟段类型保留，不在格式层改写路径含义。
   AS_PATH 生成检查每段 1..255 项与两字节数值上限，直接写最终属性值；不会截断四字节 AS 号。
   AS4 重建、发送投影、选路跳数及纯选择核心归 system/bgp；出口政策/AS prepend、联盟准入与选择核心的运行时装配仍待接入。
5. NLRI 只解释两族普通 unicast/multicast 前缀；完整检查长度/范围，读取时规范化无关尾位，发送不泄漏主机位。
   MP_REACH 保留族/SAFI、原始下一跳与 NLRI，接收忽略 reserved；MP_UNREACH 保留空列表供 End-of-RIB。
   未知 AFI/SAFI 仍保留属性原值，但 prefixes 明确返回 Unsupported，不套用普通前缀解释 VPN/带标签/ADD-PATH。
   下一跳的长度组合、链路范围与跨族下一跳须按会话协商接纳；格式捕获不建立可达性证明。
   MP 生成从所属 Nlri 取得 AFI，清 reserved，直接写最终属性值；最终可达证明仍应从授权 RIB 跟踪能力取得。

## 会话状态与 OPEN 接纳

system/bgp 独占 Config 的不可变会话投影、六状态、协商结果与 retry/hold/keepalive 的绝对单调截止；会话/接收核心不分配，路径驻留使用 alloc 与 fallible-tree，不依赖内核或 Linux UAPI。
Config 不是另一份持久配置文件/Schema，未来配置服务须从唯一树投影；已有会话参数改变时须先完成旧会话终止，不能人工同步两份政策。

1. Config 检查非零 AS/Identifier、合法 hold time 和非零 retry；Identifier 按 RFC 6286 的非零整数检查，不要求它是本机或单播地址。
   emit_open 复用唯一 codec，固定 22-byte 栈窗口构造 capability，始终携真实四字节 AS，My AS 不可表示时使用 AS_TRANS；只宣告本地启用的族/能力。
   接纳检查版本、AS、Identifier/同 AS 重复 ID、hold time、可选参数/capability，未知 capability 忽略，未知参数的错误通知保留完整原参数头/值。
   两个 NEW speaker 使用 capability 中的真实 AS 取代 My AS；重复相同 AS4 接纳、冲突拒绝，不从 My AS 截断值恢复身份。
   hold time 取双方最小值，任一为零则关闭周期 hold/keepalive；MP 按交集，未见 MP 时允许配置中的基础 IPv4，未知族不被误接纳。
   扩展消息/刷新保存方向授权；本地未宣告时不能放宽接收长度。Session 再核对已捕获帧的接收上限，避免调用者用错误的 Frame 参数绕过会话授权。
2. Start 主动进入 Connect 并请求连接，被动进入 Active 且无主动重试；重试/失败按唯一 retry 截止推进，没有定时轮询。
   当前 TCP 确认进入 OpenSent、请求发送 OPEN 并启动 240 秒初始 HoldTimer；尚未收到 OPEN 时不能使用未协商的 hold time。
   有效 OPEN 进入 OpenConfirm，请求初始 KEEPALIVE；对端 KEEPALIVE 才进入 Established。
   KEEPALIVE 完整送入 TCP 后才设置下个发送期限，已发送 UPDATE 同样刷新发送计时；收到 KEEPALIVE/UPDATE 刷新 HoldTimer。
   保活到期只交出一次发送工作，等待 Sent 再重武装；发送背压不能形成到期忙循环，HoldTimer 仍能结束卡住的连接。
   旧 timer 唤醒按当前截止复核；重复当前连接确认不关闭活跃连接。传输 owner 必须先过滤已退休连接/尝试的旧事件，不能把它们交给新会话。
3. 终止只产生一个 Close 命令：可选通知为 best-effort，失败也必须关闭 TCP；已建立连接还必须完成来源撤回。
   withdraw_routes 保存退出前的路由清理义务，send_extended 保存通知发送权限；协商随状态立即退休，执行器不能再查询它来猜测清理或发包规则。
   通知发送失败不得短路 Close/撤回；撤回未完成不得继续新连接，actor 必须保留实际未完成工作并重试，不能假装转换到 Idle 就已完成外部清理。
   普通失败在 Idle 持 retry 截止；Stop 关闭全部期限、不重连。收到 NOTIFICATION 不回复另一个错误通知，异常状态消息带 RFC 6608 子码和原消息类型。
   碰撞 loser 停止且不重连；prefer_outgoing 按较大 Identifier 发起者裁决，ID 相等的 eBGP 按较大实际 AS，已有 Established 由 peer owner 优先保留。
   该纯比较函数不等于双连接协调已装配；Input::CollisionLost 必须来自实际 peer owner 的决定。
4. Established 的 UPDATE 先经下节接收裁决及 AS4 接收重建，才移交增量决定；严重错误沿同一关闭命令处理，不在解析途中发布路由。入口策略、选择/Loc-RIB 的原生装配与来源工作仍须接入。
   Route Refresh 必须已在本地宣告，未宣告的族/SAFI 忽略；原 reserved/subtype 按基础刷新语义处理，不宣称增强刷新已接入。
   Effects 只有固定两槽，最坏为“取消旧尝试、发起新尝试”；不新增锁、缓存、堆消息或协议状态副本。

## UPDATE 接收裁决

received module 只由 Session 为同一 Frame 的 UPDATE 视图/原包调用；ReceivedUpdate 的私有字段保证原包、属性区与结果属于同一次输入。
整个消息检查完成才返回增量决定；不在逐属性/前缀解析期间修改来源或下一跳状态。

1. 先检查基础撤回和宣告 NLRI，再顺序捕获属性与完整原 wire；唯一 codec 拥有长度布局，错误通知不在会话层重建头字段。
   普通重复属性只处理首项，后续副本不能修复先到的坏值；MP_REACH/MP_UNREACH 重复则重置会话，即使该族未启用也不例外。
   未知 well-known 属性产生标准 3/2 通知并附原属性；未知 optional non-transitive 忽略，optional transitive 保留给后续传播 owner。
   已知属性检查 Optional/Transitive 期望值，well-known/optional non-transitive 的 Partial 置位也按错误处理；低保留位不误作错误，原属性不是可直接发送的最终属性。
2. ORIGIN/AS_PATH/NEXT_HOP/MED、内部邻居的 LOCAL_PREF/ORIGINATOR_ID/CLUSTER_LIST，以及三类 community 的内容错误按 TreatAsWithdraw。
   路径中的 AS 0 按 RFC 7607 分类；坏 ATOMIC_AGGREGATE/AGGREGATOR 与 OLD peer 的坏 AS4 辅助属性只丢弃属性，不能误撤回有效路由。
   eBGP 的 LOCAL_PREF/反射字段直接丢弃；NEW peer 携带 AS4_PATH/AS4_AGGREGATOR 也丢弃，不把不应使用的辅助值当成当前政策。
   path 提供按 RFC 6793 重建的有效路径，aggregator 提供按聚合器覆盖规则选定的节点；原 AS_PATH/AGGREGATOR 及两种辅助属性不混入普通保留序列。
   按撤回处理时不交出有效路径/聚合器；AS 号去环决策与传播仍须后续路由 owner 执行，不能把路径成员查询当成准入已经完成。
3. 有宣告时要求 ORIGIN/AS_PATH，基础 IPv4 宣告还要求 NEXT_HOP，iBGP 还要求 LOCAL_PREF；缺失按 TreatAsWithdraw。
   只撤回或 EoR 不伪造必需属性。多个错误取更强动作，不因先遇到可恢复错误跳过后续严重错误。
   属性 framing 截断依靠外层 Total Attribute Length 定位基础 NLRI；已辨认的 MP NLRI 也保留，不猜测重新同步位置。
   需要按撤回处理却没有可辨认的 reachable NLRI 时重置，不能忽略报文而保留旧路由；仅丢弃属性的错误不扩大为撤回。
4. MP 共同 framing 校验后才检查地址族/SAFI 协商范围，不对未知族套用普通前缀解释；当前只接纳已协商的双栈 unicast。
   当前未协商跨族下一跳，IPv4 需要四字节下一跳，IPv6 需要十六或三十二字节；第二个 IPv6 地址必须为 link-local。
   不能可靠定位 NLRI 的 MP 头/长度、错误 flags 或非法前缀导致 3/9 重置并引用原属性；基础 NLRI 非法导致 3/10。
   位置明确但地址非法的下一跳使整批按撤回处理；接口 zone、直连/eBGP 地址语义及可达证明仍须 transport/RIB owner 检查。
5. changes 先给全部显式撤回，再给宣告；TreatAsWithdraw 将每条可辨认宣告转换为撤回，不能只丢弃当前输入。
   整批决定必须由后续路由 owner 应用并发布，相同前缀的宣告覆盖本消息的显式撤回，不能逐项发布半个批次。
   空基础 UPDATE 或唯一空 MP_UNREACH 提供相应族 EoR 标记；不由此引入跨重启恢复或 Graceful Restart。
   原包和错误编号供运行时诊断留证，属性 framing 错误另有明确标志；原生结构化日志尚未接入，不声称已满足运行时诊断验收。

## AS4 接收重建

system/bgp::path 拥有有效 Path/PathSegment 视图；packet-wire 仍只拥有原始段的格式。ReceivedUpdate 完整捕获属性后一次选定路径和聚合器，不保留辅助接口双轨。
1. NEW peer 的辅助属性在接收裁决中丢弃；OLD peer 两种聚合器同时有效时，基础 ASN 非 AS_TRANS 则忽略两种 AS4 信息，保留基础聚合器/路径。
   基础 ASN 为 AS_TRANS 时改用辅助聚合器并正常重建路径；两者 Partial 取并集，不能清除上游不完整信息。仅有辅助聚合器时不凭空生成基础属性，只有基础值时保留它。
2. 序列逐 AS 计数、集合为一跳、联盟段为零。辅助路径跳数超过基础值则忽略；其联盟段去掉后为空也保留基础路径。
   否则保留基础路径前部差额再拼接辅助段，边界可截短序列、不可拆集合；基础前导/与完整保留段相邻的联盟段保留，位于被替换尾部之后的段不保留。
3. hop_count 保留基础选路长度，contains 检查重建后的全部段，支持后续四字节 AS 去环；auxiliary_partial 只保留实际参与重建的辅助属性标志，忽略的辅助属性不污染元数据。
   需跨消息保活时由 PathPool 规范化/共享，联盟成员准入仍未接入；Aggregator 是含 ASN/Identifier/Partial 的有效语义值，不再使用丢失传播标志的二元组。
   借用视图的 Eq 只比较其表示，不是路径集合排序/去重或相邻序列合并后的语义等价关系，不能直接充当属性驻留的 canonical key。

## AS4 发送方向

Path::emit_attributes 接受最终出口路径、可选聚合器及该方向协商的 AsnWidth；当前尚无运行服务调用，不代表完整 UPDATE/出口政策已装配。
1. NEW 方向写四字节基础 AS_PATH/AGGREGATOR，不附两种辅助属性。OLD 方向基础路径用两字节 AS，不能表示的普通 AS 置为 AS_TRANS。
   只有普通路径含不可映射 AS 才附四字节 AS4_PATH，保留完整普通路径、剔除联盟段；普通 AS 全部可映射时不发送辅助路径。
   不可映射聚合器在基础属性中使用 AS_TRANS，同时携真实 AS4_AGGREGATOR；可映射或缺失聚合器不额外携带辅助聚合器。
2. OLD 方向不能通过辅助路径恢复不可映射联盟成员，按 RFC 6793 的联盟过渡约束拒绝这种输入，不把它静默映射为 AS_TRANS。
   AS 0、属性值/完整属性序列超过 u16 及容量不足均在整批写入前失败，输出不变；出口 AS prepend、成员准入和跨联盟边界清理仍由后续出口政策负责。
3. packet-wire 的 AsPath::encoded_len/emit 接受可重复遍历的段/AS 迭代器；旧切片入口已替换，不保留另一份编码实现。
   Attribute::encoded_len/emit_with 唯一拥有短/扩展属性头，既有 Attributes::emit 也走此入口；值回调在尺寸预检后只写精确目标区间且不能返回失败。
   聚合器布局/尺寸也由 packet-wire 唯一编码。BGP 只选择语义值，不复制字段布局，不建立完整 AS 数组、中间属性缓冲或逐消息堆分配。

## 跨消息路径驻留

PathPool 是单个 BGP actor 的路径内容 owner；StoredPath 是一个可克隆指针，不复制 ASN、增加逐前缀堆节点或保活整个池。
1. 在 AS4 重建后捕获规范路径：相邻同类普通/联盟序列合并；普通/联盟集合排序去重，但相邻集合不合并，序列重复 prepend 不删除，集合与序列的选路代价不变。
   先用固定 255 项集合窗口计算实际 AS/段数，再可失败地按规范值预留两个紧凑向量，避免重复项或碎片化输入造成长期空余容量。
   AS 号为 u32，段只保存类型与 u32 结束索引；有效输入来自有消息长度上限的已重建路径，不按协议栈对象大小放大逐路由表示。
2. 规范内容通过已有 FallibleMap 的 AVL 比较驻留，不新增无随机种子的散列表或未经验证的 hash 算法。
   内容比较为 O(B)，查找/删除最多 O(log U) 次比较；U 为活跃不同路径数，B 为一次路径内容比较量，不按全部路由扫描。
   key 保活不可变规范值，value 只弱引用共享 Resident；Resident 又弱引用索引，最后一个路由引用 Drop 立即精确删除对应节点，没有引用环、后台清扫或死 key。
3. Rc/RefCell 限于同一 actor，类型不允许跨线程发送，没有全局缓存/新锁。查找与提交的借用均不跨分配、外部回调或 Resident 析构；所有可失败分配在发布前完成，失败保持旧索引/句柄不变。
   池先退出时弱 owner 无法升级，已有句柄仍保活规范数据；未发表候选析构只会尝试移除不存在的 key。并发 actor/线程不得共享这个可变索引。
4. StoredPath 的 Eq/Ord 比较规范内容，Ord 只用于身份/索引而非路径优劣；其发送按 255 项重新分段，复用 Path 的唯一 AS4 投影。完整属性通过 RouteAttributes::emit_path_attributes 提供聚合器/Partial，不把传播元数据塞进 ASN 内容身份。
   三个路径驻留用例覆盖分段/集合表示复用、序列/集合边界保持、最后引用/池退出及六百 AS 合并后 OLD/NEW 编码再重建的同一对象复用。

## 跨消息完整属性驻留

AttributePool 拥有一个 actor 的完整属性、独立路径及三类团体集合；RouteAttributes 为一指针不可变引用，不按前缀复制属性包。
1. intern 只捕获确有已接纳宣告的 ReceivedUpdate；仅撤回、EoR、TreatAsWithdraw 返回 None，不创建属性/路径对象，也不代替 RIB 的撤回处理。
   路径先经 PathPool 共享；NLRI、MP NLRI 和下一跳从身份中剥离，属于各条路由。ORIGIN、MED/LOCAL_PREF 的缺失/显式值、ATOMIC_AGGREGATE、有效聚合器均保留，不暗中添加入口政策缺省值。
2. 普通/扩展/大团体分别按其完整值排序去重，按紧凑规范值驻留；临时排序向量的重复项空间不进入长期对象。
   团体集合独立共享，不因属性包的 MED、Partial 或反射字段不同而复制成员；缺失用 None，不创建空集合对象。扩展团体的跨 AS 传播筛选仍属出口政策。
   ORIGINATOR_ID 与 CLUSTER_LIST 保留；反射链保留顺序和重复，不能套用团体集合归一化。未知 optional transitive 按 code 排序、保留精确值，供传播的视图按 RFC 4271 置 Partial；未知 non-transitive 不进入驻留。
3. 已识别可传递属性的 Partial 不得清除；AS4_PATH 标志与 ASN 身份分开持有，聚合器标志归 Aggregator。is_partial 与 emit_path_attributes 读取同一个内容对象，不另存人工同步的发送状态。
   路径、团体、完整属性的共享索引统一使用 crate 私有 residency 机制；每个 Pool<T> 只容纳不可变规范内容。引用图为属性到路径/团体，禁止同池自引用；索引不持强句柄，不执行后台扫描。
4. 所有捕获/节点分配可失败，索引借用不跨分配或内容析构。中途失败时候选的子组件引用自动退回；最后一个完整属性引用释放时，未被其他属性使用的路径/团体也立即退驻留。
   池先退出不使既有 RouteAttributes/子组件引用失效；Shared 的 Eq/Ord 有同一对象快速路径，跨池仍比较规范内容，不把指针数值当作身份。
   三个完整属性用例经生产接收裁决保护重排/重复团体复用、不同 MED 的子组件共享、反射顺序、Partial/缺失值区分、未知属性保留、撤回不驻留及嵌套释放/池退出。

## 邻居接收表

AdjRibIn 是每个邻居的原始双栈 unicast 候选 owner；其 actor 绑定会话/邻居/路由域身份，本表不重复保存需要同步的会话参数。
1. IPv4/IPv6 分别用紧凑整数地址加长度作 key，通过现有可失败 AVL 动态精确增删；不把冷构建 LPM 索引用于每条宣告重建，也不按 UPDATE 复制整张接收表。
   每条 ReceivedRoute 只持共享 RouteAttributes 引用与自己的 NextHop。Adj-RIB-In 保留协议接纳的原始候选，不等于通过入口政策、AS/反射去环或可达性准入；这些须进入后续 Loc-RIB 决策。
2. apply 只收同一邻居的一份完整 ReceivedUpdate：先收集报文增量、按规范前缀去重，再捕获一次属性并准备全部新节点和结果向量；所有可失败操作在任何旧路由修改前完成。
   同报文宣告覆盖显式撤回；重复前缀按 received::changes 最后一项决定。RFC 4760 对跨 MP/基础区间的同族重复行为未定义，本实现保持同一确定性顺序，不据此宣称该输入有规范指定的优先级。
   无变化的最终属性/下一跳及不存在的撤回不进入结果；结果按前缀排序去重，只为实际变化分配容量，供后续精确触发选路，不给重复/未知前缀发送伪变更。
3. 提交只有预分配节点填入、替换及删除，独占借用/同步流程内无用户回调，不再分配；OOM 保留原表和旧属性引用，未发表候选自动释放。
   调用者不得静默跳过失败批次继续接收并保留陈旧路由，必须由后续 actor 执行有界背压或会话终止义务。仅撤回/EoR 不驻留属性，TreatAsWithdraw 真实删除可辨认前缀的既有宣告。
4. retire 仅移动两个表的所有权，当前邻居立即变空，不分配或复制逐路由数组；返回旧表供全局选路/来源撤回遍历，属性随最后一个实际引用释放。
   清空本表不能代替 Loc-RIB/来源撤回；原生服务尚未装配。get 是规范前缀精确查询，iter 不复制共享属性；两族 key 和 Prefix::Ord 只表示身份，不是选路优先级。

三个接收表用例沿生产 OPEN/Session/接收裁决进入 AdjRibIn，保护双栈批次、重复/无变化处理、隐式替换、同报文撤回/宣告、按撤回处理、邻居隔离和退出后的共享引用生命周期。
OPEN/UPDATE 用例共用 bgp_peer 夹具，仅复用既有握手/输入路径，不在生产代码插入测试入口；OOM 原子性由全部分配前置及现有 FallibleMap prepare/commit 语义审查，不宣称已完成故障注入验收。
Loc-RIB 的原生来源发表、Adj-RIB-Out、入口/出口政策、反射传播、完整 UPDATE 输出及百万前缀规模验收仍未接入；host 接收表验证不等于完整 BGP 服务可用。

## 最佳路径选择核心

selection 拥有 RFC 4271 §9.1.2.2 / RFC 4456 的纯选择过程，不拥有政策、下一跳解析结果、Loc-RIB 或发送状态。
SelectionContext 只捕获有效本地 AS/Identifier 与可选有效 RR cluster ID；None 表示不是 RR，配置 owner 负责投影缺省 cluster ID，不在选择器内另存配置。
1. Candidate 必须属于同一前缀/路由域且每个稳定邻居身份至多一条；它借用不可变 Peer/RouteAttributes，并携后政策逻辑下一跳、Phase 1 degree 与同一目标的权威可达性结果。
   degree 越大越优，None 为政策拒绝、Some(0) 仍可用，不能直接拿统一 RIB 的管理距离填入。PeerKind 由配置/会话关系给出，MED 的相邻 AS 则按实际 AS_PATH 确定，不能用 transport peer ASN 替代。
   Reachability 区分不可解析与可解析但无 IGP 代价；Resolved 必须已覆盖“安装本路由后仍可解析”的检查，旧 FIB Forward/随意 Some(0) 不构成准入证明。
2. 首先排除无效邻居、政策拒绝、不可解析、本地 AS 出现在完整有效路径（含集合）的路由，以及本机 ORIGINATOR_ID/本 RR cluster 循环；原始 Adj-RIB-In 不被删除。
   再按最高 degree、最短路径、最低 ORIGIN 消除；AS_SET 一跳，联盟段不占跳数。MED 只在前序幸存者中按相邻 AS 分组，各组保留最小值，缺失为零。
   路径首个普通序列的第一 AS 给出组；跳过联盟段，iBGP 的空路径/前导 AS_SET 按本地 AS 处理，外部空/集合路径用直接邻居 AS 兜底。
3. MED 后先优先 eBGP，再比较下一跳 IGP 代价；若此时任一仍可达的幸存者无代价，则跳过整个代价步骤，不能伪造零/无穷大，也不能被早已淘汰的候选影响。
   最后按最低有效 BGP Identifier（ORIGINATOR_ID 优先）、最短 CLUSTER_LIST、最低邻居地址消除；协议身份完全相同时以域内稳定邻居 ID 作本地确定性消歧，不使用指针/PID。
   MED 条件比较不满足传递性，所以没有把它塞进排序 Ord/两两 fold；只对 (相邻 AS, MED) 这样的全序分组 key 排序。输入顺序不改变同一完整候选集的赢家，但不由此声称能消除网络级 MED/RR 振荡。
4. Selector 只复用派生标量工作区的容量，进入/返回清空元素，不保活候选引用或缓存赢家；选择不克隆属性、增加逐前缀驻留对象或修改候选/接收表。
   工作区扩容 OOM 直接返回错误，无旧赢家回退。输入满足前提时返回原 slice 的唯一最优索引，全部被拒绝返回 None；rejection 提供相同门槛的首个原因供后续诊断。

五个选择用例用生产 codec/接收/驻留准备属性，受控候选覆盖 MED 循环胜负的所有排列、工作区复用后重算、阶段优先级、集合/空路径/联盟组、未知代价、四字节 AS/反射去环及反射平局顺序。
既有邻居退出用例同时把真实 Session/Adj-RIB-In 的属性与 OPEN 邻居投影交给选择器，检查首选到备用到无候选的重算；政策/可达性仍为受控输入，不虚构原生 NHT 成功。
Phase 1 政策、真实 NHT/安装后递归检查、Loc-RIB 的原生来源发布及 RR client/non-client 出口传播仍未装配；这些 host 结果不替代实际协议和路由服务验收。

## Loc-RIB 持有与发表边界

LocRib 拥有一个 BGP 路由域的当前协议选择；SelectedRoute::select 消费纯选择核心结果，持有共享属性、邻居稳定 ID、后政策逻辑下一跳与实际 degree。
1. 每前缀不复制完整 Peer/配置/NHT 状态，peer ID 的退出与重算次序仍由注册 actor 负责。degree 与管理距离分开，生成 iBGP LOCAL_PREF 时必须用已计算的 degree，不能误取旧输入属性。
   接收表退出不释放 Loc-RIB 持有的属性；原始候选与当前选择有不同职责，不能把两表当作人工同步的同一份状态。立即出口/递归图仍归统一 routing-rib，本表不另存 FIB。
2. prepare 从可失败迭代器捕获当前批次的已选择结果，调用者可逐前缀复用候选/Selector 工作区；不收集全表候选矩阵或复制当前 Loc-RIB。
   同前缀最后一项生效，None 撤回，最终相同的邻居 ID/属性/下一跳/degree 不产生伪变更；先按规范前缀排序去重，再只为真正新增的节点和变化前缀向量预分配。
   非规范前缀、错误地址族、未支持的跨族下一跳和上游选择/分配失败不修改旧表；所有节点/结果容量均在外部发布前准备好。
3. 当前授权来源接口逐地址族完整替换，因此每个 LocRibUpdate 也只属于一个地址族，两族分别确认；不能先后发布两族后把一个家族的失败当成双族整体回滚。
   guard 独占借用当前 LocRib，形成不靠 generation/pending flag 同步的唯一租约。iter 合并该族旧表与已准备增量给出完整冻结候选视图，不复制逐路由 DTO，也不扫描另一族的表。
   调用者必须经现有统一 RIB 来源通道发表该视图；确认成功后 commit 消费 guard，只填入已预留节点、替换/删除并返回实际变化前缀，不分配、不发 RPC/调用用户回调。
   取消/发表失败丢弃 guard 只释放候选，旧表仍在；发表成功后不得省略本地 commit。must_use 防止静默丢弃准备结果，但不伪造外部确认能力。
4. 当前只提供协议状态/候选视图，不自动调用 SourceService、撤回来源或写 FIB。原生 actor 须按来源授权生成管理距离/metric/下一跳字典，不能把 BGP degree 直接写入管理距离。
   新 Loc-RIB 用例沿实际接收表/选择核心取得结果，覆盖双族独立视图、批次提交/取消、无变化、最后决定、错误族/前缀/上游失败和接收表退出后的强引用/撤回释放。
   测试中的政策/NHT/发布确认仍为受控边界；不声明已完成原生发布、内存故障注入或双栈真实转发。

## 成本与验证

接收/发送视图只借用调用者输入；迭代重建一个定长地址/段视图，不收集完整前缀/AS/属性数组；明确调用 PathPool::intern 后才按不同规范路径拥有 AS 数据。
编码先检查本层负责的尺寸/字段/容量，失败不改输出；随后只写共同头和最终区间，UPDATE 每段复制一次，无隐藏中间完整帧。
已实现的属性/路径/团体驻留模型须由 BGP 服务持有；流接收缓冲、发送背压和路由表仍待装配。packet-wire 不保留跨消息缓存或恢复状态。
Adj-RIB-In 的结构预备为 O(P log P + P log R)，另加属性驻留/释放成本；P 为当前消息前缀数、R 为该邻居现有路由数。暂存只随当前消息变化，退表移动无分配，旧表释放按实际路由数执行；百万前缀内存和吞吐仍须随服务全表加载测量。
最佳路径工作区与单个前缀的候选数 K 成正比，不随总前缀数复制数组；除路径检查外为 O(K log K) 分组排序/线性消除，已有容量足够时不分配。复用容量仅在 Selector 生命周期内保留，不承载权威状态。
Loc-RIB 准备为批次 B 的 O(B log B + B log L)，另加引用释放成本；L 为当前记录数。候选视图单次为 O(L_family+B)，只合并所属地址族，不建立另一份完整 DTO；当前节点/逻辑下一跳的百万前缀内存仍须随完整服务实测，不能宣称已经最优。

benchmark 决策：新增工作为控制面消息/属性/NLRI 的线性边界检查，无报文转发热路径改变。
驻留增加路径集合的有界排序、团体集合的 O(C log C) 排序和 O(log U) 内容索引；分配只在跨消息捕获时发生，不在逐前缀引用/克隆或路径发送中分配。规模成本须随真实 RIB 全表加载一起测量，当前不虚设独立阈值或宣称最优。
将来服务接入时，完整更新捕获、属性共享、选择工作区、全表加载/传播须一起纳入既有规模基准，当前不新增没有真实消费者的吞吐门限。
`netstack-unit/tests/bgp_formats.rs` 直接调用生产 codec，保护拆包/粘包、普通/扩展长度、双栈所有前缀边界、两种 AS 宽度、
独立属性失败、有效 MP 前缀后截断、输出失败不变及有限嵌套变异；现有 IP/UDP fixture 不覆盖这些新字节边界。
会话夹具直接用同一生产 OPEN codec 驱动 Session，检查握手、真实发送计时、零 hold/被动模式、超时撤回义务、方向授权退休、错误数据及碰撞决定。
状态转换为 O(1)，OPEN/UPDATE 的字段检查线性于输入；属性编号集合固定 32 bytes，不按前缀/属性建堆节点。
UPDATE 冻结视图只持固定四个 NLRI 区间及借用元数据；Effects 的两槽成本有界、不随前缀条数增加，因此此 enum 使用最窄的 large_enum_variant expect，避免为每消息装箱分配。
接收完整检查与后续属性驻留是不同阶段；不为减少一次材料化遍历而在本层创建持久属性副本。诊断编号与前缀按需迭代，完整报文不复制。
十一个 UPDATE 用例经真实 Session 保护错误裁决、两族下一跳/EoR、AS4 重建/聚合器覆盖，以及 OLD/NEW 发送组合、接收回环还原、容量失败不写及联盟迁移拒绝；既有格式/会话用例同步通过。
传输队列、RIB 副作用和全表工作尚未接入，届时一起测量包含事件视图复制的完整成本；host 结果不等于完整 fuzz、FRR/BIRD 互通或百万前缀验收。

## 尚未接入的行为

协议服务/监督、TCP 读写/背压、实际通知/诊断日志/来源撤回、双连接碰撞协调及能力拒绝后的标准重协商仍未装配；接收裁决不等于完整 RFC 7606 运行时诊断与恢复验收。
Adj-RIB-Out、选择/Loc-RIB 与政策/NHT 的实际连通、反射传播、RIB 跟踪/来源发布、配置和管理观测，以及原生 actor 装配仍属于路由计划。
不得先宣告 Extended Message 等能力再补它要求的错误处理；当前没有服务会使用或对外宣告这些能力。
跨重启恢复依计划 R12 留到二期；本期正常会话关闭撤回、配置持久化和全部所需协议功能仍须实现。
