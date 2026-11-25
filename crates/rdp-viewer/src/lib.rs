//! Session views on Wayland for the RDP client and the replay tool: windows per output,
//! windowed crop, connection bar (overlay), keyboard grab, cursor shapes, and the local
//! monitor layout sent to the server.

pub mod grab;
pub mod layout;
pub mod text;
pub mod viewer;
pub mod wl_outputs;

pub use viewer::{
    Action, CursorShape, CursorUpdate, Input, Mode, SessionOutput, Viewer, ViewerOptions,
};
