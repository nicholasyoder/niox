package ipc

import (
	"errors"
	"fmt"
	"net"
	"os"
	"path/filepath"
)

// socketMode restricts the control socket to its owner, which — combined
// with niox-session listening from inside the session user's own runtime
// directory — is the access boundary ARCHITECTURE.md describes: only that
// user (and root, which bypasses DAC checks) can connect.
const socketMode = 0o600

// Listener accepts incoming control connections on a Unix domain socket.
type Listener struct {
	ln   net.Listener
	path string
}

// Listen creates (or replaces) a Unix domain socket at path and starts
// listening on it. Any stale socket file left behind by a previous,
// uncleanly-terminated run is removed first; Listen does not check
// whether that file was actually stale (i.e. no process still holds it),
// matching the spike-scoped stance internal/transport/doc.go takes on
// similar robustness gaps.
func Listen(path string) (*Listener, error) {
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return nil, fmt.Errorf("ipc: create socket directory: %w", err)
	}
	if err := os.Remove(path); err != nil && !errors.Is(err, os.ErrNotExist) {
		return nil, fmt.Errorf("ipc: remove stale socket: %w", err)
	}

	ln, err := net.Listen("unix", path)
	if err != nil {
		return nil, fmt.Errorf("ipc: listen on %s: %w", path, err)
	}
	if err := os.Chmod(path, socketMode); err != nil {
		ln.Close()
		return nil, fmt.Errorf("ipc: set socket permissions: %w", err)
	}

	return &Listener{ln: ln, path: path}, nil
}

// Accept waits for and returns the next connection to the listener.
func (l *Listener) Accept() (*Conn, error) {
	c, err := l.ln.Accept()
	if err != nil {
		return nil, fmt.Errorf("ipc: accept: %w", err)
	}
	return newConn(c), nil
}

// Close stops listening and removes the socket file.
func (l *Listener) Close() error {
	err := l.ln.Close()
	if rmErr := os.Remove(l.path); rmErr != nil && !errors.Is(rmErr, os.ErrNotExist) && err == nil {
		err = fmt.Errorf("ipc: remove socket: %w", rmErr)
	}
	return err
}

// Dial connects to the control socket at path.
func Dial(path string) (*Conn, error) {
	c, err := net.Dial("unix", path)
	if err != nil {
		return nil, fmt.Errorf("ipc: dial %s: %w", path, err)
	}
	return newConn(c), nil
}
