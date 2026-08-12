//! Focused Connect relay lifetime and bounded-state proofs.

use std::net::{IpAddr, Ipv4Addr};

use devmanager::connect::{
    AccountId, DevicePublicId, HostPublicId as RelayHostPublicId, OpaqueRelay, RateKey, RelayError,
    RouteId, RouteTicket, SignedRouteTicket, TicketAudience, TicketId, TicketSigningKey,
    MAX_RELAY_CONSUMED_NONCES, PRESENCE_TTL_SECS,
};
use devmanager::protocol::SealedFrame;
use uuid::Uuid;

fn uuid(tail: u8) -> Uuid {
    let mut bytes = [0_u8; 16];
    bytes[0] = 0x01;
    bytes[1] = 0x23;
    bytes[2] = 0x45;
    bytes[3] = 0x67;
    bytes[4] = 0x89;
    bytes[5] = 0xab;
    bytes[6] = 0x70;
    bytes[7] = 0xcd;
    bytes[8] = 0x80;
    bytes[9] = 0xef;
    bytes[15] = tail;
    Uuid::from_bytes(bytes)
}

fn sealed(seq: u64) -> SealedFrame {
    SealedFrame::from_parts(1, seq, [9; 16], vec![1, 2, 3, 4], [7; 32]).expect("frame")
}

fn ticket(
    key: &TicketSigningKey,
    route_tail: u8,
    ticket_tail: u8,
    audience: TicketAudience,
    issued: u64,
    expires: u64,
    nonce_tail: u8,
) -> SignedRouteTicket {
    let mut nonce = [0_u8; 16];
    nonce[15] = nonce_tail;
    let claims = RouteTicket::new(
        TicketId::from_uuid(uuid(ticket_tail)).unwrap(),
        RouteId::from_uuid(uuid(route_tail)).unwrap(),
        RelayHostPublicId::from_uuid(uuid(0x11)).unwrap(),
        DevicePublicId::from_uuid(uuid(0x21)).unwrap(),
        AccountId::from_uuid(uuid(0x31)).unwrap(),
        audience,
        issued,
        expires,
        nonce,
    )
    .unwrap();
    SignedRouteTicket::issue(key, claims)
}

#[test]
fn stale_bound_tickets_cannot_forward() {
    let key = TicketSigningKey::from_bytes([3; 32]);
    let mut relay = OpaqueRelay::with_signing_key(key.clone()).with_queue_bounds(2, 1_024);
    let host = ticket(&key, 0x41, 0x51, TicketAudience::HostSocket, 100, 200, 1);
    let device = ticket(&key, 0x41, 0x52, TicketAudience::DeviceSocket, 100, 200, 2);
    let source = RateKey::Ip(IpAddr::V4(Ipv4Addr::LOCALHOST));
    relay.bind(&host, 100, source).unwrap();
    relay
        .bind(
            &device,
            100,
            RateKey::Ip(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2))),
        )
        .unwrap();
    let route = host.claims().route_id();
    assert!(relay
        .admit(route, TicketAudience::HostSocket, sealed(1), 150)
        .is_ok());
    assert_eq!(
        relay.admit(route, TicketAudience::HostSocket, sealed(2), 200),
        Err(RelayError::ExpiredTicket)
    );
}

#[test]
fn presence_expiry_removes_route_channels() {
    let key = TicketSigningKey::from_bytes([4; 32]);
    let mut relay = OpaqueRelay::with_signing_key(key.clone());
    let host = ticket(
        &key,
        0x42,
        0x53,
        TicketAudience::HostSocket,
        10,
        10 + PRESENCE_TTL_SECS + 60,
        3,
    );
    let route = host.claims().route_id();
    relay
        .bind(&host, 10, RateKey::Ip(IpAddr::V4(Ipv4Addr::LOCALHOST)))
        .unwrap();
    assert!(relay.presence_live(route, 10));
    assert_eq!(
        relay.admit(
            route,
            TicketAudience::HostSocket,
            sealed(1),
            10 + PRESENCE_TTL_SECS,
        ),
        Err(RelayError::UnknownRoute)
    );
    assert!(!relay.presence_live(route, 10 + PRESENCE_TTL_SECS));
}

#[test]
fn consumed_nonce_state_cannot_grow_without_bound() {
    use devmanager::connect::{
        BIND_RATE_WINDOW_SECS, MAX_BIND_ATTEMPTS_PER_WINDOW, MAX_ROUTE_TICKET_TTL_SECS,
    };

    let key = TicketSigningKey::from_bytes([5; 32]);
    let mut relay = OpaqueRelay::with_signing_key(key.clone());
    let account = RateKey::Account(AccountId::from_uuid(uuid(0x31)).unwrap());
    for i in 0..MAX_RELAY_CONSUMED_NONCES {
        let now = 1 + (i as u64 / MAX_BIND_ATTEMPTS_PER_WINDOW as u64) * BIND_RATE_WINDOW_SECS;
        let mut nonce = [0_u8; 16];
        nonce[..8].copy_from_slice(&(i as u64).to_be_bytes());
        let claims = RouteTicket::new(
            TicketId::from_uuid(uuid(((i % 200) + 1) as u8)).unwrap(),
            RouteId::from_uuid(uuid(0x44)).unwrap(),
            RelayHostPublicId::from_uuid(uuid(0x11)).unwrap(),
            DevicePublicId::from_uuid(uuid(0x21)).unwrap(),
            AccountId::from_uuid(uuid(0x31)).unwrap(),
            TicketAudience::HostSocket,
            now,
            now + MAX_ROUTE_TICKET_TTL_SECS,
            nonce,
        )
        .unwrap();
        let signed = SignedRouteTicket::issue(&key, claims);
        relay.bind(&signed, now, account).unwrap();
    }
    let now = 1
        + (MAX_RELAY_CONSUMED_NONCES as u64 / MAX_BIND_ATTEMPTS_PER_WINDOW as u64)
            * BIND_RATE_WINDOW_SECS;
    let mut nonce = [0xff; 16];
    nonce[0] = 0xab;
    let overflow = SignedRouteTicket::issue(
        &key,
        RouteTicket::new(
            TicketId::from_uuid(uuid(0xfe)).unwrap(),
            RouteId::from_uuid(uuid(0x45)).unwrap(),
            RelayHostPublicId::from_uuid(uuid(0x11)).unwrap(),
            DevicePublicId::from_uuid(uuid(0x22)).unwrap(),
            AccountId::from_uuid(uuid(0x31)).unwrap(),
            TicketAudience::HostSocket,
            now,
            now + MAX_ROUTE_TICKET_TTL_SECS,
            nonce,
        )
        .unwrap(),
    );
    assert_eq!(
        relay.bind(&overflow, now, account),
        Err(RelayError::StateBoundExceeded)
    );
}
