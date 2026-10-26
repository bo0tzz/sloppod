# sloppod

A runtime for coding-agent sessions on Kubernetes. The design lives in the ADRs (`docs/adr`, indexed in `README.md`) and the current plan in `docs/mvp.md`. Read the ADRs that cover what you're changing before changing it; if the change contradicts one, say so instead of silently diverging.

## Layout

- `crates/contracts`: extension-point traits ([ADR 7](docs/adr/0007-extension-points.md)). Built-in implementations register through these like a plugin would; the core never branches on which implementation it has.
- `crates/protocol`: wire types for the runtime API and the supervisor link.
- `crates/runtime`: the `sloppod` binary. API, ACP proxy, authorizer, placement and enforcement.
- `crates/supervisor`: `sloppod-supervisor`, the session pod's entrypoint ([ADR 17](docs/adr/0017-in-pod-supervisor.md)).
- `crates/cli`: `sloppodctl`, a test client.
- `ui`: the web UI (SvelteKit SPA with @immich/ui), served by the runtime. `ui/mock` is a mock runtime for UI tests.
- `charts/sloppod`: the Helm chart. `Containerfile` builds both images (`--target runtime`, `--target session`).
- `docs/api.md` is the contract between the runtime and UIs; `docs/deploy.md` covers deploying.

## Working here

- Tools come from `mise.toml`. `mise run check` runs the format check, clippy and tests, the same as CI.
- UI: `cd ui && npm ci`, then `npm run check` and `npm run test:e2e` (Playwright against the mock runtime).
- End-to-end on a local cluster, where docker and kind are available:
  1. `dev/up.sh` creates a kind cluster with Cilium, agent-sandbox and Postgres.
  2. Create the `sloppod-claude` and `sloppod-github` Secrets in namespace `sloppod`, as in `docs/deploy.md` (`dev/values.yaml` expects those names, plus `sloppod-db`, which `up.sh` creates).
  3. `dev/deploy.sh` builds both images, loads them and installs the chart.
  4. Port-forward `svc/sloppod` (e.g. to 18080) and drive it with `sloppodctl` (`cargo build -p sloppod-cli`; set `SLOPPOD_URL`) or the UI. `dev/smoke.sh` runs the milestone checks; it pushes a `sloppod/<id>` branch to the repository and uses a few small model calls.
- Inside a sloppod session there's no container runtime, so stick to `mise run check` and the UI tests there.
- ADRs are MADR style. Numbers are permanent and append-only; add new ones to the README index under the right group.
- Only comment code for intent a reader couldn't infer.
