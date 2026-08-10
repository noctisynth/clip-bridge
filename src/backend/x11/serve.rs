use x11rb::protocol::xproto::{Atom, AtomEnum};

use crate::domain::TextPayload;

use super::atoms::Atoms;

const REQUEST_OVERHEAD_BYTES: usize = 256;
const MIN_CHUNK_BYTES: usize = 1024;

pub(super) struct EncodedSelection {
    pub property_type: Atom,
    pub bytes: Vec<u8>,
}

pub(super) fn encode_target(
    target: Atom,
    payload: &TextPayload,
    atoms: &Atoms,
) -> Option<EncodedSelection> {
    if target == atoms.utf8_string
        || target == atoms.text_plain_utf8
        || target == atoms.text_plain
        || target == atoms.text
    {
        return Some(EncodedSelection {
            property_type: atoms.utf8_string,
            bytes: payload.as_str().as_bytes().to_vec(),
        });
    }

    if target == atoms.string {
        return encode_latin1(payload).map(|bytes| EncodedSelection {
            property_type: AtomEnum::STRING.into(),
            bytes,
        });
    }

    None
}

fn encode_latin1(payload: &TextPayload) -> Option<Vec<u8>> {
    payload
        .as_str()
        .chars()
        .map(|character| u8::try_from(u32::from(character)).ok())
        .collect()
}

pub(super) fn request_property(property: Atom, target: Atom) -> Atom {
    if property == AtomEnum::NONE.into() {
        target
    } else {
        property
    }
}

pub(super) fn chunk_size(maximum_request_bytes: usize) -> usize {
    maximum_request_bytes
        .saturating_sub(REQUEST_OVERHEAD_BYTES)
        .max(MIN_CHUNK_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_property_falls_back_to_target() {
        assert_eq!(request_property(AtomEnum::NONE.into(), 42), 42);
        assert_eq!(request_property(7, 42), 7);
    }

    #[test]
    fn latin1_encoding_is_lossless_or_rejected() {
        let latin1 = TextPayload::from_string("A£ÿ".to_owned()).expect("test text is valid");
        assert_eq!(encode_latin1(&latin1), Some(vec![0x41, 0xa3, 0xff]));

        let unicode = TextPayload::from_string("snowman ☃".to_owned()).expect("test text is valid");
        assert_eq!(encode_latin1(&unicode), None);
    }

    #[test]
    fn chunk_size_is_derived_from_server_request_limit() {
        assert_eq!(chunk_size(65_536), 65_280);
        assert_eq!(chunk_size(100), MIN_CHUNK_BYTES);
    }
}
