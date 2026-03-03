# AlephTX v3.2.0 - TODO List

## 🚨 Critical - Signature Verification Issue

### Current Status
- ✅ FFI integration with Go signer library working
- ✅ Order signing successful (tx_type=14, tx_hash generated)
- ✅ HTTP request reaches Lighter API
- ❌ **Signature verification fails**: `{"code":21120,"message":"invalid signature"}`

### Root Cause Analysis Needed
1. **HTTP Request Format**
   - Current: `multipart/form-data` with `tx_type` and `tx_info` fields
   - Need to verify against Python SDK's actual HTTP format
   - Check if `tx_info` needs base64 encoding or other transformation

2. **Signature Format**
   - Go signer returns hex-encoded `tx_info`
   - Verify if Lighter API expects different encoding
   - Compare with Python SDK's `send_tx` implementation

3. **Nonce Management**
   - Currently using simple counter starting from 1
   - May need to fetch initial nonce from API
   - Check if nonce should be per-session or persistent

### Action Items
- [ ] Capture Python SDK HTTP request with tcpdump/mitmproxy
- [ ] Compare exact HTTP headers and body format
- [ ] Test with Lighter's official Go SDK directly
- [ ] Add debug logging for raw HTTP request/response
- [ ] Verify `tx_info` encoding (hex vs base64 vs raw bytes)

## 🔧 Technical Debt

### Order Management
- [ ] Implement proper cancel order via FFI
- [ ] Add order status tracking
- [ ] Implement order reconciliation from WebSocket events

### Error Handling
- [ ] Add retry logic for transient network errors
- [ ] Implement circuit breaker for API failures
- [ ] Better error messages with actionable suggestions

### Testing
- [ ] Unit tests for FFI bindings
- [ ] Integration tests with mock Lighter API
- [ ] End-to-end test with testnet

## 📝 Documentation
- [ ] Document FFI architecture
- [ ] Add troubleshooting guide
- [ ] Update deployment instructions

## 🎯 Future Enhancements
- [ ] Support for other order types (IOC, FOK, etc.)
- [ ] Position management and PnL tracking
- [ ] Risk management integration
- [ ] Performance metrics and monitoring

---

**Last Updated**: 2026-03-03
**Status**: Signature verification blocking production deployment
