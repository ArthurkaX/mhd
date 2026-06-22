//! Pure data model for Vision Snip.
//!
//! Defines geometry types, annotation state, label allocation, validation,
//! undo history, and normalized-coordinate conversion.
//!
//! This module has no Win32 dependencies and can be fully unit tested.

use std::collections::HashSet;

// ── Geometry types ─────────────────────────────────────────────────────

/// A 2D point in pixel coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

/// An axis-aligned rectangle in pixel coordinates.
///
/// `left <= right` and `top <= bottom` after normalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    /// Create a rectangle from two unordered corners.
    pub fn new(x1: i32, y1: i32, x2: i32, y2: i32) -> Self {
        Rect {
            left: x1.min(x2),
            top: y1.min(y2),
            right: x1.max(x2),
            bottom: y1.max(y2),
        }
    }

    /// Width in pixels.
    pub fn width(&self) -> u32 {
        (self.right - self.left) as u32
    }

    /// Height in pixels.
    pub fn height(&self) -> u32 {
        (self.bottom - self.top) as u32
    }

    /// Check if this rect contains a point (inclusive).
    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.left && p.x <= self.right && p.y >= self.top && p.y <= self.bottom
    }

    /// Check if the rect is valid (non-negative dimensions).
    pub fn is_valid(&self) -> bool {
        self.right >= self.left && self.bottom >= self.top
    }

    /// Check if the rect meets the minimum size requirement.
    pub fn meets_min_size(&self, min: u32) -> bool {
        self.width() >= min && self.height() >= min
    }
}

// ── Annotation types ───────────────────────────────────────────────────

/// Fixed color palette for annotations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnotationColor {
    Red,
    Yellow,
    Green,
    Blue,
    Magenta,
    White,
}

impl AnnotationColor {
    /// All available colors in display order.
    pub const ALL: [AnnotationColor; 6] = [
        AnnotationColor::Red,
        AnnotationColor::Yellow,
        AnnotationColor::Green,
        AnnotationColor::Blue,
        AnnotationColor::Magenta,
        AnnotationColor::White,
    ];

    /// Default color (index into ALL).
    pub const fn default_index() -> usize {
        0 // Red
    }

    /// Return the RGBA colour value for rendering.
    pub fn rgba(&self) -> (u8, u8, u8, u8) {
        match self {
            AnnotationColor::Red => (255, 60, 60, 255),
            AnnotationColor::Yellow => (255, 220, 40, 255),
            AnnotationColor::Green => (60, 200, 60, 255),
            AnnotationColor::Blue => (50, 120, 255, 255),
            AnnotationColor::Magenta => (220, 50, 220, 255),
            AnnotationColor::White => (240, 240, 240, 255),
        }
    }

    /// Return a contrasting text colour (black or white) for label badges.
    pub fn text_rgba(&self) -> (u8, u8, u8, u8) {
        match self {
            AnnotationColor::Yellow | AnnotationColor::White => (0, 0, 0, 255),
            _ => (255, 255, 255, 255),
        }
    }
}

/// The geometry of a single annotation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnnotationGeometry {
    /// A point marker with a badge at an offset from the anchor.
    Point { anchor: Point, badge_origin: Point },
    /// An arrow from `from` (tail) to `to` (arrowhead/target).
    Arrow { from: Point, to: Point },
    /// A rectangular region of interest.
    Rectangle { rect: Rect },
}

/// A single annotation with geometry, label, colour, and description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Annotation {
    pub label: char,
    pub geometry: AnnotationGeometry,
    pub color: AnnotationColor,
    pub description: String,
}

// ── Pending annotation ──────────────────────────────────────────────────

/// An annotation that is being created but not yet committed.
#[derive(Debug, Clone)]
pub struct PendingAnnotation {
    pub geometry: AnnotationGeometry,
    pub color: AnnotationColor,
}

// ── Drag state ──────────────────────────────────────────────────────────

/// What the user is currently dragging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragState {
    /// Dragging a new crop rectangle.
    Crop(Point),
    /// Dragging a new arrow (start point).
    Arrow(Point),
    /// Dragging a new rectangle annotation (start point).
    Rectangle(Point),
}

// ── Edit commands (undo history) ────────────────────────────────────────

