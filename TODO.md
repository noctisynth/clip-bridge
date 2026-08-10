# Clip Bridge 重设计 TODO

> 技术事实来源见 [docs/DESIGN.md](docs/DESIGN.md)。本文件只记录该设计与当前实现之间的差异，
> 不作为独立需求来源。完成并验证后才能勾选事项；设计变化应先修改设计文档。

## 总体目标

- [x] 只以有界、非空 UTF-8 纯文本作为跨 backend 内容模型
- [x] 用纯 coordinator reducer 统一启动快照、冲突、去重、clear 和 stale 语义
- [x] 让 X11 与 Wayland actor 分别独占协议连接、对象和 transfer 状态
- [x] 用有界事件 channel 和 latest-wins、edge-triggered command mailbox 替代无界 channel
- [x] 完成 X11 direct/INCR 收发和 Wayland offer/source 完整 I/O
- [x] 默认测试不依赖开发者已有的 X11/Wayland 会话
- [x] 日志、错误和 panic 路径不输出或持久化剪贴板正文
- [x] README、示例、公开 API、运行要求和发布说明与已验证能力一致

## 阶段 0：建立安全基线

### 设计与现状

- [x] 建立 `docs/DESIGN.md` 作为技术事实来源
- [x] 建立只跟踪设计/实现差异的根 `TODO.md`
- [x] 在设计文档记录当前模块、并发、协议、错误、隐私和测试问题
- [x] 建立当前版本的手工行为基线：X11 → Wayland、Wayland → X11、Clipboard、Primary 实际状态
- [x] 记录目标 compositor、data-control global/version、X server/XWayland 和外部测试工具版本
- [x] 修正 README 中 Rust 1.70、Primary 完整双向同步和 `RUST_LOG` 等未被当前实现证明的表述

### 默认检查与测试分层

- [x] 移除两个默认 X11 单元测试对开发者当前 `DISPLAY` 的无条件依赖
- [x] 把需要真实 display 的测试迁移到具有明确 harness/skip 条件的 integration test
- [x] 为当前无显示环境建立可运行的最小 library 测试入口
- [x] 清理示例中的两个既有 redundant import Clippy 警告
- [x] 记录迁移开始前 `fmt`、`check`、Clippy、默认测试与手工协议测试结果

### 日志与错误最低安全线

- [x] 删除 `main.rs` 中输出 `ClipboardContent` 正文的 Debug 日志
- [x] 为文本领域类型提供不泄漏正文的诊断表示
- [x] 明确区分初始化失败、单次 transfer 失败和 backend fatal disconnect
- [x] 对当前被静默忽略的 channel、flush、ownership 和 event-loop 错误建立清单

### 阶段完成条件

- [x] 默认无显示测试可以运行，协议测试的环境依赖被显式隔离
- [x] 当前功能基线与已知缺陷有可复查记录
- [x] 日志不再输出剪贴板正文
- [x] 本阶段不改变 selection 同步语义

## 阶段 1：领域模型与纯协调器

### 领域类型

- [x] 新建 `src/domain.rs`
- [x] 定义 `BackendId` 与 `SelectionKind`
- [x] 定义互不混用的 `Revision`、`BackendEpoch`、`OfferToken` 和 `CommandId`
- [x] 实现只允许非空、有效 UTF-8、最大 16 MiB 的 `TextPayload`
- [x] 使用 `Arc<str>` 共享 payload，并确保 `Debug`/错误不泄漏正文
- [x] 定义 `BackendCapabilities`、snapshot outcome 和 typed error category

### Backend 契约

- [x] 新建 `src/backend/mod.rs`
- [x] 定义 Ready、SelectionChanged、InitialSnapshot、observed text、selection unavailable、
  ownership applied/failed/lost、recoverable/fatal error 事件
- [x] 定义包含 command id、revision、expected target epoch 和 payload 的 edge-triggered `SetText`
- [x] 定义幂等 `Shutdown`
- [x] 明确 stale offer、stale ack 和旧 command result 的丢弃规则
- [x] 为 backend/selection command 增加 `BackendEpoch` 校验，避免待处理写入覆盖刚发生的外部
  owner

