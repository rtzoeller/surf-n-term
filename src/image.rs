use crate::{encoder::base64_encode, Color, Error, Position, Surface, TerminalEvent, RGBA};
use std::{
    collections::HashMap,
    io::{Cursor, Read, Write},
};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ImageHandle(usize);

pub trait ImageStorage {
    /// Regeister image
    ///
    /// Usally means converting it to appropirate format so it could be send
    /// multiple times quicker.
    fn register(&mut self, img: &dyn Surface<Item = RGBA>) -> Result<ImageHandle, Error>;

    /// Draw image
    ///
    /// Send approprite terminal escape sequence so image would be rendered.
    fn draw(&mut self, handle: ImageHandle, out: &mut dyn Write) -> Result<(), Error>;

    /// Erase image at specified position
    ///
    /// This is needed when erasing characters is not actually removing
    /// image from the terminal. For example kitty needs to send separate
    /// escape sequence to actually erase image.
    fn erase(&mut self, pos: Position, out: &mut dyn Write) -> Result<(), Error>;

    /// Handle events frome the terminal
    ///
    /// True means event has been handled and should not be propagated to a user
    fn handle(&mut self, event: &TerminalEvent) -> Result<bool, Error>;
}

/// Image storage for kitty graphic protocol
///
/// Reference: https://sw.kovidgoyal.net/kitty/graphics-protocol.html
pub struct KittyImageStorage {
    imgs: HashMap<ImageHandle, Option<Vec<u8>>>,
}

impl KittyImageStorage {
    pub fn new() -> Self {
        Self {
            imgs: Default::default(),
        }
    }
}

impl Default for KittyImageStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageStorage for KittyImageStorage {
    fn register(&mut self, img: &dyn Surface<Item = RGBA>) -> Result<ImageHandle, Error> {
        let handle = ImageHandle(self.imgs.len() + 1); // id = 0 can not be used.
        let raw: Vec<_> = img.iter().flat_map(|c| RGBAIter::new(*c)).collect();
        let compressed = miniz_oxide::deflate::compress_to_vec_zlib(&raw, /* level */ 10);
        let mut base64 = Cursor::new(Vec::new());
        base64_encode(base64.get_mut(), compressed.iter().copied())?;

        let mut buf = [0u8; 4096];
        let mut data = Vec::new();
        loop {
            let size = base64.read(&mut buf)?;
            let more = if base64.position() < base64.get_ref().len() as u64 {
                1
            } else {
                0
            };
            // a = t - transfer only
            // a = T - transfer and show
            // a = p - present only using `i = id`
            write!(
                &mut data,
                "\x1b_Ga=T,f=32,o=z,v={},s={},m={},i={};",
                img.height(),
                img.width(),
                more,
                handle.0,
            )?;
            data.write_all(&buf[..size])?;
            data.write_all(b"\x1b\\")?;
            if more == 0 {
                break;
            }
        }
        self.imgs.insert(handle.clone(), Some(data));
        Ok(handle)
    }

    fn draw(&mut self, handle: ImageHandle, out: &mut dyn Write) -> Result<(), Error> {
        match self.imgs.get_mut(&handle).and_then(|data| data.take()) {
            Some(data) => {
                // data has not been send yet.
                out.write(&data)?;
            }
            None => {
                // data has already been send and we can just use an id.
                write!(out, "\x1b_Ga=p,i={};\x1b\\", handle.0)?;
            }
        }
        Ok(())
    }

    fn erase(&mut self, pos: Position, out: &mut dyn Write) -> Result<(), Error> {
        write!(
            out,
            "\x1b_Ga=d,d=p,x={},y={}\x1b\\",
            pos.col + 1,
            pos.row + 1
        )?;
        Ok(())
    }

    fn handle(&mut self, event: &TerminalEvent) -> Result<bool, Error> {
        match event {
            TerminalEvent::KittyImage { .. } => Ok(true),
            _ => Ok(false),
        }
    }
}

struct RGBAIter {
    color: [u8; 4],
    index: usize,
}

impl RGBAIter {
    fn new(color: RGBA) -> Self {
        Self {
            color: color.rgba_u8(),
            index: 0,
        }
    }
}

impl Iterator for RGBAIter {
    type Item = u8;

    fn next(&mut self) -> Option<Self::Item> {
        let result = self.color.get(self.index).copied();
        self.index += 1;
        result
    }
}