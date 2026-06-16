//! Kitty graphics protocol support.
//!
//! Apps inside a zellij pane transmit images using the Kitty graphics protocol
//! (<https://sw.kovidgoyal.net/kitty/graphics-protocol/>), which ships control
//! data and image payloads inside APC sequences: `ESC _ G <keys> ; <payload> ST`.
//! The vendored `vte` fork (`vendor/vte-apc`) surfaces those APC bodies to
//! `Perform::apc_dispatch`; `Grid::apc_dispatch` strips the leading `G` and hands
//! the remainder here.
//!
//! Phase 1a (this file): parse the control keys and classify a command into a
//! [`KittyOutcome`] — a support *query* (`a=q`), a *store* command
//! (transmit/display/place), or *ignore*. Storage, anchoring, and rendering land
//! in later phases. See `GOAL-kitty-graphics.md` and `KITTY_GRAPHICS_PLAN.md`.

use std::collections::HashMap;

/// The classification of a parsed Kitty graphics command, as far as Phase 1a
/// cares. The Grid acts on this without re-parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KittyOutcome {
    /// `a=q` — a support probe / capability query. Answered locally (zellij
    /// *is* the terminal the app talks to) and ordered with surrounding PTY
    /// bytes via the host-query pause/replay machinery. Stores nothing.
    Query(KittyQuery),
    /// `a=t` / `a=T` / `a=p` — transmit / transmit+display / place. Carries the
    /// parsed control keys and the (still base64) payload for later phases.
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

/// A transmit/display/place command. Phase 1a keeps the raw single-char control
/// keys plus the undecoded payload; richer typed fields are added in Phase 1b.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyCommand {
    /// Single-character control keys (`a`, `i`, `f`, `s`, `v`, `m`, …) → value.
    pub keys: HashMap<char, String>,
    /// Bytes after the `;` separator (base64, not yet decoded).
    pub payload: Vec<u8>,
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
    let keys = parse_keys(keys_bytes);

    // Kitty's default action when `a` is absent is transmit (`t`).
    let action = keys.get(&'a').map(String::as_str).unwrap_or("t");

    match action {
        "q" => KittyOutcome::Query(KittyQuery {
            id: keys.get(&'i').and_then(|v| v.parse().ok()),
            image_number: keys.get(&'I').and_then(|v| v.parse().ok()),
            quiet: keys.get(&'q').and_then(|v| v.parse().ok()).unwrap_or(0),
        }),
        "t" | "T" | "p" => KittyOutcome::Store(KittyCommand { keys, payload }),
        // `d` (delete) and unknown actions are handled in later phases.
        _ => KittyOutcome::Ignore,
    }
}

/// Parse a comma-separated list of `k=v` control keys. Keys are single ASCII
/// characters; unknown or malformed entries are skipped (forward-compatible).
fn parse_keys(bytes: &[u8]) -> HashMap<char, String> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn transmit_and_display_is_store() {
        let out = dispatch(b"a=T,f=100,i=1;iVBORw0KGgo=");
        match out {
            KittyOutcome::Store(cmd) => {
                assert_eq!(cmd.keys.get(&'f').map(String::as_str), Some("100"));
                assert_eq!(cmd.payload, b"iVBORw0KGgo=".to_vec());
            },
            other => panic!("expected Store, got {:?}", other),
        }
    }

    #[test]
    fn absent_action_defaults_to_transmit_store() {
        // No `a=` key → default transmit → Store.
        assert!(matches!(dispatch(b"f=24,s=1,v=1;AAAA"), KittyOutcome::Store(_)));
    }

    #[test]
    fn delete_and_unknown_are_ignored_in_phase1() {
        assert_eq!(dispatch(b"a=d,i=1"), KittyOutcome::Ignore);
        assert_eq!(dispatch(b"a=z"), KittyOutcome::Ignore);
    }

    #[test]
    fn malformed_keys_skipped() {
        // `a=q` survives even with junk keys around it.
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
}
