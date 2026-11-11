//! Where the other screens are, and the geometry of moving between them.
//!
//! `interdesk.toml`, in the crossdrop data directory:
//!
//! ```toml
//! [[neighbor]]
//! device = "bravo"   # crossdrop device name or id prefix
//! side = "right"     # left | right | top | bottom
//! id = "VCZU-QFPU-…" # optional: the device id, kept by `interdesk add`
//! from = 50          # optional: the part of this screen's edge that the
//! to = 100           # neighbour faces, in % (0 = left or top end)
//! ```
//!
//! `from`/`to` line up screens of different sizes: a laptop facing the
//! lower half of a tall monitor's right edge is `from = 50`. The cursor
//! crosses only there, and comes back there.

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl std::str::FromStr for Side {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "left" | "gauche" => Side::Left,
            "right" | "droite" => Side::Right,
            "top" | "up" | "above" | "haut" => Side::Top,
            "bottom" | "down" | "below" | "bas" => Side::Bottom,
            _ => bail!("unknown side {s:?}: use left, right, top or bottom"),
        })
    }
}

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Side::Left => "left",
            Side::Right => "right",
            Side::Top => "top",
            Side::Bottom => "bottom",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Neighbor {
    /// Name (or id prefix) given when the device was placed, for display.
    pub device: String,
    pub side: Side,
    /// The device's id, when known: still right after the device is renamed,
    /// and unambiguous when two devices share a name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Start of the shared part of this screen's edge, in %.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub from: u8,
    /// End of the shared part of this screen's edge, in %.
    #[serde(default = "full", skip_serializing_if = "is_full")]
    pub to: u8,
}

fn is_zero(v: &u8) -> bool {
    *v == 0
}

fn full() -> u8 {
    100
}

fn is_full(v: &u8) -> bool {
    *v == 100
}

impl Neighbor {
    /// The shared part of this screen's edge, as fractions of its length.
    pub fn edge(&self) -> (f64, f64) {
        (f64::from(self.from) / 100.0, f64::from(self.to) / 100.0)
    }

    /// How to ask the daemon for this device: its id when known.
    pub fn target(&self) -> &str {
        self.id.as_deref().unwrap_or(&self.device)
    }

    fn is(&self, device: &str) -> bool {
        self.device.eq_ignore_ascii_case(device) || self.id.as_deref() == Some(device)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layout {
    #[serde(default, rename = "neighbor")]
    pub neighbors: Vec<Neighbor>,
}

impl Layout {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)?;
        let layout: Self = toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))?;
        layout.validate()?;
        Ok(layout)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let header = "# interdesk: devices around this screen. side = left | right | top | bottom\n\n";
        crossdrop_sdk::write_atomic(path, format!("{header}{}", toml::to_string_pretty(self)?).as_bytes())?;
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        for (i, a) in self.neighbors.iter().enumerate() {
            if self.neighbors[..i].iter().any(|b| b.side == a.side) {
                bail!("two devices are on the {} side", a.side);
            }
            if a.from >= a.to || a.to > 100 {
                bail!("{}: from = {} and to = {} must satisfy 0 <= from < to <= 100", a.device, a.from, a.to);
            }
        }
        Ok(())
    }

    /// Adds or moves `device`; any other device on that side is replaced.
    pub fn set(&mut self, device: &str, side: Side) {
        let (from, to) = self.edge_of(side, |n| n.is(device));
        self.neighbors.retain(|n| n.side != side && !n.is(device));
        self.neighbors.push(Neighbor { device: device.to_string(), side, id: None, from, to });
    }

    /// Like [`Layout::set`], also remembering the device's id.
    pub fn place(&mut self, name: &str, id: &str, side: Side) {
        let (from, to) = self.edge_of(side, |n| n.is(name) || n.is(id));
        self.neighbors.retain(|n| n.side != side && !n.is(name) && !n.is(id));
        self.neighbors.push(Neighbor { device: name.to_string(), side, id: Some(id.to_string()), from, to });
    }

    /// The range kept when a device is placed again on the side it is on.
    fn edge_of(&self, side: Side, same: impl Fn(&Neighbor) -> bool) -> (u8, u8) {
        match self.neighbors.iter().find(|n| n.side == side && same(n)) {
            Some(n) => (n.from, n.to),
            None => (0, 100),
        }
    }

    /// Sets the part of this screen's edge that `device` faces.
    pub fn set_edge(&mut self, device: &str, from: u8, to: u8) -> Result<()> {
        let Some(n) = self.neighbors.iter_mut().find(|n| n.is(device)) else {
            bail!("{device} is not in the layout");
        };
        (n.from, n.to) = (from, to);
        self.validate()
    }

    /// Removes the device named `device`, or with that id.
    pub fn remove(&mut self, device: &str) -> bool {
        let before = self.neighbors.len();
        self.neighbors.retain(|n| !n.is(device));
        before != self.neighbors.len()
    }
}

/// A desktop area in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn right(&self) -> i32 {
        self.x + self.w - 1
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.h - 1
    }

    pub fn center(&self) -> (i32, i32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }
}

