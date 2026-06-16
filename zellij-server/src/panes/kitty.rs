//! Kitty graphics protocol support.
//!
//! Apps inside a zellij pane transmit images using the Kitty graphics protocol
//! (<https://sw.kovidgoyal.net/kitty/graphics-protocol/>), which ships control
//! data and image payloads inside APC sequences: `ESC _ G <keys> ; <payload> ST`.
//! The vendored `vte` fork (`vendor/vte-apc`) surfaces those APC bodies to
//! `Perform::apc_dispatch`; `Grid::apc_dispatch` strips the leading `G` and hands
//! the remainder here.
//!
//! - Phase 1a: classify a command into a [`KittyOutcome`] (query / store / ignore).
//! - Phase 1b (this file): typed control-key parsing ([`KittyControl`]),
//!   dimension extraction without a full decode ([`image_dimensions`]), and
//!   multi-chunk (`m=1`) reassembly ([`PendingUpload`]).
//!
//! Storage/anchoring (wiring the reassembler into the Grid) and rendering land in
//! Phases 1c+. See `GOAL-kitty-graphics.md` and `KITTY_GRAPHICS_PLAN.md`.

use super::sixel::PixelRect;
use crate::output::KittyImageChunk;
use std::collections::HashMap;
use zellij_utils::pane_size::SizeInPixels;

/// The classification of a parsed Kitty graphics command, as far as the Grid
/// boundary cares. The Grid acts on this without re-parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KittyOutcome {
    /// `a=q` — a support probe / capability query. Answered locally (zellij
    /// *is* the terminal the app talks to) and ordered with surrounding PTY
    /// bytes via the host-query pause/replay machinery. Stores nothing.
    Query(KittyQuery),
    /// `a=t` / `a=T` / `a=p` — transmit / transmit+display / place. Carries the
    /// typed control and the (still base64) payload of *this chunk*; the Grid
    /// feeds it to a [`PendingUpload`] for `m=1` reassembly (Phase 1c).
    Store(KittyCommand),
    /// Anything not handled in Phase 1 (delete `a=d`, unknown actions, SOS/PM
    /// payloads that slipped past the `G` gate, malformed input).
    Ignore,
}

/// A Kitty support query (`a=q`). Carries only what is needed to synthesize the
/// reply (`ESC _ G i=<id>[,I=<number>] ; OK ST`) and honour quiet mode.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KittyQuery {
    /// `i=` image id chosen by the app (the probe echoes this back).
    pub id: Option<u32>,
    /// `I=` image number (alternate addressing).
    pub image_number: Option<u32>,
    /// `q=` quiet level: `1` suppresses OK replies, `2` also suppresses errors.
    pub quiet: u8,
}

/// One transmit/display/place command chunk: its typed control plus the
/// undecoded (base64) payload bytes of this chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyCommand {
    pub control: KittyControl,
    /// Bytes after the `;` separator (base64, not yet decoded).
    pub payload: Vec<u8>,
}

/// The action requested by the `a=` control key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KittyAction {
    /// `t` — transmit only. The default when `a=` is absent.
    Transmit,
    /// `T` — transmit and display.
    TransmitAndDisplay,
    /// `p` — put (place an already-transmitted image).
    Place,
    /// `d` — delete.
    Delete,
    /// `q` — query / capability probe.
    Query,
    /// An action key we do not recognise.
    Unknown,
}

/// Pixel format of the transmitted data (`f=`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KittyFormat {
    /// `f=24` — packed RGB.
    Rgb,
    /// `f=32` — packed RGBA. The Kitty default when `f=` is absent.
    Rgba,
    /// `f=100` — PNG.
    Png,
}

/// Transmission medium (`t=`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KittyMedium {
    /// `t=d` — direct: base64 payload inline. The default, and the only medium
    /// supported in v1.
    Direct,
    /// `t=f` — a regular file path.
    File,
    /// `t=t` — a temporary file path.
    TempFile,
    /// `t=s` — a POSIX shared-memory object.
    SharedMemory,
}

/// Typed view of the Kitty control keys this implementation cares about.
/// Unknown keys are ignored (forward-compatible).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyControl {
    pub action: KittyAction,
    pub format: KittyFormat,
    pub medium: KittyMedium,
    /// `i=` app-chosen image id.
    pub image_id: Option<u32>,
    /// `I=` image number.
    pub image_number: Option<u32>,
    /// `p=` placement id.
    pub placement_id: Option<u32>,
    /// `m=1` — more chunks follow.
    pub more: bool,
    /// `s=` source width in px (raw formats).
    pub src_width: Option<u32>,
    /// `v=` source height in px (raw formats).
    pub src_height: Option<u32>,
    /// `o=z` — payload is zlib-compressed.
    pub compressed: bool,
    /// `q=` quiet level.
    pub quiet: u8,
}

