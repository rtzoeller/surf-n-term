//! Handling everything to do with images.
//!
//! Provides:
//!  - Protocol handling
//!  - Image object
//!  - Quantization and dithering
use crate::{
    common::{clamp, Rnd},
    encoder::Base64Encoder,
    Blend, Color, Error, Position, Shape, Size, Surface, SurfaceMut, SurfaceOwned, TerminalEvent,
    TerminalSize, RGBA,
};
use flate2::{write::ZlibEncoder, Compression};
use std::{
    borrow::Cow,
    cmp::Ordering,
    collections::{hash_map::Entry, HashMap, HashSet},
    fmt,
    io::Write,
    iter::FromIterator,
    ops::{Add, AddAssign, Mul},
    str::FromStr,
    sync::Arc,
};

const IMAGE_CACHE_SIZE: usize = 134217728; // 128MB

/// Arc wrapped RGBA surface with precomputed hash
#[derive(Clone)]
pub struct Image {
    surf: Arc<dyn Surface<Item = RGBA> + Send + Sync>,
    hash: u64,
}

impl Image {
    /// Create new image from the RGBA surface
    pub fn new(surf: impl Surface<Item = RGBA> + Send + Sync + 'static) -> Self {
        Self {
            hash: surf.hash(),
            surf: Arc::new(surf),
        }
    }

    /// Image size in bytes
    pub fn size(&self) -> usize {
        self.surf.height() * self.surf.width() * 4
    }

    /// Size in cells
    pub fn size_cells(&self, term_size: TerminalSize) -> Size {
        let cell_size = term_size.cell_size();
        if cell_size.width == 0 || cell_size.height == 0 {
            return Size::new(0, 0);
        }
        let height = self.height() / cell_size.height;
        let width = self.width() / cell_size.width;
        Size { height, width }
    }

    /// Quantize image
    ///
    /// Perform palette extraction and Floyd–Steinberg dithering.
    #[tracing::instrument(level = "debug")]
    pub fn quantize(
        &self,
        palette_size: usize,
        dither: bool,
        bg: Option<RGBA>,
    ) -> Option<(ColorPalette, SurfaceOwned<usize>)> {
        let bg = bg.unwrap_or_else(|| RGBA::new(0, 0, 0, 255));
        let palette = ColorPalette::from_image(self, palette_size, bg)?;
        let mut qimg = SurfaceOwned::new(self.height(), self.width());

        // quantize and dither
        let mut errors: Vec<ColorError> = Vec::new();
        let ewidth = self.width() + 2; // to avoid check for the first and the last pixels
        if dither {
            errors.resize_with(ewidth * 2, ColorError::new);
        }
        for row in 0..self.height() {
            if dither {
                // swap error rows
                for col in 0..ewidth {
                    errors[col] = errors[col + ewidth];
                    errors[col + ewidth] = ColorError::new();
                }
            }
            // quantize and spread the error
            for col in 0..self.width() {
                let mut color = *self.get(row, col)?;
                if color.rgba_u8()[3] < 255 {
                    color = bg.blend(color, Blend::Over);
                }
                if dither {
                    color = errors[col + 1].add(color); // account for error
                }
                let (qindex, qcolor) = palette.find(color);
                qimg.set(row, col, qindex);
                if dither {
                    // spread the error according to Floyd–Steinberg dithering matrix:
                    // [[0   , X   , 7/16],
                    // [3/16, 5/16, 1/16]]
                    let error = ColorError::between(color, qcolor);
                    errors[col + 2] += error * 0.4375; // 7/16
                    errors[col + ewidth] += error * 0.1875; // 3/16
                    errors[col + ewidth + 1] += error * 0.3125; // 5/16
                    errors[col + ewidth + 2] += error * 0.0625; // 1/16
                }
            }
        }
        Some((palette, qimg))
    }

    /// Write image as PNG
    pub fn write_png(&self, w: impl Write) -> Result<(), png::EncodingError> {
        let mut encoder = png::Encoder::new(w, self.width() as u32, self.height() as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        let mut stream_writer = writer.stream_writer()?;
        for color in self.iter() {
            stream_writer.write_all(&color.rgba_u8())?;
        }
        stream_writer.flush()?;
        Ok(())
    }
}

impl PartialEq for Image {
    fn eq(&self, other: &Self) -> bool {
        self.hash == other.hash
    }
}

impl Eq for Image {}

impl PartialOrd for Image {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.hash.partial_cmp(&other.hash)
    }
}

impl Ord for Image {
    fn cmp(&self, other: &Self) -> Ordering {
        self.hash.cmp(&other.hash)
    }
}

impl std::hash::Hash for Image {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write_u64(self.hash)
    }
}

impl fmt::Debug for Image {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Image({})", self.hash)
    }
}

impl Surface for Image {
    type Item = RGBA;

    fn shape(&self) -> Shape {
        self.surf.shape()
    }

    fn hash(&self) -> u64 {
        self.hash
    }

    fn data(&self) -> &[Self::Item] {
        self.surf.data()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageHandlerKind {
    Kitty,
    Sixel,
    ITerm,
    Dummy,
}

impl ImageHandlerKind {
    pub(crate) fn into_image_handler(self, bg: Option<RGBA>) -> Box<dyn ImageHandler> {
        use ImageHandlerKind::*;
        match self {
            Kitty => Box::new(KittyImageHandler::new()),
            Sixel => Box::new(SixelImageHandler::new(bg)),
            ITerm => Box::new(ItermImageHandler::new()),
            Dummy => Box::new(DummyImageHandler),
        }
    }
}

impl FromStr for ImageHandlerKind {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        use ImageHandlerKind::*;
        match s.to_ascii_lowercase().as_str() {
            "kitty" => Ok(Kitty),
            "sixel" => Ok(Sixel),
            "iterm" => Ok(ITerm),
            "dummy" => Ok(Dummy),
            _ => Err(Error::ParseError(
                "ImageHandlerKind",
                format!("invalid image handler type: {}", s),
            )),
        }
    }
}

/// Image rendering/handling interface
pub trait ImageHandler: Send + Sync {
    /// Name
    fn kind(&self) -> ImageHandlerKind;