### Coordinator reducer

- [x] 新建 `src/coordinator.rs`
- [x] 为 Clipboard 与 Primary 建立完全独立的 `SelectionState`
- [x] 实现 Ready/capability 汇总
- [x] 实现启动快照：相同不写、单侧有效补齐、双侧冲突等待、Failed 不覆盖
- [x] 实现双向外部文本路由与目标端 observed/owned 相同内容去重
- [x] 收到 SelectionChanged 时清除引用旧目标 epoch 的 pending，等待新 observation 而不盲目重试
- [x] 收到 SelectionChanged 时把该 backend 的旧 observed 设为未知，但不在缺少证据时清除 owned
- [x] 实现 runtime clear、empty、unsupported 不跨 backend 清空
- [x] 实现 revision 单调递增与溢出错误
- [x] 实现 pending command 被更新命令替换，旧 applied/failed/error 不改变新状态
- [x] Reducer 只返回新状态和 effect，不读取协议对象、环境、时钟或日志实现

### 纯单元测试

- [x] 覆盖 `TextPayload` 空值、UTF-8、16 MiB 上下边界和隐私 Debug
- [x] 覆盖 Clipboard/Primary 状态隔离
- [x] 覆盖全部启动 snapshot 组合
- [x] 覆盖两个同步方向、相同文本、ownership lost 和重复 owner 变化
- [x] 覆盖 clear/empty/unsupported/failed 不覆盖另一端
- [x] 覆盖 capability 缺失和 Primary 禁用
- [x] 覆盖 stale offer、stale ack、latest command 和 revision 溢出
- [x] 对相同状态/事件验证 reducer 结果确定性

### 临时接线

- [x] 用 adapter shim 把当前 `SyncEvent` 转换为新领域事件
- [x] 用 coordinator effect 取代 `main.rs` 中四组重复 match/route 分支
- [x] 迁移期 shim 完成后由已验证的原生 X11/Wayland actor 取代
- [x] 为 shim 标记明确删除阶段，不把两套事件模型长期并存

### 阶段完成条件

- [x] 所有跨端策略只存在于 coordinator，不在 main/backend 各自复制
- [x] 领域和 reducer 测试完全不需要 display server
- [x] shim 迁移窗口结束，生产路径与旧协议实现均已删除
- [x] 本阶段变更具有独立 Semifold changeset

## 阶段 2：有界运行时与生命周期

### Runtime

- [x] 新建 `src/runtime.rs`
- [x] Coordinator 串行消费 backend event并同步执行 reducer/effect 分发
- [x] Backend → coordinator 改为容量有限的 MPSC channel
- [x] Coordinator → backend 按 selection 使用保存最新 CommandId 的 watch/mailbox
- [x] Actor 记录已处理 CommandId，旧 watch 值不得导致重新夺取 ownership
- [x] 高频命令合并时只执行最新值，backend event 不静默丢失
- [x] Worker completion channel 有界并按 token 丢弃过期结果

### 生命周期

- [x] 两个 backend 都通过 Ready 后才进入启动 snapshot 协调
- [x] Clipboard 必需 capability 缺失时启动失败
- [x] Primary capability 缺失时只禁用 Primary 并记录一次 warning
- [x] 接入 SIGINT/SIGTERM 协调关闭
- [x] 任一 FatalError、join panic 或 coordinator 不变量错误关闭另一个 backend
- [x] Shutdown 停止新 transfer并在有界时间内等待 actor/worker
- [x] 根据正常信号或 fatal 根因返回正确退出状态
- [x] 首版明确不实现 backend 自动重连

### 资源限制

- [x] 统一 16 MiB payload 上限
- [x] 统一 5 秒 idle timeout 和 30 秒 total timeout
- [x] 每个 backend/selection 最多一个有效读取 transfer
- [x] 用固定 worker pool 限制并发 write worker
- [x] 所有 fd write helper 循环处理 partial write、`EINTR` 和 `EAGAIN`
- [x] 超限、timeout、cancel 和 shutdown 都释放 fd/buffer/permit
- [x] 为 channel 容量、actor poll timeout 和固定 worker pool 建立压力测试后确定最终常量