impl KittyControl {
    /// Whether v1 can handle this command's transmission medium and format.
    /// v1 supports only direct (`t=d`) transmission; `t=f`/`t=s` are placeholders.
    pub fn medium_supported(&self) -> bool {
        matches!(self.medium, KittyMedium::Direct)
    }
}

impl KittyQuery {
    /// The `OK` reply for this query, or `None` if quiet mode suppresses it.
    /// Caller decides *whether* to answer OK (capability gating, Phase 1e); this
    /// only builds the bytes.
    pub fn ok_reply(&self) -> Option<Vec<u8>> {
        if self.quiet >= 1 {
            // q=1 (and q=2) suppress OK responses.
            return None;
        }
        let mut keys = String::new();
        if let Some(id) = self.id {
            keys.push_str(&format!("i={}", id));
        }
        if let Some(number) = self.image_number {
            if !keys.is_empty() {
                keys.push(',');
            }
            keys.push_str(&format!("I={}", number));
        }
        Some(format!("\x1b_G{};OK\x1b\\", keys).into_bytes())
    }
}

/// Parse the body of a Kitty graphics APC (the bytes *after* the leading `G`)
/// into a [`KittyOutcome`]. `body` is `<keys>[;<payload>]`.
pub fn dispatch(body: &[u8]) -> KittyOutcome {
    let (keys_bytes, payload) = match body.iter().position(|&b| b == b';') {
        Some(i) => (&body[..i], body[i + 1..].to_vec()),
        None => (body, Vec::new()),
    };
    let control = parse_control(&parse_keys(keys_bytes));

    match control.action {
        KittyAction::Query => KittyOutcome::Query(KittyQuery {
            id: control.image_id,
            image_number: control.image_number,
            quiet: control.quiet,
        }),
        KittyAction::Transmit | KittyAction::TransmitAndDisplay | KittyAction::Place => {
            KittyOutcome::Store(KittyCommand { control, payload })
        },
        // `d` (delete) and unknown actions are handled in later phases.
        KittyAction::Delete | KittyAction::Unknown => KittyOutcome::Ignore,
    }
}

/// Parse a comma-separated list of `k=v` control keys. Keys are single ASCII
/// characters; unknown or malformed entries are skipped (forward-compatible).
pub fn parse_keys(bytes: &[u8]) -> HashMap<char, String> {
    let mut keys = HashMap::new();
    if bytes.is_empty() {
        return keys;
    }
    for pair in bytes.split(|&b| b == b',') {
        let Some(eq) = pair.iter().position(|&b| b == b'=') else {
            continue;
        };
        let (k, v) = (&pair[..eq], &pair[eq + 1..]);
        if k.len() != 1 {
            continue;
        }
        let key = k[0] as char;
        if let Ok(value) = std::str::from_utf8(v) {
            keys.insert(key, value.to_string());
        }
    }
    keys
}

/// Build a typed [`KittyControl`] from raw control keys. Absent keys take Kitty
/// defaults (action `t`, format RGBA, medium direct).
pub fn parse_control(keys: &HashMap<char, String>) -> KittyControl {
    let num = |k: char| keys.get(&k).and_then(|v| v.parse::<u32>().ok());

    let action = match keys.get(&'a').map(String::as_str) {
        None | Some("t") => KittyAction::Transmit,
        Some("T") => KittyAction::TransmitAndDisplay,
        Some("p") => KittyAction::Place,
        Some("d") => KittyAction::Delete,
        Some("q") => KittyAction::Query,
        Some(_) => KittyAction::Unknown,
    };
    let format = match keys.get(&'f').map(String::as_str) {
        Some("24") => KittyFormat::Rgb,
        Some("100") => KittyFormat::Png,
        // Kitty default is f=32 (RGBA).
        _ => KittyFormat::Rgba,
    };
    let medium = match keys.get(&'t').map(String::as_str) {
        Some("f") => KittyMedium::File,
        Some("t") => KittyMedium::TempFile,
        Some("s") => KittyMedium::SharedMemory,
        // Kitty default is t=d (direct).
        _ => KittyMedium::Direct,
    };

    KittyControl {
        action,
        format,
        medium,
        image_id: num('i'),
        image_number: num('I'),
        placement_id: num('p'),
        more: keys.get(&'m').map(String::as_str) == Some("1"),
        src_width: num('s'),
        src_height: num('v'),
        compressed: keys.get(&'o').map(String::as_str) == Some("z"),
        quiet: keys.get(&'q').and_then(|v| v.parse().ok()).unwrap_or(0),
    }
}

/// Decode a base64 payload, tolerating no embedded whitespace (Kitty chunks are
/// raw base64). Returns `None` on malformed input.
pub fn decode_payload(b64: &[u8]) -> Option<Vec<u8>> {
    base64::decode(b64).ok()
}