    /// Draw image
    ///
    /// Send an appropriate terminal escape sequence so the image would be rendered.
    fn draw(&mut self, out: &mut dyn Write, img: &Image, pos: Position) -> Result<(), Error>;

    /// Erase image at specified position
    ///
    /// This is needed when erasing characters is not actually removing
    /// image from the terminal. For example kitty needs to send separate
    /// escape sequence to actually erase image. If position is not specified
    /// all matching images are deleted.
    fn erase(
        &mut self,
        out: &mut dyn Write,
        img: &Image,
        pos: Option<Position>,
    ) -> Result<(), Error>;

    /// Handle events from the terminal
    ///
    /// True means event has been handled and should not be propagated to a user
    fn handle(&mut self, event: &TerminalEvent) -> Result<bool, Error>;
}

impl<'a> ImageHandler for Box<dyn ImageHandler> {
    fn kind(&self) -> ImageHandlerKind {
        (**self).kind()
    }

    fn draw(&mut self, out: &mut dyn Write, img: &Image, pos: Position) -> Result<(), Error> {
        (**self).draw(out, img, pos)
    }

    fn erase(
        &mut self,
        out: &mut dyn Write,
        img: &Image,
        pos: Option<Position>,
    ) -> Result<(), Error> {
        (**self).erase(out, img, pos)
    }

    fn handle(&mut self, event: &TerminalEvent) -> Result<bool, Error> {
        (**self).handle(event)
    }
}

/// Image handler which ignores requests
pub struct DummyImageHandler;

impl ImageHandler for DummyImageHandler {
    fn kind(&self) -> ImageHandlerKind {
        ImageHandlerKind::Dummy
    }

    fn draw(&mut self, _out: &mut dyn Write, _img: &Image, _pos: Position) -> Result<(), Error> {
        Ok(())
    }

    fn erase(
        &mut self,
        _out: &mut dyn Write,
        _img: &Image,
        _pos: Option<Position>,
    ) -> Result<(), Error> {
        Ok(())
    }

    fn handle(&mut self, _event: &TerminalEvent) -> Result<bool, Error> {
        Ok(false)
    }
}

/// Image handler for iTerm2 graphic protocol
///
/// Reference: [iTerm2 Image Protocol](https://iterm2.com/documentation-images.html)
pub struct ItermImageHandler {
    imgs: lru::LruCache<u64, Vec<u8>>,
    size: usize,
}

impl ItermImageHandler {
    pub fn new() -> Self {
        Self {
            imgs: lru::LruCache::unbounded(),
            size: 0,
        }
    }
}

impl Default for ItermImageHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageHandler for ItermImageHandler {
    fn kind(&self) -> ImageHandlerKind {
        ImageHandlerKind::ITerm
    }

    fn draw(&mut self, out: &mut dyn Write, img: &Image, _pos: Position) -> Result<(), Error> {
        if let Some(data) = self.imgs.get(&img.hash()) {
            out.write_all(data.as_slice())?;
            return Ok(());
        }

        let mut data = Vec::new();
        write!(data, "\x1b]1337;File=inline=1;width={}px:", img.width())?;
        let mut base64 = Base64Encoder::new(&mut data);
        img.write_png(&mut base64).map_err(|err| match err {
            png::EncodingError::IoError(err) => err.into(),
            err => Error::Other(Cow::from(err.to_string())),
        })?;
        base64.finish()?;
        data.write_all(b"\x07")?;

        out.write_all(data.as_slice())?;

        self.size += data.len();
        self.imgs.put(img.hash(), data);
        if self.size > IMAGE_CACHE_SIZE {
            if let Some((_, lru_image)) = self.imgs.pop_lru() {
                self.size -= lru_image.len();
            }
        }

        Ok(())
    }

    fn erase(
        &mut self,
        _out: &mut dyn Write,
        _img: &Image,
        _pos: Option<Position>,
    ) -> Result<(), Error> {
        Ok(())
    }

    fn handle(&mut self, _event: &TerminalEvent) -> Result<bool, Error> {
        Ok(false)
    }
}

/// Image handler for kitty graphic protocol
///
/// Reference: [Kitty Graphic Protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/)
pub struct KittyImageHandler {
    imgs: HashMap<u64, usize>, // hash -> size in bytes
}

impl KittyImageHandler {
    pub fn new() -> Self {
        Self {
            imgs: Default::default(),
        }
    }
}

impl Default for KittyImageHandler {
    fn default() -> Self {
        Self::new()
    }
}

/// Kitty id/placement_id must not exceed this value
const KITTY_MAX_ID: u64 = 4294967295;
/// We are using position to derive placement_id, and this is the limit
/// on terminal dimension (width and height).
const KITTY_MAX_DIM: u64 = 65536;

/// Identification for image data
fn kitty_image_id(img: &Image) -> u64 {
    img.hash() % KITTY_MAX_ID
}

/// Identification of particular placement of the image
///
/// In general this identification is just represents individual placement
/// but in particular implementation it is bound to a physical position on
/// the screen.
fn kitty_placement_id(pos: Position) -> u64 {
    (pos.row as u64 % KITTY_MAX_DIM) + (pos.col as u64 % KITTY_MAX_DIM) * KITTY_MAX_DIM
}

impl ImageHandler for KittyImageHandler {
    fn kind(&self) -> ImageHandlerKind {
        ImageHandlerKind::Kitty
    }

