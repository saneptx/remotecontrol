//! macOS 剪贴板（NSPasteboard）读写。

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{msg_send, ClassType};
use objc2_foundation::{NSArray, NSData, NSDictionary, NSString, NSURL};
use rc_protocol::data::{ClipData, MAX_CLIPBOARD};
use std::path::PathBuf;

const TEXT: &str = "public.utf8-plain-text";
const PNG: &str = "public.png";
const TIFF: &str = "public.tiff";
const FILE_URL: &str = "public.file-url";
const NS_BITMAP_IMAGE_FILE_TYPE_PNG: usize = 4;

// 让 NSPasteboard / NSBitmapImageRep 在运行时可用
#[link(name = "AppKit", kind = "framework")]
extern "C" {}

fn general() -> Option<Retained<AnyObject>> {
    let cls = AnyClass::get(c"NSPasteboard")?;
    unsafe { msg_send![cls, generalPasteboard] }
}

/// 变化计数：每次有程序写入剪贴板都会加一。
pub fn change_count() -> isize {
    general().map(|pb| unsafe { msg_send![&*pb, changeCount] }).unwrap_or(0)
}

fn has_type(pb: &AnyObject, ty: &str) -> bool {
    let types = NSArray::from_retained_slice(&[NSString::from_str(ty)]);
    let found: Option<Retained<NSString>> = unsafe { msg_send![pb, availableTypeFromArray: &*types] };
    found.is_some()
}

fn data_for(pb: &AnyObject, ty: &str) -> Option<Retained<NSData>> {
    unsafe { msg_send![pb, dataForType: &*NSString::from_str(ty)] }
}

fn tiff_to_png(tiff: &NSData) -> Option<Vec<u8>> {
    let cls = AnyClass::get(c"NSBitmapImageRep")?;
    let rep: Option<Retained<AnyObject>> = unsafe { msg_send![cls, imageRepWithData: tiff] };
    let props = NSDictionary::<NSString, AnyObject>::new();
    let png: Option<Retained<NSData>> =
        unsafe { msg_send![&*rep?, representationUsingType: NS_BITMAP_IMAGE_FILE_TYPE_PNG, properties: &*props] };
    png.map(|d| d.to_vec())
}

fn png_to_tiff(png: &[u8]) -> Option<Retained<NSData>> {
    let cls = AnyClass::get(c"NSBitmapImageRep")?;
    let data = NSData::with_bytes(png);
    let rep: Option<Retained<AnyObject>> = unsafe { msg_send![cls, imageRepWithData: &*data] };
    unsafe { msg_send![&*rep?, TIFFRepresentation] }
}

/// 读取剪贴板里的文字或图片。复制的是文件（访达里 ⌘C）时返回 `None`。
pub fn read() -> Option<ClipData> {
    let pb = general()?;
    if has_type(&pb, FILE_URL) {
        return None;
    }
    if has_type(&pb, PNG) {
        let png = data_for(&pb, PNG)?.to_vec();
        return (png.len() <= MAX_CLIPBOARD).then_some(ClipData::Image(png));
    }
    if has_type(&pb, TIFF) {
        let png = tiff_to_png(&*data_for(&pb, TIFF)?)?;
        return (png.len() <= MAX_CLIPBOARD).then_some(ClipData::Image(png));
    }
    let s: Option<Retained<NSString>> = unsafe { msg_send![&*pb, stringForType: &*NSString::from_str(TEXT)] };
    let text = s?.to_string();
    (!text.is_empty() && text.len() <= MAX_CLIPBOARD).then_some(ClipData::Text(text))
}

pub fn write(data: &ClipData) {
    let Some(pb) = general() else { return };
    unsafe {
        let _: isize = msg_send![&*pb, clearContents];
        match data {
            ClipData::Text(t) => {
                let _: bool = msg_send![&*pb, setString: &*NSString::from_str(t), forType: &*NSString::from_str(TEXT)];
            }
            ClipData::Image(png) => {
                let _: bool = msg_send![&*pb, setData: &*NSData::with_bytes(png), forType: &*NSString::from_str(PNG)];
                if let Some(tiff) = png_to_tiff(png) {
                    let _: bool = msg_send![&*pb, setData: &*tiff, forType: &*NSString::from_str(TIFF)];
                }
            }
            ClipData::Files { .. } => {}
        }
    }
}

/// 剪贴板里的文件（访达里 ⌘C 复制的）。
pub fn read_files() -> Vec<PathBuf> {
    let Some(pb) = general() else { return Vec::new() };
    if !has_type(&pb, FILE_URL) {
        return Vec::new();
    }
    let Some(cls) = AnyClass::get(c"NSURL") else { return Vec::new() };
    let classes = class_array(cls);
    let opts = NSDictionary::<NSString, AnyObject>::new();
    let urls: Option<Retained<NSArray<NSURL>>> = unsafe { msg_send![&*pb, readObjectsForClasses: &*classes, options: &*opts] };
    urls.map(|a| a.iter().filter_map(|u| u.path().map(|p| PathBuf::from(p.to_string()))).collect()).unwrap_or_default()
}

fn class_array(cls: &AnyClass) -> Retained<NSArray<AnyObject>> {
    let obj: *const AnyClass = cls;
    let obj = obj as *const AnyObject;
    NSArray::from_slice(&[unsafe { &*obj }])
}

/// 把文件放进剪贴板，可在访达里 ⌘V 粘贴。
pub fn write_files(paths: &[PathBuf]) {
    let Some(pb) = general() else { return };
    let urls: Vec<Retained<NSURL>> = paths.iter().map(|p| NSURL::fileURLWithPath(&NSString::from_str(&p.to_string_lossy()))).collect();
    let array = NSArray::from_retained_slice(&urls);
    unsafe {
        let _: isize = msg_send![&*pb, clearContents];
        let _: bool = msg_send![&*pb, writeObjects: &*array];
    }
    let _ = NSURL::class();
}
