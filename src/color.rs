//! Color definition
use crate::common::{clamp, Rnd};
use crate::error::Error;
use std::{
    fmt,
    ops::{Add, Mul},
    str::FromStr,
};

/// Blend methods
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Blend {
    Over,
    Out,
    In,
    Atop,
    Xor,
}

pub trait Color: From<ColorLinear> + Into<ColorLinear> + Copy {
    /// Convert color into 32-bit sRGB array with alpha (channels are not pre-multiplied).
    fn rgba_u8(self) -> [u8; 4];

    /// Convert color into 24-bit sRGB array without alpha (channels are not pre-multiplied).
    fn rgb_u8(self) -> [u8; 3] {
        let [r, g, b, a] = self.rgba_u8();
        if a == 255 {
            [r, g, b]
        } else {
            let alpha = a as f64 / 255.0;
            let [r, g, b, _] = ColorLinear([0.0, 0.0, 0.0, 1.0])
                .lerp(self, alpha)
                .rgba_u8();
            [r, g, b]
        }
    }

    /// Blend current color with the other color, with the specified blend method.
    fn blend(self, other: impl Color, method: Blend) -> Self {
        // Reference:
        // https://ciechanow.ski/alpha-compositing/
        // http://ssp.impulsetrain.com/porterduff.html
        let dst = self.into();
        let dst_a = dst.0[3];
        let src = other.into();
        let src_a = src.0[3];
        let color = match method {
            Blend::Over => src + dst * (1.0 - src_a),
            Blend::Out => src * (1.0 - dst_a),
            Blend::In => src * dst_a,
            Blend::Atop => src * dst_a + dst * (1.0 - src_a),
            Blend::Xor => src * (1.0 - dst_a) + dst * (1.0 - src_a),
        };
        color.into()
    }

    /// Linear interpolation between self and other colors.
    fn lerp(self, other: impl Color, t: f64) -> Self {
        let start = self.into();
        let end = other.into();
        let color = start * (1.0 - t) + end * t;
        color.into()
    }

    /// Calculate luma of the color.
    fn luma(self) -> f64 {
        let [r, g, b] = self.rgb_u8();
        0.2126 * (r as f64 / 255.0) + 0.7152 * (g as f64 / 255.0) + 0.0722 * (b as f64 / 255.0)
    }

    /// Pick color that produces the best contrast with self
    fn best_contrast(self, c0: impl Color, c1: impl Color) -> Self {
        let luma = self.luma();
        let c0: ColorLinear = c0.into();
        let c1: ColorLinear = c1.into();
        if (luma - c0.luma()).abs() < (luma - c1.luma()).abs() {
            c1.into()
        } else {
            c0.into()
        }
    }
}

/// Convert Linear RGB color component into a SRGB color component.
///
/// It was hard to optimize this function, even current version
/// is slow because of the conditional jump. Lookup table is not working
/// here as well it should be at least 4K in size an not cache friendly.
#[inline]
pub fn linear_to_srgb(x0: f64) -> f64 {
    if x0 <= 0.0031308 {
        x0 * 12.92
    } else {
        // This function is generated by least square fitting of
        // `f(x) = 1.055 * x.powf(1.0 / 2.4) - 0.055` on value [0.0031308..1.0]
        // see `scripts/srgb.py` for details.
        let x1 = x0.sqrt();
        let x2 = x1.sqrt();
        let x3 = x2.sqrt();
        -0.01848558 * x0 + 0.64455921 * x1 + 0.70994762 * x2 - 0.33605254 * x3
    }
}

/// Color in linear RGB color space with premultiplied alpha
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct ColorLinear(pub [f64; 4]);

impl Mul<f64> for ColorLinear {
    type Output = Self;

    #[inline]
    fn mul(self, val: f64) -> Self::Output {
        let Self([r, g, b, a]) = self;
        Self([r * val, g * val, b * val, a * val])
    }
}

