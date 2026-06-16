# PHASE0_VTE_PATCH.md — Surface APC to `Perform`, implement `Grid::apc_dispatch`

Phase 0 of the Kitty graphics plan. This document is self-contained: an engineer
(or `/goal`) can execute it without re-investigating. All `file:line` references
were re-verified against the vendored crate and the working tree on
`shuv1337/wentletrap` (HEAD `cddcb6df`) on 2026-06-16.

## 0. Build-config facts that change the patch (verified, read these first)

These were checked against the live tree and **correct an assumption in the
investigator inputs**:

- **vte version in use is `0.11.0`.** `Cargo.toml:114` pins the workspace
  dependency `vte = { version = "0.11.0", default-features = false }`;
  `zellij-server/Cargo.toml:43` and root `Cargo.toml:44` consume it via
  `vte = { workspace = true }`. `Cargo.lock` lists *two* vte entries —
  `0.10.1` (line 4093, a transitive dep of `strip-ansi-escapes 0.1.1`) and
  `0.11.0` (line 4105, used by `zellij` and `zellij-server`). **Only `0.11.0`
  matters; do not touch the 0.10.1 path.**
- **`no_std` is OFF in this build.** vte `0.11.0` declares `default = ["no_std"]`
  and `no_std = ["arrayvec"]` (its `Cargo.toml:50-53`), but zellij sets
  `default-features = false`, so the `no_std` feature is **not** enabled.
  Therefore `self.osc_raw` resolves to the plain **`Vec<u8>`** arm
  (`lib.rs:82-83`), which grows unbounded and has **no `is_full()`** method.
  Consequence: the `if self.osc_raw.is_full() { return; }` guard the VTE
  investigator calls for is `#[cfg(feature = "no_std")]`-only and is **dead in
  this build** — but keep it cfg-gated in the fork so the crate still compiles
  under `no_std` for any other consumer / for upstreamability.
- **Vendored source path** (read-only registry copy, copy this into the fork):
  `/home/shuv/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/vte-0.11.0`.

---

## 1. Fork setup (`zellij-vte-apc`)

We cannot add a trait method by overriding alone — vte 0.11.0's `Perform` has no
`apc_dispatch` and treats the `SosPmApcString` body as `Action::Ignore`, so there
is no callback to override. A patched fork is mandatory.

### 1a. Create the in-tree fork

Vendor the crate into the workspace so it travels with the repo (no external
repo, no submodule):

```bash
mkdir -p vendor
cp -r "/home/shuv/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/vte-0.11.0" \
      vendor/vte-apc
chmod -R u+w vendor/vte-apc          # registry copies are read-only
```

Edit `vendor/vte-apc/Cargo.toml`:
- Rename the package so the patch is unambiguous and to avoid a name collision
  with the registry crate:
  ```toml
  [package]
  name = "vte"          # keep the crate name "vte" so `use vte::...` is unchanged
  version = "0.11.0"
  # add a marker so it's obvious this is the fork:
  # description = "Parser for implementing terminal emulators (APC fork for zellij)"
  ```
  Keeping the **crate name `vte`** means zellij's `use vte::{Params, Perform};`
  imports need **no change**. The `[patch]` stanza (below) is keyed by the
  *original* crate name, so the package name must stay `vte`.
- Leave `[features] default = ["no_std"]` intact. The `[patch]` consumer
  (`default-features = false`) controls whether `no_std` is on; do not change the
  default or you'll flip the buffer type for other consumers.

### 1b. `[patch.crates.io]` stanza

Append to the **workspace root** `/home/shuv/orca/workspaces/zellij/wentletrap/Cargo.toml`:

```toml
[patch.crates.io]
vte = { path = "vendor/vte-apc" }
```

This redirects **every** `vte = "0.11.0"` (and the `default-features = false`
spec at `Cargo.toml:114`) to the fork. The transitive `vte 0.10.1` is a
*different version line* and is unaffected by a single-version patch — but
`[patch.crates.io] vte` with a path entry replaces the source for all versions
that resolve to it; since the path crate is `0.11.0`, only `0.11.0`-resolving
requirements repoint. `strip-ansi-escapes 0.1.1` requires `^0.10`, so it keeps
the registry `0.10.1`. Verify after patching (step 1c) that `0.10.1` is still
present and untouched.

### 1c. `Cargo.lock` implication

After adding the stanza, run:

