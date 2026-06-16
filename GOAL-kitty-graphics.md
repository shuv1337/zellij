# GOAL: Kitty Graphics Protocol support in zellij

Execution plan for `/goal`. Work the phases **in order** — each depends on the
previous compiling and running. Check off `- [ ]` items as completed. Source of
truth for design + line numbers:

- `KITTY_GRAPHICS_PLAN.md` — full design, decisions locked (see its §11 log).
- `PHASE0_VTE_PATCH.md` — the vte fork spec (Phase 0, already applied).

**Locked decisions** (do not relitigate): patch vte (not pre-scan) · one outer
id per `(client, source image)` · inner `a=q` answers OK if *any* client
supports Kitty · scope out negative-z, Unicode/virtual placement, Windows
render, `t=f`/`t=s`/animation.

**Working agreement:** after every phase, `cargo check -p <crate>` and the
phase's tests must be green before starting the next. Add tests in the same
commit as the code. Keep `vendor/vte-apc/` edits tagged with
`zellij-vte-apc fork:` comments.

---

## Phase 0 — vte APC fork ✅ DONE

The blocker is cleared. Already in-tree and verified:

- [x] `vendor/vte-apc/` = patched `vte 0.11.0` with `Perform::apc_dispatch`
  (5 edits, tagged in-source; see `FORK_NOTES.md`).
