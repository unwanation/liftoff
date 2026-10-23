//! Artifacts: a user's HTML page with the files next to it, in
//! versions, private with a TTL or shared for good.

use core::fmt;

use sha2::{Digest as _, Sha256};

use super::ids::UserId;
use super::secret::VersionNumber;
use super::timestamp::Timestamp;
use super::token::SecretBody;
use crate::domain::Visibility;
use crate::error::{Problem, ValidationError};

/// The identifier of an artifact: 12 base62 characters.
///
/// They are drawn from the [`Rng`](crate::ports::Rng) (about 71 bits, as
/// in the standalone artifacts server): a shared artifact's link is public, so its id
/// must not be guessable.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ArtifactId([u8; 12]);

impl ArtifactId {
    /// Number of characters.
    pub const LEN: usize = 12;
    /// Human-readable shape for errors.
    pub const PATTERN: &'static str = "12 characters of [0-9A-Za-z]";

    /// The first [`ArtifactId::LEN`] characters of a random secret body
    /// (`Rng::token_secret`): how a service mints a new id.
    #[must_use]
    pub fn from_secret(body: &SecretBody) -> Self {
        let mut id = [0_u8; Self::LEN];
        for (slot, byte) in id.iter_mut().zip(body.as_str().bytes()) {
            *slot = byte;
        }
        Self(id)
    }

    /// Validates 12 base62 characters (an id in a URL, a stored row).
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] on field `artifact` otherwise.
    pub fn parse(raw: &str) -> Result<Self, ValidationError> {
        let bytes = raw.as_bytes();
        if bytes.len() == Self::LEN && bytes.iter().all(u8::is_ascii_alphanumeric) {
            let mut id = [0_u8; Self::LEN];
            id.copy_from_slice(bytes);
            Ok(Self(id))
        } else {
            Err(ValidationError::new(
                "artifact",
                Problem::Pattern {
                    expected: Self::PATTERN,
                },
            ))
        }
    }

    /// The 12 characters.
    #[must_use]
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.0).unwrap_or_default()
    }
}

impl fmt::Display for ArtifactId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for ArtifactId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ArtifactId({})", self.as_str())
    }
}

/// The relative path of a file of an artifact version.
///
/// It is normalised (`.` and empty segments dropped: `a/./b` is `a/b`), with no
/// segment `..`, none starting with a dot (no hidden files), none longer
/// than [`ArtifactPath::MAX_SEGMENT_BYTES`] and the whole at most
/// [`ArtifactPath::MAX_BYTES`], not
/// absolute, without backslashes or control characters, and never
/// `index.html`, which is the version's HTML itself
/// ([`ArtifactPath::index`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ArtifactPath(String);

impl ArtifactPath {
    /// The file of a version that holds its HTML.
    pub const INDEX: &'static str = "index.html";

    /// The longest segment, in bytes: a file name on disk (`NAME_MAX`).
    pub const MAX_SEGMENT_BYTES: usize = 255;

    /// The longest normalised path, in bytes.
    pub const MAX_BYTES: usize = 1024;

