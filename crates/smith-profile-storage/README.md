# Smith Next encrypted profile storage

This crate supplies the application's SQLCipher connection and the migration backend used by the automatic local-workspace host. It has no profile discovery, keychain access, network access, command-line key input or secret environment configuration. Its public errors contain static messages, never SQL values, paths or secrets.

The normal filename is `profile.sqlite3`. `create(path, &Secret)` exclusively publishes a new encrypted schema-11 profile; `unlock(path, &Secret)` authenticates an existing encrypted profile and rejects plaintext, damaged files, unknown application identities, schema changes and missing write-history protection. SQLCipher is keyed through its C API before any database read. An actual supported `cipher_version` is required. The same rusqlite 0.37 connection is returned to the app; there is no separate plaintext repository.

`Secret::new(String)` takes ownership of a zeroizing passphrase. It cannot be cloned, debug-printed or serialized. The host must consume it for the requested operation and drop it promptly. SQLCipher retains connection key state until the connection closes. Root request handlers must not log or persist passphrase request bodies.

The host owns the exclusive profile lock and request/task lifecycle. It must stop admission, cancel and drain background work, relinquish every application connection owner, then call `close`. A failed checkpoint or close returns `CloseFailure::into_parts() -> (Connection, Error)` so the host can retain the connection in a fault state. It must not report the profile locked or reopen another connection while ownership is unresolved. A successful close checkpoints and closes the actual connection. `recover_interrupted` is an explicit operation after unlocking and parsing saved app configuration; it does not run during verification or migration.

`initialize_schema` is public for deliberately constructed synthetic fixtures. It does not establish encryption and is not a normal application opening API. Schema 6 adds encrypted drafts, media originals and derived data, agents, Trash and undo/redo metadata while retaining existing conversations, attachments and artifacts. Additive tables share the same transaction boundary, foreign-key policy and write-generation tracking.

Schema 7 adds dormant original-import evidence, scoped agent resources, tool receipts, memories and explicit host-operation audit records. Authenticated schema-6 unlock verifies and rehearses the additive upgrade privately, then commits the schema, guards, signature and incremented write generation atomically.

Schema 11 adds `run_model_provenance` for bounded, versioned requested inference intent. Old runs remain without a record; the host must report unknown provenance instead of inferring a thinking mode. The host validates its strict DTO and inserts provenance in the accepted-run transaction. The storage layer records no runtime capability or permission to execute. Authenticated schemas 6–10 upgrade through the private encrypted rehearsal and one atomic schema/audit/signature/generation commit. Existing archives keep their original target version: verification accepts copied-Next targets 6–11 and original-import targets 7–11; future formats fail closed. Upgrading an archived baseline is a real write and makes deactivation ineligible.

## App-managed local workspace

`prepare_local_workspace(parent)` exclusively publishes a private `local-workspace` directory containing a generated `managed.key` and encrypted database together. Existing slots are checked, never repaired with new keys. `local_workspace_secret(root)` validates owner/private permissions, regular-file/single-link/format constraints and returns an ephemeral owned `Secret`. The normal host opens automatically. Existing manually keyed siblings remain untouched. The colocated key intentionally relies on OS-account access; it is not protection against a party able to read both files. See [automatic local storage](../../docs/AUTOMATIC-LOCAL-STORAGE.md).

## Original schema-25 copied-profile import

`LegacyKeys` accepts an explicit original master-hex or separate database-passphrase/blob-key contract. `preflight_legacy_copy`, `import_legacy_copy`, `verify_legacy_import` and `check_legacy_rollback` provide the normal host backend. The request takes selected `source_copy`, `managed_copy`, exclusive `destination` and `offline_copy_attested`; import takes an independent new `Secret`. The destination contains new-key encrypted `profile.sqlite3` and `import.db`, never persisted original credentials. Exact typed rows, schema objects and original file bytes are retained; usable conversations/artifacts and read-only original task/message details are projected separately. Stored authority stays dormant. See [the full contract and browser evidence](../../docs/ORIGINAL-PROFILE-IMPORT.md).

## Explicit plaintext-copy migration

`migrate_plaintext_copy(&MigrationRequest { source_copy, destination, offline_copy_attested }, &Secret)` takes a directory containing an independent, stopped copy of `smith-next.sqlite3`, its copied WAL/SHM when present, and any ancillary files to retain. It accepts Next schemas 1–5, upgrades only the encrypted stage to 11, and atomically publishes a **new** destination directory containing:

