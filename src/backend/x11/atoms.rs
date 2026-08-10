use x11rb::{
    connection::Connection,
    protocol::xproto::{Atom, AtomEnum, ConnectionExt},
};

use crate::domain::{ProtocolError, SelectionKind};

pub(super) struct Atoms {
    pub clipboard: Atom,
    pub targets: Atom,
    pub multiple: Atom,
    pub incr: Atom,
    pub timestamp: Atom,
    pub utf8_string: Atom,
    pub text: Atom,
    pub string: Atom,
    pub text_plain_utf8: Atom,
    pub text_plain: Atom,
    clipboard_property: Atom,
    primary_property: Atom,
}

impl Atoms {
    pub fn intern<C: Connection>(connection: &C) -> Result<Self, ProtocolError> {
        Ok(Self {
            clipboard: intern(connection, "CLIPBOARD")?,
            targets: intern(connection, "TARGETS")?,
            multiple: intern(connection, "MULTIPLE")?,
            incr: intern(connection, "INCR")?,
            timestamp: intern(connection, "TIMESTAMP")?,
            utf8_string: intern(connection, "UTF8_STRING")?,
            text: intern(connection, "TEXT")?,
            string: intern(connection, "STRING")?,
            text_plain_utf8: intern(connection, "text/plain;charset=utf-8")?,
            text_plain: intern(connection, "text/plain")?,
            clipboard_property: intern(connection, "CLIP_BRIDGE_CLIPBOARD")?,
            primary_property: intern(connection, "CLIP_BRIDGE_PRIMARY")?,
        })
    }

    pub fn selection(&self, selection: SelectionKind) -> Atom {
        match selection {
            SelectionKind::Clipboard => self.clipboard,
            SelectionKind::Primary => AtomEnum::PRIMARY.into(),
        }
    }

    pub const fn transfer_property(&self, selection: SelectionKind) -> Atom {
        match selection {
            SelectionKind::Clipboard => self.clipboard_property,
            SelectionKind::Primary => self.primary_property,
        }
    }

    pub fn selection_kind(&self, atom: Atom) -> Option<SelectionKind> {
        if atom == self.clipboard {
            Some(SelectionKind::Clipboard)
        } else if atom == AtomEnum::PRIMARY.into() {
            Some(SelectionKind::Primary)
        } else {
            None
        }
    }
}

#[cfg(test)]
impl Atoms {
    pub fn for_test() -> Self {
        Self {
            clipboard: 1,
            targets: 2,
            multiple: 3,
            incr: 4,
            timestamp: 5,
            utf8_string: 6,
            text: 7,
            string: 8,
            text_plain_utf8: 9,
            text_plain: 10,
            clipboard_property: 11,
            primary_property: 12,
        }
    }
}

fn intern<C: Connection>(connection: &C, name: &'static str) -> Result<Atom, ProtocolError> {
    connection
        .intern_atom(false, name.as_bytes())
        .map_err(|error| ProtocolError::operation("x11-intern-atom", error))?
        .reply()
        .map(|reply| reply.atom)
        .map_err(|error| ProtocolError::operation("x11-intern-atom-reply", error))
}
