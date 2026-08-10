# Clip Bridge Agent 协作约定

本文件适用于仓库中的全部目录和任务。

## 1. 权威文档

- 技术设计：[docs/DESIGN.md](docs/DESIGN.md)
- 实施差异清单：[TODO.md](TODO.md)

`docs/DESIGN.md` 是项目需求、架构、协议语义、兼容边界和测试策略的技术事实来源。开始实现
前先确认设计已经表达本次需求；如果需求或技术方案发生变化，必须先更新设计文档，再修改
代码。

`TODO.md` 只记录设计与当前实现之间的差异：

- 设计领先于代码时，新增或更新对应 TODO；
- 完成并验证一项设计要求后，才能勾选对应 TODO；
- 不要把 TODO 作为独立需求来源，也不要让 TODO 与设计文档冲突。

涉及技术方案的修改顺序始终是：

1. `docs/DESIGN.md`
2. `TODO.md`
3. 代码、配置、测试和面向用户的文档

## 2. 修改授权

- 未经用户明确要求，不直接修改代码、配置或文档。分析、审查、解释和方案讨论不构成修改授权。
- 修改范围以用户请求为准，不顺带处理无关问题；发现额外问题时在交付说明中列出。
- 开始修改前检查工作区状态。现有未提交内容属于用户，必须保留，不得覆盖、回退、格式化或
  复用为本任务产物。
- 技术方向存在会显著影响协议行为、兼容性、依赖或发布结果的歧义时，先停止相关实现并向
  用户确认。

## 3. Rust 与 Cargo 规则

- 生产 Rust 代码不得新增可能因外部状态而 panic 的 `unwrap()`。显示服务器、IPC、文件
  描述符、channel、解析、用户输入和系统调用失败必须传播、转换或有意记录。
- 只有类型系统、构造流程或同一函数内的穷尽分支已经证明内部不变量时才使用 `expect()`，
  且消息必须具体说明不变量；测试中的断言性 `unwrap()`、`expect()` 可以保留。
- 除非用户明确要求直接编辑，否则不得手工修改 `Cargo.toml`。新增、移除或更新依赖必须使用
  Cargo CLI，例如 `cargo add -p clip-bridge`、`cargo remove -p clip-bridge` 或
  `cargo update -p <dependency> --precise <version>`。
- Cargo CLI 命令失败、预期差异不明确或受网络权限阻塞时，立即停止并说明原因；不得通过
  手工编辑 manifest 或 lockfile 绕过。
- 不得借依赖操作修改 package version。版本号、`Cargo.lock` 中的本包版本和 `CHANGELOG.md`
  由发布流程维护。

## 4. 验证与文档同步

- 根据变更范围运行格式、编译、Clippy 和测试检查；具体测试层级、显示服务器要求与协议验收
  以 `docs/DESIGN.md` 为准。
- 默认基线命令为：

  ```bash
  cargo fmt --all -- --check
  cargo check --locked --all-targets
  cargo clippy --locked --all-targets
  cargo test --locked --all-targets --no-run
  ```

- 不要求在无关任务中清理全部既有警告或环境相关测试问题，但不得新增警告或让基线退化。
- 每次完成代码任务后，判断是否影响用户可见行为、协议契约、运行要求、示例或架构说明；有
  影响时在同一任务中同步相应文档。判断不需要更新时，在交付说明中写明原因。
- 不得把仅编译成功描述为完成了 X11/Wayland 协议运行期验证。缺少图形会话或外部工具时，
  明确列出未完成的验证及原因。

## 5. Semifold 与发布

- 影响已发布 `clip-bridge` 包的功能、修复、性能、重构、依赖或测试能力变更，使用
  `semifold commit` 创建独立 changeset；纯文档、CI 和不影响发布包的仓库维护可以不创建。
- changeset 只使用 `.changes/config.toml` 中登记的 `clip-bridge`，并选择与语义版本和配置
  tag 一致的 level/tag。
- 默认一个独立任务对应一个新 changeset。不得修改、合并、删除或复用工作区中已有的
  changeset，除非用户明确要求把多个改动作为同一发布项。
- 不得手工编写 changeset 来绕过 CLI。创建后运行 `semifold status`，确认文件可解析且发布
  计划符合预期。
- 本地和 Agent 环境不得执行 `semifold version`、`semifold publish`、`semifold ci` 或
  `scripts/release-aur.mjs`，包括 dry-run。版本、crate、GitHub Release 和 AUR 发布由 main
  分支上的 Semifold CI 独占完成。

## 6. 交付说明

- 总结实际修改、用户可见影响、完成的验证和所有未完成或被阻塞的事项。
- 明确说明本次技术方案相对任务开始时既有设计的变化；若无变化，写明“本次技术方案相对
  既有设计无变动”。
- 若方案发生变化，说明变化原因及其对架构、协议、兼容性、并发、依赖、测试或发布的影响，
  并说明如何同步到 `docs/DESIGN.md` 与 `TODO.md`。
