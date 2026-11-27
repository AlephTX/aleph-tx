package exchanges

import (
	"encoding/hex"
	"fmt"
	"os"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/elliottech/lighter-go/signer"
	"github.com/elliottech/lighter-go/types"
)

// LighterAuth handles Lighter authentication using Poseidon2 + Schnorr signatures
type LighterAuth struct {
	mu             sync.Mutex
	keyManager     signer.KeyManager
	accountIndex   int64
	apiKeyIndex    uint8
	authExpiry     time.Duration
	lastAuthToken  string
	lastAuthExpiry time.Time
}

// LoadLighterAuthFromEnv loads authentication credentials from LIGHTER_* env vars.
func LoadLighterAuthFromEnv() (*LighterAuth, error) {
	return LoadLighterAuthFromEnvPrefix("LIGHTER_")
}

// LoadLighterAuthFromEnvPrefix loads Lighter authentication credentials using
// `<prefix>PRIVATE_KEY`, `<prefix>ACCOUNT_INDEX`, `<prefix>API_KEY_INDEX`, and
// `<prefix>CHAIN_ID` (with `API_KEY_PRIVATE_KEY` fallback when prefix is `LIGHTER_`).
func LoadLighterAuthFromEnvPrefix(prefix string) (*LighterAuth, error) {
	privKeyHex := strings.TrimSpace(os.Getenv(prefix + "PRIVATE_KEY"))
	if privKeyHex == "" && prefix == "LIGHTER_" {
		privKeyHex = strings.TrimSpace(os.Getenv("API_KEY_PRIVATE_KEY"))
	}
	if privKeyHex == "" {
		return nil, fmt.Errorf("%sPRIVATE_KEY not set in environment", prefix)
	}
	privKeyBytes, err := hex.DecodeString(strings.TrimPrefix(privKeyHex, "0x"))
	if err != nil {
		return nil, fmt.Errorf("invalid Lighter private key encoding")
	}
	defer clear(privKeyBytes)

	if len(privKeyBytes) != 40 {
		return nil, fmt.Errorf("%sPRIVATE_KEY must be 40 bytes, got %d", prefix, len(privKeyBytes))
	}

	keyManager, err := signer.NewKeyManager(privKeyBytes)
	if err != nil {
		return nil, fmt.Errorf("failed to create key manager: %w", err)
	}

	accountIndexStr := strings.TrimSpace(os.Getenv(prefix + "ACCOUNT_INDEX"))
	if accountIndexStr == "" {
		return nil, fmt.Errorf("%sACCOUNT_INDEX not set in environment", prefix)
	}
	accountIndex, err := strconv.ParseInt(accountIndexStr, 10, 64)
	if err != nil {
		return nil, fmt.Errorf("invalid %sACCOUNT_INDEX: %w", prefix, err)
	}
	if accountIndex <= 0 {
		return nil, fmt.Errorf("%sACCOUNT_INDEX must be positive", prefix)
	}

	apiKeyIndexStr := strings.TrimSpace(os.Getenv(prefix + "API_KEY_INDEX"))
	if apiKeyIndexStr == "" {
		return nil, fmt.Errorf("%sAPI_KEY_INDEX not set in environment", prefix)
	}
	apiKeyIndexInt, err := strconv.Atoi(apiKeyIndexStr)
	if err != nil {
		return nil, fmt.Errorf("invalid %sAPI_KEY_INDEX: %w", prefix, err)
	}
	if apiKeyIndexInt < 0 || apiKeyIndexInt > 254 {
		return nil, fmt.Errorf("%sAPI_KEY_INDEX is outside the signing range", prefix)
	}
	if strings.TrimSpace(os.Getenv(prefix+"CHAIN_ID")) == "466324" && apiKeyIndexInt == 157 {
		return nil, fmt.Errorf("%sAPI_KEY_INDEX 157 is reserved on Lighter Robinhood Chain", prefix)
	}
	apiKeyIndex := uint8(apiKeyIndexInt)

	return &LighterAuth{
		keyManager:   keyManager,
		accountIndex: accountIndex,
		apiKeyIndex:  apiKeyIndex,
		authExpiry:   10 * time.Minute, // Default 10 minutes
	}, nil
}

// CreateAuthToken generates a new authentication token
// Format: "{deadline_unix}:{account_index}:{api_key_index}:{signature_hex}"
func (la *LighterAuth) CreateAuthToken() (string, error) {
	la.mu.Lock()
	defer la.mu.Unlock()

	// Reuse cached token if still valid (with 1 minute buffer)
	if time.Now().Before(la.lastAuthExpiry.Add(-1 * time.Minute)) {
		return la.lastAuthToken, nil
	}

	deadline := time.Now().Add(la.authExpiry)
	authToken, err := types.ConstructAuthToken(la.keyManager, deadline, &types.TransactOpts{
		FromAccountIndex: &la.accountIndex,
		ApiKeyIndex:      &la.apiKeyIndex,
	})
	if err != nil {
		return "", fmt.Errorf("failed to sign Lighter auth token: %w", err)
	}

	// Cache the token
	la.lastAuthToken = authToken
	la.lastAuthExpiry = deadline

	return authToken, nil
}

// GetAccountIndex returns the account index
func (la *LighterAuth) GetAccountIndex() int64 {
	return la.accountIndex
}

// GetAPIKeyIndex returns the API key index
func (la *LighterAuth) GetAPIKeyIndex() uint8 {
	return la.apiKeyIndex
}