/// An undoable command that changed the model state.
///
/// Stores enough information to revert the change.
#[derive(Debug, Clone)]
pub enum EditCommand {
    /// Crop was replaced. Store the old crop.
    CropReplaced { old: Rect },
    /// An annotation was added.
    AnnotationAdded { annotation: Annotation },
    /// An annotation was replaced (arrow or rectangle replacement).
    AnnotationReplaced { old: Annotation, new: Annotation },
    /// An annotation's description was edited.
    DescriptionEdited {
        label: char,
        old: String,
        new: String,
    },
    /// All annotations were cleared (Clear command).
    Clear { annotations: Vec<Annotation> },
}

/// Errors that can occur during model operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelError {
    LabelLimitReached,
    ArrowAlreadyExists,
    RectangleAlreadyExists,
    AnnotationOutsideCrop,
    CropTooSmall,
    ArrowTooShort,
    RectangleTooSmall,
    NoAnnotationForLabel(char),
    InvalidCrop,
    AnnotationNotFound,
}

impl std::fmt::Display for ModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModelError::LabelLimitReached => write!(f, "Maximum number of annotations reached"),
            ModelError::ArrowAlreadyExists => write!(f, "An arrow already exists"),
            ModelError::RectangleAlreadyExists => {
                write!(f, "A rectangle annotation already exists")
            }
            ModelError::AnnotationOutsideCrop => write!(f, "Move annotations inside the crop"),
            ModelError::CropTooSmall => write!(f, "Crop is too small"),
            ModelError::ArrowTooShort => write!(f, "Arrow is too short"),
            ModelError::RectangleTooSmall => write!(f, "Rectangle annotation is too small"),
            ModelError::NoAnnotationForLabel(l) => write!(f, "No annotation with label {l}"),
            ModelError::InvalidCrop => write!(f, "Invalid crop"),
            ModelError::AnnotationNotFound => write!(f, "Annotation not found"),
        }
    }
}

impl std::error::Error for ModelError {}

// ── Active tool ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Crop,
    Marker,
    Arrow,
    Rectangle,
    Color,
    None,
}

// ── Output types ────────────────────────────────────────────────────────

/// Annotation data sent to the model alongside the image.
#[derive(Debug, Clone)]
pub struct ModelAnnotation {
    pub label: char,
    pub geometry_type: String,
    pub color: String,
    pub description: String,
    /// Normalized coordinates [x, y] or [x1, y1, x2, y2].
    pub coordinates: Vec<i32>,
}

// ── Main model ──────────────────────────────────────────────────────────

/// The pure Vision Snip state machine.
///
/// Contains screenshot metadata, crop, annotations, tool selection, drag
/// state, pending annotation, and undo history.
///
/// The screenshot pixels themselves are stored outside this struct.
#[derive(Debug, Clone)]
pub struct VisionSnipModel {
    /// Full captured monitor dimensions.
    pub monitor_width: u32,
    pub monitor_height: u32,

    /// Current crop rectangle in monitor-local coordinates.
    /// Default is the full monitor.
    pub crop: Rect,
    /// The previous valid crop (for reverting invalid changes).
    pub prev_crop: Rect,

    /// Committed annotations, ordered by creation time.
    pub annotations: Vec<Annotation>,

    /// Currently selected colour for new annotations.
    pub selected_color: AnnotationColor,

    /// Active tool.
    pub active_tool: Tool,

    /// Current drag state (None when not dragging).
    pub drag: Option<DragState>,

    /// Annotation being created but not yet committed.
    pub pending: Option<PendingAnnotation>,

    /// Undo history stack (most recent at end).
    pub history: Vec<EditCommand>,
}

impl VisionSnipModel {
    /// Create a new model for a monitor of the given dimensions.
    /// Default crop is the full monitor, default colour is Red.
    pub fn new(monitor_width: u32, monitor_height: u32) -> Self {
        let full = Rect {
            left: 0,
            top: 0,
            right: monitor_width as i32,
            bottom: monitor_height as i32,
        };
        VisionSnipModel {
            monitor_width,
            monitor_height,
            crop: full,
            prev_crop: full,
            annotations: Vec::new(),
            selected_color: AnnotationColor::Red,
            active_tool: Tool::None,
            drag: None,
            pending: None,
            history: Vec::new(),
        }
    }

    // ── Label allocation ─────────────────────────────────────────────

    /// Return the next available capital letter `A-Z`, or `None` if all are used.
    pub fn next_available_label(&self) -> Option<char> {
        let used: HashSet<char> = self.annotations.iter().map(|a| a.label).collect();
        for c in 'A'..='Z' {
            if !used.contains(&c) {
                return Some(c);
            }
        }
        None
    }

