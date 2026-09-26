"""Read-only exact-commit validation gate for automated release publication."""
import argparse
import json
import re
import subprocess
import sys
import time
from urllib.parse import urlencode

REPOSITORY = "chenjicheng/upmc"
# The tag workflow builds the actual release artifact. Only the independent
# PR/main validation is required here; CodeQL remains an advisory scan.
# Resolve the exact workflow, event, commit and attempt, not a generic green badge.
POLICIES = (
    (".github/workflows/validate-updater.yml", "push", ("UPMC tests",), ()),
)


class ReleaseGateError(RuntimeError):
    pass


class ReleaseGatePending(ReleaseGateError):
    """The canonical validation may still finish; it has not authorized release."""


def _require(condition, message):
    if not condition:
        raise ReleaseGateError(message)


def _positive(value):
    return type(value) is int and value > 0


def _api(endpoint, *, timeout=45):
    result = subprocess.run(
        ["gh", "api", "--hostname", "github.com", "--method", "GET", endpoint,
         "-H", "Accept: application/vnd.github+json", "-H", "X-GitHub-Api-Version: 2022-11-28"],
        # Decode in the calling thread. Windows subprocess text-mode readers can
        # otherwise fail in a background thread under the system ANSI code page.
        capture_output=True, timeout=timeout,
    )
    try:
        stdout = result.stdout.decode("utf-8", errors="strict")
        stderr = result.stderr.decode("utf-8", errors="strict")
    except UnicodeDecodeError as error:
        raise ReleaseGateError("GitHub validation response is not valid UTF-8") from error
    _require(result.returncode == 0, f"GitHub validation lookup failed: {stderr.strip()}")
    return json.loads(stdout)


def _collection(api, endpoint, field):
    items = []
    total = None
    for page in range(1, 11):
        separator = "&" if "?" in endpoint else "?"
        response = api(f"{endpoint}{separator}per_page=100&page={page}")
        _require(isinstance(response, dict), f"Malformed {field} response")
        count, batch = response.get("total_count"), response.get(field)
        _require(type(count) is int and 0 <= count <= 1000 and isinstance(batch, list)
                 and len(batch) <= 100 and all(isinstance(item, dict) for item in batch),
                 f"Incomplete or oversized {field} response")
        if total is None:
            total = count
        _require(count == total, f"{field} changed during pagination; retry after validation settles")
        items.extend(batch)
        _require(len(items) <= total, f"Inconsistent {field} count")
        if len(items) == total:
            ids = [item.get("id") for item in items]
            _require(all(_positive(item) for item in ids) and len(set(ids)) == len(ids),
                     f"Invalid or duplicate {field} identity")
            return items
        _require(len(batch) == 100, f"Missing {field} page data")
    raise ReleaseGateError(f"Cannot prove complete {field} pagination")


def _validate_run(run, workflow, path, event, sha):
    _require(isinstance(run, dict), f"Missing run for {path}")
    _require(all(_positive(run.get(key)) for key in ("id", "run_number", "run_attempt", "workflow_id")), f"Invalid run identity for {path}")
    _require(run.get("workflow_id") == workflow and run.get("path") in
             (path, path + "@main", path + "@refs/heads/main"), f"Wrong workflow identity for {path}")
    _require(run.get("event") == event and run.get("head_branch") == "main"
             and run.get("head_sha") == sha, f"Run is not canonical {event}/main at {sha}: {path}")
    for key in ("repository", "head_repository"):
        _require(isinstance(run.get(key), dict) and run[key].get("full_name") == REPOSITORY,
                 f"Wrong {key} for {path}")


def _latest(api, workflow, path, event, sha):
    query = urlencode({"event": event, "branch": "main", "head_sha": sha})
    runs = _collection(api, f"repos/{REPOSITORY}/actions/workflows/{workflow}/runs?{query}", "workflow_runs")
    if not runs:
        raise ReleaseGatePending(f"No canonical {event}/main validation exists for {path} at {sha}")
    for run in runs:
        _validate_run(run, workflow, path, event, sha)
    # GitHub increments run_number for new runs of this workflow; run IDs are
    # opaque identifiers. A re-run keeps its number and increments run_attempt.
    _require(len({run["run_number"] for run in runs}) == len(runs), f"Ambiguous workflow run ordering for {path}")
    return max(runs, key=lambda run: run["run_number"])


def _successful(run, path):
    if run.get("status") in {"queued", "in_progress", "waiting", "requested", "pending"} and run.get("conclusion") is None:
        raise ReleaseGatePending(f"Waiting for {path} run {run.get('id')} attempt {run.get('run_attempt')}")
    _require(run.get("status") == "completed" and run.get("conclusion") == "success",
             f"Required validation is not green: {path} run {run.get('id')} "
             f"attempt {run.get('run_attempt')} ({run.get('status')}/{run.get('conclusion')})")