- [x] `[patch.crates-io] vte = { path = "vendor/vte-apc" }` in root `Cargo.toml`
  (note: `crates-io` with a dash — the spec's `crates.io` was wrong).
- [x] `Cargo.lock` repoints `vte 0.11.0` to the path (sourceless); `0.10.1` /
  `0.14.1` untouched.
- [x] `cargo test` in the fork: 30 pass under both `--no-default-features` and
  `--features no_std`.
- [x] `cargo check -p zellij-server` green (default trait body keeps Grid's
  existing `Perform` impl compiling).

**Acceptance:** ✅ met. APC bytes now reach `Perform::apc_dispatch`; nothing
downstream consumes them yet.

---

## Phase 1 — parse, store & answer Kitty (and its prerequisite, Phase 2)

> Capability detection (Phase 2) is a **prerequisite** of Phase 1's `a=q` reply
> path — build the client plumbing first, then wire replies. Phases 1 and 2 land
> together.

### 1a. `Grid::apc_dispatch` + kitty module skeleton ✅ DONE
- [x] New `zellij-server/src/panes/kitty.rs`; exported from `panes/mod.rs`.
- [x] Router contract `KittyOutcome { Query(KittyQuery), Store(KittyCommand), Ignore }`
  + `pub fn dispatch(body: &[u8]) -> KittyOutcome` with key parser. `KittyQuery`
  carries `id`/`image_number`/`quiet` and an `ok_reply()` builder.
- [x] `Grid::apc_dispatch` (after `esc_dispatch`): G-gate via `split_first()`;
  `Query` → push `HostQuery::KittyGraphics(..)` (NOT cell-size gated — startup
  probe window); `Store` → gated on `current_cursor_pixel_coordinates().is_some()`
  (storage stub, Phase 1c). No kitty-specific pause flag.
- [x] `HostQuery::KittyGraphics(KittyQuery)` variant + `to_query_bytes` →
  `Vec::new()`; added the unreachable cache-fallback arm too.
- [x] `screen.rs::forward_host_query` short-circuit →
  `answer_kitty_graphics_query_locally`: guards `PaneId::Plugin`, conservative
  silent default (TODO 1e gates on the any-client aggregate), always unblocks the
  forward-paused pane. Honors `q` quiet via `KittyQuery::ok_reply`.
- [x] **Tests (10, green):** 7 kitty-module (`dispatch` classification, malformed
  keys, `ok_reply`), 3 Grid-level (`a=q` enrols `KittyGraphics`; non-`G`/SOS
  ignored; `a=T` transmit does not enrol). `cargo check -p zellij-server` clean;
  no regression in existing forwarded-query / `csi_996n` tests.

### 1b. Protocol parsing & reassembly (`kitty.rs`) — mostly DONE
- [x] Typed control parse (`KittyControl`): `a,i,I,p,f,t,m,s,v,o,q` with Kitty
  defaults (a=t, f=32, t=d). `c,r,x,y,w,h,z` deferred to Phase 3 (placement/crop,
  where they're consumed). `medium_supported()` flags `t=f`/`t=s` (placeholdered).
- [x] `PendingUpload` reassembler: `begin`/`append`/`finish` concatenates `m=1`
  chunks; first chunk carries control. **State ownership (Grid holds
  `Option<PendingUpload>`) + final-chunk cursor → Phase 1c** (needs the Grid/store).
- [x] Dimensions without full decode (`image_dimensions`): `s,v` for raw; PNG
  IHDR for `f=100`. **`o=z` without `s,v` → `None` (placeholder).** Inflate-IHDR
  deferred — needs a zlib dep (no `flate2` vendored); see decision note below.
- [ ] `a=q` mid-upload must not disturb the accumulator — trivially holds today
  (Query never touches the upload); **re-assert once the Grid owns the
  accumulator in 1c.**
- [ ] `I` image-number → allocate a zellij-local id + reply — **deferred to 1c**
  (needs an id-allocation table on the store/Screen).
- [x] **Tests (13 green):** typed parse + defaults; unsupported media; raw `s,v`
  + PNG IHDR + `o=z` dim paths; multi-chunk reassembly (raw + PNG).

> **Decision needed (non-blocking):** `o=z` (zlib-compressed) images without
> explicit `s,v` currently placeholder. Recovering their dimensions needs a zlib
> inflater (`flate2`/`miniz_oxide`), a new dependency. Deferred until it's worth
> adding — most real payloads carry `s,v` or are uncompressed PNG.

### 1c. Storage & anchoring
- [ ] `KittyImageStore` (`HashMap<u32, KittyImage{format,payload,px_w,px_h}>`),
  shared `Rc<RefCell<…>>`, built at `screen.rs:1569`, cloned down
  tab→pane→grid. **Also holds the durable per-client outer-terminal state from
  the start** (`transmitted`, `placements`, `outer_ids` — see Phase 3).
- [ ] `KittyGrid` (`placements: HashMap<PlacementKey, PixelRect>`,
  `image_ids_to_reap`, shared cell-size handle). **Reuse `PixelRect`
  (`sixel.rs:13-19`) unchanged** — call `PixelRect::new(x, y, height, width)`
  (height before width).
- [ ] On `a=T`/`a=p`: anchor from `current_cursor_pixel_coordinates()`
  (`grid.rs:2921`), build `PixelRect`, insert, advance cursor by whole cells
  (`move_cursor_down_by_pixels` `grid.rs:2908`) — mirror `create_sixel_image`
  (`grid.rs:2933`). Cell-size guard like sixel `hook` (`grid.rs:3485`).
- [ ] Reuse `offset_grid_top` (`sixel.rs:213-218` via `bounded_push`
  `grid.rs:319`), `character_cell_size_possibly_changed` (`sixel.rs:235`),
  alt-screen swap (`grid.rs:4009`).
- [ ] **Tests:** store/anchor; scroll reaping; cursor advance (clone sixel cases
  in `panes/unit/grid_tests.rs`).

### 1d/Phase 2. Per-client outer-terminal capability detection
- [ ] `stdin_ansi_parser.rs`: third `strip_replies` branch for APC (`:424-480`)
  + `partial_apc` buffer (`:232-234`), mirroring OSC/CSI cap/`finalize()`/
  split-feed handling; classify Kitty replies; strip complete APC replies from
  keyboard residue.
- [ ] `build_startup_query_string()` (`stdin_handler.rs:274`): append
  `\x1b_Gi=<probe>,a=q,s=1,v=1,t=d,f=24;AAAA\x1b\\\x1b[c`. DA1 (`ESC[c`) is the
  negative-detection barrier; the scanner implements the barrier itself (probe is
  fire-and-forget, `:50-54`, not on the `completed_forward` path `:358-366`).
- [ ] `HostReply::KittyGraphics(bool)`; handle in `input_handler.rs`; send
  `ClientToServerMsg::KittyGraphicsSupport { supported }`; `route.rs` attaches
  the connection `client_id` → `ScreenInstruction::TerminalKittyGraphicsSupport(ClientId,bool)`.
- [ ] IPC contract: `ipc.rs`, `client_server_contract/*.proto`,
  `protobuf_conversion.rs` + roundtrip test.
- [ ] `Screen`: `outer_supports_kitty: HashMap<ClientId,bool>`, default false on
  connect, set on probe reply, remove on detach. Web clients stay false.
- [ ] **Tests:** startup probe success; negative path with DA barrier (assert
  against ghostty/wezterm/foot semantics, not just kitty); split APC reply; APC
  reply stripped from residue.

### 1e. Inner `a=q` aggregate (Phase 5 self-query truth)
- [ ] Answer inner `a=q` OK if **any** regular client on the pane's tab has
  `outer_supports_kitty == true` (per locked decision). No supporting client →
  no OK (error only when not a probe and quiet allows). Reconcile with the env
  hint (Phase 5).
- [ ] Static env hint: export `ZELLIJ_GRAPHICS=kitty` (ungated) at child spawn —
  `os_input_output_unix.rs:224` and `os_input_output_windows.rs:177`, beside
  `ZELLIJ_PANE_ID`.
- [ ] **Test:** mixed-support clients (kitty + web) → inner `a=q` says OK.

**Phase 1+2 acceptance:** an app inside a pane gets a correct `a=q` OK/silent
answer based on real per-client outer support; Kitty images are parsed, stored,
and anchored; no rendering yet. `cargo check -p zellij-server -p zellij-client
-p zellij-utils` green; all new unit tests pass.

---

## Phase 3 — render to the outer terminal (single staged model)

> One outer id per `(client_id, source app_image_id)`; re-place/re-transmit the
> whole image when any rect changes. **No per-chunk ids** (they thrash —
> `changed_rects` re-slices every frame and Kitty retransmit deletes
> placements). Spec §5.

- [ ] `KittyImageChunk` (or tagged enum) ≈ `SixelImageChunk`
  (`output/mod.rs:1026-1034`); thread through the five seams:
  `grid.rs:1498 read_changes` → `grid.rs:1561 render` → `terminal_pane.rs:357` →
  `pane_contents_and_ui.rs:81-128` →
  `output/mod.rs:478 add_kitty_image_chunks_to_multiple_clients` (call site `:541`).
- [ ] Inject in `serialize_chunks` (`output/mod.rs:197`, post-text block
  `:289-298`). Per client, after `vte_goto_instruction(cell_x,cell_y)`:
  - transmit once if not in `transmitted[client]`:
    `ESC _ G a=t,q=2,i=<outer_id>,f=<fmt>[,s,v][,o=z];<base64> ESC \`
  - then placement w/ source crop:
    `ESC _ G a=p,q=2,i=<outer_id>,p=<pid>,x,y,w,h,C=1 ESC \`
- [ ] Per-client durable state in `KittyImageStore`: `transmitted`,
  `placements`, `outer_ids` (high/randomized range, always `q=2`). Invalidate on
  full reset / detach / re-attach.
- [ ] Per-base64 cache per source image (like `SixelImageCache`, `sixel.rs:427`).
- [ ] Reuse coverage clip geometry `remove_covered_sixel_parts`
  (`output/mod.rs:830`).
- [ ] Capability gating in serialize: Kitty if `outer_supports_kitty`, else Sixel
  if source was Sixel, else **net-new** placeholder (there is *no* existing
  placeholder — `grid.rs:824` is `impl Debug`, not the render path).
- [ ] **Tests:** floating-pane clipping/scroll that re-slices one source across
  frames renders correctly with one outer id, no transmit/delete thrash; no
  Kitty bytes when `outer_supports_kitty == false` (placeholder instead).

**Acceptance:** a stored Kitty image renders in a supporting outer terminal,
survives partial scroll/coverage; unsupported clients get the placeholder.

---

## Phase 4 — lifecycle & teardown (net-new; no sixel analog)

> Kitty's outer terminal holds placements, so every removal path must emit
> deletes per client. Spec §6.

- [ ] Explicit `a=d` (`d=i`/`d=a`/…): remove from store/placements + emit
  `a=d,i=<outer_id>` (and/or placement delete) to each client.
- [ ] Implicit removal emits outer deletes on every sixel detection path: cell
  overwrite (`grid.rs:1882`), ED (`grid.rs:3848`/`:3851`), reset (`grid.rs:2283`),
  scroll-out (`offset_grid_top`), cover-reap (`sixel.rs:163-181`), render-time
  `drain_image_ids_to_reap` (`grid.rs:1520`).
- [ ] Alt-screen (net-new): `grid.rs:3895` is alt-screen *exit* (not ED). On
  enter emit `a=d` for the leaving screen's placements; on exit re-place primary.
- [ ] Resize: `character_cell_size_possibly_changed` (`sixel.rs:235`) rescales;
  re-emit placements with new crop.
- [ ] Pane close (net-new): enumerate the pane's per-client outer ids and emit
  deletes (avoid leaking outer-terminal memory).
- [ ] **Tests:** alt-screen enter deletes + exit re-places; pane close deletes;
  reset/detach clears per-client transmitted state — assert no orphaned images.

**Acceptance:** no ghost/orphaned images in the outer terminal across delete,
alt-screen (vim/less), resize, pane close, reset, detach.

---

## Final verification
- [ ] Full `cargo test` (workspace) green; clippy clean on changed crates.
- [ ] e2e: add a Kitty case to `src/tests/e2e/remote_runner.rs` (sixel
  scaffolding exists).
- [ ] Manual matrix: `pi` emitting Kitty inside this zellij, inside
  {kitty/ghostty/wezterm} × {xterm}; verify image, partial scroll, pane resize,
  float over image, mixed kitty+web clients, clean teardown.
- [ ] Update `KITTY_GRAPHICS_PLAN.md` status → implemented; note any deviations.

## Out of scope for v1 (placeholder/ignore + document)
negative-z (image below text) · Unicode/virtual placement (U+10EEEE) · Windows
render path (env var only) · `t=f`/`t=s`/animation · Sixel↔Kitty transcode.
