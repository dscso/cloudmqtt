use winnow::Bytes;

use crate::v5::{
    fixed_header::PacketType,
    properties::define_properties,
    variable_header::{PacketIdentifier, ReasonString, UserProperties},
    MResult,
};

crate::v5::reason_code::make_combined_reason_code! {
    pub enum PubrelReasonCode {
        Success = crate::v5::reason_code::Success,
        PacketIdentifierNotFound = crate::v5::reason_code::PacketIdentifierNotFound,
    }
}

define_properties!(
    packet_type: MPubrel,
    anker: "_Toc3901145",
    pub struct PubrelProperties<'i> {
        (anker: "_Toc3901147")
        reason_string: ReasonString<'i>,

        (anker: "_Toc3901148")
        user_properties: UserProperties<'i>,
    }
);

pub struct MPubrel<'i> {
    pub packet_identifier: PacketIdentifier,
    pub reason: PubrelReasonCode,
    pub properties: PubrelProperties<'i>,
}

impl<'i> MPubrel<'i> {
    pub const PACKET_TYPE: PacketType = PacketType::Pubrel;

    pub fn parse(input: &mut &'i Bytes) -> MResult<Self> {
        let packet_identifier = PacketIdentifier::parse(input)?;

        if input.is_empty() {
            Ok(Self {
                packet_identifier,
                reason: PubrelReasonCode::Success,
                properties: PubrelProperties::new(),
            })
        } else {
            let reason = PubrelReasonCode::parse(input)?;
            let properties = PubrelProperties::parse(input)?;
            Ok(Self {
                packet_identifier,
                reason,
                properties,
            })
        }
    }
}
