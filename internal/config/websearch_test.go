package config

import (
	"os"
	"path/filepath"
	"testing"
)

func TestWebSearchProviderValidation(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "config.toml")
	if err := os.WriteFile(p, []byte("[websearch]\nprovider = \"bogus\"\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if _, err := Load(p); err == nil {
		t.Fatal("expected unknown provider error")
	}
	if err := os.WriteFile(p, []byte("[websearch]\nprovider = \"tavily\"\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	c, err := Load(p)
	if err != nil {
		t.Fatal(err)
	}
	if c.WebSearch.Provider != "tavily" {
		t.Fatalf("provider=%q", c.WebSearch.Provider)
	}
}
