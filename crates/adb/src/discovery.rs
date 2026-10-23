//! Classification of files discovered in accessible device storage.

use pr_model::model::{Classification, ImageFormat};
use serde::{Deserialize, Serialize};

/// File-name extensions searched on the device (case-insensitive).
pub const IMAGE_EXTENSIONS: [&str; 11] = [
    "jpg", "jpeg", "png", "webp", "heic", "heif", "avif", "gif", "bmp", "dng", "jfif",
];

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum DeviceFileKind {
    /// Camera output (DCIM/Camera and similar).
    CameraOriginal,
    Screenshot,
    /// Messaging/social app media folders (WhatsApp, Telegram, ...).
    AppMedia,
    Download,
    /// Android 11+ MediaStore trash item (`.trashed-<expiry>-<name>`).
    MediaStoreTrash,
    /// Folder whose name suggests a gallery/app recycle bin (heuristic).
    RecycleBinHeuristic,
    /// Thumbnail cache image (e.g. `DCIM/.thumbnails/*.jpg`).
    ThumbnailCache,
    /// Thumbnail container to be carved (e.g. `.thumbdata3-*`).
    ThumbnailContainer,
    OtherImage,
}

impl DeviceFileKind {
    pub fn classification(self) -> Classification {
        match self {
            DeviceFileKind::ThumbnailCache | DeviceFileKind::ThumbnailContainer => {
                Classification::Thumbnail
            }
            _ => Classification::ExistingOriginal,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            DeviceFileKind::CameraOriginal => "camera",
            DeviceFileKind::Screenshot => "screenshot",
            DeviceFileKind::AppMedia => "app media",
            DeviceFileKind::Download => "download",
            DeviceFileKind::MediaStoreTrash => "system trash (Recently deleted)",
            DeviceFileKind::RecycleBinHeuristic => "recycle bin (heuristic)",
            DeviceFileKind::ThumbnailCache => "thumbnail cache",
            DeviceFileKind::ThumbnailContainer => "thumbnail container",
            DeviceFileKind::OtherImage => "other",
        }
    }
}

/// Is this a path we want to acquire? Images by extension plus thumbnail
/// containers which have no extension.
pub fn is_candidate(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or("").to_ascii_lowercase();
    if name.starts_with(".thumbdata") {
        return true;
    }
    let ext = name.rsplit('.').next().unwrap_or("");
    name.contains('.') && IMAGE_EXTENSIONS.contains(&ext)
}

pub fn classify(path: &str) -> DeviceFileKind {
    let lower = path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or("");
    if name.starts_with(".thumbdata") {
        return DeviceFileKind::ThumbnailContainer;
    }
    if name.starts_with(".trashed-") {
        return DeviceFileKind::MediaStoreTrash;
    }
    let dirs: Vec<&str> = lower.split('/').collect();
    let has = |needle: &str| dirs.iter().any(|d| d.contains(needle));
    if has(".thumbnails") || has("thumbnail") || has("thumbcache") || has(".thumbs") {
        return DeviceFileKind::ThumbnailCache;
    }
    if has("recycle") || has("trash") || has(".deleted") || has("recentlydeleted") {
        return DeviceFileKind::RecycleBinHeuristic;
    }
    if has("screenshot") {
        return DeviceFileKind::Screenshot;
    }
    if lower.contains("/dcim/camera/")
        || lower.contains("/dcim/100andro/")
        || lower.contains("/dcim/") && !has("cache")
    {
        return DeviceFileKind::CameraOriginal;
    }
    if lower.contains("/android/media/")
        || has("whatsapp")
        || has("telegram")
        || has("instagram")
        || has("snapchat")
        || has("messenger")
    {
        return DeviceFileKind::AppMedia;
    }
    if has("download") {
        return DeviceFileKind::Download;
    }
    DeviceFileKind::OtherImage
}

pub fn format_for(path: &str) -> ImageFormat {
    ImageFormat::from_extension(path)
}

/// Warnings that must accompany a result of this kind.
pub fn kind_warnings(kind: DeviceFileKind) -> Vec<String> {
    match kind {
        DeviceFileKind::MediaStoreTrash => vec![
            "File is in the system trash (user-deleted, retained until it expires). It still exists on the device; this is a copy of an existing file, not raw deleted-data recovery.".into(),
        ],
        DeviceFileKind::RecycleBinHeuristic => vec![
            "Found in a folder that appears to be an app recycle bin. The file still exists; location identification is heuristic.".into(),
        ],
        DeviceFileKind::ThumbnailCache | DeviceFileKind::ThumbnailContainer => vec![
            "Thumbnail/preview only — lower resolution than the original photo. It is not a substitute for the original.".into(),
        ],
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification() {
        assert_eq!(
            classify("/sdcard/DCIM/Camera/IMG_20240101_1.jpg"),
            DeviceFileKind::CameraOriginal
        );
        assert_eq!(
            classify("/sdcard/DCIM/Camera/.trashed-1712345678-IMG_1.jpg"),
            DeviceFileKind::MediaStoreTrash
        );
        assert_eq!(
            classify("/sdcard/DCIM/.thumbnails/1234.jpg"),
            DeviceFileKind::ThumbnailCache
        );
        assert_eq!(
            classify("/sdcard/DCIM/.thumbnails/.thumbdata4--1967290299"),
            DeviceFileKind::ThumbnailContainer
        );
        assert_eq!(
            classify("/sdcard/Pictures/Screenshots/Screenshot_1.png"),
            DeviceFileKind::Screenshot
        );
        assert_eq!(
            classify("/sdcard/Android/media/com.whatsapp/WhatsApp/Media/WhatsApp Images/IMG-1.jpg"),
            DeviceFileKind::AppMedia
        );
        assert_eq!(
            classify("/sdcard/.RecycleBin/x.jpg"),
            DeviceFileKind::RecycleBinHeuristic
        );
        assert_eq!(
            classify("/sdcard/Download/x.webp"),
            DeviceFileKind::Download
        );
        assert_eq!(
            DeviceFileKind::ThumbnailCache.classification(),
            Classification::Thumbnail
        );
        assert!(is_candidate("/sdcard/a/B.JPEG"));
        assert!(is_candidate(
            "/sdcard/DCIM/.thumbnails/.thumbdata3--1967290299"
        ));
        assert!(!is_candidate("/sdcard/a/notes.txt"));
    }
}
