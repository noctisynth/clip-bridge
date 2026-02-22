# X11 & Wayland Clipboard Bridge

This Rust-based program provides seamless synchronization of clipboard contents between X11 and Wayland environments. It supports both Clipboard and Primary selection types, enabling real-time, bidirectional clipboard sharing.

## Features

- ✅ **Bidirectional Sync**: Synchronizes clipboard data between X11 and Wayland
- ✅ **Real-time Monitoring**: Automatically detects clipboard changes
- ✅ **Dual Selection Support**: Handles both Clipboard and Primary selections
- ✅ **Content Deduplication**: Prevents redundant synchronization of identical content
- ✅ **UTF-8 Compatible**: Full support for multi-byte characters including Chinese

## Build and Run Instructions

### Prerequisites
- Rust 1.70 or higher
- Development libraries for X11 and Wayland
- `xclip` utility (for testing purposes)

### Build

```bash
cargo build --release
```

### Run

```bash
cargo run
```

### Manual Testing

1. Start the program:
   ```bash
   cargo run
   ```

2. In another terminal, test clipboard operations:
   ```bash
   # Test Clipboard selection
   echo "Test content $(date)" | xclip -selection clipboard

   # Test Primary selection
   echo "Primary selection content $(date)" | xclip -selection primary
   ```

3. Observe the program output for synchronization logs.

## How It Works

### X11 Side
- Creates a hidden window to receive clipboard events
- Periodically checks clipboard ownership changes
- Requests new clipboard content upon change detection and sends it to Wayland

### Wayland Side
- Uses the `zwlr_data_control_v1` protocol to monitor clipboard changes
- Reads clipboard content on change and sends it to X11
- Supports setting clipboard content

### Synchronization Logic
- Caches content to avoid duplicate synchronization
- Detects clipboard clearing events
- Processes asynchronously to avoid UI blocking

## Logging Levels

The program outputs detailed logs configurable via environment variables:

```bash
# Debug mode (default)
RUST_LOG=debug cargo run

# Info level only
RUST_LOG=info cargo run

# Disable logs except errors
RUST_LOG=error cargo run
```

## Troubleshooting

### Common Issues

1. **Build Failures**: Ensure all required development libraries are installed
2. **Permission Denied**: Verify access rights to X11 and Wayland servers
3. **Sync Failures**: Check logs for error messages

### Debugging Tips

- Use `RUST_LOG=debug` for verbose logging
- Confirm X11 and Wayland are running properly
- Test clipboard manually with `xclip` and `wl-paste`

## Technical Details

### Dependencies
- `x11rb`: X11 bindings
- `wayland-client`: Wayland client library
- `wayland-protocols`: Wayland protocol definitions
- `tokio`: Asynchronous runtime
- `tracing`: Logging framework

### Protocol Support
- X11 Clipboard and Primary selections
- Wayland `zwlr_data_control_v1` protocol
- UTF-8 text format

## License

This project is licensed under the MIT License.

## Contribution

Contributions are welcome! Please open issues or submit pull requests.
