// Strictly read-only authenticated fee-tier auditor for Lighter Core,
// Lighter Robinhood, and Backpack. Reads secrets solely from environment
// variables and emits strictly redacted fee-tier JSON.
package main

import (
	"context"
	"crypto/ed25519"
	"crypto/subtle"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"math/big"
	"net/http"
	"net/url"
	"os"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"time"

	"github.com/AlephTX/aleph-tx/feeder/exchanges"
)

const (
	defaultLighterCoreURL = "https://mainnet.zklighter.elliot.ai"
	defaultLighterRHURL   = "https://api.rh.lighter.xyz"
	defaultBackpackURL    = "https://api.backpack.exchange"
	maxResponseBytes      = 4 << 20

	statusVerified = "verified_account_tier"
	statusUnknown  = "unknown_requires_account_evidence"
)

var (
	evmAddrRe = regexp.MustCompile(`0x[0-9a-fA-F]{40}`)
	hex64Re   = regexp.MustCompile(`(?:^|[^0-9a-fA-F])(?:0x)?[0-9a-fA-F]{64}(?:[^0-9a-fA-F]|$)`)
)

type auditConfig struct {
	venues                  []string
	timeout                 time.Duration
	lighterCoreURL          string
	lighterRHURL            string
	backpackURL             string
	allowInsecureTestServer bool
	nowMillis               func() int64
}

type auditReport struct {
	SchemaVersion string                  `json:"schema_version"`
	Venues        map[string]*venueResult `json:"venues"`
}

type venueResult struct {
	Venue                  string   `json:"venue"`
	Settlement             string   `json:"settlement,omitempty"`
	Configured             bool     `json:"configured"`
	Status                 string   `json:"status"`
	AccountFeeStatus       string   `json:"account_fee_status"`
	SourceEndpoints        []string `json:"source_endpoints,omitempty"`
	MakerFeeBps            *string  `json:"maker_fee_bps"`
	TakerFeeBps            *string  `json:"taker_fee_bps"`
	PublicScheduleMakerBps *string  `json:"public_schedule_maker_fee_bps,omitempty"`
	PublicScheduleTakerBps *string  `json:"public_schedule_taker_fee_bps,omitempty"`
	CurrentMakerFeeTick    *int64   `json:"current_maker_fee_tick,omitempty"`
	CurrentTakerFeeTick    *int64   `json:"current_taker_fee_tick,omitempty"`
	AccountType            *int     `json:"account_type,omitempty"`
	AccountTypeLabel       string   `json:"account_type_label,omitempty"`
	UserTier               string   `json:"user_tier,omitempty"`
	UserTierName           string   `json:"user_tier_name,omitempty"`
	BoFeeRate              *string  `json:"bo_fee_rate,omitempty"`
	StandardZeroConfirmed  *bool    `json:"standard_zero_confirmed,omitempty"`
	CustomFeeOverrideFound *bool    `json:"custom_fee_override_detected,omitempty"`
	AuthenticatedProbeOK   *bool    `json:"authenticated_probe_ok,omitempty"`
	FeeFieldsExposed       *bool    `json:"fee_fields_exposed,omitempty"`
	FeeTier                string   `json:"fee_tier,omitempty"`
	SpotMakerFeeBps        *string  `json:"spot_maker_fee_bps,omitempty"`
	SpotTakerFeeBps        *string  `json:"spot_taker_fee_bps,omitempty"`
	FuturesMakerFeeBps     *string  `json:"futures_maker_fee_bps,omitempty"`
	FuturesTakerFeeBps     *string  `json:"futures_taker_fee_bps,omitempty"`
	Note                   string   `json:"note,omitempty"`
	Error                  string   `json:"error,omitempty"`
}

func strPtr(s string) *string { return &s }
func boolPtr(b bool) *bool    { return &b }

func feeTickToBpsString(tick int64) string {
	// Lighter fee ticks are scaled to 1e6 = 100%, so 100 ticks = 1 bps (0.01%).
	r := new(big.Rat).SetFrac64(tick, 100)
	return formatRat(r)
}

func formatRat(r *big.Rat) string {
	if r.Sign() == 0 {
		return "0"
	}
	s := r.FloatString(8)
	if strings.Contains(s, ".") {
		s = strings.TrimRight(s, "0")
		s = strings.TrimRight(s, ".")
	}
	if s == "-0" {
		return "0"
	}
	return s
}