/// Determine the image's pixel dimensions without a full pixel decode.
///
/// - raw formats (`f=24`/`f=32`): from the `s`,`v` control keys;
/// - PNG (`f=100`): from the IHDR header of the decoded payload;
/// - `o=z` (zlib) without `s`,`v`: not recoverable without inflate (no zlib
///   decoder is currently vendored), so returns `None` → caller placeholders.
///   TODO(Phase 1b follow-up): inflate just enough to read the PNG IHDR.
pub fn image_dimensions(control: &KittyControl, decoded_payload: &[u8]) -> Option<(u32, u32)> {
    if let (Some(w), Some(h)) = (control.src_width, control.src_height) {
        return Some((w, h));
    }
    if control.compressed {
        return None;
    }
    match control.format {
        KittyFormat::Png => png_dimensions(decoded_payload),
        // Raw RGB/RGBA carry no header; dimensions must come from s,v.
        KittyFormat::Rgb | KittyFormat::Rgba => None,
    }
}

/// Read width/height from a PNG IHDR. Layout: 8-byte signature, then the IHDR
/// chunk `len(4) "IHDR"(4) width(4) height(4) …` — width at byte 16, height at
/// byte 20, both big-endian.
fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    if bytes.len() < 24 || !bytes.starts_with(PNG_SIGNATURE) {
        return None;
    }
    let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    Some((w, h))
}

/// A finalized image, ready for the store (Phase 1c). The base64 payload is kept
/// undecoded so it can be re-transmitted to the outer terminal verbatim
/// (Phase 3), including when `o=z`-compressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinishedImage {
    pub control: KittyControl,
    pub payload_b64: Vec<u8>,
    pub dimensions: Option<(u32, u32)>,
}

/// Accumulates a multi-chunk (`m=1`) Kitty upload. Kitty requires a single
/// active graphics upload to finish before any other graphics command; only the
/// first chunk carries the control keys (format/dims/id), continuation chunks
/// carry just `m` (and maybe `q`). The Grid owns one `Option<PendingUpload>`
/// (Phase 1c); a support query (`a=q`) that arrives mid-upload must NOT disturb
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingUpload {
    /// Control from the FIRST chunk (continuation chunks' control is ignored
    /// except for the `more` flag, tracked separately on append).
    control: KittyControl,
    /// Accumulated base64 payload across all chunks so far.
    payload_b64: Vec<u8>,
}

impl PendingUpload {
    /// Begin an upload from its first chunk.
    pub fn begin(control: KittyControl, first_chunk_b64: &[u8]) -> Self {
        PendingUpload {
            control,
            payload_b64: first_chunk_b64.to_vec(),
        }
    }

    /// Append a continuation chunk's base64 payload.
    pub fn append(&mut self, chunk_b64: &[u8]) {
        self.payload_b64.extend_from_slice(chunk_b64);
    }

    /// The image id (`i`) this upload addresses, if any.
    pub fn image_id(&self) -> Option<u32> {
        self.control.image_id
    }

    /// Finalize the upload, computing dimensions from control + decoded payload.
    pub fn finish(self) -> FinishedImage {
        let dimensions = match decode_payload(&self.payload_b64) {
            Some(decoded) => image_dimensions(&self.control, &decoded),
            // If the base64 itself is malformed we can still anchor from s,v.
            None => image_dimensions(&self.control, &[]),
        };
        FinishedImage {
            control: self.control,
            payload_b64: self.payload_b64,
            dimensions,
        }
    }
}

/// A stored, finalized Kitty image, keyed by its (app-chosen or allocated) id.
/// The base64 payload is kept undecoded for verbatim re-transmission to the
/// outer terminal (Phase 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredKittyImage {
    pub control: KittyControl,
    pub payload_b64: Vec<u8>,
    pub dimensions: Option<(u32, u32)>,
}

/// An anchored placement of a stored image. `rect.y` is absolute pixels over the
/// whole scrollback (signed, so it scrolls negative) — see [`PixelRect`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyPlacement {
    pub image_id: u32,
    pub placement_id: Option<u32>,
    pub rect: PixelRect,
}

/// Returned by [`KittyGrid::feed_chunk`] when a finalized command wants a
/// placement anchored at the cursor (transmit+display or place). The Grid owns
/// the cursor↔pixel transform, so it computes the rect and calls back into
/// [`KittyGrid::add_placement`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlacementRequest {
    pub image_id: u32,
    pub placement_id: Option<u32>,
}

