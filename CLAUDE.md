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

- Tools come from `mise.toml`. `mise run check` runs the format check, clippy and tests, the same as CI. Tests run against a throwaway Postgres (`dev/with-db.sh`); database tests use `#[sqlx::test]`, which gives each test a fresh migrated database.
- UI: `cd ui && npm ci`, then `npm run check` and `npm run test:e2e` (Playwright against the mock runtime).
- End-to-end on a local cluster, where docker and kind are available:
  `mise run dev:all` does steps 1–3 in one go.
  1. `dev/up.sh` creates a kind cluster with Cilium, agent-sandbox and Postgres, plus the `sloppod-db` and `sloppod-master-key` Secrets.
  2. `dev/deploy.sh` builds both images, loads them and installs the chart.
  3. Port-forward `svc/sloppod` (e.g. to 18080) and drive it with `sloppodctl` (`cargo build -p sloppod-cli`; set `SLOPPOD_URL`) or the UI. Requests act as the user `dev`.
  4. Add a Claude account and a GitHub App under Settings, or with `sloppodctl add-credential` (see `docs/deploy.md`); they're stored in the database, not in Secrets.
  5. `dev/smoke.sh` runs the milestone checks; it pushes a `sloppod/<id>` branch to the repository and uses a few small model calls. Set the model to Haiku in Settings first to keep them cheap.
- Inside a sloppod session there's no container runtime, so stick to `mise run check` and the UI tests there. The UI tests need a browser first: `npx playwright install chromium` (its download can be slow).
- Keep the docs in step with the code: a PR that changes behaviour described in an ADR, `docs/*.md` or this file updates them in the same PR. If a change contradicts an ADR, amend the ADR in the same PR (with a dated note under More Information) or say in the PR why it doesn't need to.
- ADRs are MADR style. Numbers are permanent and append-only; add new ones to the README index under the right group.
- Only comment code for intent a reader couldn't infer.
