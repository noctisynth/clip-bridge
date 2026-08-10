# Clip Bridge 技术设计

## 1. 文档状态与治理

本文定义 Clip Bridge 的目标需求、架构、协议语义、兼容范围、失败策略和测试标准，是项目的
技术事实来源。

当前 `src/` 实现早于本文形成，只能作为现状和迁移输入，不能反向成为新设计的规范。当前实现
已经能够在部分环境中完成文本转发，但模块职责、状态所有权、并发模型、错误处理和协议状态机
都不满足本文目标。设计与实现之间的差异统一记录在根目录 [TODO.md](../TODO.md)；TODO 不是
独立需求来源。

迁移已经建立 `domain`、backend contract、纯 coordinator reducer、统一 runtime 和原生 X11/Wayland
actor。生产路径使用容量 32 的 backend event channel 与按 backend/selection 划分的 latest-wins
watch mailbox；SIGINT/SIGTERM、actor 失败/panic 和限时关闭已经由 runtime 统一处理。X11 已使用
XFixes、direct/INCR 状态机和 fd poll，Wayland 已实现 ext 优先、wlr fallback、offer/source 生命周期
与固定 worker pool。旧 public backend state 与 legacy adapter 已删除，公开入口已收敛为 `run()`。
默认测试不连接现有 display，日志不输出正文，`RUST_LOG` env filter 与 typed error 已接线。
Xvfb 已验证外部 direct/INCR receive、direct/INCR serving、两种 selection 隔离、读取中的 owner
替换、慢 requester 和断线；KWin virtual 已验证 ext provider 的 Clipboard/Primary offer/source、
source 替换、迟到 cancellation 与 clear。轻量协议 server 已验证 wlr v1/v2 capability、缺少
provider、offer/source 与断线；无显示测试覆盖 stale worker、读写 timeout/cancel、超限、无效
UTF-8 和 worker/channel 容量。Nested Niri 26.04 已通过测试专用 provider 注入验证真实 wlr v2
的 Clipboard/Primary wire；生产路径仍按 ext 优先规则选择 provider。准确状态以 TODO 为准。

技术方案发生变化时，更新顺序必须是：

1. 更新本文，明确新的决定和影响；
2. 更新 `TODO.md`，反映设计与实现的最新差距；
3. 修改代码、配置、测试和用户文档。

已实现且通过必要验证的能力，才可以在 TODO 中标记为完成。尚待原型验证的内容必须在本文中
明确标注，不得以含糊实现代替设计决定。

## 2. 背景与现状诊断

Clip Bridge 运行在同时具有原生 Wayland 和 X11/XWayland 客户端的桌面会话中，使两个协议域的
文本 selection 能够互相读取。Linux 桌面没有一个对所有 compositor 和 X server 都透明的通用
剪贴板存储区；selection 内容由 owner 按需提供，因此桥接程序必须同时扮演：

- 源协议中的 selection 观察者和读取者；
- 目标协议中的 selection owner 和按需数据提供者；
- 两个协议之间的冲突、并发和反馈回环协调器。

当前实现的主要问题如下：

1. 初始 `src/main.rs` 同时负责生命周期、状态缓存、去重和路由，X11/Wayland 两个方向以及
   Clipboard/Primary 两种 selection 的分支几乎完全重复；第一个领域切片已经用 reducer effect
   取代重复路由，但生命周期编排仍暂时位于 `main.rs`。
2. `src/x11.rs` 和 `src/wayland.rs` 同时承担协议对象管理、传输、缓存、channel 通信和错误
   策略，单文件过大且没有可独立测试的状态机边界。
3. 初始跨模块 channel 全部无界；第一切片已把生产 backend event 改为有界 MPSC，把写入命令
   改为 latest-wins watch mailbox，但 pipe worker、completion 与全局并发上限尚未重构。
4. X11 使用固定 sleep 轮询，并在 selection 请求函数内部再次轮询全局事件；这会吞掉无关
   事件、阻塞正常 dispatch，并让多次并发请求无法正确关联。
5. X11 没有完整的 `TARGETS` 协商和 `INCR` 收发状态机；`MULTIPLE`、property、partial
   transfer 等行为也不完整。
6. Wayland 使用反复 `roundtrip` 驱动长期事件循环，registry bind 使用固定版本，存在把高于
   compositor 广告版本的接口版本传给 bind 的风险。
7. Wayland Primary 接收路径被整体注释，另行绑定的 primary-selection manager 没有参与实际
   数据路径；README 对能力的描述因此高于实现。
8. Wayland source 发送只执行一次 `write`，没有保证写完整；接收只按单次 idle timeout 循环，
   没有总大小上限，慢速来源可以长期占用任务并无限扩张缓冲区。
9. 多处协议错误、channel 关闭、flush 和 ownership 设置结果被 `unwrap()` 或 `let _ = ...`
   忽略；初始化错误、单次传输错误和连接断开也没有清晰分类。
10. 初始日志以 `Debug` 输出实际剪贴板内容；第一切片已删除该路由日志并为 `TextPayload`
    提供只显示长度的 Debug，但 README 声称的 `RUST_LOG` 环境过滤契约仍未实现。
11. 初始默认单元测试直接连接真实 X server；第一切片已将其明确标记为等待 Xvfb harness 的
    ignored integration test，并新增无显示领域/reducer 测试，真实 wire 自动化仍为空缺。
12. 没有统一的启动冲突策略、运行期 clear 策略、资源上限、关闭流程和 backend 断线策略。

重设计不以保留上述实现结构为目标。迁移过程中可以复用已经验证有效的协议调用，但必须逐步
收敛到本文定义的状态与边界。

## 3. 目标、范围与非目标

### 3.1 核心目标

- 在 X11/XWayland 与 Wayland 之间双向同步非空纯文本。
- 分别支持 X11 `CLIPBOARD` 与 `PRIMARY`；Wayland Primary 只在所选 data-control provider
  明确支持时启用。
- 使用一个协议无关的纯协调器统一处理启动快照、去重、冲突、顺序和反馈回环。
- 让 X11 与 Wayland adapter 各自独占协议连接及对象，不向领域层暴露 proxy、atom、fd 或
  display event。
- 对 channel、文本大小、传输时间和并发传输数量设置明确边界。
- 正确处理 X11 selection 的异步请求/响应、owner 变化和 `INCR` 大文本传输。
- 正确处理 Wayland offer/source 生命周期、partial write、取消、stale offer 和接口版本协商。
- 使领域逻辑、文本验证和绝大多数状态机可以在没有 X11/Wayland 会话时自动测试。
- 默认不记录剪贴板内容，不在磁盘、网络或历史数据库中持久化文本。
- backend 发生不可恢复错误时可诊断地结束整个桥接进程，不留下失去协调器的半运行后端。

