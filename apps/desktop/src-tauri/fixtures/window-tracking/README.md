# Window-tracking characterization fixtures (#742)

Rung A of the window-registry test strategy
(`internal/docs/WINDOW_REGISTRY_PLAN.md` §7.1). These pin the CURRENT behavior
of the CG-snapshot projections so the registry migration (#744) cannot change
it silently — the registry must replay these same fixtures through its
fixture-source ingest and produce the identical goldens.

## Files

- `session-a.jsonl` — a real recorded session (~30 Hz), captured with
  `PETAL_RECORD_WINDOW_FIXTURES=<dir>` on a **Screen-Recording-granted** build.
  Foreign window titles/app names are scrubbed to `w<number>`/`app<pid>` (PII);
  Petal's own windows keep real names so the own-chrome hit-test exercises.
- `synthetic-transitions.jsonl` — hand-built to force every decision branch
  (Shareable / BlockedByOwnProcess / BlockedByExternalWindow / NoHit /
  NoCursor; telepointer mover + visibility). Coordinates are integers.
- `*.golden.json` — the expected decision sequence per projection per fixture.

## Regenerating a golden (deliberate act only)

Goldens are behavior contracts. Changing one is a behavior change and must be
called out in the PR (plan §0 rule 6). To re-bless after an INTENTIONAL change:

    PETAL_BLESS_GOLDENS=1 cargo test --lib -- golden_over_recorded_session

Then review the git diff of the `.golden.json` before committing. A silent
re-bless to make a red test green is forbidden.

## Recording a new live fixture

    PETAL_RECORD_WINDOW_FIXTURES=~/tmp/fx PETAL_RECORD_WINDOW_FIXTURES_SECS=14 \
      ./target/debug/desktop