func parseBpsString(raw any) (string, error) {
	if raw == nil {
		return "", errors.New("missing fee bps")
	}
	var text string
	switch v := raw.(type) {
	case string:
		text = strings.TrimSpace(v)
	case json.Number:
		text = strings.TrimSpace(v.String())
	case float64:
		text = strconv.FormatFloat(v, 'f', -1, 64)
	case int64:
		text = strconv.FormatInt(v, 10)
	default:
		return "", fmt.Errorf("unsupported fee bps type")
	}
	if text == "" {
		return "", errors.New("empty fee bps")
	}
	r, ok := new(big.Rat).SetString(text)
	if !ok {
		return "", fmt.Errorf("invalid fee bps")
	}
	return formatRat(r), nil
}

func validateBaseURL(raw string, allowInsecure bool) (string, error) {
	trimmed := strings.TrimRight(strings.TrimSpace(raw), "/")
	u, err := url.Parse(trimmed)
	if err != nil || u.Host == "" {
		return "", fmt.Errorf("invalid base URL")
	}
	if u.Scheme != "https" && !(allowInsecure && u.Scheme == "http") {
		return "", fmt.Errorf("only HTTPS endpoints are allowed")
	}
	return trimmed, nil
}

func newHTTPClient(timeout time.Duration) *http.Client {
	return &http.Client{
		Timeout: timeout,
		CheckRedirect: func(*http.Request, []*http.Request) error {
			return http.ErrUseLastResponse
		},
	}
}

func sanitizeError(err error, forbidden []string) string {
	if err == nil {
		return ""
	}
	msg := err.Error()
	msg = evmAddrRe.ReplaceAllString(msg, "<redacted_evm>")
	msg = hex64Re.ReplaceAllString(msg, "<redacted_hex>")
	for _, lit := range forbidden {
		t := strings.TrimSpace(lit)
		if len(t) >= 2 {
			msg = strings.ReplaceAll(msg, t, "<redacted>")
		}
	}
	return msg
}

func collectSecretLiterals() []string {
	var lits []string
	for _, k := range []string{
		"LIGHTER_PRIVATE_KEY",
		"API_KEY_PRIVATE_KEY",
		"LIGHTER_ACCOUNT_INDEX",
		"LIGHTER_API_KEY_INDEX",
		"LIGHTER_BASE_URL",
		"LIGHTER_CHAIN_ID",
		"LIGHTER_RH_PRIVATE_KEY",
		"LIGHTER_RH_ACCOUNT_INDEX",
		"LIGHTER_RH_API_KEY_INDEX",
		"LIGHTER_RH_BASE_URL",
		"LIGHTER_RH_CHAIN_ID",
		"BACKPACK_PUBLIC_KEY",
		"BACKPACK_SECRET_KEY",
		"BACKPACK_BASE_URL",
	} {
		if v := strings.TrimSpace(os.Getenv(k)); v != "" {
			lits = append(lits, v)
		}
	}
	return lits
}

func assertNoSecretLeak(serialized string, forbidden []string) error {
	if evmAddrRe.MatchString(serialized) {
		return errors.New("redaction guard rejected EVM address in output")
	}
	if hex64Re.MatchString(serialized) {
		return errors.New("redaction guard rejected 64-hex secret in output")
	}
	lower := strings.ToLower(serialized)
	for _, raw := range forbidden {
		lit := strings.TrimSpace(raw)
		if lit == "" {
			continue
		}
		isDigits := true
		for _, r := range lit {
			if r < '0' || r > '9' {
				isDigits = false
				break
			}
		}
		if isDigits && len(lit) < 5 {
			continue
		}
		if len(lit) >= 2 && strings.Contains(lower, strings.ToLower(lit)) {
			return errors.New("redaction guard rejected forbidden env literal in output")
		}
	}
	return nil
}