    /// Validates and normalises the name of an uploaded file.
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] on field `files` whose path is `raw` (the
    /// name as it was sent, ART-02) for a name that breaks a rule above.
    pub fn new(raw: &str) -> Result<Self, ValidationError> {
        let path = Self::normalise(raw)?;
        if path.0 == Self::INDEX {
            return Err(ValidationError::at(
                "files",
                raw,
                Problem::Invalid("is the version's HTML; send it as `html`".to_owned()),
            ));
        }
        Ok(path)
    }

    /// The rules of [`ArtifactPath::new`] but the one about `index.html`.
    fn normalise(raw: &str) -> Result<Self, ValidationError> {
        let refuse =
            |why: &str| ValidationError::at("files", raw, Problem::Invalid(why.to_owned()));
        if raw.is_empty() {
            return Err(ValidationError::at("files", raw, Problem::Empty));
        }
        if raw.starts_with('/') {
            return Err(refuse("must be a relative path"));
        }
        if raw.contains('\\') {
            return Err(refuse("must not contain a backslash"));
        }
        if raw.chars().any(char::is_control) {
            return Err(refuse("must not contain control characters"));
        }
        let mut segments = Vec::new();
        for segment in raw.split('/') {
            match segment {
                "" | "." => {}
                ".." => return Err(refuse("must not contain a `..` segment")),
                hidden if hidden.starts_with('.') => {
                    return Err(refuse("must not contain a segment starting with a dot"));
                }
                long if long.len() > Self::MAX_SEGMENT_BYTES => {
                    return Err(refuse("must not contain a segment longer than 255 bytes"));
                }
                visible => segments.push(visible),
            }
        }
        if segments.is_empty() {
            return Err(ValidationError::at("files", raw, Problem::Empty));
        }
        let path = segments.join("/");
        if path.len() > Self::MAX_BYTES {
            return Err(refuse("must not be longer than 1024 bytes"));
        }
        Ok(Self(path))
    }

    /// `index.html`, the version's HTML.
    #[must_use]
    pub fn index() -> Self {
        Self(Self::INDEX.to_owned())
    }

    /// A name read back from the store or from the files of a version:
    /// [`ArtifactPath::new`]'s rules, except that `index.html` is the
    /// version's HTML.
    ///
    /// # Errors
    ///
    /// As [`ArtifactPath::new`].
    pub fn stored(raw: &str) -> Result<Self, ValidationError> {
        Self::normalise(raw)
    }

    /// The normalised path.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ArtifactPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The SHA-256 of a file's bytes, 64 lower-case hex characters: the key
/// of a blob, so a content is stored once whatever versions use it
/// (ART-13).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlobHash(String);

/// The lower-case hex digits.
const HEX: &[u8; 16] = b"0123456789abcdef";

impl BlobHash {
    /// Number of hex characters.
    pub const LEN: usize = 64;

    /// The hash of `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        let digest = Sha256::digest(bytes);
        let mut hex = String::with_capacity(Self::LEN);
        for byte in digest {
            for nibble in [byte >> 4, byte & 0x0f] {
                hex.push(char::from_digit(u32::from(nibble), 16).unwrap_or('0'));
            }
        }
        Self(hex)
    }

    /// Parses 64 lower-case hex characters (read back from the store).
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] on field `hash` otherwise.
    pub fn parse(raw: &str) -> Result<Self, ValidationError> {
        if raw.len() == Self::LEN && raw.bytes().all(|byte| HEX.contains(&byte)) {
            Ok(Self(raw.to_owned()))
        } else {
            Err(ValidationError::new(
                "hash",
                Problem::Pattern {
                    expected: "64 characters of [0-9a-f]",
                },
            ))
        }
    }

    /// The 64 hex characters.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BlobHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The longest title kept, in characters (as the standalone artifacts server).
const MAX_TITLE: usize = 200;

/// `raw` with its runs of whitespace collapsed to one space, trimmed and
/// cut to 200 characters; `None` when nothing is left.
#[must_use]
pub fn clean_title(raw: &str) -> Option<String> {
    let text: String = raw
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_TITLE)
        .collect();
    (!text.is_empty()).then_some(text)
}

/// The `<title>` of an HTML page, as the standalone artifacts server reads it.
///
/// Cleaned ([`clean_title`]) with its character references decoded
/// (`&amp;`, `&lt;`, `&gt;`, `&quot;`, `&apos;` and numeric ones); `None`
/// without one.
#[must_use]
pub fn title_of(html: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(html);
    // ASCII lower-casing keeps every byte where it was, so an offset in
    // `lower` is one in `text`.
    let lower = text.to_ascii_lowercase();
    let (_before, after) = lower.split_once("<title")?;
    // `<title>` or `<title …>`, never `<titles>`.
    if !after.starts_with(['>', ' ', '\t', '\n', '\r', '/']) {
        return None;
    }
    let (_attributes, inner) = after.split_once('>')?;
    let (title, _rest) = inner.split_once("</title>")?;
    let start = lower.len().checked_sub(inner.len())?;
    let end = start.checked_add(title.len())?;
    clean_title(&decode_entities(text.get(start..end)?))
}

/// Decodes the character references [`title_of`] knows; any other `&`
/// stays as it is.
fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((before, tail)) = rest.split_once('&') {
        out.push_str(before);
        let decoded = tail
            .split_once(';')
            .and_then(|(name, after)| entity(name).map(|c| (c, after)));
        if let Some((c, after)) = decoded {
            out.push(c);
            rest = after;
        } else {
            out.push('&');
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

/// The character the reference `&name;` stands for.
fn entity(name: &str) -> Option<char> {
    let lower = name.to_ascii_lowercase();
    let code = if let Some(hex) = lower.strip_prefix("#x") {
        u32::from_str_radix(hex, 16).ok()
    } else if let Some(decimal) = lower.strip_prefix('#') {
        decimal.parse().ok()
    } else {
        return match lower.as_str() {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => None,
        };
    };
    code.and_then(char::from_u32)
}

/// An artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    /// Identifier, 12 base62 characters.
    pub id: ArtifactId,
    /// Who published it.
    pub owner_user_id: UserId,
    /// The title of the current version.
    pub title: String,
    /// `private` (with a TTL) or `shared` (for good).
    pub visibility: Visibility,
    /// The version `/a/{id}` serves.
    pub current_version: VersionNumber,
    /// When a private artifact expires; `None` while it is shared.
    pub expires_at: Option<Timestamp>,
    /// Creation time (of version 1).
    pub created_at: Timestamp,
    /// When it last changed (a version, share or unshare).
    pub updated_at: Timestamp,
}

/// A file of an artifact version: its normalised path, the hash of its
/// bytes and their number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactFile {
    /// The relative path (`index.html` for the HTML).
    pub name: ArtifactPath,
    /// SHA-256 of the bytes.
    pub hash: BlobHash,
    /// Number of bytes.
    pub size: u64,
}

