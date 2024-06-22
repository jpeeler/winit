use std::hash::{Hash, Hasher};
use std::iter;
use std::sync::Arc;

use x11rb::connection::Connection;
use x11rb::protocol::render::{self, ConnectionExt as _};
use x11rb::protocol::xproto;

use crate::platform_impl::PlatformCustomCursorSource;
use crate::window::CursorIcon;

use super::super::ActiveEventLoop;
use super::*;

impl XConnection {
    pub fn set_cursor_icon(&self, window: xproto::Window, cursor: Option<CursorIcon>) {
        let cursor = *self
            .cursor_cache
            .lock()
            .unwrap()
            .entry(cursor)
            .or_insert_with(|| self.get_cursor(cursor).expect("failed to create cursor"));

        self.update_cursor(window, cursor).expect("Failed to set cursor");
    }

    pub(crate) fn set_custom_cursor(&self, window: xproto::Window, cursor: &CustomCursor) {
        self.update_cursor(window, cursor.inner.cursor).expect("Failed to set cursor");
    }

    /// Create a cursor from an image.
    fn create_cursor_from_image(
        &self,
        width: u16,
        height: u16,
        depth: u8,
        hotspot_x: u16,
        hotspot_y: u16,
        image: &[u8],
    ) -> Result<xproto::Cursor, X11Error> {
        // Create a pixap for the default root window.
        let root = self.default_root().root;
        let pixmap = self.xcb_connection().generate_id()?;
        self.xcb_connection().create_pixmap(depth, pixmap, root, width, height)?.ignore_error();
        let pixmap_guard = CallOnDrop(|| {
            self.xcb_connection().free_pixmap(pixmap).map(|r| r.ignore_error()).ok();
        });

        // Create a GC to draw with.
        let gc = self.xcb_connection().generate_id()?;
        self.xcb_connection().create_gc(gc, pixmap, &Default::default())?.ignore_error();
        let gc_guard = CallOnDrop(|| {
            self.xcb_connection().free_gc(gc).map(|r| r.ignore_error()).ok();
        });

        // Draw the data into it.
        let format =
            if depth == 1 { xproto::ImageFormat::XY_PIXMAP } else { xproto::ImageFormat::Z_PIXMAP };
        self.xcb_connection()
            .put_image(format, pixmap, gc, width, height, 0, 0, 0, depth, image)?
            .ignore_error();
        drop(gc_guard);

        // Create the XRender picture.
        let picture = self.xcb_connection().generate_id()?;
        self.xcb_connection()
            .render_create_picture(picture, pixmap, self.find_argb32_format(), &Default::default())?
            .check()?;
        let _picture_guard = CallOnDrop(|| {
            self.xcb_connection().render_free_picture(picture).map(|r| r.ignore_error()).ok();
        });
        drop(pixmap_guard);

        // Create the cursor.
        let cursor = self.xcb_connection().generate_id()?;
        self.xcb_connection()
            .render_create_cursor(cursor, picture, hotspot_x, hotspot_y)?
            .check()?;

        Ok(cursor)
    }

    /// Find the render format that corresponds to ARGB32.
    fn find_argb32_format(&self) -> render::Pictformat {
        macro_rules! direct {
            ($format:expr, $shift_name:ident, $mask_name:ident, $shift:expr) => {{
                ($format).direct.$shift_name == $shift && ($format).direct.$mask_name == 0xff
            }};
        }

        self.render_formats()
            .formats
            .iter()
            .find(|format| {
                format.type_ == render::PictType::DIRECT
                    && format.depth == 32
                    && direct!(format, red_shift, red_mask, 16)
                    && direct!(format, green_shift, green_mask, 8)
                    && direct!(format, blue_shift, blue_mask, 0)
                    && direct!(format, alpha_shift, alpha_mask, 24)
            })
            .expect("unable to find ARGB32 xrender format")
            .id
    }

    fn create_empty_cursor(&self) -> Result<xproto::Cursor, X11Error> {
        self.create_cursor_from_image(1, 1, 32, 0, 0, &[0, 0, 0, 0])
    }

    fn get_cursor(&self, cursor: Option<CursorIcon>) -> Result<xproto::Cursor, X11Error> {
        let cursor = match cursor {
            Some(cursor) => cursor,
            None => return self.create_empty_cursor(),
        };

        let database = self.database();
        let handle = x11rb::cursor::Handle::new(
            self.xcb_connection(),
            self.default_screen_index(),
            &database,
        )?
        .reply()?;

        let mut last_error = None;
        for &name in iter::once(&cursor.name()).chain(cursor.alt_names().iter()) {
            match handle.load_cursor(self.xcb_connection(), name) {
                Ok(cursor) => return Ok(cursor),
                Err(err) => last_error = Some(err.into()),
            }
        }

        Err(last_error.unwrap())
    }

    fn update_cursor(
        &self,
        window: xproto::Window,
        cursor: xproto::Cursor,
    ) -> Result<(), X11Error> {
        self.xcb_connection()
            .change_window_attributes(
                window,
                &xproto::ChangeWindowAttributesAux::new().cursor(cursor),
            )?
            .ignore_error();

        self.xcb_connection().flush()?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectedCursor {
    Custom(CustomCursor),
    Named(CursorIcon),
}

#[derive(Debug, Clone)]
pub struct CustomCursor {
    inner: Arc<CustomCursorInner>,
}

impl Hash for CustomCursor {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.inner).hash(state);
    }
}

impl PartialEq for CustomCursor {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Eq for CustomCursor {}

impl CustomCursor {
    pub(crate) fn new(
        event_loop: &ActiveEventLoop,
        mut cursor: PlatformCustomCursorSource,
    ) -> CustomCursor {
        // Reverse RGBA order to BGRA.
        cursor.0.rgba.chunks_mut(4).for_each(|chunk| {
            chunk[0..3].reverse();
        });

        let cursor = event_loop
            .xconn
            .create_cursor_from_image(
                cursor.0.width,
                cursor.0.height,
                32,
                cursor.0.hotspot_x,
                cursor.0.hotspot_y,
                &cursor.0.rgba,
            )
            .expect("failed to create a custom cursor");

        Self { inner: Arc::new(CustomCursorInner { xconn: event_loop.xconn.clone(), cursor }) }
    }
}

#[derive(Debug)]
struct CustomCursorInner {
    xconn: Arc<XConnection>,
    cursor: xproto::Cursor,
}

impl Drop for CustomCursorInner {
    fn drop(&mut self) {
        self.xconn.xcb_connection().free_cursor(self.cursor).map(|r| r.ignore_error()).ok();
    }
}

impl Default for SelectedCursor {
    fn default() -> Self {
        SelectedCursor::Named(Default::default())
    }
}

struct CallOnDrop<F: FnMut()>(F);
impl<F: FnMut()> Drop for CallOnDrop<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}
