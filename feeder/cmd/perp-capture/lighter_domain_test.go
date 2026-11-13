package main

import (
	"testing"
	"time"
)

func TestRobinhoodDomainHasIndependentSources(t *testing.T) {
	sources, err := robinhoodSubscriptions("1,0", true, 4*time.Minute)
	if err != nil {
		t.Fatal(err)
	}
	if len(sources) != 2 {
		t.Fatalf("sources=%d", len(sources))
	}
	if sources[0].source != "lighter_robinhood" || sources[0].url != "wss://api.rh.lighter.xyz/stream" || sources[0].clockRefresh != 4*time.Minute {
		t.Fatal("wrong independent domain")
	}
	if sources[1].source != "lighter_robinhood_trades" || len(sources[1].requests) != 2 {
		t.Fatal("missing public trade channels")
	}
	if _, err := robinhoodSubscriptions("1,1", false, 0); err == nil {
		t.Fatal("duplicate market accepted")
	}
	core, err := subscriptions("1", "", "", "", "", "", "", 0, 0, false)
	if err != nil || core[0].source != "lighter" || core[0].url != "wss://mainnet.zklighter.elliot.ai/stream" {
		t.Fatal("core compatibility changed")
	}
}
