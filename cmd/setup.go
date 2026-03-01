package cmd

import (
	"bufio"
	"context"
	"fmt"
	"os"
	"strings"
	"time"

	"ait/internal/ai"
	"ait/internal/config"

	"github.com/spf13/cobra"
	"golang.org/x/term"
)

var setupCmd = &cobra.Command{
	Use:   "setup",
	Short: "Interactive setup wizard",
	Long:  `Guides you through configuring AIT with your API credentials and preferences.`,
	RunE: func(cmd *cobra.Command, args []string) error {
		return runSetup()
	},
}

func init() {
	rootCmd.AddCommand(setupCmd)
}

// runSetup runs the interactive setup wizard.
func runSetup() error {
	reader := bufio.NewReader(os.Stdin)

	fmt.Println()
	fmt.Println("╔══════════════════════════════════════╗")
	fmt.Println("║        Welcome to AIT Setup          ║")
	fmt.Println("╚══════════════════════════════════════╝")
	fmt.Println()

	cfg, err := config.Load()
	if err != nil {
		cfg = config.DefaultConfig()
	}

	// API Endpoint
	fmt.Printf("API Endpoint [%s]: ", cfg.APIEndpoint)
	endpoint, _ := reader.ReadString('\n')
	endpoint = strings.TrimSpace(endpoint)
	if endpoint != "" {
		cfg.APIEndpoint = endpoint
	}

	// API Token (masked input)
	fmt.Print("API Token: ")
	tokenBytes, err := term.ReadPassword(int(os.Stdin.Fd()))
	fmt.Println() // newline after hidden input
	if err != nil {
		// Fallback to normal input if terminal password reading fails
		fmt.Print("API Token (input will be visible): ")
		token, _ := reader.ReadString('\n')
		token = strings.TrimSpace(token)
		if token != "" {
			cfg.APIToken = token
		}
	} else {
		token := strings.TrimSpace(string(tokenBytes))
		if token != "" {
			cfg.APIToken = token
		}
	}

	// Model
	fmt.Printf("Model [%s]: ", cfg.Model)
	model, _ := reader.ReadString('\n')
	model = strings.TrimSpace(model)
	if model != "" {
		cfg.Model = model
	}

	// Test connection
	if cfg.APIToken != "" {
		fmt.Print("\nTesting API connection... ")
		client := ai.NewClient(cfg)
		ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
		defer cancel()

		if err := client.TestConnection(ctx); err != nil {
			fmt.Printf("✗ Failed: %v\n", err)
			fmt.Println("Configuration will be saved anyway. You can update it later with 'ait config set'.")
		} else {
			fmt.Println("✓ Connection successful!")
		}
	}

	// Save configuration
	if err := cfg.Save(); err != nil {
		return fmt.Errorf("failed to save configuration: %w", err)
	}

	path, _ := config.ConfigFilePath()
	fmt.Printf("\nConfiguration saved to %s\n", path)
	fmt.Println("Run 'ait' to start the AI Terminal!")
	fmt.Println()

	return nil
}