    fn draw(&mut self, out: &mut dyn Write, img: &Image, pos: Position) -> Result<(), Error> {
        tracing::trace!(image_handler = "kitty", ?pos, ?img, "draw image");
        let img_id = kitty_image_id(img);

        // transfer image if it has not been transferred yet
        if let Entry::Vacant(entry) = self.imgs.entry(img_id) {
            let _ =
                tracing::debug_span!("transfer image", image_handler = "kitty", ?pos, ?img).enter();
            // zlib compressed and base64 encoded RGBA image data
            let mut payload_write =
                ZlibEncoder::new(Base64Encoder::new(Vec::new()), Compression::default());
            for color in img.iter() {
                payload_write.write_all(&color.rgba_u8())?;
            }
            let payload = payload_write.finish()?.finish()?;

            // NOTE:
            //  - data needs to be transferred in chunks
            //  - chunks should be multiple of 4, otherwise kitty complains that it is not
            //    valid base64 encoded data.
            let chunks = payload.chunks(4096);
            let count = chunks.len();
            for (index, chunk) in chunks.enumerate() {
                // control data
                let more = if index + 1 < count { 1 } else { 0 };
                if index == 0 {
                    // a=t  - action is transmit only
                    // f=32 - RGBA pixel format
                    // o=z  - zlib compressed data
                    // i    - image data identifier
                    // v    - height of the image
                    // s    - width of the image
                    // m    - whether more chunks will follow or not
                    write!(
                        out,
                        "\x1b_Ga=t,f=32,o=z,i={},v={},s={},m={};",
                        img_id,
                        img.height(),
                        img.width(),
                        more
                    )?;
                } else {
                    // only first chunk requires all attributes
                    write!(out, "\x1b_Gm={};", more)?;
                }
                // data
                out.write_all(chunk)?;
                // epilogue
                out.write_all(b"\x1b\\")?;
            }

            // remember that image data has been send
            entry.insert(img.size());
        }

        // request image to be shown
        let placement_id = kitty_placement_id(pos);
        // a=p - action is put image
        // i   - image data identifier
        // p   - placement identifier
        write!(out, "\x1b_Ga=p,i={},p={};\x1b\\", img_id, placement_id)?;
        Ok(())
    }

    fn erase(
        &mut self,
        out: &mut dyn Write,
        img: &Image,
        pos: Option<Position>,
    ) -> Result<(), Error> {
        tracing::trace!(image_handler = "kitty", ?pos, ?img, "erase image");
        // Delete image by image id and placement id
        // a=d - action delete image
        // d=i - delete by image and placement id without freeing data
        // i   - image data identifier
        // p   - placement identifier
        match pos {
            Some(pos) => write!(
                out,
                "\x1b_Ga=d,d=i,i={},p={}\x1b\\",
                kitty_image_id(img),
                kitty_placement_id(pos),
            )?,
            None => write!(out, "\x1b_Ga=d,d=i,i={}\x1b\\", kitty_image_id(img))?,
        }
        Ok(())
    }

    fn handle(&mut self, event: &TerminalEvent) -> Result<bool, Error> {
        match event {
            TerminalEvent::KittyImage { id, error } => {
                let filter = if !error.is_none() {
                    tracing::warn!("kitty image error: {:?}", error);
                    // remove element from cache, and propagate event to
                    // the user which will cause the redraw
                    self.imgs.remove(id);
                    false
                } else {
                    true
                };
                Ok(filter)
            }
            _ => Ok(false),
        }
    }
}

/// Image handler for sixel graphic protocol
///
/// Reference: [Sixel](https://en.wikipedia.org/wiki/Sixel)
pub struct SixelImageHandler {
    imgs: lru::LruCache<u64, Vec<u8>>,
    size: usize,
    bg: Option<RGBA>,
}

impl SixelImageHandler {
    pub fn new(bg: Option<RGBA>) -> Self {
        SixelImageHandler {
            imgs: lru::LruCache::unbounded(),
            size: 0,
            bg,
        }
    }
}

impl ImageHandler for SixelImageHandler {
    fn kind(&self) -> ImageHandlerKind {
        ImageHandlerKind::Sixel
    }

