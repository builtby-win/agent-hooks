package agenthooks

import (
	"archive/tar"
	"bytes"
	"compress/gzip"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"sync/atomic"
	"testing"
)

const stubScript = `#!/bin/sh
if [ "$1" = "--version" ]; then echo "agent-hooks 1.2.3"; exit 0; fi
echo "Installing hooks for harness"
echo '{"path":"/x/agent-hooks","action":"installed","version":"1.2.3","harnesses":[{"id":"claude-code","status":"installed"}],"on_path":false}'
`

func buildArchive(t *testing.T, triple string) []byte {
	t.Helper()
	var buf bytes.Buffer
	gz := gzip.NewWriter(&buf)
	tw := tar.NewWriter(gz)
	body := []byte(stubScript)
	if err := tw.WriteHeader(&tar.Header{Name: "agent-hooks-" + triple + "/agent-hooks", Mode: 0o755, Size: int64(len(body)), Typeflag: tar.TypeReg}); err != nil {
		t.Fatal(err)
	}
	tw.Write(body)
	tw.Close()
	gz.Close()
	return buf.Bytes()
}

func server(t *testing.T, sum string) (*httptest.Server, *atomic.Int32) {
	t.Helper()
	triple, err := targetTriple(runtime.GOOS, runtime.GOARCH)
	if err != nil {
		t.Skip(err)
	}
	archive := buildArchive(t, triple)
	if sum == "" {
		h := sha256.Sum256(archive)
		sum = hex.EncodeToString(h[:])
	}
	name := "agent-hooks-" + triple + ".tar.gz"
	var hits atomic.Int32
	mux := http.NewServeMux()
	mux.HandleFunc("/"+name, func(w http.ResponseWriter, r *http.Request) {
		hits.Add(1)
		w.Write(archive)
	})
	mux.HandleFunc("/"+name+".sha256", func(w http.ResponseWriter, r *http.Request) {
		hits.Add(1)
		w.Write([]byte(sum + "  " + name + "\n"))
	})
	srv := httptest.NewServer(mux)
	t.Cleanup(srv.Close)
	return srv, &hits
}

func isolate(t *testing.T) string {
	home := t.TempDir()
	t.Setenv("HOME", home)
	t.Setenv("USERPROFILE", home)
	t.Setenv("AGENT_HOOKS_HOME", filepath.Join(home, ".agent-hooks"))
	t.Setenv("AGENT_HOOKS_BIN", "")
	return home
}

func TestTargetTriple(t *testing.T) {
	cases := map[[2]string]string{
		{"darwin", "arm64"}:  "aarch64-apple-darwin",
		{"linux", "amd64"}:   "x86_64-unknown-linux-musl",
		{"windows", "arm64"}: "aarch64-pc-windows-msvc",
	}
	for in, want := range cases {
		got, err := targetTriple(in[0], in[1])
		if err != nil || got != want {
			t.Errorf("%v: got %q, %v; want %q", in, got, err, want)
		}
	}
	if _, err := targetTriple("plan9", "amd64"); err == nil {
		t.Error("expected error for unsupported OS")
	}
	if _, err := targetTriple("linux", "386"); err == nil {
		t.Error("expected error for unsupported arch")
	}
}

func TestInitDownloadsAndParses(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("shell stub")
	}
	isolate(t)
	srv, hits := server(t, "")
	res, err := Init(context.Background(), Options{BaseURL: srv.URL, NoHooks: true})
	if err != nil {
		t.Fatal(err)
	}
	if res.Action != "installed" || res.Version != "1.2.3" || len(res.Harnesses) != 1 || res.Harnesses[0].ID != "claude-code" || res.OnPath {
		t.Fatalf("unexpected result %+v", res)
	}
	if hits.Load() != 2 {
		t.Fatalf("expected 2 downloads, got %d", hits.Load())
	}
}

func TestInitRejectsTamperedChecksum(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("shell stub")
	}
	isolate(t)
	srv, _ := server(t, strings.Repeat("0", 64))
	_, err := Init(context.Background(), Options{BaseURL: srv.URL})
	if err == nil || !strings.Contains(err.Error(), "checksum mismatch") {
		t.Fatalf("expected checksum mismatch, got %v", err)
	}
	if p, err := BinaryPath(); err == nil {
		t.Fatalf("binary unexpectedly installed at %s", p)
	}
}

func TestInitUsesExistingBinary(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("shell stub")
	}
	isolate(t)
	shared, err := sharedBinary()
	if err != nil {
		t.Fatal(err)
	}
	os.MkdirAll(filepath.Dir(shared), 0o755)
	if err := os.WriteFile(shared, []byte(stubScript), 0o755); err != nil {
		t.Fatal(err)
	}
	srv, hits := server(t, "")
	res, err := Init(context.Background(), Options{BaseURL: srv.URL, MinVersion: "1.2.0"})
	if err != nil {
		t.Fatal(err)
	}
	if res.Version != "1.2.3" || hits.Load() != 0 {
		t.Fatalf("res=%+v hits=%d", res, hits.Load())
	}
	if p, err := BinaryPath(); err != nil || p != shared {
		t.Fatalf("BinaryPath = %q, %v", p, err)
	}
}
