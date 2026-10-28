# What Smith should do next

Status: planned work, recorded on 2026-10-28. All completion criteria below are
open. This roadmap covers the published Smith refactor at
`f81fdc6f1c9ba137fbb08073dc5886a5620df440` and ideas inspected in
[Hob at `83a86c0`](https://github.com/FrigadeHQ/hob/tree/83a86c0617d190dcc84d8328ff64160bb84f0e67).
Hob references are pinned to that source version, not promises of future behavior.

Keep Smith's Rust/Slint desktop, encrypted profile, native MLX/GGUF workers,
reviewed execution plans and inference provenance. Adapt the useful product
workflows to those components. Existing conversation history, document handling,
chart rendering, saved extensions and runtime preferences should be extended.

## First implementation priorities

The four priorities below are the next work to complete. Start the CI work
alongside first-run development; installer publication depends on its checks.
Qualify macOS first, then complete Windows/Linux adapters and desktop journeys.

### 1. Complete the first-run journey

Recommend a model, explain disk/memory requirements, download with verification
and resume, then reach a working conversation.

- Extend `smith-acceleration` and `smith-local-catalog` with a curated,
  versioned catalog of compatible model/runtime combinations, digests, sizes,
  capabilities and license references. Keep provisioning separate from passive
  saved-model records.
- Explain recommendations using observed hardware, available disk space and
  memory headroom. Label speed/fit estimates as estimates. Preserve manual
  selection and an offline path for already-owned compatible files.
- Show download size, destination, progress and cancellation. Resume interrupted
  downloads across restarts, handle stalled transfers and disk exhaustion, and
  verify the complete artifact before atomic publication. Revalidate selected
  bytes again when preparing a runtime plan.
- Offer a smaller starter model where useful. Make additional downloads and
  model/device changes explicit, preserve the user's choice, and record the
  actual model used for every accepted run.
- Guide the user through load, first response, cancellation and restart recovery
  with actionable errors. Retain the existing account-free local startup.

**Done when:** a fresh supported Mac reaches a persisted local conversation
through the native UI; interrupted/corrupt downloads and insufficient resources
recover without replacing valid files or loading an unreviewed model; an offline
restart can use the already-provisioned selection.

Hob references: [model picker][hob-picker], [setup controller][hob-setup],
[verified downloader][hob-downloader].

### 2. Ship a qualified installer and update path

Ship macOS first, followed by actual Windows/Linux lifecycle support.

- Package the backend, native UI, required runtime helpers and notices with
  verified paths and ownership. Resolve the application/asset licensing items
  already recorded in [license status](../LICENSE-STATUS.md) before distribution.
- Produce signed, notarized macOS packages. Test install, first launch, reopen,
  quit, process cleanup, upgrade and restoration using disposable profile copies.
- Add a user-controlled update channel with authenticated release identity,
  package verification, interrupted-update recovery and clear restart behavior.
  Keep profile keys with their data; follow the existing schema compatibility
  and restoration rules rather than opening a newer schema with an older app.
- Implement the Windows process/listener, private storage and runtime adapters;
  complete Linux/Windows paths, dialogs and clipboard before claiming those
  platforms are supported. Track each actual OS/architecture separately.
- Require the release commit's build, application and package checks to pass
  before publication. Preserve an explicit maintainer publishing step.

**Done when:** an installable macOS release completes the documented lifecycle,
upgrade and recovery checks on a supported machine. Windows/Linux remain open
until their own adapters, installers and native desktop checks pass.

Smith references: [release process](RELEASING.md),
[platform implementation gaps](CI-NATIVE-BUILDS.md#source-grounded-platform-gaps).
Hob references: [packaging][hob-packaging], [updates][hob-updates],
[release workflow][hob-release].

### 3. Add end-to-end CI around user outcomes

Cover first launch, inference, cancellation, restart, document work and recovery.

- Keep deterministic synthetic-provider tests for fast native UI and lifecycle
  feedback: create/reopen workspace, first response, stop, restart, document
  import/export, stale actions, failed jobs and interrupted-write recovery.
- Add distinct real-worker/model and packaged-app gates. Record exact source,
  runtime, model, OS/device and package identities; synthetic success must remain
  distinguishable from actual inference and hardware evidence.
- Run supported-platform journeys on their real operating systems, including
  Windows as a standard user. Explicitly report unsupported paths.
- Exercise no-network operation, failed downloads, worker crashes, resource
  exhaustion and duplicate/retried requests. Check that cancellation and exit
  reap owned processes and preserve committed data.
- Retain bounded, redacted failure diagnostics and connect required results to
  release eligibility. A successful packaging job cannot override failing tests.

**Done when:** each advertised journey has a required check with inspectable
evidence, and the exact release commit cannot publish while any required gate
is failing, missing or incomplete.

Hob references: [application CI][hob-ci], [real-tool smoke workflow][hob-smoke].
These are useful examples of separating fixture-based UI tests from real tools;
they do not by themselves qualify Smith's runtimes.

### 4. Build usable MCP integration

Retain Smith's explicit permission and execution boundaries.

- Add native connector management for remote HTTPS/OAuth and explicitly selected
  local stdio servers: setup, connection status, discovered tools, reconnect,
  cancellation, credential refresh and removal.
- Bind each invocation to the selected server identity, tool/schema revision,
  exact arguments and task grant. Treat server descriptions, tool annotations
  and returned content as untrusted input; none can grant execution authority.
- Keep remote credential custody separate from local-workspace access. Local
  servers need explicit executable/environment/resource scope and owned-process
  cleanup. Show any platform confinement limitation before enabling execution.
- Support reviewed configuration import, then assess `.mcpb` compatibility.
  Parsing a configuration or bundle must not install dependencies, start a
  server, fetch referenced files or authorize tools by itself.
- Test protocol negotiation, HTTP/stdio behavior, OAuth expiry, schema changes,
  revoked grants, stale approvals, malformed responses and disconnects. Report
  uncertain side effects instead of automatically retrying mutating calls.

**Done when:** users can configure representative local and remote connectors,
review and run scoped tools, cancel them, restart Smith and revoke access through
the native UI, with no credentials or executable authority recovered from traces.

Hob references: [connector guide][hob-connectors], [MCP client][hob-mcp],
[plugin registry][hob-plugins]. Implement Smith's supported protocol contract;
Hob's client version and transport fallbacks are reference choices only.

## Additional useful ideas from Hob

These follow-on items extend the four priorities. Their order favors reusable
workflow foundations before broader media and polish. Each remains planned.

### H1. Projects with selected folders and reusable instructions

Make Smith's existing workspace grants and saved agent definitions easy to use
through named projects containing chats, selected folders and instructions.
Show missing/moved folders and let users review or revoke each scope. Keep
project instructions separate from authority; switching projects must not
carry over another project's grants.

**Done when:** create, reopen, rename and remove-project journeys preserve chats,
handle unavailable folders and pass cross-project access-isolation checks.
Reference: [Hob projects][hob-projects].

### H2. Clear task progress and reversible file effects

Build a readable work log from Smith's existing task receipts: current action,
approval, retry, changed files, completion and failure. Group repeated attempts
without discarding failed or uncertain outcomes from the inspectable history.
Extend the existing conversation-metadata action history with a separate,
durable journal for supported external file effects.

**Done when:** users can inspect a task's changes and undo supported operations
after restart; undo detects intervening user edits and never silently overwrites
them. Show backup limits, conflicts and operations that cannot be reversed,
including external MCP side effects. Preserve Smith's existing history semantics.
References: [Hob work-log grouping][hob-steps], [file journal][hob-journal].

### H3. Turn a successful workflow into a reusable skill

Extend saved skills/extensions with an explicit "save as skill" flow containing
reviewable instructions, validated script/data files, dependency versions and
required capabilities. Support import/export of the Agent Skills format and
deliberate updates to existing skills. Show provenance and prevent secrets,
untrusted tool output or stored instructions from becoming persistent authority.

**Done when:** a user can inspect, save, rerun, edit and remove a skill; each run
receives fresh scoped authority and records the exact skill revision used.
References: [Hob skill tools][hob-skills], [Smith extensions](../src/agent_extensions.rs).

### H4. Make document and chart results useful in the native transcript

Extend Smith's document/artifact flow and existing SVG chart renderer with
clear file cards, preview/download actions, source-data references and richer
validated chart specifications. Begin with a complete spreadsheet/CSV to
chart/report workflow; add local PDF extraction/OCR and form workflows only
where their adapters have been exercised.

**Done when:** a selected input produces a previewable, exportable result with
traceable source data, survives reopen and respects non-overwrite semantics.
Unsupported formats and OS-specific features remain visible. Keep rendered
results inert; model-produced markup must not gain host execution privileges.
References: [Hob charts][hob-charts], [Smith chart/domain adapter](../src/agent_domain.rs),
[native document flow](../desktop/slint/src/native_documents.rs).

### H5. Optional vision and dictation with bounded resource use

Use Hob's on-demand media model pattern to finish Smith's live image and speech
journeys. Build on the existing transcription and historical-image components;
separate viewing an imported image from running vision inference. Provision
optional workers explicitly, account for their combined memory use, and unload
them when idle. Provide progress, stop and permission feedback.

**Done when:** image questions and microphone dictation work locally on a
qualified platform, stop reliably, recover from worker failure and release
resources without changing the main model or contacting a remote provider.
References: [Hob vision worker lifecycle][hob-vision], [dictation][hob-speech].

### H6. Faster follow-up responses and predictable resource recovery

Evaluate prompt/KV-cache reuse within the same authorized conversation and
optional small models for background jobs such as titles. Benchmark both native
MLX and GGUF workers before choosing a strategy. Extend current runtime
preferences with clear memory-pressure, worker-crash and smaller-model recovery
choices; changing a reviewed model or device still needs a new valid plan.

**Done when:** measurements show the latency/memory tradeoff for exact model and
hardware combinations; caches cannot leak across profiles or incompatible
conversation branches, and model changes remain visible in run provenance.
References: [Hob inference worker][hob-worker], [setup/recovery][hob-setup],
[Smith runtime preferences](../src/runtime_preferences.rs).

### H7. Localization with completeness checks

Adopt a consistent message catalog, OS-locale default and user override for the
native UI. Test missing strings, placeholders and plural forms for each shipped
locale, then review longer translations and right-to-left layouts where offered.
Select supported languages deliberately rather than copying Hob's language count.

**Done when:** changing locale preserves user data and settings, every shipped
locale passes catalog checks, and critical setup/error/approval screens have
been reviewed in the native UI.
Reference: [Hob translation tests][hob-locales].

### H8. Understandable storage usage and cleanup

Show model/runtime download sizes, partial downloads, caches, profile data and
recoverable removals separately. Offer reviewed cleanup of owned caches and
unused managed models; external model files remain external. Explain export,
backup, retention and uninstall choices while preserving profiles and user
documents by default.

**Done when:** users can see what consumes disk space and reclaim selected
owned data without deleting active models, workspace keys, chats or exported
files. Destructive profile removal is a separate explicit action.
References: [Hob setup/model management][hob-setup], [removal planning][hob-uninstall].

## Implementation constraints

- Keep SQLCipher and Smith's current key/profile ownership checks. Hob's plain
  chat database is not a storage migration target.
- Preserve permission and execution boundaries when adapting the workflows.
  Hob's MCP subprocesses are unsandboxed, and its Windows Python confinement
  leaves reads/network open; those are limitations to address, not a Smith
  sandbox specification. See [Hob's sandbox implementation][hob-sandbox].
- Preserve reviewed model identities, full file validation, retained error
  evidence and explicit recovery. Do not import automatic fallback or cached
  verification assumptions that bypass those contracts.
- Reuse designs and test scenarios first. Any source reuse needs applicable
  attribution and dependency/asset notices; this roadmap changes no license.
- Close items using source, tests and actual platform evidence. Documentation,
  inventory, a visible control or a compiled binary alone does not close them.

[hob-picker]: https://github.com/FrigadeHQ/hob/blob/83a86c0617d190dcc84d8328ff64160bb84f0e67/packages/picker/src/pick.ts
[hob-setup]: https://github.com/FrigadeHQ/hob/blob/83a86c0617d190dcc84d8328ff64160bb84f0e67/app/src/main/setup.ts
[hob-downloader]: https://github.com/FrigadeHQ/hob/blob/83a86c0617d190dcc84d8328ff64160bb84f0e67/app/src/main/downloader.ts
[hob-packaging]: https://github.com/FrigadeHQ/hob/blob/83a86c0617d190dcc84d8328ff64160bb84f0e67/app/electron-builder.yml
[hob-updates]: https://github.com/FrigadeHQ/hob/blob/83a86c0617d190dcc84d8328ff64160bb84f0e67/app/src/main/updates.ts
[hob-release]: https://github.com/FrigadeHQ/hob/blob/83a86c0617d190dcc84d8328ff64160bb84f0e67/.github/workflows/release.yml
[hob-ci]: https://github.com/FrigadeHQ/hob/blob/83a86c0617d190dcc84d8328ff64160bb84f0e67/.github/workflows/ci.yml
[hob-smoke]: https://github.com/FrigadeHQ/hob/blob/83a86c0617d190dcc84d8328ff64160bb84f0e67/.github/workflows/smoke.yml
[hob-connectors]: https://github.com/FrigadeHQ/hob/blob/83a86c0617d190dcc84d8328ff64160bb84f0e67/docs/plugins/connectors.md
[hob-mcp]: https://github.com/FrigadeHQ/hob/blob/83a86c0617d190dcc84d8328ff64160bb84f0e67/app/src/main/plugins/mcp.ts
[hob-plugins]: https://github.com/FrigadeHQ/hob/blob/83a86c0617d190dcc84d8328ff64160bb84f0e67/app/src/main/plugins/registry.ts
[hob-projects]: https://github.com/FrigadeHQ/hob/blob/83a86c0617d190dcc84d8328ff64160bb84f0e67/app/src/main/projects.ts
[hob-steps]: https://github.com/FrigadeHQ/hob/blob/83a86c0617d190dcc84d8328ff64160bb84f0e67/app/src/shared/steps.ts
[hob-journal]: https://github.com/FrigadeHQ/hob/blob/83a86c0617d190dcc84d8328ff64160bb84f0e67/app/src/main/agent/journal.ts
[hob-skills]: https://github.com/FrigadeHQ/hob/blob/83a86c0617d190dcc84d8328ff64160bb84f0e67/app/src/main/plugins/skillTools.ts
[hob-charts]: https://github.com/FrigadeHQ/hob/blob/83a86c0617d190dcc84d8328ff64160bb84f0e67/app/src/main/tools/chart.ts
[hob-vision]: https://github.com/FrigadeHQ/hob/blob/83a86c0617d190dcc84d8328ff64160bb84f0e67/app/src/main/vision/server.ts
[hob-speech]: https://github.com/FrigadeHQ/hob/blob/83a86c0617d190dcc84d8328ff64160bb84f0e67/app/src/renderer/src/speech/useDictation.ts
[hob-worker]: https://github.com/FrigadeHQ/hob/blob/83a86c0617d190dcc84d8328ff64160bb84f0e67/app/src/engine/worker.ts
[hob-locales]: https://github.com/FrigadeHQ/hob/blob/83a86c0617d190dcc84d8328ff64160bb84f0e67/app/test/i18n-tables.test.ts
[hob-uninstall]: https://github.com/FrigadeHQ/hob/blob/83a86c0617d190dcc84d8328ff64160bb84f0e67/app/src/main/uninstall.ts
[hob-sandbox]: https://github.com/FrigadeHQ/hob/blob/83a86c0617d190dcc84d8328ff64160bb84f0e67/app/src/main/python/run.ts