- `profile.sqlite3`: encrypted application data, including exact original typed values and implicit rowids, plus additive schema upgrades and audit metadata.
- `migration.db`: encrypted manifest and byte archive of every supplied original file, including the original plaintext database/WAL and ancillary files. No keys are stored.

SQLite is never opened against the selected source. A mode-0700 scratch directory with mode-0600 files holds a temporary clone of the already-plaintext input while SQLite reads its copied WAL. That temporary clone is removed on ordinary completion/failure; abrupt termination may leave it. This operation does not promise secure erasure of plaintext on SSDs. The published destination and verification scratch databases are encrypted. The original supplied plaintext copy remains unchanged.

Each original schema object and typed row, including implicit rowid and NULL/BLOB/integer distinctions, is fingerprinted before and after encrypted copying. Original table values are compared again after upgrades. Source statuses are not normalized: opening the migrated profile later may perform explicit interrupted-response recovery, which counts as a subsequent write. Original virtual tables, triggers, reserved storage objects and unsupported schemas fail closed; they are not silently discarded or executed. Extra ordinary tables and views can be preserved when their contracts survive the schema upgrade.

`verify_migration(profile_copy, &Secret, true)` checks a private encrypted snapshot, validates the full source-file archive, and reports whether the current application database still equals its migration baseline. It does not reset or repair either selected directory. `check_rollback(current_profile_copy, source_copy, &Secret, true)` additionally verifies the retained source-copy bytes. It returns `eligible: false` after any committed application-table write, even an edit followed by an undo or insert followed by deletion. A rolled-back transaction does not count as a committed write. Both the persistent generation and complete baseline fingerprint must match.

Rollback eligibility authorizes only the host's bounded deactivation transition while it holds the migrated profile lock and has drained all work. It does not delete, overwrite, decrypt a new plaintext output, reverse a schema, or reopen plaintext in Next. Both copies remain available; the prior build may use the retained source copy. The host must obtain its ephemeral owned key and recheck eligibility immediately before changing its selection marker; normal local mode uses the existing private managed key.

## Boundaries

Selected source, destination and ancestor directories must be trusted, quiescent and independent; no concurrent renames, writers, symlink changes or path aliases may cross the boundary. Terminal-file `O_NOFOLLOW` and before/after fingerprints do not replace descriptor-anchored filesystem traversal. The host holds its root lock and, when activated, the migrated directory's own `profile.lock` to exclude another host started at that directory.

Copy migration rejects symlinks, hard links, special files, non-UTF-8/traversal paths, hot rollback journals, existing destinations and overlaps. Source traversal is bounded before file reads: 256 MiB cumulative bytes, 10,000 entries, 32 nested directory levels. The encrypted source archive must also fit 256 MiB. Ordinary application unlock streams encrypted file copies and is not limited by that migration input bound. Empty directories, timestamps, ACLs and extended attributes are not archived. No external location mentioned by stored data is resolved or traversed. Original Smith schema-25 import uses the preservation library's verified-copy bridge and external-store checks before projection; all registered locations remain reference-only.

Publication uses no-replace atomic rename on macOS/Linux. Failure before publication cleans only this operation's stage. If the final directory sync fails after rename, the error explicitly reports that publication happened; retain and verify the complete destination. Supported destination entries are the two databases, their WAL/SHM sidecars and the host's `profile.lock`; unexpected content prevents rollback verification.

Audit metadata is an application integrity guard, not protection against someone who possesses the passphrase and deliberately rewrites both the database and its manifest. It detects normal committed writes, schema changes and missing audit triggers. OS memory dumps, swap, a hostile filesystem administrator, actual power loss and unsupported-platform behavior are outside the synthetic acceptance evidence.

## Synthetic checks

```sh
cargo test --locked --offline --manifest-path crates/smith-profile-storage/Cargo.toml
cargo clippy --locked --offline --manifest-path crates/smith-profile-storage/Cargo.toml --all-targets -- -D warnings
```

Tests use only temporary synthetic data. They cover actual SQLCipher ciphertext/reopen, every copied Next version 1–5, rowids, branching/status payloads, copied WAL commits, immutable/read-only sources, wrong-key/corrupt/future-schema refusal, exact source archives, private permissions, competing publication, bounded traversal, write/undo rollback refusal, interrupted-response recovery, normal close and retained ownership after transaction/busy-checkpoint failures. Browser session isolation and cancellation/draining are host integration responsibilities and are tested separately.