func fetchJSONMap(ctx context.Context, client *http.Client, fullURL string, headers map[string]string) (map[string]any, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, fullURL, nil)
	if err != nil {
		return nil, fmt.Errorf("build request failed")
	}
	req.Header.Set("Accept", "application/json")
	req.Header.Set("User-Agent", "AlephTX-FeeAudit/1.0")
	for k, v := range headers {
		req.Header.Set(k, v)
	}
	resp, err := client.Do(req)
	if err != nil {
		return nil, fmt.Errorf("transport error: %w", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("HTTP %d", resp.StatusCode)
	}
	dec := json.NewDecoder(io.LimitReader(resp.Body, maxResponseBytes))
	dec.UseNumber()
	var payload map[string]any
	if err := dec.Decode(&payload); err != nil {
		return nil, fmt.Errorf("invalid JSON response")
	}
	return payload, nil
}

func jsonInt64(v any) (int64, bool) {
	switch n := v.(type) {
	case json.Number:
		parsed, err := n.Int64()
		return parsed, err == nil
	case float64:
		if n != float64(int64(n)) {
			return 0, false
		}
		return int64(n), true
	case string:
		parsed, err := strconv.ParseInt(strings.TrimSpace(n), 10, 64)
		return parsed, err == nil
	default:
		return 0, false
	}
}

func jsonString(v any) string {
	switch s := v.(type) {
	case string:
		return strings.TrimSpace(s)
	case json.Number:
		return s.String()
	case float64:
		return strconv.FormatFloat(s, 'f', -1, 64)
	default:
		return ""
	}
}

func isLighterConfigured(prefix string) bool {
	if strings.TrimSpace(os.Getenv(prefix+"PRIVATE_KEY")) != "" ||
		strings.TrimSpace(os.Getenv(prefix+"ACCOUNT_INDEX")) != "" ||
		strings.TrimSpace(os.Getenv(prefix+"API_KEY_INDEX")) != "" {
		return true
	}
	if prefix == "LIGHTER_" && strings.TrimSpace(os.Getenv("API_KEY_PRIVATE_KEY")) != "" {
		return true
	}
	return false
}

