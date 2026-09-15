// Command niox-helper is the privileged process responsible for
// keyboard/mouse capture, virtual input, and the KM network hot path.
// See ARCHITECTURE.md for its role relative to niox-daemon.
package main

import (
	"flag"
	"fmt"
	"os"
	"path/filepath"

	"github.com/nicholasyoder/niox/internal/ipc"
)

func defaultSocketPath() string {
	if dir := os.Getenv("XDG_RUNTIME_DIR"); dir != "" {
		return filepath.Join(dir, "niox", "niox.sock")
	}
	return filepath.Join(os.TempDir(), fmt.Sprintf("niox-%d.sock", os.Getuid()))
}

func main() {
	socketPath := flag.String("socket", defaultSocketPath(), "path to the niox-daemon control socket")
	flag.Parse()

	if err := ping(*socketPath); err != nil {
		fmt.Fprintf(os.Stderr, "niox-helper: %v\n", err)
		os.Exit(1)
	}
}

// ping dials niox-daemon's control socket, sends a ping, and prints the
// status reply. It's a smoke test for the control channel per
// ROADMAP.md milestone 3 -- the real capture/send hot path arrives with
// milestone 4.
func ping(socketPath string) error {
	conn, err := ipc.Dial(socketPath)
	if err != nil {
		return err
	}
	defer conn.Close()

	if err := conn.Send(ipc.Message{Type: ipc.MessageTypePing}); err != nil {
		return err
	}

	reply, err := conn.Recv()
	if err != nil {
		return err
	}

	fmt.Printf("niox-helper: got %s from niox-daemon (ready=%v)\n", reply.Type, reply.Ready)
	return nil
}
