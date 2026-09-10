# Deterministic HTTP timeout fixture

The failed CI test combined a 40 ms global client deadline with a server that
required one accepted request within its own three-second deadline. A permitted
early transport timeout could leave the server waiting for a request that would
never arrive. The test then failed in fixture cleanup, after its latency and
transient-error assertions had passed. Sending the entire response after a
250 ms sleep also made the intended timeout phase dependent on scheduling.

This change is limited to test harness code. Product HTTP timeouts, transport,
retry classification, update integrity checks, and release policy are unchanged.

The timeout test now calls the production metadata-fetch function against a
dedicated loopback fixture. That fixture reads the request, sends valid headers,
and withholds the declared body until explicit cancellation. The short 40 ms
deadline applies to body reception; a five-second test-only global cap bounds
connection/setup failures. The required error is exactly
`ureq::Error::Timeout(ureq::Timeout::RecvBody)`, and it must still be classified
as retryable. A successful header exchange establishes acceptance before the
phase being tested. Connection/global timeouts, EOFs, and parse errors do not
satisfy this assertion.

Accept and partial-header reads poll cancellation instead of imposing an
independent accept deadline. Cleanup requests cancellation, waits at most two
seconds for completion, and joins only a finished worker; exceeding that bound
is an explicit fixture failure. `Drop` also requests bounded cleanup during
early return or assertion unwind. Normal tests finish the fixture before making
assertions so cleanup errors remain visible.

## Evidence

- Deterministic RED: zero global budget and cancellation before a gated client
  starts both reproduced the old accept-deadline panic (two failures, 3.01 s).
- Typed-timeout RED: the old response fixture produced a JSON parse error rather
  than `Timeout(RecvBody)` (one failure, 0.25 s).
- Focused GREEN covers those boundaries, an explicitly delayed real client,
  cancellation during partial request headers, listener release on assertion
  unwind, and 16 concurrent early/body-timeout cycles across four workers.
- `cargo test --locked --workspace` passed with default test parallelism:
  189 updater tests and four proxy-library tests, zero failures. Five existing
  explicit native/manual tests remained ignored; no test was newly ignored.

Logs in the implementation worktree: `target/http-fixture-red.log`,
`target/http-body-timeout-red.log`, `target/http-fixture-green.log`,
`target/http-fixture-final-focused.log`, and
`target/http-fixture-workspace-green.log`. The final focused run also verifies
the retained retryability assertion alongside the stricter timeout type.
