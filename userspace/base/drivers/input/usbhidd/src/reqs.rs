use rehid::report_desc::ReportTy;
use xhcid_interface::{
    DeviceReqData, PortReqRecipient, PortReqTy, XhciClientHandle, XhciClientHandleError,
};

const GET_REPORT_REQ: u8 = 0x1;
const SET_IDLE_REQ: u8 = 0xA;

fn concat(hi: u8, lo: u8) -> u16 {
    (u16::from(hi) << 8) | u16::from(lo)
}

pub fn get_report(
    handle: &XhciClientHandle,
    report_ty: ReportTy,
    report_id: u8,
    if_num: u16,
    buffer: &mut [u8],
) -> Result<(), XhciClientHandleError> {
    handle.device_request(
        PortReqTy::Class,
        PortReqRecipient::Interface,
        GET_REPORT_REQ,
        concat(report_ty as u8, report_id),
        if_num,
        DeviceReqData::In(buffer),
    )
}
pub fn set_idle(
    handle: &XhciClientHandle,
    duration: u8,
    report_id: u8,
    if_num: u16,
) -> Result<(), XhciClientHandleError> {
    handle.device_request(
        PortReqTy::Class,
        PortReqRecipient::Interface,
        SET_IDLE_REQ,
        concat(duration, report_id),
        if_num,
        DeviceReqData::NoData,
    )
}