    // ── Arrow singleton ──────────────────────────────────────────────

    /// Return the arrow annotation if one exists.
    pub fn arrow(&self) -> Option<&Annotation> {
        self.annotations
            .iter()
            .find(|a| matches!(a.geometry, AnnotationGeometry::Arrow { .. }))
    }

    /// Return the index of the arrow annotation if one exists.
    fn arrow_index(&self) -> Option<usize> {
        self.annotations
            .iter()
            .position(|a| matches!(a.geometry, AnnotationGeometry::Arrow { .. }))
    }

    // ── Rectangle singleton ──────────────────────────────────────────

    /// Return the rectangle annotation if one exists.
    pub fn rectangle(&self) -> Option<&Annotation> {
        self.annotations
            .iter()
            .find(|a| matches!(a.geometry, AnnotationGeometry::Rectangle { .. }))
    }

    /// Return the index of the rectangle annotation if one exists.
    fn rectangle_index(&self) -> Option<usize> {
        self.annotations
            .iter()
            .position(|a| matches!(a.geometry, AnnotationGeometry::Rectangle { .. }))
    }

    // ── Pending annotation management ─────────────────────────────────

    /// Start a new pending annotation. Returns `Err` if limits are exceeded.
    pub fn start_pending(&mut self, geometry: AnnotationGeometry) -> Result<(), ModelError> {
        // Validate singleton limits
        match &geometry {
            AnnotationGeometry::Arrow { .. } => {
                // Allow replacement: if an arrow exists, we'll replace it on commit.
                // But we don't error here — we let it proceed and handle replacement on commit.
            }
            AnnotationGeometry::Rectangle { rect } => {
                if !rect.meets_min_size(8) {
                    return Err(ModelError::RectangleTooSmall);
                }
            }
            AnnotationGeometry::Point { .. } => {}
        }
        self.pending = Some(PendingAnnotation {
            geometry,
            color: self.selected_color,
        });
        Ok(())
    }

    /// Allocate a label and assign it to the pending annotation, committing it.
    /// If an annotation of the same type already exists, this is a replacement.
    pub fn commit_pending(&mut self, description: String) -> Result<(), ModelError> {
        let pending = self.pending.take().ok_or(ModelError::AnnotationNotFound)?;
        let label = self
            .next_available_label()
            .ok_or(ModelError::LabelLimitReached)?;
        let desc = if description.trim().is_empty() {
            "Attention here".to_string()
        } else {
            description.trim().to_string()
        };

        let new_ann = Annotation {
            label,
            geometry: pending.geometry.clone(),
            color: pending.color,
            description: desc,
        };

        // Check replacement semantics
        match &pending.geometry {
            AnnotationGeometry::Arrow { from, to } => {
                let dx = to.x - from.x;
                let dy = to.y - from.y;
                if (dx * dx + dy * dy) < 64 {
                    // Minimum arrow length: 8 pixels
                    self.pending = Some(pending); // restore pending
                    return Err(ModelError::ArrowTooShort);
                }
                if let Some(idx) = self.arrow_index() {
                    let old = self.annotations[idx].clone();
                    self.history.push(EditCommand::AnnotationReplaced {
                        old,
                        new: new_ann.clone(),
                    });
                    self.annotations[idx] = new_ann;
                    return Ok(());
                }
            }
            AnnotationGeometry::Rectangle { rect } => {
                if !rect.meets_min_size(8) {
                    self.pending = Some(pending);
                    return Err(ModelError::RectangleTooSmall);
                }
                if let Some(idx) = self.rectangle_index() {
                    let old = self.annotations[idx].clone();
                    self.history.push(EditCommand::AnnotationReplaced {
                        old,
                        new: new_ann.clone(),
                    });
                    self.annotations[idx] = new_ann;
                    return Ok(());
                }
            }
            AnnotationGeometry::Point { .. } => {
                // Point markers are not singletons
            }
        }

        self.history.push(EditCommand::AnnotationAdded {
            annotation: new_ann.clone(),
        });
        self.annotations.push(new_ann);
        Ok(())
    }

    /// Cancel the pending annotation without changes.
    pub fn cancel_pending(&mut self) {
        self.pending = None;
    }

    // ── Crop management ──────────────────────────────────────────────