impl Add<Self> for ColorLinear {
    type Output = Self;

    #[inline]
    fn add(self, other: Self) -> Self::Output {
        let Self([r0, g0, b0, a0]) = self;
        let Self([r1, g1, b1, a1]) = other;
        Self([r0 + r1, g0 + g1, b0 + b1, a0 + a1])
    }
}

impl ColorLinear {
    pub fn new(r: f64, g: f64, b: f64, a: f64) -> Self {
        Self([r, g, b, a])
    }

    pub fn distance(&self, other: &Self) -> f64 {
        let Self([r0, g0, b0, _]) = *self;
        let Self([r1, g1, b1, _]) = *other;
        ((r0 - r1).powi(2) + (g0 - g1).powi(2) + (b0 - b1).powi(2)).sqrt()
    }
}

impl Color for ColorLinear {
    fn rgba_u8(self) -> [u8; 4] {
        RGBA::from(self).rgba_u8()
    }
}

/// u8 RGBA color
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RGBA(pub [u8; 4]);

impl Default for RGBA {
    fn default() -> Self {
        Self([0, 0, 0, 0])
    }
}

impl RGBA {
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        RGBA([r, g, b, a])
    }

    /// Override alpha channel
    pub fn with_alpha(self, alpha: f64) -> Self {
        let Self([r, g, b, _]) = self;
        let a = (clamp(alpha, 0.0, 1.0) * 255.0).round() as u8;
        Self([r, g, b, a])
    }

    /// Generate random opaque colors
    pub fn random() -> impl Iterator<Item = RGBA> {
        let mut rnd = Rnd::new();
        std::iter::from_fn(move || {
            let value = rnd.next_u32();
            Some(RGBA::new(
                (value & 0xff) as u8,
                ((value >> 8) & 0xff) as u8,
                ((value >> 16) & 0xff) as u8,
                255,
            ))
        })
    }

    /// Parse color from string
    pub fn from_str_opt(rgba: &str) -> Option<Self> {
        let rgba = rgba.trim_matches('"');
        if rgba.starts_with('#') && (rgba.len() == 7 || rgba.len() == 9) {
            // #RRGGBB(AA)
            let mut hex = crate::decoder::hex_decode(rgba[1..].as_bytes());
            let red = hex.next()?;
            let green = hex.next()?;
            let blue = hex.next()?;
            let alpha = if rgba.len() == 9 { hex.next()? } else { 255 };
            Some(Self([red, green, blue, alpha]))
        } else if let Some(rgba) = rgba.strip_prefix("rgb:") {
            // rgb:r{1-4}/g{1-4}/b{1-4}
            // This format is used when querying colors with OCS,
            // refrence [xparsecolor](https://linux.die.net/man/3/xparsecolor)
            fn parse_component(string: &str) -> Option<u8> {
                let value = usize::from_str_radix(string, 16).ok()?;
                let value = match string.len() {
                    4 => value / 256,
                    3 => value / 16,
                    2 => value,
                    1 => value * 17,
                    _ => return None,
                };
                Some(clamp(value, 0, 255) as u8)
            }
            let mut iter = rgba.split('/');
            let red = parse_component(iter.next()?)?;
            let green = parse_component(iter.next()?)?;
            let blue = parse_component(iter.next()?)?;
            Some(Self([red, green, blue, 255]))
        } else {
            None
        }
    }
}

impl Color for RGBA {
    fn rgba_u8(self) -> [u8; 4] {
        self.0
    }
}

impl From<RGBA> for ColorLinear {
    fn from(color: RGBA) -> Self {
        let [r, g, b, a] = color.rgba_u8();
        let a = (a as f64) / 255.0;
        unsafe {
            let r = SRGB_TO_LIN.get_unchecked(r as usize) * a;
            let g = SRGB_TO_LIN.get_unchecked(g as usize) * a;
            let b = SRGB_TO_LIN.get_unchecked(b as usize) * a;
            ColorLinear([r, g, b, a])
        }
    }
}

