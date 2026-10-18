//! X PixMap version 3 (`.xpm`): a C source file holding an array of strings: a values
//! line (`width height ncolors chars_per_pixel`), the colour table, then the pixel rows.
//! Colours are `#rgb` hex, `None` (transparent) or an X11 colour name from the table below;
//! an unknown name is an error rather than a guess.

use std::collections::HashMap;

use image::DynamicImage;

use super::{DecodeError, Result, pixel_bytes, rgba_image};

/// Common X11 colour names (lower-case, no spaces) with their `rgb.txt` values.
const NAMES: &[(&str, [u8; 3])] = &[
    ("black", [0, 0, 0]),
    ("white", [255, 255, 255]),
    ("red", [255, 0, 0]),
    ("green", [0, 255, 0]),
    ("blue", [0, 0, 255]),
    ("yellow", [255, 255, 0]),
    ("cyan", [0, 255, 255]),
    ("magenta", [255, 0, 255]),
    ("gray", [190, 190, 190]),
    ("grey", [190, 190, 190]),
    ("lightgray", [211, 211, 211]),
    ("lightgrey", [211, 211, 211]),
    ("darkgray", [169, 169, 169]),
    ("darkgrey", [169, 169, 169]),
    ("dimgray", [105, 105, 105]),
    ("dimgrey", [105, 105, 105]),
    ("slategray", [112, 128, 144]),
    ("slategrey", [112, 128, 144]),
    ("lightslategray", [119, 136, 153]),
    ("lightslategrey", [119, 136, 153]),
    ("darkslategray", [47, 79, 79]),
    ("darkslategrey", [47, 79, 79]),
    ("gainsboro", [220, 220, 220]),
    ("whitesmoke", [245, 245, 245]),
    ("snow", [255, 250, 250]),
    ("ghostwhite", [248, 248, 255]),
    ("ivory", [255, 255, 240]),
    ("linen", [250, 240, 230]),
    ("beige", [245, 245, 220]),
    ("seashell", [255, 245, 238]),
    ("azure", [240, 255, 255]),
    ("aliceblue", [240, 248, 255]),
    ("mintcream", [245, 255, 250]),
    ("honeydew", [240, 255, 240]),
    ("lavender", [230, 230, 250]),
    ("lavenderblush", [255, 240, 245]),
    ("mistyrose", [255, 228, 225]),
    ("oldlace", [253, 245, 230]),
    ("floralwhite", [255, 250, 240]),
    ("cornsilk", [255, 248, 220]),
    ("lemonchiffon", [255, 250, 205]),
    ("lightyellow", [255, 255, 224]),
    ("lightgoldenrodyellow", [250, 250, 210]),
    ("lightgoldenrod", [238, 221, 130]),
    ("papayawhip", [255, 239, 213]),
    ("moccasin", [255, 228, 181]),
    ("peachpuff", [255, 218, 185]),
    ("bisque", [255, 228, 196]),
    ("blanchedalmond", [255, 235, 205]),
    ("antiquewhite", [250, 235, 215]),
    ("wheat", [245, 222, 179]),
    ("navajowhite", [255, 222, 173]),
    ("burlywood", [222, 184, 135]),
    ("tan", [210, 180, 140]),
    ("khaki", [240, 230, 140]),
    ("darkkhaki", [189, 183, 107]),
    ("palegoldenrod", [238, 232, 170]),
    ("gold", [255, 215, 0]),
    ("goldenrod", [218, 165, 32]),
    ("darkgoldenrod", [184, 134, 11]),
    ("orange", [255, 165, 0]),
    ("darkorange", [255, 140, 0]),
    ("orangered", [255, 69, 0]),
    ("tomato", [255, 99, 71]),
    ("coral", [255, 127, 80]),
    ("lightcoral", [240, 128, 128]),
    ("salmon", [250, 128, 114]),
    ("darksalmon", [233, 150, 122]),
    ("lightsalmon", [255, 160, 122]),
    ("indianred", [205, 92, 92]),
    ("firebrick", [178, 34, 34]),
    ("brown", [165, 42, 42]),
    ("darkred", [139, 0, 0]),
    ("maroon", [176, 48, 96]),
    ("crimson", [220, 20, 60]),
    ("pink", [255, 192, 203]),
    ("lightpink", [255, 182, 193]),
    ("hotpink", [255, 105, 180]),
    ("deeppink", [255, 20, 147]),
    ("palevioletred", [219, 112, 147]),
    ("mediumvioletred", [199, 21, 133]),
    ("violetred", [208, 32, 144]),
    ("violet", [238, 130, 238]),
    ("orchid", [218, 112, 214]),
    ("mediumorchid", [186, 85, 211]),
    ("darkorchid", [153, 50, 204]),
    ("plum", [221, 160, 221]),
    ("thistle", [216, 191, 216]),
    ("purple", [160, 32, 240]),
    ("mediumpurple", [147, 112, 219]),
    ("blueviolet", [138, 43, 226]),
    ("darkviolet", [148, 0, 211]),
    ("darkmagenta", [139, 0, 139]),
    ("indigo", [75, 0, 130]),
    ("slateblue", [106, 90, 205]),
    ("darkslateblue", [72, 61, 139]),
    ("mediumslateblue", [123, 104, 238]),
    ("navy", [0, 0, 128]),
    ("navyblue", [0, 0, 128]),
    ("darkblue", [0, 0, 139]),
    ("mediumblue", [0, 0, 205]),
    ("midnightblue", [25, 25, 112]),
    ("royalblue", [65, 105, 225]),
    ("cornflowerblue", [100, 149, 237]),
    ("dodgerblue", [30, 144, 255]),
    ("steelblue", [70, 130, 180]),
    ("lightsteelblue", [176, 196, 222]),
    ("deepskyblue", [0, 191, 255]),
    ("skyblue", [135, 206, 235]),
    ("lightskyblue", [135, 206, 250]),
    ("lightblue", [173, 216, 230]),
    ("powderblue", [176, 224, 230]),
    ("paleturquoise", [175, 238, 238]),
    ("lightcyan", [224, 255, 255]),
    ("cadetblue", [95, 158, 160]),
    ("turquoise", [64, 224, 208]),
    ("mediumturquoise", [72, 209, 204]),
    ("darkturquoise", [0, 206, 209]),
    ("darkcyan", [0, 139, 139]),
    ("aquamarine", [127, 255, 212]),
    ("mediumaquamarine", [102, 205, 170]),
    ("mediumseagreen", [60, 179, 113]),
    ("seagreen", [46, 139, 87]),
    ("darkseagreen", [143, 188, 143]),
    ("lightseagreen", [32, 178, 170]),
    ("palegreen", [152, 251, 152]),
    ("lightgreen", [144, 238, 144]),
    ("springgreen", [0, 255, 127]),
    ("mediumspringgreen", [0, 250, 154]),
    ("lawngreen", [124, 252, 0]),
    ("chartreuse", [127, 255, 0]),
    ("greenyellow", [173, 255, 47]),
    ("yellowgreen", [154, 205, 50]),
    ("limegreen", [50, 205, 50]),
    ("forestgreen", [34, 139, 34]),
    ("darkgreen", [0, 100, 0]),
    ("olivedrab", [107, 142, 35]),
    ("darkolivegreen", [85, 107, 47]),
    ("saddlebrown", [139, 69, 19]),
    ("sienna", [160, 82, 45]),
    ("chocolate", [210, 105, 30]),
    ("peru", [205, 133, 63]),
    ("sandybrown", [244, 164, 96]),
    ("rosybrown", [188, 143, 143]),
];