    /// Set the crop rectangle. Rejects invalid/small crops.
    pub fn set_crop(&mut self, rect: Rect) -> Result<(), ModelError> {
        if !rect.is_valid() {
            return Err(ModelError::InvalidCrop);
        }
        if !rect.meets_min_size(32) {
            return Err(ModelError::CropTooSmall);
        }
        let old = self.crop;
        self.history.push(EditCommand::CropReplaced { old });
        self.prev_crop = self.crop;
        self.crop = rect;
        Ok(())
    }

    /// Reset crop to the full monitor.
    pub fn reset_crop(&mut self) {
        let full = Rect {
            left: 0,
            top: 0,
            right: self.monitor_width as i32,
            bottom: self.monitor_height as i32,
        };
        let old = self.crop;
        if old != full {
            self.history.push(EditCommand::CropReplaced { old });
            self.prev_crop = self.crop;
            self.crop = full;
        }
    }

    // ── Validation ───────────────────────────────────────────────────

    /// Validate the model is ready for analysis.
    /// Checks that all annotations are inside the crop.
    pub fn validate_for_analysis(&self) -> Result<(), ModelError> {
        if self.pending.is_some() {
            // Should not happen — pending must be committed or cancelled first.
            return Err(ModelError::AnnotationNotFound);
        }
        for ann in &self.annotations {
            if !self.is_annotation_inside_crop(ann) {
                return Err(ModelError::AnnotationOutsideCrop);
            }
        }
        Ok(())
    }

    /// Check whether a single annotation is entirely inside the crop.
    fn is_annotation_inside_crop(&self, ann: &Annotation) -> bool {
        match &ann.geometry {
            AnnotationGeometry::Point {
                anchor,
                badge_origin,
            } => self.crop.contains(*anchor) && self.crop.contains(*badge_origin),
            AnnotationGeometry::Arrow { from, to } => {
                self.crop.contains(*from) && self.crop.contains(*to)
            }
            AnnotationGeometry::Rectangle { rect } => {
                self.crop.contains(Point {
                    x: rect.left,
                    y: rect.top,
                }) && self.crop.contains(Point {
                    x: rect.right,
                    y: rect.bottom,
                })
            }
        }
    }

    // ── Undo ─────────────────────────────────────────────────────────

    /// Undo the most recent command.
    pub fn undo(&mut self) {
        let cmd = match self.history.pop() {
            Some(c) => c,
            None => return,
        };
        match cmd {
            EditCommand::CropReplaced { old } => {
                self.crop = old;
            }
            EditCommand::AnnotationAdded { annotation } => {
                self.annotations.retain(|a| a.label != annotation.label);
            }
            EditCommand::AnnotationReplaced { old, new } => {
                if let Some(idx) = self.annotations.iter().position(|a| a.label == new.label) {
                    self.annotations[idx] = old;
                }
            }
            EditCommand::DescriptionEdited { label, old, new: _ } => {
                if let Some(ann) = self.annotations.iter_mut().find(|a| a.label == label) {
                    ann.description = old;
                }
            }
            EditCommand::Clear { annotations } => {
                self.annotations = annotations;
            }
        }
    }

    // ── Clear ────────────────────────────────────────────────────────

    /// Remove all annotations as one undoable operation.
    pub fn clear_annotations(&mut self) {
        if self.annotations.is_empty() {
            return;
        }
        let old = self.annotations.clone();
        self.history.push(EditCommand::Clear { annotations: old });
        self.annotations.clear();
    }

    // ── Description editing ──────────────────────────────────────────

    /// Edit the description of an existing annotation.
    pub fn edit_description(&mut self, label: char, new_desc: String) -> Result<(), ModelError> {
        let ann = self
            .annotations
            .iter_mut()
            .find(|a| a.label == label)
            .ok_or(ModelError::NoAnnotationForLabel(label))?;
        let trimmed = new_desc.trim().to_string();
        if trimmed.is_empty() {
            return Ok(()); // Keep the old description if new is empty
        }
        let old = ann.description.clone();
        if old != trimmed {
            self.history.push(EditCommand::DescriptionEdited {
                label,
                old,
                new: trimmed.clone(),
            });
            ann.description = trimmed;
        }
        Ok(())
    }

    // ── Coordinate conversion ────────────────────────────────────────

