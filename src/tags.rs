//! Read/write tags. Keys are always Vorbis-comment style (TITLE, ALBUMARTIST, MUSICBRAINZ_ALBUMID…).
//! FLAC goes through native Vorbis comments so arbitrary custom keys survive; other formats use
//! lofty's generic tag and only keys lofty can map.
// ponytail: Ogg/Opus use the generic path too, so custom (unmapped) keys are skipped there; give them
// the native VorbisComments path like FLAC if the library grows non-FLAC custom-tag needs.

use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::{AudioFile, FileType, TaggedFileExt};
use lofty::flac::FlacFile;
use lofty::ogg::OggPictureStorage;
use lofty::picture::{Picture, PictureInformation, PictureType};
use lofty::prelude::ItemKey;
use lofty::tag::{Tag, TagType};
use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;

pub type Fields = BTreeMap<String, String>;

pub struct Info {
    pub format: String,
    pub bitrate: u32,
    pub sample_rate: u32,
    pub bit_depth: u8,
    pub duration: f64,
    pub has_cover: bool,
    /// shorter side of the front (or first) embedded picture, 0 when none or unreadable
    pub cover_px: u32,
    pub fields: Fields,
}

pub const AUDIO_EXT: &[&str] = &["flac", "mp3", "m4a", "ogg", "opus", "oga", "aac", "wv", "ape", "aiff", "wav"];

pub fn read(path: &Path) -> Result<Info, String> {
    if is_flac(path) {
        // one parse: FlacFile has properties, native comments (unmapped keys too) and picture sizes
        let f = open_flac(path)?;
        let p = f.properties();
        let pics = f.pictures();
        let front = pics.iter().find(|(p, _)| p.pic_type() == PictureType::CoverFront).or(pics.first());
        return Ok(Info {
            format: "FLAC".into(),
            bitrate: p.audio_bitrate(),
            sample_rate: p.sample_rate(),
            bit_depth: p.bit_depth(),
            duration: p.duration().as_secs_f64(),
            has_cover: !pics.is_empty(),
            cover_px: front.and_then(|(p, i)| Some(i.width.min(i.height)).filter(|px| *px > 0).or_else(|| image_px(p.data()))).unwrap_or(0),
            fields: f.vorbis_comments().map(|vc| vc.items().filter(|(_, v)| !v.trim().is_empty()).map(|(k, v)| (k.to_ascii_uppercase(), v.to_string())).collect()).unwrap_or_default(),
        });
    }
    let tf = lofty::read_from_path(path).map_err(|e| e.to_string())?;
    let p = tf.properties();
    let mut info = Info {
        format: format_name(tf.file_type()),
        bitrate: p.audio_bitrate().unwrap_or(0),
        sample_rate: p.sample_rate().unwrap_or(0),
        bit_depth: p.bit_depth().unwrap_or(0),
        duration: p.duration().as_secs_f64(),
        has_cover: tf.tags().iter().any(|t| t.picture_count() > 0),
        cover_px: front(&tf).and_then(|p| image_px(p.data())).unwrap_or(0),
        fields: Fields::new(),
    };
    if let Some(tag) = tf.primary_tag().or(tf.first_tag()) {
        info.fields = tag_fields(tag);
    }
    Ok(info)
}

fn tag_fields(tag: &Tag) -> Fields {
    tag.items()
        .filter_map(|item| Some((item.key().map_key(TagType::VorbisComments)?.to_string(), item.value().text()?.to_string())))
        .filter(|(_, v)| !v.trim().is_empty()) // an empty MUSICBRAINZ_ALBUMID would look like a release id
        .collect()
}

pub fn is_flac(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("flac"))
}

/// Apply `changes` (None = remove key). `cover`: Some(bytes) sets the front cover, Some(empty) removes it.
/// Untyped ("Other") pictures count as the front cover too, so a replacement never leaves two covers.
// FlacFile → TaggedFile moves pictures into the Vorbis tag, so this sees every format's art
fn front(tf: &lofty::file::TaggedFile) -> Option<&Picture> {
    let pics: Vec<&Picture> = tf.tags().iter().flat_map(|t| t.pictures()).collect();
    pics.iter().find(|p| p.pic_type() == PictureType::CoverFront).or(pics.first()).copied()
}

/// Shorter side in pixels of a JPEG/PNG.
pub fn image_px(bytes: &[u8]) -> Option<u32> {
    let pic = Picture::from_reader(&mut &bytes[..]).ok()?;
    PictureInformation::from_picture(&pic).ok().map(|i| i.width.min(i.height))
}