func probeLighterVenue(
	ctx context.Context,
	client *http.Client,
	venue string,
	settlement string,
	prefix string,
	rawBaseURL string,
	allowInsecure bool,
	forbidden *[]string,
) *venueResult {
	if !isLighterConfigured(prefix) {
		return &venueResult{
			Venue:            venue,
			Settlement:       settlement,
			Configured:       false,
			Status:           "unconfigured",
			AccountFeeStatus: statusUnknown,
		}
	}

	fail := func(err error) *venueResult {
		return &venueResult{
			Venue:            venue,
			Settlement:       settlement,
			Configured:       true,
			Status:           "error",
			AccountFeeStatus: statusUnknown,
			Error:            sanitizeError(err, *forbidden),
		}
	}

	baseURL, err := validateBaseURL(rawBaseURL, allowInsecure)
	if err != nil {
		return fail(err)
	}

	auth, err := exchanges.LoadLighterAuthFromEnvPrefix(prefix)
	if err != nil {
		return fail(err)
	}
	token, err := auth.CreateAuthToken()
	if err != nil {
		return fail(err)
	}
	*forbidden = append(*forbidden, token, strconv.FormatInt(auth.GetAccountIndex(), 10))

	acctIdxStr := strconv.FormatInt(auth.GetAccountIndex(), 10)
	limitsURL := fmt.Sprintf("%s/api/v1/accountLimits?account_index=%s", baseURL, url.QueryEscape(acctIdxStr))
	limitsMap, err := fetchJSONMap(ctx, client, limitsURL, map[string]string{
		"Authorization": token,
	})
	if err != nil {
		return fail(fmt.Errorf("accountLimits query failed: %w", err))
	}
	if code, ok := jsonInt64(limitsMap["code"]); !ok || code != 200 {
		return fail(fmt.Errorf("accountLimits returned code=%v", limitsMap["code"]))
	}

	makerTick, okMaker := jsonInt64(limitsMap["current_maker_fee_tick"])
	takerTick, okTaker := jsonInt64(limitsMap["current_taker_fee_tick"])
	if !okMaker || !okTaker {
		return fail(errors.New("accountLimits missing current_maker_fee_tick or current_taker_fee_tick"))
	}

	// Query /api/v1/account to verify account_type (0 = Standard) and detect any account-level fee fields.
	accountURL := fmt.Sprintf("%s/api/v1/account?by=index&value=%s", baseURL, url.QueryEscape(acctIdxStr))
	acctMap, err := fetchJSONMap(ctx, client, accountURL, nil)
	if err != nil {
		return fail(fmt.Errorf("account query failed: %w", err))
	}
	if code, ok := jsonInt64(acctMap["code"]); !ok || code != 200 {
		return fail(fmt.Errorf("account returned code=%v", acctMap["code"]))
	}

	rawAccounts, ok := acctMap["accounts"].([]any)
	if !ok {
		return fail(errors.New("account response missing accounts list"))
	}
	var matchedAcct map[string]any
	matchCount := 0
	for _, item := range rawAccounts {
		m, isMap := item.(map[string]any)
		if !isMap {
			continue
		}
		if l1, ok := m["l1_address"].(string); ok && strings.TrimSpace(l1) != "" {
			*forbidden = append(*forbidden, strings.TrimSpace(l1))
		}
		if idx, ok := jsonInt64(m["index"]); ok && idx == auth.GetAccountIndex() {
			matchedAcct = m
			matchCount++
		}
	}
	if matchCount != 1 || matchedAcct == nil {
		return fail(errors.New("account response missing unique matching account"))
	}

	var accountTypePtr *int
	accountTypeLabel := ""
	if rawAT, exists := matchedAcct["account_type"]; exists && rawAT != nil {
		atVal, ok := jsonInt64(rawAT)
		if !ok {
			return fail(errors.New("invalid account_type in account response"))
		}
		atInt := int(atVal)
		accountTypePtr = &atInt
		if atInt == 0 {
			accountTypeLabel = "standard_type_0"
		} else {
			accountTypeLabel = fmt.Sprintf("account_type_%d", atInt)
		}
	}

	// Check if accountLimits or account carries any non-zero custom fee override fields.
	extraOverride := false
	for k, v := range limitsMap {
		kl := strings.ToLower(k)
		if kl == "current_maker_fee_tick" || kl == "current_taker_fee_tick" || kl == "bo_fee_rate" {
			continue
		}
		if (strings.Contains(kl, "fee") || strings.Contains(kl, "maker") || strings.Contains(kl, "taker")) && v != nil {
			if n, ok := jsonInt64(v); ok && n != 0 {
				extraOverride = true
			}
		}
	}
	for _, fk := range []string{"maker_fee_bps", "taker_fee_bps", "maker_fee", "taker_fee"} {
		if v, exists := matchedAcct[fk]; exists && v != nil {
			if text := jsonString(v); text != "" && text != "0" && text != "0.0" {
				extraOverride = true
			}
		}
	}

	makerBps := feeTickToBpsString(makerTick)
	takerBps := feeTickToBpsString(takerTick)
	stdZero := accountTypePtr != nil && *accountTypePtr == 0 && makerTick == 0 && takerTick == 0 && !extraOverride
	customOverride := (accountTypePtr != nil && *accountTypePtr == 0 && (makerTick != 0 || takerTick != 0)) || extraOverride

	userTier := jsonString(limitsMap["user_tier"])
	userTierName := jsonString(limitsMap["user_tier_name"])
	var boFeeRatePtr *string
	if bo := jsonString(limitsMap["bo_fee_rate"]); bo != "" {
		boFeeRatePtr = strPtr(bo)
	}

	note := "Verified via read-only signed GET /api/v1/accountLimits + GET /api/v1/account."
	if stdZero {
		note = "Confirmed Standard (account_type == 0) with 0/0 fee ticks in signed /api/v1/accountLimits."
	} else if customOverride {
		note = "Non-zero fee ticks or fee override detected in signed /api/v1/accountLimits despite account_type == 0."
	}

	return &venueResult{
		Venue:                  venue,
		Settlement:             settlement,
		Configured:             true,
		Status:                 "ok",
		AccountFeeStatus:       statusVerified,
		SourceEndpoints:        []string{"GET /api/v1/accountLimits", "GET /api/v1/account"},
		MakerFeeBps:            strPtr(makerBps),
		TakerFeeBps:            strPtr(takerBps),
		PublicScheduleMakerBps: strPtr("0"),
		PublicScheduleTakerBps: strPtr("0"),
		CurrentMakerFeeTick:    &makerTick,
		CurrentTakerFeeTick:    &takerTick,
		AccountType:            accountTypePtr,
		AccountTypeLabel:       accountTypeLabel,
		UserTier:               userTier,
		UserTierName:           userTierName,
		BoFeeRate:              boFeeRatePtr,
		StandardZeroConfirmed:  boolPtr(stdZero),
		CustomFeeOverrideFound: boolPtr(customOverride),
		AuthenticatedProbeOK:   boolPtr(true),
		Note:                   note,
	}
}