### 错误与日志

- [x] 引入 typed `StartupError`、`TransferError`、`ProtocolError`、`CoordinatorError` 和
  `ShutdownError`
- [x] 使用 Cargo CLI 引入 `thiserror`
- [x] 为 `tracing-subscriber` 通过 Cargo CLI 启用 `env-filter`
- [x] 实现 `RUST_LOG` 契约和默认 info filter
- [x] 所有 span 只包含 backend、selection、revision/token/id、长度、MIME 和阶段
- [x] channel 接收端关闭时结束组件，不循环记录 warning 后半运行

### 阶段完成条件

- [x] 生产路径没有无界 channel、detached task 或无限 worker 创建
- [x] 正常信号、backend fatal、join panic 和 shutdown timeout 都有自动测试
- [x] 压力测试下内存受 payload/channel/worker 上限约束
- [x] 本阶段变更具有独立 Semifold changeset

## 阶段 3：X11 Adapter 重写

### Actor 与初始化

- [x] 建立 X11 专用 actor，connection/window/atom 只在该 actor 内访问
- [x] 使用 fd poll 和 deadline 驱动事件，不使用固定 sleep busy loop
- [x] 查询 XFixes 并为 Clipboard/Primary 注册 owner 通知
- [x] 用 typed `Atoms` 一次性 intern 固定 atom/property
- [x] 删除运行期字符串 HashMap atom lookup 和相关 `unwrap()`
- [x] 为启动时两个 selection 查询 owner并产生 snapshot

### 读取状态机

- [x] 为每个 selection 实现独立 Idle/Targets/Data/INCR 状态
- [x] 每次 owner 变化分配新 OfferToken并废弃旧 transfer
- [x] 先请求 `TARGETS`，按设计优先级选择一种文本 target
- [x] 正确关联 SelectionNotify 的 requestor/selection/target/property/token
- [x] 实现 direct property 读取
- [x] 实现 INCR receive、chunk delete/notify、上限与 timeout
- [x] owner 变化、property 错误、invalid UTF-8 和超限时安全终止
- [x] 实现 UTF-8、TEXT 明确类型和 ISO-8859-1 STRING 解码
- [x] 删除 request 函数内部嵌套 poll 和 sleep

### Ownership 与 serving

- [x] `SetText` 建立不可变 owned snapshot并使用合适 timestamp 请求 ownership
- [x] 验证/确认 ownership 后才报告 OwnershipApplied
- [x] SelectionClear 只报告 OwnershipLost，不跨 backend clear
- [x] 正确响应 TARGETS 与 TIMESTAMP
- [x] 正确响应 UTF8_STRING、`text/plain;charset=utf-8`、`text/plain` 和 TEXT
- [x] STRING 只在可无损表示时返回，否则明确拒绝
- [x] property 为 NONE 时使用 target property fallback
- [x] 对 MULTIPLE 明确拒绝，或完整实现每个 target/property pair
- [x] 根据 X server maximum request length 推导 direct/INCR threshold
- [x] 实现 INCR send、requester property delete、partial chunk、timeout 和并发限制
- [x] 每个 requester transfer 使用创建时 payload 快照

### X11 自动化测试

- [x] 为 target 选择、Latin-1、property fallback 和 chunk assembler 建立纯测试
- [x] 建立自动启动/清理 Xvfb 的 integration harness
- [x] 覆盖启动 snapshot、Clipboard/Primary 和外部 owner 读取
- [x] 覆盖 bridge TARGETS/TIMESTAMP/UTF-8/STRING serving
- [x] 覆盖 direct 与 INCR 双向传输
- [x] 覆盖 owner 在 transfer 中变化和两个 selection 并发
- [x] 覆盖 shutdown 与 connection failure

### 阶段完成条件

