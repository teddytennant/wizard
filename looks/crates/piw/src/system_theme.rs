// OWNER: theme (Pi `system` theme generator)
//! Port of Pi's `system-theme.js` (`generateSystemThemeColors`) and the OKHSL / Oklab maths of
//! `pi-tui` it stands on (`oklab.js`, `colors.js`). Pure arithmetic: the terminal's reported
//! foreground, background and 16 palette colours in, one colour per token out.
//!
//! The numbers matter more than the style here. Every channel is rounded where the JavaScript
//! rounds (`okhslToRgb`, `linearSrgbToRgb`, `hexOf`) and every power goes through `powf`, because
//! `Math.pow` and repeated multiplication differ in the last bit and a bisection can amplify that
//! into a different hex digit. The tests compare against vectors that Pi's own generator printed
//! (`crates/tuikit/themes-pi/system-golden.json` and `system-golden-more.json`).

// the constants are copied digit for digit from pi-tui's oklab.js
#![allow(clippy::excessive_precision)]

use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rgb {
    pub r: f64,
    pub g: f64,
    pub b: f64,
}

impl Rgb {
    pub fn new(r: u8, g: u8, b: u8) -> Rgb {
        Rgb {
            r: r as f64,
            g: g as f64,
            b: b as f64,
        }
    }
    pub fn hex(self) -> String {
        let c = |v: f64| format!("{:02x}", v.round().clamp(0.0, 255.0) as u8);
        format!("#{}{}{}", c(self.r), c(self.g), c(self.b))
    }
    pub fn bytes(self) -> (u8, u8, u8) {
        let c = |v: f64| v.round().clamp(0.0, 255.0) as u8;
        (c(self.r), c(self.g), c(self.b))
    }
}

// ------------------------------------------------------------------------------------------
// oklab.js
// ------------------------------------------------------------------------------------------

type M3 = [[f64; 3]; 3];

const LINEAR_SRGB_TO_LMS: M3 = [
    [0.4122214694707629, 0.5363325372617349, 0.0514459932675022],
    [0.2119034958178251, 0.6806995506452344, 0.1073969535369405],
    [0.0883024591900564, 0.2817188391361215, 0.6299787016738222],
];
const LMS_TO_LAB: M3 = [
    [0.210454268309314, 0.793617774702305, -0.0040720430116193],
    [1.9779985324311684, -2.42859224204858, 0.450593709617411],
    [0.0259040424655478, 0.7827717124575296, -0.8086757549230774],
];
const LAB_TO_LMS: M3 = [
    [1.0, 0.3963377773761749, 0.2158037573099136],
    [1.0, -0.1055613458156586, -0.0638541728258133],
    [1.0, -0.0894841775298119, -1.2914855480194092],
];
const LMS_TO_LINEAR_SRGB: M3 = [
    [4.0767416360759583, -3.3077115392580629, 0.2309699031821043],
    [-1.2684379732850315, 2.6097573492876882, -0.341319376002657],
    [-0.0041960761386756, -0.7034186179359362, 1.7076146940746117],
];

/// Per sRGB channel: the (a, b) half-plane where that channel clips first, and the polynomial
/// approximating the maximum saturation there.
const SATURATION_FIT: [([f64; 2], [f64; 5]); 3] = [
    (
        [-1.8817031, -0.80936501],
        [1.19086277, 1.76576728, 0.59662641, 0.75515197, 0.56771245],
    ),
    (
        [1.8144408, -1.19445267],
        [0.73956515, -0.45954404, 0.08285427, 0.12541073, -0.14503204],
    ),
    (
        [0.13110758, 1.81333971],
        [1.35733652, -0.00915799, -1.1513021, -0.50559606, 0.00692167],
    ),
];

const K1: f64 = 0.206;
const K2: f64 = 0.03;
const K3: f64 = (1.0 + K1) / (1.0 + K2);

