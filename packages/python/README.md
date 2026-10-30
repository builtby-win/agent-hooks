# builtby-win-agent-hooks

Python wrapper for [agent-hooks](https://github.com/builtby-win/agent-hooks). Wheels are platform-specific and contain the `agent-hooks` binary.

```sh
pip install builtby-win-agent-hooks
```

```python
import builtby_win_agent_hooks as ah

result = ah.init()          # installs ~/.agent-hooks/bin/agent-hooks, registers hooks
print(result["path"], result["action"])
ah.run(["--version"])
```

`init(harness="all", hooks=True, min_version=None, socket=None)` is idempotent. `binary_path()` returns the bundled binary (or `AGENT_HOOKS_BIN`).

Run without installing: `uvx --from builtby-win-agent-hooks agent-hooks --version`