fn lookup_name(name: &str) -> Option<[u8; 3]> {
    let key: String = name
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    if let Some(&(_, rgb)) = NAMES.iter().find(|(n, _)| *n == key) {
        return Some(rgb);
    }
    // grayNN / greyNN: NN percent.
    let pct = key
        .strip_prefix("gray")
        .or_else(|| key.strip_prefix("grey"))?;
    let pct: u32 = pct.parse().ok().filter(|p| *p <= 100)?;
    let v = ((pct * 255 + 50) / 100) as u8;
    Some([v, v, v])
}

/// `#rgb`, `#rrggbb`, `#rrrgggbbb` or `#rrrrggggbbbb`: the top 8 bits of each component.
fn parse_hex(spec: &str) -> Option<[u8; 3]> {
    let hex = spec.strip_prefix('#')?;
    if !hex.is_ascii() || hex.is_empty() || hex.len() % 3 != 0 || hex.len() > 12 {
        return None;
    }
    let n = hex.len() / 3;
    let mut rgb = [0u8; 3];
    for (c, out) in rgb.iter_mut().enumerate() {
        let digits = &hex[c * n..(c + 1) * n];
        let v = u32::from_str_radix(digits, 16).ok()?;
        *out = match n {
            1 => (v * 17) as u8,
            _ => u32::from_str_radix(&digits[..2], 16).ok()? as u8,
        };
    }
    Some(rgb)
}

