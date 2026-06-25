# goose-cc

`goose-cc` is our Goose-based Rust coding-agent fork. Test this checkout from source; do not use the upstream release installer when validating fork features.

The fork keeps Goose's Rust CLI, terminal session UI, Electron desktop app, MCP support, and multi-provider model layer, then adds coding-agent engineering features we need for Claude-Code-like workflows:

- TUI slash commands: `/plan`, `/review`, `/debug`, `/compact`, `/resume`, `/status`, `/cost`, `/doctor`, `/worktree`.
- Permission profiles: `readonly`, `guarded`, `standard`, `yolo`.
- Context Core compaction: large tool outputs are stored as session artifacts and exposed through `context_core_read` / `context_core_search`.
- Session resume that restores conversation history and the session working directory, including worktree sessions.
- Review/check subagents for read-only exploration, code review, and test diagnosis.
- Manual and automatic git worktree isolation for long write-like tasks.
- Desktop permission/profile UI, worktree picker support, and simplified Chinese UI.

## Repository

```bash
cd /Volumes/macsoftware/codes/agentscompany/shanmuagent/goose-cc
```

This README assumes the current repo has already been cloned and Rust/Node dependencies are available locally.

## Build CLI and Server

Use the local binaries under `target/`; these are the fork binaries.

```bash
cargo build -p goose-cli --bin goose --no-default-features --features rustls-tls
cargo build -p goose-server --bin goosed --features rustls-tls

./target/debug/goose --help
./target/debug/goosed --help
```

For broad local validation:

```bash
cargo fmt --all -- --check
cargo check -p goose-cli --no-default-features --features rustls-tls
cargo check -p goose-server --features rustls-tls
```

## Configure Provider and API Key

Interactive setup:

```bash
./target/debug/goose configure
```

For OpenAI-compatible gateways, environment variables are the fastest test path:

```bash
export GOOSE_PROVIDER=openai
export GOOSE_MODEL='your-model-name'
export OPENAI_API_KEY='your-api-key'
export OPENAI_BASE_URL='https://your-gateway.example/v1'
```

Then smoke-test the model:

```bash
./target/debug/goose run --no-session --with-builtin developer \
  --provider openai \
  --model "$GOOSE_MODEL" \
  --system "Always answer in Simplified Chinese." \
  -t "用中文简短回复：当前模型是否连接成功？"
```

Notes:

- `OPENAI_BASE_URL` is supported by this fork and accepts normal `/v1` OpenAI-compatible endpoints.
- `OPENAI_HOST` is still honored for older Goose configs and scripts, but `OPENAI_BASE_URL` is preferred for gateway testing.
- `GOOSE_PROVIDER` and `GOOSE_MODEL` can be overridden per run with `--provider` and `--model`.
- The parent project `.env` may use keys such as `model_provider`, `model`, `base_url`, and `OPENAI_API_KEY`; map them to `GOOSE_PROVIDER`, `GOOSE_MODEL`, `OPENAI_BASE_URL`, and `OPENAI_API_KEY` before running Goose.

## Terminal and TUI Testing

Start an interactive TUI session:

```bash
./target/debug/goose session --with-builtin developer
```

Useful TUI commands:

```text
/?
/mode readonly
/mode guarded
/mode standard
/mode yolo
/status
/cost
/doctor
/debug on
/compact
/review --summary-only --dry-run
/resume --history
/worktree status
/worktree start demo-task --base HEAD
/quit
```

Resume a previous session from the shell:

```bash
./target/debug/goose session --resume --history
./target/debug/goose session --resume --session-id <session-id> --history
./target/debug/goose session list
```

Run a one-shot task:

```bash
./target/debug/goose run --with-builtin developer \
  --provider openai \
  --model "$GOOSE_MODEL" \
  -t "只读分析当前仓库的模块结构，用中文回答。"
```

Run a review from the shell:

```bash
./target/debug/goose review --dry-run
./target/debug/goose review main...HEAD --severity medium
```

Terminal-integrated sessions are available through `goose term`:

```bash
eval "$(./target/debug/goose term init zsh)"
./target/debug/goose term run "用中文总结当前目录"
```

## Worktree Mode

Manual worktree creation inside TUI:

```text
/worktree start long-task --base HEAD
```

Automatic worktree mode for long write-like prompts:

```bash
GOOSE_AUTO_WORKTREE=1 ./target/debug/goose run --with-builtin developer \
  --provider openai \
  --model "$GOOSE_MODEL" \
  -t "修复构建失败并运行测试"
```

Behavior to verify:

- A sibling git worktree is created on a `goose/<session>-<label>` branch.
- The session working directory switches to that worktree.
- Running inside an existing linked worktree is detected and not nested.
- `--quiet` suppresses auto-worktree status chatter.

## Context Core

Large tool outputs are compacted into session artifacts. The model-visible summary includes an `artifact_path` and tells the model to use:

- `context_core_read` for slices of raw artifact output.
- `context_core_search` for finding specific lines before reading.

This keeps token usage lower while still preserving exact raw tool output under the current session's storage.

## Desktop App from This Fork

Install UI dependencies from the `ui` workspace:

```bash
cd /Volumes/macsoftware/codes/agentscompany/shanmuagent/goose-cc/ui
pnpm install
```

Run the desktop app against the fork's local `goosed` binary:

```bash
cd /Volumes/macsoftware/codes/agentscompany/shanmuagent/goose-cc

GOOSED_BINARY="$PWD/target/debug/goosed" \
GOOSE_LOCALE=zh-CN \
pnpm --dir ui/desktop run start-gui
```

