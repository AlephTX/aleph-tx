package main

import (
	"bytes"
	"context"
	"crypto/ed25519"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strconv"
	"strings"
	"testing"
	"time"
)

func test40ByteKeyHex(seed byte) string {
	b := make([]byte, 40)
	for i := range b {
		b[i] = seed + byte(i)
	}
	return hex.EncodeToString(b)
}

func TestLighterCoreAndRobinhoodStandardAndCustomFeeOverride(t *testing.T) {
	coreKey := test40ByteKeyHex(0x11)
	rhKey := test40ByteKeyHex(0x55)
	coreIdx := "123456"
	rhIdx := "654321"
	coreL1 := "0x1111111111111111111111111111111111111111"
	rhL1 := "0x2222222222222222222222222222222222222222"

	t.Setenv("LIGHTER_PRIVATE_KEY", coreKey)
	t.Setenv("LIGHTER_ACCOUNT_INDEX", coreIdx)
	t.Setenv("LIGHTER_API_KEY_INDEX", "2")
	t.Setenv("LIGHTER_CHAIN_ID", "304")

	t.Setenv("LIGHTER_RH_PRIVATE_KEY", rhKey)
	t.Setenv("LIGHTER_RH_ACCOUNT_INDEX", rhIdx)
	t.Setenv("LIGHTER_RH_API_KEY_INDEX", "3")
	t.Setenv("LIGHTER_RH_CHAIN_ID", "466324")

	var seenCoreAuth, seenRHAuth string
	var seenCorePaths, seenRHPaths []string

	coreSrv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		seenCorePaths = append(seenCorePaths, r.Method+" "+r.URL.Path)
		switch r.URL.Path {
		case "/api/v1/accountLimits":
			seenCoreAuth = r.Header.Get("Authorization")
			if seenCoreAuth == "" || r.URL.Query().Get("account_index") != coreIdx {
				http.Error(w, "unauthorized", http.StatusUnauthorized)
				return
			}
			_ = json.NewEncoder(w).Encode(map[string]any{
				"code":                   200,
				"current_maker_fee_tick": 0,
				"current_taker_fee_tick": 0,
				"user_tier":              "std",
				"user_tier_name":         "standard",
				"bo_fee_rate":            1.0,
			})
		case "/api/v1/account":
			_ = json.NewEncoder(w).Encode(map[string]any{
				"code": 200,
				"accounts": []map[string]any{
					{
						"index":             123456,
						"l1_address":        coreL1,
						"account_type":      0,
						"collateral":        "9999.99",
						"available_balance": "8888.88",
					},
				},
			})
		default:
			http.NotFound(w, r)
		}
	}))
	defer coreSrv.Close()

	rhSrv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		seenRHPaths = append(seenRHPaths, r.Method+" "+r.URL.Path)
		switch r.URL.Path {
		case "/api/v1/accountLimits":
			seenRHAuth = r.Header.Get("Authorization")
			if seenRHAuth == "" || r.URL.Query().Get("account_index") != rhIdx {
				http.Error(w, "unauthorized", http.StatusUnauthorized)
				return
			}
			// Simulate custom fee override on account_type==0 (50 ticks = 0.5 bps, 150 ticks = 1.5 bps)
			_ = json.NewEncoder(w).Encode(map[string]any{
				"code":                   200,
				"current_maker_fee_tick": 50,
				"current_taker_fee_tick": 150,
				"user_tier":              "plus",
				"user_tier_name":         "Plus",
				"bo_fee_rate":            0.72,
			})
		case "/api/v1/account":
			_ = json.NewEncoder(w).Encode(map[string]any{
				"code": 200,
				"accounts": []map[string]any{
					{
						"index":        654321,
						"l1_address":   rhL1,
						"account_type": 0,
						"collateral":   "4321.00",
					},
				},
			})
		default:
			http.NotFound(w, r)
		}
	}))
	defer rhSrv.Close()

	report, err := runAudit(context.Background(), auditConfig{
		venues:                  []string{"lighter", "lighter_robinhood"},
		timeout:                 5 * time.Second,
		lighterCoreURL:          coreSrv.URL,
		lighterRHURL:            rhSrv.URL,
		allowInsecureTestServer: true,
	}, nil)
	if err != nil {
		t.Fatalf("runAudit failed: %v", err)
	}

	if seenCoreAuth == "" || !strings.Contains(seenCoreAuth, ":"+coreIdx+":2:") {
		t.Fatalf("unexpected Core auth token format")
	}
	if seenRHAuth == "" || !strings.Contains(seenRHAuth, ":"+rhIdx+":3:") {
		t.Fatalf("unexpected RH auth token format")
	}
	if len(seenCorePaths) != 2 || seenCorePaths[0] != "GET /api/v1/accountLimits" || seenCorePaths[1] != "GET /api/v1/account" {
		t.Fatalf("unexpected Core endpoints called: %v", seenCorePaths)
	}
	if len(seenRHPaths) != 2 || seenRHPaths[0] != "GET /api/v1/accountLimits" || seenRHPaths[1] != "GET /api/v1/account" {
		t.Fatalf("unexpected RH endpoints called: %v", seenRHPaths)
	}

	coreRes := report.Venues["lighter"]
	if coreRes.AccountFeeStatus != statusVerified || *coreRes.MakerFeeBps != "0" || *coreRes.TakerFeeBps != "0" {
		t.Fatalf("unexpected Core result: %+v", coreRes)
	}
	if !*coreRes.StandardZeroConfirmed || *coreRes.CustomFeeOverrideFound {
		t.Fatalf("expected standard_zero_confirmed=true and custom_fee_override_detected=false, got %+v", coreRes)
	}

	rhRes := report.Venues["lighter_robinhood"]
	if rhRes.AccountFeeStatus != statusVerified || *rhRes.MakerFeeBps != "0.5" || *rhRes.TakerFeeBps != "1.5" {
		t.Fatalf("unexpected RH result: %+v", rhRes)
	}
	if *rhRes.StandardZeroConfirmed || !*rhRes.CustomFeeOverrideFound {
		t.Fatalf("expected custom_fee_override_detected=true on RH, got %+v", rhRes)
	}

	raw, _ := json.Marshal(report)
	text := string(raw)
	for _, secret := range []string{coreKey, rhKey, coreIdx, rhIdx, coreL1, rhL1, seenCoreAuth, seenRHAuth, "9999.99", "8888.88", "4321.00"} {
		if strings.Contains(strings.ToLower(text), strings.ToLower(secret)) {
			t.Fatalf("output leaked forbidden value %q", secret)
		}
	}
}

