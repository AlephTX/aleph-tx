package main

import (
	"compress/gzip"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
)

// Both plain files and compressed outputs preserve the capture durability
// boundary. A successful gzip close alone does not certify collector:end.
type syncWriter interface {
	io.Writer
	Sync() error
}

type syncWriteCloser interface {
	syncWriter
	io.Closer
}

type captureOutput struct {
	file       syncWriteCloser
	compressor *gzip.Writer
	closed     bool
	closeErr   error
}

func newCaptureOutput(file syncWriteCloser, compressed bool) (*captureOutput, error) {
	out := &captureOutput{file: file}
	if compressed {
		var err error
		out.compressor, err = gzip.NewWriterLevel(file, gzip.BestSpeed)
		if err != nil {
			return nil, errors.Join(err, file.Close())
		}
	}
	return out, nil
}

func openCaptureOutput(path string, compressed bool) (*captureOutput, error) {
	file, err := os.OpenFile(path, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0600)
	if err != nil {
		return nil, err
	}
	return newCaptureOutput(file, compressed)
}

func (out *captureOutput) Write(data []byte) (int, error) {
	if out.closed {
		return 0, os.ErrClosed
	}
	if out.compressor != nil {
		return out.compressor.Write(data)
	}
	return out.file.Write(data)
}

func (out *captureOutput) Sync() error {
	if out.closed {
		return os.ErrClosed
	}
	if out.compressor != nil {
		if err := out.compressor.Flush(); err != nil {
			return err
		}
	}
	return out.file.Sync()
}

func (out *captureOutput) Close() error {
	if out.closed {
		return out.closeErr
	}
	out.closed = true
	var finalErr error
	if out.compressor != nil {
		finalErr = out.compressor.Close()
	}
	// Sync again after the gzip footer, then close even on flush/sync failure.
	out.closeErr = errors.Join(finalErr, out.file.Sync(), out.file.Close())
	return out.closeErr
}

// segmentPath derives `<stem>.segNNNN<ext>` from basePath so rotating captures
// preserve standard `.ndjson.gz` / `.ndjson` suffixes for downstream sealers.
func segmentPath(basePath string, index int) string {
	switch {
	case strings.HasSuffix(basePath, ".ndjson.gz"):
		return fmt.Sprintf("%s.seg%04d.ndjson.gz", strings.TrimSuffix(basePath, ".ndjson.gz"), index)
	case strings.HasSuffix(basePath, ".ndjson"):
		return fmt.Sprintf("%s.seg%04d.ndjson", strings.TrimSuffix(basePath, ".ndjson"), index)
	case strings.HasSuffix(basePath, ".gz"):
		return fmt.Sprintf("%s.seg%04d.gz", strings.TrimSuffix(basePath, ".gz"), index)
	default:
		return fmt.Sprintf("%s.seg%04d", basePath, index)
	}
}

// rollingCaptureOutput writes each active segment to `<final>.partial` and
// atomically renames it to `<final>` only after flushing, closing gzip, and
// fsyncing. Aborted segments remain `.partial` and are never shipped to Bronze.
type rollingCaptureOutput struct {
	basePath    string
	compressed  bool
	index       int
	active      *captureOutput
	partialPath string
	finalPath   string
	sealedPaths []string
}

func openRollingCaptureOutput(basePath string, compressed bool) (*rollingCaptureOutput, error) {
	r := &rollingCaptureOutput{
		basePath:   basePath,
		compressed: compressed,
	}
	if err := r.openSegment(0); err != nil {
		return nil, err
	}
	return r, nil
}

func (r *rollingCaptureOutput) openSegment(index int) error {
	final := segmentPath(r.basePath, index)
	partial := final + ".partial"
	if _, err := os.Lstat(final); err == nil {
		return fmt.Errorf("segment output already exists: %s", final)
	} else if !errors.Is(err, os.ErrNotExist) {
		return err
	}
	out, err := openCaptureOutput(partial, r.compressed)
	if err != nil {
		return err
	}
	r.index = index
	r.active = out
	r.partialPath = partial
	r.finalPath = final
	return nil
}

func (r *rollingCaptureOutput) Write(data []byte) (int, error) {
	if r.active == nil {
		return 0, os.ErrClosed
	}
	return r.active.Write(data)
}

func (r *rollingCaptureOutput) Sync() error {
	if r.active == nil {
		return os.ErrClosed
	}
	return r.active.Sync()
}

func (r *rollingCaptureOutput) sealCurrent() (string, error) {
	if r.active == nil {
		return "", os.ErrClosed
	}
	active := r.active
	partial := r.partialPath
	final := r.finalPath
	r.active = nil
	if err := active.Close(); err != nil {
		return "", err
	}
	if err := os.Rename(partial, final); err != nil {
		return "", err
	}
	if dir, err := os.Open(filepath.Dir(final)); err == nil {
		_ = dir.Sync()
		_ = dir.Close()
	}
	r.sealedPaths = append(r.sealedPaths, final)
	return final, nil
}

// Rotate seals the current `.partial` segment into its immutable `.segNNNN`
// path and immediately opens segment `index+1` without dropping WS state.
func (r *rollingCaptureOutput) Rotate() (string, int, error) {
	prevFinal, err := r.sealCurrent()
	if err != nil {
		return "", r.index, err
	}
	nextIdx := r.index + 1
	if err := r.openSegment(nextIdx); err != nil {
		return prevFinal, r.index, err
	}
	return filepath.Base(prevFinal), nextIdx, nil
}

// Abort closes any unsealed `.partial` file on error without renaming it.
func (r *rollingCaptureOutput) Abort() error {
	if r.active == nil {
		return nil
	}
	active := r.active
	r.active = nil
	return active.Close()
}
