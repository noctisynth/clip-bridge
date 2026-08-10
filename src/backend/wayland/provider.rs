use wayland_client::{Proxy, QueueHandle, backend::ObjectId, protocol::wl_seat::WlSeat};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::ExtDataControlDeviceV1,
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::ExtDataControlOfferV1,
    ext_data_control_source_v1::ExtDataControlSourceV1,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
    zwlr_data_control_source_v1::ZwlrDataControlSourceV1,
};

use crate::domain::{ProtocolError, SelectionKind};

use super::{SourceData, WaylandState};

#[derive(Clone)]
pub(super) enum ProviderManager {
    Ext(ExtDataControlManagerV1),
    Wlr {
        manager: ZwlrDataControlManagerV1,
        version: u32,
    },
}

impl ProviderManager {
    pub fn supports_primary(&self) -> bool {
        match self {
            Self::Ext(_) => true,
            Self::Wlr { version, .. } => *version >= 2,
        }
    }

    pub fn get_device(&self, seat: &WlSeat, queue: &QueueHandle<WaylandState>) -> ProviderDevice {
        match self {
            Self::Ext(manager) => ProviderDevice::Ext(manager.get_data_device(seat, queue, ())),
            Self::Wlr { manager, .. } => {
                ProviderDevice::Wlr(manager.get_data_device(seat, queue, ()))
            }
        }
    }

    pub fn create_source(
        &self,
        queue: &QueueHandle<WaylandState>,
        data: SourceData,
    ) -> ProviderSource {
        match self {
            Self::Ext(manager) => ProviderSource::Ext(manager.create_data_source(queue, data)),
            Self::Wlr { manager, .. } => {
                ProviderSource::Wlr(manager.create_data_source(queue, data))
            }
        }
    }
}

#[derive(Clone)]
pub(super) enum ProviderDevice {
    Ext(ExtDataControlDeviceV1),
    Wlr(ZwlrDataControlDeviceV1),
}

impl ProviderDevice {
    pub fn set_selection(
        &self,
        selection: SelectionKind,
        source: &ProviderSource,
    ) -> Result<(), ProtocolError> {
        match (self, source, selection) {
            (Self::Ext(device), ProviderSource::Ext(source), SelectionKind::Clipboard) => {
                device.set_selection(Some(source));
                Ok(())
            }
            (Self::Ext(device), ProviderSource::Ext(source), SelectionKind::Primary) => {
                device.set_primary_selection(Some(source));
                Ok(())
            }
            (Self::Wlr(device), ProviderSource::Wlr(source), SelectionKind::Clipboard) => {
                device.set_selection(Some(source));
                Ok(())
            }
            (Self::Wlr(device), ProviderSource::Wlr(source), SelectionKind::Primary) => {
                device.set_primary_selection(Some(source));
                Ok(())
            }
            _ => Err(ProtocolError::invalid_state(
                "wayland-set-selection",
                "device and source data-control provider variants differ",
            )),
        }
    }
}

#[derive(Clone)]
pub(super) enum ProviderOffer {
    Ext(ExtDataControlOfferV1),
    Wlr(ZwlrDataControlOfferV1),
}

impl ProviderOffer {
    pub fn id(&self) -> ObjectId {
        match self {
            Self::Ext(offer) => offer.id(),
            Self::Wlr(offer) => offer.id(),
        }
    }

    pub fn receive(&self, mime: String, fd: std::os::fd::BorrowedFd<'_>) {
        match self {
            Self::Ext(offer) => offer.receive(mime, fd),
            Self::Wlr(offer) => offer.receive(mime, fd),
        }
    }

    pub fn destroy(self) {
        match self {
            Self::Ext(offer) => offer.destroy(),
            Self::Wlr(offer) => offer.destroy(),
        }
    }
}

#[derive(Clone)]
pub(super) enum ProviderSource {
    Ext(ExtDataControlSourceV1),
    Wlr(ZwlrDataControlSourceV1),
}

impl ProviderSource {
    pub fn id(&self) -> ObjectId {
        match self {
            Self::Ext(source) => source.id(),
            Self::Wlr(source) => source.id(),
        }
    }

    pub fn offer_text(&self) {
        for mime in ["text/plain;charset=utf-8", "text/plain"] {
            match self {
                Self::Ext(source) => source.offer(mime.to_owned()),
                Self::Wlr(source) => source.offer(mime.to_owned()),
            }
        }
    }

    pub fn destroy(self) {
        match self {
            Self::Ext(source) => source.destroy(),
            Self::Wlr(source) => source.destroy(),
        }
    }

    pub fn ensure_matches(&self, manager: &ProviderManager) -> Result<(), ProtocolError> {
        if matches!(
            (self, manager),
            (Self::Ext(_), ProviderManager::Ext(_)) | (Self::Wlr(_), ProviderManager::Wlr { .. })
        ) {
            Ok(())
        } else {
            Err(ProtocolError::invalid_state(
                "wayland-set-selection",
                "source and data-control provider variants differ",
            ))
        }
    }
}