```bash
cargo build -p zellij-server    # or: cargo update -p vte --precise 0.11.0
```

`Cargo.lock` for the `vte 0.11.0` entry (currently lines 4104-4107) loses its
`source = "registry+..."` / `checksum = ...` lines and gains nothing in their
place (path patches are sourceless in the lock). Commit the updated `Cargo.lock`.
Confirm:
- the `vte 0.11.0` package block no longer has a `checksum`,
- `zellij` and `zellij-server` still resolve `-> "vte 0.11.0"`,
- the `vte 0.10.1` block (line 4093) is **unchanged**.

---

## 2. vte patch — exact edits (apply order a → e)

All paths below are inside `vendor/vte-apc/`. Line numbers are the verified
current positions in the pristine 0.11.0 copy.

### (a) Add `apc_dispatch` to the `Perform` trait — `src/lib.rs:412-414`

The `Perform` trait spans `lib.rs:363-414`. **Correction to VTE-investigator
input:** the trait does *not* "end at :412"; `esc_dispatch` is declared at
**:412**, and the trait's closing `}` is at **:414**. Insert the new method
between them.

Current (`lib.rs:408-414`):
```rust
    /// The final character of an escape sequence has arrived.
    ///
    /// The `ignore` flag indicates that more than two intermediates arrived and
    /// subsequent characters were ignored.
    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {}
}
```

Replace with:
```rust
    /// The final character of an escape sequence has arrived.
    ///
    /// The `ignore` flag indicates that more than two intermediates arrived and
    /// subsequent characters were ignored.
    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {}

    /// Dispatch an APC (Application Program Command) string.
    ///
    /// `bytes` is the full APC payload with the introducer (`ESC _` / C1 `0x9f`)
    /// and terminator (`ESC \` / C1 `0x9c`) already stripped. Buffered and
    /// delivered once on the terminating ST. The default empty body keeps every
    /// existing `Perform` impl compiling unchanged.
    fn apc_dispatch(&mut self, _bytes: &[u8]) {}
}
```

The **empty default body is load-bearing**: it keeps the in-crate test
`Dispatcher` (`lib.rs:447`), `BenchDispatcher` (`lib.rs:938`), and every
downstream `Perform` impl compiling without edits.

### (b) `SosPmApcString` body: `Ignore` → `Put` — `src/table.rs:159`

Current (`table.rs:155-161`):
```rust
    SosPmApcString {
        0x00..=0x17 => (Anywhere, Ignore),
        0x19        => (Anywhere, Ignore),
        0x1c..=0x1f => (Anywhere, Ignore),
        0x20..=0x7f => (Anywhere, Ignore),
        0x9c        => (Ground, None),
    },
```

Change **only line 159**:
```rust
        0x20..=0x7f => (Anywhere, Put),
```

Leave the C0 controls (`0x00..=0x17`, `0x19`, `0x1c..=0x1f`) as `Ignore` — APC
body is the printable range. Note this range **includes `0x7f`/DEL** (unlike
OscString); DEL therefore gets buffered into the payload. Kitty payloads are
7-bit printable, so this is acceptable; keep it intentionally.

Do **not** touch `0x9c => (Ground, None)` at :160 — it already drives the C1-ST
path (see (e)). Do **not** touch the three entry rows at `table.rs:47-49`
(`ESC X`/SOS, `ESC ^`/PM, `ESC _`/APC all map to `SosPmApcString`).

### (c) `perform_state_change` entry + exit wiring — `src/lib.rs:165-188`

This mirrors the **OscString** precedent exactly: exit action keyed off the
**OLD** `self.state` (still old here — `self.state` is assigned only at `:191`,
after the action runs), entry action keyed off the **NEW** `state`.

The OscString lines being mirrored:
- Exit (`lib.rs:169-171`): `State::OscString => { self.perform_action(performer, Action::OscEnd, byte); }`
- Entry (`lib.rs:184-186`): `State::OscString => { self.perform_action(performer, Action::OscStart, byte); }`
- Entry buffer-clear body (`lib.rs:231-234`, `Action::OscStart`): `self.osc_raw.clear(); self.osc_num_params = 0;`