    fn draw(&mut self, out: &mut dyn Write, img: &Image, pos: Position) -> Result<(), Error> {
        tracing::debug!(image_handler = "sixel", ?pos, ?img, "draw image");
        if let Some(sixel_image) = self.imgs.get(&img.hash()) {
            out.write_all(sixel_image.as_slice())?;
            return Ok(());
        }
        let _ = tracing::debug_span!("encode image", image_handler = "sixel");
        // sixel color chanel has a range [0,100] colors, we need to reduce it before
        // quantization, it will produce smaller or/and better palette for this color depth
        let dimg = Image::new(img.map(|_, _, color| {
            let [red, green, blue, alpha] = color.rgba_u8();
            let red = ((red as f32 / 2.55).round() * 2.55) as u8;
            let green = ((green as f32 / 2.55).round() * 2.55) as u8;
            let blue = ((blue as f32 / 2.55).round() * 2.55) as u8;
            RGBA::new(red, green, blue, alpha)
        }));
        let (palette, qimg) = match dimg.quantize(256, true, self.bg) {
            None => return Ok(()),
            Some(qimg) => qimg,
        };

        let mut sixel_image = Vec::new();
        // header
        sixel_image.write_all(b"\x1bPq")?;
        write!(sixel_image, "\"1;1;{};{}", qimg.width(), qimg.height())?;
        // palette
        for (index, color) in palette.colors().iter().enumerate() {
            let [red, green, blue] = color.rgb_u8();
            let red = (red as f32 / 2.55).round() as u8;
            let green = (green as f32 / 2.55).round() as u8;
            let blue = (blue as f32 / 2.55).round() as u8;
            write!(sixel_image, "#{};2;{};{};{}", index, red, green, blue)?;
        }
        // color_index -> [(offset, sixel_code)]
        let mut sixel_lines: HashMap<usize, Vec<(usize, u8)>> = HashMap::new();
        let mut colors: HashSet<usize> = HashSet::with_capacity(6);
        for row in (0..qimg.height()).step_by(6) {
            sixel_lines.clear();
            // extract sixel line
            for col in 0..img.width() {
                // extract sixel
                let mut sixel = [0usize; 6];
                for (i, s) in sixel.iter_mut().enumerate() {
                    if let Some(index) = qimg.get(row + i, col) {
                        *s = *index;
                    }
                }
                // construct sixel
                colors.clear();
                colors.extend(sixel.iter().copied());
                for color in colors.iter() {
                    let mut code = 0;
                    for (s_index, s_color) in sixel.iter().enumerate() {
                        if s_color == color {
                            code |= 1 << s_index;
                        }
                    }
                    sixel_lines
                        .entry(*color)
                        .or_insert_with(Vec::new)
                        .push((col, code + 63));
                }
            }
            // render sixel line
            for (color, sixel_line) in sixel_lines.iter() {
                write!(sixel_image, "#{}", color)?;

                let mut offset = 0;
                let mut codes = sixel_line.iter().peekable();
                while let Some((column, code)) = codes.next() {
                    // find shift needed to get to the correct offset
                    let shift = column - offset;
                    if shift > 0 {
                        if shift > 3 {
                            write!(sixel_image, "!{}?", shift)?;
                        } else {
                            for _ in 0..shift {
                                sixel_image.write_all(b"?")?;
                            }
                        }
                    }
                    // find repeated sixels
                    let mut repeats = 1;
                    while let Some((column_next, code_next)) = codes.peek() {
                        if *column_next != column + repeats || code_next != code {
                            break;
                        }
                        repeats += 1;
                        codes.next();
                    }
                    // write sixel
                    if repeats > 3 {
                        write!(sixel_image, "!{}", repeats)?;
                        sixel_image.write_all(&[*code])?;
                    } else {
                        for _ in 0..repeats {
                            sixel_image.write_all(&[*code])?;
                        }
                    }
                    offset = column + repeats;
                }
                sixel_image.write_all(b"$")?;
            }
            sixel_image.write_all(b"-")?;
        }
        // EOF sixel
        sixel_image.write_all(b"\x1b\\")?;

        out.write_all(sixel_image.as_slice())?;

        self.size += sixel_image.len();
        self.imgs.put(img.hash(), sixel_image);
        if self.size > IMAGE_CACHE_SIZE {
            if let Some((_, lru_image)) = self.imgs.pop_lru() {
                self.size -= lru_image.len();
            }
        }

        Ok(())
    }

    fn erase(
        &mut self,
        _out: &mut dyn Write,
        _img: &Image,
        _pos: Option<Position>,
    ) -> Result<(), Error> {
        Ok(())
    }

    fn handle(&mut self, _event: &TerminalEvent) -> Result<bool, Error> {
        Ok(false)
    }
}

/// Color like object to track quantization
///
/// Used in Floyd–Steinberg dithering.
#[derive(Clone, Copy)]
struct ColorError([f32; 3]);

impl ColorError {
    fn new() -> Self {
        Self([0.0; 3])
    }

    /// Error between two colors
    fn between(c0: RGBA, c1: RGBA) -> Self {
        let [r0, g0, b0] = c0.rgb_u8();
        let [r1, g1, b1] = c1.rgb_u8();
        Self([
            r0 as f32 - r1 as f32,
            g0 as f32 - g1 as f32,
            b0 as f32 - b1 as f32,
        ])
    }

    /// Add error to the color
    fn add(self, color: RGBA) -> RGBA {
        let [r, g, b] = color.rgb_u8();
        let Self([re, ge, be]) = self;
        RGBA::new(
            clamp(r as f32 + re, 0.0, 255.0) as u8,
            clamp(g as f32 + ge, 0.0, 255.0) as u8,
            clamp(b as f32 + be, 0.0, 255.0) as u8,
            255,
        )
    }
}

impl Add<Self> for ColorError {
    type Output = Self;

    #[inline]
    fn add(self, other: Self) -> Self::Output {
        let Self([r0, g0, b0]) = self;
        let Self([r1, g1, b1]) = other;
        Self([r0 + r1, g0 + g1, b0 + b1])
    }
}

impl AddAssign for ColorError {
    fn add_assign(&mut self, rhs: Self) {
        *self = *self + rhs
    }
}

impl Mul<f32> for ColorError {
    type Output = Self;

    #[inline]
    fn mul(self, val: f32) -> Self::Output {
        let Self([r, g, b]) = self;
        Self([r * val, g * val, b * val])
    }
}

#[derive(Debug, Clone, Copy)]
struct OcTreeLeaf {
    red_acc: usize,
    green_acc: usize,
    blue_acc: usize,
    color_count: usize,
    index: usize,
}

impl OcTreeLeaf {
    fn new() -> Self {
        Self {
            red_acc: 0,
            green_acc: 0,
            blue_acc: 0,
            color_count: 0,
            index: 0,
        }
    }

    fn from_rgba(rgba: RGBA) -> Self {
        let [r, g, b] = rgba.rgb_u8();
        Self {
            red_acc: r as usize,
            green_acc: g as usize,
            blue_acc: b as usize,
            color_count: 1,
            index: 0,
        }
    }

    fn to_rgba(self) -> RGBA {
        let r = (self.red_acc / self.color_count) as u8;
        let g = (self.green_acc / self.color_count) as u8;
        let b = (self.blue_acc / self.color_count) as u8;
        RGBA::new(r, g, b, 255)
    }
}

impl AddAssign<RGBA> for OcTreeLeaf {
    fn add_assign(&mut self, rhs: RGBA) {
        let [r, g, b] = rhs.rgb_u8();
        self.red_acc += r as usize;
        self.green_acc += g as usize;
        self.blue_acc += b as usize;
        self.color_count += 1;
    }
}

impl AddAssign<OcTreeLeaf> for OcTreeLeaf {
    fn add_assign(&mut self, rhs: Self) {
        self.red_acc += rhs.red_acc;
        self.green_acc += rhs.green_acc;
        self.blue_acc += rhs.blue_acc;
        self.color_count += rhs.color_count;
    }
}

