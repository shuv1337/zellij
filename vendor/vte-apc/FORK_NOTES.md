# zellij-vte-apc — fork notes

A vendored copy of `vte 0.11.0` patched to surface APC (Application Program
Command, `ESC _ … ST`) sequences to the `Perform` trait. This is **Phase 0** of
Kitty graphics support in this zellij fork — vanilla vte 0.11 routes APC into
`SosPmApcString` and drops the body (`Action::Ignore`), so Kitty image data is
invisible without this patch.

Wired into the workspace via `[patch.crates.io] vte = { path = "vendor/vte-apc" }`
in the root `Cargo.toml`. The crate name stays `vte` so `use vte::...` needs no
change. Only the `0.11.0` version line repoints; the transitive `vte 0.10.1`
(via `strip-ansi-escapes`) stays on the registry.

## The five edits (re-apply by hand on a vte version bump)

All against the pristine `vte 0.11.0` source. Each is tagged with a
`zellij-vte-apc fork:` comment in-tree.

1. **`src/lib.rs`** — add `fn apc_dispatch(&mut self, _bytes: &[u8]) {}` to the
   `Perform` trait (after `esc_dispatch`). Empty default body keeps all existing
   `Perform` impls compiling.
2. **`src/table.rs`** — in `SosPmApcString`, change the body row
   `0x20..=0x7f => (Anywhere, Ignore)` to `(Anywhere, Put)`. The `0x9c` (C1 ST)
   row is untouched.
3. **`src/lib.rs` `perform_state_change`** — add a `State::SosPmApcString` arm to
   both the exit `match self.state` (flush: `self.apc_dispatch(performer)`) and
   the entry `match state` (clear: `self.osc_raw.clear()`), mirroring
   `OscString`. Add the private helper `fn apc_dispatch<P: Perform>(&self, …)`
   next to `osc_dispatch`.
4. **`src/lib.rs` `perform_action`** — make `Action::Put` state-aware: in
   `SosPmApcString`, push into the reused `osc_raw` buffer (with the
   `#[cfg(feature = "no_std")] is_full()` guard); otherwise `performer.put(byte)`
   as before. Relies on `self.state` still being the OLD state here.
5. **`src/lib.rs` tests** — `Sequence::Apc(Vec<u8>)` variant, `apc_dispatch`
   override on the test `Dispatcher`, and the `apc_*` tests.

## Invariants / gotchas

- **`no_std` is OFF in this build** (zellij sets `default-features = false`), so
  `osc_raw` is an unbounded `Vec<u8>` and `is_full()` does not exist — the cap
  guard is `#[cfg(feature = "no_std")]`-gated and dead here, but kept for
  portability. Do not flip the default feature.
- **No new `Action`/`State` variants.** Both enums are 4-bit packed and
  `unsafe`-transmuted (`definitions.rs`); they must stay at 16 variants. The
  patch reuses `Action::Put` + a plain helper call.
- **SOS/PM collapse:** SOS (`ESC X`), PM (`ESC ^`) and APC (`ESC _`) share
  `SosPmApcString`; all three now reach `apc_dispatch`. The zellij consumer
  filters by the leading `G` byte (Kitty), so SOS/PM are dropped downstream.
- **C1 `0x9f` introducer is intentionally not wired** (Kitty uses `ESC _`). Only
  `ESC _` enters the APC buffer.
- Prefer staying pinned to `0.11.0`; do not auto-upgrade.