### 3.2 次要目标

- 优先使用 `ext-data-control-v1`，在 compositor 未提供时回退到
  `wlr-data-control-unstable-v1`，扩展目标 Wayland compositor 覆盖面。
- 保持常驻进程内存和 CPU 使用稳定；没有事件时不得依靠无间隔 busy loop 工作。
- 提供足够的结构化日志以定位 backend、selection、revision 和 transfer 阶段，同时不泄漏
  文本内容。
- 保持单 package 结构，避免为一个小型守护进程引入不必要的 Cargo workspace 和跨 crate
  接口。

### 3.3 明确的非目标

- 不同步图片、HTML、RTF、文件 URI、拖放数据、应用私有 MIME 或任意二进制数据。
- 不保留源端 MIME、字符集标签或富文本格式；内部唯一内容模型是 UTF-8 纯文本。
- 不实现剪贴板历史、搜索、固定、持久化、跨设备同步或网络服务。
- 不提供加密、访问控制或应用级读取授权；selection 的访问边界仍由 X server、Wayland
  compositor 和桌面会话决定。
- 不保证进程内文本能够安全清零。Rust `String`/`Arc<str>`、协议库和显示服务器都可能复制
  数据；本项目只保证不主动持久化或记录内容。
- 首版不在 backend 断线后自动重连。连接断开触发协调关闭；稳定重连需要单独设计 generation
  重建和启动冲突语义。
- 不提供动态配置文件、GUI、托盘图标或运行时 MIME 策略。资源限制先使用有测试覆盖的代码
  常量。
- 不承诺当前公开的 `X11State`、`WaylandState`、`SyncEvent` 等内部形态保持 API 兼容；目标
  crate 的公开入口收敛为最小运行 API。

### 3.4 支持契约

目标实现只接受满足以下条件的 payload：

- 解码后是有效 UTF-8；
- 字节长度大于 0 且不超过 16 MiB；
- 内容按原始 Unicode scalar/UTF-8 字节语义传递，不执行 Unicode normalization、换行转换、
  首尾空白裁剪或 NUL 替换。

超过上限、解码失败、空文本或只提供不支持 MIME/target 的 selection 不会覆盖另一端，只记录
不含内容的可诊断事件。16 MiB 是首版安全上限，不是协议极限；调整上限属于资源与兼容性变更，
必须同步更新本文和边界测试。

## 4. 系统边界与目标架构

```text
                         clip-bridge process

  X server                                                  Wayland compositor
      │                                                              │
      ▼                                                              ▼
┌──────────────┐      BackendEvent       ┌────────────────┐      ┌────────────────┐
│  X11 actor   │ ──────────────────────► │  Coordinator   │ ◄─── │ Wayland actor  │
│ connection   │                         │ pure reducer + │      │ connection +   │
│ owner/read   │ ◄── latest command ──── │ selection state│ ───► │ offer/source   │
└──────┬───────┘                         └───────┬────────┘      └───────┬────────┘
       │                                         │                       │
       └──── bounded transfer workers ───────────┴───────────────────────┘
                                                 │
                                                 ▼
                                        Runtime / shutdown owner
```

系统由四类组件组成：

1. **Domain**：定义 backend、selection、文本、revision、capability、事件、命令和错误分类；不
   依赖 X11、Wayland、Tokio I/O 或日志实现。
2. **Coordinator**：每次接收一个领域事件，通过纯 reducer 更新状态并产生 effect；不直接执行
   protocol I/O。
3. **Backend actors**：X11 和 Wayland 各自由一个长期 actor 独占连接、协议对象和本地 ownership
   状态；把协议事件转换为领域事件，并消费协调器产生的命令。
4. **Runtime**：初始化日志和 backend、转发 reducer effect、管理 worker 与 join handle、响应
   信号，并决定进程退出状态。

两个 backend 不能直接互相调用，也不能读取对方的状态。所有跨边界信息都必须通过领域消息，
从而允许 coordinator 用 fake backend 完整测试。

## 5. 领域模型与协调语义

### 5.1 核心类型

目标领域模型至少包含：

```rust
enum BackendId {
    X11,
    Wayland,
}

enum SelectionKind {
    Clipboard,
    Primary,
}

struct Revision(u64);
struct BackendEpoch(u64);
struct OfferToken(u64);
struct CommandId(u64);

struct TextPayload {
    text: Arc<str>,
}

struct BackendCapabilities {
    clipboard: bool,
    primary: bool,
}
```

`TextPayload` 只能通过验证构造器创建，构造器负责 UTF-8、非空和 16 MiB 上限。它不得派生会
输出正文的 `Debug`；诊断视图只包含字节长度。领域层用完整文本相等性判断重复，不用有碰撞
风险的 hash 代替正确性判断。

`Revision` 由 coordinator 按 selection 单调递增，表示已经接受的跨后端文本变化；
`BackendEpoch` 由 backend 在每次观察到 selection identity/owner 变化时按 selection 递增，用于
阻止尚未执行的旧命令覆盖新 owner；`OfferToken` 由 backend 为每次异步读取分配，用于识别读取
结果是否过期；`CommandId` 标识一次对 backend 的 edge-triggered 命令。四者语义不同，不得
互相复用。

### 5.2 Backend 事件

协议 adapter 向 coordinator 报告以下领域事实：

- `Ready { backend, capabilities }`：初始化完成并声明实际能力；
- `SelectionChanged { backend, selection, epoch }`：adapter 一观察到 selection identity/owner
  变化就立即报告的新代次，不等待文本读取完成；
- `InitialSnapshot { backend, selection, epoch, token, outcome }`：启动时观察到的现有 selection，
  outcome 为 `Text`、`Empty`、`Unsupported` 或 `Failed`；
- `ObservedText { backend, selection, epoch, token, payload }`：当前 selection 已被读取并通过
  验证；它可能来自外部 owner，也可能是 Wayland 对本进程 source 的不可判定回声；
- `SelectionUnavailable { backend, selection, epoch, token, reason }`：selection 被清空、格式
  不支持、过大或本次读取失败；
- `OwnershipApplied { backend, selection, command_id, revision }`：backend 已成功建立并 flush
  本地 ownership/source；
- `OwnershipFailed { backend, selection, command_id, revision, error }`：目标命令未能建立
  ownership；只有匹配当前 pending 的失败才能清理 pending；
