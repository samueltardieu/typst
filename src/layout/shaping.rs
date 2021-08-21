use std::borrow::Cow;
use std::ops::Range;

use rustybuzz::UnicodeBuffer;

use super::{Element, Frame, Glyph, LayoutContext, Text};
use crate::eval::{FontState, LineState};
use crate::font::{Face, FaceId, FontVariant, LineMetrics};
use crate::geom::{Dir, Length, Point, Size};
use crate::layout::Geometry;
use crate::util::SliceExt;

/// Shape text into [`ShapedText`].
pub fn shape<'a>(
    ctx: &mut LayoutContext,
    text: &'a str,
    dir: Dir,
    state: &'a FontState,
) -> ShapedText<'a> {
    let mut glyphs = vec![];
    if !text.is_empty() {
        shape_segment(
            ctx,
            &mut glyphs,
            0,
            text,
            dir,
            state.size,
            state.variant(),
            state.families(),
            None,
        );
    }

    let (size, baseline) = measure(ctx, &glyphs, state);

    ShapedText {
        text,
        dir,
        state,
        size,
        baseline,
        glyphs: Cow::Owned(glyphs),
    }
}

/// The result of shaping text.
///
/// This type contains owned or borrowed shaped text runs, which can be
/// measured, used to reshape substrings more quickly and converted into a
/// frame.
pub struct ShapedText<'a> {
    /// The text that was shaped.
    pub text: &'a str,
    /// The text direction.
    pub dir: Dir,
    /// The properties used for font selection.
    pub state: &'a FontState,
    /// The font size.
    pub size: Size,
    /// The baseline from the top of the frame.
    pub baseline: Length,
    /// The shaped glyphs.
    pub glyphs: Cow<'a, [ShapedGlyph]>,
}

/// A single glyph resulting from shaping.
#[derive(Copy, Clone)]
pub struct ShapedGlyph {
    /// The font face the glyph is contained in.
    pub face_id: FaceId,
    /// The glyph's index in the face.
    pub glyph_id: u16,
    /// The advance width of the glyph.
    pub x_advance: Length,
    /// The horizontal offset of the glyph.
    pub x_offset: Length,
    /// The start index of the glyph in the source text.
    pub text_index: usize,
    /// Whether splitting the shaping result before this glyph would yield the
    /// same results as shaping the parts to both sides of `text_index`
    /// separately.
    pub safe_to_break: bool,
}

impl<'a> ShapedText<'a> {
    /// Build the shaped text's frame.
    pub fn build(&self, ctx: &LayoutContext) -> Frame {
        let mut frame = Frame::new(self.size, self.baseline);
        let mut offset = Length::zero();

        for (face_id, group) in self.glyphs.as_ref().group_by_key(|g| g.face_id) {
            let pos = Point::new(offset, self.baseline);

            let mut text = Text {
                face_id,
                size: self.state.size,
                fill: self.state.fill,
                glyphs: vec![],
            };

            let mut width = Length::zero();
            for glyph in group {
                text.glyphs.push(Glyph {
                    id: glyph.glyph_id,
                    x_advance: glyph.x_advance,
                    x_offset: glyph.x_offset,
                });
                width += glyph.x_advance;
            }

            frame.push(pos, Element::Text(text));
            decorate(ctx, &mut frame, pos, width, face_id, &self.state);

            offset += width;
        }

        frame
    }

