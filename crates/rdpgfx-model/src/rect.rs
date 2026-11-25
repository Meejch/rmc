use serde::{Deserialize, Serialize};

/// Axis-aligned rectangle with exclusive right/bottom edges (like `RECTANGLE_16`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rect {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

impl Rect {
    pub const fn new(left: u32, top: u32, right: u32, bottom: u32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    pub const fn from_xywh(x: u32, y: u32, w: u32, h: u32) -> Self {
        Self::new(x, y, x + w, y + h)
    }

    pub const fn width(&self) -> u32 {
        self.right.saturating_sub(self.left)
    }

    pub const fn height(&self) -> u32 {
        self.bottom.saturating_sub(self.top)
    }

    pub const fn is_empty(&self) -> bool {
        self.width() == 0 || self.height() == 0
    }

    pub fn intersect(&self, other: &Rect) -> Option<Rect> {
        let r = Rect::new(
            self.left.max(other.left),
            self.top.max(other.top),
            self.right.min(other.right),
            self.bottom.min(other.bottom),
        );
        (!r.is_empty()).then_some(r)
    }

    /// Grows the rectangle outwards to even coordinates (whole 2x2 chroma blocks).
    pub fn align_to_chroma_blocks(&self) -> Rect {
        Rect::new(
            self.left & !1,
            self.top & !1,
            (self.right + 1) & !1,
            (self.bottom + 1) & !1,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersect_and_align() {
        let a = Rect::from_xywh(0, 0, 10, 10);
        let b = Rect::from_xywh(5, 5, 10, 10);
        assert_eq!(a.intersect(&b), Some(Rect::new(5, 5, 10, 10)));
        assert_eq!(a.intersect(&Rect::from_xywh(10, 0, 5, 5)), None);
        assert_eq!(
            Rect::new(3, 1, 7, 8).align_to_chroma_blocks(),
            Rect::new(2, 0, 8, 8)
        );
    }
}
