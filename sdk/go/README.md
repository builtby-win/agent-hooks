# agent-hooks Go SDK

```sh
go get github.com/builtby-win/agent-hooks/sdk/go
```

```go
import agenthooks "github.com/builtby-win/agent-hooks/sdk/go"

res, err := agenthooks.Init(ctx, agenthooks.Options{MinVersion: "0.1.0"})
// res.Path, res.Action ("installed|upgraded|kept|current"), res.HooksInstalled
```

Go cannot vendor platform binaries, so `Init` downloads the matching GitHub release
asset on first use, verifies its `.sha256`, and runs `agent-hooks self-install --json`.
Unverified bytes are never executed. Later calls reuse the installed binary.

The shared path (`~/.agent-hooks/bin/agent-hooks`, or `$AGENT_HOOKS_HOME/bin`) is where
hooks point; `BinaryPath()` returns it. Set `AGENT_HOOKS_BIN` to use your own binary.
Options: `Version`, `BaseURL` (mirror), `Harness`, `NoHooks`, `MinVersion`, `Socket`, `HTTPClient`.
