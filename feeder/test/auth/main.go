package main

import (
	"fmt"
	"log"
	"os"

	"github.com/AlephTX/aleph-tx/feeder/exchanges"
	"github.com/joho/godotenv"
)

func main() {
	// Load .env.lighter
	if err := godotenv.Load("../.env.lighter"); err != nil {
		log.Printf("Warning: .env.lighter not found, using environment variables")
	}

	// Create auth
	auth, err := exchanges.LoadLighterAuthFromEnv()
	if err != nil {
		log.Fatalf("Failed to load Lighter auth: %v", err)
	}

	fmt.Printf("✓ Loaded Lighter credentials\n")
	fmt.Printf("  Account Index: %d\n", auth.GetAccountIndex())
	fmt.Printf("  API Key Index: %d\n", auth.GetAPIKeyIndex())

	// Generate auth token
	token, err := auth.CreateAuthToken()
	if err != nil {
		log.Fatalf("Failed to create auth token: %v", err)
	}

	fmt.Printf("\n✓ Generated auth token:\n")
	fmt.Printf("  %s\n", token)
	fmt.Printf("\n✓ Token length: %d bytes\n", len(token))

	// Test token caching
	token2, err := auth.CreateAuthToken()
	if err != nil {
		log.Fatalf("Failed to create second auth token: %v", err)
	}

	if token == token2 {
		fmt.Printf("\n✓ Token caching works (same token returned)\n")
	} else {
		fmt.Printf("\n✗ Token caching failed (different tokens)\n")
		os.Exit(1)
	}

	fmt.Printf("\n✓ All authentication tests passed!\n")
}
