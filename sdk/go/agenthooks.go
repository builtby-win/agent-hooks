// Package agenthooks installs the agent-hooks binary (downloading and
// verifying the release asset on first use) and registers hooks in detected
// harnesses via `agent-hooks self-install`.
package agenthooks

import (
	"archive/tar"
	"archive/zip"
	"bytes"
	"compress/gzip"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"os"
	"os/exec"
	"path"
	"path/filepath"
	"regexp"
	"runtime"
	"strconv"
	"strings"
	"time"
)

const (
	releasesLatest = "https://github.com/builtby-win/agent-hooks/releases/latest/download"
	releasesTagged = "https://github.com/builtby-win/agent-hooks/releases/download/"
	maxBinarySize  = 512 << 20
)

// Options configures Init.
type Options struct {
	// Version is a release tag (e.g. "v0.3.0"); empty means latest.
	Version string
	// BaseURL overrides the release download location (mirrors, tests).
	BaseURL string
	// Harness is the harness id or "all" (default).
	Harness string
	// NoHooks skips hook registration.
	NoHooks bool
	// MinVersion is the minimum acceptable installed version (X.Y.Z).
	MinVersion string
	// Socket is an optional daemon socket path.
	Socket string
	// HTTPClient is used for downloads; defaults to a client with a timeout.
	HTTPClient *http.Client
}

// HarnessResult is one harness's outcome from the hook registration pass.
type HarnessResult struct {
	ID string `json:"id"`
	// Status is "installed", "current", "skipped" or "failed".
	Status string `json:"status"`
	Error  string `json:"error,omitempty"`
}

// Result is the JSON object printed by `agent-hooks self-install --json`.
// Harnesses is empty when hooks were not requested or no harness was detected.
type Result struct {
	Path      string          `json:"path"`
	Action    string          `json:"action"`
	Version   string          `json:"version"`
	Harnesses []HarnessResult `json:"harnesses"`
	OnPath    bool            `json:"on_path"`
}

func exeName() string {
	if runtime.GOOS == "windows" {
		return "agent-hooks.exe"
	}
	return "agent-hooks"
}

// targetTriple maps a GOOS/GOARCH pair to a release target triple.
func targetTriple(goos, goarch string) (string, error) {
	var arch string
	switch goarch {
	case "amd64":
		arch = "x86_64"
	case "arm64":
		arch = "aarch64"
	default:
		return "", fmt.Errorf("agent-hooks: unsupported architecture %q", goarch)
	}
	switch goos {
	case "darwin":
		return arch + "-apple-darwin", nil
	case "linux":
		return arch + "-unknown-linux-musl", nil
	case "windows":
		return arch + "-pc-windows-msvc", nil
	}
	return "", fmt.Errorf("agent-hooks: unsupported OS %q", goos)
}

func sharedBinary() (string, error) {
	home := os.Getenv("AGENT_HOOKS_HOME")
	if home == "" {
		h, err := os.UserHomeDir()
		if err != nil {
			return "", err
		}
		home = filepath.Join(h, ".agent-hooks")
	}
	return filepath.Join(home, "bin", exeName()), nil
}

// BinaryPath returns the shared installed binary path if it exists.
func BinaryPath() (string, error) {
	p, err := sharedBinary()
	if err != nil {
		return "", err
	}
	if st, err := os.Stat(p); err != nil || st.IsDir() {
		return "", fmt.Errorf("agent-hooks: not installed at %s (call Init first)", p)
	}
	return p, nil
}

// Run executes binPath with args, inheriting stdio.
func Run(ctx context.Context, binPath string, args ...string) error {
	cmd := exec.CommandContext(ctx, binPath, args...)
	cmd.Stdin, cmd.Stdout, cmd.Stderr = os.Stdin, os.Stdout, os.Stderr
	return cmd.Run()
}

var semverRe = regexp.MustCompile(`(\d+)\.(\d+)\.(\d+)`)

func parseSemver(s string) ([3]int, bool) {
	m := semverRe.FindStringSubmatch(s)
	if m == nil {
		return [3]int{}, false
	}
	var v [3]int
	for i := range 3 {
		v[i], _ = strconv.Atoi(m[i+1])
	}
	return v, true
}

// satisfies reports whether bin's --version is >= min.
func satisfies(ctx context.Context, bin, min string) bool {
	want, ok := parseSemver(min)
	if !ok {
		return false
	}
	out, err := exec.CommandContext(ctx, bin, "--version").Output()
	if err != nil {
		return false
	}
	got, ok := parseSemver(string(out))
	if !ok {
		return false
	}
	for i := range 3 {
		if got[i] != want[i] {
			return got[i] > want[i]
		}
	}
	return true
}

func fetch(ctx context.Context, c *http.Client, url string, limit int64) ([]byte, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return nil, err
	}
	resp, err := c.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("agent-hooks: GET %s: %s", url, resp.Status)
	}
	b, err := io.ReadAll(io.LimitReader(resp.Body, limit+1))
	if err != nil {
		return nil, err
	}
	if int64(len(b)) > limit {
		return nil, fmt.Errorf("agent-hooks: GET %s: response too large", url)
	}
	return b, nil
}

