# Required validation before release publication

`upmc/Cargo.toml` remains the version source. This change adds release authorization and does not change a version, tag, artifact identity, frozen first-hop descriptor, or predecessor policy.

## Merge check

`.github/workflows/validate-updater.yml` runs on pull requests targeting `main` and pushes to `main`. Its unique job name is **UPMC tests**. It runs every Python publication regression, builds the matched embedded DLL pair, then runs the locked Rust workspace test suite. It has read-only repository permissions, no secrets, no publication job, and no tag/manual trigger. Interactive tests explicitly excluded from CI remain separate manual acceptance work.

The `main` ruleset requires `UPMC tests` from the GitHub Actions app (integration ID `15368`), with strict/up-to-date checks. The ruleset is a server-side configuration; merely adding this workflow does not configure branch protection. Existing review and CodeQL rules remain separate requirements.

## Publication gate

The current `release_publication.py publish` CLI calls `release_test_gate.require_green` after source/tag validation and before calling either Release or Pages mutation code. Therefore calling the actual publisher locally does not skip the validation gate. The read-only gate has no workflow, event, branch, repository or success override on its CLI.

For the exact release commit SHA, all of these canonical validations must exist and be successful:

| Registered workflow path | Required event / branch | Required jobs |
| --- | --- | --- |
| `.github/workflows/validate-updater.yml` | `push` / `main` | `UPMC tests` |
| `.github/workflows/release-slint.yml` | `push` / `main` | `build` |
| `dynamic/github-code-scanning/codeql` | `dynamic` / `main` | `Analyze (actions)`, `Analyze (rust)`, `Analyze (python)` |

Workflow IDs are discovered by exact registered path and active state. No generic `build` check from a tag, a manual run, a PR, another repository, or another commit can substitute. For each workflow, the greatest workflow `run_number` is authoritative; an older green run never overrides a newer failed or unfinished run. The latest run must be completed successfully, its current-attempt jobs must identify the same SHA/run/attempt, and required job names must occur exactly once. Every returned job must succeed, except the canonical main release workflow's deliberately skipped `publish` job. CodeQL execution success is checked here; alert/severity policy remains enforced by the existing code-scanning ruleset.

The gate reads all API pages (bounded to the API's 1,000-result search limit), rejects malformed/incomplete/ambiguous responses, and re-reads run identity and latest-run selection after reading jobs. Missing, queued, pending, failed, cancelled, timed-out, skipped, neutral or unavailable validation blocks publication. API errors do not fall back to older evidence. These are point-in-time status checks, not an atomic lock on future GitHub reruns or administrator actions.

Only a tag-push artifact build can reach the current publish job. That build still runs its own tests. Canonical main runs do not invoke the gate, and their skipped publish job is explicitly allowed, avoiding a circular dependency. If a tag run arrives before its main validations finish, publication fails closed; wait for all canonical main validations to finish and rerun the tag workflow. Manual workflow dispatch remains build-only.

The frozen historical `v0.4.8` workflow/publisher and legacy readers are unchanged. Historical revisions cannot be retroactively rewritten with a new gate. Existing immutable-release/source/Pages lease safeguards remain in place.

## Validation and administrative boundary

The read-only diagnostic command is:

```powershell
python scripts/release_test_gate.py --sha <full-lowercase-commit-sha>
```

It only reads GitHub Actions metadata and prints accepted run evidence or a blocking error. It does not create a tag, workflow run, Release, asset, or Pages commit. Regression fixtures cover wrong-event substitution, exact-SHA selection, non-green/missing states, latest-run and attempt changes, pagination, malformed responses, failed main build/CodeQL, and publisher mutation ordering. They execute no remote publication.

Branch rules and this workflow cannot universally intercept a repository administrator's manual GitHub Release UI/API operation or execution of an old checkout. Restricting repository write/admin access and protecting release tags are separate administrative controls. The claim here is that the current automated publisher fails closed unless its required validations are green; it is not a claim that administrators cannot bypass repository automation.

API references: [workflow runs](https://docs.github.com/en/rest/actions/workflow-runs) and [workflow jobs](https://docs.github.com/en/rest/actions/workflow-jobs).