/// Which edge the cursor touches, if any. Corners count as left/right.
pub fn edge_at(screen: &Rect, x: i32, y: i32) -> Option<Side> {
    if x <= screen.x {
        Some(Side::Left)
    } else if x >= screen.right() {
        Some(Side::Right)
    } else if y <= screen.y {
        Some(Side::Top)
    } else if y >= screen.bottom() {
        Some(Side::Bottom)
    } else {
        None
    }
}

/// Position along an edge, from 0.0 (top / left end) to 1.0.
pub fn along(screen: &Rect, side: Side, x: i32, y: i32) -> f64 {
    let (pos, start, len) = match side {
        Side::Left | Side::Right => (y, screen.y, screen.h),
        Side::Top | Side::Bottom => (x, screen.x, screen.w),
    };
    if len <= 1 {
        return 0.5;
    }
    ((pos - start) as f64 / (len - 1) as f64).clamp(0.0, 1.0)
}

fn scale(frac: f64, len: i32) -> i32 {
    (frac * (len - 1).max(0) as f64).round() as i32
}

/// Where the cursor appears on a neighbour of size `w`×`h` placed on `side`.
/// Coordinates are relative to the neighbour's top-left corner.
pub fn entry_point(side: Side, frac: f64, w: i32, h: i32) -> (i32, i32) {
    match side {
        Side::Right => (0, scale(frac, h)),
        Side::Left => (w - 1, scale(frac, h)),
        Side::Bottom => (scale(frac, w), 0),
        Side::Top => (scale(frac, w), h - 1),
    }
}

/// Where the local cursor goes back to when it returns from `side`: two
/// pixels inside the edge so that it does not cross again at once.
pub fn return_point(screen: &Rect, side: Side, frac: f64) -> (i32, i32) {
    match side {
        Side::Right => (screen.right() - 2, screen.y + scale(frac, screen.h)),
        Side::Left => (screen.x + 2, screen.y + scale(frac, screen.h)),
        Side::Bottom => (screen.x + scale(frac, screen.w), screen.bottom() - 2),
        Side::Top => (screen.x + scale(frac, screen.w), screen.y + 2),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Rect = Rect { x: 0, y: 0, w: 1920, h: 1080 };

    #[test]
    fn edges_and_fractions() {
        assert_eq!(edge_at(&SCREEN, 1919, 500), Some(Side::Right));
        assert_eq!(edge_at(&SCREEN, 0, 0), Some(Side::Left));
        assert_eq!(edge_at(&SCREEN, 500, 1079), Some(Side::Bottom));
        assert_eq!(edge_at(&SCREEN, 500, 500), None);
        assert_eq!(along(&SCREEN, Side::Right, 1919, 0), 0.0);
        assert_eq!(along(&SCREEN, Side::Right, 1919, 1079), 1.0);
    }

    #[test]
    fn entry_keeps_relative_position() {
        let frac = along(&SCREEN, Side::Right, 1919, 540);
        assert_eq!(entry_point(Side::Right, frac, 2560, 1440), (0, 720));
        assert_eq!(entry_point(Side::Left, 0.0, 1280, 800), (1279, 0));
        assert_eq!(entry_point(Side::Top, 1.0, 1280, 800), (1279, 799));
        assert_eq!(return_point(&SCREEN, Side::Right, 0.5), (1917, 540));
    }

    #[test]
    fn layout_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("interdesk.toml");
        assert!(Layout::load(&path).unwrap().neighbors.is_empty());
        let mut layout = Layout::default();
        layout.set("bravo", Side::Right);
        layout.set("charlie", Side::Left);
        layout.set("delta", Side::Right); // replaces bravo
        layout.save(&path).unwrap();
        let again = Layout::load(&path).unwrap();
        assert_eq!(again.neighbors.len(), 2);
        assert!(again.neighbors.iter().any(|n| n.device == "delta" && n.side == Side::Right));
        assert!(toml::from_str::<Layout>("[[neighbor]]\ndevice='a'\nside='up'").is_err());

        // A range is kept when the device is placed again, and checked.
        let mut layout = Layout::default();
        layout.place("bravo", "ABCD", Side::Right);
        layout.set_edge("ABCD", 50, 100).unwrap();
        layout.place("bravo", "ABCD", Side::Right);
        assert_eq!(layout.neighbors[0].edge(), (0.5, 1.0));
        assert!(layout.set_edge("ABCD", 60, 40).is_err());
        let text =
            toml::to_string(&Layout { neighbors: vec![Neighbor { from: 0, to: 100, ..layout.neighbors[0].clone() }] })
                .unwrap();
        assert!(!text.contains("from") && !text.contains("to ="), "defaults are not written: {text}");

        // An id survives a rename and removes by either name or id.
        let mut layout = Layout::default();
        layout.place("bravo", "ABCD-EFGH", Side::Right);
        assert_eq!(layout.neighbors[0].target(), "ABCD-EFGH");
        layout.place("bravo-renamed", "ABCD-EFGH", Side::Left);
        assert_eq!(layout.neighbors.len(), 1, "same device moved, not added");
        assert!(layout.remove("ABCD-EFGH"));
        // Files without ids still work.
        let old: Layout = toml::from_str("[[neighbor]]\ndevice='bravo'\nside='right'").unwrap();
        assert_eq!(old.neighbors[0].target(), "bravo");
        assert_eq!("droite".parse::<Side>().unwrap(), Side::Right);
    }
}
