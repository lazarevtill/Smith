# Native macOS hardware inventory

The app-callable implementation is `src/hardware_inventory.rs` at the repository
root. It uses compiled bindings to public sysctl, Metal, Objective-C,
CoreFoundation and Mach VM APIs. Importing the module performs no query.

`observe()` returns a bounded typed observation. A single-flight gate retains
ownership of a timed-out native query until it finishes; late results are
discarded. No model, Metal command queue or kernel is created. The query does
not install drivers, capture a screen, read user profiles or access Keychain.

CPU and unified-memory GPUs share one memory pool. Recommended working-set and
maximum-buffer figures remain separate metadata. The conservative free-page
sample is instantaneous and unreserved; it is not guaranteed allocatable memory.
Opaque device IDs are scoped to the boot/registry lifetime, with an explicit
process-lifetime fallback. Raw serials and registry IDs are not returned.

`main.swift` is a standalone read-only comparison probe, not an app dependency.
Compile it with the installed Apple toolchain into an explicit local output
directory. It is optional and is never launched automatically by the app.

An empty GPU list can reflect restricted visibility. Device enumeration does
not establish runtime installation, model fit, accelerator placement or speed.
Windows/Linux observations have their own implementation and tests; this module
does not certify those platforms.
