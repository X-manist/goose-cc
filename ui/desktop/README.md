# goose-cc Desktop App

This is the Electron + React desktop app for the local `goose-cc` fork. Use it with this checkout's local Rust server binary when validating fork features.

## Prerequisites

From the repo root:

```bash
cd /Volumes/macsoftware/codes/agentscompany/shanmuagent/goose-cc

cargo build -p goose-server --bin goosed --features rustls-tls
```

Install UI dependencies from the `ui` workspace root, not from `ui/desktop`:

```bash
cd /Volumes/macsoftware/codes/agentscompany/shanmuagent/goose-cc/ui
pnpm install
```

The desktop package declares:

- Node: `^24.10.0`
- pnpm: `>=10.30.0`

Node 22 may run local smoke tests, but it will print an engine warning. Use Node 24 for clean validation.

## Start Desktop from Source

```bash
cd /Volumes/macsoftware/codes/agentscompany/shanmuagent/goose-cc

GOOSED_BINARY="$PWD/target/debug/goosed" \
GOOSE_LOCALE=zh-CN \
pnpm --dir ui/desktop run start-gui
```

For a debug port:

```bash
GOOSED_BINARY="$PWD/target/debug/goosed" \
GOOSE_LOCALE=zh-CN \
pnpm --dir ui/desktop run start-gui-debug
```

For isolated manual QA:

```bash
cd /Volumes/macsoftware/codes/agentscompany/shanmuagent/goose-cc
mkdir -p debug/manual-test/goose-home

GOOSE_PATH_ROOT="$PWD/debug/manual-test/goose-home" \
GOOSED_BINARY="$PWD/target/debug/goosed" \
GOOSE_LOCALE=zh-CN \
pnpm --dir ui/desktop run start-gui
```

## Provider Setup

You can configure providers in the desktop UI, or start with environment variables:

```bash
export GOOSE_PROVIDER=openai
export GOOSE_MODEL='your-model-name'
export OPENAI_API_KEY='your-api-key'
export OPENAI_BASE_URL='https://your-gateway.example/v1'
```

Then start the app:

```bash
GOOSED_BINARY="$PWD/target/debug/goosed" \
GOOSE_LOCALE=zh-CN \
pnpm --dir ui/desktop run start-gui
```

`OPENAI_BASE_URL` is supported by this fork for OpenAI-compatible gateways. `OPENAI_HOST` still works for older configs.

## Chinese UI

Force Simplified Chinese:

```bash
GOOSE_LOCALE=zh-CN pnpm --dir ui/desktop run start-gui
```

Supported desktop catalogs include `en`, `hi`, `ja`, `ru`, `tr`, and `zh-CN`. Simplified Chinese aliases resolve to `zh-CN`; Traditional Chinese currently falls back to English unless a catalog is added.

## Fork Feature QA

Desktop areas to test:

- Provider connection with OpenAI-compatible base URL.
- Chinese chat input and output.
- Permission profiles in Settings: Read-only, Guarded, Standard, YOLO.
- Permission rules modal from each configurable profile.
- Approval cards: success should appear only after backend delivery succeeds.
- Workspace menu: current directory and git worktree list.
- MCP app rendering.
- Subagent tool logs and subagent session link when available.

Useful checks:

```bash
CI=true pnpm --dir ui/desktop run typecheck
CI=true pnpm --dir ui/desktop exec vitest run src/components/ToolCallConfirmation.test.tsx
CI=true pnpm --dir ui/desktop exec vitest run src/i18n/i18n.test.ts
```

## pnpm 11 Troubleshooting

If startup fails during dependency install with:

```text
ERR_PNPM_EXOTIC_SUBDEP
Exotic dependency "@electron/node-gyp" ... is not allowed in subdependencies
```

run:

```bash
cd /Volumes/macsoftware/codes/agentscompany/shanmuagent/goose-cc/ui
pnpm install
```

This fork stores pnpm 11-compatible settings in:

- `ui/pnpm-workspace.yaml` for `nodeLinker`, `blockExoticSubdeps`, `supportedArchitectures`, `overrides`, `onlyBuiltDependencies`, and `allowBuilds`
- `ui/.npmrc` for registry and offline preference

Platform warnings for non-current `goose-binary-*` workspace packages are expected during local development because the workspace includes macOS, Linux, and Windows binary packages. The local development config keeps dependency resolution on the current platform so these warnings should not turn into long downloads or install failures. Cross-platform bundle scripts prepare platform binaries separately.

If Forge fails with:

```text
Electron failed to install correctly, please delete node_modules/electron and try installing again
```

run:

```bash
pnpm --dir ui/desktop run ensure-electron
```

The start and packaging scripts run this automatically. It repairs pnpm hoisted installs where Electron's binary exists in `ui/node_modules/electron` but Forge resolves the nested `.pnpm/electron@...` package.

If the app opens to `Cannot read properties of null (reading 'useRef')`, clear the Vite optimized dependency cache:

```bash
rm -rf ui/desktop/node_modules/.vite
```

The renderer Vite config dedupes `react` and `react-dom`; clearing the cache removes older optimized chunks created before that config was applied.

## Packaging

Development testing should use `start-gui`. Packaging commands are still available:

```bash
pnpm --dir ui/desktop run package
pnpm --dir ui/desktop run make
pnpm --dir ui/desktop run bundle:default
```

macOS signing/notarization still depends on the environment variables referenced by Electron Forge config. Do not put API keys into a bundled app; configure users through provider setup or environment/config files.