fn multiply(m: &M3, v: [f64; 3]) -> [f64; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

fn cube(v: f64) -> f64 {
    v.powf(3.0)
}

pub fn oklab_to_okhsl_lightness(x: f64) -> f64 {
    0.5 * (K3 * x - K1 + ((K3 * x - K1).powf(2.0) + 4.0 * K2 * K3 * x).sqrt())
}

fn okhsl_to_oklab_lightness(x: f64) -> f64 {
    (x * x + K1 * x) / (K3 * (x + K2))
}

fn linear_to_srgb(v: f64) -> f64 {
    if v > 0.0031308 {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    } else {
        12.92 * v
    }
}

fn srgb_to_linear(v: f64) -> f64 {
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

fn oklab_to_linear_srgb(lab: [f64; 3]) -> [f64; 3] {
    let lms = multiply(&LAB_TO_LMS, lab);
    multiply(
        &LMS_TO_LINEAR_SRGB,
        [cube(lms[0]), cube(lms[1]), cube(lms[2])],
    )
}

fn linear_srgb_to_oklab(rgb: [f64; 3]) -> [f64; 3] {
    let lms = multiply(&LINEAR_SRGB_TO_LMS, rgb);
    multiply(&LMS_TO_LAB, [lms[0].cbrt(), lms[1].cbrt(), lms[2].cbrt()])
}

pub fn rgb_to_oklab(c: Rgb) -> [f64; 3] {
    linear_srgb_to_oklab([
        srgb_to_linear(c.r / 255.0),
        srgb_to_linear(c.g / 255.0),
        srgb_to_linear(c.b / 255.0),
    ])
}

/// JavaScript's `Math.round`: halves go up.
fn js_round(v: f64) -> f64 {
    (v + 0.5).floor()
}

fn linear_srgb_to_rgb(l: [f64; 3]) -> Rgb {
    let ch = |v: f64| js_round(linear_to_srgb(v).clamp(0.0, 1.0) * 255.0);
    Rgb {
        r: ch(l[0]),
        g: ch(l[1]),
        b: ch(l[2]),
    }
}

fn lms_slopes(a: f64, b: f64) -> [f64; 3] {
    [
        LAB_TO_LMS[0][1] * a + LAB_TO_LMS[0][2] * b,
        LAB_TO_LMS[1][1] * a + LAB_TO_LMS[1][2] * b,
        LAB_TO_LMS[2][1] * a + LAB_TO_LMS[2][2] * b,
    ]
}

fn max_saturation(a: f64, b: f64) -> f64 {
    let channel = SATURATION_FIT
        .iter()
        .enumerate()
        .position(|(i, ([x, y], _))| i == 2 || x * a + y * b > 1.0)
        .unwrap_or(2);
    let [k0, k1, k2, k3, k4] = SATURATION_FIT[channel].1;
    let w = LMS_TO_LINEAR_SRGB[channel];
    let saturation = k0 + k1 * a + k2 * b + k3 * a * a + k4 * a * b;
    let slopes = lms_slopes(a, b);
    let base = [
        1.0 + saturation * slopes[0],
        1.0 + saturation * slopes[1],
        1.0 + saturation * slopes[2],
    ];
    let dot = |v: [f64; 3]| w[0] * v[0] + w[1] * v[1] + w[2] * v[2];
    let f = dot([cube(base[0]), cube(base[1]), cube(base[2])]);
    let f1 = dot([
        3.0 * slopes[0] * base[0].powf(2.0),
        3.0 * slopes[1] * base[1].powf(2.0),
        3.0 * slopes[2] * base[2].powf(2.0),
    ]);
    let f2 = dot([
        6.0 * slopes[0].powf(2.0) * base[0],
        6.0 * slopes[1].powf(2.0) * base[1],
        6.0 * slopes[2].powf(2.0) * base[2],
    ]);
    saturation - (f * f1) / (f1 * f1 - 0.5 * f * f2)
}

fn cusp(a: f64, b: f64) -> [f64; 2] {
    let s = max_saturation(a, b);
    let lin = oklab_to_linear_srgb([1.0, s * a, s * b]);
    let l = (1.0 / lin[0].max(lin[1]).max(lin[2])).cbrt();
    [l, l * s]
}

fn max_chroma(a: f64, b: f64, lightness: f64, cusp: [f64; 2]) -> f64 {
    let [cusp_l, cusp_c] = cusp;
    if lightness <= cusp_l {
        return cusp_c * lightness / cusp_l;
    }
    let t = cusp_c * (lightness - 1.0) / (cusp_l - 1.0);
    let slopes = lms_slopes(a, b);
    let lms = [
        lightness + t * slopes[0],
        lightness + t * slopes[1],
        lightness + t * slopes[2],
    ];
    let cubes = [cube(lms[0]), cube(lms[1]), cube(lms[2])];
    let first = [
        3.0 * slopes[0] * lms[0].powf(2.0),
        3.0 * slopes[1] * lms[1].powf(2.0),
        3.0 * slopes[2] * lms[2].powf(2.0),
    ];
    let second = [
        6.0 * slopes[0].powf(2.0) * lms[0],
        6.0 * slopes[1].powf(2.0) * lms[1],
        6.0 * slopes[2].powf(2.0) * lms[2],
    ];
    let dot = |row: &[f64; 3], v: [f64; 3]| row[0] * v[0] + row[1] * v[1] + row[2] * v[2];
    let mut best = f64::INFINITY;
    for row in LMS_TO_LINEAR_SRGB.iter() {
        let f = dot(row, cubes) - 1.0;
        let f1 = dot(row, first);
        let f2 = dot(row, second);
        let u = f1 / (f1 * f1 - 0.5 * f * f2);
        let step = if u >= 0.0 { -f * u } else { f64::MAX };
        best = best.min(step);
    }
    t + best
}

fn chroma_stops(l: f64, a: f64, b: f64) -> [f64; 3] {
    let peak = cusp(a, b);
    let c_max = max_chroma(a, b, l, peak);
    let k = c_max / (l * (peak[1] / peak[0])).min((1.0 - l) * (peak[1] / (1.0 - peak[0])));
    let mid_s = 0.11516993
        + 1.0
            / (7.4477897
                + 4.1590124 * b
                + a * (-2.19557347
                    + 1.75198401 * b
                    + a * (-2.13704948 - 10.02301043 * b
                        + a * (-4.24894561 + 5.38770819 * b + 4.69891013 * a))));
    let mid_t = 0.11239642
        + 1.0
            / (1.6132032 - 0.68124379 * b
                + a * (0.40370612
                    + 0.90148123 * b
                    + a * (-0.27087943
                        + 0.6122399 * b
                        + a * (0.00299215 - 0.45399568 * b - 0.14661872 * a))));
    let c_mid = 0.9
        * k
        * (1.0 / (1.0 / (l * mid_s).powf(4.0) + 1.0 / ((1.0 - l) * mid_t).powf(4.0)))
            .sqrt()
            .sqrt();
    let c0 = (1.0 / (1.0 / (l * 0.4).powf(2.0) + 1.0 / ((1.0 - l) * 0.8).powf(2.0))).sqrt();
    [c0, c_mid, c_max]
}

/// OKHSL to sRGB channels, rounded and clipped.
pub fn okhsl_to_rgb(hue: f64, saturation: f64, lightness: f64) -> Rgb {
    let l = okhsl_to_oklab_lightness(lightness);
    let mut lab = [l, 0.0, 0.0];
    if l > 0.0 && l < 1.0 && saturation > 0.0 {
        let angle = 2.0 * std::f64::consts::PI * (((hue % 360.0) + 360.0) % 360.0) / 360.0;
        let a = angle.cos();
        let b = angle.sin();
        let [c0, c_mid, c_max] = chroma_stops(l, a, b);
        let chroma = if saturation < 0.8 {
            let t = 1.25 * saturation;
            let k1 = 0.8 * c0;
            (t * k1) / (1.0 - (1.0 - k1 / c_mid) * t)
        } else {
            let t = 5.0 * (saturation - 0.8);
            let k1 = (0.2 * c_mid.powf(2.0) * 1.25f64.powf(2.0)) / c0;
            c_mid + (t * k1) / (1.0 - (1.0 - k1 / (c_max - c_mid)) * t)
        };
        lab = [l, chroma * a, chroma * b];
    }
    linear_srgb_to_rgb(oklab_to_linear_srgb(lab))
}

#[derive(Clone, Copy, Debug)]
pub struct Okhsl {
    pub h: f64,
    pub s: f64,
    pub l: f64,
}

pub fn rgb_to_okhsl(c: Rgb) -> Okhsl {
    let [l, lab_a, lab_b] = rgb_to_oklab(c);
    let chroma = lab_a.hypot(lab_b);
    let lightness = oklab_to_okhsl_lightness(l);
    if chroma < 1e-9 || lightness <= 0.0 || lightness >= 1.0 {
        return Okhsl {
            h: 0.0,
            s: 0.0,
            l: lightness,
        };
    }
    let hue = ((lab_b.atan2(lab_a) * 180.0) / std::f64::consts::PI + 360.0) % 360.0;
    let [c0, c_mid, c_max] = chroma_stops(l, lab_a / chroma, lab_b / chroma);
    let saturation = if chroma < c_mid {
        let k1 = 0.8 * c0;
        0.8 * (chroma / (k1 + (1.0 - k1 / c_mid) * chroma))
    } else {
        let k1 = (0.2 * c_mid.powf(2.0) * 1.25f64.powf(2.0)) / c0;
        let offset = chroma - c_mid;
        0.8 + 0.2 * (offset / (k1 + (1.0 - k1 / (c_max - c_mid)) * offset))
    };
    Okhsl {
        h: hue,
        s: saturation.clamp(0.0, 1.0),
        l: lightness,
    }
}

// ------------------------------------------------------------------------------------------
// colors.js
// ------------------------------------------------------------------------------------------

fn is_in_gamut(l: [f64; 3]) -> bool {
    let eps = 1e-7;
    l.iter().all(|c| *c >= -eps && *c <= 1.0 + eps)
}

/// OKLCH to sRGB with the hue fixed and chroma bisected down until the colour fits.
pub fn oklch_to_rgb(l: f64, c: f64, h: f64) -> Rgb {
    let radians = h * std::f64::consts::PI / 180.0;
    let (cos, sin) = (radians.cos(), radians.sin());
    let at = |chroma: f64| oklab_to_linear_srgb([l, chroma * cos, chroma * sin]);
    let direct = at(c);
    if is_in_gamut(direct) {
        return linear_srgb_to_rgb(direct);
    }
    let mut linear = at(0.0);
    let (mut low, mut high) = (0.0, c);
    for _ in 0..20 {
        let chroma = (low + high) / 2.0;
        let cand = at(chroma);
        if is_in_gamut(cand) {
            low = chroma;
            linear = cand;
        } else {
            high = chroma;
        }
    }
    linear_srgb_to_rgb(linear)
}

pub fn rgb_to_oklch(c: Rgb) -> (f64, f64, f64) {
    let [l, a, b] = rgb_to_oklab(c);
    (
        l,
        a.hypot(b),
        ((b.atan2(a) * 180.0) / std::f64::consts::PI + 360.0) % 360.0,
    )
}

fn oklab_lightness(c: Rgb) -> f64 {
    rgb_to_oklab(c)[0]
}

// ------------------------------------------------------------------------------------------
// system-theme.js
// ------------------------------------------------------------------------------------------

struct Family {
    hue: f64,
    min: f64,
    max: f64,
    slot: usize,
}

fn family(name: &str) -> Family {
    let (hue, min, max, slot) = match name {
        "neutral" => (231.49, 0.02, 0.08, 8),
        "blue" => (231.49, 0.1, 0.68, 4),
        "green" => (158.68, 0.1, 0.76, 2),
        "red" => (20.0, 0.1, 0.92, 1),
        "yellow" => (82.36, 0.5, 1.0, 3),
        "orange" => (52.0, 0.12, 0.85, 3),
        "violet" => (295.0, 0.2, 0.6, 5),
        "calamine" => (202.43, 0.1, 0.74, 6),
        "thinkingSlate" => (231.49, 0.08, 0.2, 4),
        "thinkingBlue" => (231.49, 0.2, 0.45, 4),
        "thinkingPeriwinkle" => (263.25, 0.3, 0.6, 6),
        "thinkingViolet" => (295.0, 0.4, 0.75, 5),
        "thinkingMagenta" => (337.5, 0.5, 0.85, 13),
        "thinkingRed" => (20.0, 0.95, 1.0, 1),
        other => panic!("unknown colour family {other}"),
    };
    Family {
        hue,
        min,
        max,
        slot,
    }
}

/// Insertion order of Pi's `TOKEN_FAMILIES`, which fixes the order of the solve.
const TOKEN_FAMILIES: &[(&str, &str)] = &[
    ("selectedBg", "blue"),
    ("searchMatchBg", "orange"),
    ("userMessageBg", "blue"),
    ("customMessageBg", "violet"),
    ("toolPendingBg", "neutral"),
    ("toolSuccessBg", "green"),
    ("toolErrorBg", "red"),
    ("text", "neutral"),
    ("userMessageText", "neutral"),
    ("customMessageText", "neutral"),
    ("toolTitle", "neutral"),
    ("syntaxOperator", "neutral"),
    ("syntaxPunctuation", "neutral"),
    ("muted", "neutral"),
    ("dim", "neutral"),
    ("thinkingText", "neutral"),
    ("toolOutput", "neutral"),
    ("mdLinkUrl", "neutral"),
    ("mdQuote", "neutral"),
    ("mdQuoteBorder", "neutral"),
    ("mdHr", "neutral"),
    ("mdCodeBlockBorder", "neutral"),
    ("toolDiffContext", "neutral"),
    ("syntaxComment", "neutral"),
    ("scrollbarTrack", "neutral"),
    ("scrollbarThumb", "neutral"),
    ("searchMatchText", "neutral"),
    ("borderMuted", "neutral"),
    ("accent", "violet"),
    ("borderAccent", "violet"),
    ("customMessageLabel", "violet"),
    ("mdCode", "violet"),
    ("mdListBullet", "violet"),
    ("syntaxType", "violet"),
    ("border", "blue"),
    ("mdLink", "blue"),
    ("syntaxKeyword", "blue"),
    ("syntaxVariable", "calamine"),
    ("success", "green"),
    ("mdCodeBlock", "green"),
    ("toolDiffAdded", "green"),
    ("bashMode", "green"),
    ("syntaxNumber", "green"),
    ("error", "red"),
    ("toolDiffRemoved", "red"),
    ("warning", "yellow"),
    ("mdHeading", "yellow"),
    ("syntaxFunction", "yellow"),
    ("syntaxString", "orange"),
    ("thinkingOff", "neutral"),
    ("thinkingMinimal", "thinkingSlate"),
    ("thinkingLow", "thinkingBlue"),
    ("thinkingMedium", "thinkingPeriwinkle"),
    ("thinkingHigh", "thinkingViolet"),
    ("thinkingXhigh", "thinkingMagenta"),
    ("thinkingMax", "thinkingRed"),
];

fn token_family(token: &str) -> &'static str {
    TOKEN_FAMILIES
        .iter()
        .find(|(t, _)| *t == token)
        .map(|(_, f)| *f)
        .unwrap_or_else(|| panic!("unknown token {token}"))
}

fn token_slot(token: &str) -> Option<usize> {
    match token {
        "syntaxString" => Some(2),
        "syntaxNumber" => Some(5),
        "searchMatchBg" => Some(3),
        _ => None,
    }
}

struct Curve {
    coefficients: [f64; 6],
    reachable: [f64; 2],
}

const LEVELS: &[(&str, Curve, Curve)] = &[
    (
        "panel",
        Curve {
            coefficients: [0.29131, -0.39746, 2.33185, -0.85524, -1.2076, 0.86276],
            reachable: [0.0, 0.979],
        },
        Curve {
            coefficients: [-3.74073, 27.94549, -78.44258, 112.6798, -79.60015, 22.11277],
            reachable: [0.348, 1.0],
        },
    ),
    (
        "track",
        Curve {
            coefficients: [0.39028, -0.23015, 0.83573, 2.43829, -4.38292, 2.01582],
            reachable: [0.0, 0.946],
        },
        Curve {
            coefficients: [
                -5.24921, 38.37322, -107.28833, 152.10005, -106.17127, 29.18061,
            ],
            reachable: [0.368, 1.0],
        },
    ),
    (
        "thinking0",
        Curve {
            coefficients: [0.52988, -0.05809, -0.30924, 4.63567, -6.52933, 2.89108],
            reachable: [0.0, 0.873],
        },
        Curve {
            coefficients: [
                -28.27749, 182.85284, -469.62416, 603.15916, -384.59976, 97.35147,
            ],
            reachable: [0.51, 1.0],
        },
    ),
    (
        "thinking1",
        Curve {
            coefficients: [0.55278, -0.03667, -0.45659, 4.95347, -6.90265, 3.0706],
            reachable: [0.0, 0.858],
        },
        Curve {
            coefficients: [
                -37.10484, 235.86282, -596.62344, 754.3633, -474.00763, 118.3551,
            ],
            reachable: [0.535, 1.0],
        },
    ),
    (
        "thinking2",
        Curve {
            coefficients: [0.57486, -0.01765, -0.58987, 5.25227, -7.27175, 3.25532],
            reachable: [0.0, 0.842],
        },
        Curve {
            coefficients: [
                -59.89653, 377.05024, -945.07843, 1182.03145, -734.96375, 181.68658,
            ],
            reachable: [0.556, 1.0],
        },
    ),
    (
        "thinking3",
        Curve {
            coefficients: [0.59621, -0.00062, -0.71148, 5.53588, -7.6392, 3.44606],
            reachable: [0.0, 0.827],
        },
        Curve {
            coefficients: [
                -72.07122,
                445.84082,
                -1099.57352,
                1353.88793,
                -829.53392,
                202.26164,
            ],
            reachable: [0.58, 1.0],
        },
    ),
    (
        "thinking4",
        Curve {
            coefficients: [0.61691, 0.01462, -0.82288, 5.80651, -8.00641, 3.64333],
            reachable: [0.0, 0.811],
        },
        Curve {
            coefficients: [
                -110.14338,
                674.21488,
                -1645.75941,
                2004.32367,
                -1215.15899,
                293.3183,
            ],
            reachable: [0.6, 1.0],
        },
    ),
    (
        "thinking5",
        Curve {
            coefficients: [0.63702, 0.02826, -0.92498, 6.06465, -8.37246, 3.84651],
            reachable: [0.0, 0.795],
        },
        Curve {
            coefficients: [
                -175.47701,
                1063.54495,
                -2570.70594,
                3098.80776,
                -1860.15527,
                444.76392,
            ],
            reachable: [0.62, 1.0],
        },
    ),
    (
        "thinking6",
        Curve {
            coefficients: [0.65658, 0.04044, -1.01835, 6.30989, -8.73529, 4.05439],
            reachable: [0.0, 0.779],
        },
        Curve {
            coefficients: [
                -183.81712,
                1094.70055,
                -2602.68539,
                3088.71276,
                -1826.91131,
                430.75931,
            ],
            reachable: [0.643, 1.0],
        },
    ),
    (
        "subtle",
        Curve {
            coefficients: [0.56762, -0.02475, -0.5383, 5.12628, -7.10931, 3.17324],
            reachable: [0.0, 0.848],
        },
        Curve {
            coefficients: [
                -232.85459,
                1376.54473,
                -3249.11801,
                3827.91186,
                -2248.29472,
                526.55751,
            ],
            reachable: [0.657, 1.0],
        },
    ),
    (
        "thumb",
        Curve {
            coefficients: [0.60323, 0.00278, -0.73328, 5.57157, -7.68067, 3.46933],
            reachable: [0.0, 0.823],
        },
        Curve {
            coefficients: [
                -82.89897,
                511.01355,
                -1255.98095,
                1540.76821,
                -940.68087,
                228.58523,
            ],
            reachable: [0.586, 1.0],
        },
    ),
    (
        "readable",
        Curve {
            coefficients: [0.66937, 0.04704, -1.06871, 6.43941, -8.9332, 4.17229],
            reachable: [0.0, 0.77],
        },
        Curve {
            coefficients: [
                -1554.52576,
                8733.56817,
                -19604.93507,
                21977.72696,
                -12300.99599,
                2749.81288,
            ],
            reachable: [0.751, 1.0],
        },
    ),
    (
        "emphasis",
        Curve {
            coefficients: [0.7303, 0.07695, -1.31626, 7.1681, -10.14436, 4.92846],
            reachable: [0.0, 0.712],
        },
        Curve {
            coefficients: [
                -4948.31942,
                26870.91986,
                -58334.48399,
                63280.17197,
                -34298.01053,
                7430.30146,
            ],
            reachable: [0.811, 1.0],
        },
    ),
    (
        "textOnPanel",
        Curve {
            coefficients: [0.86713, 0.05232, -0.89428, 4.79014, -5.5432, 1.75023],
            reachable: [0.0, 0.542],
        },
        Curve {
            coefficients: [
                -8570.89457,
                43954.60805,
                -90084.00702,
                92220.6791,
                -47152.15802,
                9632.27113,
            ],
            reachable: [0.867, 1.0],
        },
    ),
    (
        "text",
        Curve {
            coefficients: [0.89242, 0.02311, -0.44862, 2.34417, -0.06084, -2.63844],
            reachable: [0.0, 0.5],
        },
        Curve {
            coefficients: [
                -2004.67048,
                6664.47299,
                -6060.70202,
                -1792.61209,
                5133.82359,
                -1939.85583,
            ],
            reachable: [0.894, 1.0],
        },
    ),
];

const TOOL_PANELS: &[&str] = &["toolPendingBg", "toolSuccessBg", "toolErrorBg"];
const MESSAGE_PANELS: &[&str] = &["userMessageBg", "customMessageBg"];
const PANELS: &[&str] = &[
    "userMessageBg",
    "toolPendingBg",
    "toolSuccessBg",
    "toolErrorBg",
    "selectedBg",
    "searchMatchBg",
    "customMessageBg",
];
const THINKING: &[&str] = &[
    "thinkingOff",
    "thinkingMinimal",
    "thinkingLow",
    "thinkingMedium",
    "thinkingHigh",
    "thinkingXhigh",
    "thinkingMax",
];
const THINKING_LEVELS: &[&str] = &[
    "thinking0",
    "thinking1",
    "thinking2",
    "thinking3",
    "thinking4",
    "thinking5",
    "thinking6",
];
const FOREGROUND_TOKENS: &[&str] = &["text", "userMessageText", "toolTitle"];

struct Rule {
    token: &'static str,
    on: Vec<&'static str>,
    level: &'static str,
}

fn rules() -> Vec<Rule> {
    let mut r: Vec<Rule> = Vec::new();
    let each =
        |r: &mut Vec<Rule>, tokens: &[&'static str], on: Vec<&'static str>, level: &'static str| {
            for t in tokens {
                r.push(Rule {
                    token: t,
                    on: on.clone(),
                    level,
                });
            }
        };
    let cat = |parts: &[&[&'static str]]| -> Vec<&'static str> {
        parts.iter().flat_map(|p| p.iter().copied()).collect()
    };
    each(&mut r, PANELS, vec!["background"], "panel");
    each(&mut r, &["text"], vec!["background"], "text");
    each(&mut r, &["text"], vec!["selectedBg"], "textOnPanel");
    each(
        &mut r,
        &["userMessageText"],
        vec!["userMessageBg"],
        "textOnPanel",
    );
    each(&mut r, &["toolTitle"], TOOL_PANELS.to_vec(), "textOnPanel");
    each(
        &mut r,
        &["accent", "success", "error", "warning"],
        cat(&[&["background", "selectedBg"], TOOL_PANELS]),
        "readable",
    );
    each(
        &mut r,
        &["muted"],
        cat(&[
            &["background", "selectedBg", "customMessageBg"],
            TOOL_PANELS,
        ]),
        "readable",
    );
    each(
        &mut r,
        &["dim"],
        cat(&[
            &["background", "selectedBg", "customMessageBg"],
            TOOL_PANELS,
        ]),
        "subtle",
    );
    each(&mut r, &["thinkingText"], vec!["background"], "readable");
    each(
        &mut r,
        &["customMessageText"],
        cat(&[&["customMessageBg"], TOOL_PANELS]),
        "readable",
    );
    each(
        &mut r,
        &["customMessageLabel"],
        cat(&[
            &["background", "customMessageBg", "selectedBg"],
            TOOL_PANELS,
        ]),
        "readable",
    );
    each(
        &mut r,
        &["toolOutput"],
        cat(&[&["background"], TOOL_PANELS]),
        "readable",
    );
    each(
        &mut r,
        &[
            "mdHeading",
            "mdLink",
            "mdLinkUrl",
            "mdCode",
            "mdQuote",
            "mdCodeBlockBorder",
            "mdListBullet",
        ],
        cat(&[&["background"], MESSAGE_PANELS]),
        "readable",
    );
    each(
        &mut r,
        &["mdCodeBlock"],
        cat(&[&["background"], MESSAGE_PANELS, TOOL_PANELS]),
        "readable",
    );
    each(
        &mut r,
        &["toolDiffAdded", "toolDiffRemoved", "toolDiffContext"],
        cat(&[&["background"], TOOL_PANELS]),
        "readable",
    );
    each(
        &mut r,
        &[
            "syntaxComment",
            "syntaxKeyword",
            "syntaxFunction",
            "syntaxVariable",
            "syntaxString",
            "syntaxNumber",
            "syntaxType",
            "syntaxOperator",
            "syntaxPunctuation",
        ],
        cat(&[&["background"], MESSAGE_PANELS, TOOL_PANELS]),
        "readable",
    );
    each(
        &mut r,
        &["searchMatchText"],
        vec!["searchMatchBg"],
        "readable",
    );
    each(
        &mut r,
        &["bashMode", "border", "borderAccent"],
        vec!["background"],
        "readable",
    );
    each(&mut r, &["borderMuted"], vec!["background"], "subtle");
    each(
        &mut r,
        &["mdQuoteBorder", "mdHr"],
        cat(&[&["background"], MESSAGE_PANELS, TOOL_PANELS]),
        "readable",
    );
    each(&mut r, &["scrollbarTrack"], vec!["background"], "track");
    each(&mut r, &["scrollbarThumb"], vec!["scrollbarTrack"], "thumb");
    for (i, t) in THINKING.iter().enumerate() {
        r.push(Rule {
            token: t,
            on: vec!["background"],
            level: THINKING_LEVELS[i],
        });
    }
    r
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Appearance {
    Dark,
    Light,
}

impl Appearance {
    fn lighter(self) -> bool {
        self == Appearance::Dark
    }
}

fn curve(level: &str, a: Appearance) -> &'static Curve {
    let (_, dark, light) = LEVELS
        .iter()
        .find(|(n, _, _)| *n == level)
        .unwrap_or_else(|| panic!("unknown level {level}"));
    if a == Appearance::Dark {
        dark
    } else {
        light
    }
}

fn level_target(level: &str, a: Appearance, surface_l: f64) -> Option<f64> {
    let c = curve(level, a);
    if surface_l < c.reachable[0] || surface_l > c.reachable[1] {
        return None;
    }
    Some(
        c.coefficients
            .iter()
            .enumerate()
            .fold(0.0, |sum, (power, k)| {
                sum + k * surface_l.powf(power as f64)
            }),
    )
}

pub fn relative_luminance(c: Rgb) -> f64 {
    let lin = |ch: f64| {
        let v = ch / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b)
}

pub fn wcag_contrast(a: Rgb, b: Rgb) -> f64 {
    let (x, y) = (relative_luminance(a), relative_luminance(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

const TEXT_MINIMUM_WCAG_CONTRAST: f64 = 4.5;

/// Whether a terminal is dark or light, from its reported colours.
pub fn terminal_appearance(background: Rgb, foreground: Option<Rgb>) -> Appearance {
    let white = Rgb::new(255, 255, 255);
    let black = Rgb::new(0, 0, 0);
    let white_contrast = wcag_contrast(white, background);
    let black_contrast = wcag_contrast(black, background);
    if let Some(fg) = foreground {
        let fl = oklab_lightness(fg);
        let bl = oklab_lightness(background);
        if (fl - bl).abs() > 0.05 {
            let appearance = if fl > bl {
                Appearance::Dark
            } else {
                Appearance::Light
            };
            let best = if appearance == Appearance::Dark {
                white_contrast
            } else {
                black_contrast
            };
            if best >= TEXT_MINIMUM_WCAG_CONTRAST {
                return appearance;
            }
        }
    }
    if white_contrast >= black_contrast {
        Appearance::Dark
    } else {
        Appearance::Light
    }
}

fn bell_weight(lightness: f64) -> f64 {
    let g = |x: f64| (-((x - 0.5).powf(2.0)) / (2.0 * 0.25f64.powf(2.0))).exp();
    (g(lightness) - g(0.0)) / (1.0 - g(0.0))
}

fn saturation_curve(f: &Family, lightness: f64) -> f64 {
    let floor = if f.max > 0.0 { f.min / f.max } else { 1.0 };
    floor + (1.0 - floor) * bell_weight(lightness)
}

#[derive(Clone, Copy)]
struct Source {
    h: f64,
    s: f64,
    l: f64,
    chroma: f64,
}

fn source_of(c: Rgb) -> Source {
    let k = rgb_to_okhsl(c);
    Source {
        h: k.h,
        s: k.s,
        l: k.l,
        chroma: rgb_to_oklch(c).1,
    }
}

fn anchored(source: Source, f: &Family, lightness: f64, saturation: f64) -> Rgb {
    let anchor = saturation_curve(f, source.l);
    let falloff = if anchor > 0.0 {
        (saturation_curve(f, lightness) / anchor).min(1.0)
    } else {
        1.0
    };
    let color = okhsl_to_rgb(
        source.h,
        (source.s * falloff * saturation).clamp(0.0, 1.0),
        lightness.clamp(0.0, 1.0),
    );
    let cap = source.chroma * falloff * saturation;
    let (l, c, _) = rgb_to_oklch(color);
    if c <= cap {
        color
    } else {
        oklch_to_rgb(l.clamp(0.0, 1.0), cap, source.h)
    }
}

fn with_text_contrast(color: Rgb, surfaces: &[Rgb], lighter: bool) -> Rgb {
    let meets = |c: Rgb| {
        surfaces
            .iter()
            .all(|s| wcag_contrast(c, *s) >= TEXT_MINIMUM_WCAG_CONTRAST)
    };
    if meets(color) {
        return color;
    }
    let Okhsl { h, s, l } = rgb_to_okhsl(color);
    let at = |lightness: f64| okhsl_to_rgb(h, s, lightness.clamp(0.0, 1.0));
    let extreme = if lighter { 1.0 } else { 0.0 };
    if !meets(at(extreme)) {
        return at(extreme);
    }
    let (mut low, mut high) = (l, extreme);
    for _ in 0..20 {
        let middle = (low + high) / 2.0;
        if meets(at(middle)) {
            high = middle;
        } else {
            low = middle;
        }
    }
    at(high)
}

/// What the terminal answered to OSC 10, OSC 11 and OSC 4;0..15.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Reported {
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
    /// Only set when all 16 colours arrived.
    pub palette: Option<[Rgb; 16]>,
}

/// One generated token.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Gen {
    /// The terminal's own default colour (`""`).
    Default,
    Rgb(Rgb),
    /// An ANSI palette slot, rendered by the terminal.
    Index(u8),
}

#[derive(Clone, Debug)]
pub struct Generated {
    pub colors: HashMap<&'static str, Gen>,
    /// Tokens drawn faint (SGR 2) on top of their colour.
    pub dim: Vec<&'static str>,
    pub appearance: Appearance,
}

/// `generateSystemThemeColors`. `saturation` is 1 except while Pi waits for the terminal (0).
pub fn generate(reported: &Reported, saturation: f64, hint: Appearance) -> Generated {
    let saturation = saturation.clamp(0.0, 1.0);
    let Some(background) = reported.background else {
        return indexed_colors(saturation, hint);
    };
    let foreground = reported.foreground;
    let palette: Option<Vec<Source>> = reported
        .palette
        .map(|p| p.iter().map(|c| source_of(*c)).collect());
    let appearance = terminal_appearance(background, foreground);
    let lighter = appearance.lighter();
    let extreme = if lighter { 1.0 } else { 0.0 };
    let background_l = oklab_lightness(background);
    let rules = rules();

    let paint = |token: &str, oklab_l: f64| -> Rgb {
        let lightness = oklab_to_okhsl_lightness(oklab_l);
        let fam = family(token_family(token));
        match &palette {
            None => okhsl_to_rgb(
                fam.hue,
                ((fam.min + (fam.max - fam.min) * bell_weight(lightness)) * saturation)
                    .clamp(0.0, 1.0),
                lightness.clamp(0.0, 1.0),
            ),
            Some(p) => anchored(
                p[token_slot(token).unwrap_or(fam.slot)],
                &fam,
                lightness,
                saturation,
            ),
        }
    };

    let target = |level: &str, surface_l: f64, t: f64| -> Option<f64> {
        let reached = level_target(level, appearance, surface_l);
        if reached.is_none() && t == 0.0 {
            return None;
        }
        let distance = reached.unwrap_or(extreme) - surface_l;
        let floor_level = if lighter { "readable" } else { "subtle" };
        let floor = level_target(floor_level, appearance, surface_l).unwrap_or(extreme) - surface_l;
        let compressed = if distance.abs() > floor.abs() {
            distance - (distance - floor) * t.min(1.0)
        } else {
            distance
        };
        Some(surface_l + compressed * (1.0 - (t - 1.0).max(0.0)))
    };

    let extreme_text = if lighter {
        Rgb::new(255, 255, 255)
    } else {
        Rgb::new(0, 0, 0)
    };
    let readable = |c: Rgb| wcag_contrast(extreme_text, c) >= TEXT_MINIMUM_WCAG_CONTRAST;
    let limit_panel = |token: &str, l: f64| -> Rgb {
        let color = paint(token, l);
        if readable(color) {
            return color;
        }
        let (mut low, mut high) = (background_l, l);
        for _ in 0..20 {
            let middle = (low + high) / 2.0;
            if readable(paint(token, middle)) {
                low = middle;
            } else {
                high = middle;
            }
        }
        paint(token, low)
    };

    // Tokens in dependency order: every surface before the tokens drawn on it.
    let mut order: Vec<&'static str> = Vec::new();
    fn visit(token: &'static str, order: &mut Vec<&'static str>, rules: &[Rule]) {
        if order.contains(&token) {
            return;
        }
        for rule in rules {
            if rule.token != token {
                continue;
            }
            for surface in &rule.on {
                if *surface != "background" {
                    visit(surface, order, rules);
                }
            }
        }
        order.push(token);
    }
    for rule in &rules {
        visit(rule.token, &mut order, &rules);
    }

    let solve = |t: f64| -> Option<HashMap<&'static str, Rgb>> {
        let mut colors: HashMap<&'static str, Rgb> = HashMap::new();
        colors.insert("background", background);
        for token in &order {
            let mut targets = Vec::new();
            for rule in &rules {
                if rule.token != *token {
                    continue;
                }
                for surface in &rule.on {
                    let sl = oklab_lightness(*colors.get(surface).unwrap_or(&background));
                    let v = target(rule.level, sl, t)?;
                    if !(0.0..=1.0).contains(&v) {
                        return None;
                    }
                    targets.push(v);
                }
            }
            let l = if lighter {
                targets.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
            } else {
                targets.iter().cloned().fold(f64::INFINITY, f64::min)
            };
            let c = if PANELS.contains(token) {
                limit_panel(token, l)
            } else {
                paint(token, l)
            };
            colors.insert(token, c);
        }
        Some(colors)
    };

    let mut relaxation = 0.0;
    let mut colors = solve(0.0);
    if colors.is_none() {
        // Mid-gray backgrounds cannot fit every level: relax as little as possible.
        let (mut low, mut high) = (0.0f64, 2.0f64);
        colors = solve(high);
        for _ in 0..20 {
            let middle = (low + high) / 2.0;
            match solve(middle) {
                Some(attempt) => {
                    high = middle;
                    colors = Some(attempt);
                }
                None => low = middle,
            }
        }
        relaxation = high;
    }
    let solved = colors.unwrap_or_default();
    let surfaces_of = |token: &str| -> Vec<Rgb> {
        rules
            .iter()
            .filter(|r| r.token == token)
            .flat_map(|r| r.on.iter().map(|s| *solved.get(s).unwrap_or(&background)))
            .collect()
    };

    let mut result: HashMap<&'static str, Gen> = HashMap::new();
    for (token, _) in TOKEN_FAMILIES {
        result.insert(
            token,
            solved.get(token).map_or(Gen::Default, |c| Gen::Rgb(*c)),
        );
    }
    for token in FOREGROUND_TOKENS {
        let surfaces = surfaces_of(token);
        let mut text = solved.get(token).copied();
        if let Some(fg) = foreground {
            let targets: Vec<Option<f64>> = surfaces
                .iter()
                .map(|s| target("emphasis", oklab_lightness(*s), relaxation))
                .collect();
            if targets
                .iter()
                .all(|v| v.is_some_and(|v| (0.0..=1.0).contains(&v)))
            {
                let ts: Vec<f64> = targets.iter().map(|v| v.unwrap_or(0.0)).collect();
                let needed = if lighter {
                    ts.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
                } else {
                    ts.iter().cloned().fold(f64::INFINITY, f64::min)
                };
                let fl = oklab_lightness(fg);
                if if lighter { fl >= needed } else { fl <= needed } {
                    result.insert(token, Gen::Default);
                    continue;
                }
                text = Some(anchored(
                    source_of(fg),
                    &family("neutral"),
                    oklab_to_okhsl_lightness(needed),
                    saturation,
                ));
            }
        }
        if let Some(t) = text {
            result.insert(token, Gen::Rgb(with_text_contrast(t, &surfaces, lighter)));
        }
    }
    Generated {
        colors: result,
        dim: Vec::new(),
        appearance,
    }
}

/// The tier for a terminal that reported nothing: ANSI slots and the default colours, which the
/// terminal renders itself. Neutral tokens below body text are faint, panels are unpainted.
fn indexed_colors(saturation: f64, appearance: Appearance) -> Generated {
    let mut colors = HashMap::new();
    let mut dim = Vec::new();
    for (token, fam) in TOKEN_FAMILIES {
        if PANELS.contains(token) {
            colors.insert(*token, Gen::Default);
            continue;
        }
        let neutral = *fam == "neutral";
        colors.insert(
            *token,
            if !neutral && saturation > 0.0 {
                Gen::Index(token_slot(token).unwrap_or(family(fam).slot) as u8)
            } else {
                Gen::Default
            },
        );
        if neutral && !FOREGROUND_TOKENS.contains(token) {
            dim.push(*token);
        }
    }
    Generated {
        colors,
        dim,
        appearance,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn hex(s: &str) -> Rgb {
        let v = |i: usize| u8::from_str_radix(&s[i..i + 2], 16).unwrap();
        Rgb::new(v(1), v(3), v(5))
    }

    /// Compare a generated theme with what Pi's generator printed, token by token.
    fn check(name: &str, reported: &Reported, want: &Value) {
        let got = generate(reported, 1.0, Appearance::Dark);
        let appearance = want["appearance"].as_str().unwrap();
        assert_eq!(
            got.appearance == Appearance::Dark,
            appearance == "dark",
            "{name}: appearance"
        );
        let colors = want["colors"].as_object().unwrap();
        assert_eq!(colors.len(), got.colors.len(), "{name}: token count");
        for (token, w) in colors {
            let g = got.colors[token.as_str()];
            match w {
                Value::String(s) if s.is_empty() => assert_eq!(g, Gen::Default, "{name}: {token}"),
                Value::String(s) => assert_eq!(
                    g,
                    Gen::Rgb(hex(s)),
                    "{name}: {token}: want {s}, got {}",
                    match g {
                        Gen::Rgb(c) => c.hex(),
                        other => format!("{other:?}"),
                    }
                ),
                Value::Number(n) => {
                    assert_eq!(g, Gen::Index(n.as_u64().unwrap() as u8), "{name}: {token}")
                }
                other => panic!("{name}: {token}: {other}"),
            }
        }
        let dim: Vec<&str> = want["dim"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        let mut got_dim = got.dim.clone();
        got_dim.sort_unstable();
        let mut want_dim = dim;
        want_dim.sort_unstable();
        assert_eq!(got_dim, want_dim, "{name}: dim");
    }

    #[test]
    fn the_five_spec_vectors_and_the_nothing_reported_tier_match_pi() {
        let all: Value =
            serde_json::from_str(include_str!("../../tuikit/themes-pi/system-golden.json"))
                .unwrap();
        for (name, want) in all.as_object().unwrap() {
            // names read `<appearance>-<bg>-<fg>`; the last has nothing reported
            let parts: Vec<&str> = name.split('-').collect();
            let reported = if parts.len() == 3 {
                Reported {
                    background: Some(hex(&format!("#{}", parts[1]))),
                    foreground: Some(hex(&format!("#{}", parts[2]))),
                    palette: None,
                }
            } else {
                Reported::default()
            };
            check(name, &reported, want);
        }
    }

    #[test]
    fn palette_tier_midgray_relaxation_and_random_terminals_match_pi() {
        let all: Value = serde_json::from_str(include_str!(
            "../../tuikit/themes-pi/system-golden-more.json"
        ))
        .unwrap();
        let cases = all.as_array().unwrap();
        assert!(cases.len() >= 30);
        let mut with_palette = 0;
        for c in cases {
            let name = c["name"].as_str().unwrap();
            let opt = |k: &str| c[k].as_str().map(hex);
            let palette = c["palette"].as_array().map(|p| {
                with_palette += 1;
                let mut a = [Rgb::new(0, 0, 0); 16];
                for (i, v) in p.iter().enumerate() {
                    a[i] = hex(v.as_str().unwrap());
                }
                a
            });
            let reported = Reported {
                background: opt("bg"),
                foreground: opt("fg"),
                palette,
            };
            check(name, &reported, &c["out"]);
        }
        assert!(
            with_palette >= 8,
            "the palette tier needs vectors of its own"
        );
    }

    #[test]
    fn spec_golden_values_named_in_the_spec() {
        let r = Reported {
            background: Some(Rgb::new(0, 0, 0)),
            foreground: Some(Rgb::new(0xe5, 0xe5, 0xe7)),
            palette: None,
        };
        let g = generate(&r, 1.0, Appearance::Dark);
        assert_eq!(g.colors["userMessageBg"], Gen::Rgb(hex("#1d2e37")));
        assert_eq!(g.colors["toolSuccessBg"], Gen::Rgb(hex("#1e3026")));
        assert_eq!(g.colors["muted"], Gen::Rgb(hex("#969fa4")));
        assert_eq!(g.colors["text"], Gen::Default);
    }

    #[test]
    fn a_pending_terminal_renders_in_grayscale() {
        // while the query is in flight Pi draws every non-neutral token in the default colour
        let g = generate(&Reported::default(), 0.0, Appearance::Dark);
        assert!(g.colors.values().all(|c| *c == Gen::Default));
        assert!(!g.dim.is_empty());
    }
}
