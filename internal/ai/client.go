package ai

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"runtime"
	"strings"
	"time"

	"aiterm/internal/config"
)

// Client handles communication with an OpenAI-compatible API.
type Client struct {
	cfg        *config.Config
	httpClient *http.Client
}

// NewClient creates a new AI client from the given configuration.
func NewClient(cfg *config.Config) *Client {
	return &Client{
		cfg: cfg,
		httpClient: &http.Client{
			Timeout: 120 * time.Second,
		},
	}
}

// chatRequest represents the request body for the chat completions API.
type chatRequest struct {
	Model    string        `json:"model"`
	Messages []chatMessage `json:"messages"`
}

// chatMessage represents a single message in the chat history.
type chatMessage struct {
	Role    string `json:"role"`
	Content string `json:"content"`
}

// chatResponse represents the response body from the chat completions API.
type chatResponse struct {
	Choices []struct {
		Message struct {
			Content string `json:"content"`
		} `json:"message"`
	} `json:"choices"`
	Error *struct {
		Message string `json:"message"`
		Type    string `json:"type"`
		Code    string `json:"code"`
	} `json:"error,omitempty"`
}

// ResolveTargetOS maps a user-provided -t flag to a full OS name.
// If empty or "auto", it detects the current OS.
func ResolveTargetOS(target string) (osName string, shellType string) {
	switch strings.ToLower(target) {
	case "win", "windows":
		return "Windows", "PowerShell"
	case "linux":
		return "Linux", "bash"
	case "mac", "macos", "darwin":
		return "macOS", "zsh"
	default:
		// Auto-detect
		switch runtime.GOOS {
		case "windows":
			return "Windows", "PowerShell"
		case "darwin":
			return "macOS", "zsh"
		default:
			return "Linux", "bash"
		}
	}
}

// systemPrompt builds the system prompt for the given OS and shell.
func systemPrompt(osName, shellType string) string {
	return fmt.Sprintf(
		"You are a shell command generator. Generate only a single, valid shell command based on the user's description. "+
			"Return ONLY the command with no explanation, no markdown, no code blocks. "+
			"The command should work in %s on %s.",
		shellType, osName,
	)
}

// GenerateCommand sends a natural language description to the AI API and
// returns the generated shell command. targetOS can be "win", "linux", "mac", or "" for auto.
func (c *Client) GenerateCommand(ctx context.Context, description, targetOS string) (string, error) {
	if err := c.cfg.Validate(); err != nil {
		return "", err
	}

	osName, shellType := ResolveTargetOS(targetOS)

	reqBody := chatRequest{
		Model: c.cfg.Model,
		Messages: []chatMessage{
			{Role: "system", Content: systemPrompt(osName, shellType)},
			{Role: "user", Content: fmt.Sprintf("Generate a single shell command for: %s", description)},
		},
	}

	bodyBytes, err := json.Marshal(reqBody)
	if err != nil {
		return "", fmt.Errorf("failed to marshal request: %w", err)
	}

	req, err := http.NewRequestWithContext(ctx, http.MethodPost, c.cfg.APIEndpoint, bytes.NewReader(bodyBytes))
	if err != nil {
		return "", fmt.Errorf("failed to create request: %w", err)
	}

	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Authorization", "Bearer "+c.cfg.APIToken)

	resp, err := c.httpClient.Do(req)
	if err != nil {
		if ctx.Err() != nil {
			return "", fmt.Errorf("request timed out")
		}
		return "", fmt.Errorf("API request failed: %w", err)
	}
	defer resp.Body.Close()

	respBytes, err := io.ReadAll(resp.Body)
	if err != nil {
		return "", fmt.Errorf("failed to read response: %w", err)
	}

	// Handle HTTP error codes
	switch resp.StatusCode {
	case http.StatusUnauthorized:
		return "", fmt.Errorf("authentication failed — check your API token")
	case http.StatusTooManyRequests:
		return "", fmt.Errorf("rate limit exceeded — please try again later")
	case http.StatusInternalServerError, http.StatusBadGateway, http.StatusServiceUnavailable:
		return "", fmt.Errorf("API server error (HTTP %d)", resp.StatusCode)
	}

	if resp.StatusCode != http.StatusOK {
		return "", fmt.Errorf("API returned HTTP %d: %s", resp.StatusCode, string(respBytes))
	}

	var chatResp chatResponse
	if err := json.Unmarshal(respBytes, &chatResp); err != nil {
		return "", fmt.Errorf("failed to parse API response: %w", err)
	}

	// Check for API-level error in response body
	if chatResp.Error != nil {
		return "", fmt.Errorf("API error: %s", chatResp.Error.Message)
	}

	if len(chatResp.Choices) == 0 {
		return "", fmt.Errorf("API returned no choices")
	}

	command := strings.TrimSpace(chatResp.Choices[0].Message.Content)

	// Strip markdown code fences if the model returned them anyway
	command = stripCodeFences(command)

	return command, nil
}

// TestConnection verifies that the API endpoint and token are working.
func (c *Client) TestConnection(ctx context.Context) error {
	reqBody := chatRequest{
		Model: c.cfg.Model,
		Messages: []chatMessage{
			{Role: "user", Content: "Reply with exactly: ok"},
		},
	}

	bodyBytes, err := json.Marshal(reqBody)
	if err != nil {
		return fmt.Errorf("failed to marshal request: %w", err)
	}

	req, err := http.NewRequestWithContext(ctx, http.MethodPost, c.cfg.APIEndpoint, bytes.NewReader(bodyBytes))
	if err != nil {
		return fmt.Errorf("failed to create request: %w", err)
	}

	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Authorization", "Bearer "+c.cfg.APIToken)

	resp, err := c.httpClient.Do(req)
	if err != nil {
		return fmt.Errorf("connection failed: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode == http.StatusUnauthorized {
		return fmt.Errorf("authentication failed — invalid API token")
	}
	if resp.StatusCode != http.StatusOK {
		body, _ := io.ReadAll(resp.Body)
		return fmt.Errorf("API returned HTTP %d: %s", resp.StatusCode, string(body))
	}

	return nil
}

// stripCodeFences removes markdown code block fences from a string.
func stripCodeFences(s string) string {
	lines := strings.Split(s, "\n")
	if len(lines) >= 2 && strings.HasPrefix(lines[0], "```") && strings.HasSuffix(lines[len(lines)-1], "```") {
		lines = lines[1 : len(lines)-1]
	}
	return strings.TrimSpace(strings.Join(lines, "\n"))
}
