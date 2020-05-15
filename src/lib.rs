#![allow(clippy::type_complexity)]

pub mod automata;
mod common;
pub mod decoder;
pub mod encoder;
pub mod error;
pub mod render;
mod style;
mod surface;
pub mod terminal;
mod unix;

pub use render::{Cell, TerminalView};
pub use style::{Color, Face, FaceAttrs};
pub use surface::{Shape, Surface, View, ViewExt, ViewMut, ViewMutExt};
pub use terminal::{
    DecMode, DecModeStatus, Key, KeyMod, KeyName, Position, Terminal, TerminalAction,
    TerminalCommand, TerminalEvent,
};

pub type SystemTerminal = unix::UnixTerminal;
