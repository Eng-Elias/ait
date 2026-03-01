package cmd

import (
	"context"
	"fmt"
	"os"
	"strings"
	"time"

	"ait/internal/ai"
	"ait/internal/config"

	"github.com/spf13/cobra"
)

// Version is set at build time via ldflags.
var Version = "dev"

var (
	debug      bool
	targetType string
)

var rootCmd = &cobra.Command{
	Use:   "ait [prompt]",
	Short: "AIT - AI Terminal command generator",
	Long: `AIT (AI Terminal) generates shell commands from natural language descriptions.
Simply describe what you want and AIT prints the command you can run.

Examples:
  ait "list all files larger than 100MB"
  ait "find all PDFs modified in the last 7 days" -t linux
  ait "show disk usage sorted by size" -t mac`,
	Args: cobra.ArbitraryArgs,
	RunE: func(cmd *cobra.Command, args []string) error {
		if len(args) == 0 {
			return cmd.Help()
		}

		prompt := strings.Join(args, " ")
		return runGenerate(prompt, targetType)
	},
}

func init() {
	rootCmd.PersistentFlags().BoolVar(&debug, "debug", false, "Enable debug logging to ~/.ait/debug.log")
	rootCmd.Flags().StringVarP(&targetType, "type", "t", "", "Target OS type: win, linux, mac (auto-detected if omitted)")
}

// Execute runs the root command.
func Execute() {
	if err := rootCmd.Execute(); err != nil {
		os.Exit(1)
	}
}

// runGenerate sends the prompt to the AI and prints the suggested command.
func runGenerate(prompt, target string) error {
	cfg, err := config.Load()
	if err != nil {
		return fmt.Errorf("failed to load config: %w", err)
	}

	if err := cfg.Validate(); err != nil {
		fmt.Fprintln(os.Stderr, "AI not configured. Run 'ait setup' first.")
		return err
	}

	osName, shellType := ai.ResolveTargetOS(target)
	fmt.Fprintf(os.Stderr, "\033[90m[%s / %s] Generating...\033[0m\n", osName, shellType)

	client := ai.NewClient(cfg)
	ctx, cancel := context.WithTimeout(context.Background(), 120*time.Second)
	defer cancel()

	command, err := client.GenerateCommand(ctx, prompt, target)
	if err != nil {
		return fmt.Errorf("generation failed: %w", err)
	}

	// Print the command to stdout so the user can copy/pipe it
	fmt.Println(command)
	return nil
}
