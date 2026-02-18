# X11 <-> Wayland 剪切板同步程序

这是一个用于在X11和Wayland之间同步剪切板内容的Rust程序。它支持剪切板(Clipboard)和主选择(Primary)两种类型的同步。

## 功能特性

- ✅ **双向同步**: X11 ↔ Wayland
- ✅ **实时监听**: 自动检测剪切板变化
- ✅ **双类型支持**: 剪切板(Clipboard) + 主选择(Primary)
- ✅ **内容去重**: 避免重复同步相同内容
- ✅ **UTF-8支持**: 完整支持中文等多字节字符

## 修复的问题

### 1. 通道连接错误
- **问题**: 事件发送到错误的通道，导致同步失败
- **修复**: 修正了`x11_sync_tx`和`wayland_sync_tx`的赋值

### 2. 缺失的实时监听
- **问题**: 无法检测程序启动后的剪切板变化
- **修复**: 添加了定期检查剪切板所有权变化的机制

### 3. 事件处理逻辑错误
- **问题**: 事件被错误地识别为来自错误的源
- **修复**: 修正了同步循环中的事件匹配逻辑

### 4. Wayland事件循环问题
- **问题**: 事件处理不及时，可能导致同步延迟
- **修复**: 改进了Wayland事件循环的处理方式

## 编译和运行

### 前置要求
- Rust 1.70+
- X11和Wayland开发库
- `xclip`工具（用于测试）

### 编译
```bash
cargo build --release
```

### 运行
```bash
cargo run
# 或者运行发布版本
./target/release/clip-brige
```

## 测试

程序包含了一个测试脚本：

```bash
chmod +x test_clipboard.sh
./test_clipboard.sh
```

### 手动测试
1. 启动程序：
   ```bash
   cargo run
   ```

2. 在另一个终端中测试剪切板：
   ```bash
   # 测试剪切板
   echo "测试内容 $(date)" | xclip -selection clipboard
   
   # 测试主选择
   echo "主选择内容 $(date)" | xclip -selection primary
   ```

3. 观察程序输出，应该能看到同步日志

## 工作原理

### X11端
- 创建一个隐藏窗口用于接收剪切板事件
- 定期检查剪切板所有权变化
- 当检测到变化时，请求新内容并发送到Wayland

### Wayland端
- 使用`zwlr_data_control_v1`协议监听剪切板变化
- 当检测到变化时，读取内容并发送到X11
- 支持设置剪切板内容

### 同步逻辑
- 使用内容缓存避免重复同步
- 支持剪切板清空检测
- 异步处理，不阻塞UI

## 日志级别

程序使用详细的日志输出，可以通过环境变量控制：

```bash
# 调试模式（默认）
RUST_LOG=debug cargo run

# 只显示信息
RUST_LOG=info cargo run

# 关闭日志
RUST_LOG=error cargo run
```

## 故障排除

### 常见问题

1. **编译错误**: 确保安装了所需的开发库
2. **权限问题**: 确保程序有访问X11和Wayland的权限
3. **同步失败**: 检查日志中的错误信息

### 调试技巧

1. 使用`RUST_LOG=debug`查看详细日志
2. 检查X11和Wayland是否正常运行
3. 使用`xclip`和`wl-paste`手动测试剪切板

## 技术细节

### 依赖项
- `x11rb`: X11绑定
- `wayland-client`: Wayland客户端库
- `wayland-protocols`: Wayland协议
- `tokio`: 异步运行时
- `tracing`: 日志框架

### 协议支持
- X11 CLIPBOARD和PRIMARY选择
- Wayland zwlr_data_control_v1协议
- UTF-8文本格式

## 许可证

MIT License

## 贡献

欢迎提交Issue和Pull Request！