package ai

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"

	"ait/internal/config"
)

func TestGenerateCommand_Success(t *testing.T) {
	// Create a mock API server
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		// Verify request headers
		if r.Header.Get("Content-Type") != "application/json" {
			t.Error("expected Content-Type: application/json")
		}
		if r.Header.Get("Authorization") != "Bearer test-token" {
			t.Error("unexpected Authorization header")
		}

		resp := map[string]interface{}{
			"choices": []map[string]interface{}{
				{
					"message": map[string]string{
						"content": "ls -la",
					},
				},
			},
		}
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(resp)
	}))
	defer server.Close()

	cfg := &config.Config{
		APIEndpoint: server.URL,
		APIToken:    "test-token",
		Model:       "gpt-4o-mini",
		Shell:       "bash",
	}

	client := NewClient(cfg)
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	cmd, err := client.GenerateCommand(ctx, "list all files", "")
	if err != nil {
		t.Fatalf("GenerateCommand failed: %v", err)
	}

	if cmd != "ls -la" {
		t.Errorf("unexpected command: %q, want %q", cmd, "ls -la")
	}
}

func TestGenerateCommand_AuthError(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusUnauthorized)
		w.Write([]byte(`{"error": {"message": "Invalid API key"}}`))
	}))
	defer server.Close()

	cfg := &config.Config{
		APIEndpoint: server.URL,
		APIToken:    "bad-token",
		Model:       "gpt-4o-mini",
		Shell:       "bash",
	}

	client := NewClient(cfg)
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	_, err := client.GenerateCommand(ctx, "list files", "")
	if err == nil {
		t.Fatal("expected error for unauthorized request")
	}
}

func TestGenerateCommand_RateLimit(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusTooManyRequests)
	}))
	defer server.Close()

	cfg := &config.Config{
		APIEndpoint: server.URL,
		APIToken:    "test-token",
		Model:       "gpt-4o-mini",
		Shell:       "bash",
	}

	client := NewClient(cfg)
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	_, err := client.GenerateCommand(ctx, "list files", "")
	if err == nil {
		t.Fatal("expected error for rate-limited request")
	}
}

func TestGenerateCommand_ValidationError(t *testing.T) {
	cfg := &config.Config{
		APIEndpoint: "https://api.example.com",
		APIToken:    "", // missing token
		Model:       "gpt-4o-mini",
	}

	client := NewClient(cfg)
	ctx := context.Background()

	_, err := client.GenerateCommand(ctx, "list files", "")
	if err == nil {
		t.Fatal("expected validation error for missing token")
	}
}

func TestStripCodeFences(t *testing.T) {
	tests := []struct {
		input    string
		expected string
	}{
		{"ls -la", "ls -la"},
		{"```\nls -la\n```", "ls -la"},
		{"```bash\nls -la\n```", "ls -la"},
		{"```sh\nfind . -name '*.go'\n```", "find . -name '*.go'"},
		{"no fences here", "no fences here"},
	}

	for _, tt := range tests {
		result := stripCodeFences(tt.input)
		if result != tt.expected {
			t.Errorf("stripCodeFences(%q) = %q, want %q", tt.input, result, tt.expected)
		}
	}
}

func TestTestConnection_Success(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		resp := map[string]interface{}{
			"choices": []map[string]interface{}{
				{"message": map[string]string{"content": "ok"}},
			},
		}
		json.NewEncoder(w).Encode(resp)
	}))
	defer server.Close()

	cfg := &config.Config{
		APIEndpoint: server.URL,
		APIToken:    "test-token",
		Model:       "gpt-4o-mini",
	}

	client := NewClient(cfg)
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	if err := client.TestConnection(ctx); err != nil {
		t.Errorf("TestConnection failed: %v", err)
	}
}

func TestResolveTargetOS(t *testing.T) {
	tests := []struct {
		input         string
		expectedOS    string
		expectedShell string
	}{
		{"win", "Windows", "PowerShell"},
		{"windows", "Windows", "PowerShell"},
		{"linux", "Linux", "bash"},
		{"mac", "macOS", "zsh"},
		{"macos", "macOS", "zsh"},
		{"darwin", "macOS", "zsh"},
	}

	for _, tt := range tests {
		osName, shellType := ResolveTargetOS(tt.input)
		if osName != tt.expectedOS {
			t.Errorf("ResolveTargetOS(%q) osName = %q, want %q", tt.input, osName, tt.expectedOS)
		}
		if shellType != tt.expectedShell {
			t.Errorf("ResolveTargetOS(%q) shellType = %q, want %q", tt.input, shellType, tt.expectedShell)
		}
	}

	// Auto-detect (empty string) should return something valid
	osName, shellType := ResolveTargetOS("")
	if osName == "" || shellType == "" {
		t.Errorf("ResolveTargetOS(\"\") returned empty values: os=%q shell=%q", osName, shellType)
	}
}