**Exit arm** — current (`lib.rs:165-174`):
```rust
                match self.state {
                    State::DcsPassthrough => {
                        self.perform_action(performer, Action::Unhook, byte);
                    },
                    State::OscString => {
                        self.perform_action(performer, Action::OscEnd, byte);
                    },
                    _ => (),
                }
```
Add a `SosPmApcString` arm (use a direct helper call, not a fake `Action`, since
we are not adding an Action variant):
```rust
                match self.state {
                    State::DcsPassthrough => {
                        self.perform_action(performer, Action::Unhook, byte);
                    },
                    State::OscString => {
                        self.perform_action(performer, Action::OscEnd, byte);
                    },
                    State::SosPmApcString => {
                        self.apc_dispatch(performer);
                    },
                    _ => (),
                }
```

**Entry arm** — current (`lib.rs:177-188`):
```rust
                match state {
                    State::CsiEntry | State::DcsEntry | State::Escape => {
                        self.perform_action(performer, Action::Clear, byte);
                    },
                    State::DcsPassthrough => {
                        self.perform_action(performer, Action::Hook, byte);
                    },
                    State::OscString => {
                        self.perform_action(performer, Action::OscStart, byte);
                    },
                    _ => (),
                }
```
Add a `SosPmApcString` arm that clears the reused buffer (mirrors OscStart):
```rust
                match state {
                    State::CsiEntry | State::DcsEntry | State::Escape => {
                        self.perform_action(performer, Action::Clear, byte);
                    },
                    State::DcsPassthrough => {
                        self.perform_action(performer, Action::Hook, byte);
                    },
                    State::OscString => {
                        self.perform_action(performer, Action::OscStart, byte);
                    },
                    State::SosPmApcString => {
                        self.osc_raw.clear();
                    },
                    _ => (),
                }
```

Add the `apc_dispatch` helper next to `osc_dispatch` (`osc_dispatch` is at
`lib.rs:200-214`). Insert after its closing brace (`:214`):
```rust
    /// Dispatch the accumulated APC payload. Borrows self read-only; APC has a
    /// single raw payload (no `;`-split params), so no MaybeUninit dance is
    /// needed (unlike `osc_dispatch`).
    #[inline]
    fn apc_dispatch<P: Perform>(&self, performer: &mut P) {
        performer.apc_dispatch(&self.osc_raw);
    }
```

### (d) Buffer reuse: make `Action::Put` state-aware — `src/lib.rs:230`

`self.osc_raw` is reused as the APC buffer. This is safe because
`SosPmApcString` and `DcsPassthrough` (the only other `Put` user) are mutually
exclusive states, and `self.osc_raw` is cleared on APC entry by (c).

Current (`lib.rs:230`):
```rust
            Action::Put => performer.put(byte),
```
Replace with a state-aware Put that buffers in APC and streams in DCS:
```rust
            Action::Put => {
                if let State::SosPmApcString = self.state {
                    #[cfg(feature = "no_std")]
                    {
                        if self.osc_raw.is_full() {
                            return;
                        }
                    }
                    self.osc_raw.push(byte);
                } else {
                    performer.put(byte);
                }
            },
```
The `self.state` read here is the **OLD** state (assigned only at `:191`), which
is exactly what distinguishes an APC-body `Put` from a DCS `Put`. Do not reorder
the `self.state = state` assignment. The `no_std` `is_full()` guard mirrors
`OscPut` (`lib.rs:238-240`); in this build (`no_std` off) it compiles out and
`osc_raw` is an unbounded `Vec`, matching existing OSC behavior.

### (e) ST / C1 `0x9c` dispatch, and `ESC \` — **no further table edits**

Both ST forms already route through the exit arm added in (c):
- **C1 ST `0x9c`**: `table.rs:160` already maps `SosPmApcString --0x9c--> Ground`.
  Departing `SosPmApcString` fires the new exit arm → `apc_dispatch`. No edit.
