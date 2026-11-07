package exchanges

import (
	"encoding/hex"
	"fmt"
	"os"
	"strconv"
	"strings"
	"time"

	"github.com/elliottech/lighter-go/signer"
	"github.com/elliottech/lighter-go/types"
)

// LighterAuth handles Lighter authentication using Poseidon2 + Schnorr signatures
type LighterAuth struct {
	keyManager     signer.KeyManager
	accountIndex   int64
	apiKeyIndex    uint8
	authExpiry     time.Duration
	lastAuthToken  string
	lastAuthExpiry time.Time
}

// LoadLighterAuthFromEnv loads authentication credentials from .env.lighter
func LoadLighterAuthFromEnv() (*LighterAuth, error) {
	// Load private key (40 bytes hex)
	privKeyHex := os.Getenv("LIGHTER_PRIVATE_KEY")
	if privKeyHex == "" {
		privKeyHex = os.Getenv("API_KEY_PRIVATE_KEY")
	}
	privKeyBytes, err := hex.DecodeString(strings.TrimPrefix(privKeyHex, "0x"))
	if err != nil {
		return nil, fmt.Errorf("invalid Lighter private key encoding")
	}

	if len(privKeyBytes) != 40 {
		return nil, fmt.Errorf("API_KEY_PRIVATE_KEY must be 40 bytes, got %d", len(privKeyBytes))
	}

	// Create key manager
	keyManager, err := signer.NewKeyManager(privKeyBytes)
	if err != nil {
		return nil, fmt.Errorf("failed to create key manager: %w", err)
	}

	// Load account index
	accountIndexStr := os.Getenv("LIGHTER_ACCOUNT_INDEX")
	if accountIndexStr == "" {
		return nil, fmt.Errorf("LIGHTER_ACCOUNT_INDEX not set in environment")
	}
	accountIndex, err := strconv.ParseInt(accountIndexStr, 10, 64)
	if err != nil {
		return nil, fmt.Errorf("invalid LIGHTER_ACCOUNT_INDEX: %w", err)
	}
	if accountIndex <= 0 {
		return nil, fmt.Errorf("LIGHTER_ACCOUNT_INDEX must be positive")
	}

	// Load API key index
	apiKeyIndexStr := os.Getenv("LIGHTER_API_KEY_INDEX")
	if apiKeyIndexStr == "" {
		return nil, fmt.Errorf("LIGHTER_API_KEY_INDEX not set in environment")
	}
	apiKeyIndexInt, err := strconv.Atoi(apiKeyIndexStr)
	if err != nil {
		return nil, fmt.Errorf("invalid LIGHTER_API_KEY_INDEX: %w", err)
	}
	if apiKeyIndexInt < 0 || apiKeyIndexInt > 254 {
		return nil, fmt.Errorf("LIGHTER_API_KEY_INDEX is outside the signing range")
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
