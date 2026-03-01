package config

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"strings"
)

// Config represents the application configuration.
type Config struct {
	APIEndpoint string `json:"api_endpoint"`
	APIToken    string `json:"api_token"`
	Model       string `json:"model"`
	Shell       string `json:"shell"`
}

// DefaultConfig returns a Config with sensible defaults.
func DefaultConfig() *Config {
	return &Config{
		APIEndpoint: "https://api.openai.com/v1/chat/completions",
		APIToken:    "",
		Model:       "gpt-4o-mini",
		Shell:       "auto",
	}
}

// ConfigDir returns the path to the AIT configuration directory.
func ConfigDir() (string, error) {
	var home string
	if runtime.GOOS == "windows" {
		home = os.Getenv("USERPROFILE")
	} else {
		home = os.Getenv("HOME")
	}
	if home == "" {
		return "", fmt.Errorf("unable to determine home directory")
	}
	return filepath.Join(home, ".ait"), nil
}

// ConfigFilePath returns the full path to config.json.
func ConfigFilePath() (string, error) {
	dir, err := ConfigDir()
	if err != nil {
		return "", err
	}
	return filepath.Join(dir, "config.json"), nil
}

// ensureConfigDir creates the config directory with proper permissions.
func ensureConfigDir() error {
	dir, err := ConfigDir()
	if err != nil {
		return err
	}
	if runtime.GOOS != "windows" {
		return os.MkdirAll(dir, 0700)
	}
	return os.MkdirAll(dir, os.ModePerm)
}

// Load reads the configuration from disk. If the file does not exist,
// it creates a default configuration file and returns it.
func Load() (*Config, error) {
	path, err := ConfigFilePath()
	if err != nil {
		return nil, fmt.Errorf("config path error: %w", err)
	}

	data, err := os.ReadFile(path)
	if err != nil {
		if os.IsNotExist(err) {
			// Create default config
			cfg := DefaultConfig()
			if saveErr := cfg.Save(); saveErr != nil {
				return nil, fmt.Errorf("failed to create default config: %w", saveErr)
			}
			return cfg, nil
		}
		return nil, fmt.Errorf("failed to read config: %w", err)
	}

	var cfg Config
	if err := json.Unmarshal(data, &cfg); err != nil {
		return nil, fmt.Errorf("failed to parse config: %w", err)
	}

	return &cfg, nil
}

// Save writes the configuration to disk with proper permissions.
func (c *Config) Save() error {
	if err := ensureConfigDir(); err != nil {
		return fmt.Errorf("failed to create config directory: %w", err)
	}

	path, err := ConfigFilePath()
	if err != nil {
		return err
	}

	data, err := json.MarshalIndent(c, "", "  ")
	if err != nil {
		return fmt.Errorf("failed to marshal config: %w", err)
	}

	perm := os.FileMode(0600)
	if runtime.GOOS == "windows" {
		perm = os.ModePerm
	}

	if err := os.WriteFile(path, data, perm); err != nil {
		return fmt.Errorf("failed to write config: %w", err)
	}

	return nil
}

// Get retrieves a configuration value by key name.
func (c *Config) Get(key string) (string, error) {
	switch strings.ToLower(key) {
	case "api_endpoint":
		return c.APIEndpoint, nil
	case "api_token":
		return c.APIToken, nil
	case "model":
		return c.Model, nil
	case "shell":
		return c.Shell, nil
	default:
		return "", fmt.Errorf("unknown config key: %s", key)
	}
}

// Set updates a configuration value by key name and saves to disk.
func (c *Config) Set(key, value string) error {
	switch strings.ToLower(key) {
	case "api_endpoint":
		c.APIEndpoint = value
	case "api_token":
		c.APIToken = value
	case "model":
		c.Model = value
	case "shell":
		c.Shell = value
	default:
		return fmt.Errorf("unknown config key: %s", key)
	}
	return c.Save()
}

// Validate checks that required configuration fields are present.
func (c *Config) Validate() error {
	if c.APIEndpoint == "" {
		return fmt.Errorf("api_endpoint is required")
	}
	if c.APIToken == "" {
		return fmt.Errorf("api_token is required — run 'ait setup' to configure")
	}
	if c.Model == "" {
		return fmt.Errorf("model is required")
	}
	return nil
}

// MaskToken returns the API token with all but the last 4 characters masked.
func MaskToken(token string) string {
	if len(token) <= 4 {
		return "****"
	}
	return strings.Repeat("*", len(token)-4) + token[len(token)-4:]
}

// Display prints the configuration with the API token masked.
func (c *Config) Display() string {
	masked := *c
	masked.APIToken = MaskToken(c.APIToken)
	data, _ := json.MarshalIndent(masked, "", "  ")
	return string(data)
}