func TestBackpackEd25519SignatureAndRedaction(t *testing.T) {
	seed := bytes.Repeat([]byte{0x2a}, ed25519.SeedSize)
	priv := ed25519.NewKeyFromSeed(seed)
	pub := priv.Public().(ed25519.PublicKey)
	pubB64 := base64.StdEncoding.EncodeToString(pub)
	secB64 := base64.StdEncoding.EncodeToString(seed)

	t.Setenv("BACKPACK_PUBLIC_KEY", pubB64)
	t.Setenv("BACKPACK_SECRET_KEY", secB64)

	const fixedNow int64 = 1795000000123
	var verifiedSig bool

	bpSrv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodGet || r.URL.Path != "/api/v1/account" {
			http.NotFound(w, r)
			return
		}
		if r.Header.Get("X-API-Key") != pubB64 {
			http.Error(w, "bad key", http.StatusUnauthorized)
			return
		}
		ts := r.Header.Get("X-Timestamp")
		win := r.Header.Get("X-Window")
		if ts != strconv.FormatInt(fixedNow, 10) || win != "5000" {
			http.Error(w, "bad ts/window", http.StatusBadRequest)
			return
		}
		sigBytes, err := base64.StdEncoding.DecodeString(r.Header.Get("X-Signature"))
		if err != nil {
			http.Error(w, "bad sig encoding", http.StatusUnauthorized)
			return
		}
		msg := "instruction=accountQuery&timestamp=" + ts + "&window=" + win
		verifiedSig = ed25519.Verify(pub, []byte(msg), sigBytes)
		if !verifiedSig {
			http.Error(w, "invalid signature", http.StatusUnauthorized)
			return
		}
		// Include sensitive limit/settings fields that MUST be stripped from output.
		_ = json.NewEncoder(w).Encode(map[string]any{
			"autoBorrowSettlements": true,
			"autoLend":              false,
			"autoRealizePnl":        true,
			"borrowLimit":           "777777",
			"leverageLimit":         "10",
			"positionLimit":         "555555",
		})
	}))
	defer bpSrv.Close()

	report, err := runAudit(context.Background(), auditConfig{
		venues:                  []string{"backpack"},
		timeout:                 5 * time.Second,
		backpackURL:             bpSrv.URL,
		allowInsecureTestServer: true,
		nowMillis:               func() int64 { return fixedNow },
	}, nil)
	if err != nil {
		t.Fatalf("runAudit failed: %v", err)
	}
	if !verifiedSig {
		t.Fatalf("server did not verify Ed25519 signature")
	}

	bpRes := report.Venues["backpack"]
	if bpRes.Status != "ok_no_fee_fields_exposed" || bpRes.AccountFeeStatus != statusUnknown {
		t.Fatalf("unexpected backpack status: %+v", bpRes)
	}
	if !*bpRes.AuthenticatedProbeOK || *bpRes.FeeFieldsExposed {
		t.Fatalf("unexpected backpack flags: %+v", bpRes)
	}

	raw, _ := json.Marshal(report)
	text := string(raw)
	for _, forbidden := range []string{pubB64, secB64, "777777", "555555", "borrowLimit", "leverageLimit", "positionLimit"} {
		if strings.Contains(text, forbidden) {
			t.Fatalf("backpack output leaked %q", forbidden)
		}
	}
}