/// The front (or first) embedded picture: (bytes, mime type).
pub fn read_cover(path: &Path) -> Option<(Vec<u8>, String)> {
    let tf = lofty::read_from_path(path).ok()?;
    let p = front(&tf)?;
    Some((p.data().to_vec(), p.mime_type().map_or("image/jpeg", |m| m.as_str()).to_string()))
}

/// One parse, one save. `before(current fields, current front cover)` runs after the file is read and
/// before anything is written (the undo log goes there): Ok(false) skips the write, Err aborts it.
/// Returns whether the file was written.
// ponytail: lofty rewrites the whole FLAC whenever the metadata size changes (it ignores padding,
// lofty#445). On rclone that's free since it uploads the whole file anyway; on a kernel CIFS mount a
// writer that resizes the PADDING block would make tag writes in-place.
pub fn write(
    path: &Path,
    changes: &[(String, Option<String>)],
    cover: Option<&[u8]>,
    before: impl FnOnce(&Fields, Option<&[u8]>) -> Result<bool, String>,
) -> Result<bool, String> {
    if is_flac(path) {
        let mut f = open_flac(path)?;
        let fields: Fields = f.vorbis_comments().map(|vc| vc.items().map(|(k, v)| (k.to_ascii_uppercase(), v.to_string())).collect()).unwrap_or_default();
        let pics = f.pictures();
        let old = pics.iter().find(|(p, _)| p.pic_type() == PictureType::CoverFront).or(pics.first()).map(|(p, _)| p.data().to_vec());
        if !before(&fields, old.as_deref())? {
            return Ok(false);
        }
        if f.vorbis_comments().is_none() {
            f.set_vorbis_comments(Default::default());
        }
        let vc = f.vorbis_comments_mut().unwrap();
        for (k, v) in changes {
            let _ = vc.remove(k).count();
            if let Some(v) = v {
                vc.insert(k.clone(), v.clone());
            }
        }
        if let Some(bytes) = cover {
            f.remove_picture_type(PictureType::CoverFront);
            f.remove_picture_type(PictureType::Other);
            if !bytes.is_empty() {
                f.insert_picture(front_cover(bytes)?, None).map_err(|e| e.to_string())?;
            }
        }
        return f.save_to_path(path, WriteOptions::default()).map(|_| true).map_err(|e| e.to_string());
    }
    let mut tf = lofty::read_from_path(path).map_err(|e| e.to_string())?;
    let fields = tf.primary_tag().or(tf.first_tag()).map(tag_fields).unwrap_or_default();
    let old = front(&tf).map(|p| p.data().to_vec());
    if !before(&fields, old.as_deref())? {
        return Ok(false);
    }
    if tf.primary_tag().is_none() {
        tf.insert_tag(Tag::new(tf.primary_tag_type()));
    }
    let tag = tf.primary_tag_mut().unwrap();
    for (k, v) in changes {
        let Some(key) = ItemKey::from_key(TagType::VorbisComments, k) else { continue };
        tag.remove_key(key);
        if let Some(v) = v {
            tag.insert_text(key, v.clone());
        }
    }
    if let Some(bytes) = cover {
        tag.remove_picture_type(PictureType::CoverFront);
        tag.remove_picture_type(PictureType::Other);
        if !bytes.is_empty() {
            tag.push_picture(front_cover(bytes)?);
        }
    }
    tf.save_to_path(path, WriteOptions::default()).map(|_| true).map_err(|e| e.to_string())
}

fn open_flac(path: &Path) -> Result<FlacFile, String> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    FlacFile::read_from(&mut file, ParseOptions::new()).map_err(|e| e.to_string())
}

fn front_cover(bytes: &[u8]) -> Result<Picture, String> {
    let mut pic = Picture::from_reader(&mut &bytes[..]).map_err(|e| e.to_string())?;
    pic.set_pic_type(PictureType::CoverFront);
    Ok(pic)
}

fn format_name(ft: FileType) -> String {
    match ft {
        FileType::Flac => "FLAC",
        FileType::Mpeg => "MP3",
        FileType::Mp4 => "M4A",
        FileType::Vorbis => "OGG",
        FileType::Opus => "Opus",
        FileType::Aac => "AAC",
        FileType::WavPack => "WavPack",
        FileType::Ape => "APE",
        FileType::Aiff => "AIFF",
        FileType::Wav => "WAV",
        _ => "Other",
    }
    .into()
}
