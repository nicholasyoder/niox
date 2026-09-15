// Command niox-helper is the privileged process responsible for
// keyboard/mouse capture, virtual input, and the KM network hot path.
// See ARCHITECTURE.md for its role relative to niox-daemon.
package main

import (
	"fmt"
	"os"
)

func main() {
	fmt.Fprintln(os.Stderr, "niox-helper: not yet implemented")
	os.Exit(1)
}