    /// Convert monitor-local coordinates to crop-local.
    pub fn to_crop_local(&self, p: &Point) -> Point {
        Point {
            x: p.x - self.crop.left,
            y: p.y - self.crop.top,
        }
    }

    /// Convert a crop-local point to normalized 0..1000 range.
    pub fn to_normalized(&self, p: &Point) -> (i32, i32) {
        let cw = self.crop.width().max(1);
        let ch = self.crop.height().max(1);
        let nx = (p.x as i64 * 1000 / cw as i64).clamp(0, 1000) as i32;
        let ny = (p.y as i64 * 1000 / ch as i64).clamp(0, 1000) as i32;
        (nx, ny)
    }

    // ── Model output ─────────────────────────────────────────────────

    /// Build the structured annotation metadata for the prompt.
    pub fn build_model_annotations(&self) -> Vec<ModelAnnotation> {
        self.annotations
            .iter()
            .map(|ann| {
                let (geometry_type, coordinates) = match &ann.geometry {
                    AnnotationGeometry::Point { anchor, .. } => {
                        let (nx, ny) = self.to_normalized(&self.to_crop_local(anchor));
                        ("point".to_string(), vec![nx, ny])
                    }
                    AnnotationGeometry::Arrow { from, to } => {
                        let (fx, fy) = self.to_normalized(&self.to_crop_local(from));
                        let (tx, ty) = self.to_normalized(&self.to_crop_local(to));
                        ("arrow".to_string(), vec![fx, fy, tx, ty])
                    }
                    AnnotationGeometry::Rectangle { rect } => {
                        let tl = self.to_crop_local(&Point {
                            x: rect.left,
                            y: rect.top,
                        });
                        let br = self.to_crop_local(&Point {
                            x: rect.right,
                            y: rect.bottom,
                        });
                        let (lx, ty) = self.to_normalized(&tl);
                        let (rx, by) = self.to_normalized(&br);
                        ("rectangle".to_string(), vec![lx, ty, rx, by])
                    }
                };
                let color_name = format!("{:?}", ann.color);
                ModelAnnotation {
                    label: ann.label,
                    geometry_type,
                    color: color_name,
                    description: ann.description.clone(),
                    coordinates,
                }
            })
            .collect()
    }

