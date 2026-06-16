# Implementation Plan: Kitty Graphics Protocol support in zellij

Status: proposal / not yet implemented
Target: this fork (`~/repos/zellij`, HEAD `b6a5ad0e`)
Author intent: let applications running *inside* a zellij pane (e.g. the `pi`
coding agent) display inline images via the Kitty graphics protocol, the same
way zellij already supports Sixel.

> **Revision note (post-review).** This draft has been revised after a
> source-grounded review. Firm decisions are now baked in (Phase 0 uses the
> patched-vte approach; rendering uses one outer id per source image; inner
> `a=q` answers OK if *any* client supports Kitty). Line numbers have been
> corrected against HEAD `b6a5ad0e`. Several "just mirror sixel" claims were
> wrong and are now called out as net-new work. See §11 for the resolved
> decision log.

---

## 0. TL;DR

Kitty support can reuse much of the existing **Sixel** machinery (storage,
pixel-rect anchoring, scroll/scrollback reaping, coverage clipping, the chunk
render pipeline). Four things are genuinely new — and two of them are load-bearing
design pillars the first draft got wrong:

1. **The transport is invisible today.** Kitty ships image data in **APC**
   sequences (`ESC _ G … ESC \`). zellij's pinned `vte = 0.11.0` routes APC into
   a dead-end "ignore" state and never calls back into `Perform`. *Nothing can
   work until this is fixed.* This is Phase 0 and the only true blocker.
   **Decision: patch vte to surface an APC callback** (was Option A pre-scan;
   see §2 for why that was changed).
2. **The query/ack path is part of the protocol and is stream-ordered.** Apps do
   not only transmit images; they ask whether Kitty is supported and may wait for
   OK/error acks or image-number id assignments. Those replies are **synthesized
   locally** (like `ColorPaletteMode`) and must ride the existing
   `forward_paused`/`pending_pty_input` pause/replay contract so PTY bytes after
   the query cannot overtake the reply. Patching vte (decision #1) is what keeps
   Kitty queries on the *same* ordered queue as existing CSI queries — the
   pre-scan approach would have created a second, racing pause producer.
3. **The rendering model is stateful, and chunk-level ids do not work.** Sixel is
   stateless DCS that zellij re-emits wholesale. Kitty images persist in the
   *outer* terminal under an id and are *placed*. Crucially, `changed_rects`
   re-slices an image into a *different* set of chunks every frame, and Kitty
   *deletes an id's placements whenever its data is retransmitted* — so a
   per-chunk outer id thrashes. **Decision: one outer id per
   `(client_id, source app_image_id)`**, re-placed/re-transmitted on any change
   (see §5).
4. **The outer terminal holds placement state, so teardown is net-new.** Sixel
   needs no outer-terminal cleanup (the outer terminal holds nothing). Kitty does:
   alt-screen swap, pane close, scroll-out, and explicit `a=d` must each emit
   deletes to every client's outer terminal, or images leak/ghost.

Everything else is a close parallel of `zellij-server/src/panes/sixel.rs`, but
the implementation must keep Kitty protocol state explicit rather than treating
APC as just another image payload.

---

## 1. How Sixel works today (the template)

The data path for an inline image, end to end:

```
app PTY bytes → terminal_bytes.rs:67  (ScreenInstruction::PtyBytes)
  → screen.rs                          (feed each byte to the pane's vte::Parser)
  → terminal_pane.rs:227               vte_parser.advance(&mut grid, byte)
  → grid.rs:3434 impl Perform for Grid  hook/put/unhook capture the DCS payload
  → sixel.rs                            SixelGrid + SixelImageStore  (decode + store)
  → grid.rs:1567 Grid::render          read_changes() → Vec<SixelImageChunk>
  → pane_contents_and_ui.rs:92         output.add_sixel_image_chunks_to_*client
  → output/mod.rs:197 serialize_chunks       ← THE INJECTION POINT (:289-298)
  → output/mod.rs Output::serialize    → HashMap<ClientId, String>
  → screen.rs:2967 ServerInstruction::Render
  → zellij-utils/src/ipc.rs:183 ServerToClientMsg::Render { content: String }
  → [protobuf over unix socket]
  → zellij-client/src/lib.rs:195 ClientInstruction::Render
  → zellij-client/src/lib.rs:531 stdout.write_all(...) → real terminal
```

Key existing pieces to mirror (`zellij-server/src/panes/sixel.rs` unless noted).
**Line numbers below are corrected against HEAD `b6a5ad0e`.**

| Concept | Sixel impl | Notes for Kitty |
| --- | --- | --- |
| Decoded-image store | `SixelImageStore` `sixel.rs:429` `HashMap<usize,(SixelImage,Cache)>`, shared `Rc<RefCell<…>>` built once at `screen.rs:1569`, cloned down tab→pane→grid | new `KittyImageStore`, same sharing |
| Per-grid image state | `SixelGrid` `sixel.rs:62-71` (`sixel_image_locations: HashMap<id,PixelRect>`, in-flight parser, reap queue) | new `KittyGrid` or extend `SixelGrid` |
| Position anchoring | `PixelRect{ x, y: isize, w, h }` `sixel.rs:13-19` — `y` is **absolute pixels over the whole scrollback**, deliberately signed so it can scroll negative. **`PixelRect::new(x, y, height, width)` takes height *before* width — easy to mis-call.** | **reuse `PixelRect` verbatim** |
| Scroll into scrollback | `offset_grid_top()` `sixel.rs:213-218` called from `bounded_push` `grid.rs:319`; reaps when bottom edge ≤ 0 | reuse verbatim |
| Cell↔pixel transform | cursor→abs px `grid.rs:2921`; px→cell chunking `sixel.rs:344` (fn start); cell px size in shared `Rc<RefCell<Option<SizeInPixels>>>` assigned at `screen.rs:2455` | reuse |
| Cover / overwrite / clear | cell overwrite punches a hole `grid.rs:1882` → `cut_out`; ED `grid.rs:3848`/`:3851`, reset `grid.rs:2283`; alt-screen swap `grid.rs:4009` | Kitty deletes are explicit (`a=d`) **and** these implicit paths |
| Change-driven chunks | `changed_sixel_chunks_in_viewport()` `sixel.rs:344-423` → `Vec<SixelImageChunk>` `output/mod.rs:1026-1034` (`cell_x,cell_y` + `image_pixel_{x,y,w,h}` + `id`) | a `SixelImageChunk` already carries exactly what a Kitty *placement with source crop* needs |
| Coverage clip (floating panes) | `remove_covered_sixel_parts()` `output/mod.rs:830` splits a chunk around covering panes | reuse the geometry |
| Serialize / inject | `serialize_chunks()` `output/mod.rs:197`, images flushed **after** text wrapped in `ESC[s … ESC[u` for z-order (`:289-298`) | Kitty sequences inject in the same block |

**Correction — there is no existing on-screen placeholder to reuse.** The first
draft pointed at `grid.rs:824` (a `"Sixel"` string) as a placeholder to
generalize. That string lives inside `impl Debug for Grid` (`grid.rs:816`) — the
snapshot/test formatter, **not** the live render path. The real client render
path emits sixel bytes and renders *nothing* for an undisplayable image. A
visible "graphics unsupported" placeholder for a Sixel-only / no-graphics outer
terminal is **net-new code in `serialize_chunks`**, not a one-line generalization.

---

## 2. Phase 0 — make APC sequences visible (BLOCKER)

### The problem (verified)

`impl Perform for Grid` (`grid.rs:3434`) implements all 8 methods vte 0.11
defines: `print, execute, hook, put, unhook, osc_dispatch, csi_dispatch,
esc_dispatch`. **vte 0.11's `Perform` trait has no APC method at all.** In the
vendored state table (`vte-0.11.0/src/table.rs`):

```
table.rs:49     0x5f => (SosPmApcString, None)          // ESC _  enters APC
table.rs:155-161  SosPmApcString { 0x20..=0x7f => (Anywhere, Ignore) }  // body dropped
```

`perform_state_change` (`vte-0.11.0/src/lib.rs:144`) wires entry/Put/Unhook
actions for `DcsPassthrough`/`OscString`/`CsiEntry`/… but **not** for
`SosPmApcString`. So every byte of a Kitty APC is silently discarded — zellij
cannot see Kitty graphics today. (Verified verbatim against the vendored crate.)

### Options considered

| Option | What | Blast radius | Verdict |
| --- | --- | --- | --- |
| **A. Pre-scan PTY bytes** | Peel `ESC _ G … ESC \` out of the stream *before* `vte_parser.advance` and hand it to a Kitty handler. | `terminal_pane.rs` + new buffering state. No dependency change. | **Rejected.** See below. |
| **B. Patch/fork vte 0.11** | Add `apc_dispatch` to the trait, change `SosPmApcString` actions `Ignore→Put`, wire entry/unhook in `perform_state_change`; pull via `[patch.crates.io]`. | new vte fork repo + `Cargo.toml` patch + new `apc_*` arm in `grid.rs:3434`. | **CHOSEN.** |
| **C. Upgrade vte** | `0.14.1` (already in `Cargo.lock:4114`, transitive via `strip-ansi-escapes` only) still routes APC through internal `anywhere()` with **no** `Perform` callback (`vte-0.14.1/src/lib.rs:438-450`). | dependency churn, no APC surface gained. | **Rejected** — does not solve the blocker. |

### Decision: Option B (patch vte). **Changed from the first draft.**

The first draft chose Option A to keep `Cargo.toml`/`Cargo.lock` identical to
upstream for cheap rebasing. The review showed that advantage is outweighed by
two concrete costs that Option A imposes and Option B avoids:

1. **Stream-order correctness (was a blocker).** zellij's query pause is armed
   only when **vte's `Perform` populates `grid.pending_forwarded_queries`**
   (set in the dispatch path, checked at `terminal_pane.rs:228`; replay in
   `tab/mod.rs:2798-2827`). A pre-scanner that strips APC *before* vte means Grid
   never produces the query, so Kitty becomes a **second, independent pause
   producer** feeding the same single `forward_paused` flag and
   `pending_pty_input` queue. A Kitty `a=q` later in a 64 KB read could then pause
   *ahead of* an earlier CSI query vte catches mid-loop — **inverting reply
   order**, the exact failure mode §9 calls out as critical. Routing APC through
   `Perform` keeps Kitty on the *same* ordered queue as every existing query, for
   free.
2. **Framing for free.** Patching vte inherits its complete framing: the 8-bit
   C1 introducer `0x9f` and C1 ST `0x9c`, ST (`ESC \`) handling, and
   continuation across the 64 KB read boundary. Option A would re-implement all
   of this by hand (the first draft's scanner watched only `ESC _`/`ESC \` and
   missed C1 forms — a stream-swallowing bug).

Net: Kitty then mirrors the DCS `hook/put/unhook` triad exactly, and the only
ongoing cost is a single patched dependency. Re-evaluate only if upstream zellij
itself moves to a vte with an APC callback.

### Phase 0 deliverables (Option B)

- **Fork repo `zellij-vte-apc`** (patch of `vte 0.11.0`):
  - Add `fn apc_dispatch(&mut self, bytes: &[u8])` to the `Perform` trait with a
    default empty body (so no other `Perform` impl breaks).
  - In `table.rs`, change `SosPmApcString` body action `Ignore → Put`, and add
    entry/exit (`Clear`/dispatch) actions for `SosPmApcString` in
    `perform_state_change` (`lib.rs:144`), mirroring `OscString`/`DcsPassthrough`.
    Accumulate the APC body in the existing OSC/DCS-style buffer; dispatch on ST
    (both `ESC \` and C1 `0x9c`).
  - Keep SOS/PM (`ESC X` / `ESC ^`) ignored as today — only APC (`ESC _` / C1
    `0x9f`) gains a callback.
- **`Cargo.toml`** `[patch.crates.io]` entry pointing vte at the fork; vendor or
  pin to a commit hash. `Cargo.lock` updates accordingly (the one intentional
  divergence from upstream).
- **`grid.rs:3434`** gains `fn apc_dispatch(&mut self, bytes)`: if the body
  starts with `G`, hand to the Kitty handler (`kitty.rs`); otherwise ignore
  (leave non-`G` APC users unaffected). Guard cell-size-known exactly like
  sixel's `hook` (`grid.rs:3485`).
- **Unit tests** in the fork: APC body delivered via `apc_dispatch`; APC split
  across two `advance` feeds (image payloads routinely exceed 64 KB) reassembles;
  C1 `0x9f`…`0x9c` framing dispatches; a non-`G` APC still reaches the callback
  but is ignored by Grid; SOS/PM remain dropped.

---

## 3. Phase 1 — parse & store Kitty images

New module `zellij-server/src/panes/kitty.rs` mirroring `sixel.rs`.

> **Prerequisite, corrected.** Phase 1's inner-app `a=q` replies need the
> per-client `outer_supports_kitty` aggregate that the first draft deferred to
> Phase 2. **Capability detection (§4) is a prerequisite of Phase 1's reply
> path, not a successor.** Build the client capability plumbing first, then wire
> `a=q` replies.

### 3a. Protocol parsing

A Kitty APC is `G <key=val,…> ; <base64 payload> ST`. Parse the control keys we
need (ignore unknown keys gracefully):

| key | meaning | handling |
| --- | --- | --- |
| `a` | action: `t` transmit, `T` transmit+display, `p` put/place, `d` delete, `q` query | drives the state machine |
| `i` | image id (app-chosen) | primary key in our store |
| `I` | image number (alt addressing) | map to id on response |
| `p` | placement id | a transmitted image may have N placements |
| `f` | format: `24` RGB, `32` RGBA, `100` PNG | needed to know payload kind |
| `t` | medium: `d` direct (base64 in payload), `f`/`t` file/temp-file, `s` shared-mem | **v1: support `d` only**; placeholder others (§9) |
| `m` | more-chunks flag (`1` = more APCs follow, `0`/absent = last) | reassemble multi-APC payloads |
| `s`,`v` | source width,height in px (raw formats) | dims for raw; for `f=100` read from PNG header |
| `c`,`r` | target columns,rows | optional explicit cell size |
| `x,y,w,h` | source-rect crop (px) | used on placement |
| `z` | z-index | **v1: positive/above-text only** (§9) |
| `o` | compression (`z` = zlib) | preserve compressed bytes for outer retransmit; for dims see below |
| `q` | quiet (`1` no ok, `2` no errors) | honor separately for replies to the inner app and for zellij's quiet outer-terminal commands |

Reassembly: Kitty requires a single active graphics upload to finish before any
other graphics command is sent. Accumulate payload across `m=1` chunks in one
active upload state, not by assuming every continuation has `i`/`I`. Only the
first chunk has to carry width/height/format/etc.; continuation chunks normally
carry only `m` and maybe `q`. Finalize on `m=0`/absent, and use the cursor
position from the final chunk when the finalized command displays an image.

**`a=q` may arrive mid-upload.** A query carries no image data and must be
answerable *without* disturbing the in-flight `m=1` accumulator. Explicitly do
not reset/finalize the active upload on a query; synthesize the reply and leave
the accumulator intact.

Dimensions without a decode: reuse the lightweight header trick (PNG IHDR at
bytes 16/20; RGB/RGBA dims come from `s`,`v`). **We do not need to decode pixels
in Phase 1** if we re-transmit the original payload to the outer terminal (see
§5) — store the raw base64 + format + dims.

**`o=z` (zlib) dimension handling (corrected).** Anchoring *requires* pixel
dimensions to build the `PixelRect` and advance the cursor in whole cells
(`create_sixel_image` `grid.rs:2933`, `move_cursor_down_by_pixels` `grid.rs:2908`).
A compressed PNG with no `s,v` is legal (Kitty reads dims from the decompressed
header) and leaves us with no dimension source. **v1 policy:** if `o=z` and
`s,v` are present, use them and pass the compressed bytes through to the outer
terminal untouched; if `o=z` and `s,v` are absent, inflate just enough of the
zlib stream to read the PNG IHDR for dims (still forward the original compressed
bytes for transmission). If neither dims nor a readable header are available,
placeholder the image.

Responses to the inner app (synthesized locally — see §3c):

- Honor `q` quiet mode for replies emitted **to the app inside zellij**. Quiet
  mode used by zellij when talking to the outer terminal is a separate concern.
- For `a=q`, do not store or place anything. Synthesize the OK/no-response and
  order it with surrounding PTY bytes via the pause path.
- For `i`-addressed transmit/place/delete commands, return OK/error when not
  silenced.
- For unsupported v1 features (`t=f`, `t=s`, animation, malformed `i`+`I`),
  send an appropriate error unless quiet suppresses it, or stay silent when
  emulating "graphics unsupported" for a support probe.
- For `I` image-number commands, assign a zellij-local app image id and reply
  with `i=<allocated>,I=<number>;OK` unless quiet suppresses it. **This reply can
  block the app**, so it uses the same synthesize-and-pause path as `a=q`, not
  fire-and-forget.

### 3b. Storage & anchoring

- `KittyImageStore` ≈ `SixelImageStore`: `HashMap<u32 /*app image id*/, KittyImage>`
  where `KittyImage { format, payload: Vec<u8>, px_w, px_h }`, shared
  `Rc<RefCell<…>>`, constructed at `screen.rs:1569` and cloned down the same
  chain (`tab/mod.rs` → `terminal_pane.rs` → `grid.rs`). **It also holds the
  durable per-client outer-terminal state from the start** (see §5) — this is no
  longer deferred.
- `KittyGrid` ≈ `SixelGrid`: `placements: HashMap<PlacementKey, PixelRect>`,
  `image_ids_to_reap`, shared cell-size handle. **Reuse `PixelRect`
  (`sixel.rs:13-19`) unchanged** — the signed absolute-pixel `y` is exactly what
  we need for scroll anchoring. **Call `PixelRect::new(x, y, height, width)` with
  height before width.**
- On `a=T` (transmit+display) or `a=p` (place): compute anchor pixel coords from
  the cursor (`current_cursor_pixel_coordinates()` `grid.rs:2921`), build a
  `PixelRect`, insert into `placements`, then advance the cursor past the image
  in whole cells (`move_cursor_down_by_pixels` `grid.rs:2908`) — exactly as
  `create_sixel_image` (`grid.rs:2933`) does. Guard on cell-size-known just like
  sixel's `hook` (`grid.rs:3485`).
- Reuse anchoring/reaping wholesale: `offset_grid_top` on `bounded_push`
  (`grid.rs:319`/`sixel.rs:213-218`), `character_cell_size_possibly_changed`
  (`sixel.rs:235`), and the alt-screen swap (`grid.rs:4009`).

### 3c. Synthesizing replies (corrected framing)

Inner-app replies are **synthesized locally**, not round-tripped through a host.
The precedent is `ColorPaletteMode` (`screen.rs:2506`, synthesized at
`screen.rs:2538`), which answers a query without going on the wire. `HostQuery`
(`host_query.rs:45`) carries no pane/client fields, so a new local-query variant
(or direct synthesis in the pane) is the right shape. What we reuse from the
host-query machinery is **only the pause/replay ordering**
(`forward_paused`/`pending_pty_input`, replay in `tab/mod.rs:2798-2827`), not a
host round-trip. Add `HostQuery::KittyGraphics` (or equivalent local-synthesis
variant) for inner `a=q`/`I`-allocation ordering.

---

## 4. Phase 2 — detect the OUTER terminal's Kitty support

zellij must not emit Kitty sequences to an outer terminal that can't render them.
There is **no graphics capability detection today** (confirmed: no DA1 /
XTSMGRAPHICS / graphics probing anywhere in `zellij-client`/`zellij-utils`).
This capability is **per connected client**, unlike today's pixel-dimension
cache, which is global. **Build this before wiring Phase 1's `a=q` replies.**

Add one, modelled on the existing pixel-dimensions probe:

- The client already parses terminal responses in
  `zellij-client/src/stdin_ansi_parser.rs` and models them as `HostReply`
  (`PixelDimensions` from CSI `…t` at `from_csi_report` `:106`, sync-output,
  theme, color regs). **This is real work, not a one-liner:** `strip_replies`
  (`stdin_ansi_parser.rs:424-480`) recognizes only `ESC ]` (OSC) and `ESC [`
  (CSI); there is no APC branch and no `partial_apc` field (only
  `partial_osc`/`partial_csi` at `:232-234`). Add a third `strip_replies` branch
  for APC plus a `partial_apc` buffer, mirror the OSC/CSI cap/`finalize()`/
  split-across-feed handling, classify Kitty graphics replies, and strip complete
  APC replies from residue so they are not injected as spurious keypresses.
- At client startup, extend `build_startup_query_string()`
  (`zellij-client/src/stdin_handler.rs:274`) with the Kitty-recommended probe:
  `\x1b_Gi=<probe>,a=q,s=1,v=1,t=d,f=24;AAAA\x1b\\\x1b[c`. The DA1
  (`ESC[c`) is the negative-detection barrier: if the parser sees the DA reply
  before the Kitty APC OK for this probe id, emit `HostReply::KittyGraphics(false)`;
  if the APC OK arrives first, emit `HostReply::KittyGraphics(true)`.
  - **Caveat to verify, not assume:** the barrier relies on the outer terminal
    processing the APC *before* the trailing `ESC[c`. Kitty's spec recommends
    this but does not guarantee it across implementations, so a
    capable-but-slow terminal could be misclassified. The startup probe is
    fire-and-forget (`stdin_handler.rs:50-54`) and won't be captured by the
    existing `completed_forward`/`active_forward` DA1 path (`:358-366`), so the
    scanner must implement the DA barrier itself. **Test against
    ghostty/wezterm/foot, not just kitty.**
- Add `HostReply::KittyGraphics(bool)`, handle it in
  `zellij-client/src/input_handler.rs`, and send a new
  `ClientToServerMsg::KittyGraphicsSupport { supported }`. `route.rs` must
  attach the current connection's `client_id` when converting this to
  `ScreenInstruction::TerminalKittyGraphicsSupport(ClientId, bool)`.
- Store `outer_supports_kitty: HashMap<ClientId, bool>` (or a broader
  per-client terminal-capability struct) on `Screen`, defaulting to false on
  connect, updating on the probe reply, and removing on client detach. Web
  clients remain false until they have an explicit render implementation.
- **Protocol gating** at the serialize step (`output/mod.rs:197`): per client,
  choose Kitty if `outer_supports_kitty`, else Sixel if the image came as Sixel,
  else the **net-new** text placeholder (§1 — there is no existing placeholder to
  generalize). Images received as Kitty but rendered to a Sixel-only outer
  terminal → placeholder in v1 (cross-format transcode is out of scope, §9).

---

## 5. Phase 3 — render Kitty images to the outer terminal

The chunk pipeline is mostly protocol-agnostic and reused as-is. A `SixelImageChunk`
(`output/mod.rs:1026-1034`) already carries `cell_x, cell_y, image_pixel_{x,y,w,h},
image_id`. Add a parallel `KittyImageChunk` (or a tagged enum) and thread it
through the same five seams: `grid.rs:1498 read_changes` → `grid.rs:1561 render`
tuple → `terminal_pane.rs:357` → `pane_contents_and_ui.rs:81-128` →
`output/mod.rs:478` (`add_*_to_multiple_clients`, called at `:541` inside
`serialize()`) → `serialize_chunks` `output/mod.rs:197` (inject `:289-298`).

The injection happens in the same post-text, cursor-save/restore block
(`output/mod.rs:289-298`).

### Why the first draft's two-stage plan was wrong (Blocker 2)

`changed_sixel_chunks_in_viewport` (`sixel.rs:344-423`) emits one chunk **per
`changed_rect` intersecting the image**, and `changed_rects`
(`output/mod.rs:1285-1322`) is recomputed every frame from whatever text changed
*anywhere* in the pane. So a *static* image is sliced into a **different** set of
`(source_rect, cell_y)` chunks frame to frame. The first draft keyed an outer id
on `(client, app_image_id, placement, source_rect, destination_cell)` — both
`source_rect` and `destination_cell` shift every frame. Combined with Kitty's
rule that *retransmitting data for an existing id deletes that id's placements*,
per-chunk ids churn and thrash transmit+delete every frame. Sixel tolerates this
because pixel-writes are idempotent and stateless; **Kitty placements are
stateful.** Per-chunk ids do not work.

### Decision: one outer id per source image, single staged model

Allocate **one outer id per `(client_id, source app_image_id)`**. When any rect
of an image changes (scroll, coverage, resize, content), re-place or re-transmit
the *whole* image for that id. This is correct under churn and folds the first
draft's "Stage 1" and "Stage 2" into one model. The durable per-client state
lives in `KittyImageStore` from the start (not deferred), because `Output` is
rebuilt fresh every render (`screen.rs:2829`) and cannot hold "have I transmitted
image X to client C" across frames:

```
KittyImageStore (additions):
  transmitted: HashMap<ClientId, HashSet<app_image_id>>
  placements:  HashMap<ClientId, HashSet<(app_image_id, placement_id)>>
  outer_ids:   per-client allocation of outer image ids (high/randomized range, §9)
```

Render per changed image, per client (with `outer_supports_kitty == true`):

1. If not yet `transmitted` for this client, transmit once:
   `ESC _ G a=t,q=2,i=<outer_id>,f=<fmt>[,s=,v=][,o=z] ; <base64> ESC \`,
   then mark transmitted.
2. Emit/refresh the placement with a stable placement id and source crop
   (handles partial scroll + pane coverage natively — Kitty crops; Sixel had to
   `cut_out`), after a `vte_goto_instruction(cell_x,cell_y)`:
   `ESC _ G a=p,q=2,i=<outer_id>,p=<placement_id>,x=…,y=…,w=…,h=…,C=1 ESC \`.
3. `C=1` suppresses cursor movement. Cache the base64 per source image (like
   `SixelImageCache` `sixel.rs` cache alias at `:427`) to avoid re-encoding.

When an image's chunk set shrinks (coverage/scroll/resize removes a region),
re-emit placements for the surviving crop; the single id means no sibling-replace
hazard. Bandwidth note: transmit-once + cheap placements is the steady state;
only the first frame (or a content change) ships the payload. The 10 ms render
debounce (`screen.rs:2789`) bounds frequency; the per-image cache bounds
re-encode cost.

---

## 6. Phase 4 — lifecycle & edge cases (teardown is net-new)

Sixel needs **no** outer-terminal teardown (the outer terminal holds nothing).
Kitty's outer terminal holds placements, so every removal path must emit deletes
to each client's outer terminal. This is net-new work with no sixel analog.

- **Explicit delete:** handle `a=d` APC variants (`d=i` by id, `d=a` all, etc.)
  → remove from `KittyImageStore`/placements and emit `a=d,i=<outer_id>` (and/or
  placement delete) to each client's outer terminal.
- **Implicit removal:** reuse every sixel detection path — cell overwrite punches
  the image (`grid.rs:1882`), ED `grid.rs:3848`/`:3851`, reset `grid.rs:2283`,
  scroll-out reaping (`offset_grid_top` `sixel.rs:213-218`), cover-reaping in
  `end_image` (`sixel.rs:163-181`), render-time `drain_image_ids_to_reap`
  (`grid.rs:1520`). **Each reap must additionally emit an outer-terminal delete**
  for the source id and/or placement ids per client.
- **Alt-screen (net-new teardown):** the swap shares the `Rc` store
  (`grid.rs:4009-4020`; `AlternateScreenState` struct begins `grid.rs:4521`).
  Note `grid.rs:3895` is the alt-screen *exit* (`CSI ?1049l`), **not** ED — do
  not file it as an erase path. On alt-screen *enter*, emit `a=d` for the leaving
  screen's placements to each client; on *exit*, re-place the primary screen's
  images. Without this, vim/less leave ghost images.
- **Resize / cell-size change:** `character_cell_size_possibly_changed`
  (`sixel.rs:235`) rescales rects; placements re-emit with new crop.
- **Pane close (net-new teardown):** dropping the grid drops its placements, but
  the outer terminal still holds them. Enumerate the pane's outer ids per client
  and emit deletes, or the outer terminal leaks image memory.
- **Multi-client:** reuse the existing per-`ClientId` chunk maps
  (`output/mod.rs:358`); outer-ids and transmitted-state are per client.
- **Invalidation:** clear all per-client transmitted/placement state on full
  reset, client detach, and source-image reap, and on client re-attach (the new
  outer terminal holds nothing yet).

---

## 7. Phase 5 — advertise Kitty support to apps inside the pane

Apps inside zellij (like `pi`) need to know this fork can carry Kitty. Use two
signals with different semantics:

1. **Env var as a static capability hint.** Export `ZELLIJ_GRAPHICS=kitty` into
   every pane's environment when this fork is built with Kitty proxy support. Do
   **not** gate this env var on outer-terminal support: pane environments are
   fixed at child spawn time, while outer support is discovered asynchronously
   and changes as clients attach/detach. Plain `ZELLIJ` is insufficient (upstream
   zellij also sets it). Env-only detectors may emit Kitty and rely on zellij to
   placeholder unsupported outer clients; query-based detection is the runtime
   truth.
2. **Answer the Kitty self-query as the runtime truth.** Since zellij *is* the
   terminal the inner app talks to, respond to an inner app's
   `ESC _ Gi=…,a=q… ESC \` with `ESC _ Gi=…;OK ESC \` when the current
   client-support aggregate says Kitty can be rendered.

**Decision: answer `a=q` OK if *any* client viewing the pane's tab supports
Kitty (changed from the first draft's conservative "all clients").** Render
output is already per-client (`output/mod.rs:358` per-`ClientId` maps), so a
Kitty client and a web client can coexist — the Kitty client gets the image, the
web client gets a placeholder. The conservative "all clients" rule would have
disabled images for the Kitty user the moment a web client attached (the common
attach-a-second-client case) and would have contradicted the always-on
`ZELLIJ_GRAPHICS=kitty` env hint. Answer OK if **any** regular client on the tab
has `outer_supports_kitty == true`; if no client supports it (or support is
still unknown), do not send OK (send an error only when the app is not running a
support-probe query and quiet mode allows errors). Reconcile this with the env
hint: both should agree that this fork *can* carry Kitty.

---

## 8. Testing

- **Unit (mirror existing sixel tests):** `panes/unit/grid_tests.rs`,
  `output/unit/output_tests.rs`, `panes/unit/terminal_pane_tests.rs` all have
  sixel cases — clone them for Kitty (parse, store, chunk geometry, scroll
  reaping, coverage clipping, alt-screen).
- **Phase 0 (vte fork):** `apc_dispatch` delivery; APC split across `advance`
  feeds; C1 `0x9f`/`0x9c` framing; non-`G` APC ignored by Grid; SOS/PM still
  dropped.
- **Reply ordering:** an app Kitty `a=q` (and `I`-allocation) followed by normal
  text pauses, synthesizes the reply, replays the text in order; a Kitty query
  interleaved with an existing CSI query in the same read preserves stream order
  (the property Option B buys).
- **`a=q` mid-`m=1` upload:** a query during an active multi-chunk upload is
  answered without corrupting the accumulator.
- **`o=z` dims:** compressed PNG with and without `s,v`; IHDR-from-zlib path;
  placeholder when no dims recoverable.
- **Capability gating:** no Kitty bytes emitted when `outer_supports_kitty ==
  false`; net-new placeholder rendered instead.
- **Client APC parser:** startup probe success; negative path with DA barrier
  (against ghostty/wezterm/foot, not just kitty); split APC reply across feeds;
  APC reply stripped from keyboard residue.
- **Render id correctness:** floating-pane clipping / coverage / scroll that
  re-slices one source image across frames renders correctly with a single outer
  id per source image and does not thrash transmit/delete; stale source ids and
  placements are deleted on reap/coverage/resize.
- **Teardown:** alt-screen enter emits deletes and exit re-places; pane close
  deletes outer ids; reset/detach clears per-client transmitted state — assert no
  orphaned outer-terminal images.
- **Multi-client capability:** attach/detach and mixed-support clients exercise
  the "any client" aggregate for inner `a=q` replies and per-client render gating.
- **e2e:** `src/tests/e2e/remote_runner.rs` already has sixel scaffolding; add a
  Kitty image case.
- **Manual matrix:** an app emitting Kitty (e.g. `pi` reading a PNG) inside this
  zellij, inside an outer terminal that {does (kitty/ghostty/wezterm)} × {does
  not (xterm)} support Kitty; verify image, partial scroll, pane resize, float
  over the image, mixed Kitty+web clients, and clean teardown (no orphaned images).

---

## 9. Risks, scope & open questions

### Explicitly out of scope for v1 (placeholder/ignore, documented as future work)

- **Negative `z` (image below text).** The injection block
  (`output/mod.rs:289-298`) hard-codes image-above-text via a single post-text
  `ESC[s…ESC[u`. Image-below-text (backgrounds) needs a second injection point;
  deferred. (So §3a's `z` handling is positive/above-text only in v1.)
- **Unicode placeholder / virtual placement** (U+10EEEE) — how tmux and many TUIs
  place Kitty images. Entirely net-new; deferred but tracked.
- **Windows render path.** v1 exports `ZELLIJ_GRAPHICS=kitty` on Windows but
  ships **no render path** (outer terminals there rarely support Kitty).
- **`t=f` (file) / `t=s` (shared-memory) transmission** and **animation frames** —
  direct `t=d` only; detect and placeholder/error the rest.
- **Sixel↔Kitty transcoding** across mismatched outer terminals — fall back to
  the net-new text placeholder.

### Remaining risks

- **vte fork maintenance.** One patched dependency via `[patch.crates.io]`; pin
  to a commit and re-base when upstream zellij bumps vte. This is the accepted
  cost of Option B (and far cheaper than Option A's two blockers).
- **Stream-order correctness.** Option B keeps Kitty queries on the existing
  single ordered queue; still test interleaved CSI+Kitty queries to confirm no
  regression.
- **Bandwidth (transient).** Transmit-once + placements is the steady state; a
  content change re-ships the payload through the protobuf socket + outer
  terminal. The 10 ms debounce (`screen.rs:2789`) and per-image cache bound it.
- **Outer-terminal id namespace.** zellij's outer ids must not collide with ids
  an inner app might use if any passthrough leaks. Allocate outer ids from a
  high/randomized range and always set `q=2`.
- **Outer-id lifecycle leaks.** Source ids and placements must be deleted on
  reap, coverage change, resize, alt-screen enter, pane close, full reset, and
  client detach (§6).
- **DA-barrier reliability.** The negative-detection trick is recommended, not
  guaranteed; verify across terminals (§4).

### Open questions to resolve during implementation

1. **vte fork hosting** — vendored in-tree vs separate repo pinned by commit?
2. **Single ordered query queue** — confirm APC-sourced and CSI-sourced queries
   share `pending_pty_input`/`forward_paused` cleanly now that both flow through
   `Perform` (Option B should make this automatic — verify).
3. **Outer-id GC completeness** — enumerate every path that must delete (§6) and
   assert no leak in the teardown tests.
4. **`o=z` without `s,v` policy** — confirm inflate-IHDR is acceptable vs
   require-decode vs placeholder for the specific payloads `pi` emits.
5. **Placeholder UX** — what the net-new "graphics unsupported" placeholder looks
   like on screen for Sixel-only / no-graphics outer clients.

---

## 10. File-by-file change checklist

New:
- `zellij-vte-apc` (forked vte 0.11.0) — `apc_dispatch` on `Perform`,
  `SosPmApcString` `Ignore→Put` + entry/exit wiring, C1 framing, tests
- `zellij-server/src/panes/kitty.rs` — `KittyImageStore` (incl. per-client
  transmitted/placement/outer-id state), `KittyGrid`, parser, reply synthesis

Modified:
- `Cargo.toml` / `Cargo.lock` — `[patch.crates.io]` vte → fork (the one
  intentional upstream divergence)
- `zellij-server/src/panes/grid.rs:3434` — add `apc_dispatch` (route `G` APCs to
  kitty, ignore others); `:914/:922` construct `KittyGrid`; placement on
  transmit/place; reuse reap/scroll/resize/alt-screen hooks (`:319`, `:1520`,
  `:4009-4020`); `:1498/:1561` add kitty chunks to `read_changes`/`render`;
  outer-terminal deletes on every reap path (`:1882`, `:3848`/`:3851`, `:2283`)
- `zellij-server/src/host_query.rs` — add `KittyGraphics` local-synthesis variant
  for inner `a=q`/`I`-allocation ordering (synthesize like `ColorPaletteMode`,
  not a host round-trip)
- `zellij-server/src/panes/terminal_pane.rs` — `:227` advance loop now reaches
  `apc_dispatch` via the patched vte (no pre-scan); `:228` query-pause check
  already covers Kitty; carry kitty store on the pane
- `zellij-server/src/panes/mod.rs` — export `kitty`
- `zellij-server/src/output/mod.rs` — `KittyImageChunk` (≈`:1026-1034`),
  `add_kitty_image_chunks_to_multiple_clients` (≈`:478`, call site `:541`),
  inject/delete in `serialize_chunks` (`:197`, block `:289-298`), per-client maps
  (`:358`), per-client capability gating, **net-new** graphics placeholder
- `zellij-server/src/ui/pane_contents_and_ui.rs:81-128` — forward kitty chunks
- `zellij-server/src/screen.rs:1569` build store; `:2455` cell size already
  shared; plumb `TerminalKittyGraphicsSupport(ClientId,bool)`; per-client
  `outer_supports_kitty`; cleanup on detach; **any-client** aggregate for inner
  query replies; reply synthesis precedent at `:2506/:2538`
- `zellij-server/src/route.rs` — route `ClientToServerMsg::KittyGraphicsSupport`
  with the current connection's `client_id`
- `zellij-client/src/stdin_handler.rs:274` — append Kitty probe + DA barrier to
  the startup query string (note fire-and-forget path `:50-54`/`:358-366`)
- `zellij-client/src/stdin_ansi_parser.rs` — third `strip_replies` branch for APC
  (`:424-480`), `partial_apc` buffer (`:232-234`), startup probe state, parse
  `HostReply::KittyGraphics`, strip APC replies from residue
- `zellij-client/src/input_handler.rs` — handle `HostReply::KittyGraphics`, send
  the server capability message
- `zellij-utils/src/ipc.rs`, `zellij-utils/src/client_server_contract/*.proto`,
  `zellij-utils/src/ipc/protobuf_conversion.rs`, IPC roundtrip tests — add
  capability message variant
- `zellij-server/src/os_input_output_unix.rs:224` and
  `zellij-server/src/os_input_output_windows.rs:177` — export static
  `ZELLIJ_GRAPHICS=kitty` alongside `ZELLIJ_PANE_ID`

---

## 11. Decision log (resolved)

| # | Decision | Choice | Rationale |
| --- | --- | --- | --- |
| 1 | Phase 0 APC ingestion | **Patch vte (Option B)** | Keeps Kitty queries on the existing single ordered pause queue (no reply-order inversion); inherits C1/ST/continuation framing for free. Cost: one patched dep. |
| 2 | Inner `a=q` aggregate | **Any client supports** | Matches per-client render gating + the always-on env hint; avoids disabling images when a web/non-Kitty client attaches. |
| 3 | Render staging | **One outer id per source image; stages collapsed** | Per-chunk ids thrash because `changed_rects` re-slices every frame and Kitty retransmit deletes placements. Durable per-client state from the start. |
| 4 | v1 scope-out | **Negative-z, Unicode/virtual placement, Windows render, `t=f`/`t=s`/animation** | All net-new or low-value for v1; placeholdered and tracked as future work. |
