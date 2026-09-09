# Legacy first-hop post-health cleanup

## Scope

A candidate launched by an older `upmc-update-helper*.exe` can clean the incoming helper and `.exe.old` after its health acknowledgment succeeds and the actual helper parent exits successfully. This needs no new marker emitted by the older helper.

The candidate opens its parent with query and synchronization rights before acknowledging health, checks the recognized canonical same-directory helper image and parent creation order, and retains that exact native handle until waiting finishes. Ordinary and `self_update` parents are ignored. Failure to capture a proof is logged and does not suppress the existing health acknowledgment.

The proof records volume/file IDs and executable size/SHA256 for the candidate, rollback backup, and helper. The helper must match the backup contents. Cleanup requires a signaled parent handle with exit code zero, acquires the persistent update transaction lock, and rechecks every identity. New `.exe.new`, `.exe.old.pending`, or any other recognized helper causes retention. The exact helper is removed first; if its image is still locked, the backup is retained. Cleanup never scans away unrelated files or library-owned self-replacement artifacts, and never removes the persistent lock.

Timeout, exit-query failure, nonzero exit, lock contention, changed identities, and filesystem failures retain remaining artifacts with a structured diagnostic. The wait budget is 120 seconds on the existing background health watcher.

## Integration dependency

The concurrent library migration must call its `library::ensure_process_can_update()` guard immediately after this module obtains the transaction lock. That module is not present in this branch's baseline. This prevents an already-relocated process from applying a deferred receipt even if other file checks would pass. Both update implementations use the existing persistent lock.

## Validation evidence

Baseline: `4bc0ee964d003eca846e5396ef105b81ca7e41e0`.

- Built both embedded release DLLs in this worktree before the first client test.
- Behavioral RED: four executable tests failed for expected missing behavior: successful cleanup, helper/backup matching, failed-exit retention, and acknowledgment independence. They passed after implementation.
- Native RED: the copied test-executable helper-to-candidate fixture failed because its legacy parent was not captured. It passed after native handle capture and waiting were implemented.
- Health gate RED: a successfully committed acknowledgment did not report success through the new return value. It passed after the watcher returned true only for a successful write.
- GREEN: `cargo test -p upmc selfupdate -- --test-threads=3`: 88 passed, 0 failed, 2 pre-existing subprocess fixtures ignored.
- Native coverage includes a live mapped helper, zero and nonzero parent exits, a retained handle after parent exit, and a real synchronization-only handle whose exit-code query is denied.
- File coverage includes changed target/backup, same-content replacement with a different native file ID, wrong helper contents/name, staging, another helper, lock contention, and preservation of unrelated files and the persistent lock.
- `git diff --check` passed.

The native fixture simulates the established old-helper exit contract with isolated test executables. It does not launch the published 0.4.8 or 0.5.1 updater normally, mutate a real installation, or access Discord state. Combined migration review and release acceptance remain separate integration gates.