    /// Generate the structured prompt text for the vision model.
    pub fn build_prompt(&self) -> String {
        let cw = self.crop.width();
        let ch = self.crop.height();
        let anns = self.build_model_annotations();

        if anns.is_empty() {
            return format!(
                "Analyze the attached cropped screenshot. \
                 Return a direct, useful description \
                 of the visible content and any likely issue or important detail.\
                 \n\nImage size: {}x{} pixels.",
                cw, ch
            );
        }

        let mut prompt = String::from(
            "Analyze the attached screenshot according to the user's annotations.\n\n\
             The image contains capital-letter labels that correspond to the entries below.\n\
             Coordinates are normalized to a 0..1000 range with origin at the top-left.\n\
             For arrows, \"to\" is the arrowhead and indicates the target.\n\n",
        );
        prompt.push_str(&format!(
            "Image size before provider-side scaling: {}x{} pixels.\n\n",
            cw, ch
        ));
        prompt.push_str("Annotations:\n");

        for ma in &anns {
            prompt.push_str(&format!("- {}\n", ma.label));
            prompt.push_str(&format!("  Type: {}\n", ma.geometry_type));
            match ma.geometry_type.as_str() {
                "point" => {
                    if ma.coordinates.len() >= 2 {
                        prompt.push_str(&format!(
                            "  Anchor: [{}, {}]\n",
                            ma.coordinates[0], ma.coordinates[1]
                        ));
                    }
                }
                "arrow" => {
                    if ma.coordinates.len() >= 4 {
                        prompt.push_str(&format!(
                            "  From: [{}, {}]\n",
                            ma.coordinates[0], ma.coordinates[1]
                        ));
                        prompt.push_str(&format!(
                            "  To: [{}, {}]\n",
                            ma.coordinates[2], ma.coordinates[3]
                        ));
                    }
                }
                "rectangle" => {
                    if ma.coordinates.len() >= 4 {
                        prompt.push_str(&format!(
                            "  Bounds: [{}, {}, {}, {}]\n",
                            ma.coordinates[0],
                            ma.coordinates[1],
                            ma.coordinates[2],
                            ma.coordinates[3]
                        ));
                    }
                }
                _ => {}
            }
            prompt.push_str(&format!("  User description: {}\n", ma.description));
        }

        prompt.push_str(
            "\nUse the visual labels, geometry, and user descriptions together. Focus on the \
             user's indicated concerns. Return a direct, useful answer without discussing \
             the annotation format.",
        );
        prompt
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rect_normalization() {
        let r = Rect::new(100, 200, 50, 400);
        assert_eq!(r.left, 50);
        assert_eq!(r.right, 100);
        assert_eq!(r.top, 200);
        assert_eq!(r.bottom, 400);
        assert_eq!(r.width(), 50);
        assert_eq!(r.height(), 200);
    }

    #[test]
    fn test_label_allocation() {
        let m = VisionSnipModel::new(1920, 1080);
        assert_eq!(m.next_available_label(), Some('A'));
    }

    #[test]
    fn test_label_exhaustion() {
        let mut m = VisionSnipModel::new(1920, 1080);
        // Use all 26 labels
        for l in 'A'..='Z' {
            m.annotations.push(Annotation {
                label: l,
                geometry: AnnotationGeometry::Point {
                    anchor: Point { x: 100, y: 100 },
                    badge_origin: Point { x: 120, y: 80 },
                },
                color: AnnotationColor::Red,
                description: "test".into(),
            });
        }
        assert_eq!(m.next_available_label(), None);
    }

    #[test]
    fn test_label_reuse_after_undo() {
        let mut m = VisionSnipModel::new(1920, 1080);
        // Add an annotation through the proper path
        m.start_pending(AnnotationGeometry::Point {
            anchor: Point { x: 100, y: 100 },
            badge_origin: Point { x: 120, y: 80 },
        })
        .unwrap();
        m.commit_pending("test".into()).unwrap();
        assert_eq!(m.annotations.len(), 1);
        assert_eq!(m.annotations[0].label, 'A');

        // Undo removes it
        m.undo();
        assert_eq!(m.annotations.len(), 0);

        // Now 'A' should be available again
        assert_eq!(m.next_available_label(), Some('A'));
    }

    #[test]
    fn test_arrow_singleton() {
        let mut m = VisionSnipModel::new(1920, 1080);
        let g = AnnotationGeometry::Arrow {
            from: Point { x: 100, y: 100 },
            to: Point { x: 200, y: 200 },
        };
        m.start_pending(g).unwrap();
        m.commit_pending("First arrow".into()).unwrap();
        assert!(m.arrow().is_some());

        // Replace arrow
        let g2 = AnnotationGeometry::Arrow {
            from: Point { x: 150, y: 150 },
            to: Point { x: 300, y: 300 },
        };
        m.start_pending(g2).unwrap();
        m.commit_pending("Replacement".into()).unwrap();
        assert!(m.arrow().is_some());
        assert_eq!(m.annotations.len(), 1);
        assert_eq!(m.arrow().unwrap().description, "Replacement");
    }

    #[test]
    fn test_rectangle_singleton() {
        let mut m = VisionSnipModel::new(1920, 1080);
        let g = AnnotationGeometry::Rectangle {
            rect: Rect::new(100, 100, 300, 300),
        };
        m.start_pending(g).unwrap();
        m.commit_pending("First rect".into()).unwrap();
        assert!(m.rectangle().is_some());

        // Replace rectangle
        let g2 = AnnotationGeometry::Rectangle {
            rect: Rect::new(50, 50, 200, 200),
        };
        m.start_pending(g2).unwrap();
        m.commit_pending("Replacement".into()).unwrap();
        assert_eq!(m.annotations.len(), 1);
    }

    #[test]
    fn test_crop_validation() {
        let mut m = VisionSnipModel::new(1920, 1080);
        assert!(m.set_crop(Rect::new(10, 10, 20, 20)).is_err()); // too small
        assert!(m.set_crop(Rect::new(10, 10, 100, 100)).is_ok());
        assert!(m.set_crop(Rect::new(100, 100, 50, 50)).is_ok()); // auto-normalized, too small
    }

    #[test]
    fn test_annotation_outside_crop() {
        let mut m = VisionSnipModel::new(1920, 1080);
        m.crop = Rect::new(100, 100, 500, 500);

        m.annotations.push(Annotation {
            label: 'A',
            geometry: AnnotationGeometry::Point {
                anchor: Point { x: 50, y: 50 },
                badge_origin: Point { x: 70, y: 30 },
            },
            color: AnnotationColor::Red,
            description: "outside".into(),
        });
        assert_eq!(
            m.validate_for_analysis(),
            Err(ModelError::AnnotationOutsideCrop)
        );

        // Move inside
        m.annotations[0] = Annotation {
            label: 'A',
            geometry: AnnotationGeometry::Point {
                anchor: Point { x: 200, y: 200 },
                badge_origin: Point { x: 220, y: 180 },
            },
            color: AnnotationColor::Red,
            description: "inside".into(),
        };
        assert!(m.validate_for_analysis().is_ok());
    }

    #[test]
    fn test_undo_clear() {
        let mut m = VisionSnipModel::new(1920, 1080);
        for i in 0..3 {
            m.annotations.push(Annotation {
                label: char::from(b'A' + i),
                geometry: AnnotationGeometry::Point {
                    anchor: Point {
                        x: 100 + i as i32 * 10,
                        y: 100,
                    },
                    badge_origin: Point {
                        x: 120 + i as i32 * 10,
                        y: 80,
                    },
                },
                color: AnnotationColor::Red,
                description: format!("ann {i}"),
            });
        }
        assert_eq!(m.annotations.len(), 3);
        m.clear_annotations();
        assert!(m.annotations.is_empty());
        m.undo();
        assert_eq!(m.annotations.len(), 3);
    }

    #[test]
    fn test_coordinate_conversion() {
        let m = VisionSnipModel::new(1920, 1080);
        // Default crop is full monitor, so crop-local = same as monitor-local
        let p = Point { x: 960, y: 540 };
        let cl = m.to_crop_local(&p);
        assert_eq!(cl.x, 960);
        assert_eq!(cl.y, 540);

        let (nx, ny) = m.to_normalized(&cl);
        assert_eq!(nx, 500); // 960 * 1000 / 1920 = 500
        assert_eq!(ny, 500); // 540 * 1000 / 1080 = 500
    }

    #[test]
    fn test_prompt_generation_no_annotations() {
        let m = VisionSnipModel::new(1920, 1080);
        let prompt = m.build_prompt();
        assert!(
            prompt.contains("no annotations") || prompt.contains("cropped screenshot"),
            "prompt should indicate no annotations: {prompt}"
        );
    }

    #[test]
    fn test_prompt_generation_with_annotations() {
        let mut m = VisionSnipModel::new(1920, 1080);
        m.annotations.push(Annotation {
            label: 'A',
            geometry: AnnotationGeometry::Point {
                anchor: Point { x: 960, y: 540 },
                badge_origin: Point { x: 980, y: 520 },
            },
            color: AnnotationColor::Red,
            description: "The main issue is here.".into(),
        });
        let prompt = m.build_prompt();
        assert!(prompt.contains("- A"));
        assert!(prompt.contains("The main issue is here."));
        assert!(prompt.contains("point"));
    }

    #[test]
    fn test_arrow_short_rejection() {
        let mut m = VisionSnipModel::new(1920, 1080);
        let g = AnnotationGeometry::Arrow {
            from: Point { x: 100, y: 100 },
            to: Point { x: 102, y: 100 }, // only 2 pixels
        };
        m.start_pending(g).unwrap();
        assert_eq!(
            m.commit_pending("test".into()),
            Err(ModelError::ArrowTooShort)
        );
    }

    #[test]
    fn test_replace_preserves_old_on_cancel() {
        let mut m = VisionSnipModel::new(1920, 1080);
        let g1 = AnnotationGeometry::Arrow {
            from: Point { x: 100, y: 100 },
            to: Point { x: 300, y: 300 },
        };
        m.start_pending(g1).unwrap();
        m.commit_pending("Original".into()).unwrap();
        let original_label = m.arrow().unwrap().label;

        // Start a replacement but cancel it
        let g2 = AnnotationGeometry::Arrow {
            from: Point { x: 10, y: 10 },
            to: Point { x: 11, y: 10 }, // too short (dx=1, dy=0 -> 1 < 64)
        };
        m.start_pending(g2).unwrap();
        let r = m.commit_pending("Replacement".into());
        assert_eq!(r, Err(ModelError::ArrowTooShort));
        m.cancel_pending();

        // Original should still be there
        assert_eq!(m.annotations.len(), 1);
        assert_eq!(m.arrow().unwrap().label, original_label);
        assert_eq!(m.arrow().unwrap().description, "Original");
    }
}