/// Per-grid Kitty graphics state: the active multi-chunk upload, the stored
/// images, and their placements.
///
/// Phase 1c keeps this grid-local (constructed by `Grid::new` with no signature
/// change). Promotion to a shared `Rc<RefCell<KittyImageStore>>` threaded like
/// the sixel store — needed only once the Phase 3 render path must read image
/// payloads outside the grid — is deferred. See `GOAL-kitty-graphics.md`.
#[derive(Debug, Clone, Default)]
pub struct KittyGrid {
    upload: Option<PendingUpload>,
    images: HashMap<u32, StoredKittyImage>,
    placements: Vec<KittyPlacement>,
    /// Counter for ids we allocate locally (anonymous transmits / `I`-number).
    next_local_id: u32,
}

impl KittyGrid {
    /// Feed one `Store` command chunk. Drives `m=1` reassembly. Returns a
    /// [`PlacementRequest`] when a finalized command should be anchored at the
    /// cursor (transmit+display `a=T`, or place `a=p`); `None` while more chunks
    /// are expected, on a transmit-only (`a=t`) finalize, or for a `place` of an
    /// unknown id.
    pub fn feed_chunk(&mut self, cmd: KittyCommand) -> Option<PlacementRequest> {
        match cmd.control.action {
            // Place references an already-stored image; no payload reassembly.
            KittyAction::Place => {
                let id = cmd.control.image_id?;
                self.images.contains_key(&id).then_some(PlacementRequest {
                    image_id: id,
                    placement_id: cmd.control.placement_id,
                })
            },
            KittyAction::Transmit | KittyAction::TransmitAndDisplay => {
                let more = cmd.control.more;
                match self.upload.as_mut() {
                    // Continuation chunk: append to the active upload (its
                    // control — action/format/id — comes from the first chunk).
                    Some(upload) => upload.append(&cmd.payload),
                    // First chunk (also the single-chunk case).
                    None => {
                        self.upload =
                            Some(PendingUpload::begin(cmd.control.clone(), &cmd.payload));
                    },
                }
                if more {
                    return None; // await continuation chunks
                }
                let upload = self.upload.take()?;
                let display = matches!(upload.control.action, KittyAction::TransmitAndDisplay);
                let placement_id = upload.control.placement_id;
                let finished = upload.finish();
                let id = self.allocate_id(finished.control.image_id);
                self.images.insert(
                    id,
                    StoredKittyImage {
                        control: finished.control,
                        payload_b64: finished.payload_b64,
                        dimensions: finished.dimensions,
                    },
                );
                display.then_some(PlacementRequest {
                    image_id: id,
                    placement_id,
                })
            },
            KittyAction::Delete | KittyAction::Query | KittyAction::Unknown => None,
        }
    }

    /// Resolve the id to store under: the app-chosen `i` if given, else a fresh
    /// locally-allocated id that does not collide with a stored image.
    fn allocate_id(&mut self, requested: Option<u32>) -> u32 {
        if let Some(id) = requested {
            return id;
        }
        loop {
            self.next_local_id = self.next_local_id.wrapping_add(1);
            if self.next_local_id != 0 && !self.images.contains_key(&self.next_local_id) {
                return self.next_local_id;
            }
        }
    }

    /// Pixel dimensions of a stored image, if known.
    pub fn image_dimensions(&self, image_id: u32) -> Option<(u32, u32)> {
        self.images.get(&image_id)?.dimensions
    }

    /// Anchor a placement of a stored image at `rect`.
    pub fn add_placement(&mut self, image_id: u32, placement_id: Option<u32>, rect: PixelRect) {
        self.placements.push(KittyPlacement {
            image_id,
            placement_id,
            rect,
        });
    }

    // --- accessors (rendering/lifecycle phases + tests) ---

    pub fn has_active_upload(&self) -> bool {
        self.upload.is_some()
    }
    pub fn image_count(&self) -> usize {
        self.images.len()
    }
    pub fn stored_image(&self, image_id: u32) -> Option<&StoredKittyImage> {
        self.images.get(&image_id)
    }
    pub fn placements(&self) -> &[KittyPlacement] {
        &self.placements
    }

