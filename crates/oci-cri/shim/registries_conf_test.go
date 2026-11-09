// SPDX-License-Identifier: Apache-2.0

package main

import (
	"strings"
	"testing"
)

func TestInsecureRegistriesConfPrefixes(t *testing.T) {
	text := string(insecureRegistriesConf())
	for _, prefix := range []string{"localhost", "127.0.0.1", "oci-registry", "10.", "192.168.", "172."} {
		wantPrefix := "prefix = \"" + prefix + "\""
		wantLocation := "location = \"" + prefix + "\""
		if !strings.Contains(text, wantPrefix) {
			t.Fatalf("missing %s\n%s", wantPrefix, text)
		}
		if !strings.Contains(text, wantLocation) {
			t.Fatalf("missing %s\n%s", wantLocation, text)
		}
	}
	if strings.Contains(text, "prefix = \"\"") || strings.Contains(text, "location = \"\"") {
		t.Fatal("registry block has an empty prefix or location")
	}
	if strings.Count(text, "insecure = true") != 6 {
		t.Fatalf("expected 6 insecure entries, got %d", strings.Count(text, "insecure = true"))
	}
}
