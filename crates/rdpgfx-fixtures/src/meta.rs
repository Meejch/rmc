//! Fixture metadata (scenario.json + one JSON-lines file per surface).
//!
//! Coordinates: `origin`/output rects are session desktop coordinates relative to the
//! top-left corner of the desktop bounding box (the server may use a different origin,
//! e.g. the primary monitor at 0,0; the GPU core only needs consistent relative layout).

use serde::{Deserialize, Serialize};

use rdpgfx_model::{Avc444Layout, ChromaMode, Rect};

use crate::scene::Scene;

pub const FORMAT_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    Avc420,
    Avc444,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Layout {
    /// One surface per monitor (3 decoders).
    Multi,
    /// One surface over the whole desktop bounding box (1 decoder, 7440x2560).
    Single,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Output {
    pub name: String,
    pub rect: Rect,
    pub primary: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Surface {
    pub id: u16,
    pub width: u32,
    pub height: u32,
    /// Position of the surface on the desktop (MapSurfaceToOutput).
    pub origin: (u32, u32),
    pub stream: String,
    pub updates: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Encoder {
    pub tool: String,
    pub preset: String,
    pub crf: u32,
    pub params: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Scenario {
    pub format_version: u32,
    pub name: String,
    pub layout: Layout,
    pub codec: Codec,
    pub avc444_layout: Avc444Layout,
    pub chroma: ChromaMode,
    pub fps: u32,
    pub frames: u32,
    pub scene: Scene,
    pub outputs: Vec<Output>,
    pub surfaces: Vec<Surface>,
    pub encoder: Encoder,
}

/// One coded picture inside the surface's stream.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Picture {
    pub offset: usize,
    pub len: usize,
    /// Region rectangles (surface coordinates) this picture updates.
    pub rects: Vec<Rect>,
}

/// LC field of RFX_AVC444_BITMAP_STREAM.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lc {
    Both,
    Luma,
    Chroma,
}

/// One surface command (one line in `surface-N.jsonl`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Update {
    pub frame: u32,
    pub codec: Codec,
    /// Destination rectangle on the surface.
    pub dest: Rect,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lc: Option<Lc>,
    pub main: Picture,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aux: Option<Picture>,
}

/// The real desktop of the target setup (FreeRDP monitor IDs 4, 3, 5).
pub fn desktop_outputs() -> (u32, u32, Vec<Output>) {
    let outputs = vec![
        Output {
            name: "left-portrait".into(),
            rect: Rect::from_xywh(0, 0, 1440, 2560),
            primary: false,
        },
        Output {
            name: "center".into(),
            rect: Rect::from_xywh(1440, 546, 3440, 1440),
            primary: true,
        },
        Output {
            name: "right".into(),
            rect: Rect::from_xywh(4880, 546, 2560, 1440),
            primary: false,
        },
    ];
    (7440, 2560, outputs)
}