    /// Produce the visible-in-viewport [`KittyImageChunk`]s for this grid. Each
    /// placement is clipped to the viewport (top/bottom by scroll, right edge by
    /// width); the source crop (`src_*`) carries the visible region. Coverage
    /// splitting around floating panes is applied later in `Output` (reusing the
    /// sixel geometry), so v1 emits one chunk per visible placement.
    pub fn visible_kitty_chunks(
        &self,
        scrollback_size_in_lines: usize,
        viewport_rows: usize,
        viewport_width_in_cells: usize,
        viewport_x_offset: usize,
        viewport_y_offset: usize,
        cell_size: SizeInPixels,
    ) -> Vec<KittyImageChunk> {
        let (cell_w, cell_h) = (cell_size.width, cell_size.height);
        if cell_w == 0 || cell_h == 0 {
            return vec![];
        }
        let viewport_top_px = (scrollback_size_in_lines * cell_h) as isize;
        let viewport_bottom_px = viewport_top_px + (viewport_rows * cell_h) as isize;
        let viewport_right_px = viewport_width_in_cells * cell_w;

        let mut chunks = Vec::new();
        for (idx, placement) in self.placements.iter().enumerate() {
            let Some(image) = self.images.get(&placement.image_id) else {
                continue;
            };
            let rect = &placement.rect;
            let image_top = rect.y;
            let image_bottom = rect.y + rect.height as isize;
            let visible_top = image_top.max(viewport_top_px);
            let visible_bottom = image_bottom.min(viewport_bottom_px);
            if visible_bottom <= visible_top {
                continue; // scrolled fully out of the viewport
            }
            // Source crop within the image.
            let src_y = (visible_top - image_top) as usize;
            let src_height = (visible_bottom - visible_top) as usize;
            let src_width = if rect.x + rect.width <= viewport_right_px {
                rect.width
            } else {
                viewport_right_px.saturating_sub(rect.x)
            };
            if src_width == 0 {
                continue;
            }
            // Viewport cell position of the visible top-left (cell-aligned: both
            // the anchor and the viewport top are multiples of cell_h).
            let cell_x = viewport_x_offset + rect.x / cell_w;
            let cell_y = viewport_y_offset + ((visible_top - viewport_top_px) as usize) / cell_h;

            let (full_w, full_h) = image
                .dimensions
                .unwrap_or((rect.width as u32, rect.height as u32));
            let format = match image.control.format {
                KittyFormat::Rgb => 24,
                KittyFormat::Rgba => 32,
                KittyFormat::Png => 100,
            };
            chunks.push(KittyImageChunk {
                cell_x,
                cell_y,
                source_image_id: placement.image_id,
                placement_id: (idx as u32) + 1,
                format,
                compressed: image.control.compressed,
                full_width: full_w as usize,
                full_height: full_h as usize,
                src_x: 0,
                src_y,
                src_width,
                src_height,
                payload_b64: image.payload_b64.clone(),
            });
        }
        chunks
    }
}

/// Durable, per-client state for rendering Kitty images to outer terminals.
///
/// `Output` is rebuilt every render, so this lives on `Screen` (behind an
/// `Rc<RefCell>`) and is handed to each fresh `Output`. It implements the
/// "transmit once, then place" model: one outer-terminal image id per
/// `(client, source image id)`, transmitted a single time, then re-placed each
/// frame. zellij allocates outer ids from a high base so they do not collide
/// with ids an inner app might use if any passthrough ever leaks.
#[derive(Debug)]
pub struct KittyRenderState {
    /// `(client, source image id)` → outer-terminal image id.
    outer_ids: HashMap<(u16, u32), u32>,
    /// Outer ids already transmitted to each client's terminal.
    transmitted: HashMap<(u16, u32), bool>,
    /// Next outer id to hand out (allocated from a high base).
    next_outer_id: u32,
}

impl Default for KittyRenderState {
    fn default() -> Self {
        KittyRenderState {
            outer_ids: HashMap::new(),
            transmitted: HashMap::new(),
            // High base: keep zellij's outer ids clear of low app-chosen ids.
            next_outer_id: 0x9000_0000,
        }
    }
}

impl KittyRenderState {
    /// Resolve (allocating if needed) the outer-terminal id for a source image
    /// on a given client.
    fn outer_id(&mut self, client_id: u16, source_image_id: u32) -> u32 {
        if let Some(id) = self.outer_ids.get(&(client_id, source_image_id)) {
            return *id;
        }
        let id = self.next_outer_id;
        self.next_outer_id = self.next_outer_id.wrapping_add(1).max(0x9000_0000);
        self.outer_ids.insert((client_id, source_image_id), id);
        id
    }

    /// Forget all state for a client (detach / re-attach: the new outer terminal
    /// holds nothing).
    pub fn reset_client(&mut self, client_id: u16) {
        self.outer_ids.retain(|(c, _), _| *c != client_id);
        self.transmitted.retain(|(c, _), _| *c != client_id);
    }

    /// Forget a source image across all clients (on reap / delete). Returns the
    /// outer ids that were live so the caller can emit deletes (Phase 4).
    pub fn forget_image(&mut self, source_image_id: u32) -> Vec<(u16, u32)> {
        let mut deleted = Vec::new();
        self.outer_ids.retain(|(c, src), outer| {
            if *src == source_image_id {
                deleted.push((*c, *outer));
                false
            } else {
                true
            }
        });
        self.transmitted
            .retain(|(_, src), _| *src != source_image_id);
        deleted
    }