impl From<ColorLinear> for RGBA {
    fn from(color: ColorLinear) -> Self {
        let ColorLinear([r, g, b, a]) = color;
        if a < std::f64::EPSILON {
            Self([0, 0, 0, 0])
        } else {
            let a = clamp(a, 0.0, 1.0);
            let r = (linear_to_srgb(r / a) * 255.0).round() as u8;
            let g = (linear_to_srgb(g / a) * 255.0).round() as u8;
            let b = (linear_to_srgb(b / a) * 255.0).round() as u8;
            let a = (a * 255.0) as u8;
            Self([r, g, b, a])
        }
    }
}

impl FromStr for RGBA {
    type Err = Error;

    fn from_str(string: &str) -> Result<Self, Self::Err> {
        Self::from_str_opt(string).ok_or_else(|| Error::ParseError("RGBA", string.to_string()))
    }
}

impl FromStr for ColorLinear {
    type Err = Error;

    fn from_str(string: &str) -> Result<Self, Self::Err> {
        RGBA::from_str(string).map(ColorLinear::from)
    }
}

impl fmt::Display for RGBA {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [r, g, b, a] = self.rgba_u8();
        write!(fmt, "#{:02x}{:02x}{:02x}", r, g, b)?;
        if a != 255 {
            write!(fmt, "{:02x}", a)?;
        }
        Ok(())
    }
}

impl fmt::Debug for RGBA {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [bg_r, bg_g, bg_b] = self.rgb_u8();
        let [fg_r, fg_g, fg_b] = self
            .best_contrast(RGBA::new(255, 255, 255, 255), RGBA::new(0, 0, 0, 255))
            .rgb_u8();
        write!(
            fmt,
            "\x1b[38;2;{};{};{};48;2;{};{};{}m",
            fg_r, fg_g, fg_b, bg_r, bg_g, bg_b
        )?;
        write!(fmt, "{}", self)?;
        write!(fmt, "\x1b[m")
    }
}

