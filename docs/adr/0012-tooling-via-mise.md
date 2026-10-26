# 12. Tools come from mise via a trusted installer

- Status: Accepted
- Date: 2026-10-26

## Context and Problem Statement

Sessions need CLI tools: some from the project, some from their capabilities, such as `kubectl` for a cluster capability. Baking tools into the image makes it specific to its user and stale. A shared, writable tool cache shared between sessions can be poisoned by any one of them.

## Decision Outcome

Tools are first-class [mise](https://mise.jdx.dev) tools.

- **Tool set:** a session's tool set is the project's `mise.toml` plus the mise fragments of its granted capabilities.
- **Image:** the base image is minimal: OS, mise, and the agent adapter.
- **Installs:** a **trusted installer** in the runtime is the only writer to a **read-only tool store**. It installs only locked versions, verified against lockfile checksums and provenance (`MISE_LOCKED=1`, `MISE_LOCKED_VERIFY_PROVENANCE=1`; `mise.lock` records per-platform URLs, checksums and SLSA/cosign/attestations).
- **In sessions:** sessions mount the store read-only. A missing tool or version is just another capability request.
- **Package managers** (npm/pnpm, Go modules, PyPI, container images) use pull-through caches. Lockfile integrity hashes make a poisoned cache detectable.

### Consequences

- Good: no tool-specific images, and a session can't affect another session's tools.
- Bad: tools without a lock entry or checksum need the installer to compute and pin them first.

## More Information

Spike, 2026-10-26, mise 2026.10.18:

- **Read-only store:** a session with the store mounted read-only ran the locked tool.
- **Unlocked versions:** requesting a version not in the lockfile failed without installing anything.
- **Poisoned cache:** after a cached download was replaced with a fake script, a fresh install detected the mismatch (the download cache is content-addressed with blake3), downloaded again, and installed a binary matching the lockfile checksum.
