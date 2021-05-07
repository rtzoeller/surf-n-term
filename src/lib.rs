#![allow(clippy::type_complexity)]
#![allow(clippy::reversed_empty_ranges)]
#![deny(warnings)]

pub mod automata;
pub mod color;
pub mod common;
pub mod decoder;
pub mod encoder;
pub mod error;
pub mod face;
pub mod glyph;
pub mod image;
pub mod keys;
pub mod render;
pub mod surface;
pub mod terminal;
mod unix;
pub mod widgets;

pub use color::{Blend, Color, ColorLinear, RGBA};
pub use error::Error;
pub use face::{Face, FaceAttrs};
pub use glyph::{FillRule, Glyph};
pub use image::{ColorPalette, Image, ImageHandler, KittyImageHandler, SixelImageHandler};
pub use keys::{Key, KeyMap, KeyMod, KeyName};
pub use render::{Cell, TerminalSurface, TerminalSurfaceExt, TerminalWritable, TerminalWriter};
pub use surface::{
    Shape, Surface, SurfaceIter, SurfaceMut, SurfaceMutIter, SurfaceMutView, SurfaceOwned,
    SurfaceOwnedView, SurfaceView,
};
pub use terminal::{
    DecMode, DecModeStatus, Position, Size, Terminal, TerminalAction, TerminalColor,
    TerminalCommand, TerminalEvent, TerminalSize, TerminalWaker,
};

/// System specific terminal
pub type SystemTerminal = unix::UnixTerminal;