func decodeKeyBytes(raw string) ([]byte, error) {
	trimmed := strings.TrimSpace(raw)
	if trimmed == "" {
		return nil, errors.New("empty key")
	}
	hexCandidate := strings.TrimPrefix(trimmed, "0x")
	if len(hexCandidate) == 64 || len(hexCandidate) == 128 {
		if b, err := hex.DecodeString(hexCandidate); err == nil {
			return b, nil
		}
	}
	for _, enc := range []*base64.Encoding{
		base64.StdEncoding,
		base64.RawStdEncoding,
		base64.URLEncoding,
		base64.RawURLEncoding,
	} {
		if b, err := enc.DecodeString(trimmed); err == nil {
			return b, nil
		}
	}
	return nil, errors.New("unsupported key encoding")
}

func parseBackpackKeys(pubRaw, secRaw string) (ed25519.PrivateKey, string, error) {
	secBytes, err := decodeKeyBytes(secRaw)
	if err != nil {
		return nil, "", fmt.Errorf("invalid BACKPACK_SECRET_KEY encoding")
	}
	defer clear(secBytes)

	var priv ed25519.PrivateKey
	switch len(secBytes) {
	case ed25519.SeedSize:
		priv = ed25519.NewKeyFromSeed(secBytes)
	case ed25519.PrivateKeySize:
		privCopy := make([]byte, ed25519.PrivateKeySize)
		copy(privCopy, secBytes)
		priv = ed25519.PrivateKey(privCopy)
	default:
		return nil, "", fmt.Errorf("BACKPACK_SECRET_KEY must decode to 32 or 64 bytes, got %d", len(secBytes))
	}

	derivedPub := priv.Public().(ed25519.PublicKey)
	if strings.TrimSpace(pubRaw) != "" {
		pubBytes, err := decodeKeyBytes(pubRaw)
		if err != nil || len(pubBytes) != ed25519.PublicKeySize {
			clear(priv)
			return nil, "", errors.New("invalid BACKPACK_PUBLIC_KEY")
		}
		if subtle.ConstantTimeCompare(derivedPub, pubBytes) != 1 {
			clear(priv)
			return nil, "", errors.New("BACKPACK_PUBLIC_KEY does not match BACKPACK_SECRET_KEY")
		}
	}
	return priv, base64.StdEncoding.EncodeToString(derivedPub), nil
}

