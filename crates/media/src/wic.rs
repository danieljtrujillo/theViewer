//! Still images through the Windows Imaging Component.
//!
//! The same argument as the video path, one layer up. HEIF and AVIF need an HEVC and an AV1
//! decoder respectively; Windows already has the machinery to host both, sold as Store extensions
//! and installed on a great many machines. Bundling `libheif` and `dav1d` to decode files the
//! operating system can already decode would add tens of megabytes for a worse result.
//!
//! WIC is also the escape hatch for everything else Windows learns to read, camera RAW through a
//! vendor codec, JPEG XR, whatever ships next, without this crate changing. So it is used as a
//! **fallback** rather than only for the two formats that need it: if the bundled Rust decoders
//! decline a file, this gets a turn before the viewer gives up.

#![allow(unsafe_code)]

use std::path::Path;

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppRGBA, IWICImagingFactory,
    WICBitmapDitherTypeNone, WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};

use crate::MediaError;

/// One decoded picture, in the RGBA8 the renderer uploads.
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// What WIC says it used, for the inspector.
    pub decoder: String,
}

/// Refuse anything that would not fit in memory twice over before asking for it.
const MAX_PIXELS: u64 = 512 * 1024 * 1024;

fn platform(e: windows::core::Error) -> MediaError {
    MediaError::Platform(e.message().to_string())
}

pub fn decode(path: &Path) -> Result<DecodedImage, MediaError> {
    // SAFETY: COM has to be initialized on this thread before any of the calls below. Repeated
    // initialization on an already-initialized thread returns a benign already-set code.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    let url = HSTRING::from(path.as_os_str());
    unsafe {
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
                .map_err(platform)?;

        // Decoded from a filename rather than a stream: WIC's own memory mapping is better than
        // anything achieved by handing it bytes, and some codecs consult the extension.
        let decoder = factory
            .CreateDecoderFromFilename(
                PCWSTR(url.as_ptr()),
                None,
                windows::Win32::Foundation::GENERIC_READ,
                WICDecodeMetadataCacheOnDemand,
            )
            .map_err(|_| {
                MediaError::NoDecoder(
                    path.extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("this image")
                        .to_ascii_uppercase(),
                )
            })?;

        let name = decoder
            .GetDecoderInfo()
            .ok()
            .and_then(|info| {
                let mut buffer = [0u16; 128];
                let mut written = 0u32;
                info.GetFriendlyName(&mut buffer, &mut written).ok()?;
                Some(String::from_utf16_lossy(
                    &buffer[..written.saturating_sub(1) as usize],
                ))
            })
            .unwrap_or_default();

        // Frame zero. Animated HEIF and multi-page TIFF have more; the first is the picture.
        let frame = decoder.GetFrame(0).map_err(platform)?;
        let mut width = 0u32;
        let mut height = 0u32;
        frame.GetSize(&mut width, &mut height).map_err(platform)?;
        if width == 0 || height == 0 {
            return Err(MediaError::Platform("the image has no pixels".into()));
        }
        if width as u64 * height as u64 > MAX_PIXELS {
            return Err(MediaError::Platform(format!(
                "{width} x {height} is larger than this viewer will decode"
            )));
        }

        // The converter is what turns whatever the codec produced, 10-bit YUV for HEIF, planar
        // float for some RAW, into one predictable layout.
        let converter = factory.CreateFormatConverter().map_err(platform)?;
        converter
            .Initialize(
                &frame,
                &GUID_WICPixelFormat32bppRGBA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
            .map_err(platform)?;

        let stride = width * 4;
        let mut rgba = vec![0u8; (stride * height) as usize];
        converter
            .CopyPixels(std::ptr::null(), stride, &mut rgba)
            .map_err(platform)?;

        Ok(DecodedImage {
            width,
            height,
            rgba,
            decoder: name,
        })
    }
}

/// Whether WIC has a decoder for this file, without decoding it.
///
/// Used to tell "the machine is missing the HEIF extension" apart from "the file is broken",
/// which are the same failure to a user and completely different problems.
pub fn can_decode(path: &Path) -> bool {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let Ok(factory) = CoCreateInstance::<_, IWICImagingFactory>(
            &CLSID_WICImagingFactory,
            None,
            CLSCTX_INPROC_SERVER,
        ) else {
            return false;
        };
        let url = HSTRING::from(path.as_os_str());
        factory
            .CreateDecoderFromFilename(
                PCWSTR(url.as_ptr()),
                None,
                windows::Win32::Foundation::GENERIC_READ,
                WICDecodeMetadataCacheOnDemand,
            )
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A four-pixel PNG, written by hand so the test needs no encoder.
    fn tiny_png() -> Vec<u8> {
        // 2x2 RGB: red, green / blue, white.
        const PNG: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x02, 0x00, 0x00,
            0x00, 0xFD, 0xD4, 0x9A, 0x73, 0x00, 0x00, 0x00, 0x16, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0xF8, 0xCF, 0xC0, 0xC0, 0xF0, 0x9F, 0x81, 0x81, 0xE1, 0x3F, 0x03, 0x03,
            0x03, 0x00, 0x1B, 0x0B, 0x03, 0x03, 0x1C, 0x1F, 0x93, 0xE1, 0x00, 0x00, 0x00, 0x00,
            0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        PNG.to_vec()
    }

    /// Write bytes to a uniquely-named file in the temp directory, and remove it afterwards.
    struct Temp(std::path::PathBuf);

    impl Temp {
        fn new(name: &str, bytes: &[u8]) -> Temp {
            let path =
                std::env::temp_dir().join(format!("viewer-wic-{}-{name}", std::process::id()));
            std::fs::write(&path, bytes).expect("the temp directory should be writable");
            Temp(path)
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn the_platform_path_decodes_a_format_windows_has_always_known() {
        // PNG is not why this code exists, which is exactly what makes it the right test subject:
        // it proves the COM plumbing, the format conversion and the pixel copy without depending
        // on which Store extensions happen to be installed on the machine running the suite.
        let file = Temp::new("probe.png", &tiny_png());
        let image = decode(&file.0).expect("Windows has decoded PNG since before WIC existed");
        assert_eq!((image.width, image.height), (2, 2));
        assert_eq!(image.rgba.len(), 2 * 2 * 4);
        assert!(image.rgba.chunks_exact(4).all(|p| p[3] == 255));
        assert!(!image.decoder.is_empty(), "the decoder should name itself");
    }

    #[test]
    fn a_file_the_platform_cannot_read_reports_a_missing_decoder() {
        let file = Temp::new("nonsense.bin", b"this is not an image of any kind");
        assert!(matches!(
            decode(&file.0),
            Err(MediaError::NoDecoder(_)) | Err(MediaError::Platform(_))
        ));
        assert!(!can_decode(&file.0));
    }

    #[test]
    fn a_path_that_does_not_exist_fails_without_panicking() {
        let missing = std::env::temp_dir().join("viewer-wic-does-not-exist.png");
        assert!(decode(&missing).is_err());
        assert!(!can_decode(&missing));
    }

    #[test]
    fn availability_can_be_asked_without_decoding() {
        let file = Temp::new("probe2.png", &tiny_png());
        assert!(can_decode(&file.0));
    }
}
