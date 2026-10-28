//! Номер причины отключения Wi-Fi по `DisconnectReason` из esp-radio.
//!
//! Неудача `connect_async` отдаёт только enum: номера из него апстрим убрал
//! (esp-hal#4923). Сырой номер есть в событии `StationDisconnected`, но
//! подписка держит `&WifiController`, а `connect_async` требует `&mut` —
//! на время подключения подписаться нельзя. Поэтому номер восстанавливаем
//! обратной таблицей к `DisconnectReason::from_raw` (`esp-radio/src/wifi/mod.rs`).
//! Номера, которых в enum нет, драйвер уже свернул в `Unspecified` ⇒ здесь они 1.

use esp_radio::wifi::DisconnectReason;
use meteo_core::wifi_retry::ReasonCode;

/// Номер IEEE 802.11 (1–68) или ESP (200+); `None` — вариант, появившийся в
/// esp-radio позже этой таблицы.
#[must_use]
pub const fn reason_code(reason: DisconnectReason) -> Option<ReasonCode> {
    let code = match reason {
        DisconnectReason::Unspecified => 1,
        DisconnectReason::AuthenticationExpired => 2,
        DisconnectReason::AuthenticationLeave => 3,
        DisconnectReason::DisassociatedDueToInactivity => 4,
        DisconnectReason::AssociationTooMany => 5,
        DisconnectReason::Class2FrameFromNonAuthenticatedStation => 6,
        DisconnectReason::Class3FrameFromNonAssociatedStation => 7,
        DisconnectReason::AssociationLeave => 8,
        DisconnectReason::AssociationNotAuthenticated => 9,
        DisconnectReason::DisassociatedPowerCapabilityBad => 10,
        DisconnectReason::DisassociatedUnsupportedChannel => 11,
        DisconnectReason::BssTransitionDisassociated => 12,
        DisconnectReason::IeInvalid => 13,
        DisconnectReason::MicFailure => 14,
        DisconnectReason::FourWayHandshakeTimeout => 15,
        DisconnectReason::GroupKeyUpdateTimeout => 16,
        DisconnectReason::IeIn4wayDiffers => 17,
        DisconnectReason::GroupCipherInvalid => 18,
        DisconnectReason::PairwiseCipherInvalid => 19,
        DisconnectReason::AkmpInvalid => 20,
        DisconnectReason::UnsupportedRsnIeVersion => 21,
        DisconnectReason::InvalidRsnIeCapabilities => 22,
        DisconnectReason::_802_1xAuthenticationFailed => 23,
        DisconnectReason::CipherSuiteRejected => 24,
        DisconnectReason::TdlsPeerUnreachable => 25,
        DisconnectReason::TdlsUnspecified => 26,
        DisconnectReason::SspRequestedDisassociation => 27,
        DisconnectReason::NoSspRoamingAgreement => 28,
        DisconnectReason::BadCipherOrAkm => 29,
        DisconnectReason::NotAuthorizedThisLocation => 30,
        DisconnectReason::ServiceChangePercludesTs => 31,
        DisconnectReason::UnspecifiedQos => 32,
        DisconnectReason::NotEnoughBandwidth => 33,
        DisconnectReason::MissingAcks => 34,
        DisconnectReason::ExceededTxOp => 35,
        DisconnectReason::StationLeaving => 36,
        DisconnectReason::EndBlockAck => 37,
        DisconnectReason::UnknownBlockAck => 38,
        DisconnectReason::Timeout => 39,
        DisconnectReason::PeerInitiated => 46,
        DisconnectReason::AccessPointInitiatedDisassociation => 47,
        DisconnectReason::InvalidFtActionFrameCount => 48,
        DisconnectReason::InvalidPmkid => 49,
        DisconnectReason::InvalidMde => 50,
        DisconnectReason::InvalidFte => 51,
        DisconnectReason::TransmissionLinkEstablishmentFailed => 67,
        DisconnectReason::AlternativeChannelOccupied => 68,
        DisconnectReason::BeaconTimeout => 200,
        DisconnectReason::NoAccessPointFound => 201,
        DisconnectReason::AuthenticationFailed => 202,
        DisconnectReason::AssociationFailed => 203,
        DisconnectReason::HandshakeTimeout => 204,
        DisconnectReason::ConnectionFailed => 205,
        DisconnectReason::AccessPointTsfReset => 206,
        DisconnectReason::Roaming => 207,
        DisconnectReason::AssociationComebackTimeTooLong => 208,
        DisconnectReason::SaQueryTimeout => 209,
        DisconnectReason::NoAccessPointFoundWithCompatibleSecurity => 210,
        DisconnectReason::NoAccessPointFoundInAuthmodeThreshold => 211,
        DisconnectReason::NoAccessPointFoundInRssiThreshold => 212,
        _ => return None,
    };
    Some(ReasonCode(code))
}