func probeBackpack(
	ctx context.Context,
	client *http.Client,
	rawBaseURL string,
	allowInsecure bool,
	nowMillis func() int64,
	forbidden *[]string,
) *venueResult {
	pubRaw := strings.TrimSpace(os.Getenv("BACKPACK_PUBLIC_KEY"))
	secRaw := strings.TrimSpace(os.Getenv("BACKPACK_SECRET_KEY"))
	if pubRaw == "" && secRaw == "" {
		return &venueResult{
			Venue:            "backpack",
			Settlement:       "USDC",
			Configured:       false,
			Status:           "unconfigured",
			AccountFeeStatus: statusUnknown,
		}
	}

	fail := func(err error) *venueResult {
		return &venueResult{
			Venue:            "backpack",
			Settlement:       "USDC",
			Configured:       true,
			Status:           "error",
			AccountFeeStatus: statusUnknown,
			Error:            sanitizeError(err, *forbidden),
		}
	}

	if secRaw == "" {
		return fail(errors.New("BACKPACK_SECRET_KEY not set in environment"))
	}
	baseURL, err := validateBaseURL(rawBaseURL, allowInsecure)
	if err != nil {
		return fail(err)
	}

	priv, pubB64, err := parseBackpackKeys(pubRaw, secRaw)
	if err != nil {
		return fail(err)
	}
	defer clear(priv)
	*forbidden = append(*forbidden, pubB64)

	ts := strconv.FormatInt(nowMillis(), 10)
	const window = "5000"
	signingPayload := fmt.Sprintf("instruction=accountQuery&timestamp=%s&window=%s", ts, window)
	sig := ed25519.Sign(priv, []byte(signingPayload))
	sigB64 := base64.StdEncoding.EncodeToString(sig)
	*forbidden = append(*forbidden, sigB64)

	accountURL := baseURL + "/api/v1/account"
	respMap, err := fetchJSONMap(ctx, client, accountURL, map[string]string{
		"X-API-Key":   pubB64,
		"X-Signature": sigB64,
		"X-Timestamp": ts,
		"X-Window":    window,
	})
	if err != nil {
		return fail(fmt.Errorf("Backpack accountQuery failed: %w", err))
	}

	// Only inspect explicit non-sensitive fee/tier fields; drop all limits/balances/IDs.
	res := &venueResult{
		Venue:                "backpack",
		Settlement:           "USDC",
		Configured:           true,
		SourceEndpoints:      []string{"GET /api/v1/account (instruction=accountQuery)"},
		AuthenticatedProbeOK: boolPtr(true),
	}

	if ft := jsonString(respMap["feeTier"]); ft != "" {
		res.FeeTier = ft
	}
	parsePair := func(mkKey, tkKey string) (*string, *string, error) {
		mkRaw, hasMk := respMap[mkKey]
		tkRaw, hasTk := respMap[tkKey]
		if !hasMk && !hasTk {
			return nil, nil, nil
		}
		if !hasMk || !hasTk || mkRaw == nil || tkRaw == nil {
			return nil, nil, fmt.Errorf("partial fee pair %s/%s", mkKey, tkKey)
		}
		mkBps, err := parseBpsString(mkRaw)
		if err != nil {
			return nil, nil, err
		}
		tkBps, err := parseBpsString(tkRaw)
		if err != nil {
			return nil, nil, err
		}
		return strPtr(mkBps), strPtr(tkBps), nil
	}

	spotMk, spotTk, err := parsePair("spotMakerFee", "spotTakerFee")
	if err != nil {
		return fail(err)
	}
	futMk, futTk, err := parsePair("futuresMakerFee", "futuresTakerFee")
	if err != nil {
		return fail(err)
	}
	genMk, genTk, err := parsePair("makerFee", "takerFee")
	if err != nil {
		return fail(err)
	}

	res.SpotMakerFeeBps = spotMk
	res.SpotTakerFeeBps = spotTk
	res.FuturesMakerFeeBps = futMk
	res.FuturesTakerFeeBps = futTk

	if futMk != nil && futTk != nil {
		res.MakerFeeBps = futMk
		res.TakerFeeBps = futTk
	} else if genMk != nil && genTk != nil {
		res.MakerFeeBps = genMk
		res.TakerFeeBps = genTk
	}

	if res.MakerFeeBps != nil && res.TakerFeeBps != nil {
		res.Status = "ok"
		res.AccountFeeStatus = statusVerified
		res.FeeFieldsExposed = boolPtr(true)
		res.Note = "Verified via signed Ed25519 GET /api/v1/account (instruction=accountQuery)."
	} else {
		res.Status = "ok_no_fee_fields_exposed"
		res.AccountFeeStatus = statusUnknown
		res.FeeFieldsExposed = boolPtr(false)
		res.Note = "Authenticated Ed25519 GET /api/v1/account (instruction=accountQuery) succeeded, but endpoint exposes account settings only and omits fee-tier rates."
	}
	return res
}

func parseVenuesFlag(raw string) ([]string, error) {
	supported := map[string]bool{
		"lighter":           true,
		"lighter_robinhood": true,
		"backpack":          true,
	}
	if strings.TrimSpace(raw) == "" {
		return []string{"lighter", "lighter_robinhood", "backpack"}, nil
	}
	seen := make(map[string]bool)
	var out []string
	for _, part := range strings.Split(raw, ",") {
		v := strings.ToLower(strings.TrimSpace(part))
		if v == "" {
			continue
		}
		if !supported[v] {
			return nil, fmt.Errorf("unsupported venue %q", v)
		}
		if !seen[v] {
			seen[v] = true
			out = append(out, v)
		}
	}
	if len(out) == 0 {
		return nil, errors.New("--venues must specify at least one venue")
	}
	return out, nil
}

