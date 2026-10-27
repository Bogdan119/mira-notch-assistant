//! Anti-aliased drawing for the notch: GDI+ (flat API) renders into a
//! premultiplied 32-bit DIB that is pushed to a per-pixel-alpha layered window
//! with `UpdateLayeredWindow`. Fully transparent pixels are click-through, so
//! the window can be larger than the shape it shows.
//!
//! All drawing is in logical (96-DPI) units; `begin` applies the DPI scale.

use std::collections::HashMap;
use std::ptr::{null, null_mut};

use windows_sys::Win32::{
    Foundation::*,
    Graphics::{Gdi::*, GdiPlus::*},
    UI::WindowsAndMessaging::*,
};

use crate::wide;

const PIXEL_FORMAT_32BPP_PARGB: i32 = 0xE200B;
const PIXEL_FORMAT_32BPP_ARGB: i32 = 0x26200A;

/// `0xAARRGGBB`, as GDI+ expects.
pub type Argb = u32;

/// Parse a design-token colour (`#RRGGBB` or `#RRGGBBAA`).
pub fn hex(s: &str) -> Argb {
    let s = s.trim_start_matches('#');
    let v = u32::from_str_radix(s, 16).unwrap_or(0xffffff);
    if s.len() == 8 {
        (v >> 8) | ((v & 0xff) << 24)
    } else {
        0xff00_0000 | v
    }
}