func TestBackpackExposesFeesAndLighterRejectsLeakInError(t *testing.T) {
	seed := bytes.Repeat([]byte{0x3b}, ed25519.SeedSize)
	priv := ed25519.NewKeyFromSeed(seed)
	pubB64 := base64.StdEncoding.EncodeToString(priv.Public().(ed25519.PublicKey))
	secB64 := base64.StdEncoding.EncodeToString(seed)

	t.Setenv("BACKPACK_PUBLIC_KEY", pubB64)
	t.Setenv("BACKPACK_SECRET_KEY", secB64)
	t.Setenv("LIGHTER_PRIVATE_KEY", test40ByteKeyHex(0x99))
	t.Setenv("LIGHTER_ACCOUNT_INDEX", "987654")
	t.Setenv("LIGHTER_API_KEY_INDEX", "5")
	t.Setenv("LIGHTER_CHAIN_ID", "304")

	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/api/v1/account" && r.Header.Get("X-API-Key") != "" {
			_ = json.NewEncoder(w).Encode(map[string]any{
				"feeTier":         "Tier 2",
				"spotMakerFee":    "7",
				"spotTakerFee":    "9",
				"futuresMakerFee": "1.9",
				"futuresTakerFee": "4.5",
			})
			return
		}
		http.Error(w, "denied", http.StatusUnauthorized)
	}))
	defer srv.Close()

	report, err := runAudit(context.Background(), auditConfig{
		venues:                  []string{"lighter", "backpack"},
		timeout:                 5 * time.Second,
		lighterCoreURL:          srv.URL,
		backpackURL:             srv.URL,
		allowInsecureTestServer: true,
	}, nil)
	if err != nil {
		t.Fatalf("runAudit failed: %v", err)
	}

	bp := report.Venues["backpack"]
	if bp.AccountFeeStatus != statusVerified || *bp.MakerFeeBps != "1.9" || *bp.TakerFeeBps != "4.5" {
		t.Fatalf("unexpected backpack parsed fees: %+v", bp)
	}
	if *bp.SpotMakerFeeBps != "7" || *bp.SpotTakerFeeBps != "9" || bp.FeeTier != "Tier 2" {
		t.Fatalf("unexpected backpack spot/tier fields: %+v", bp)
	}

	lgt := report.Venues["lighter"]
	if lgt.Status != "error" || lgt.AccountFeeStatus != statusUnknown {
		t.Fatalf("expected error status on 401 lighter, got %+v", lgt)
	}

	raw, _ := json.Marshal(report)
	if strings.Contains(string(raw), "987654") || strings.Contains(string(raw), pubB64) || strings.Contains(string(raw), secB64) {
		t.Fatalf("output leaked identifier/secret on error path")
	}
}

func TestRunCLIRejectsNonHTTPSAndSecretFlags(t *testing.T) {
	var out bytes.Buffer
	if err := runCLI([]string{"--venues", "lighter", "--private-key", "abc"}, &out); err == nil {
		t.Fatalf("expected error for unknown secret flag")
	}
	t.Setenv("LIGHTER_PRIVATE_KEY", test40ByteKeyHex(0x12))
	t.Setenv("LIGHTER_ACCOUNT_INDEX", "55555")
	t.Setenv("LIGHTER_API_KEY_INDEX", "1")
	t.Setenv("LIGHTER_BASE_URL", "http://insecure.example.com")
	out.Reset()
	if err := runCLI([]string{"--venues", "lighter"}, &out); err != nil {
		t.Fatalf("runCLI should isolate per-venue error into JSON: %v", err)
	}
	if !strings.Contains(out.String(), `"only HTTPS endpoints are allowed"`) {
		t.Fatalf("expected HTTPS rejection in output, got: %s", out.String())
	}
	if strings.Contains(out.String(), "55555") {
		t.Fatalf("output leaked account index")
	}
}