- [x] X11 路径没有嵌套全局事件消费、固定轮询 sleep 或协议 `unwrap()`
- [x] direct/INCR 收发和 ownership 通过 Xvfb 测试
- [x] 旧 X11 handler/缓存/测试代码完全删除
- [x] 本阶段变更具有独立 Semifold changeset

## 阶段 4：Wayland Adapter 重写

### Provider 与 registry

- [x] 建立 Wayland 专用 actor，connection/event queue/proxy 只在 actor 内访问
- [x] 使用 prepare-read/poll/dispatch/flush 事件循环替代长期 roundtrip loop
- [x] 实现 ext-data-control provider
- [x] 实现 wlr-data-control fallback
- [x] 两者同时存在时只选择 ext provider
- [x] bind version 使用 advertised/client supported 的最小值
- [x] Clipboard provider 缺失时返回启动错误
- [x] wlr v1 只启用 Clipboard，v2 启用 Primary
- [x] 选择并固定一个默认 seat，多个 seat 时记录明确策略
- [x] 删除未接线 `zwp_primary_selection_device_manager_v1` 和无用途 compositor 对象

### Offer 读取

- [x] 按 provider object id 保存 offer 与 MIME 集合
- [x] 每次 selection 变化生成新 token并取消/废弃旧 worker
- [x] 实现 UTF-8 `text/plain` MIME 选择与参数解析
- [x] 不把 X11 atom 名称作为 Wayland MIME 请求
- [x] receive request 后 flush并正确关闭本地 write fd
- [x] read worker执行 idle/total timeout、16 MiB 上限和完整读取
- [x] worker 返回 actor后再次验证 token
- [x] empty、unsupported、invalid UTF-8、clear 和过大不覆盖 X11
- [x] 接通 Clipboard 与 capability-gated Primary 的真实 receive 路径

### Source 与写入

- [x] 每次 SetText 创建一次性 source并保存 command/revision/selection/payload
- [x] 只广告 `text/plain;charset=utf-8` 与 `text/plain`
- [x] 新 source set/flush 成功后再替换旧 source 记录
- [x] 按 source object id 选择准确 payload 快照
- [x] write worker循环写完整并处理 partial I/O/timeout/cancel
- [x] 旧 source 迟到 cancelled 不清理新 source
- [x] selection event 先递增并上报 `BackendEpoch`，不依赖无协议证据的 source/offer proxy
  等同
- [x] 无法证明来源的自身 selection echo 作为 observation 上报，由 coordinator 按目标端内容去重
- [x] source cancelled/external replacement 正确报告 OwnershipLost
- [x] 单个 fd 失败 recoverable，connection/protocol error fatal

### Wayland 自动化与实机测试

- [x] 原型比较可控 headless compositor 与 Wayland protocol test server
- [x] 选定并记录 ext CI harness、版本与启动方式
- [x] 覆盖 ext 优先、wlr fallback、无 provider 和版本 capability
- [x] 覆盖 offer MIME、replacement、clear、stale worker 和 Primary
- [x] 覆盖 source send、partial write、cancelled 和迟到旧事件
- [x] 覆盖超限、invalid UTF-8、timeout 和 compositor disconnect
- [x] 在至少一个 ext provider 和一个 wlr provider 目标 compositor 上实机验证

### 阶段完成条件

- [x] Wayland 路径没有长期 roundtrip loop、无界 pipe buffer 或 single-write 假设
- [x] Clipboard 与 capability-gated Primary 均有 ext provider 真实 wire 验证
- [x] 旧注释 Primary 路径、重复缓存和未使用 protocol state 完全删除
- [x] 本阶段变更具有独立 Semifold changeset

## 阶段 5：接线、API 与依赖收敛

### 模块与主程序

- [x] 按设计建立 `backend/x11/` 与 `backend/wayland/` 的状态所有权模块
- [x] `main.rs` 只负责默认运行入口和退出码
- [x] `runtime` 成为唯一同时构造 coordinator 与两个 backend 的模块
- [x] 删除旧 `SyncEvent`、`ClipboardContent::Empty` 路由和 backend 直出 public state
- [x] 删除全部迁移 shim、临时 allow、死代码和重复缓存

### 公开 API