- `OwnershipLost { backend, selection, revision }`：先前由桥接进程持有的 selection 被外部
  owner 替换或 source 被取消；
- `RecoverableError { backend, selection?, stage, error }`：单次 transfer 或请求失败；
- `FatalError { backend, error }`：连接、必需 global 或 actor 状态已经无法继续。

X11 可以用 owner window 证明通知来自自身，因此应在 adapter 内抑制读取。Wayland offer 与
本地 source 之间没有可依赖的对象身份关联，adapter 不得仅凭 proxy 顺序或文本相同宣称它一定
是自身回声；无法证明时按普通 `ObservedText` 上报，由 coordinator 根据目标端 observed/owned
状态去重。异步读取完成时必须同时比较 `BackendEpoch` 与 `OfferToken`；旧结果直接丢弃，不进入
领域层。

### 5.3 Backend 命令

Coordinator 只向 backend 发出：

- `SetText { command_id, selection, revision, expected_target_epoch, payload }`；
- `Shutdown`。

不提供跨后端 `ClearSelection` 命令。selection clear 的安全语义见 5.6 节。

命令是 edge-triggered，而不是“只要 desired state 存在就不断重新夺回 ownership”的声明式
命令。Backend 对每个 `CommandId` 最多处理一次；外部应用取得 ownership 后，不得因为 watch
中仍保留旧值而自动重新抢回 selection。

Backend 只有在自己的当前 `BackendEpoch` 等于 `expected_target_epoch` 时才执行 `SetText`。
如果等待 mailbox 期间 selection 已变化，命令以 stale target 结果拒绝；不能先夺取新 owner，
再由 coordinator 事后修正。Coordinator 收到新的 `SelectionChanged` 后清除指向该旧 epoch 的
pending，不盲目重试，等待新 selection 的读取结果决定下一步。

`SelectionChanged` 还必须立即把该 backend 的 `observed` 设为未知，因为旧文本只描述旧 owner；
在新读取完成前不能用它参与目标端去重。它不会单独清除 `owned`：X11 的 owner window、Wayland
source cancelled 或命令结果才是本地 ownership 是否仍成立的证据。

### 5.4 每个 selection 的状态

`Clipboard` 与 `Primary` 各自持有完全独立的 `SelectionState`：

```text
revision
epoch[x11|wayland]
initial[x11|wayland]
observed[x11|wayland]
owned[x11|wayland]
pending[x11|wayland]
startup_phase
```

- `observed` 保存最近一次成功接受的外部或初始文本；
- `owned` 只在相应 backend 确认 ownership 后更新，并包含 revision 与 payload；
- `pending` 保存尚未得到成功/失败结果的最新命令；旧 command result 不改变新状态；
- 任一 selection 的事件不得读写另一 selection 的缓存。

Reducer 对相同输入状态和事件必须产生相同的新状态与 effect，不能读取时钟、环境变量或协议
对象。日志、watch 更新和关闭动作由 runtime 执行 effect。

### 5.5 启动快照与冲突

两个 backend Ready 后，各自对实际支持的 selection 提供一次 `InitialSnapshot`。Coordinator
按 selection 使用以下确定性策略：

1. 两端都是相同文本：记录为已观察内容，不发送命令；
2. 只有一端有有效文本，另一端明确为 `Empty` 或 `Unsupported`：把有效文本设置到另一端；
3. 两端都有有效但不同的文本：不覆盖任一端，记录一次 startup conflict，等待启动完成后的
   第一次外部 ownership 变化决定新内容；
4. 任一端 snapshot 为 `Failed` 或在启动超时内未知：不进行启动覆盖，保留可诊断警告；
5. 某端不支持 Primary：该 selection 不在这两个 backend 之间启动同步。

这样既不会无条件把 Wayland 或 X11 当作权威端，也能在只有一侧已有文本时完成有用的初始
同步。启动快照只使用一次；之后完全按运行期事件顺序处理。

### 5.6 运行期同步、重复与 clear

接受一个当前 epoch/token 的 `ObservedText` 后，reducer：

1. 验证 epoch/token 已由 adapter 判定为当前 selection/offer，并更新来源端 `observed`；
2. 如果目标端不支持该 selection，停止；
3. 如果目标端 `observed`、`owned` 或相同 payload 的当前 `pending` 与其完全相同，视为重复并
   停止；这也
   安全消解无法由 Wayland wire 身份证明的本地 source 回声；
4. 否则递增该 selection 的 `Revision`，使用目标端最新 `BackendEpoch` 生成新的 `SetText`
   命令并替换目标端旧 pending；
5. 目标 backend 只处理最新可见 command；旧命令的 ack、错误或 worker 完成不会覆盖新 revision。

外部 owner 即使提供与上次相同的文本，也代表新的 ownership 事实。Coordinator 可以在目标端
已经 observed/owned 同一文本时跳过写入，但不能仅凭来源端文本相同就忽略 ownership 变化。

运行期 `SelectionUnavailable`、selection clear 和空文本只更新该来源端的 observed/ownership
事实，不跨后端清空 selection。原因是 X11 与 Wayland 都可能在 owner 交接中短暂报告 `None`，
把 clear 传播到另一端会造成数据丢失和递归清空。将来若要同步显式清空，必须先设计可区分
“用户清空”和“owner 交接”的协议证据，不能直接转发 `None`。

### 5.7 顺序与并发

- Coordinator 串行处理领域事件，事件被接受的顺序就是跨 backend 的冲突顺序；不使用两个
  display server 之间不可比较的时间戳决定先后。
- Backend 在读取文本前先上报 `SelectionChanged` 并递增 epoch；因此已经在 mailbox 等待、但
  尚未执行且引用旧 epoch 的命令必须被拒绝，不会覆盖刚发生的外部复制。
- 每个 backend/selection 同时最多存在一个有效读取 transfer；新 offer 到达后取消或废弃旧
  transfer。
- 每个 backend/selection 同时最多存在一个需要建立 ownership 的最新命令；尚未处理的旧命令
  可以被新命令合并替换。
- 已经开始向外部 requester 写数据的 source 使用创建时捕获的不可变 `Arc<str>` 快照，不因
  后续 selection 变化而在一次传输中切换内容。
- revision 溢出视为不可恢复的内部错误并触发协调关闭；不得回绕后继续比较。

## 6. 并发、背压与生命周期

### 6.1 执行模型

- Tokio runtime 运行 coordinator、信号监听和有限的 pipe transfer worker；
- X11 actor 在一个专用 blocking task/OS thread 中独占 X11 connection；
- Wayland actor 在另一个专用 blocking task/OS thread 中独占 Wayland connection、event queue
  和所有 proxy；
