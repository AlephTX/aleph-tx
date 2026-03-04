# AlephTX Rust Core Guidelines (HFT & FFI)

You are operating in the Rust Core. Latency, Memory Safety, and Determinism are paramount.

## 🌉 1. FFI & Memory Safety (CRITICAL)
When modifying `src/lighter_ffi.rs` or handling CGO cross-language calls:
- **Async Starvation Prevention**: ANY FFI call (e.g., `sign_create_order`) inside Rust MUST be wrapped in `tokio::task::spawn_blocking(move || { ... })`. Never block a Tokio async worker thread with CGO context switching or cryptography.
- **Memory Leaks (Use-after-Free)**: If Go allocates a `C.CString` to return a signature, Rust MUST explicitly call a Go-exported free function (e.g., `FreeCString`) via FFI. DO NOT use `libc::free` (wrong allocator) or Rust's `CString::from_raw` to drop memory allocated by Go.

## ⚡ 2. Hot-Path Constraints (Quoting Loop)
- **ZERO Heap Allocations**: Inside `try_read`, `check_arbitrage`, or `on_idle`, you must NOT allocate heap memory (no `String`, no `Vec::push`, no `Box::new`). Use stack variables.
- **Rollback Discipline**: If an order fails (at the FFI signing step or HTTP step), you MUST execute `ledger.add_in_flight(-signed_size)` to rollback the optimistic state. Do not leave ghost in-flight positions. Use RAII guards if possible.

## 🔒 3. Concurrency & Atomics
- **Hardware Memory Barriers**: When reading `write_idx` from shared memory in `shm_event_reader.rs`, use `AtomicU64::load(Ordering::Acquire)` and `compiler_fence(Ordering::Acquire)`. DO NOT use `std::ptr::read_volatile` for synchronization across threads/processes.
- **RwLock Hygiene**: When accessing `ShadowLedger`, keep locks extremely brief. Extract data, drop the lock guard, THEN execute async HTTP calls.

## 📐 4. Math & Precision
- Never hardcode format strings like `format!("{:.2}", price)`. Always use dynamic `round_to_tick(val, tick_size)`.
- **Division by Zero Avoidance**: In `lighter_mm.rs`, if `last_price == 0.0` (strategy boot), bypass the deviation threshold check to prevent `NaN` crashes.