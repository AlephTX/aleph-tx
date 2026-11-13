package main

import (
	"bytes"
	"compress/gzip"
	"context"
	"encoding/json"
	"errors"
	"io"
	"os"
	"path/filepath"
	"testing"
)

type failingCaptureFile struct {
	bytes.Buffer
	writeErr error
	syncErr  error
	closeErr error
	closed   bool
	syncs    int
}

func (f *failingCaptureFile) Write(data []byte) (int, error) {
	if f.writeErr != nil {
		return 0, f.writeErr
	}
	return f.Buffer.Write(data)
}

func (f *failingCaptureFile) Sync() error  { f.syncs++; return f.syncErr }
func (f *failingCaptureFile) Close() error { f.closed = true; return f.closeErr }

func TestCaptureOutputRoundTripAndEnd(t *testing.T) {
	for _, compressed := range []bool{false, true} {
		path := filepath.Join(t.TempDir(), "public.ndjson")
		out, err := openCaptureOutput(path, compressed)
		if err != nil {
			t.Fatal(err)
		}
		// No network sources: still exercise the real terminal marker/flush path.
		if err := capture(context.Background(), nil, out); err != nil {
			t.Fatal(err)
		}
		if err := out.Close(); err != nil {
			t.Fatal(err)
		}
		if err := out.Close(); err != nil {
			t.Fatal(err)
		}
		if _, err := out.Write([]byte("late")); !errors.Is(err, os.ErrClosed) {
			t.Fatal("write after close accepted")
		}
		if err := out.Sync(); !errors.Is(err, os.ErrClosed) {
			t.Fatal("sync after close accepted")
		}
		if _, err := openCaptureOutput(path, compressed); !errors.Is(err, os.ErrExist) {
			t.Fatal("existing archive overwritten")
		}
		info, err := os.Stat(path)
		if err != nil || info.Mode().Perm() != 0600 {
			t.Fatal("capture permissions changed")
		}
		data, err := os.ReadFile(path)
		if err != nil {
			t.Fatal(err)
		}
		if compressed {
			reader, err := gzip.NewReader(bytes.NewReader(data))
			if err != nil {
				t.Fatal(err)
			}
			data, err = io.ReadAll(reader)
			if err != nil {
				t.Fatal(err)
			}
			if err := reader.Close(); err != nil {
				t.Fatal(err)
			}
		}
		var row record
		if err := json.Unmarshal(bytes.TrimSpace(data), &row); err != nil {
			t.Fatal(err)
		}
		if row.Source != "collector" || row.Event != "end" {
			t.Fatal("missing capture end")
		}
	}
}

func TestCompressedCaptureRejectsTruncatedArchive(t *testing.T) {
	f := &failingCaptureFile{}
	out, err := newCaptureOutput(f, true)
	if err != nil {
		t.Fatal(err)
	}
	if err := capture(context.Background(), nil, out); err != nil {
		t.Fatal(err)
	}
	// Sync exposes a recoverable prefix, not a sealed gzip archive.
	prefix := append([]byte(nil), f.Bytes()...)
	if err := out.Close(); err != nil {
		t.Fatal(err)
	}
	for _, data := range [][]byte{prefix, f.Bytes()[:f.Len()-4]} {
		reader, err := gzip.NewReader(bytes.NewReader(data))
		if err != nil {
			t.Fatal(err)
		}
		if _, err = io.ReadAll(reader); !errors.Is(err, io.ErrUnexpectedEOF) {
			t.Fatalf("truncation accepted: %v", err)
		}
		reader.Close()
	}
}

func TestCaptureOutputPropagatesFlushAndCloseFailures(t *testing.T) {
	writeErr := errors.New("synthetic write failure")
	syncErr := errors.New("synthetic sync failure")
	closeErr := errors.New("synthetic close failure")
	for _, compressed := range []bool{false, true} {
		f := &failingCaptureFile{writeErr: writeErr}
		out, err := newCaptureOutput(f, compressed)
		if err != nil {
			t.Fatal(err)
		}
		if err := capture(context.Background(), nil, out); !errors.Is(err, writeErr) {
			t.Fatalf("write failure swallowed: %v", err)
		}
		_ = out.Close()
		if !f.closed {
			t.Fatal("failed output not closed")
		}

		f = &failingCaptureFile{}
		out, err = newCaptureOutput(f, compressed)
		if err != nil {
			t.Fatal(err)
		}
		if err := capture(context.Background(), nil, out); err != nil {
			t.Fatal(err)
		}
		// Final gzip footer and final fsync/close may fail after capture Sync.
		f.syncErr, f.closeErr = syncErr, closeErr
		if compressed {
			f.writeErr = writeErr
		}
		err = out.Close()
		if !errors.Is(err, syncErr) || !errors.Is(err, closeErr) || (compressed && !errors.Is(err, writeErr)) {
			t.Fatalf("close failures swallowed: %v", err)
		}
		if !f.closed || f.syncs != 2 {
			t.Fatal("final durability/close boundary skipped")
		}
		if out.Close() != err {
			t.Fatal("close did not preserve original failure")
		}
	}
}

func TestCaptureSyncFailureCannotReportSuccess(t *testing.T) {
	want := errors.New("synthetic fsync failure")
	f := &failingCaptureFile{syncErr: want}
	out, err := newCaptureOutput(f, true)
	if err != nil {
		t.Fatal(err)
	}
	if err := capture(context.Background(), nil, out); !errors.Is(err, want) {
		t.Fatalf("sync failure swallowed: %v", err)
	}
	if err := out.Close(); !errors.Is(err, want) {
		t.Fatal("close lost sync failure")
	}
}