- 协议 proxy 和 X11 connection 不跨 actor 共享，不用 `Arc<Mutex<ProtocolState>>` 把所有状态
  暴露给任意任务。

两个 actor 都通过底层 fd 的 poll/prepare-read/dispatch 能力等待事件，并设置有限 poll timeout
以检查命令和 shutdown。初始化完成后不得用无限 `roundtrip` 循环或固定 10 ms sleep 充当事件
驱动。

### 6.2 Channel 与 mailbox

- Backend → coordinator 使用容量有限的 MPSC 事件 channel；coordinator 的 reducer/effect
  分发不得在消费循环中等待 backend command queue，从而保证 actor 的 `blocking_send` 能持续
  被排空。
- Coordinator → backend 对每个 selection 使用保存“最新命令 + CommandId”的 watch/mailbox；
  runtime 用非阻塞 `send_replace` 发布，actor 记录已处理 CommandId，因此 mailbox 保留旧值
  不会导致重新夺取 ownership。
- 高频变化允许在命令真正执行前合并掉旧的目标命令，但已经报告给 coordinator 的外部事件
  不得静默丢失。事件 channel 永久堵塞或接收端关闭时，actor 结束并触发整体 shutdown。
- Worker completion 使用小容量内部 channel 返回所属 actor；队列满时以 transfer token 为依据
  合并/丢弃已过期完成结果，不能创建无界 completion 队列。

容量使用私有常量并由边界测试锁定：backend event channel 为 32；Wayland 使用 4 个固定 worker、
容量 8 的 job queue 和容量 12 的 completion channel，后者可容纳全部活动与已排队 job 的完成
结果。10,000 次 command burst 测试证明 mailbox 只保留最新命令；worker 饱和测试证明第 13 个
同时存在的 transfer 不会继续增长队列。两个 actor 的协议 poll timeout 均为 100 ms。调整这些
容量不改变领域语义，但必须同步压力与关闭响应测试。

### 6.3 Transfer 资源边界

- 单 payload 最大 16 MiB；读取超过上限立即终止并丢弃整份内容，绝不截断后传播；
- 单次 transfer idle timeout 为 5 秒，总 timeout 为 30 秒；
- 每个 backend/selection 最多一个有效读取 transfer；固定 4-worker pool 作为全局并发 permit，
  再以容量 8 的 job queue 防止恶意客户端无限创建并发读写任务；
- 所有 write 必须循环到完整写入或明确失败，正确处理 partial write、`EINTR` 和可等待的
  `EAGAIN`；
- pipe worker 在读写前把 fd 设为 nonblocking，使 5 秒 idle 与 30 秒 total timeout 不会被单次
  blocking syscall 绕过；
- timeout、过大、取消和 fd 错误都关闭 fd、释放 buffer，并返回不含正文的错误。

### 6.4 启动与关闭

启动步骤为：

1. 初始化 tracing filter；
2. 创建 runtime、领域状态和有界通信原语；
3. 启动 X11 与 Wayland actor；
4. 等待两个 backend Ready，校验强制 capability；
5. 收集有界时间内的 initial snapshot；
6. 进入正常同步。

X11 connection、Wayland connection 或 Wayland Clipboard data-control provider 缺失时启动失败并
返回非零。Primary capability 缺失只禁用 Primary，并记录一次 warning。

收到 SIGINT/SIGTERM、任一 actor `FatalError`、coordinator 不变量错误或 join panic 时，runtime
发布一次 shutdown，停止接受新 transfer，通知两个 actor，等待所有 handle 在有限时间内退出，
然后按根因返回成功或失败。首版不在 shutdown 时主动清空 selection；连接销毁后 display server
自然撤销本进程 ownership。

## 7. X11 Adapter 设计

### 7.1 初始化与能力

X11 actor：

- 建立一个 connection 和一个专用 owner window；
- 查询并验证 XFixes extension；
- 一次性 intern 所有固定 atom 和每个 selection 的 transfer property；
- 对 `CLIPBOARD` 与 `PRIMARY` 注册 XFixes owner 通知；
- 保存 typed `Atoms`，构造失败立即返回错误，后续不通过字符串 HashMap + `unwrap()` 查找；
- 使用 server/event timestamp 建立 ownership，只有协议没有可用时间时才按明确策略使用
  `CURRENT_TIME`。

X11 对 Clipboard 和 Primary 都声明 capability。初始化 snapshot 查询当前 owner：无 owner 产生
`Empty`；bridge 自己不是启动 owner；有外部 owner 则进入正常读取状态机，但结果标记为 initial。

### 7.2 外部 selection 读取状态机

每个 selection 拥有独立状态：

```text
Idle
  └─ owner changed ─► RequestingTargets
                         ├─ no supported target ─► Idle/Unsupported
                         └─ choose target ───────► RequestingData
                                                     ├─ direct property ─► Complete
                                                     └─ INCR ────────────► ReceivingChunks
                                                                                └─ Complete
```

关键规则：

- 每次 owner 变化分配新 `OfferToken`；旧 owner 的 `SelectionNotify`、`PropertyNotify` 和 timeout
  不能完成新 transfer；
- 先请求 `TARGETS`，再按目标优先级选择一种格式；不能在一个函数里 sleep 并连续发多个
  `ConvertSelection` 猜测格式；
- `SelectionNotify` 必须根据 requestor、selection、target、property 和 transfer token 对应到
  唯一状态，不匹配事件交回主 dispatch；
- direct property 与 INCR chunk 共用一个有上限的 buffer assembler；完成后按选定 target
  解码并构造 `TextPayload`；
- owner 再次变化、property 被替换、timeout、超限或解码失败时终止旧 transfer；
- 任何等待都由 actor poll deadline 驱动，不在 event handler 内阻塞或嵌套消费全局事件。

读取 target 优先级为：

1. `UTF8_STRING`；
2. `text/plain;charset=utf-8`；
3. `text/plain`（按 UTF-8 验证，不根据 locale 猜测）；
4. `TEXT`，但只有返回 property type 能明确归入 UTF-8 或 `STRING` 时接受；
5. `STRING`，按 ISO-8859-1 映射为 Unicode 后转成内部 UTF-8。

不支持 `COMPOUND_TEXT` 和其他 locale-dependent 编码。读取结果不会保留原 target 信息。

### 7.3 作为 selection owner

