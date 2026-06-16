# Implementation Plan: Kitty Graphics Protocol support in zellij

Status: proposal / not yet implemented
Target: this fork (`~/repos/zellij`, HEAD `b6a5ad0e`)
Author intent: let applications running *inside* a zellij pane (e.g. the `pi`
coding agent) display inline images via the Kitty graphics protocol, the same
way zellij already supports Sixel.

---

## 0. TL;DR

Kitty support can reuse much of the existing **Sixel** machinery (storage,
pixel-rect anchoring, scroll/scrollback reaping, coverage clipping, the chunk
render pipeline). Three things are genuinely new:

1. **The transport is invisible today.** Kitty ships image data in **APC**
   sequences (`ESC _ G … ESC \`). zellij's pinned `vte = 0.11.0` routes APC into
   a dead-end "ignore" state and never calls back into `Perform`. *Nothing can
   work until this is fixed.* This is Phase 0 and the only true blocker.
2. **The query/ack path is part of the protocol.** Apps do not only transmit
   images; they also ask whether Kitty graphics is supported and may wait for
   OK/error acknowledgements or image-number id assignments. Those replies must
   use zellij's existing host-query pause/resume ordering contract, otherwise
   PTY bytes after the query can overtake the reply.
3. **The rendering model is stateful.** Sixel is stateless DCS that zellij
   re-emits wholesale whenever a region changes. Kitty images persist in the
   *outer* terminal's memory under an id and are *placed* — so we must manage an
   outer-terminal image lifecycle (transmit / place / delete) and detect whether
   the outer terminal even speaks Kitty.

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
  → output/mod.rs:260-298 serialize_chunks   ← THE INJECTION POINT
  → output/mod.rs Output::serialize    → HashMap<ClientId, String>
  → screen.rs:2967 ServerInstruction::Render
  → zellij-utils/src/ipc.rs:183 ServerToClientMsg::Render { content: String }
  → [protobuf over unix socket]
  → zellij-client/src/lib.rs:195 ClientInstruction::Render
  → zellij-client/src/lib.rs:531 stdout.write_all(...) → real terminal
```

Key existing pieces to mirror (`zellij-server/src/panes/sixel.rs` unless noted):

| Concept | Sixel impl | Notes for Kitty |
| --- | --- | --- |
| Decoded-image store | `SixelImageStore` `sixel.rs:428` `HashMap<usize,(SixelImage,Cache)>`, shared `Rc<RefCell<…>>` built once at `screen.rs:1569`, cloned down tab→pane→grid | new `KittyImageStore`, same sharing |
| Per-grid image state | `SixelGrid` `sixel.rs:62` (`sixel_image_locations: HashMap<id,PixelRect>`, in-flight parser, reap queue) | new `KittyGrid` or extend `SixelGrid` |
| Position anchoring | `PixelRect{ x, y: isize, w, h }` `sixel.rs:13` — `y` is **absolute pixels over the whole scrollback**, deliberately signed so it can scroll negative | **reuse `PixelRect` verbatim** |
| Scroll into scrollback | `offset_grid_top()` `sixel.rs:213` called from `bounded_push` `grid.rs:314`; reaps when bottom edge ≤ 0 | reuse verbatim |
| Cell↔pixel transform | cursor→abs px `grid.rs:2921`; px→cell chunking `sixel.rs:294,354`; cell px size in shared `Rc<RefCell<Option<SizeInPixels>>>` set at `screen.rs:2452` | reuse |
| Cover / overwrite / clear | cell overwrite punches a hole `grid.rs:1882` → `cut_out`; ED/reset `grid.rs:3848,3895,2283`; alt-screen swap `grid.rs:4009` | Kitty deletes are explicit (`a=d`) **and** these implicit paths |
| Change-driven chunks | `changed_sixel_chunks_in_viewport()` `sixel.rs:344` → `Vec<SixelImageChunk>` `output/mod.rs:1026` (`cell_x,cell_y` + `image_pixel_{x,y,w,h}` + `id`) | a `SixelImageChunk` already carries exactly what a Kitty *placement with source crop* needs |
| Coverage clip (floating panes) | `remove_covered_sixel_parts()` `output/mod.rs:830` splits a chunk around covering panes | reuse the geometry |
| Serialize / inject | `serialize_chunks()` `output/mod.rs:260-298`, images flushed **after** text wrapped in `ESC[s … ESC[u` for z-order | Kitty sequences inject in the same block |

---

## 2. Phase 0 — make APC sequences visible (BLOCKER)

### The problem (verified)

`impl Perform for Grid` (`grid.rs:3434`) implements all 8 methods vte 0.11
defines: `print, execute, hook, put, unhook, osc_dispatch, csi_dispatch,
esc_dispatch`. **vte 0.11's `Perform` trait has no APC method at all.** In the
vendored state table (`~/.cargo/.../vte-0.11.0/src/table.rs`):

```
0x5f => (SosPmApcString, None)          // ESC _  enters APC
SosPmApcString { 0x20..=0x7f => (Anywhere, Ignore) }   // entire "G…;<base64>" dropped
```

`perform_state_change` (`vte-0.11.0/src/lib.rs:144`) wires entry/Put/Unhook
actions for `DcsPassthrough`/`OscString`/`CsiEntry`/… but **not** for
`SosPmApcString`. So every byte of a Kitty APC is silently discarded — zellij
cannot see Kitty graphics today.

### Options

| Option | What | Blast radius | Verdict |
| --- | --- | --- | --- |
| **A. Pre-scan PTY bytes** | In `handle_pty_bytes` (`terminal_pane.rs:216`), run a small state machine that peels `ESC _ G … ESC \` out of the stream *before* `vte_parser.advance`, hands it to a Kitty handler, and forwards the remainder to vte. | `terminal_pane.rs` + new buffering state on `TerminalPane`. **No dependency change.** | **Recommended for a fork.** Self-contained, zero dep divergence → clean rebases on upstream zellij. Cost: we own APC framing across the 64 KB read boundary (`terminal_bytes.rs:53`) and the byte-at-a-time advance loop's `forward_paused`/`pending_pty_input` replay logic. |
| **B. Patch/fork vte 0.11** | Add `apc_dispatch` to the trait, change `SosPmApcString` actions `Ignore→Put`, wire entry/unhook in `perform_state_change`; pull via `[patch.crates.io]` (none today). | new vte fork repo + `Cargo.toml:114` patch + new `apc_*` arm in `grid.rs:3434`. | Cleanest semantically (Kitty then mirrors the DCS `hook/put/unhook` triad exactly, gets ST handling + cross-read continuation for free), but maintains a forked dependency. |
| **C. Upgrade vte** | A newer parser would only help if it exposes APC to `Perform`. The `0.14.1` copy already in `Cargo.lock:4114` still does **not** expose an APC callback; it ignores SOS/PM/APC strings internally. | dependency churn plus potential rewrite of the `Perform` impl and parser call sites, with no guaranteed APC callback in the currently locked newer version. | Not recommended as a first step — high regression risk and does not solve the blocker unless a future vte release adds the missing surface. |

**Decision: Option A (pre-scan).** Rationale: a fork's top priority is cheap
rebasing on upstream zellij; A keeps `Cargo.toml`/`Cargo.lock` identical to
upstream and confines all Kitty code to new zellij modules. Re-evaluate B if the
framing state machine gets hairy.

### Phase 0 deliverables (Option A)

- New `zellij-server/src/panes/kitty_apc.rs`: a resumable, **order-preserving**
  scanner `KittyApcScanner` with states
  `Ground | EscSeen | Apc(buf) | ApcEscSeen`.
  Prefer an API shaped like
  `feed(&mut self, bytes) -> Vec<KittyApcEvent>`, where events are either
  `PassThrough(Vec<u8>)` or `Kitty(KittyApcSequence)`, and every event can
  recover its original bytes for replay. A tuple of "all pass-through bytes" and
  "all complete APCs" is **not** sufficient because it loses the byte position
  where a query/ack pause must happen.
- Must survive an APC split across two `handle_pty_bytes` calls (image payloads
  routinely exceed 64 KB), and must not swallow a non-`G` APC (forward those to
  vte unchanged so other APC users are unaffected).
- Wire it into `terminal_pane.rs:216` *ahead of* the `vte_parser.advance` loop.
  Process events in order. If a Kitty event produces a query/ack that must be
  answered later, enqueue the corresponding `HostQuery`, stop processing, and
  append the raw bytes of all remaining unprocessed events to
  `pending_pty_input`, matching the current `forward_paused` early-break.
- Unit tests: whole APC in one chunk; APC split across 2–3 chunks at every
  boundary (after `ESC`, mid-`G`, mid-base64, between `ESC` and `\`); a
  non-Kitty APC passes through untouched; interleaved normal text + APC; APC
  query followed by normal text pauses before the normal text and replays it
  only after the reply.

---

## 3. Phase 1 — parse & store Kitty images

New module `zellij-server/src/panes/kitty.rs` mirroring `sixel.rs`.

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
| `t` | medium: `d` direct (base64 in payload), `f`/`t` file/temp-file, `s` shared-mem | **Phase 1: support `d` only**; reject/placeholder others |
| `m` | more-chunks flag (`1` = more APCs follow, `0`/absent = last) | reassemble multi-APC payloads |
| `s`,`v` | source width,height in px (raw formats) | dims for raw; for `f=100` read from PNG header |
| `c`,`r` | target columns,rows | optional explicit cell size |
| `x,y,w,h` | source-rect crop (px) | used on placement |
| `z` | z-index | map to our existing image-over-text z-order |
| `q` | quiet (`1` no ok, `2` no errors) | honor separately for replies to the inner app and for zellij's quiet outer-terminal commands |

Reassembly: Kitty requires a single active graphics upload to finish before any
other graphics command is sent. Accumulate payload across `m=1` chunks in one
active upload state, not by assuming every continuation has `i`/`I`. Only the
first chunk has to carry width/height/format/etc.; continuation chunks normally
carry only `m` and maybe `q`. Finalize on `m=0`/absent, and use the cursor
position from the final chunk when the finalized command displays an image.

Dimensions without a decode: reuse the lightweight header trick (PNG IHDR at
bytes 16/20; RGB/RGBA dims come from `s`,`v`). **We do not need to decode pixels
in Phase 1** if we re-transmit the original payload to the outer terminal (see
Phase 3, Stage 1) — store the raw base64 + format + dims. If `o=z` compression
is present, preserve the compressed payload for outer retransmission, but do not
attempt to inspect raw dimensions unless they were supplied by control keys.

Responses to the inner app:

- Honor `q` quiet mode for replies emitted **to the app inside zellij**. Quiet
  mode used by zellij when talking to the outer terminal is a separate concern.
- For `a=q`, do not store or place anything. Route the query through the
  existing `HostQuery` pause/resume machinery so the OK/no-response decision is
  ordered with surrounding PTY bytes.
- For `i`-addressed transmit/place/delete commands, return OK/error when not
  silenced. For unsupported v1 features (`t=f`, `t=s`, animation, malformed
  `i`+`I`), either send an appropriate error unless quiet suppresses it, or
  deliberately stay silent when emulating "graphics unsupported" for a support
  probe.
- For `I` image-number commands, assign a zellij-local app image id and reply
  with `i=<allocated>,I=<number>;OK` unless quiet suppresses it. Future commands
  may then address the image by either the newest number mapping or the returned
  id.

### 3b. Storage & anchoring

- `KittyImageStore` ≈ `SixelImageStore`: `HashMap<u32 /*app image id*/, KittyImage>`
  where `KittyImage { format, payload: Vec<u8>, px_w, px_h }`, shared
  `Rc<RefCell<…>>`, constructed at `screen.rs:1569` and cloned down the same
  chain (`tab/mod.rs` → `terminal_pane.rs:1083` → `grid.rs:914`).
- `KittyGrid` ≈ `SixelGrid`: `placements: HashMap<PlacementKey, PixelRect>`,
  `image_ids_to_reap`, shared cell-size handle. **Reuse `PixelRect` (`sixel.rs:13`)
  unchanged** — the signed absolute-pixel `y` is exactly what we need for
  scroll anchoring.
- On `a=T` (transmit+display) or `a=p` (place): compute anchor pixel coords from
  the cursor (`current_cursor_pixel_coordinates()` `grid.rs:2921`), build a
  `PixelRect`, insert into `placements`, then advance the cursor past the image
  in whole cells (`move_cursor_down_by_pixels` `grid.rs:2908`) — exactly as
  `create_sixel_image` (`grid.rs:2933`) does. Guard on cell-size-known just like
  sixel's `hook` (`grid.rs:3485`).
- Reuse anchoring/reaping wholesale: `offset_grid_top` on `bounded_push`
  (`grid.rs:314`/`sixel.rs:213`), `character_cell_size_possibly_changed`
  (`sixel.rs:235`), and the alt-screen swap (`grid.rs:4009`).

---

## 4. Phase 2 — detect the OUTER terminal's Kitty support

zellij must not emit Kitty sequences to an outer terminal that can't render them.
There is **no graphics capability detection today** (confirmed: no DA1 /
XTSMGRAPHICS / graphics probing anywhere in `zellij-client`/`zellij-utils`).
This capability is **per connected client**, unlike today's pixel-dimension
cache, which is global.

Add one, modelled on the existing pixel-dimensions probe:

- The client already parses terminal responses in
  `zellij-client/src/stdin_ansi_parser.rs` and models them as `HostReply`
  (`PixelDimensions` from CSI `…t` at `from_csi_report` `:106-130`, sync-output,
  theme, color regs). That parser currently only understands OSC and selected
  CSI replies, so add a client-side APC reply scanner with a `partial_apc`
  buffer. Run it before normal keyboard parsing, classify Kitty graphics replies,
  and strip complete APC replies from residue so they are not misinterpreted as
  key input.
- At client startup, extend `build_startup_query_string()`
  (`zellij-client/src/stdin_handler.rs:274`) with the Kitty-recommended probe:
  `\x1b_Gi=<probe>,a=q,s=1,v=1,t=d,f=24;AAAA\x1b\\\x1b[c`. The DA1
  (`ESC[c`) is the negative-detection barrier: if the parser sees the DA reply
  before the Kitty APC OK for this probe id, emit `HostReply::KittyGraphics(false)`;
  if the APC OK arrives first, emit `HostReply::KittyGraphics(true)`.
- Add `HostReply::KittyGraphics(bool)`, handle it in
  `zellij-client/src/input_handler.rs`, and send a new
  `ClientToServerMsg::KittyGraphicsSupport { supported }`. `route.rs` must
  attach the current connection's `client_id` when converting this to
  `ScreenInstruction::TerminalKittyGraphicsSupport(ClientId, bool)`.
- Store `outer_supports_kitty: HashMap<ClientId, bool>` (or a broader
  per-client terminal-capability struct) on `Screen`, defaulting to false on
  connect, updating on the probe reply, and removing on client detach. Web
  clients should remain false until they have an explicit render implementation.
- **Protocol gating** at the serialize step (`output/mod.rs:260`): per client,
  choose Kitty if `outer_supports_kitty`, else Sixel if the image came as Sixel,
  else the text placeholder (`grid.rs:824` already writes a `"Sixel"`
  stand-in — generalize to a graphics placeholder). Images received as Kitty but
  rendered to a Sixel-only outer terminal → placeholder in Phase 1 (cross-format
  transcode is out of scope).

---

## 5. Phase 3 — render Kitty images to the outer terminal

The chunk pipeline is mostly protocol-agnostic and reused as-is. A `SixelImageChunk`
(`output/mod.rs:1026`) already carries `cell_x, cell_y, image_pixel_{x,y,w,h},
image_id` — which is *exactly* a Kitty placement-with-source-crop. Add a parallel
`KittyImageChunk` (or a tagged enum) and thread it through the same five seams:
`grid.rs:1498 read_changes` → `grid.rs:1561 render` tuple →
`terminal_pane.rs:357` → `pane_contents_and_ui.rs:81-128` →
`output/mod.rs:478/541 add_*chunks` → `serialize_chunks` `output/mod.rs:260-298`.

The injection happens in the same post-text, cursor-save/restore block
(`output/mod.rs:289-298`). **Two staged strategies:**

### Stage 1 — chunk-as-image transmit-and-place (simple, correct, bandwidth-heavy)

For each visible chunk emit, after a `vte_goto_instruction(cell_x,cell_y)`:

```
ESC _ G a=T,q=2,i=<outer_id>,f=<fmt>,x=<px_x>,y=<px_y>,w=<px_w>,h=<px_h>,C=1 ; <base64> ESC \
```

i.e. transmit + display with a **source crop** (`x,y,w,h`) handling partial
scroll and pane coverage natively — no pixel surgery (Kitty crops; Sixel had to
`cut_out`). **Do not use one outer id per source image in this stage.** The
Kitty spec deletes an image's existing placements whenever image data is
retransmitted for the same id, so clipped/split chunks would replace each other.

Instead, allocate one outer id per visible **chunk**:
`(client_id, app_image_id, placement_key, source_rect, destination_cell)`.
Re-emitting that exact chunk id replaces only that chunk. If coverage, scrolling,
or resize changes make a previously emitted chunk key disappear, the durable
store must enqueue a delete for that stale chunk id. Stage 1 therefore needs a
small per-client active-chunk-id table in `KittyImageStore`, even though it does
not yet maintain the optimized "source image transmitted once" state.

`C=1` suppresses cursor movement. Cache the base64 per source-rect like
`SixelImageCache` (`sixel.rs:434`) to avoid re-encoding. This mirrors sixel's
stateless re-emit while avoiding the Kitty id replacement trap.

Cost: the full payload crosses the socket + outer terminal for every changed
chunk, and clipped images duplicate the same payload under multiple outer ids.
Acceptable for correctness; optimize in Stage 2. The Stage 1 stale-id table must
also be cleared on full reset, client detach, and source-image reap.

### Stage 2 — transmit once, then cheap placements (optimize)

Transmit each source image to the outer terminal **once**
(`a=t,i=<outer_id>,q=2`), mark it transmitted, then per changed frame emit only
placements with stable placement ids:

```
ESC _ G a=p,q=2,i=<outer_id>,p=<outer_placement_id>,x=…,y=…,w=…,h=…,C=1 ESC \
```

Bandwidth drops from "image per frame" to "placement command per frame". This
needs **persistent per-client transmitted-state** — but `Output` is rebuilt
fresh every render (`screen.rs:2829`), so the "have I transmitted image X to
client C's outer terminal" set must live somewhere durable (extend
`KittyImageStore` with a per-client `transmitted: HashMap<ClientId,HashSet<id>>`
and `placements: HashMap<ClientId,HashSet<(id,placement_id)>>`, invalidated on
full-screen reset / client re-attach). zellij is multi-client, so this is per
outer terminal. Defer until Stage 1 works.

---

## 6. Phase 4 — lifecycle & edge cases

- **Explicit delete:** handle `a=d` APC variants (`d=i` by id, `d=a` all, etc.)
  → remove from `KittyImageStore`/placements and emit a corresponding
  `a=d,i=<outer_id>` to each client's outer terminal on reap.
- **Implicit removal:** reuse every sixel path — cell overwrite punches the
  image (`grid.rs:1882`), ED/erase/reset (`grid.rs:3848,3895,2283`), scroll-out
  reaping (`offset_grid_top`), cover-reaping in `end_image`
  (`sixel.rs:163-181`), render-time `drain_image_ids_to_reap` (`grid.rs:1520`).
  Each reap must also tell the outer terminal to delete. In Stage 1 this means
  draining stale chunk outer ids; in Stage 2 it means deleting source ids and/or
  placement ids.
- **Alt-screen:** swap in a fresh `KittyGrid` sharing the store, stash the
  primary, mirroring `AlternateScreenState` (`grid.rs:4009-4020,4549`).
- **Resize / cell-size change:** `character_cell_size_possibly_changed`
  (`sixel.rs:235`) rescales rects; placements re-emit with new crop.
- **Pane close:** dropping the grid drops its placements; ensure outer-terminal
  images/chunk ids are deleted to avoid leaking outer-terminal memory.
- **Multi-client:** reuse the existing per-`ClientId` chunk maps
  (`output/mod.rs:358`); outer-ids and transmitted-state are per client.

---

## 7. Phase 5 — advertise Kitty support to apps inside the pane (cross-repo contract)

Apps inside zellij (like `pi`) need to know this fork can carry Kitty. Use two
signals with different semantics:

1. **Env var as a static capability hint.** Export a distinguishing variable
   into every pane's environment, e.g. `ZELLIJ_GRAPHICS=kitty`, when this fork is
   built with Kitty proxy support. Do **not** gate this env var on outer-terminal
   support: pane environments are fixed at child spawn time, while outer support
   is discovered asynchronously and can change as clients attach/detach. Plain
   `ZELLIJ` is insufficient because upstream zellij also sets it. Env-only
   detectors may choose to emit Kitty and rely on zellij to render placeholders
   for unsupported outer clients; query-based detection is the runtime truth.
2. **Answer the Kitty self-query as the runtime truth.** Since zellij *is*
   the terminal the inner app talks to, respond to an inner app's
   `\x1b_Gi=…,a=q…\x1b\\` with `\x1b_Gi=…;OK\x1b\\` only when the current
   client-support aggregate says Kitty can be rendered.

Define the v1 aggregate conservatively: answer OK only if every regular client
currently viewing the pane's tab has `outer_supports_kitty == true`. If there
are no such clients, if any support is unknown/false, or if the only clients are
web clients without Kitty rendering, do not send OK (or send an error only when
the app is not asking a support-probe style query and quiet mode allows errors).
This avoids advertising runtime support when any visible outer terminal would
drop the bytes. A later policy can relax this to "any supporting client" because
render output is already per-client gated, but v1 should stay conservative.

---

## 8. Testing

- **Unit (mirror existing sixel tests):** `panes/unit/grid_tests.rs`,
  `output/unit/output_tests.rs`, `panes/unit/terminal_pane_tests.rs` all have
  sixel cases — clone them for Kitty (parse, store, chunk geometry, scroll
  reaping, coverage clipping, alt-screen).
- **Phase 0 scanner:** dedicated boundary-split tests (§2).
- **Capability gating:** unit-test that no Kitty bytes are emitted when
  `outer_supports_kitty == false`.
- **Client APC parser:** startup probe success; startup probe negative path with
  DA barrier; split APC reply; APC reply stripped from keyboard residue.
- **Inner query ordering:** an app Kitty `a=q` followed by normal text pauses,
  replies through `resume_pane_after_forward`, then replays the text in order.
- **Stage 1 id correctness:** floating-pane clipping / coverage that creates
  multiple Kitty chunks for one source image must render all chunks and must not
  replace siblings by sharing one outer image id; stale chunk ids are deleted
  when coverage/resize changes.
- **Multi-client capability:** attach/detach and mixed-support clients exercise
  the conservative aggregate for inner query replies and per-client render
  gating.
- **e2e:** `src/tests/e2e/remote_runner.rs` already has sixel scaffolding; add a
  Kitty image case.
- **Manual matrix:** an app emitting Kitty (e.g. `pi` reading a PNG) inside this
  zellij, inside an outer terminal that {does (kitty/ghostty/wezterm)} × {does
  not (xterm)} support Kitty; verify image, partial scroll, pane resize, float
  over the image, and clean teardown (no orphaned images in the outer terminal).

---

## 9. Risks & open questions

- **Phase 0 framing correctness** is the main risk: mis-handling an APC split
  across reads corrupts both the image and the following byte stream. Heavy
  boundary tests required. If it gets fragile, fall back to Option B (patched
  vte), which gets continuation/ST handling for free.
- **Stream-order correctness** is equally important: APC queries must not be
  handled out of band from `forward_paused`/`pending_pty_input`, or apps can see
  replies after later output.
- **Bandwidth (Stage 1):** re-shipping multi-MB payloads every changed frame
  through the protobuf socket + outer terminal. The 10 ms render debounce
  (`screen.rs:2789`) bounds frequency; the per-rect cache bounds re-encode cost;
  but the socket copies remain until Stage 2.
- **Stage 1 outer-id lifecycle:** chunk-as-image ids are correct but can leak
  outer-terminal memory unless stale chunk ids are explicitly deleted on reap,
  coverage changes, resize, full reset, and client detach.
- **Outer-terminal image-id namespace:** zellij's outer-ids must not collide
  with ids the *inner* app might also be using if any passthrough ever leaks.
  Allocate outer-ids from a high/randomized range and always set `q=2`.
- **`t=f`/`t=s` (file / shared-memory) transmission** and **animation frames**
  are out of scope for v1 (direct `t=d` only); detect and placeholder them.
- **Sixel↔Kitty transcoding** across mismatched outer terminals is out of scope;
  fall back to the text placeholder.

---

## 10. File-by-file change checklist

New:
- `zellij-server/src/panes/kitty_apc.rs` — Phase 0 APC scanner + tests
- `zellij-server/src/panes/kitty.rs` — `KittyImageStore`, `KittyGrid`, parser

Modified:
- `zellij-server/src/host_query.rs` — add `HostQuery::KittyGraphics` or equivalent local-query variant for inner `a=q` support probes and ordered OK/no-reply synthesis
- `zellij-server/src/panes/terminal_pane.rs:216` — run order-preserving APC scanner before `vte_parser.advance`; `:135/:1083` carry scanner state and kitty store; preserve pause/replay semantics
- `zellij-server/src/panes/grid.rs` — `:914/:922` construct `KittyGrid`; placement on transmit/place; reuse reap/scroll/resize/alt-screen hooks; `:1498/:1561` add kitty chunks to `read_changes`/`render`
- `zellij-server/src/panes/mod.rs` — export `kitty`, `kitty_apc`
- `zellij-server/src/output/mod.rs` — `KittyImageChunk` (≈`:1026`), `add_kitty_image_chunks_*` (≈`:478`), inject/delete in `serialize_chunks` (`:289-298`), per-client maps (`:358`), per-client capability gating
- `zellij-server/src/ui/pane_contents_and_ui.rs:81-128` — forward kitty chunks
- `zellij-server/src/screen.rs:1569` build store; `:2452` cell size already shared; `:506` plumb `TerminalKittyGraphicsSupport(ClientId,bool)`; per-client `outer_supports_kitty`; cleanup on detach; conservative aggregate for inner query replies
- `zellij-server/src/route.rs` — route `ClientToServerMsg::KittyGraphicsSupport` with the current connection's `client_id`
- `zellij-client/src/stdin_handler.rs:274` — append Kitty support query + DA barrier to the startup query string
- `zellij-client/src/stdin_ansi_parser.rs` — client-side APC reply scanner, `partial_apc`, startup probe state, parse `HostReply::KittyGraphics`, strip APC replies from residue
- `zellij-client/src/input_handler.rs` — handle `HostReply::KittyGraphics` and send the server capability message
- `zellij-utils/src/ipc.rs`, `zellij-utils/src/client_server_contract/*.proto`, `zellij-utils/src/ipc/protobuf_conversion.rs`, IPC roundtrip tests — add capability message variant
- `zellij-server/src/os_input_output_unix.rs:222` and `zellij-server/src/os_input_output_windows.rs:171` — export static `ZELLIJ_GRAPHICS=kitty` alongside `ZELLIJ_PANE_ID`