For isolated manual QA that does not touch your normal Goose config:

```bash
cd /Volumes/macsoftware/codes/agentscompany/shanmuagent/goose-cc

mkdir -p debug/manual-test/goose-home

GOOSE_PATH_ROOT="$PWD/debug/manual-test/goose-home" \
GOOSED_BINARY="$PWD/target/debug/goosed" \
GOOSE_LOCALE=zh-CN \
pnpm --dir ui/desktop run start-gui
```

Desktop things to test:

- Provider setup accepts OpenAI-compatible base URLs.
- Chat can send and receive Chinese prompts.
- Settings show permission profiles: Read-only, Guarded, Standard, YOLO.
- Permission rules modal opens from profile rows.
- Tool approval cards do not show success until backend confirmation delivery succeeds.
- Workspace menu lists git worktrees.
- Subagent tool logs show subagent activity and allow opening subagent sessions when available.

More desktop-specific details are in [ui/desktop/README.md](ui/desktop/README.md).

## Chinese Language Support

Desktop UI supports Simplified Chinese. Force it with:

```bash
GOOSE_LOCALE=zh-CN pnpm --dir ui/desktop run start-gui
```

Supported desktop translation catalogs include `en`, `hi`, `ja`, `ru`, `tr`, and `zh-CN`. Simplified Chinese aliases such as `zh`, `zh-CN`, `zh-Hans`, `zh-SG`, and `zh-MY` resolve to `zh-CN`. Traditional Chinese variants currently fall back to English unless a catalog is added.

CLI/TUI command names and help text are mostly English, but Chinese prompts and Chinese model replies work normally. For one-shot runs, add a system instruction:

```bash
./target/debug/goose run --system "始终用简体中文回答。" -t "介绍当前项目。"
```

## Testing Checklist

Core Rust checks:

```bash
cargo fmt --all -- --check
git diff --check
cargo test -p goose --features rustls-tls tool_confirmation_router
cargo test -p goose --features rustls-tls subagent_handler
cargo test -p goose --features rustls-tls handle_confirmation
cargo test -p goose --features rustls-tls worktree
cargo test -p goose-cli --no-default-features --features rustls-tls auto_worktree
cargo test -p goose-server --features rustls-tls action_required
```

Desktop checks:

```bash
CI=true pnpm --dir ui/desktop run typecheck
CI=true pnpm --dir ui/desktop exec vitest run src/components/ToolCallConfirmation.test.tsx
CI=true pnpm --dir ui/desktop exec vitest run src/i18n/i18n.test.ts
```

Manual E2E evidence from previous rounds is under:

- [debug/10-approval-worktree/test-report.md](debug/10-approval-worktree/test-report.md)
- [debug/10-approval-worktree/residual-risk-fix-report.md](debug/10-approval-worktree/residual-risk-fix-report.md)
- [debug/12-worktree-resume-retry/reports/ui-click-worktree-resume-report.md](debug/12-worktree-resume-retry/reports/ui-click-worktree-resume-report.md)

## Troubleshooting

### `ERR_PNPM_EXOTIC_SUBDEP`

If desktop startup fails with:

```text
ERR_PNPM_EXOTIC_SUBDEP
Exotic dependency "@electron/node-gyp" ... is not allowed in subdependencies
```

the cause is pnpm 11's default `blockExoticSubdeps=true` combined with Electron Forge's git-hosted `@electron/node-gyp` dependency. This fork stores pnpm 11-compatible settings in:

- [ui/pnpm-workspace.yaml](ui/pnpm-workspace.yaml) for `nodeLinker`, `blockExoticSubdeps`, `supportedArchitectures`, `overrides`, `onlyBuiltDependencies`, and `allowBuilds`
- [ui/.npmrc](ui/.npmrc) for registry and offline preference

Run from `ui` once:

```bash
cd /Volumes/macsoftware/codes/agentscompany/shanmuagent/goose-cc/ui
pnpm install
```

### Electron Binary Not Found

If desktop startup reaches Electron Forge but then fails with:

```text
Electron failed to install correctly, please delete node_modules/electron and try installing again
```

the dependency install finished, but pnpm's hoisted layout placed Electron's downloaded binary under `ui/node_modules/electron` while Electron Forge resolved the nested `.pnpm/electron@.../node_modules/electron` package. The desktop scripts now run:

```bash
pnpm --dir ui/desktop run ensure-electron
```

before `start-gui`, `start-gui-debug`, `package`, and `make`. If you started Forge manually, run that command once before retrying.

### React `useRef` Startup Crash

If the desktop window opens to:

```text
Cannot read properties of null (reading 'useRef')
```

clear the stale Vite optimized dependency cache:

```bash
rm -rf ui/desktop/node_modules/.vite
```

This fork's renderer Vite config now uses the React plugin and explicitly dedupes `react` / `react-dom` so pnpm hoisting does not create inconsistent optimized React chunks.

### Node Version Warning

The desktop package declares Node `^24.10.0`. Node 22 may still run local smoke tests, but use Node 24 for clean desktop validation:

```bash
node -v
pnpm -v
```

### Platform Warnings

Warnings for non-current `goose-binary-*` workspace packages are expected because the UI workspace includes macOS, Linux, and Windows binary packages. The local development config keeps dependency resolution on the current platform so these warnings should not turn into long downloads or install failures.

### Use Fork Binaries

For fork validation, prefer:

```bash
./target/debug/goose
./target/debug/goosed
```

Do not validate fork features with an upstream `goose` installed from a release script unless you intentionally want an upstream comparison.