/// Colour for one definition value (`None` -> fully transparent).
fn parse_color(spec: &str) -> Result<[u8; 4]> {
    let spec = spec.trim();
    if spec.eq_ignore_ascii_case("none") {
        return Ok([0, 0, 0, 0]);
    }
    let rgb = if spec.starts_with('#') {
        parse_hex(spec)
    } else {
        lookup_name(spec)
    };
    rgb.map(|[r, g, b]| [r, g, b, 255])
        .ok_or_else(|| DecodeError::malformed(format!("unknown XPM colour '{spec}'")))
}

/// The quoted strings of the C array, in order. Comments between them are skipped.
fn strings(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let Some(start) = data.iter().position(|&b| b == b'{') else {
        return out;
    };
    let mut i = start + 1;
    while i < data.len() {
        match data[i] {
            b'"' => {
                let from = i + 1;
                let mut j = from;
                while j < data.len() && data[j] != b'"' {
                    j += if data[j] == b'\\' { 2 } else { 1 };
                }
                if j >= data.len() {
                    break;
                }
                out.push(&data[from..j]);
                i = j + 1;
            }
            b'/' if data.get(i + 1) == Some(&b'*') => {
                i = match data[i + 2..].windows(2).position(|w| w == b"*/") {
                    Some(p) => i + 2 + p + 2,
                    None => break,
                };
            }
            b'/' if data.get(i + 1) == Some(&b'/') => {
                i = data[i..]
                    .iter()
                    .position(|&b| b == b'\n')
                    .map_or(data.len(), |p| i + p + 1);
            }
            b'}' => break,
            _ => i += 1,
        }
    }
    out
}

