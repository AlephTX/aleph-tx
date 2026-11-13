package main

import (
	"compress/gzip"
	"errors"
	"io"
	"os"
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
