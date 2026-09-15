// Command niox-daemon is the user-session process responsible for audio,
// the focus event bus, and the HTTP control API / web UI.
// See ARCHITECTURE.md for its role relative to niox-helper.
package main

import (
	"encoding/base64"
	"flag"
	"fmt"
	"os"
	"path/filepath"

	"github.com/nicholasyoder/niox/internal/config"
)

func defaultConfigPath() string {
	if dir, err := os.UserConfigDir(); err == nil {
		return filepath.Join(dir, "niox", "niox.toml")
	}
	return "niox.toml"
}

func main() {
	configPath := flag.String("config", defaultConfigPath(), "path to niox config file")
	printConfig := flag.Bool("print-config", false, "load (or create) the config file and print it, then exit")
	flag.Parse()

	if *printConfig {
		if err := os.MkdirAll(filepath.Dir(*configPath), 0o700); err != nil {
			fmt.Fprintf(os.Stderr, "niox-daemon: %v\n", err)
			os.Exit(1)
		}
		cfg, err := config.Load(*configPath)
		if err != nil {
			fmt.Fprintf(os.Stderr, "niox-daemon: %v\n", err)
			os.Exit(1)
		}
		fmt.Printf("config path:   %s\n", *configPath)
		fmt.Printf("public key:    %s\n", base64.StdEncoding.EncodeToString(cfg.Identity.PublicKey))
		fmt.Printf("known peers:   %d\n", len(cfg.Peers))
		return
	}

	fmt.Fprintln(os.Stderr, "niox-daemon: not yet implemented")
	os.Exit(1)
}