处理 `SetText` 时，X11 actor 保存不可变 `OwnedSelection { revision, payload, timestamp }`，请求
selection ownership，flush，并通过 owner 查询/相应 XFixes 事件确认成功后发送
`OwnershipApplied`。如果外部 owner 抢占，清理对应 owned state 并发送 `OwnershipLost`；不会
自动重新夺回。

响应 `SelectionRequest` 至少支持：

- `TARGETS`：返回实际可提供的 targets；
- `TIMESTAMP`：返回本次取得 ownership 的 timestamp；
- `UTF8_STRING`、`text/plain;charset=utf-8`、`text/plain`：返回完整 UTF-8；
- `TEXT`：以明确的 UTF-8 property type 返回；
- `STRING`：仅当所有字符都可无损映射为 ISO-8859-1 时返回，否则以 property `NONE` 拒绝；
- `MULTIPLE`：首个迁移版本可以明确拒绝；如果实现，必须逐 pair 返回真实成功/失败，不能把
  所有 property 写成空值伪装成功。

当 request property 为 `NONE` 时按 ICCCM 规则使用 target 作为 property。小 payload 直接写入；
超过一次安全 property 大小的 payload 使用完整 INCR send 状态机，根据 requestor 的 property
delete 通知逐块发送。每个 requester transfer 捕获当时 payload 快照，并受总大小、并发数与
timeout 约束。

### 7.4 X11 反馈回环

- XFixes 通知中的 owner 等于 bridge window 时只用于确认 pending ownership，不触发读取；
- `SelectionClear` 只表示本进程失去 ownership，不向 Wayland 传播 clear；
- 外部 owner 变化即使文本与上次相同也生成新的 offer token并完成读取，最终是否需要写入目标
  由 coordinator 判断；
- 不以临时 property 名称、缓存文本或固定 sleep 猜测请求归属。

## 8. Wayland Adapter 设计

### 8.1 Data-control provider 选择

Wayland adapter 只使用能够在没有 surface/keyboard focus 的情况下观察和设置 selection 的
data-control 协议。启动时按以下顺序选择一个 provider：

1. `ext-data-control-v1`；
2. `wlr-data-control-unstable-v1`。

如果两者都存在，只绑定优先级更高者，避免同一 selection 收到两套重复事件。没有 provider
时启动失败。Provider 在 adapter 内通过 enum/窄接口统一 offer、device 和 source 语义，不向
coordinator 暴露具体 proxy 类型。

所有 registry bind 使用 `min(advertised_version, client_supported_version)`，并在发出带
`since` 要求的 request 前检查实际绑定版本：

- Clipboard 是必需能力；
- ext data-control 提供 Primary 时启用；
- wlr data-control 只有绑定版本至少为 2 时启用 Primary；版本 1 只运行 Clipboard；
- 不再绑定与所选 provider 数据路径无关的 `zwp_primary_selection_device_manager_v1`；
- 不创建没有实际用途的 compositor/surface 对象。

当前只桥接默认 seat。多 seat 支持需要定义 selection 到 seat 的公开映射，不在首版范围内；
发现多个 seat 时选择第一个完成 capability 的 seat并记录 seat name，不能悄悄在运行期切换。

### 8.2 Offer 与读取

Adapter 为每个 data offer 保存 provider object id、累计 MIME 集合和生命周期。收到 selection
事件后：

1. 先递增该 selection 的 `BackendEpoch` 并立即发送 `SelectionChanged`，使引用旧 epoch 的待
   处理命令在读取完成前就失效；
2. selection 为 `None` 时分配新 token、取消旧读取并报告 `SelectionUnavailable::Cleared`；
3. selection 指向 offer 时分配新 token，完成/读取该 offer 已广告的 MIME；
4. 按优先级选择 `text/plain;charset=utf-8`、带大小写无关 `charset=utf-8` 参数的
   `text/plain`、无 charset 的 `text/plain`；无 charset 内容仍必须通过 UTF-8 验证；
5. 不请求 `UTF8_STRING`、`STRING` 等 X11 atom 名称作为 Wayland MIME；
6. 创建 pipe，把 write fd 交给 `offer.receive`，flush request，关闭本地 write end；
7. read fd 移交受限 worker，按 5 秒 idle、30 秒 total 和 16 MiB 上限读取；
8. worker 结果回到 actor后再次比较 epoch 与 offer token，只有当前结果才能形成 snapshot 或
   `ObservedText`。

Offer destroyed、selection 替换、timeout 或 shutdown 都使对应 token 失效。异步 worker 不持有
Wayland proxy，只持有 `OwnedFd`、token、selection 和必要的不可变元数据。

### 8.3 Source 与写入

处理 `SetText` 时：

- 为该 command 创建一次性 data source；同一个 source 不能用于两次 set request；
- 只广告 `text/plain;charset=utf-8` 和 `text/plain`；两者都发送相同 UTF-8 字节；
- 先创建并设置新 source，成功 flush 后再替换 actor 内旧 source 记录；
- source 记录包含 command id、revision、selection 和 `Arc<str>` payload；
- `send` 事件根据 source object id 查找精确快照，把 fd 交给受限 write worker并循环写完整；
- `cancelled` 只清理匹配的当前 source。旧 source 的迟到 cancelled 不得清除新 ownership；
- request/flush 成功并建立当前 source 后报告 `OwnershipApplied`；协议错误报告 fatal，单个 fd
  写失败报告 recoverable。

Provider 不承诺可以验证 compositor 已经让所有客户端观察到新 source；`OwnershipApplied` 的
语义是本进程已经成功发出并 flush 合法 set request，而不是对方应用已经读取。

### 8.4 Wayland 反馈回环

Adapter 保存当前 source object id 与 revision，但 data-control selection offer 不提供可依赖的
source/offer 身份映射。收到任何 selection identity 变化时先递增并上报 `BackendEpoch`，再读取
新 offer；不能仅凭“这是 set request 后的下一个 offer”抑制事件。读回自身 payload 时，
coordinator 会因为另一端已经 observed 相同文本而去重。source cancelled 只用于确认对应本地
ownership 已丢失；迟到 cancelled 不清除新 source。带旧 epoch 的读取结果和命令都会被拒绝。

Wayland clear 和空文本不向 X11 传播。该规则来自统一领域策略，而不是针对单个应用名称的特殊
条件分支。

## 9. 文本、资源与隐私

### 9.1 Canonical text

领域层只保存 `Arc<str>`，以避免 coordinator、backend command 和并发 source request 为同一
文本制造不必要的完整复制。协议解码在 adapter 边界完成，协议编码也在 adapter 边界完成。