func runAudit(ctx context.Context, cfg auditConfig, client *http.Client) (*auditReport, error) {
	if client == nil {
		client = newHTTPClient(cfg.timeout)
	}
	nowMs := cfg.nowMillis
	if nowMs == nil {
		nowMs = func() int64 { return time.Now().UnixMilli() }
	}
	forbidden := collectSecretLiterals()
	results := make(map[string]*venueResult, len(cfg.venues))

	for _, venue := range cfg.venues {
		switch venue {
		case "lighter":
			baseURL := strings.TrimSpace(cfg.lighterCoreURL)
			if baseURL == "" {
				baseURL = strings.TrimSpace(os.Getenv("LIGHTER_BASE_URL"))
			}
			if baseURL == "" {
				baseURL = defaultLighterCoreURL
			}
			results[venue] = probeLighterVenue(ctx, client, "lighter", "USDC", "LIGHTER_", baseURL, cfg.allowInsecureTestServer, &forbidden)
		case "lighter_robinhood":
			baseURL := strings.TrimSpace(cfg.lighterRHURL)
			if baseURL == "" {
				baseURL = strings.TrimSpace(os.Getenv("LIGHTER_RH_BASE_URL"))
			}
			if baseURL == "" {
				baseURL = defaultLighterRHURL
			}
			results[venue] = probeLighterVenue(ctx, client, "lighter_robinhood", "USDG", "LIGHTER_RH_", baseURL, cfg.allowInsecureTestServer, &forbidden)
		case "backpack":
			baseURL := strings.TrimSpace(cfg.backpackURL)
			if baseURL == "" {
				baseURL = strings.TrimSpace(os.Getenv("BACKPACK_BASE_URL"))
			}
			if baseURL == "" {
				baseURL = defaultBackpackURL
			}
			results[venue] = probeBackpack(ctx, client, baseURL, cfg.allowInsecureTestServer, nowMs, &forbidden)
		default:
			return nil, fmt.Errorf("unsupported venue %q", venue)
		}
	}

	report := &auditReport{
		SchemaVersion: "aleph_go_fee_audit_v1",
		Venues:        results,
	}
	rawBytes, err := json.Marshal(report)
	if err != nil {
		return nil, fmt.Errorf("marshal report failed")
	}
	if err := assertNoSecretLeak(string(rawBytes), forbidden); err != nil {
		return nil, err
	}
	return report, nil
}

func runCLI(args []string, stdout io.Writer) error {
	fs := flag.NewFlagSet("fee-audit", flag.ContinueOnError)
	fs.SetOutput(io.Discard)
	venuesFlag := fs.String("venues", "lighter,lighter_robinhood,backpack", "Comma-separated venues")
	timeoutFlag := fs.Duration("timeout", 15*time.Second, "HTTP request timeout")
	if err := fs.Parse(args); err != nil {
		return err
	}
	if fs.NArg() != 0 {
		keys := make([]string, 0, fs.NArg())
		for _, a := range fs.Args() {
			keys = append(keys, a)
		}
		sort.Strings(keys)
		return fmt.Errorf("unexpected positional arguments (count=%d)", len(keys))
	}
	venues, err := parseVenuesFlag(*venuesFlag)
	if err != nil {
		return err
	}
	if *timeoutFlag <= 0 {
		return errors.New("--timeout must be positive")
	}

	ctx, cancel := context.WithTimeout(context.Background(), *timeoutFlag*time.Duration(len(venues)+1))
	defer cancel()

	report, err := runAudit(ctx, auditConfig{
		venues:  venues,
		timeout: *timeoutFlag,
	}, nil)
	if err != nil {
		return err
	}
	enc := json.NewEncoder(stdout)
	enc.SetIndent("", "  ")
	return enc.Encode(report)
}

func main() {
	if err := runCLI(os.Args[1:], os.Stdout); err != nil {
		fmt.Fprintf(os.Stderr, "fee-audit: %s\n", sanitizeError(err, collectSecretLiterals()))
		os.Exit(1)
	}
}