- **`ESC \`**: while in `SosPmApcString`, the `0x1b` byte matches the
  **Anywhere** row first (`table.rs:13` → `(Escape, None)`), because `advance()`
  checks `STATE_CHANGES[Anywhere][byte]` before the current-state row
  (`lib.rs:121-125`). `Escape` is a non-Anywhere new state, so leaving
  `SosPmApcString` → `Escape` *also* fires the exit arm → `apc_dispatch`. The
  trailing `0x5c` then runs `EscDispatch` (`table.rs:42`), harmless because the
  payload is already dispatched and the buffer is cleared on any subsequent APC
  entry. No edit.

**Out of scope (do not add for Phase 0):** the single-byte **C1 `0x9f`
introducer** is *not* in the table (it falls through to `(Anywhere, None)` and is
silently dropped). Kitty uses `ESC _`, so this is not required. If a future phase
needs it, add `0x9f => (SosPmApcString, None)` to the `Anywhere` block
(`table.rs:10-14`); flag it then, not now.

**Caveat carried forward (SOS/PM collapse):** `SosPmApcString` is shared by SOS
(`ESC X`/`0x58`), PM (`ESC ^`/`0x5e`), and APC (`ESC _`/`0x5f`) — no marker
distinguishes them post-entry. This patch routes **all three** through
`apc_dispatch`. That is acceptable here only because **Grid's `apc_dispatch`
gates on the `b'G'` first byte** (section 3) and ignores everything else — SOS/PM
payloads do not start with `G`, so they are dropped at the Grid layer. We do
**not** record the entry byte in the fork (keeps the patch minimal and closer to
upstreamable). If a future requirement needs strict APC-only delivery at the vte
layer, store the introducer byte at entry and gate dispatch on `0x5f`.

---

## 3. `grid.rs` — implement `apc_dispatch`

Add the method to `impl Perform for Grid` (block begins at
`zellij-server/src/panes/grid.rs:3434`). Place it alongside the other dispatch
methods — natural spot is **after `osc_dispatch`** (which runs from `:3529`).
`osc_dispatch` is the structural precedent (single-shot, routes by a leading
selector, enrolls queries on `pending_forwarded_queries`), **not** the
hook/put/unhook triad.

```rust
    /// APC (Application Program Command). The fork's vte buffers the APC body
    /// and delivers it here once on ST, with introducer/terminator stripped.
    /// Only Kitty graphics APCs (`ESC _ G <keys>[;<payload>] ST`) are ours;
    /// every other APC user (tmux passthrough, iTerm, SOS/PM collapsed onto the
    /// same vte state) must be left untouched.
    fn apc_dispatch(&mut self, bytes: &[u8]) {
        // Non-`G` APC (incl. SOS/PM): not ours. Return immediately.
        let Some((&b'G', rest)) = bytes.split_first() else {
            return;
        };

        // `rest` is the kitty command body (control keys, optional `;`payload).
        // Parsing the `a` (action) key is cheap and MUST happen even when cell
        // size is unknown: `a=q` (and I-number allocation) reply without any
        // geometry, and the cell-size gate below must not block them.
        match crate::panes::kitty::dispatch(rest) {
            // a=q / I-allocation: answer LOCALLY but ride the ordered
            // pause/replay queue so the reply can't overtake later stream
            // bytes. Identical enrollment to ColorPaletteMode (grid.rs:4350).
            crate::panes::kitty::KittyOutcome::Query(q) => {
                self.pending_forwarded_queries
                    .push(crate::host_query::HostQuery::KittyGraphics(q));
            },
            // transmit / display / place: needs cell pixel size. Gate exactly
            // like the sixel hook (grid.rs:3485). Drop silently if unknown —
            // same behaviour as sixel.
            crate::panes::kitty::KittyOutcome::Store(cmd) => {
                if self.current_cursor_pixel_coordinates().is_some() {
                    // self.kitty_grid.handle(cmd, /* cursor px, cell size */);
                    self.mark_for_rerender();
                    let _ = cmd; // remove once kitty_grid lands
                }
            },
            crate::panes::kitty::KittyOutcome::Ignore => {},
        }
    }