/// Multiply a colour's alpha by `a` (0..1).
pub fn fade(c: Argb, a: f32) -> Argb {
    let alpha = ((c >> 24) as f32 * a.clamp(0., 1.)) as u32;
    (alpha << 24) | (c & 0x00ff_ffff)
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct R {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl R {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        R { x, y, w, h }
    }
    pub fn contains(&self, p: (f32, f32)) -> bool {
        p.0 >= self.x && p.0 < self.x + self.w && p.1 >= self.y && p.1 < self.y + self.h
    }
    pub fn inset(&self, d: f32) -> Self {
        R::new(self.x + d, self.y + d, self.w - 2. * d, self.h - 2. * d)
    }
    pub fn cx(&self) -> f32 {
        self.x + self.w / 2.
    }
    pub fn cy(&self) -> f32 {
        self.y + self.h / 2.
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// Font face for a text run.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Face {
    Text,
    Bold,
    Icon,
}

pub struct Canvas {
    token: usize,
    mem_dc: HDC,
    dib: HBITMAP,
    old: HGDIOBJ,
    cap: (i32, i32),
    bitmap: *mut GpBitmap,
    g: *mut GpGraphics,
    brush: *mut GpSolidFill,
    path: *mut GpPath,
    text_family: *mut GpFontFamily,
    icon_family: *mut GpFontFamily,
    fonts: HashMap<(Face, u32), *mut GpFont>,
    formats: HashMap<(u8, bool), *mut GpStringFormat>,
    typographic: *mut GpStringFormat,
    scale: f32,
}

impl Canvas {
    pub unsafe fn new() -> Option<Self> {
        let mut token = 0usize;
        let input = GdiplusStartupInput {
            GdiplusVersion: 1,
            DebugEventCallback: 0,
            SuppressBackgroundThread: 0,
            SuppressExternalCodecs: 0,
        };
        if GdiplusStartup(&mut token, &input, null_mut()) != 0 {
            return None;
        }
        let mut brush = null_mut();
        GdipCreateSolidFill(0xffffffff, &mut brush);
        let mut path = null_mut();
        GdipCreatePath(FillModeAlternate, &mut path);
        let family = |names: &[&str]| {
            for name in names {
                let mut f = null_mut();
                if GdipCreateFontFamilyFromName(wide(name).as_ptr(), null_mut(), &mut f) == 0 {
                    return f;
                }
            }
            null_mut()
        };
        let text_family = family(&["Segoe UI Variable Text", "Segoe UI"]);
        // Windows 11 ships Segoe Fluent Icons; Windows 10 the same code points in MDL2.
        let icon_family = family(&["Segoe Fluent Icons", "Segoe MDL2 Assets"]);
        Some(Canvas {
            token,
            mem_dc: CreateCompatibleDC(null_mut()),
            dib: null_mut(),
            old: null_mut(),
            cap: (0, 0),
            bitmap: null_mut(),
            g: null_mut(),
            brush,
            path,
            text_family,
            icon_family,
            fonts: HashMap::new(),
            formats: HashMap::new(),
            typographic: null_mut(),
            scale: 1.,
        })
    }

    /// Make sure the backing store is at least `w`×`h` device pixels.
    unsafe fn ensure(&mut self, w: i32, h: i32) {
        if w <= self.cap.0 && h <= self.cap.1 && !self.g.is_null() {
            return;
        }
        self.release_surface();
        let (w, h) = (w.max(self.cap.0).max(1), h.max(self.cap.1).max(1));
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = w;
        info.bmiHeader.biHeight = -h; // top-down, like GDI+ scan lines
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB;
        let mut bits = null_mut();
        self.dib = CreateDIBSection(null_mut(), &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        self.old = SelectObject(self.mem_dc, self.dib);
        GdipCreateBitmapFromScan0(
            w,
            h,
            w * 4,
            PIXEL_FORMAT_32BPP_PARGB,
            bits as *const u8,
            &mut self.bitmap,
        );
        GdipGetImageGraphicsContext(self.bitmap as *mut GpImage, &mut self.g);
        GdipSetSmoothingMode(self.g, SmoothingModeAntiAlias);
        GdipSetPixelOffsetMode(self.g, PixelOffsetModeHalf);
        GdipSetTextRenderingHint(self.g, TextRenderingHintAntiAliasGridFit);
        GdipSetInterpolationMode(self.g, InterpolationModeHighQualityBicubic);
        GdipSetCompositingQuality(self.g, CompositingQualityHighQuality);
        self.cap = (w, h);
    }

    unsafe fn release_surface(&mut self) {
        if !self.g.is_null() {
            GdipDeleteGraphics(self.g);
            self.g = null_mut();
        }
        if !self.bitmap.is_null() {
            GdipDisposeImage(self.bitmap as *mut GpImage);
            self.bitmap = null_mut();
        }
        if !self.dib.is_null() {
            SelectObject(self.mem_dc, self.old);
            DeleteObject(self.dib);
            self.dib = null_mut();
        }
    }

    /// Start a frame of `w`×`h` device pixels at DPI `scale`.
    pub unsafe fn begin(&mut self, w: i32, h: i32, scale: f32) {
        self.ensure(w, h);
        self.scale = scale;
        GdipResetWorldTransform(self.g);
        GdipResetClip(self.g);
        GdipGraphicsClear(self.g, 0);
        GdipScaleWorldTransform(self.g, scale, scale, MatrixOrderPrepend);
    }

    /// Push the frame to the layered window at screen position `pos`.
    pub unsafe fn present(&mut self, hwnd: HWND, pos: (i32, i32), size: (i32, i32)) {
        GdipFlush(self.g, FlushIntentionSync);
        let dst = POINT { x: pos.0, y: pos.1 };
        let sz = SIZE {
            cx: size.0,
            cy: size.1,
        };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        UpdateLayeredWindow(
            hwnd,
            null_mut(),
            &dst,
            &sz,
            self.mem_dc,
            &src,
            0,
            &blend,
            ULW_ALPHA,
        );
    }

    // --- transforms -------------------------------------------------------

    pub unsafe fn push(&mut self, dx: f32, dy: f32, sx: f32, sy: f32) {
        GdipTranslateWorldTransform(self.g, dx, dy, MatrixOrderPrepend);
        GdipScaleWorldTransform(self.g, sx, sy, MatrixOrderPrepend);
    }

    pub unsafe fn reset(&mut self) {
        GdipResetWorldTransform(self.g);
        GdipScaleWorldTransform(self.g, self.scale, self.scale, MatrixOrderPrepend);
    }

    pub unsafe fn clip(&mut self, r: R, radius: f32) {
        self.round_path(r, radius);
        GdipSetClipPath(self.g, self.path, CombineModeReplace);
    }

    pub unsafe fn unclip(&mut self) {
        GdipResetClip(self.g);
    }

    // --- shapes -----------------------------------------------------------

    unsafe fn round_path(&mut self, r: R, radius: f32) {
        GdipResetPath(self.path);
        let rad = radius.min(r.w / 2.).min(r.h / 2.).max(0.);
        if rad < 0.5 {
            GdipAddPathRectangle(self.path, r.x, r.y, r.w, r.h);
            return;
        }
        let d = rad * 2.;
        GdipAddPathArc(self.path, r.x, r.y, d, d, 180., 90.);
        GdipAddPathArc(self.path, r.x + r.w - d, r.y, d, d, 270., 90.);
        GdipAddPathArc(self.path, r.x + r.w - d, r.y + r.h - d, d, d, 0., 90.);
        GdipAddPathArc(self.path, r.x, r.y + r.h - d, d, d, 90., 90.);
        GdipClosePathFigure(self.path);
    }

    /// The notch silhouette: square top corners, rounded bottom ones.
    pub unsafe fn notch(&mut self, r: R, radius: f32, color: Argb) {
        GdipResetPath(self.path);
        let rad = radius.min(r.w / 2.).min(r.h).max(0.5);
        let d = rad * 2.;
        GdipAddPathLine(self.path, r.x, r.y, r.x + r.w, r.y);
        GdipAddPathArc(self.path, r.x + r.w - d, r.y + r.h - d, d, d, 0., 90.);
        GdipAddPathArc(self.path, r.x, r.y + r.h - d, d, d, 90., 90.);
        GdipClosePathFigure(self.path);
        GdipSetSolidFillColor(self.brush, color);
        GdipFillPath(self.g, self.brush as *mut GpBrush, self.path);
    }

    pub unsafe fn rrect(&mut self, r: R, radius: f32, color: Argb) {
        self.round_path(r, radius);
        GdipSetSolidFillColor(self.brush, color);
        GdipFillPath(self.g, self.brush as *mut GpBrush, self.path);
    }

    pub unsafe fn rrect_stroke(&mut self, r: R, radius: f32, color: Argb, width: f32) {
        self.round_path(r, radius);
        let mut pen = null_mut();
        GdipCreatePen1(color, width, UnitPixel, &mut pen);
        GdipDrawPath(self.g, pen, self.path);
        GdipDeletePen(pen);
    }

    pub unsafe fn ellipse(&mut self, cx: f32, cy: f32, rx: f32, ry: f32, color: Argb) {
        GdipSetSolidFillColor(self.brush, color);
        GdipFillEllipse(
            self.g,
            self.brush as *mut GpBrush,
            cx - rx,
            cy - ry,
            rx * 2.,
            ry * 2.,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub unsafe fn arc(
        &mut self,
        cx: f32,
        cy: f32,
        r: f32,
        start: f32,
        sweep: f32,
        color: Argb,
        width: f32,
    ) {
        let mut pen = null_mut();
        GdipCreatePen1(color, width, UnitPixel, &mut pen);
        GdipSetPenStartCap(pen, LineCapRound);
        GdipSetPenEndCap(pen, LineCapRound);
        GdipDrawArc(self.g, pen, cx - r, cy - r, r * 2., r * 2., start, sweep);
        GdipDeletePen(pen);
    }

    pub unsafe fn line(&mut self, a: (f32, f32), b: (f32, f32), color: Argb, width: f32) {
        let mut pen = null_mut();
        GdipCreatePen1(color, width, UnitPixel, &mut pen);
        GdipSetPenStartCap(pen, LineCapRound);
        GdipSetPenEndCap(pen, LineCapRound);
        GdipDrawLine(self.g, pen, a.0, a.1, b.0, b.1);
        GdipDeletePen(pen);
    }

    /// A water drop centred at (cx, cy) of height `h`.
    pub unsafe fn drop(&mut self, cx: f32, cy: f32, h: f32, color: Argb) {
        let r = h * 0.32;
        let base = cy + h / 2. - r;
        GdipResetPath(self.path);
        GdipAddPathLine(self.path, cx, cy - h / 2., cx + r * 0.95, base - r * 0.1);
        GdipAddPathArc(self.path, cx - r, base - r, r * 2., r * 2., -10., 200.);
        GdipClosePathFigure(self.path);
        GdipSetSolidFillColor(self.brush, color);
        GdipFillPath(self.g, self.brush as *mut GpBrush, self.path);
    }

    // --- text -------------------------------------------------------------

    unsafe fn font(&mut self, face: Face, size: f32) -> *mut GpFont {
        let key = (face, (size * 10.) as u32);
        if let Some(f) = self.fonts.get(&key) {
            return *f;
        }
        let family = if face == Face::Icon && !self.icon_family.is_null() {
            self.icon_family
        } else {
            self.text_family
        };
        let style = if face == Face::Bold {
            FontStyleBold
        } else {
            FontStyleRegular
        };
        let mut font = null_mut();
        GdipCreateFont(family, size, style, UnitPixel, &mut font);
        self.fonts.insert(key, font);
        font
    }

    unsafe fn format(&mut self, align: Align, wrap: bool) -> *mut GpStringFormat {
        let key = (align as u8, wrap);
        if let Some(f) = self.formats.get(&key) {
            return *f;
        }
        let mut f = null_mut();
        GdipCreateStringFormat(0, 0, &mut f);
        GdipSetStringFormatAlign(
            f,
            match align {
                Align::Left => StringAlignmentNear,
                Align::Center => StringAlignmentCenter,
                Align::Right => StringAlignmentFar,
            },
        );
        GdipSetStringFormatLineAlign(
            f,
            if wrap {
                StringAlignmentNear
            } else {
                StringAlignmentCenter
            },
        );
        GdipSetStringFormatTrimming(
            f,
            if wrap {
                StringTrimmingEllipsisWord
            } else {
                StringTrimmingEllipsisCharacter
            },
        );
        if !wrap {
            GdipSetStringFormatFlags(f, StringFormatFlagsNoWrap);
        }
        self.formats.insert(key, f);
        f
    }

    pub unsafe fn text(&mut self, s: &str, r: R, size: f32, face: Face, color: Argb, align: Align) {
        self.text_run(s, r, size, face, color, align, false);
    }

    /// Multi-line text, word-wrapped inside `r`.
    pub unsafe fn para(&mut self, s: &str, r: R, size: f32, color: Argb) {
        self.text_run(s, r, size, Face::Text, color, Align::Left, true);
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn text_run(
        &mut self,
        s: &str,
        r: R,
        size: f32,
        face: Face,
        color: Argb,
        align: Align,
        wrap: bool,
    ) {
        if s.is_empty() {
            return;
        }
        let font = self.font(face, size);
        let format = self.format(align, wrap);
        let w: Vec<u16> = s.encode_utf16().collect();
        let rect = RectF {
            X: r.x,
            Y: r.y,
            Width: r.w,
            Height: r.h,
        };
        GdipSetSolidFillColor(self.brush, color);
        GdipDrawString(
            self.g,
            w.as_ptr(),
            w.len() as i32,
            font,
            &rect,
            format,
            self.brush as *const GpBrush,
        );
    }

    /// An icon-font glyph (Segoe Fluent Icons / MDL2 code point) centred in `r`.
    pub unsafe fn icon(&mut self, glyph: char, r: R, size: f32, color: Argb) {
        let mut buf = [0u8; 4];
        self.text(
            glyph.encode_utf8(&mut buf),
            r,
            size,
            Face::Icon,
            color,
            Align::Center,
        );
    }

    /// Width of `s` in logical units.
    pub unsafe fn measure(&mut self, s: &str, size: f32, face: Face) -> f32 {
        if s.is_empty() {
            return 0.;
        }
        let font = self.font(face, size);
        if self.typographic.is_null() {
            let mut generic = null_mut();
            GdipStringFormatGetGenericTypographic(&mut generic);
            GdipCloneStringFormat(generic, &mut self.typographic);
        }
        let f = self.typographic;
        let w: Vec<u16> = s.encode_utf16().collect();
        let layout = RectF {
            X: 0.,
            Y: 0.,
            Width: 10000.,
            Height: 1000.,
        };
        let mut out: RectF = std::mem::zeroed();
        GdipMeasureString(
            self.g,
            w.as_ptr(),
            w.len() as i32,
            font,
            &layout,
            f,
            &mut out,
            null_mut(),
            null_mut(),
        );
        // Trailing spaces are not measured by the typographic format.
        let spaces = s.chars().rev().take_while(|c| *c == ' ').count() as f32;
        out.Width + spaces * size * 0.28
    }

    pub unsafe fn image(&mut self, img: *mut GpBitmap, r: R) {
        if !img.is_null() {
            GdipDrawImageRect(self.g, img as *mut GpImage, r.x, r.y, r.w, r.h);
        }
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        unsafe {
            for f in self.fonts.values() {
                GdipDeleteFont(*f);
            }
            for f in self.formats.values() {
                GdipDeleteStringFormat(*f);
            }
            if !self.typographic.is_null() {
                GdipDeleteStringFormat(self.typographic);
            }
            if !self.text_family.is_null() {
                GdipDeleteFontFamily(self.text_family);
            }
            if !self.icon_family.is_null() {
                GdipDeleteFontFamily(self.icon_family);
            }
            GdipDeleteBrush(self.brush as *mut GpBrush);
            GdipDeletePath(self.path);
            self.release_surface();
            DeleteDC(self.mem_dc);
            GdiplusShutdown(self.token);
        }
    }
}

/// Convert an `HICON` (with alpha, or a mask for old icons) into a GDI+ bitmap.
/// The icon is not destroyed.
pub unsafe fn bitmap_from_icon(icon: HICON) -> *mut GpBitmap {
    let mut info: ICONINFO = std::mem::zeroed();
    if GetIconInfo(icon, &mut info) == 0 {
        return null_mut();
    }
    let mut bm: BITMAP = std::mem::zeroed();
    let color = if info.hbmColor.is_null() {
        info.hbmMask
    } else {
        info.hbmColor
    };
    GetObjectW(
        color,
        std::mem::size_of::<BITMAP>() as i32,
        &mut bm as *mut _ as *mut _,
    );
    let (w, h) = (bm.bmWidth, bm.bmHeight.abs());
    let result = if w > 0 && h > 0 && !info.hbmColor.is_null() {
        let dc = CreateCompatibleDC(null_mut());
        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = w;
        bi.bmiHeader.biHeight = -h;
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = BI_RGB;
        let mut px = vec![0u32; (w * h) as usize];
        GetDIBits(
            dc,
            info.hbmColor,
            0,
            h as u32,
            px.as_mut_ptr() as *mut _,
            &mut bi,
            DIB_RGB_COLORS,
        );
        if px.iter().all(|p| p >> 24 == 0) {
            // No alpha channel: derive it from the AND mask.
            let mut mask = vec![0u32; (w * h) as usize];
            GetDIBits(
                dc,
                info.hbmMask,
                0,
                h as u32,
                mask.as_mut_ptr() as *mut _,
                &mut bi,
                DIB_RGB_COLORS,
            );
            for (p, m) in px.iter_mut().zip(&mask) {
                if m & 0x00ff_ffff == 0 {
                    *p |= 0xff00_0000;
                }
            }
        }
        DeleteDC(dc);
        let mut bmp = null_mut();
        GdipCreateBitmapFromScan0(w, h, 0, PIXEL_FORMAT_32BPP_ARGB, null(), &mut bmp);
        if !bmp.is_null() {
            let rect = Rect {
                X: 0,
                Y: 0,
                Width: w,
                Height: h,
            };
            let mut data: BitmapData = std::mem::zeroed();
            if GdipBitmapLockBits(
                bmp,
                &rect,
                ImageLockModeWrite as u32,
                PIXEL_FORMAT_32BPP_ARGB,
                &mut data,
            ) == 0
            {
                for y in 0..h as usize {
                    let row = (data.Scan0 as *mut u8).offset(y as isize * data.Stride as isize)
                        as *mut u32;
                    std::ptr::copy_nonoverlapping(px.as_ptr().add(y * w as usize), row, w as usize);
                }
                GdipBitmapUnlockBits(bmp, &mut data);
            }
        }
        bmp
    } else {
        null_mut()
    };
    if !info.hbmColor.is_null() {
        DeleteObject(info.hbmColor);
    }
    if !info.hbmMask.is_null() {
        DeleteObject(info.hbmMask);
    }
    result
}

pub unsafe fn free_bitmap(b: *mut GpBitmap) {
    if !b.is_null() {
        GdipDisposeImage(b as *mut GpImage);
    }
}
