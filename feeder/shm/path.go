package shm

import (
	"os"
	"path/filepath"
)

// Path resolves SHM segments into the directory owned by the supervised session.
func Path(name string) string {
	dir := os.Getenv("ALEPH_SHM_DIR")
	if dir == "" {
		dir = "/dev/shm"
	}
	return filepath.Join(dir, name)
}