    /// Reshape a range of the shaped text, reusing information from this
    /// shaping process if possible.
    pub fn reshape(
        &'a self,
        ctx: &mut LayoutContext,
        text_range: Range<usize>,
    ) -> ShapedText<'a> {
        if let Some(glyphs) = self.slice_safe_to_break(text_range.clone()) {
            let (size, baseline) = measure(ctx, glyphs, self.state);
            Self {
                text: &self.text[text_range],
                dir: self.dir,
                state: self.state,
                size,
                baseline,
                glyphs: Cow::Borrowed(glyphs),
            }
        } else {
            shape(ctx, &self.text[text_range], self.dir, self.state)
        }
    }

    /// Find the subslice of glyphs that represent the given text range if both
    /// sides are safe to break.
    fn slice_safe_to_break(&self, text_range: Range<usize>) -> Option<&[ShapedGlyph]> {
        let Range { mut start, mut end } = text_range;
        if !self.dir.is_positive() {
            std::mem::swap(&mut start, &mut end);
        }

        let left = self.find_safe_to_break(start, Side::Left)?;
        let right = self.find_safe_to_break(end, Side::Right)?;
        Some(&self.glyphs[left .. right])
    }

    /// Find the glyph offset matching the text index that is most towards the
    /// given side and safe-to-break.
    fn find_safe_to_break(&self, text_index: usize, towards: Side) -> Option<usize> {
        let ltr = self.dir.is_positive();

        // Handle edge cases.
        let len = self.glyphs.len();
        if text_index == 0 {
            return Some(if ltr { 0 } else { len });
        } else if text_index == self.text.len() {
            return Some(if ltr { len } else { 0 });
        }

        // Find any glyph with the text index.
        let mut idx = self
            .glyphs
            .binary_search_by(|g| {
                let ordering = g.text_index.cmp(&text_index);
                if ltr { ordering } else { ordering.reverse() }
            })
            .ok()?;

        let next = match towards {
            Side::Left => usize::checked_sub,
            Side::Right => usize::checked_add,
        };

        // Search for the outermost glyph with the text index.
        while let Some(next) = next(idx, 1) {
            if self.glyphs.get(next).map_or(true, |g| g.text_index != text_index) {
                break;
            }
            idx = next;
        }

        // RTL needs offset one because the left side of the range should be
        // exclusive and the right side inclusive, contrary to the normal
        // behaviour of ranges.
        if !ltr {
            idx += 1;
        }

        self.glyphs[idx].safe_to_break.then(|| idx)
    }
}

/// A visual side.
enum Side {
    Left,
    Right,
}

/// Shape text with font fallback using the `families` iterator.
fn shape_segment<'a>(
    ctx: &mut LayoutContext,
    glyphs: &mut Vec<ShapedGlyph>,
    base: usize,
    text: &str,
    dir: Dir,
    size: Length,
    variant: FontVariant,
    mut families: impl Iterator<Item = &'a str> + Clone,
    mut first_face: Option<FaceId>,
) {
    // Select the font family.
    let (face_id, fallback) = loop {
        // Try to load the next available font family.
        match families.next() {
            Some(family) => {
                if let Some(id) = ctx.fonts.select(family, variant) {
                    break (id, true);
                }
            }
            // We're out of families, so we don't do any more fallback and just
            // shape the tofus with the first face we originally used.
            None => match first_face {
                Some(id) => break (id, false),
                None => return,
            },
        }
    };

    // Remember the id if this the first available face since we use that one to
    // shape tofus.
    first_face.get_or_insert(face_id);

    // Fill the buffer with our text.
    let mut buffer = UnicodeBuffer::new();
    buffer.push_str(text);
    buffer.set_direction(match dir {
        Dir::LTR => rustybuzz::Direction::LeftToRight,
        Dir::RTL => rustybuzz::Direction::RightToLeft,
        _ => unimplemented!(),
    });

    // Shape!
    let mut face = ctx.fonts.get(face_id);
    let buffer = rustybuzz::shape(face.ttf(), &[], buffer);
    let infos = buffer.glyph_infos();
    let pos = buffer.glyph_positions();

    // Collect the shaped glyphs, doing fallback and shaping parts again with
    // the next font if necessary.
    let mut i = 0;
    while i < infos.len() {
        let info = &infos[i];
        let cluster = info.cluster as usize;

        if info.glyph_id != 0 || !fallback {
            // Add the glyph to the shaped output.
            // TODO: Don't ignore y_advance and y_offset.
            glyphs.push(ShapedGlyph {
                face_id,
                glyph_id: info.glyph_id as u16,
                x_advance: face.to_em(pos[i].x_advance).to_length(size),
                x_offset: face.to_em(pos[i].x_offset).to_length(size),
                text_index: base + cluster,
                safe_to_break: !info.unsafe_to_break(),
            });
        } else {
            // Determine the source text range for the tofu sequence.
            let range = {
                // First, search for the end of the tofu sequence.
                let k = i;
                while infos.get(i + 1).map_or(false, |info| info.glyph_id == 0) {
                    i += 1;
                }

                // Then, determine the start and end text index.
                //
                // Examples:
                // Everything is shown in visual order. Tofus are written as "_".
                // We want to find out that the tofus span the text `2..6`.
                // Note that the clusters are longer than 1 char.
                //
                // Left-to-right:
                // Text:     h a l i h a l l o
                // Glyphs:   A   _   _   C   E
                // Clusters: 0   2   4   6   8
                //              k=1 i=2
                //
                // Right-to-left:
                // Text:     O L L A H I L A H
                // Glyphs:   E   C   _   _   A
                // Clusters: 8   6   4   2   0
                //                  k=2 i=3

                let ltr = dir.is_positive();
                let first = if ltr { k } else { i };
                let start = infos[first].cluster as usize;

                let last = if ltr { i.checked_add(1) } else { k.checked_sub(1) };
                let end = last
                    .and_then(|last| infos.get(last))
                    .map_or(text.len(), |info| info.cluster as usize);

                start .. end
            };

            // Recursively shape the tofu sequence with the next family.
            shape_segment(
                ctx,
                glyphs,
                base + range.start,
                &text[range],
                dir,
                size,
                variant,
                families.clone(),
                first_face,
            );

            face = ctx.fonts.get(face_id);
        }

        i += 1;
    }
}