- X11 `STRING` 是唯一允许的非 UTF-8输入，按 ISO-8859-1 无损转换；
- Wayland 无 charset 的 `text/plain` 不使用 locale/iconv 猜测，只接受有效 UTF-8；
- 输出不会恢复原始编码或 MIME；
- 无效输入、超限输入和空输入都不会更新另一端。

### 9.2 内存与日志

- 文本只驻留于当前 observed、owned、pending 和活动 transfer 快照；新状态替换旧状态后及时
  drop 不再需要的引用；
- 不写临时文件、缓存文件、数据库、crash dump 附件或 telemetry；
- `Debug`、error、span 和 tracing field 禁止包含正文及正文片段；允许字段为 backend、
  selection、revision、command id、offer token、字节长度、MIME 名称和错误阶段；
- 默认日志级别为 info；通过 `RUST_LOG` 调整 filter，非法 filter 返回清晰启动错误或安全回退
  到 info 并只记录一次 warning；
- panic hook 和顶层错误不得 dump 含 payload 的完整领域状态。

## 10. 错误模型与恢复

错误使用 typed enum 保留 backend 和阶段信息，顶层不再以任意 `String` 作为主要错误契约。
错误分为：

- `StartupError`：环境变量、X11 connection、XFixes、Wayland connection、registry/provider、
  channel 初始化；
- `TransferError`：unsupported、invalid UTF-8、too large、timeout、cancelled、fd read/write、
  stale；
- `ProtocolError`：无效事件顺序、request/reply、bind version、proxy/atom/property；
- `CoordinatorError`：revision 溢出、不可能的 ack、内部状态不变量；
- `ShutdownError`：actor join、超时、重复/失败关闭。

恢复策略：

- 单个外部 selection 读取失败、单个 requester 写失败和 unsupported format 是 recoverable；保留
  另一端内容，等待下一次 owner 变化；
- ownership command 设置失败报告 recoverable 并清理 pending；不把失败目标标记为 owned；
- display connection 断开、Wayland protocol error、X11 connection fatal error、必需 global
  消失和 reducer 不变量错误是 fatal；关闭整个进程；
- channel 接收端消失说明系统组件已经退出，不能以循环 warning 继续半运行；
- 不在 panic、fatal 或 shutdown 路径启动新的后台恢复任务。

生产代码不得对外部状态使用 `unwrap()`。内部不变量使用 `expect()` 时，消息必须说明由哪一
构造/状态分支保证。允许 best-effort cleanup 时必须在控制流或注释中解释失败为何不影响已经
确定的退出结果。

## 11. 模块、公开 API 与依赖

### 11.1 目标源码结构

```text
src/
├── main.rs                  # 日志、默认配置、退出码
├── lib.rs                   # 最小 run API
├── domain.rs                # 领域类型、TextPayload、错误分类
├── coordinator.rs           # 纯 reducer 与 SelectionState
├── runtime.rs               # actor/mailbox/worker/shutdown 编排
└── backend/
    ├── mod.rs               # BackendEvent/BackendCommand 契约
    ├── x11/
    │   ├── mod.rs           # X11 actor
    │   ├── atoms.rs         # typed atoms 与 target 选择
    │   ├── receive.rs       # direct/INCR receive 状态机
    │   └── serve.rs         # SelectionRequest/INCR send 状态机
    └── wayland/
        ├── mod.rs           # Wayland actor 与 registry
        ├── provider.rs      # ext/wlr provider enum
        ├── offer.rs         # MIME 与 receive 生命周期
        └── source.rs        # source snapshot 与 write 生命周期
```

文件划分服务于状态所有权，不要求机械地为每个类型创建文件。`domain` 和 `coordinator` 不依赖
backend 模块；backend 可以依赖领域契约；runtime 是唯一同时构造两端的模块。

### 11.2 公开 API

`main.rs` 只调用 library 的最小入口并把 typed error 映射为退出状态。目标公开 API 为类似：

```rust
pub async fn run() -> Result<(), BridgeError>;
```

Backend state、protocol proxy、atom table、coordinator 内部状态和 mailbox 不作为公共 API。
当前 crate 仍处于 `0.x`，移除既有公开内部类型属于 pre-1.0 breaking change，必须通过 Semifold
minor changeset 明确发布，不能伪装为 patch 重构。

### 11.3 Toolchain 与依赖策略

- Rust Edition 2024，跟随最新 stable，不承诺 README 当前写出的 Rust 1.70；迁移时修正文档，
  是否新增 `rust-toolchain.toml` 由实现切片统一完成；
- 保留 `x11rb`、`wayland-client`、所选 data-control protocol crates、`nix`、`tokio`、
  `tracing` 和 `tracing-subscriber`；
- 使用 `wayland-protocols` 的 `staging` feature 提供 ext data-control，并使用
  `wayland-protocols-wlr` 提供 wlr fallback；删除不包含 ext data-control 且无其他用途的
  `wayland-protocols-misc`，以及只为当前未接线 primary manager 保留的依赖/feature；
- 为 typed error 引入 `thiserror`，为 `tracing-subscriber` 启用 `env-filter`；
- 收窄 `tokio = { features = ["full"] }` 和 `x11rb = { features = ["all-extensions"] }` 为目标
  实际需要的 feature，减少无关编译面；
- 新增、删除、更新依赖和 feature 必须通过 Cargo CLI，并检查 lockfile 差异。

## 12. 测试策略

### 12.1 无显示服务器的默认测试

默认 `cargo test` 必须不连接真实 X11/Wayland。至少覆盖：

- `TextPayload` 的空、UTF-8、16 MiB 边界和不泄漏 `Debug`；
- Clipboard 与 Primary 状态完全独立；
- 启动快照相同、单侧文本、双侧冲突、snapshot failed 和 capability 缺失；
- 外部文本的双向路由、相同文本去重、ownership lost 后同文本重新取得；
- clear/empty/unsupported 不跨后端清空；
- stale offer、stale command ack 和旧 revision 不改变当前状态；
- 快速连续事件只产生最新 backend command，同时保留 reducer 的确定性；
- revision 溢出和不可能状态返回 typed error；
- X11 target 选择、Latin-1 转换、property fallback、direct/INCR chunk assembler；
- Wayland MIME 选择、provider 优先级、版本 capability 和迟到 source cancelled；
- read/write helper 的 partial I/O、`EINTR`、timeout、取消与超限。

这些测试使用纯 reducer、fake clock/deadline input、内存 chunk 和 fake backend effect，不通过
环境变量偷偷选择是否执行。

### 12.2 X11 集成测试

使用 Xvfb 启动隔离 X server，至少验证：

