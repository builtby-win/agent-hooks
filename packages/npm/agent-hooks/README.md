# builtby.win-agent-hooks

Universal lifecycle hooks for AI coding agents, with the native binary included.

```bash
npm i builtby.win-agent-hooks
```

```js
const { init } = require("builtby.win-agent-hooks");

const { path, action, version } = await init(); // installs to ~/.agent-hooks/bin, registers hooks
```

`init()` is idempotent (newest version wins across apps), so call it on every launch.
Pass `{ hooks: false }` to place the binary without touching harness configs.

**Electron:** executables can't run from `app.asar`. Add
`asarUnpack: ["**/node_modules/builtby.win-agent-hooks-*/**"]` to electron-builder. For
macOS universal or cross-arch builds, install both platform packages
(`npm i --force --os=darwin --cpu=x64 builtby.win-agent-hooks-darwin-x64`).

CLI: `npx builtby.win-agent-hooks <args>`. Docs: https://github.com/builtby-win/agent-hooks
