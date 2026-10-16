# Releases

## Version and tag policy

Repository releases use annotated `vMAJOR.MINOR.PATCH` tags. Development releases
use `-alpha.N`, broader test releases `-beta.N`, and release candidates `-rc.N`.
Tags identify immutable source commits: never move or force-replace a published
tag. Fix a release in a new version. Internal crate versions are separate;
there is no crates.io publication in this process.

The initial `v0.1.0-alpha.1` is a source-only prerelease of the exact eight-lane
CI-tested commit `e9f0e4246c5944b154ff15529e2636bc2db036b3`. Documentation added later
on main does not change that tagged source.

## Release contents

| Part | Source prerelease | Installable release |
| --- | --- | --- |
| Annotated tag and exact commit | Required | Required |
| Human-reviewed release notes | Required | Required |
| GitHub-generated source archives | Available | Available |
| Build evidence for the exact commit | Required | Required |
| Per-platform app packages | None initially | Only qualified platforms |
| SHA256SUMS for uploaded packages | When assets exist | Required |
| Dependency/asset notices | Retained in source | Included in each package |
| Signing/notarization status | Not applicable to source | Explicit per package |
| Model weights and user data | Never included | Never included |

## Maintainer sequence

1. Choose a release commit with clean source and passing required CI. Review the
   diff, notices, platform limitations and changes affecting stored data.
2. Write notes using [the template](RELEASE-NOTES-TEMPLATE.md). State which checks
   actually ran. Generated GitHub notes are a starting point, not qualification.
3. For app packages, run fresh/reopen/quit and process cleanup on each actual OS;
   test upgrade on disposable copies, preserve the old app and original data,
   and document restoration. Never assume an older app can open a newer schema.
4. Record package hashes, native dependencies and signing status. Verify the
   archive contents contain only intended distributable files. Keep local keys,
   profile data, credentials, model caches and private logs out of assets.
5. Create an annotated version tag at the reviewed commit, push that tag without
   force, then create a GitHub prerelease or release for that exact tag. Use a
   notes file with `gh release create --verify-tag --notes-file ...`.
6. Verify the published tag resolves to the intended commit and inspect the
   resulting notes/assets. Stable status requires supported-platform runtime,
   upgrade/recovery and packaging checks, not just compilation.

The current CI has read-only repository permissions and does not publish apps.
Release publishing is a deliberate maintainer operation. Hardware qualification
is tracked separately from source builds; no accelerator is advertised as
validated solely because its backend appears in a selector.
