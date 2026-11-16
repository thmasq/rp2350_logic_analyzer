use embassy_usb::{
    Handler,
    control::{InResponse, OutResponse, Recipient, Request, RequestType},
    types::{InterfaceNumber, StringIndex},
};

pub const RPI_RESET_INTERFACE_CLASS: u8 = 0xFF;
pub const RPI_RESET_INTERFACE_SUBCLASS: u8 = 0x00;
pub const RPI_RESET_INTERFACE_PROTOCOL: u8 = 0x01;
pub const RESET_REQUEST_BOOTSEL: u8 = 0x01;

pub struct ResetHandler {
    itf_num: InterfaceNumber,
    reset_str: StringIndex,
}

impl ResetHandler {
    pub fn new(itf_num: InterfaceNumber, reset_str: StringIndex) -> Self {
        Self { itf_num, reset_str }
    }
}

impl Handler for ResetHandler {
    fn control_out(&mut self, req: Request, _buf: &[u8]) -> Option<OutResponse> {
        if req.request_type == RequestType::Vendor
            && req.recipient == Recipient::Interface
            && req.index == self.itf_num.0 as u16
            && req.request == RESET_REQUEST_BOOTSEL
        {
            embassy_rp::rom_data::reset_to_usb_boot(0, 0);
            return Some(OutResponse::Accepted);
        }
        None
    }

    fn control_in<'a>(&'a mut self, _req: Request, _buf: &'a mut [u8]) -> Option<InResponse<'a>> {
        None
    }

    fn get_string(&mut self, index: StringIndex, _lang_id: u16) -> Option<&str> {
        if index == self.reset_str {
            Some("Reset")
        } else {
            None
        }
    }
}
