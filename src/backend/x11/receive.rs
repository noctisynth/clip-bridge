use x11rb::protocol::xproto::Atom;

use crate::domain::{MAX_TEXT_BYTES, TextPayload, TransferError};

use super::atoms::Atoms;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TextTarget {
    Utf8String,
    TextPlainUtf8,
    TextPlain,
    Text,
    String,
}

impl TextTarget {
    pub const fn atom(self, atoms: &Atoms) -> Atom {
        match self {
            Self::Utf8String => atoms.utf8_string,
            Self::TextPlainUtf8 => atoms.text_plain_utf8,
            Self::TextPlain => atoms.text_plain,
            Self::Text => atoms.text,
            Self::String => atoms.string,
        }
    }
}

pub(super) fn choose_target(offered: &[Atom], atoms: &Atoms) -> Option<TextTarget> {
    [
        TextTarget::Utf8String,
        TextTarget::TextPlainUtf8,
        TextTarget::TextPlain,
        TextTarget::Text,
        TextTarget::String,
    ]
    .into_iter()
    .find(|target| offered.contains(&target.atom(atoms)))
}

pub(super) fn decode(
    target: TextTarget,
    property_type: Atom,
    bytes: Vec<u8>,
    atoms: &Atoms,
) -> Result<TextPayload, TransferError> {
    match target {
        TextTarget::Utf8String | TextTarget::TextPlainUtf8 | TextTarget::TextPlain => {
            if !matches!(
                property_type,
                atom if atom == atoms.utf8_string
                    || atom == atoms.text_plain_utf8
                    || atom == atoms.text_plain
            ) {
                return Err(TransferError::Unsupported);
            }
            TextPayload::from_bytes(bytes).map_err(payload_error)
        }
        TextTarget::Text => {
            if property_type == atoms.string {
                latin1_payload(bytes)
            } else if property_type == atoms.utf8_string
                || property_type == atoms.text_plain_utf8
                || property_type == atoms.text_plain
            {
                TextPayload::from_bytes(bytes).map_err(payload_error)
            } else {
                Err(TransferError::Unsupported)
            }
        }
        TextTarget::String => {
            if property_type != atoms.string {
                return Err(TransferError::Unsupported);
            }
            latin1_payload(bytes)
        }
    }
}

fn latin1_payload(bytes: Vec<u8>) -> Result<TextPayload, TransferError> {
    let text: String = bytes.into_iter().map(char::from).collect();
    TextPayload::from_string(text).map_err(payload_error)
}

fn payload_error(error: crate::domain::TextPayloadError) -> TransferError {
    match error {
        crate::domain::TextPayloadError::Empty => TransferError::Empty,
        crate::domain::TextPayloadError::InvalidUtf8 => TransferError::InvalidUtf8,
        crate::domain::TextPayloadError::TooLarge { size, max } => {
            TransferError::TooLarge { size, max }
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct ChunkAssembler {
    bytes: Vec<u8>,
}

impl ChunkAssembler {
    pub fn push(&mut self, chunk: &[u8]) -> Result<(), TransferError> {
        let size = self.bytes.len().saturating_add(chunk.len());
        if size > MAX_TEXT_BYTES {
            return Err(TransferError::TooLarge {
                size,
                max: MAX_TEXT_BYTES,
            });
        }
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    pub fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_selection_uses_the_declared_priority() {
        let atoms = Atoms::for_test();
        let offered = [atoms.string, atoms.text_plain, atoms.utf8_string];
        assert_eq!(
            choose_target(&offered, &atoms),
            Some(TextTarget::Utf8String)
        );

        let offered = [atoms.string, atoms.text];
        assert_eq!(choose_target(&offered, &atoms), Some(TextTarget::Text));
        assert_eq!(choose_target(&[], &atoms), None);
    }

    #[test]
    fn assembler_rejects_content_above_limit() {
        let mut assembler = ChunkAssembler::default();
        assembler
            .push(&vec![0; MAX_TEXT_BYTES])
            .expect("the exact payload limit is accepted");
        assert!(matches!(
            assembler.push(&[0]),
            Err(TransferError::TooLarge {
                size,
                max: MAX_TEXT_BYTES,
            }) if size == MAX_TEXT_BYTES + 1
        ));
    }

    #[test]
    fn latin1_maps_every_byte_to_the_same_unicode_codepoint() {
        let payload = latin1_payload(vec![0x41, 0xa3, 0xff])
            .expect("non-empty Latin-1 is always representable as UTF-8");
        assert_eq!(payload.as_str(), "A£ÿ");
    }
}
