## What this changes

<!-- The behavior difference, not the file list. -->

## How it was verified

<!-- What you actually ran and watched pass. -->

- [ ] `scripts/ci-local.sh` is green
- [ ] Tests covering the change land in this PR

## Checklist

- [ ] If wire formats changed: Rust, backend, browser client, and
      `contracts/petal-contracts.json` all updated together
- [ ] If the desktop UI changed: mirrored to the browser client
- [ ] If user-facing text changed: it fits at the real window width, untruncated
- [ ] If native window lifecycle changed: a test drives the real event path,
      not just a pure helper
- [ ] If a vendored dependency changed: its `PETAL_PATCH.md` is updated