def require_green(sha, api=None):
    """Require current successful canonical runs and their jobs, or raise.

    The injected transport is an in-process test seam, not a CLI override. Missing
    data, pending work, API errors and changed attempts never fall back to an old
    green result. This reads status only; it never triggers a workflow or release.
    """
    _require(isinstance(sha, str) and re.fullmatch(r"[0-9a-f]{40}", sha), "A full lowercase commit SHA is required")
    api = api or _api
    try:
        workflows = _collection(api, f"repos/{REPOSITORY}/actions/workflows", "workflows")
        evidence = []
        for path, event, required_jobs, permitted_skips in POLICIES:
            matches = [workflow for workflow in workflows if workflow.get("path") == path]
            _require(len(matches) == 1 and matches[0].get("state") == "active", f"Required workflow missing, ambiguous or disabled: {path}")
            workflow = matches[0]["id"]
            run = _latest(api, workflow, path, event, sha)
            _successful(run, path)
            run_id, attempt = run["id"], run["run_attempt"]
            jobs = _collection(api, f"repos/{REPOSITORY}/actions/runs/{run_id}/attempts/{attempt}/jobs", "jobs")
            names = [job.get("name") for job in jobs]
            _require(all(isinstance(name, str) and name for name in names) and len(names) == len(set(names)),
                     f"Invalid or duplicate job names in {path}")
            _require(set(required_jobs).issubset(names), f"Required test/analysis job missing from {path}")
            for job in jobs:
                _require(job.get("head_sha") == sha and job.get("run_id") == run_id
                         and type(job.get("run_attempt")) is int and job.get("run_attempt") == attempt,
                         f"Stale or foreign job in {path}")
                allowed = job.get("conclusion") == "success" or (
                    job["name"] in permitted_skips and job.get("conclusion") == "skipped")
                _require(job.get("status") == "completed" and allowed, f"Job is not green: {path} / {job['name']}")
            fresh = api(f"repos/{REPOSITORY}/actions/runs/{run_id}")
            _validate_run(fresh, workflow, path, event, sha)
            _successful(fresh, path)
            _require((fresh["id"], fresh["run_number"], fresh["run_attempt"]) ==
                     (run_id, run["run_number"], attempt), f"Validation identity changed while checking {path}")
            newest = _latest(api, workflow, path, event, sha)
            _successful(newest, path)
            _require((newest["id"], newest["run_attempt"]) == (run_id, attempt), f"New validation appeared while checking {path}")
            evidence.append({"workflow": path, "run_id": run_id, "attempt": attempt, "sha": sha, "jobs": names})
        return evidence
    except ReleaseGateError:
        raise
    except (OSError, ValueError, TypeError, KeyError, subprocess.TimeoutExpired) as error:
        raise ReleaseGateError(f"Cannot establish required validation: {error}") from error


def wait_for_green(sha, *, api=None, timeout_seconds=900, poll_seconds=15):
    """Let tag builds overlap main tests, with a bounded wait before any write.

    Only absent or still-running canonical runs are retried. Terminal failures,
    stale jobs and API errors fail immediately. Each poll revalidates the newest
    run and attempt instead of retaining earlier successful evidence. API calls
    share the deadline, including their subprocess timeout.
    """
    _require(type(timeout_seconds) is int and 0 <= timeout_seconds <= 1800,
             "Validation wait must be between 0 and 1800 seconds")
    _require(type(poll_seconds) is int and poll_seconds > 0, "Poll interval must be positive")
    if timeout_seconds == 0:
        return require_green(sha, api)
    deadline = time.monotonic() + timeout_seconds
    last_message = None

    def remaining_budget():
        remaining = deadline - time.monotonic()
        _require(remaining > 0, "Timed out waiting for required validation")
        return remaining

    def bounded_api(endpoint):
        remaining = remaining_budget()
        result = api(endpoint) if api is not None else _api(endpoint, timeout=min(45, remaining))
        remaining_budget()
        return result

    while True:
        try:
            evidence = require_green(sha, bounded_api)
            remaining_budget()
            return evidence
        except ReleaseGatePending as error:
            remaining = remaining_budget()
            if str(error) != last_message:
                print(str(error), file=sys.stderr, flush=True)
                last_message = str(error)
            time.sleep(min(poll_seconds, remaining))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sha", required=True)
    parser.add_argument("--wait-seconds", type=int, default=0,
                        help="Wait only for pending canonical tests (default: do not wait)")
    args = parser.parse_args()
    try:
        print(json.dumps(wait_for_green(args.sha, timeout_seconds=args.wait_seconds), indent=2))
        return 0
    except ReleaseGateError as error:
        print(f"Release blocked: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