#[derive(Debug, Clone)]
enum OcTreeNode {
    Leaf(OcTreeLeaf),
    Tree(Box<OcTree>),
    Empty,
}

impl OcTreeNode {
    pub fn is_empty(&self) -> bool {
        matches!(self, OcTreeNode::Empty)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct OcTreeInfo {
    // total number of leafs in the subtree
    pub leaf_count: usize,
    // total number of colors in the subtree
    pub color_count: usize,
    // node (Tree|Leaf) with smallest number of colors in the subtree
    pub min_color_count: Option<usize>,
}

impl OcTreeInfo {
    // Monoidal unit
    pub fn empty() -> Self {
        Self {
            leaf_count: 0,
            color_count: 0,
            min_color_count: None,
        }
    }

    // Monoidal sum
    pub fn join(self, other: Self) -> Self {
        let leaf_count = self.leaf_count + other.leaf_count;
        let color_count = self.color_count + other.color_count;
        let min_color_count = match (self.min_color_count, other.min_color_count) {
            (Some(c0), Some(c1)) => Some(std::cmp::min(c0, c1)),
            (None, Some(c1)) => Some(c1),
            (Some(c0), None) => Some(c0),
            (None, None) => None,
        };
        Self {
            leaf_count,
            color_count,
            min_color_count,
        }
    }

    // Monoidal sum over oll infos of nodes in the slice
    fn from_slice(slice: &[OcTreeNode]) -> Self {
        slice
            .iter()
            .fold(Self::empty(), |acc, n| acc.join(n.info()))
    }
}

impl OcTreeNode {
    // Take node content and replace it with empty node
    fn take(&mut self) -> Self {
        std::mem::replace(self, Self::Empty)
    }

    // Get info associated with the node
    fn info(&self) -> OcTreeInfo {
        use OcTreeNode::*;
        match self {
            Empty => OcTreeInfo::empty(),
            Leaf(leaf) => OcTreeInfo {
                leaf_count: 1,
                color_count: leaf.color_count,
                min_color_count: Some(leaf.color_count),
            },
            Tree(tree) => tree.info,
        }
    }
}

/// Oc(tet)Tree used for color quantization
///
/// References:
/// - [OcTree color quantization](https://www.cubic.org/docs/octree.htm)
/// - [Color quantization](http://www.leptonica.org/color-quantization.html)
#[derive(Debug, Clone)]
pub struct OcTree {
    info: OcTreeInfo,
    removed: OcTreeLeaf,
    children: [OcTreeNode; 8],
}

impl Default for OcTree {
    fn default() -> Self {
        Self::new()
    }
}

impl Extend<RGBA> for OcTree {
    fn extend<T: IntoIterator<Item = RGBA>>(&mut self, colors: T) {
        for color in colors {
            self.insert(color)
        }
    }
}

impl FromIterator<RGBA> for OcTree {
    fn from_iter<T: IntoIterator<Item = RGBA>>(iter: T) -> Self {
        let mut octree = OcTree::new();
        octree.extend(iter);
        octree
    }
}

impl OcTree {
    /// Create empty OcTree
    pub fn new() -> Self {
        use OcTreeNode::Empty;
        Self {
            info: OcTreeInfo::empty(),
            removed: OcTreeLeaf::new(),
            children: [Empty, Empty, Empty, Empty, Empty, Empty, Empty, Empty],
        }
    }

    /// Get info associated with the node
    #[cfg(test)]
    fn info(&self) -> OcTreeInfo {
        self.info
    }

    /// Find nearest color inside the octree
    ///
    /// NOTE:
    ///  - to get correct palette index call build_palette first.
    ///  - prefer `ColorPalette::find` as it produces better result, and can not return None.
    pub fn find(&self, color: RGBA) -> Option<(usize, RGBA)> {
        use OcTreeNode::*;
        let mut tree = self;
        for index in OcTreePath::new(color) {
            match &tree.children[index] {
                Empty => break,
                Leaf(leaf) => return Some((leaf.index, leaf.to_rgba())),
                Tree(next_tree) => tree = next_tree,
            }
        }
        None
    }

    /// Extract all colors present in the octree and update leaf color indices
    pub fn build_palette(&mut self) -> Vec<RGBA> {
        fn palette_rec(node: &mut OcTreeNode, palette: &mut Vec<RGBA>) {
            use OcTreeNode::*;
            match node {
                Empty => {}
                Leaf(ref mut leaf) => {
                    leaf.index = palette.len();
                    palette.push(leaf.to_rgba());
                }
                Tree(tree) => {
                    for child in tree.children.iter_mut() {
                        palette_rec(child, palette)
                    }
                }
            }
        }

        let mut palette = Vec::new();
        for child in self.children.iter_mut() {
            palette_rec(child, &mut palette);
        }
        palette
    }

    /// Update node with provided function.
    fn node_update(&mut self, index: usize, func: impl FnOnce(OcTreeNode) -> OcTreeNode) {
        self.children[index] = func(self.children[index].take());
        self.info = OcTreeInfo::from_slice(&self.children);
    }

    /// Insert color into the octree
    pub fn insert(&mut self, color: RGBA) {
        // Recursive insertion of the color into a node
        fn insert_rec(node: OcTreeNode, mut path: OcTreePath) -> OcTreeNode {
            use OcTreeNode::*;
            match path.next() {
                Some(index) => match node {
                    Empty => {
                        let mut tree = OcTree::new();
                        tree.node_update(index, move |node| insert_rec(node, path));
                        Tree(Box::new(tree))
                    }
                    Leaf(mut leaf) => {
                        leaf += path.rgba();
                        Leaf(leaf)
                    }
                    Tree(mut tree) => {
                        tree.node_update(index, move |node| insert_rec(node, path));
                        Tree(tree)
                    }
                },
                None => match node {
                    Empty => Leaf(OcTreeLeaf::from_rgba(path.rgba())),
                    Leaf(mut leaf) => {
                        leaf += path.rgba();
                        Leaf(leaf)
                    }
                    Tree(_) => unreachable!(),
                },
            }
        }

        let mut path = OcTreePath::new(color);
        let index = path.next().expect("OcTreePath can not be empty");
        self.node_update(index, |node| insert_rec(node, path));
    }

    /// Prune until desired number of colors is left
    pub fn prune_until(&mut self, color_count: usize) {
        let prune_count = color_count.max(8);
        while self.info.leaf_count > prune_count {
            self.prune();
        }
    }

    /// Remove the node with minimal number of colors in the it
    pub fn prune(&mut self) {
        use OcTreeNode::*;

        // find child index with minimal color count in the child subtree
        fn argmin_color_count(tree: &OcTree) -> Option<usize> {
            tree.children
                .iter()
                .enumerate()
                .filter_map(|(index, node)| Some((index, node.info().min_color_count?)))
                .min_by_key(|(_, min_tail_tree)| *min_tail_tree)
                .map(|(index, _)| index)
        }

        // recursive prune helper
        fn prune_rec(mut tree: Box<OcTree>) -> OcTreeNode {
            match argmin_color_count(&tree) {
                None => Leaf(tree.removed),
                Some(index) => match tree.children[index].take() {
                    Empty => unreachable!("agrmin_color_count found and empty node"),
                    Leaf(leaf) => {
                        tree.removed += leaf;
                        if tree.children.iter().all(OcTreeNode::is_empty) {
                            Leaf(tree.removed)
                        } else {
                            Tree(tree)
                        }
                    }
                    Tree(child_tree) => {
                        let child = prune_rec(child_tree);
                        match child {
                            Leaf(leaf) if tree.children.iter().all(OcTreeNode::is_empty) => {
                                tree.removed += leaf;
                                Leaf(tree.removed)
                            }
                            _ => {
                                tree.node_update(index, |_| child);
                                Tree(tree)
                            }
                        }
                    }
                },
            }
        }

        if let Some(index) = argmin_color_count(self) {
            match self.children[index].take() {
                Empty => unreachable!("agrmin_color_count found and empty node"),
                Leaf(leaf) => self.removed += leaf,
                Tree(child_tree) => {
                    let child = prune_rec(child_tree);
                    self.node_update(index, |_| child);
                }
            }
        }
    }

    /// Render octree as graphviz digraph (for debugging)
    pub fn to_digraph<W: Write>(&self, mut out: W) -> std::io::Result<()> {
        pub fn to_digraph_rec<W: Write>(
            tree: &OcTree,
            parent: usize,
            next: &mut usize,
            out: &mut W,
        ) -> std::io::Result<()> {
            use OcTreeNode::*;
            for child in tree.children.iter() {
                match child {
                    Empty => continue,
                    Leaf(leaf) => {
                        let id = *next;
                        *next += 1;

                        let fg = leaf
                            .to_rgba()
                            .best_contrast(RGBA::new(255, 255, 255, 255), RGBA::new(0, 0, 0, 255));
                        writeln!(
                            out,
                            "  {} [style=filled, fontcolor=\"{}\" fillcolor=\"{}\", label=\"{}\"]",
                            id,
                            fg,
                            leaf.to_rgba(),
                            leaf.color_count
                        )?;
                        writeln!(out, "  {} -> {}", parent, id)?;
                    }
                    Tree(child) => {
                        let id = *next;
                        *next += 1;

                        writeln!(
                            out,
                            "  {} [label=\"{} {}\"]",
                            id,
                            child.info.leaf_count,
                            child.info.min_color_count.unwrap_or(0),
                        )?;
                        writeln!(out, "  {} -> {}", parent, id)?;
                        to_digraph_rec(child, id, next, out)?
                    }
                }
            }
            Ok(())
        }

        let mut next = 1;
        writeln!(out, "digraph OcTree {{")?;
        writeln!(out, "  rankdir=\"LR\"")?;
        writeln!(
            out,
            "  0 [label=\"{} {}\"]",
            self.info.leaf_count,
            self.info.min_color_count.unwrap_or(0),
        )?;
        to_digraph_rec(self, 0, &mut next, &mut out)?;
        writeln!(out, "}}")?;
        Ok(())
    }
}

/// Iterator which goes over all most significant bits of the color
/// concatenated together.
///
/// Example:
/// For RGB (90, 13, 157) in binary form
/// R 0 1 0 1 1 0 1 0
/// G 0 1 1 1 0 0 0 1
/// B 1 0 0 1 1 1 0 1
/// Output will be [0b001, 0b110, 0b010, 0b111, 0b101, 0b001, 0b100, 0b011]
struct OcTreePath {
    rgba: RGBA,
    state: u32,
    length: u8,
}

impl OcTreePath {
    pub fn new(rgba: RGBA) -> Self {
        let [r, g, b] = rgba.rgb_u8();
        // pack RGB components into u32 value
        let state = ((r as u32) << 16) | ((g as u32) << 8) | b as u32;
        Self {
            rgba,
            state,
            length: 8,
        }
    }

    /// Convert octree path to a color
    pub fn rgba(&self) -> RGBA {
        self.rgba
    }
}

impl Iterator for OcTreePath {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        if self.length == 0 {
            return None;
        }
        self.length -= 1;
        // - We should pick most significant bit from each component
        //   and concatenate into one value to get an index inside
        //   octree.
        // - Left shift all components and set least significant bits
        //   of all components to zero.
        // - Repeat until all bits of all components are zero
        let bits = self.state & 0x00808080;
        self.state = (self.state << 1) & 0x00fefefe;
        let value = ((bits >> 21) | (bits >> 14) | (bits >> 7)) & 0b111;
        Some(value as usize)
    }
}

/// 3-dimensional KDTree which is used to quickly find nearest (euclidean distance)
/// color from the palette.
///
/// Reference: [k-d tree](https://en.wikipedia.org/wiki/K-d_tree)
pub struct KDTree {
    nodes: Vec<KDNode>,
}

#[derive(Debug, Clone, Copy)]
struct KDNode {
    color: [u8; 3],
    color_index: usize,
    dim: usize,
    left: Option<usize>,
    right: Option<usize>,
}

impl KDTree {
    /// Create k-d tree from the list of colors
    pub fn new(colors: &[RGBA]) -> Self {
        fn build_rec(
            dim: usize,
            nodes: &mut Vec<KDNode>,
            colors: &mut [(usize, [u8; 3])],
        ) -> Option<usize> {
            match colors {
                [] => return None,
                [(color_index, color)] => {
                    nodes.push(KDNode {
                        color: *color,
                        color_index: *color_index,
                        dim,
                        left: None,
                        right: None,
                    });
                    return Some(nodes.len() - 1);
                }
                _ => (),
            }
            colors.sort_by_key(|(_, c)| c[dim]);
            let index = colors.len() / 2;
            let dim_next = (dim + 1) % 3;
            let left = build_rec(dim_next, nodes, &mut colors[..index]);
            let right = build_rec(dim_next, nodes, &mut colors[(index + 1)..]);
            let (color_index, color) = colors[index];
            nodes.push(KDNode {
                color,
                color_index,
                dim,
                left,
                right,
            });
            Some(nodes.len() - 1)
        }

        let mut nodes = Vec::new();
        let mut colors: Vec<_> = colors.iter().map(|c| c.rgb_u8()).enumerate().collect();
        build_rec(0, &mut nodes, &mut colors);
        Self { nodes }
    }