- 启动 snapshot 的有/无 owner；
- 外部 owner 的 Clipboard/Primary 文本读取；
- bridge 作为 owner 响应 `TARGETS`、`TIMESTAMP`、UTF-8 和可表示/不可表示 `STRING`；
- direct 与 INCR 双向传输；
- owner 在读取中变化时旧 transfer 被丢弃；
- 两个 selection 并发不串状态；
- shutdown 后 connection/window/transfer 正常释放。

Xvfb 测试应由明确的 integration test harness 启动和清理，不把开发者当前 `DISPLAY` 当作默认
测试依赖。

### 12.3 Wayland 集成测试

ext provider 的真实 wire harness 固定为 KWin virtual：`scripts/test-wayland-kwin.sh` 创建独立
`XDG_RUNTIME_DIR` 与 D-Bus session，启动 `kwin_wayland --virtual`，再运行默认 ignored 的 actor
wire test。已验证环境为 KWin 6.7.4 与 wl-clipboard 2.3.0；该测试覆盖 ext provider 的
Clipboard/Primary offer 读取、source serving、连续 source 替换、旧 source 迟到 cancellation、
external replacement/OwnershipLost 与 clear，且不连接开发者当前图形会话。

wlr v1/v2、无 provider、disconnect 和精确数据流使用进程内轻量协议 test server。该 server
通过 Unix socket pair 驱动真实 Wayland wire，已覆盖 capability、启动 snapshot、外部 offer、
Clipboard/Primary source 与 compositor disconnect。无显示 helper 测试覆盖 MIME 优先级、
invalid UTF-8、超限、idle/total timeout、取消、partial/慢速 write、worker 饱和边界与 stale
epoch/token completion。两类 harness 合并后分别验证：

- ext provider 优先、wlr fallback 和缺少 provider 的启动错误；
- wlr v1 禁用 Primary、v2 启用 Primary；
- offer MIME 累积、读取、替换、clear 和 stale worker；
- source send、partial write、cancelled 与旧 source 迟到事件；
- Clipboard/Primary 独立；
- 超限、invalid UTF-8、timeout 和 compositor disconnect。

KWin harness 是可选的协议层检查，不进入默认 `cargo test`，因为开发机和基础 CI 镜像不保证安装
compositor；缺少工具时脚本以 77 退出。wlr test server 是默认无桌面依赖测试；限制本地 socket
系统调用的沙箱会得到有诊断的 skip，在允许 Unix socket pair 的 CI/开发环境必须实际通过。
真实 wlr compositor 的 harness 为 `scripts/test-wayland-niri-wlr.sh`：它在现有 Wayland 桌面
之上创建隔离 nested Niri，并仅在测试构建中强制选择 Niri 广告的 wlr v2 global，避免生产路径
按 ext 优先规则选择 ext。2026-08-10 已使用 Niri 26.04 与 wl-clipboard 2.3.0 验证
Clipboard/Primary offer/source、连续 source 替换、旧 source 迟到 cancellation、external
replacement/OwnershipLost 与 clear；生产 provider 选择仍只能走 ext 优先策略。

### 12.4 端到端与回归测试

在同时具有 X11/XWayland 与目标 Wayland compositor 的会话中，使用独立 producer/consumer
验证：

- X11 → Wayland 与 Wayland → X11；
- Clipboard 与受支持时的 Primary；
- ASCII、多字节 UTF-8、换行、NUL 和接近 16 MiB 的文本；
- 重复内容不产生持续 owner ping-pong；
- 两端启动时相同、单侧有值和双侧冲突；
- runtime clear 不清空另一端；
- 快速连续复制、读取中再次复制、source requester 慢读；
- SIGINT/SIGTERM 和 backend 断线的整体退出。

日志或测试 instrumentation 应按 revision/command 计数证明没有无限反馈回环，不能只凭最终文本
看起来正确判断通过。

### 12.5 提交前检查

目标实现最终必须通过：