/// Measure the size and baseline of a run of shaped glyphs with the given
/// properties.
fn measure(
    ctx: &mut LayoutContext,
    glyphs: &[ShapedGlyph],
    state: &FontState,
) -> (Size, Length) {
    let mut width = Length::zero();
    let mut top = Length::zero();
    let mut bottom = Length::zero();
    let mut expand_vertical = |face: &Face| {
        top.set_max(face.vertical_metric(state.top_edge).to_length(state.size));
        bottom.set_max(-face.vertical_metric(state.bottom_edge).to_length(state.size));
    };

    if glyphs.is_empty() {
        // When there are no glyphs, we just use the vertical metrics of the
        // first available font.
        for family in state.families() {
            if let Some(face_id) = ctx.fonts.select(family, state.variant) {
                expand_vertical(ctx.fonts.get(face_id));
                break;
            }
        }
    } else {
        for (face_id, group) in glyphs.group_by_key(|g| g.face_id) {
            let face = ctx.fonts.get(face_id);
            expand_vertical(face);

            for glyph in group {
                width += glyph.x_advance;
            }
        }
    }

    (Size::new(width, top + bottom), top)
}

/// Add underline, strikthrough and overline decorations.
fn decorate(
    ctx: &LayoutContext,
    frame: &mut Frame,
    pos: Point,
    width: Length,
    face_id: FaceId,
    state: &FontState,
) {
    let mut apply = |substate: &LineState, metrics: fn(&Face) -> &LineMetrics| {
        let metrics = metrics(ctx.fonts.get(face_id));

        let stroke = substate.stroke.unwrap_or(state.fill);

        let thickness = substate
            .thickness
            .map(|s| s.resolve(state.size))
            .unwrap_or(metrics.strength.to_length(state.size));

        let offset = substate
            .offset
            .map(|s| s.resolve(state.size))
            .unwrap_or(-metrics.position.to_length(state.size));

        let extent = substate.extent.resolve(state.size);

        let pos = Point::new(pos.x - extent, pos.y + offset);
        let target = Point::new(width + 2.0 * extent, Length::zero());
        let element = Element::Geometry(Geometry::Line(target, thickness), stroke);
        frame.push(pos, element);
    };

    if let Some(strikethrough) = &state.strikethrough {
        apply(strikethrough, |face| &face.strikethrough);
    }

    if let Some(underline) = &state.underline {
        apply(underline, |face| &face.underline);
    }

    if let Some(overline) = &state.overline {
        apply(overline, |face| &face.overline);
    }
}