    /// Find nearest neighbor color (euclidean distance) in the palette
    pub fn find(&self, color: RGBA) -> (usize, RGBA) {
        fn dist(rgb: [u8; 3], node: &KDNode) -> i32 {
            let [r0, g0, b0] = rgb;
            let [r1, g1, b1] = node.color;
            (r0 as i32 - r1 as i32).pow(2)
                + (g0 as i32 - g1 as i32).pow(2)
                + (b0 as i32 - b1 as i32).pow(2)
        }

        fn find_rec(nodes: &[KDNode], index: usize, target: [u8; 3]) -> (KDNode, i32) {
            let node = nodes[index];
            let node_dist = dist(target, &node);
            let (next, other) = if target[node.dim] < node.color[node.dim] {
                (node.left, node.right)
            } else {
                (node.right, node.left)
            };
            let (guess, guess_dist) = match next {
                None => (node, node_dist),
                Some(next_index) => {
                    let (guess, guess_dist) = find_rec(nodes, next_index, target);
                    if guess_dist >= node_dist {
                        (node, node_dist)
                    } else {
                        (guess, guess_dist)
                    }
                }
            };
            // check if the other branch is closer then best match we have found so far.
            let other_dist = (target[node.dim] as i32 - node.color[node.dim] as i32).pow(2);
            if other_dist >= guess_dist {
                return (guess, guess_dist);
            }
            match other {
                None => (guess, guess_dist),
                Some(other_index) => {
                    let (other, other_dist) = find_rec(nodes, other_index, target);
                    if other_dist < guess_dist {
                        (other, other_dist)
                    } else {
                        (guess, guess_dist)
                    }
                }
            }
        }

        let node = find_rec(&self.nodes, self.nodes.len() - 1, color.rgb_u8()).0;
        let [r, g, b] = node.color;
        (node.color_index, RGBA::new(r, g, b, 255))
    }