```bash
cargo fmt --all -- --check
cargo check --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

在迁移尚未完成、默认测试仍依赖 display 或存在既有 lint 时，每个切片必须记录基线与本次差异，
不得新增失败。协议相关切片还必须报告相应 Xvfb/headless/实机会话的实际验证结果。

## 13. 迁移策略

重构采用可审查的纵向切片，不一次性重写全部 backend，也不在没有测试安全网时机械移动文件。

### 13.1 阶段 0：建立事实与安全网

- 以本文和 TODO 固化目标语义；
- 修正默认测试必须连接真实 X server 的结构，把协议测试与纯单元测试分层；
- 建立当前可编译、lint 和手工双向文本同步基线；
- 修复日志正文泄漏和用户文档明显高于实现的问题；
- 不在此阶段改变协议行为。

### 13.2 阶段 1：领域模型与协调器

- 引入 `domain`、纯 `coordinator`、typed revision/token/command 和 reducer tests；
- 用 bounded event channel 与 latest-command mailbox 替换 main 中四组重复路由；
- 先通过 adapter shim 把当前 backend 输出映射为新事件，保持现有协议实现可运行；
- 此阶段完成后，所有跨端去重、启动策略和 clear 策略只存在于 coordinator。

### 13.3 阶段 2：运行时与生命周期

- 引入统一 runtime、actor Ready/Fatal、signal 和 join/shutdown；
- 将协议对象限制在各自 actor；
- 清除 detached task、无界 channel 和无限等待；
- 建立资源限制、worker semaphore、typed error 和内容安全日志。

### 13.4 阶段 3：X11 状态机

- 先提取 typed atom、target 选择和 transfer assembler 的纯逻辑；
- 再用事件驱动读取状态机替换嵌套 poll/sleep；
- 实现正确 TARGETS、property、direct/INCR receive；
- 实现 owner serve、partial/INCR send 与 Xvfb 集成测试；
- 新实现验证通过后删除旧重复 handler，不长期保留两套路径。

### 13.5 阶段 4：Wayland 状态机

- 建立 ext/wlr provider enum 与版本/capability 协商；
- 重写 offer/source registry、bounded worker、完整读写和 stale token；
- 接通真实 Primary receive，删除当前注释代码和未使用 manager；
- 完成可控 headless 或协议 server 集成验证后删除旧实现。

### 13.6 阶段 5：收敛与发布

- 将公开 API 收敛到最小 `run` 入口并删除旧 public backend state；
- 收窄依赖 feature、更新 README/示例/运行要求；
- 完成两种 provider、Xvfb 和实机端到端矩阵；
- 清除所有迁移 shim、临时 allow、死代码和正文日志；
- 以符合 pre-1.0 breaking change 的 Semifold changeset 发布。

每个阶段都必须保持可编译，并为实际影响的独立任务创建独立 changeset。不能先改代码、最后一次
性补本文或 TODO。

## 14. 兼容性与版本策略

- 内容兼容范围严格限于 3.4、7.2、7.3 和 8.2、8.3 定义的纯文本格式；增加或移除 MIME、
  atom、编码或 selection 属于用户可见兼容性变更；
- ext/wlr provider 选择发生在启动期，单次运行不在 provider 之间热切换；
- Clipboard 是运行的最低能力；Primary 是 capability-gated，不支持时必须清晰记录，不能假装
  已同步；
- 当前 `0.1.x` 的公共 Rust API 不作为长期兼容边界，但删除 public 类型仍按 pre-1.0 breaking
  change 提升 minor，并在 changelog 明确；
- CLI 目前没有稳定参数契约；新增参数、配置或 service 文件前先扩展本文；
- package version、changelog、crate、GitHub Release 和 AUR 发布继续由 Semifold CI 管理；
- 纯设计文档变更不创建发布 changeset；开始改变 crate 行为、API、依赖或测试能力的实现切片
  必须创建 changeset。

## 15. 已决定事项与待验证事项

### 15.1 已决定

1. 内部内容模型只支持非空 UTF-8 纯文本，固定 16 MiB 上限；
2. Clipboard 与 Primary 使用独立状态；Primary 按 provider capability 启用；
3. ext data-control 优先，wlr data-control fallback；同一运行只使用一个 provider；
4. 双侧启动快照冲突时不覆盖，单侧有有效文本时补齐另一端；
5. 运行期 clear/empty 不跨端传播；
6. 反馈回环同时依靠 adapter 自有 ownership 识别与 coordinator 文本/revision 状态，不依赖单一
   内容 cache；
7. Backend 使用 actor ownership；协议对象不由全局 Mutex 共享；
8. Backend event 有界，目标命令 latest-wins 且 edge-triggered；
9. X11 大文本必须实现 INCR 收发；Wayland fd 必须完整读写；
10. 默认测试不依赖现有图形会话；协议 wire 行为由隔离集成测试补足；
11. 首版 backend 断线整体退出，不自动重连；
12. 不记录、不持久化剪贴板正文。
13. ext data-control 的 Rust binding 来自 `wayland-protocols` 的 `staging` feature；
    `wayland-protocols-misc` 当前版本不包含该协议，不作为 provider 依赖。
14. ext wire harness 使用隔离的 KWin virtual session；wlr 版本与错误注入使用轻量协议 test
    server，避免依赖当前桌面或要求 KWin 提供它不广告的 wlr global。

### 15.2 待原型验证但不阻塞领域层

- Wayland protocol server 的 required-global remove 注入可继续增强 disconnect 定位，但现有真实
  socket close 已经覆盖统一 fatal disconnect 语义。

### 15.3 已完成的 transport/运维原型

- X11 direct/INCR threshold 已从 X server maximum request length 推导，并通过 Xvfb 的 direct、
  INCR 和慢 requester 测试；
- actor poll timeout 固定为 100 ms，backend event channel 固定为 32；Wayland 固定为 4 worker、
  8 job、12 completion，并通过 channel、10,000 command burst 和饱和 worker 测试；
- KWin 6.7.4 virtual 已验证 ext wire；nested Niri 26.04 已通过测试专用 provider 注入验证真实
  wlr v2 wire。结合 ext 优先纯测试、wlr-only 进程内 server 与两种真实 compositor harness，
  provider 选择和两套 wire 路径均有独立证据；
- 2026-08-10 在 Niri 会话中用 systemd 261 的一次性 `systemd-run --user --wait --collect` 原型验证：
  transient unit 继承 `DISPLAY=:0`、`WAYLAND_DISPLAY=wayland-2` 与
  `XDG_RUNTIME_DIR=/run/user/1000`，没有继承 `XAUTHORITY`。因此未来若设计正式 service unit，
  必须依赖桌面会话把显示变量导入 user manager，并逐环境验证 X11 认证，不能假定所有登录方式
  都提供同一环境；本阶段仍不新增 service 文件或启动顺序契约。

这些原型只允许调整 transport 参数和测试 harness；如果结果要求改变启动冲突、clear、文本范围、
provider 优先级或错误恢复语义，必须先修改本文。

## 16. 完成标准

只有同时满足以下条件，重设计才算完成：

1. Coordinator 是纯 reducer，所有启动、去重、冲突、clear 和 stale 语义有无显示单元测试；
2. 所有长期 channel 有界，目标命令可合并且不会因旧 watch 值重新夺取 ownership；
3. X11 actor 无嵌套事件轮询和固定 busy sleep，direct/INCR 收发通过 Xvfb 测试；
4. Wayland actor 完成 provider 选择、版本协商、offer/source 完整 I/O 和 Primary capability 测试；
5. 默认 `cargo test --all-targets` 不要求开发者已有 X11/Wayland 会话；
6. 端到端验证证明两个方向不会形成无限 ownership 回环；
7. 文本上限、timeout、partial I/O、invalid UTF-8 和 disconnect 均有确定结果；
8. 日志、错误和 panic 路径不包含文本正文；
9. 旧 `SyncEvent` 路由、公开 backend state、注释掉的 Primary 实现和未使用协议对象已经删除；
10. README、示例、运行要求、Semifold changeset 与实际能力一致；
11. 格式、check、Clippy `-D warnings`、默认测试和相应协议集成测试全部通过；
12. `TODO.md` 中属于本设计的实施差异全部完成或被明确移入新的已批准设计阶段。

## 17. 推荐的第一个实施切片

第一个实现任务应建立领域安全网，而不是立即重写 X11 或 Wayland：

1. 新建 `domain.rs` 和 `coordinator.rs`；
2. 实现 `TextPayload`、backend/selection/revision/token/command 类型；
3. 实现两个独立 `SelectionState` 和纯 reducer；
4. 覆盖启动快照、双侧冲突、双向同步、去重、clear、stale ack 和 capability 的单元测试；
5. 用临时 adapter shim 将当前 `SyncEvent` 输入接入 reducer，并让 `main.rs` 删除四组重复路由；
6. 把跨端 command 改为有界、latest-wins、edge-triggered mailbox；
7. 保持现有 X11/Wayland wire 行为不变，并记录手工回归结果。

这一切片首先验证最重要的状态所有权和反馈回环语义，为后续分别替换两个协议 adapter 提供稳定
边界；不会把两套尚未验证的新 wire 状态机和领域重构压进同一个不可审查变更。