- [x] 将 library 公开入口收敛为最小 async `run() -> Result<(), BridgeError>` 形态
- [x] 将 atom、proxy、backend actor 和 coordinator state 保持为内部 API
- [x] 审计并记录移除当前 public 类型的 pre-1.0 breaking impact
- [x] 为 API 收敛使用 minor changeset，不作为 patch 隐藏发布

### Cargo 与 toolchain

- [x] 通过 Cargo CLI 添加 `thiserror` 和所需 feature
- [x] 收窄 Tokio `full` feature为实际使用集合
- [x] 收窄 x11rb `all-extensions` feature为实际使用集合
- [x] 通过 `wayland-protocols` 的 `staging` feature 接线 ext data-control
- [x] 删除不包含 ext data-control 且无其他用途的 `wayland-protocols-misc`
- [x] 保留 wlr fallback依赖并移除不再使用的 primary-selection 依赖/feature
- [x] 决定并实施 latest stable toolchain 声明
- [x] 检查 `Cargo.toml`/`Cargo.lock` 只有预期差异

### 阶段完成条件

- [x] 源码依赖方向符合 domain → backend contract、backend/runtime → domain 的设计
- [x] 不再发布内部 protocol state 作为承诺 API
- [x] 没有未使用依赖和过宽 feature
- [x] 本阶段变更具有独立 Semifold changeset

## 阶段 6：端到端、文档与发布准备

### 端到端矩阵

- [x] X11 → Wayland Clipboard
- [x] Wayland → X11 Clipboard
- [x] X11 → Wayland Primary（provider 支持时）
- [x] Wayland → X11 Primary（provider 支持时）
- [x] ASCII、多字节 UTF-8、换行、NUL、接近/超过 16 MiB
- [x] 重复内容和 ownership echo 不产生持续 ping-pong
- [x] 启动时相同、单侧有值、双侧冲突和 snapshot failure
- [x] runtime clear/empty 不清空另一端
- [x] 快速连续 owner 变化、读取中再次复制和慢 requester
- [x] SIGINT/SIGTERM、X11 disconnect 与 Wayland disconnect
- [x] 用 revision/command instrumentation 证明没有无限反馈回环

### 用户与运维文档

- [x] 重写 README 的范围、非目标、依赖和运行要求
- [x] 明确 ext/wlr provider 与 Primary capability 检测
- [x] 更新 Rust stable 要求并删除 Rust 1.70 错误表述
- [x] 文档化 `RUST_LOG`、默认日志级别和隐私保证
- [x] 文档化 16 MiB、timeout、empty/clear 和 startup conflict 语义
- [x] 更新示例，使其使用目标最小 public API 或明确作为协议测试工具
- [x] 记录 systemd user service 环境原型结果，但不在设计前擅自新增 service 契约

### 最终质量门槛

- [x] `cargo fmt --all -- --check`
- [x] `cargo check --locked --all-targets`
- [x] `cargo clippy --locked --all-targets -- -D warnings`
- [x] `cargo test --locked --all-targets`
- [x] Xvfb 集成测试通过
- [x] 选定的 Wayland headless/protocol harness 通过
- [x] 目标 ext 与 wlr compositor 实机验证通过
- [x] 日志/错误/panic 内容泄漏审计通过
- [x] 所有生产外部失败路径无 `unwrap()`

### 发布

- [x] 每个独立实现切片都有独立、可解析的 Semifold changeset
- [x] `semifold status` 的最终发布计划与 API/行为变化一致
- [ ] 由 main 分支 Semifold CI 消费 changeset、更新版本/changelog 并执行 crate/GitHub/AUR 发布
- [x] 本地不执行 `semifold version`、`semifold publish`、`semifold ci` 或 AUR 发布脚本

## 完成条件

- [x] [docs/DESIGN.md](docs/DESIGN.md) 第 16 节全部完成标准均有实现和验证证据
- [x] 本文件不再存在与当前批准设计对应的未完成实现差异；仅保留由 main 分支 CI 执行的发布动作
- [x] 所有文档描述与最新已验证实现一致