// extract returns the bytes of member `want` from the archive.
func extract(archive []byte, isZip bool, want string) ([]byte, error) {
	if isZip {
		zr, err := zip.NewReader(bytes.NewReader(archive), int64(len(archive)))
		if err != nil {
			return nil, err
		}
		for _, f := range zr.File {
			if path.Clean(f.Name) != want {
				continue
			}
			rc, err := f.Open()
			if err != nil {
				return nil, err
			}
			defer rc.Close()
			return io.ReadAll(io.LimitReader(rc, maxBinarySize))
		}
	} else {
		gz, err := gzip.NewReader(bytes.NewReader(archive))
		if err != nil {
			return nil, err
		}
		defer gz.Close()
		tr := tar.NewReader(gz)
		for {
			h, err := tr.Next()
			if err == io.EOF {
				break
			}
			if err != nil {
				return nil, err
			}
			if h.Typeflag == tar.TypeReg && path.Clean(h.Name) == want {
				return io.ReadAll(io.LimitReader(tr, maxBinarySize))
			}
		}
	}
	return nil, fmt.Errorf("agent-hooks: %q not found in archive", want)
}

// download fetches, verifies and extracts the binary into dir.
func download(ctx context.Context, opts Options, dir string) (string, error) {
	triple, err := targetTriple(runtime.GOOS, runtime.GOARCH)
	if err != nil {
		return "", err
	}
	base := opts.BaseURL
	if base == "" {
		if opts.Version == "" {
			base = releasesLatest
		} else {
			base = releasesTagged + opts.Version
		}
	}
	base = strings.TrimRight(base, "/")
	isZip := runtime.GOOS == "windows"
	asset := "agent-hooks-" + triple + ".tar.gz"
	if isZip {
		asset = "agent-hooks-" + triple + ".zip"
	}
	c := opts.HTTPClient
	if c == nil {
		c = &http.Client{Timeout: 2 * time.Minute}
	}
	archive, err := fetch(ctx, c, base+"/"+asset, maxBinarySize)
	if err != nil {
		return "", err
	}
	sumFile, err := fetch(ctx, c, base+"/"+asset+".sha256", 4096)
	if err != nil {
		return "", err
	}
	fields := strings.Fields(string(sumFile))
	if len(fields) == 0 {
		return "", errors.New("agent-hooks: empty .sha256 file")
	}
	got := sha256.Sum256(archive)
	if !strings.EqualFold(fields[0], hex.EncodeToString(got[:])) {
		return "", fmt.Errorf("agent-hooks: checksum mismatch for %s: expected %s, got %x", asset, fields[0], got)
	}
	bin, err := extract(archive, isZip, "agent-hooks-"+triple+"/"+exeName())
	if err != nil {
		return "", err
	}
	out := filepath.Join(dir, exeName())
	if err := os.WriteFile(out, bin, 0o755); err != nil {
		return "", err
	}
	return out, nil
}

// Init ensures the binary is installed at the shared location and registers
// hooks. It downloads and verifies the release asset only when needed.
func Init(ctx context.Context, opts Options) (Result, error) {
	bin := os.Getenv("AGENT_HOOKS_BIN")
	if bin == "" {
		if shared, err := sharedBinary(); err == nil {
			if st, err := os.Stat(shared); err == nil && !st.IsDir() &&
				(opts.MinVersion == "" || satisfies(ctx, shared, opts.MinVersion)) {
				bin = shared
			}
		}
	}
	if bin == "" {
		dir, err := os.MkdirTemp("", "agent-hooks-dl-")
		if err != nil {
			return Result{}, err
		}
		defer os.RemoveAll(dir)
		if bin, err = download(ctx, opts, dir); err != nil {
			return Result{}, err
		}
	}

	harness := opts.Harness
	if harness == "" {
		harness = "all"
	}
	args := []string{"self-install", "--json", "--harness", harness}
	if opts.NoHooks {
		args = append(args, "--no-hooks")
	}
	if opts.MinVersion != "" {
		args = append(args, "--min-version", opts.MinVersion)
	}
	if opts.Socket != "" {
		args = append(args, "--socket", opts.Socket)
	}
	var stdout, stderr bytes.Buffer
	cmd := exec.CommandContext(ctx, bin, args...)
	cmd.Stdout, cmd.Stderr = &stdout, &stderr
	if err := cmd.Run(); err != nil {
		return Result{}, fmt.Errorf("agent-hooks self-install failed: %w: %s", err, strings.TrimSpace(stderr.String()))
	}
	lines := strings.Split(strings.TrimSpace(stdout.String()), "\n")
	var res Result
	if err := json.Unmarshal([]byte(lines[len(lines)-1]), &res); err != nil {
		return Result{}, fmt.Errorf("agent-hooks: parse self-install output %q: %w", lines[len(lines)-1], err)
	}
	return res, nil
}