    /// Render k-d tree as graphviz digraph (for debugging)
    pub fn to_digraph(&self, mut out: impl Write) -> std::io::Result<()> {
        fn to_digraph_rec(
            out: &mut impl Write,
            nodes: &[KDNode],
            index: usize,
        ) -> std::io::Result<()> {
            let node = nodes[index];
            let d = match node.dim {
                0 => "R",
                1 => "G",
                2 => "B",
                _ => unreachable!(),
            };
            let [r, g, b] = node.color;
            let color = RGBA::new(r, g, b, 255);
            let fg = color.best_contrast(RGBA::new(255, 255, 255, 255), RGBA::new(0, 0, 0, 255));
            writeln!(
                out,
                "  {} [style=filled, fontcolor=\"{}\" fillcolor=\"{}\", label=\"{} {} {:?}\"]",
                index, fg, color, d, node.color[node.dim], node.color,
            )?;
            if let Some(left) = node.left {
                writeln!(out, "  {} -> {} [color=green]", index, left)?;
                to_digraph_rec(out, nodes, left)?;
            }
            if let Some(right) = node.right {
                writeln!(out, "  {} -> {} [color=red]", index, right)?;
                to_digraph_rec(out, nodes, right)?;
            }
            Ok(())
        }

        writeln!(out, "digraph KDTree {{")?;
        writeln!(out, "  rankdir=\"LR\"")?;
        to_digraph_rec(&mut out, &self.nodes, self.nodes.len() - 1)?;
        writeln!(out, "}}")?;
        Ok(())
    }
}

/// Color palette which implements fast NNS with euclidean distance.
pub struct ColorPalette {
    colors: Vec<RGBA>,
    kdtree: KDTree,
}

impl ColorPalette {
    /// Create new palette for the list of colors
    pub fn new(colors: Vec<RGBA>) -> Option<Self> {
        if colors.is_empty() {
            None
        } else {
            let kdtree = KDTree::new(&colors);
            Some(Self { colors, kdtree })
        }
    }

    /// Extract palette from image using `OcTree`
    pub fn from_image(
        img: impl Surface<Item = RGBA>,
        palette_size: usize,
        bg: RGBA,
    ) -> Option<Self> {
        fn blend(bg: RGBA, color: RGBA) -> RGBA {
            if color.rgba_u8()[3] < 255 {
                bg.blend(color, Blend::Over)
            } else {
                color
            }
        }

        if img.is_empty() {
            return None;
        }
        let sample: u32 = (img.height() * img.width() / (palette_size * 100)) as u32;
        let mut octree: OcTree = if sample < 2 {
            img.iter().map(|c| blend(bg, *c)).collect()
        } else {
            let mut octree = OcTree::new();
            let mut rnd = Rnd::new();
            let mut colors = img.iter().copied();
            while let Some(color) = colors.nth((rnd.next_u32() % sample) as usize) {
                octree.insert(blend(bg, color));
            }
            octree
        };
        octree.prune_until(palette_size);
        Self::new(octree.build_palette())
    }