    /// Emit the outer-terminal byte sequence to render one visible chunk on
    /// `client_id`: a one-time transmit (`a=t`) of the source image, then a
    /// placement (`a=p`) with a source crop. `q=2` silences acks; `C=1` keeps
    /// the outer cursor from moving. Always quiet to the outer terminal.
    pub fn render_chunk_bytes(
        &mut self,
        client_id: u16,
        chunk: &KittyChunkSpec,
    ) -> Vec<u8> {
        let outer_id = self.outer_id(client_id, chunk.source_image_id);
        let mut out = Vec::new();
        // Transmit the source image once per (client, image).
        let already = *self.transmitted.get(&(client_id, chunk.source_image_id)).unwrap_or(&false);
        if !already {
            let mut keys = format!("a=t,q=2,i={},f={}", outer_id, chunk.format);
            if chunk.compressed {
                keys.push_str(",o=z");
            }
            if chunk.format != 100 {
                // raw formats need explicit dims
                keys.push_str(&format!(",s={},v={}", chunk.full_width, chunk.full_height));
            }
            out.extend_from_slice(b"\x1b_G");
            out.extend_from_slice(keys.as_bytes());
            out.push(b';');
            out.extend_from_slice(&chunk.payload_b64);
            out.extend_from_slice(b"\x1b\\");
            self.transmitted.insert((client_id, chunk.source_image_id), true);
        }
        // Place (with source crop) — re-emitted each frame; cheap.
        let place = format!(
            "\x1b_Ga=p,q=2,i={},p={},x={},y={},w={},h={},C=1\x1b\\",
            outer_id, chunk.placement_id, chunk.src_x, chunk.src_y, chunk.src_width, chunk.src_height
        );
        out.extend_from_slice(place.as_bytes());
        out
    }

    /// Emit a delete of a source image's placements/data on a client.
    pub fn delete_image_bytes(outer_id: u32) -> Vec<u8> {
        format!("\x1b_Ga=d,d=i,q=2,i={}\x1b\\", outer_id).into_bytes()
    }
}