/// A version of an artifact: the HTML and
/// its files as a whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactVersion {
    /// The artifact.
    pub artifact_id: ArtifactId,
    /// The number, from 1; unique per artifact.
    pub number: VersionNumber,
    /// The title of this version.
    pub title: String,
    /// The sum of the files' sizes (`index.html` included).
    pub size_bytes: u64,
    /// Who published the version.
    pub created_by: UserId,
    /// When.
    pub created_at: Timestamp,
    /// Its files, `index.html` first, then by path.
    pub files: Vec<ArtifactFile>,
}

#[cfg(test)]
mod tests {
    use super::*;

    mod artifact_id {
        use super::*;

        // TC: ART-01 (the id is the first 12 characters of a random
        // secret)
        #[test]
        fn takes_first_twelve_characters_when_made_from_secret() {
            let body = SecretBody::new("0123456789abcdefghijABCDEFGHIJ0123456789").unwrap();
            assert_eq!(ArtifactId::from_secret(&body).as_str(), "0123456789ab");
        }

        // TC: ART-01
        #[test]
        fn reads_back_id_when_twelve_base62_characters() {
            assert_eq!(
                ArtifactId::parse("aB3dE6gH9jK2").map(|id| id.to_string()),
                Ok("aB3dE6gH9jK2".to_owned())
            );
        }