```

Notes / contracts:

1. **G-prefix check** uses `split_first()` against `b'G'`; non-`G` returns
   immediately. This is what keeps SOS/PM (which collapse onto the same vte state
   per section 2e) and other APC users unaffected.
2. **`crate::panes::kitty`** (`KittyOutcome`, `dispatch`, `kitty_grid`) is a
   stub for Phase 0 — a thin router contract. `apc_dispatch` stays a thin router
   exactly as `osc_dispatch` routes by selector; key parsing and multi-chunk
   `m=1` reassembly live in `kitty.rs` (later phase). For a compiling Phase-0
   stub, `kitty::dispatch` may return `KittyOutcome::Ignore` for everything
   except a recognized `a=q`, which returns `KittyOutcome::Query(..)`.
3. **Cell-size gate is asymmetric.** Wrap **only** the `Store` (transmit/display/
   place) branch in `self.current_cursor_pixel_coordinates().is_some()`. The
   `Query` branch must work even when `character_cell_size` is `None` and even
   mid-upload — this is the startup-probe window capability detection depends on.
   `current_cursor_pixel_coordinates()` (`grid.rs:2921-2932`) returns `Some` only
   when `*self.character_cell_size.borrow()` is `Some`; this is the same
   invariant the sixel `hook` relies on at `grid.rs:3485-3487`.
4. **Query pause is enrolled, never armed here.** Pushing onto
   `pending_forwarded_queries` (field declared `grid.rs:654`, init empty `:973`)
   is the *only* action for `a=q`. `apc_dispatch` never touches `forward_paused`.
   The pipeline does the rest, in true stream order, automatically:
   - `terminal_pane.rs:225-235` feeds vte one byte at a time and, **after each
     byte**, checks `if !self.grid.pending_forwarded_queries.is_empty()`; the
     moment `apc_dispatch` (running inside `advance`) pushes, the loop stuffs the
     unfed remainder into `pending_pty_input` and breaks.
   - `tab/mod.rs:2854-2883` drains via `drain_forwarded_queries()`, arms one
     `arm_forward_pause()` per pane, and emits
     `ScreenInstruction::ForwardHostQuery { pane_id, query }`.
   - Replay at `tab/mod.rs:2798-2829` writes the reply first, then clears the
     pause and re-feeds buffered input.
   **Do NOT add any kitty-specific pause flag** — a second pause mechanism
   re-introduces the ordering bug patch-vte exists to prevent.

### 3b. `HostQuery::KittyGraphics` variant + local short-circuit (companion edits)

These are required for the `Query` arm to compile and to be answered locally
(zellij *is* the kitty implementor; the reply must never hit the wire). They
mirror `ColorPaletteMode` exactly.

- **`zellij-server/src/host_query.rs`**: add a field-light variant to `enum
  HostQuery` (`:45`), e.g. `KittyGraphics(KittyQuery)` (or unit if the query
  carries no params yet). In `to_query_bytes` (`:73`) add an arm returning
  `Vec::new()` — same as `ColorPaletteMode` at `:98` — since it is never wire-
  serialized.
- **`zellij-server/src/screen.rs`**: in `forward_host_query` (`:2496`), add a
  short-circuit **before** the token/in-flight logic — exactly where
  `ColorPaletteMode` is intercepted at `:2506-2509` — that calls a new
  `answer_kitty_graphics_query_locally(pane_id, ..)` and returns
  `STARTUP_SENTINEL_TOKEN`. The local answerer must **guard
  `PaneId::Plugin(_)`** and return early (mirror
  `answer_color_palette_mode_query_locally` at `:2538-2540`), then write the
  synthesized kitty reply into the pane's pty.

`pane_id` arrives via the `ForwardHostQuery` instruction; the `HostQuery`
variant stays field-light (no pane/client fields), matching the existing enum.

---

## 4. Verification

### 4a. Unit tests in the fork (`vendor/vte-apc/src/lib.rs`, `#[cfg(test)]`)

The in-crate test `Dispatcher` (`lib.rs:447`) implements `Perform`. Extend it
with an `apc: Vec<u8>` (or `Vec<Vec<u8>>`) field and override `apc_dispatch` to
record payloads. Add tests:

1. **APC delivery, `ESC _`…`ST`:** feed `b"\x1b_GfooST"` where ST is `0x9c`
   (`feed: 0x1b 0x5f 'G' 'f' 'o' 'o' 0x9c`); assert one `apc_dispatch` with
   `b"Gfoo"`.
