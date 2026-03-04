# AlephTX Go Feeder Guidelines (Network I/O & CGO)

You are operating in the Go Feeder. Your job is network ingestion, WebSocket management, and CGO FFI exports.

## 🌉 1. CGO Export Constraints (CRITICAL)
When writing exported functions (`//export MyFunc`) for Rust to call:
- **String Allocation & Freeing**: When returning strings (like signatures) to Rust, use `C.CString`. You MUST provide a corresponding `//export FreeCString(s *C.char)` function using `C.free` so Rust can clean it up safely.
- **No Go Pointers in C**: Never pass memory containing Go pointers (managed by Go GC) to C/Rust code.
- **Error Handling**: CGO does not support multiple return values. Return structured C-structs or pass pre-allocated error buffer pointers from Rust.

## 🚀 2. Shared Memory Writers (IPC)
- **C-ABI Memory Layout**: Structs shared with Rust (e.g., `ShmPrivateEvent` in `shm/events.go`) MUST match byte-for-byte. Check Go's implicit padding. Insert `_pad1 uint32` and `_padding [8]byte` explicitly to ensure the total size is EXACTLY 64 bytes. Assert size in `init()` using `unsafe.Sizeof`.
- **Seqlock Protocol**: When writing to the BBO Matrix (`feeder/shm/matrix.go`), strictly follow: `Seq++ (Odd) -> Write Payload -> Seq++ (Even)`.
- **Atomic Operations**: Only use `sync/atomic` (`atomic.StoreUint64`, `atomic.AddUint32`) for updating the event RingBuffer `write_idx` and Matrix versions.

## 📡 3. WebSocket Management
- **No Blocking**: WebSocket read loops must not block on channel writes.
- **Auto-Reconnect**: All exchange connections must use the `RunConnectionLoop` pattern for infinite reconnects and backoff.
- **SDK Usage**: Always prefer using the official SDK (e.g., `lighter-go`) for authentication and stream parsing.