/// Picks the colour value out of a definition tail such as `m white c #FFFFFF s name`
/// (preference: `c`, `g`, `g4`, `m`). Values may contain spaces ("dark slate gray").
fn color_value(tail: &str) -> Option<String> {
    let toks: Vec<&str> = tail.split_whitespace().collect();
    let rank = |t: &str| match t {
        "c" => 4,
        "g" => 3,
        "g4" => 2,
        "m" => 1,
        _ => 0,
    };
    let mut best: Option<(u8, String)> = None;
    for (i, &key) in toks.iter().enumerate() {
        let r = rank(key);
        if r == 0 && key != "s" {
            continue;
        }
        let end = toks[i + 1..]
            .iter()
            .position(|t| rank(t) > 0 || *t == "s")
            .map_or(toks.len(), |p| i + 1 + p);
        let value = toks[i + 1..end].join(" ");
        if r > 0 && !value.is_empty() && best.as_ref().is_none_or(|(b, _)| r > *b) {
            best = Some((r, value));
        }
    }
    best.map(|(_, v)| v)
}

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    let head = &data[..data.len().min(256)];
    if !head.windows(4).any(|w| w == b"XPM ") && !head.windows(5).any(|w| w == b"* XPM") {
        return Err(DecodeError::Unsupported);
    }
    let strs = strings(data);
    let values = std::str::from_utf8(strs.first().ok_or(DecodeError::Unsupported)?)
        .map_err(|_| DecodeError::malformed("bad XPM values line"))?;
    let nums: Vec<u32> = values
        .split_ascii_whitespace()
        .map_while(|t| t.parse().ok())
        .collect();
    let &[width, height, ncolors, cpp, ..] = nums.as_slice() else {
        return Err(DecodeError::malformed("bad XPM values line"));
    };
    if !(1..=16).contains(&cpp) || ncolors == 0 {
        return Err(DecodeError::malformed("bad XPM values line"));
    }
    let (cpp, ncolors, w, h) = (
        cpp as usize,
        ncolors as usize,
        width as usize,
        height as usize,
    );
    pixel_bytes(width, height, 4)?;
    // Every pixel needs `cpp` bytes of input, so the file bounds the image.
    let rows = strs
        .get(1 + ncolors..)
        .and_then(|s| s.get(..h))
        .ok_or_else(|| DecodeError::malformed("XPM data truncated"))?;
    let row_len = w.checked_mul(cpp).ok_or(DecodeError::TooLarge)?;
    if rows.iter().any(|r| r.len() < row_len) {
        return Err(DecodeError::malformed("XPM row too short"));
    }

    let mut table: HashMap<&[u8], [u8; 4]> = HashMap::with_capacity(ncolors);
    for def in &strs[1..=ncolors] {
        let key = def
            .get(..cpp)
            .ok_or_else(|| DecodeError::malformed("XPM colour line too short"))?;
        let tail = std::str::from_utf8(&def[cpp..])
            .map_err(|_| DecodeError::malformed("bad XPM colour line"))?;
        let value = color_value(tail)
            .ok_or_else(|| DecodeError::malformed("XPM colour without a value"))?;
        table.insert(key, parse_color(&value)?);
    }

    let mut out = Vec::with_capacity(w * h * 4);
    for row in rows {
        for px in row[..row_len].chunks_exact(cpp) {
            let c = table
                .get(px)
                .ok_or_else(|| DecodeError::malformed("XPM pixel uses an undefined colour"))?;
            out.extend_from_slice(c);
        }
    }
    rgba_image(width, height, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &[u8] = b"/* XPM */\nstatic char * t_xpm[] = {\n\"3 2 4 1\",\n\
\"a c #FF0000\",\n\"b c None\",\n\" \tc DarkSlateGray\",\n\"d m black c #00f\",\n\
/* pixels */\n\"abd\",\n\" dd\"\n};\n";

    #[test]
    fn colours_names_hex_and_transparent() {
        let img = decode(SRC).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (3, 2));
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(img.get_pixel(1, 0).0[3], 0);
        assert_eq!(img.get_pixel(2, 0).0, [0, 0, 255, 255]);
        assert_eq!(img.get_pixel(0, 1).0, [47, 79, 79, 255]);
    }

    #[test]
    fn unknown_colour_name_is_malformed() {
        let src = b"/* XPM */\nstatic char * t[] = {\"1 1 1 1\",\"a c nosuchcolor\",\"a\"};";
        assert!(matches!(decode(src), Err(DecodeError::Malformed(_))));
    }

    #[test]
    fn gray_levels_and_wide_hex() {
        assert_eq!(lookup_name("gray50"), Some([128, 128, 128]));
        assert_eq!(parse_hex("#ffff00000000"), Some([255, 0, 0]));
        assert_eq!(parse_hex("#f0f"), Some([255, 0, 255]));
    }

    #[test]
    fn hostile_inputs() {
        assert_eq!(decode(b"not xpm").unwrap_err(), DecodeError::Unsupported);
        assert!(matches!(
            decode(&SRC[..60]),
            Err(DecodeError::Malformed(_)) | Err(DecodeError::Unsupported)
        ));
        let huge =
            b"/* XPM */\nstatic char * t[] = {\"4294967295 4294967295 1 1\",\"a c red\",\"a\"};";
        assert_eq!(decode(huge).unwrap_err(), DecodeError::TooLarge);
        let huge = b"/* XPM */\nstatic char * t[] = {\"65535 65535 1 1\",\"a c red\",\"a\"};";
        assert_eq!(decode(huge).unwrap_err(), DecodeError::TooLarge);
        // Moderate dimensions but no pixel rows: rejected before allocating.
        let lie = b"/* XPM */\nstatic char * t[] = {\"5000 5000 1 1\",\"a c red\",\"a\"};";
        assert!(matches!(decode(lie), Err(DecodeError::Malformed(_))));
    }
}
