package cmd

import (
	"context"
	"fmt"
	"strings"
	"time"

	"ait/internal/ai"
	"ait/internal/config"

	"github.com/spf13/cobra"
)

var generateCmd = &cobra.Command{
	Use:   "generate <description>",
	Short: "Generate a command from a natural language description (headless mode)",
	Long:  `Generates a shell command based on the given description and prints it to stdout. Useful for scripting and piping.`,
	Args:  cobra.MinimumNArgs(1),
	RunE: func(cmd *cobra.Command, args []string) error {
		cfg, err := config.Load()
		if err != nil {
			return fmt.Errorf("failed to load config: %w", err)
		}

		if err := cfg.Validate(); err != nil {
			return err
		}

		description := strings.Join(args, " ")
		client := ai.NewClient(cfg)

		ctx, cancel := context.WithTimeout(context.Background(), 120*time.Second)
		defer cancel()

		command, err := client.GenerateCommand(ctx, description, "")
		if err != nil {
			return fmt.Errorf("generation failed: %w", err)
		}

		fmt.Println(command)
		return nil
	},
}

func init() {
	rootCmd.AddCommand(generateCmd)
}