/// The data a serialize step needs to emit one visible Kitty placement. Mirrors
/// the fields of `output::KittyImageChunk` but kept here so the emission logic is
/// unit-testable without the `Output` machinery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyChunkSpec {
    pub source_image_id: u32,
    pub placement_id: u32,
    pub format: u32,
    pub compressed: bool,
    pub full_width: usize,
    pub full_height: usize,
    pub src_x: usize,
    pub src_y: usize,
    pub src_width: usize,
    pub src_height: usize,
    pub payload_b64: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control_of(body: &[u8]) -> KittyControl {
        parse_control(&parse_keys(body))
    }

    fn store_cmd(body: &[u8]) -> KittyCommand {
        match dispatch(body) {
            KittyOutcome::Store(cmd) => cmd,
            other => panic!("expected Store, got {:?}", other),
        }
    }

    #[test]
    fn query_is_classified() {
        // The startup probe shape: i=<id>,a=q,s=1,v=1,t=d,f=24;AAAA
        let out = dispatch(b"i=31,a=q,s=1,v=1,t=d,f=24;AAAA");
        assert_eq!(
            out,
            KittyOutcome::Query(KittyQuery {
                id: Some(31),
                image_number: None,
                quiet: 0,
            })
        );
    }

    #[test]
    fn query_with_image_number_and_quiet() {
        let out = dispatch(b"I=7,a=q,q=2");
        assert_eq!(
            out,
            KittyOutcome::Query(KittyQuery {
                id: None,
                image_number: Some(7),
                quiet: 2,
            })
        );
    }

    #[test]
    fn transmit_and_display_is_store_with_typed_control() {
        match dispatch(b"a=T,f=100,i=1;iVBORw0KGgo=") {
            KittyOutcome::Store(cmd) => {
                assert_eq!(cmd.control.action, KittyAction::TransmitAndDisplay);
                assert_eq!(cmd.control.format, KittyFormat::Png);
                assert_eq!(cmd.control.image_id, Some(1));
                assert_eq!(cmd.payload, b"iVBORw0KGgo=".to_vec());
            },
            other => panic!("expected Store, got {:?}", other),
        }
    }

    #[test]
    fn defaults_when_keys_absent() {
        let c = control_of(b"i=5");
        assert_eq!(c.action, KittyAction::Transmit); // default a=t
        assert_eq!(c.format, KittyFormat::Rgba); // default f=32
        assert_eq!(c.medium, KittyMedium::Direct); // default t=d
        assert!(!c.more);
        assert!(c.medium_supported());
    }

    #[test]
    fn unsupported_media_flagged() {
        assert!(!control_of(b"a=t,t=f").medium_supported());
        assert!(!control_of(b"a=t,t=s").medium_supported());
        assert_eq!(control_of(b"a=t,t=f").medium, KittyMedium::File);
    }

    #[test]
    fn delete_and_unknown_are_ignored_in_phase1() {
        assert_eq!(dispatch(b"a=d,i=1"), KittyOutcome::Ignore);
        assert_eq!(dispatch(b"a=z"), KittyOutcome::Ignore);
    }

    #[test]
    fn malformed_keys_skipped() {
        let out = dispatch(b"a=q,,=,xx=1,i=5");
        assert_eq!(
            out,
            KittyOutcome::Query(KittyQuery {
                id: Some(5),
                image_number: None,
                quiet: 0,
            })
        );
    }

    #[test]
    fn ok_reply_echoes_id_and_honours_quiet() {
        let q = KittyQuery { id: Some(31), image_number: None, quiet: 0 };
        assert_eq!(q.ok_reply(), Some(b"\x1b_Gi=31;OK\x1b\\".to_vec()));

        let q_num = KittyQuery { id: Some(2), image_number: Some(9), quiet: 0 };
        assert_eq!(q_num.ok_reply(), Some(b"\x1b_Gi=2,I=9;OK\x1b\\".to_vec()));

        let quiet = KittyQuery { id: Some(1), image_number: None, quiet: 1 };
        assert_eq!(quiet.ok_reply(), None);
    }

    #[test]
    fn raw_dimensions_from_s_v() {
        let c = control_of(b"a=t,f=32,s=64,v=48");
        assert_eq!(image_dimensions(&c, &[]), Some((64, 48)));
    }

    #[test]
    fn png_dimensions_from_ihdr() {
        // Minimal PNG header: signature + IHDR len + "IHDR" + width(5) + height(7)
        let mut png = Vec::new();
        png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        png.extend_from_slice(&[0, 0, 0, 13]); // IHDR length
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&5u32.to_be_bytes());
        png.extend_from_slice(&7u32.to_be_bytes());
        let c = control_of(b"a=t,f=100");
        assert_eq!(image_dimensions(&c, &png), Some((5, 7)));
    }

    #[test]
    fn compressed_without_dims_is_unknown() {
        // o=z PNG without s,v: not recoverable without inflate → None (placeholder).
        let c = control_of(b"a=t,f=100,o=z");
        assert!(c.compressed);
        assert_eq!(image_dimensions(&c, b"\x89PNG\r\n\x1a\n............"), None);
        // …but explicit s,v win even when compressed.
        let c2 = control_of(b"a=t,f=100,o=z,s=10,v=20");
        assert_eq!(image_dimensions(&c2, &[]), Some((10, 20)));
    }

    #[test]
    fn reassembly_concatenates_chunks_and_finishes() {
        // "AAAA" base64-decodes to 3 zero bytes; split across two chunks.
        let first = control_of(b"a=t,f=32,s=1,v=1,m=1");
        let mut upload = PendingUpload::begin(first, b"AA");
        upload.append(b"AA");
        let finished = upload.finish();
        assert_eq!(finished.payload_b64, b"AAAA".to_vec());
        assert_eq!(finished.dimensions, Some((1, 1)));
    }

    #[test]
    fn reassembly_png_dims_after_full_decode() {
        // base64 of a minimal PNG header, transmitted whole.
        let mut png = Vec::new();
        png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        png.extend_from_slice(&[0, 0, 0, 13]);
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&3u32.to_be_bytes());
        png.extend_from_slice(&4u32.to_be_bytes());
        let b64 = base64::encode(&png);
        let upload = PendingUpload::begin(control_of(b"a=T,f=100,i=1"), b64.as_bytes());
        let finished = upload.finish();
        assert_eq!(finished.dimensions, Some((3, 4)));
        assert_eq!(finished.control.image_id, Some(1));
    }

    #[test]
    fn feed_transmit_and_display_stores_and_requests_placement() {
        let mut kg = KittyGrid::default();
        let req = kg.feed_chunk(store_cmd(b"a=T,f=32,s=10,v=20,i=3;AAAA"));
        assert_eq!(
            req,
            Some(PlacementRequest { image_id: 3, placement_id: None })
        );
        assert_eq!(kg.image_count(), 1);
        assert_eq!(kg.image_dimensions(3), Some((10, 20)));
        assert!(!kg.has_active_upload());
    }

    #[test]
    fn feed_transmit_only_stores_without_placement() {
        let mut kg = KittyGrid::default();
        let req = kg.feed_chunk(store_cmd(b"a=t,f=32,s=4,v=4,i=9;AAAA"));
        assert_eq!(req, None, "transmit-only must not request a placement");
        assert!(kg.stored_image(9).is_some());
    }

    #[test]
    fn feed_multichunk_reassembles_then_displays() {
        let mut kg = KittyGrid::default();
        // first chunk: control + m=1
        assert_eq!(kg.feed_chunk(store_cmd(b"a=T,f=32,s=1,v=1,i=2,m=1;AA")), None);
        assert!(kg.has_active_upload());
        // final chunk: continuation, m absent → finalize + display
        let req = kg.feed_chunk(store_cmd(b"m=0;AA"));
        assert_eq!(
            req,
            Some(PlacementRequest { image_id: 2, placement_id: None })
        );
        assert_eq!(kg.stored_image(2).map(|i| i.payload_b64.clone()), Some(b"AAAA".to_vec()));
        assert!(!kg.has_active_upload());
    }

    #[test]
    fn place_of_known_image_requests_placement() {
        let mut kg = KittyGrid::default();
        kg.feed_chunk(store_cmd(b"a=t,f=32,s=1,v=1,i=5;AAAA"));
        let req = kg.feed_chunk(store_cmd(b"a=p,i=5,p=7"));
        assert_eq!(
            req,
            Some(PlacementRequest { image_id: 5, placement_id: Some(7) })
        );
    }

    #[test]
    fn place_of_unknown_image_is_noop() {
        let mut kg = KittyGrid::default();
        assert_eq!(kg.feed_chunk(store_cmd(b"a=p,i=404")), None);
    }

    #[test]
    fn anonymous_transmit_allocates_local_id() {
        let mut kg = KittyGrid::default();
        kg.feed_chunk(store_cmd(b"a=t,f=32,s=1,v=1;AAAA")); // no i=
        assert_eq!(kg.image_count(), 1);
    }

    // --- KittyRenderState emission (Phase 3) ---

    fn spec() -> KittyChunkSpec {
        KittyChunkSpec {
            source_image_id: 7,
            placement_id: 1,
            format: 100,
            compressed: false,
            full_width: 20,
            full_height: 10,
            src_x: 0,
            src_y: 0,
            src_width: 20,
            src_height: 10,
            payload_b64: b"AAAA".to_vec(),
        }
    }

    #[test]
    fn render_transmits_once_then_places() {
        let mut rs = KittyRenderState::default();
        let first = rs.render_chunk_bytes(1, &spec());
        let first = String::from_utf8(first).unwrap();
        // First frame: a transmit (a=t) AND a placement (a=p).
        assert!(first.contains("a=t,q=2"), "first frame transmits: {}", first);
        assert!(first.contains("a=p,q=2"), "first frame places: {}", first);
        assert!(first.contains(";AAAA"), "first frame carries payload");

        let second = rs.render_chunk_bytes(1, &spec());
        let second = String::from_utf8(second).unwrap();
        // Second frame: placement only, no re-transmit / no payload.
        assert!(!second.contains("a=t"), "must not re-transmit: {}", second);
        assert!(second.contains("a=p,q=2"), "still re-places: {}", second);
        assert!(!second.contains("AAAA"), "no payload on re-place");
    }

    #[test]
    fn render_per_client_outer_ids_are_distinct_and_each_transmits() {
        let mut rs = KittyRenderState::default();
        let c1 = String::from_utf8(rs.render_chunk_bytes(1, &spec())).unwrap();
        let c2 = String::from_utf8(rs.render_chunk_bytes(2, &spec())).unwrap();
        // Each client gets its own transmit (separate outer terminals).
        assert!(c1.contains("a=t") && c2.contains("a=t"));
    }

    #[test]
    fn render_reset_client_forces_retransmit() {
        let mut rs = KittyRenderState::default();
        rs.render_chunk_bytes(1, &spec());
        rs.reset_client(1);
        let again = String::from_utf8(rs.render_chunk_bytes(1, &spec())).unwrap();
        assert!(again.contains("a=t"), "re-attach must re-transmit: {}", again);
    }

    #[test]
    fn render_raw_format_includes_dims_in_transmit() {
        let mut rs = KittyRenderState::default();
        let mut s = spec();
        s.format = 32; // RGBA raw
        let bytes = String::from_utf8(rs.render_chunk_bytes(1, &s)).unwrap();
        assert!(bytes.contains("s=20,v=10"), "raw transmit carries dims: {}", bytes);
    }

    #[test]
    fn forget_image_returns_live_outer_ids_and_clears() {
        let mut rs = KittyRenderState::default();
        rs.render_chunk_bytes(1, &spec());
        rs.render_chunk_bytes(2, &spec());
        let deleted = rs.forget_image(7);
        assert_eq!(deleted.len(), 2, "both clients' outer ids returned for delete");
        // After forgetting, the next render re-transmits (fresh id).
        let again = String::from_utf8(rs.render_chunk_bytes(1, &spec())).unwrap();
        assert!(again.contains("a=t"));
    }

    #[test]
    fn delete_image_bytes_well_formed() {
        let bytes = String::from_utf8(KittyRenderState::delete_image_bytes(0x90000000)).unwrap();
        assert_eq!(bytes, "\x1b_Ga=d,d=i,q=2,i=2415919104\x1b\\");
    }
}