        // TC: ART-01 (anything but 12 base62 characters is no id)
        #[test]
        fn rejects_id_when_length_or_alphabet_wrong() {
            let fields: Vec<Option<&'static str>> = [
                "aB3dE6gH9jK",
                "aB3dE6gH9jK2X",
                "aB3dE6gH9j-2",
                "aB3dE6gH9j/2",
                "aB3dE6gH9jé",
                "",
            ]
            .into_iter()
            .map(|raw| ArtifactId::parse(raw).err().map(|error| error.field))
            .collect();
            assert_eq!(fields, vec![Some("artifact"); 6]);
        }
    }

    mod artifact_path {
        use super::*;

        /// The field and path of the error for `raw`, if refused.
        fn refusal(raw: &str) -> Option<(&'static str, Option<String>)> {
            ArtifactPath::new(raw)
                .err()
                .map(|error| (error.field, error.path))
        }

        /// What [`refusal`] says of `raw`: field `files`, path `raw`.
        fn names(raw: &str) -> (&'static str, Option<String>) {
            ("files", Some(raw.to_owned()))
        }

        // TC: ART-02
        #[test]
        fn refuses_name_with_its_path_when_a_segment_is_dot_dot() {
            assert_eq!(
                [refusal("../secret.png"), refusal("img/../../x.png")],
                [Some(names("../secret.png")), Some(names("img/../../x.png"))]
            );
        }

        // TC: ART-02 (no hidden files)
        #[test]
        fn refuses_name_when_a_segment_starts_with_a_dot() {
            assert_eq!(
                [refusal(".env"), refusal("a/.git/config")],
                [Some(names(".env")), Some(names("a/.git/config"))]
            );
        }

        // TC: ART-02
        #[test]
        fn refuses_name_when_absolute() {
            assert_eq!(refusal("/etc/passwd"), Some(names("/etc/passwd")));
        }

        // TC: ART-02 (the HTML is the version's `index.html`)
        #[test]
        fn refuses_name_when_it_is_index_html() {
            assert_eq!(
                [refusal("index.html"), refusal("./index.html")],
                [Some(names("index.html")), Some(names("./index.html"))]
            );
        }

        // TC: ART-02
        #[test]
        fn refuses_name_when_it_has_a_backslash_or_control_character() {
            assert_eq!(
                [
                    refusal("a\\b.css"),
                    refusal("a\nb.css"),
                    refusal("a\u{7f}.css")
                ],
                [
                    Some(names("a\\b.css")),
                    Some(names("a\nb.css")),
                    Some(names("a\u{7f}.css"))
                ]
            );
        }

        // TC: ART-02
        #[test]
        fn refuses_name_when_empty() {
            assert_eq!(refusal(""), Some(names("")));
        }

        // TC: ART-02, BLOB-05 (a segment is a file name on disk: at most
        // 255 bytes, NAME_MAX)
        #[test]
        fn refuses_name_when_a_segment_is_longer_than_255_bytes() {
            let long = format!("a/{}", "b".repeat(256));
            assert_eq!(refusal(&long), Some(names(&long)));
        }

        // TC: ART-02, BLOB-05
        #[test]
        fn keeps_name_when_a_segment_is_255_bytes() {
            let longest = format!("a/{}", "b".repeat(255));
            assert_eq!(
                ArtifactPath::new(&longest).map(|path| path.to_string()),
                Ok(longest)
            );
        }

        // TC: ART-02, BLOB-05 (the whole normalised path is at most 1024
        // bytes)
        #[test]
        fn refuses_name_when_longer_than_1024_bytes() {
            let segment = "c".repeat(200);
            let long = [segment.as_str(); 6].join("/");
            let longest = format!("{}/{}", [segment.as_str(); 5].join("/"), "d".repeat(19));
            assert_eq!(
                (
                    refusal(&long),
                    ArtifactPath::new(&longest).map(|path| path.as_str().len())
                ),
                (Some(names(&long)), Ok(1024))
            );
        }

        // TC: ART-02 (the message names the file)
        #[test]
        fn names_the_file_when_shown() {
            let shown = ArtifactPath::new("../secret.png").map_err(|error| error.to_string());
            assert!(
                shown
                    .as_ref()
                    .is_err_and(|message| message.starts_with("files.../secret.png: ")),
                "{shown:?}"
            );
        }

        // TC: ART-02 (a relative name in a directory passes as it is;
        // `index.html` below the root is an ordinary file)
        #[test]
        fn keeps_name_when_relative_and_visible() {
            let kept: Vec<String> = ["css/site.css", "logo.png", "docs/index.html"]
                .into_iter()
                .map(|raw| ArtifactPath::new(raw).unwrap().to_string())
                .collect();
            assert_eq!(kept, ["css/site.css", "logo.png", "docs/index.html"]);
        }

        // TC: ART-02, BLOB-05 (the stored name is normalised)
        #[test]
        fn normalises_name_when_it_has_dot_or_empty_segments() {
            let kept: Vec<String> = ["a/./b.css", "a//b.css", "./a/b.css"]
                .into_iter()
                .map(|raw| ArtifactPath::new(raw).unwrap().to_string())
                .collect();
            assert_eq!(kept, ["a/b.css", "a/b.css", "a/b.css"]);
        }

        // TC: ART-04 (a stored version's HTML reads back as `index.html`)
        #[test]
        fn reads_back_index_when_stored() {
            assert_eq!(
                (
                    ArtifactPath::stored("index.html"),
                    ArtifactPath::stored("css/site.css").map(|path| path.to_string())
                ),
                (Ok(ArtifactPath::index()), Ok("css/site.css".to_owned()))
            );
        }
    }

    mod title {
        use super::*;

        // TC: ART-01 (without a title of its own a version takes its
        // HTML's, as the standalone artifacts server)
        #[test]
        fn reads_title_of_html_when_it_has_one() {
            assert_eq!(
                [
                    title_of(b"<html><TITLE lang=en>\n Q3 &amp; Q4 &#8212; report </TITLE>"),
                    title_of(b"<titles>no</titles>"),
                    title_of(b"<title>   </title>"),
                    title_of(b"<h1>none</h1>"),
                ],
                [Some("Q3 & Q4 \u{2014} report".to_owned()), None, None, None]
            );
        }

        // TC: ART-01
        #[test]
        fn cuts_title_when_longer_than_two_hundred_characters() {
            let long = "x".repeat(MAX_TITLE + 5);
            assert_eq!(
                clean_title(&long).map(|title| title.chars().count()),
                Some(MAX_TITLE)
            );
        }
    }

    mod blob_hash {
        use super::*;

        // TC: ART-13 (a blob is keyed by the SHA-256 of its bytes)
        #[test]
        fn is_sha256_hex_when_hashed() {
            assert_eq!(
                BlobHash::of(b"hello").as_str(),
                "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
            );
        }

        // TC: ART-13
        #[test]
        fn reads_back_hash_when_parsed() {
            let hash = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
            assert_eq!(
                BlobHash::parse(hash).map(|parsed| parsed.to_string()),
                Ok(hash.to_owned())
            );
        }

        // TC: ART-13
        #[test]
        fn rejects_hash_when_not_lower_case_hex_of_64() {
            let fields: Vec<Option<&'static str>> = [
                "2CF24DBA5FB0A30E26E83B2AC5B9E29E1B161E5C1FA7425E73043362938B9824",
                "2cf24dba",
                "",
            ]
            .into_iter()
            .map(|raw| BlobHash::parse(raw).err().map(|error| error.field))
            .collect();
            assert_eq!(fields, vec![Some("hash"); 3]);
        }
    }
}