    // Number of color in the palette
    pub fn size(&self) -> usize {
        self.colors.len()
    }

    /// Get color by the index
    pub fn get(&self, index: usize) -> RGBA {
        self.colors[index]
    }

    /// List of colors available in the palette
    pub fn colors(&self) -> &[RGBA] {
        &self.colors
    }

    /// Find nearest color in the palette
    ///
    /// Returns index of the color and color itself
    pub fn find(&self, color: RGBA) -> (usize, RGBA) {
        self.kdtree.find(color)
    }

    /// Find nearest color in the palette by going over all colors
    ///
    /// This is a slower version of the find method, used only for testing
    /// find correctness and speed.
    pub fn find_naive(&self, color: RGBA) -> (usize, RGBA) {
        fn dist(c0: RGBA, c1: RGBA) -> i32 {
            let [r0, g0, b0] = c0.rgb_u8();
            let [r1, g1, b1] = c1.rgb_u8();
            (r0 as i32 - r1 as i32).pow(2)
                + (g0 as i32 - g1 as i32).pow(2)
                + (b0 as i32 - b1 as i32).pow(2)
        }
        let best_dist = dist(color, self.colors[0]);
        let (best_index, _) =
            (1..self.colors.len()).fold((0, best_dist), |(best_index, best_dist), index| {
                let dist = dist(color, self.colors[index]);
                if dist < best_dist {
                    (index, dist)
                } else {
                    (best_index, best_dist)
                }
            });
        (best_index, self.colors[best_index])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Convert path generated by OcTreePath back to RGBA color
    fn color_from_path(path: &Vec<usize>) -> RGBA {
        let mut r: u8 = 0;
        let mut g: u8 = 0;
        let mut b: u8 = 0;
        for index in 0..8 {
            r <<= 1;
            g <<= 1;
            b <<= 1;
            let bits = path.get(index).unwrap_or(&0);
            if bits & 0b100 != 0 {
                r |= 1;
            }
            if bits & 0b010 != 0 {
                g |= 1;
            }
            if bits & 0b001 != 0 {
                b |= 1;
            }
        }
        RGBA::new(r, g, b, 255)
    }

    #[test]
    fn test_octree_path() -> Result<(), Error> {
        let c0 = "#5a719d".parse::<RGBA>()?;
        let path: Vec<_> = OcTreePath::new(c0).collect();
        assert_eq!(path, vec![1, 6, 2, 7, 5, 1, 4, 3]);
        assert_eq!(c0, color_from_path(&path));

        let c1 = "#808080".parse::<RGBA>()?;
        let path: Vec<_> = OcTreePath::new(c1).collect();
        assert_eq!(path, vec![7, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(c1, color_from_path(&vec![7]));

        let c2 = "#d3869b".parse::<RGBA>()?;
        let path: Vec<_> = OcTreePath::new(c2).collect();
        assert_eq!(path, vec![7, 4, 0, 5, 1, 2, 7, 5]);
        assert_eq!(c2, color_from_path(&path));

        Ok(())
    }

    #[test]
    fn test_octree_info() {
        let mut tree = OcTree::new();
        tree.node_update(1, |_| {
            OcTreeNode::Leaf(OcTreeLeaf {
                red_acc: 1,
                green_acc: 2,
                blue_acc: 3,
                color_count: 4,
                index: 0,
            })
        });
        assert_eq!(
            tree.info,
            OcTreeInfo {
                leaf_count: 1,
                color_count: 4,
                min_color_count: Some(4),
            }
        );
    }

    #[test]
    fn test_octree() -> Result<(), Error> {
        let c0 = "#5a719d".parse::<RGBA>()?;
        let c1 = "#d3869b".parse::<RGBA>()?;

        let mut tree = OcTree::new();

        tree.insert(c0);
        tree.insert(c0);
        assert_eq!(
            tree.info(),
            OcTreeInfo {
                color_count: 2,
                leaf_count: 1,
                min_color_count: Some(2),
            }
        );
        assert_eq!(tree.find(c0), Some((0, c0)));

        tree.insert(c1);
        assert_eq!(
            tree.info(),
            OcTreeInfo {
                color_count: 3,
                leaf_count: 2,
                min_color_count: Some(1),
            }
        );
        assert_eq!(tree.find(c1), Some((0, c1)));

        Ok(())
    }

    #[test]
    pub fn test_palette() {
        // make sure that k-d tree can actually find nearest neighbor
        fn dist(c0: RGBA, c1: RGBA) -> i32 {
            let [r0, g0, b0] = c0.rgb_u8();
            let [r1, g1, b1] = c1.rgb_u8();
            (r0 as i32 - r1 as i32).pow(2)
                + (g0 as i32 - g1 as i32).pow(2)
                + (b0 as i32 - b1 as i32).pow(2)
        }

        let mut gen = RGBA::random();
        let palette = ColorPalette::new((&mut gen).take(256).collect()).unwrap();
        let mut colors: Vec<_> = gen.take(65_536).collect();
        colors.extend(palette.colors().iter().copied());
        for (index, color) in colors.iter().enumerate() {
            let (_, find) = palette.find(*color);
            let (_, find_naive) = palette.find_naive(*color);
            if find != find_naive && dist(*color, find) != dist(*color, find_naive) {
                dbg!(dist(*color, find));
                dbg!(dist(*color, find_naive));
                panic!(
                    "failed to find colors[{}]={:?}: find_naive={:?} find={:?}",
                    index, color, find_naive, find
                );
            }
        }
    }
}
