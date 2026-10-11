//! Extract the Windows shell icon for a file extension or folder and return it
//! as a PNG. Per-extension, so the UI caches one icon per type. Best-effort: any
//! failure returns `None` and the UI falls back to a glyph.

use std::ffi::c_void;
use std::iter::once;
use std::mem::{size_of, zeroed};
use std::ptr;

use windows_sys::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO,
};
use windows_sys::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW};
use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, ICONINFO};

const SHGFI_TYPENAME: u32 = 0x0000_0400;
const SHGFI_USEFILEATTRIBUTES: u32 = 0x0000_0010;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
const BI_RGB: u32 = 0;
const DIB_RGB_COLORS: u32 = 0;

/// PNG of the 256 px ("jumbo") shell icon for an extension or folder, the size Explorer's thumbnails use, so a
/// grid tile's placeholder matches the thumbnail that replaces it. Needs a COM-initialised thread.
pub fn jumbo_png(ext: Option<&str>, is_dir: bool) -> Option<Vec<u8>> {
    use windows_sys::Win32::UI::Controls::ImageList_GetIcon;
    use windows_sys::Win32::UI::Shell::SHGetImageList;
    const SHGFI_SYSICONINDEX: u32 = 0x4000;
    const SHIL_JUMBO: i32 = 4;
    const ILD_TRANSPARENT: u32 = 1;
    const IID_IIMAGELIST: windows_sys::core::GUID = windows_sys::core::GUID::from_u128(0x46eb5926_582e_4017_9fdf_e8998daa0950);
    let name = if is_dir { "folder".to_string() } else { format!("x.{}", ext.unwrap_or("dat")) };
    let wide: Vec<u16> = name.encode_utf16().chain(once(0)).collect();
    let attrs = if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL };
    unsafe {
        let mut info: SHFILEINFOW = zeroed();
        let r = SHGetFileInfoW(wide.as_ptr(), attrs, &mut info, size_of::<SHFILEINFOW>() as u32, SHGFI_SYSICONINDEX | SHGFI_USEFILEATTRIBUTES);
        if r == 0 {
            return None;
        }
        let mut list: *mut c_void = ptr::null_mut();
        if SHGetImageList(SHIL_JUMBO, &IID_IIMAGELIST, &mut list) < 0 || list.is_null() {
            return None;
        }
        // the IImageList pointer doubles as an HIMAGELIST; the system image list is shared, so it is not released
        let hicon = ImageList_GetIcon(list as _, info.iIcon, ILD_TRANSPARENT);
        if hicon.is_null() {
            return None;
        }
        let png = icon_to_png(hicon);
        DestroyIcon(hicon);
        png
    }
}

/// The registry's friendly type name for an extension (e.g. `"Text Document"`)
/// or `"File folder"`. Resolved from the extension alone via
/// `SHGFI_USEFILEATTRIBUTES`, so it touches the registry but never the disk.
pub fn type_name(ext: Option<&str>, is_dir: bool) -> Option<String> {
    let name = if is_dir {
        "folder".to_string()
    } else {
        format!("x.{}", ext.unwrap_or("dat"))
    };
    let wide: Vec<u16> = name.encode_utf16().chain(once(0)).collect();
    let attrs = if is_dir {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    };

    let mut info: SHFILEINFOW = unsafe { zeroed() };
    let res = unsafe {
        SHGetFileInfoW(
            wide.as_ptr(),
            attrs,
            &mut info,
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_TYPENAME | SHGFI_USEFILEATTRIBUTES,
        )
    };
    if res == 0 {
        return None;
    }
    let len = info
        .szTypeName
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(info.szTypeName.len());
    let label = String::from_utf16_lossy(&info.szTypeName[..len]);
    (!label.is_empty()).then_some(label)
}

/// Render an `HICON`'s colour bitmap to a top-down RGBA buffer and PNG-encode it.
unsafe fn icon_to_png(hicon: *mut c_void) -> Option<Vec<u8>> {
    let (rgba, w, h) = icon_rgba(hicon)?;
    encode_png(&rgba, w, h)
}

/// An `HICON`'s pixels as top-down RGBA (straight alpha) with its size.
pub unsafe fn icon_rgba(hicon: *mut c_void) -> Option<(Vec<u8>, u32, u32)> {
    let mut icon_info: ICONINFO = zeroed();
    if GetIconInfo(hicon, &mut icon_info) == 0 {
        return None;
    }
    let hbm_color = icon_info.hbmColor;
    let hbm_mask = icon_info.hbmMask;

    let result = (|| {
        let mut bitmap: BITMAP = zeroed();
        if GetObjectW(
            hbm_color,
            size_of::<BITMAP>() as i32,
            &mut bitmap as *mut _ as *mut c_void,
        ) == 0
        {
            return None;
        }
        let (w, h) = (bitmap.bmWidth, bitmap.bmHeight);
        if w <= 0 || h <= 0 {
            return None;
        }
        let (wu, hu) = (w as usize, h as usize);

        let mut bmi: BITMAPINFO = zeroed();
        bmi.bmiHeader.biSize =
            size_of::<windows_sys::Win32::Graphics::Gdi::BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = w;
        bmi.bmiHeader.biHeight = -h; // top-down
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB;

        let hdc = GetDC(ptr::null_mut());
        let mut color = vec![0u8; wu * hu * 4];
        let got = GetDIBits(
            hdc,
            hbm_color,
            0,
            hu as u32,
            color.as_mut_ptr() as *mut c_void,
            &mut bmi,
            DIB_RGB_COLORS,
        );
        let mut mask = vec![0u8; wu * hu * 4];
        GetDIBits(
            hdc,
            hbm_mask,
            0,
            hu as u32,
            mask.as_mut_ptr() as *mut c_void,
            &mut bmi,
            DIB_RGB_COLORS,
        );
        ReleaseDC(ptr::null_mut(), hdc);
        if got == 0 {
            return None;
        }

        // 32-bpp icons carry alpha; older ones are flat, so fall back to the
        // AND mask (white = transparent) when no alpha is present.
        let has_alpha = color.chunks_exact(4).any(|px| px[3] != 0);
        let mut rgba = vec![0u8; wu * hu * 4];
        for i in 0..wu * hu {
            let b = color[i * 4];
            let g = color[i * 4 + 1];
            let r = color[i * 4 + 2];
            let a = if has_alpha {
                color[i * 4 + 3]
            } else if mask[i * 4] == 0 {
                255
            } else {
                0
            };
            rgba[i * 4] = r;
            rgba[i * 4 + 1] = g;
            rgba[i * 4 + 2] = b;
            rgba[i * 4 + 3] = a;
        }
        Some((rgba, wu as u32, hu as u32))
    })();

    DeleteObject(hbm_color);
    if !hbm_mask.is_null() {
        DeleteObject(hbm_mask);
    }
    result
}

pub fn encode_png(rgba: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().ok()?;
        writer.write_image_data(rgba).ok()?;
    }
    Some(out)
}
