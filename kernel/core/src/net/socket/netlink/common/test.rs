// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: ktests for documented no-op SO_ATTACH_FILTER / SO_DETACH_FILTER.

use ostd::prelude::*;

use crate::{
    net::socket::{
        Socket,
        netlink::{NetlinkRouteSocket, NetlinkUeventSocket},
        options::{AttachFilter, BPF_MAXINSNS, DetachFilter, validate_classic_bpf_len},
    },
    prelude::*,
    util::net::SockType,
};

fn detach_filter() -> DetachFilter {
    let mut option = DetachFilter::new();
    option.set(());
    option
}

#[ktest]
fn classic_bpf_len_validation() {
    assert!(validate_classic_bpf_len(0).is_err_and(|err| err.error() == Errno::EINVAL));
    assert!(
        validate_classic_bpf_len(BPF_MAXINSNS.checked_add(1).unwrap())
            .is_err_and(|err| err.error() == Errno::EINVAL)
    );
    assert!(validate_classic_bpf_len(1).is_ok());
    assert!(validate_classic_bpf_len(BPF_MAXINSNS).is_ok());
}

#[ktest]
fn attach_filter_succeeds_and_is_idempotent() {
    let socket = NetlinkUeventSocket::new(true, SockType::SOCK_DGRAM);

    let mut attach = AttachFilter::new();
    attach.set(1);
    socket.set_option(&attach).unwrap();
    socket.set_option(&attach).unwrap();
}

#[ktest]
fn detach_filter_after_attach_succeeds() {
    let socket = NetlinkUeventSocket::new(true, SockType::SOCK_DGRAM);

    let mut attach = AttachFilter::new();
    attach.set(1);
    socket.set_option(&attach).unwrap();

    let detach = detach_filter();
    socket.set_option(&detach).unwrap();
}

#[ktest]
fn detach_filter_without_attach_is_enoent() {
    let socket = NetlinkUeventSocket::new(true, SockType::SOCK_DGRAM);

    let detach = detach_filter();
    assert!(
        socket
            .set_option(&detach)
            .is_err_and(|err| err.error() == Errno::ENOENT)
    );
}

#[ktest]
fn attach_filter_rejects_invalid_len() {
    let socket = NetlinkUeventSocket::new(true, SockType::SOCK_DGRAM);

    let mut attach = AttachFilter::new();
    attach.set(0);
    assert!(
        socket
            .set_option(&attach)
            .is_err_and(|err| err.error() == Errno::EINVAL)
    );

    attach.set(BPF_MAXINSNS.checked_add(1).unwrap());
    assert!(
        socket
            .set_option(&attach)
            .is_err_and(|err| err.error() == Errno::EINVAL)
    );

    let detach = detach_filter();
    assert!(
        socket
            .set_option(&detach)
            .is_err_and(|err| err.error() == Errno::ENOENT)
    );
}

#[ktest]
fn route_sockets_reject_unevaluated_filters() {
    let socket = NetlinkRouteSocket::new(true, SockType::SOCK_DGRAM);
    let mut attach = AttachFilter::new();
    attach.set(1);
    assert!(
        socket
            .set_option(&attach)
            .is_err_and(|err| err.error() == Errno::ENOPROTOOPT)
    );
    assert!(
        socket
            .set_option(&detach_filter())
            .is_err_and(|err| err.error() == Errno::ENOPROTOOPT)
    );
}