2. **`ESC \` terminator:** feed `b"\x1b_Gfoo\x1b\\"`; assert one `apc_dispatch`
   with `b"Gfoo"` (the `0x1b`→Escape transition fires the exit arm; the trailing
   `0x5c` `EscDispatch` is harmless).
3. **Split across `advance` feeds:** feed `b"\x1b_Gfo"` in one loop, then
   `b"o\x1b\\"` in a second; assert a single `apc_dispatch` with `b"Gfoo"`
   (buffer accumulates across feeds; only the ST flushes).
4. **C1 framing:** introducer `ESC _` + C1 ST `0x9c` (test 1 already covers C1
   ST). Optionally assert that a lone C1 `0x9f` introducer is **dropped** (no
   `apc_dispatch`) — documents that 0x9f is intentionally unhandled in Phase 0.
5. **Non-`G` ignored at Grid layer:** this is a Grid-layer guard, but at the fork
   layer assert that a non-`G` APC (`b"\x1b_Xbar\x1b\\"`) still *delivers*
   `b"Xbar"` to `apc_dispatch` (the fork is content-agnostic; the `G` filter is
   Grid's job).
6. **SOS/PM still flow as APC payloads but carry their introducer-less body:**
   feed `ESC X`(`0x58`)`payload`+ST and `ESC ^`(`0x5e`)`payload`+ST; assert
   `apc_dispatch` fires with the body (this documents the collapse caveat).
   Confirm the Grid-layer `G` filter is what suppresses them downstream.
7. **DCS unaffected:** feed a DCS sequence (`ESC P ... ST`) and assert `put()`
   still streams byte-by-byte (the state-aware `Put` did not break DCS).
8. **Buffer cleared on re-entry:** two back-to-back APCs; assert the second
   payload does not contain the first's bytes.

Run: `cargo test -p vte` (and `cargo test -p vte --features no_std` to exercise
the `is_full()` guard path).

### 4b. Grid smoke test (`zellij-server`)

In `grid.rs` tests (or a new test module), construct a `Grid`, drive an
`ESC _ G a=q ST` sequence through a `vte::Parser`, and assert
`grid.pending_forwarded_queries` contains a `HostQuery::KittyGraphics(..)` after
`advance`. Also assert a non-`G` APC leaves `pending_forwarded_queries` empty.
Run `cargo test -p zellij-server`.

---

## 5. Risks & gotchas

- **Wrong vte version / wrong feature:** the patch only matters for `vte 0.11.0`
  with `no_std` **off** (this build). If a future change adds
  `default-features = true` or upgrades vte, re-verify: the `Put` `is_full()`
  guard and the `Vec` vs `ArrayVec` buffer type flip. The `0.10.1` transitive
  copy must stay on the registry — never repoint it.
- **Do not add an `Action` variant.** `Action`/`State` are 4-bit packed and
  `unsafe`-transmuted in `unpack`/`pack` (`definitions.rs:59-73`); both enums
  must stay at exactly 16 variants. The patch reuses `Action::Put` and a plain
  helper call — never a new `Action`. (Note: `definitions.rs:82,100` already
  reference `(State::SosPmApcString, Action::Unhook)` *in test assertions only*;
  these are bit-pattern checks of `0xee`, unrelated to our wiring — leave them.)
- **`self.state` is the OLD state during `perform_action`** (assigned only at
  `lib.rs:191`). The APC-vs-DCS `Put` discrimination and the exit-arm dispatch
  both rely on this. Do not move the assignment.
- **SOS/PM collapse** (section 2e): all three of SOS/PM/APC reach
  `apc_dispatch`. Correctness depends on Grid's `b'G'` filter. If anyone removes
  that filter, SOS/PM payloads leak into the kitty router.
- **`ESC \` double-path:** `apc_dispatch` can fire on the lone `0x1b` before the
  `0x5c` arrives. This is fine — the payload is complete at ST and the buffer is
  cleared on the next APC entry, so no double-dispatch. Verified by test 2.
- **Cell-size gate asymmetry:** wrapping the whole `apc_dispatch` body in the
  `current_cursor_pixel_coordinates().is_some()` gate (naive sixel copy) breaks
  `a=q` replies in the startup-probe window. Gate the `Store` branch only.
- **64 KB read boundary:** image payloads routinely split across `advance` feeds;
  the fork's buffer (`osc_raw`) accumulates across feeds and flushes only on ST.
  Test 3 covers this. (Protocol-level `m=1` chunking is a separate `kitty.rs`
  layer, not this patch.)
- **Maintenance / rebase posture:** the fork is a vendored `vte 0.11.0` with five
  small, localized edits (one trait method, one table cell, two match arms + one
  helper, one `Put` arm). Keep a `FORK_NOTES.md` in `vendor/vte-apc/` listing the
  five edit sites verbatim so a vte version bump can be re-applied by hand. Prefer
  staying pinned to `0.11.0`; do not auto-upgrade. If upstreaming is ever
  attempted, the trait method + table + state-change wiring are the
  upstream-worthy parts; the `osc_raw` reuse is the one design choice a maintainer
  may want as a dedicated `apc_raw` field instead.
