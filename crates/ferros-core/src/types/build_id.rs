//! Build identity of a loaded image, independent of the object-file format.
//!
//! Object formats record a build's identity in different ways: an ELF GNU build-id note is
//! usually 20 bytes, a Mach-O UUID 16, a PE/COFF CodeView signature 16 plus an age. The
//! engine only needs to compare them, so [`BuildId`] holds the bytes the backend reports
//! and [`BuildIdentity`] says whether the backend has them.

use std::fmt;

/// The bytes that identify one build of an image, as the platform backend reports them.
///
/// Holds 1 to [`BuildId::MAX_LEN`] bytes inline, enough for a 20-byte GNU build-id or a
/// 16-byte UUID. Two ids are equal only when their lengths and bytes are equal.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct BuildId
{
    bytes: [u8; Self::MAX_LEN],
    len: u8,
}

impl BuildId
{
    /// The longest id a [`BuildId`] holds.
    pub const MAX_LEN: usize = 32;

    /// The id with these bytes, or `None` when `bytes` is empty or longer than
    /// [`Self::MAX_LEN`].
    #[must_use]
    pub fn new(bytes: &[u8]) -> Option<Self>
    {
        if bytes.is_empty() || bytes.len() > Self::MAX_LEN {
            return None;
        }
        let mut stored = [0; Self::MAX_LEN];
        stored[..bytes.len()].copy_from_slice(bytes);
        Some(Self {
            bytes: stored,
            len: u8::try_from(bytes.len()).ok()?,
        })
    }

    /// The id's bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8]
    {
        &self.bytes[..usize::from(self.len)]
    }
}

impl From<[u8; 16]> for BuildId
{
    fn from(bytes: [u8; 16]) -> Self
    {
        Self::new(&bytes).expect("16 bytes fit a build id")
    }
}

impl fmt::Display for BuildId
{
    /// Lowercase hex of the bytes, with no separators.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result
    {
        self.as_bytes().iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl fmt::Debug for BuildId
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result
    {
        write!(f, "BuildId({self})")
    }
}

/// What a backend knows about one image's build identity at one stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuildIdentity
{
    /// The image's build id.
    Known(BuildId),
    /// The backend could not read the id this time. The image may have one, and a later
    /// report may know it: on macOS a UUID read can fail at one stop and succeed at the
    /// next (#170). Not evidence of a different build.
    Unknown,
    /// The image format records no id for this image. A missing id is then not evidence of
    /// the same build either, so two such reports never compare as the same build.
    Absent,
}

impl BuildIdentity
{
    /// The id, when known.
    #[must_use]
    pub const fn known(&self) -> Option<BuildId>
    {
        match self {
            Self::Known(id) => Some(*id),
            Self::Unknown | Self::Absent => None,
        }
    }

    /// This identity, completed by a later report about the same image: a known id wins
    /// over an unknown one, and otherwise `self` is kept.
    #[must_use]
    pub const fn or_later(self, later: Self) -> Self
    {
        match (self, later) {
            (Self::Unknown, Self::Known(_)) => later,
            _ => self,
        }
    }
}

impl From<Option<BuildId>> for BuildIdentity
{
    /// `Some` is [`Known`](Self::Known); `None` is [`Unknown`](Self::Unknown).
    fn from(id: Option<BuildId>) -> Self
    {
        id.map_or(Self::Unknown, Self::Known)
    }
}

/// Whether two reports for an image at the same path and load address can be the same
/// build.
///
/// - Two known ids: the same build only when the bytes are equal.
/// - [`Unknown`](BuildIdentity::Unknown) with `Unknown` or a known id: yes, since a failed
///   read is no evidence of a different build.
/// - Either side [`Absent`](BuildIdentity::Absent): no, since there is nothing to tell two
///   builds apart; the caller reads the image again.
#[must_use]
pub fn same_build(a: BuildIdentity, b: BuildIdentity) -> bool
{
    match (a, b) {
        (BuildIdentity::Known(a), BuildIdentity::Known(b)) => a == b,
        (BuildIdentity::Absent, _) | (_, BuildIdentity::Absent) => false,
        (BuildIdentity::Unknown, _) | (_, BuildIdentity::Unknown) => true,
    }
}

#[cfg(test)]
mod tests
{
    use super::*;

    fn known(bytes: &[u8]) -> BuildIdentity
    {
        BuildIdentity::Known(BuildId::new(bytes).expect("valid id"))
    }

    #[test]
    fn build_id_holds_one_to_thirty_two_bytes()
    {
        assert_eq!(BuildId::new(&[]), None);
        assert_eq!(BuildId::new(&[0; 33]), None);
        assert_eq!(BuildId::new(&[7; 32]).expect("32 bytes").as_bytes(), &[7; 32]);
        let gnu = [0xAB; 20];
        assert_eq!(BuildId::new(&gnu).expect("20 bytes").as_bytes(), &gnu);
        assert_eq!(BuildId::from([1; 16]).as_bytes(), &[1; 16]);
    }

    #[test]
    fn build_id_compares_length_and_bytes_and_prints_hex()
    {
        let short = BuildId::new(&[0xAB, 0x01]).expect("2 bytes");
        let padded = BuildId::new(&[0xAB, 0x01, 0x00]).expect("3 bytes");
        assert_ne!(short, padded, "trailing zero bytes are part of the id");
        assert_eq!(short.to_string(), "ab01");
        assert_eq!(format!("{short:?}"), "BuildId(ab01)");
    }

    #[test]
    fn same_build_trusts_known_ids_tolerates_unknown_and_never_matches_absent()
    {
        let gnu = [0x5A; 20];
        let mut changed = gnu;
        changed[19] ^= 1;
        assert!(same_build(known(&gnu), known(&gnu)));
        assert!(
            !same_build(known(&gnu), known(&changed)),
            "the last byte of a 20-byte id counts"
        );
        assert!(!same_build(known(&[1; 16]), known(&[1; 20])));

        assert!(same_build(BuildIdentity::Unknown, BuildIdentity::Unknown));
        assert!(same_build(BuildIdentity::Unknown, known(&gnu)));
        assert!(same_build(known(&gnu), BuildIdentity::Unknown));

        assert!(!same_build(BuildIdentity::Absent, BuildIdentity::Absent));
        assert!(!same_build(BuildIdentity::Absent, BuildIdentity::Unknown));
        assert!(!same_build(BuildIdentity::Unknown, BuildIdentity::Absent));
        assert!(!same_build(BuildIdentity::Absent, known(&gnu)));
        assert!(!same_build(known(&gnu), BuildIdentity::Absent));
    }

    #[test]
    fn or_later_lets_a_known_id_replace_only_an_unknown_one()
    {
        let first = known(&[1; 16]);
        let second = known(&[2; 16]);
        assert_eq!(BuildIdentity::Unknown.or_later(first), first);
        assert_eq!(first.or_later(BuildIdentity::Unknown), first);
        assert_eq!(first.or_later(second), first);
        assert_eq!(BuildIdentity::Absent.or_later(first), BuildIdentity::Absent);
        assert_eq!(BuildIdentity::Unknown.or_later(BuildIdentity::Absent), BuildIdentity::Unknown);
        assert_eq!(BuildIdentity::from(None), BuildIdentity::Unknown);
        assert_eq!(BuildIdentity::from(BuildId::new(&[3])), known(&[3]));
    }
}