// sRGB to Linear RGB component lookup table
//
// This table is calculating by evaluating `true_s2l` on all u8 values
// fn srgb_to_linear(value: f64) -> f64 {
//     if value <= 0.04045 {
//         value / 12.92
//     } else {
//         ((value + 0.055) / 1.055).powf(2.4)
//     }
// }
// Lookup in this table is fast as it is small enough to fit in the cache
const SRGB_TO_LIN: [f64; 256] = [
    0.0, 0.00030353, 0.00060705, 0.00091058, 0.00121411, 0.00151763, 0.00182116, 0.00212469,
    0.00242822, 0.00273174, 0.00303527, 0.00334654, 0.00367651, 0.00402472, 0.00439144, 0.00477695,
    0.00518152, 0.00560539, 0.00604883, 0.00651209, 0.00699541, 0.00749903, 0.00802319, 0.00856813,
    0.00913406, 0.00972122, 0.01032982, 0.01096009, 0.01161225, 0.01228649, 0.01298303, 0.01370208,
    0.01444384, 0.01520851, 0.01599629, 0.01680738, 0.01764195, 0.01850022, 0.01938236, 0.02028856,
    0.02121901, 0.02217388, 0.02315337, 0.02415763, 0.02518686, 0.02624122, 0.02732089, 0.02842604,
    0.02955683, 0.03071344, 0.03189603, 0.03310477, 0.03433981, 0.03560131, 0.03688945, 0.03820437,
    0.03954624, 0.0409152, 0.04231141, 0.04373503, 0.0451862, 0.04666509, 0.04817182, 0.04970657,
    0.05126946, 0.05286065, 0.05448028, 0.05612849, 0.05780543, 0.05951124, 0.06124605, 0.06301002,
    0.06480327, 0.06662594, 0.06847817, 0.0703601, 0.07227185, 0.07421357, 0.07618538, 0.07818742,
    0.08021982, 0.08228271, 0.08437621, 0.08650046, 0.08865559, 0.09084171, 0.09305896, 0.09530747,
    0.09758735, 0.09989873, 0.10224173, 0.10461648, 0.1070231, 0.10946171, 0.11193243, 0.11443537,
    0.11697067, 0.11953843, 0.12213877, 0.12477182, 0.12743768, 0.13013648, 0.13286832, 0.13563333,
    0.13843162, 0.14126329, 0.14412847, 0.14702727, 0.14995979, 0.15292615, 0.15592646, 0.15896084,
    0.16202938, 0.16513219, 0.1682694, 0.1714411, 0.1746474, 0.17788842, 0.18116424, 0.18447499,
    0.18782077, 0.19120168, 0.19461783, 0.19806932, 0.20155625, 0.20507874, 0.20863687, 0.21223076,
    0.2158605, 0.2195262, 0.22322796, 0.22696587, 0.23074005, 0.23455058, 0.23839757, 0.24228112,
    0.24620133, 0.25015828, 0.25415209, 0.25818285, 0.26225066, 0.2663556, 0.27049779, 0.27467731,
    0.27889426, 0.28314874, 0.28744084, 0.29177065, 0.29613827, 0.30054379, 0.30498731, 0.30946892,
    0.31398871, 0.31854678, 0.32314321, 0.3277781, 0.33245154, 0.33716362, 0.34191442, 0.34670406,
    0.3515326, 0.35640014, 0.36130678, 0.3662526, 0.37123768, 0.37626212, 0.38132601, 0.38642943,
    0.39157248, 0.39675523, 0.40197778, 0.40724021, 0.41254261, 0.41788507, 0.42326767, 0.4286905,
    0.43415364, 0.43965717, 0.44520119, 0.45078578, 0.45641102, 0.462077, 0.4677838, 0.4735315,
    0.47932018, 0.48514994, 0.49102085, 0.496933, 0.50288646, 0.50888132, 0.51491767, 0.52099557,
    0.52711513, 0.5332764, 0.53947949, 0.54572446, 0.5520114, 0.55834039, 0.56471151, 0.57112483,
    0.57758044, 0.58407842, 0.59061884, 0.59720179, 0.60382734, 0.61049557, 0.61720656, 0.62396039,
    0.63075714, 0.63759687, 0.64447968, 0.65140564, 0.65837482, 0.6653873, 0.67244316, 0.67954247,
    0.68668531, 0.69387176, 0.70110189, 0.70837578, 0.7156935, 0.72305513, 0.73046074, 0.73791041,
    0.74540421, 0.75294222, 0.7605245, 0.76815115, 0.77582222, 0.78353779, 0.79129794, 0.79910274,
    0.80695226, 0.81484657, 0.82278575, 0.83076988, 0.83879901, 0.84687323, 0.85499261, 0.86315721,
    0.87136712, 0.8796224, 0.88792312, 0.89626935, 0.90466117, 0.91309865, 0.92158186, 0.93011086,
    0.93868573, 0.94730654, 0.95597335, 0.96468625, 0.97344529, 0.98225055, 0.9911021, 1.0,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_color() -> Result<(), Error> {
        assert_eq!("#d3869b".parse::<RGBA>()?, RGBA([211, 134, 155, 255]));
        assert_eq!(
            "rgb:d3d3/86/9b".parse::<RGBA>()?,
            RGBA([211, 134, 155, 255])
        );
        assert_eq!("#b8bb2680".parse::<RGBA>()?, RGBA([184, 187, 38, 128]));
        Ok(())
    }

    #[test]
    fn test_color_linear() -> Result<(), Error> {
        let color = "#fe801970".parse()?;
        assert_eq!(RGBA::from(ColorLinear::from(color)), color);

        for input in 0..256 {
            let output = (linear_to_srgb(SRGB_TO_LIN[input]) * 255.0).round() as u8;
            assert_eq!(input as u8, output);
        }

        Ok(())
    }
}
