// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: USB Chapter-9 descriptors and standard requests.

//! USB Chapter-9 descriptors and standard requests. Pure parsing.

pub(crate) const USB_REQ_GET_STATUS: u8 = 0;
pub(crate) const USB_REQ_CLEAR_FEATURE: u8 = 1;
pub(crate) const USB_REQ_SET_FEATURE: u8 = 3;
pub(crate) const USB_REQ_SET_ADDRESS: u8 = 5;
pub(crate) const USB_REQ_GET_DESCRIPTOR: u8 = 6;
pub(crate) const USB_REQ_SET_CONFIGURATION: u8 = 9;

pub(crate) const USB_DT_DEVICE: u8 = 1;
pub(crate) const USB_DT_CONFIG: u8 = 2;
pub(crate) const USB_DT_INTERFACE: u8 = 4;
pub(crate) const USB_DT_ENDPOINT: u8 = 5;
pub(crate) const USB_DT_HUB: u8 = 0x29;
pub(crate) const USB_DT_SS_HUB: u8 = 0x2a;

pub(crate) const USB_CLASS_HID: u8 = 0x03;
pub(crate) const USB_CLASS_MASS: u8 = 0x08;
pub(crate) const USB_CLASS_HUB: u8 = 0x09;
pub(crate) const USB_SUBCLASS_SCSI: u8 = 0x06;
pub(crate) const USB_PROTO_BOT: u8 = 0x50;
pub(crate) const USB_HID_BOOT: u8 = 0x01;
pub(crate) const USB_HID_PROTO_KEYBOARD: u8 = 0x01;
pub(crate) const USB_HID_PROTO_MOUSE: u8 = 0x02;
/// HID 1.11 §7.2.4 SET_IDLE (class, interface).
pub(crate) const HID_REQ_SET_IDLE: u8 = 0x0a;
/// HID 1.11 §7.2.6 SET_PROTOCOL (class, interface).
pub(crate) const HID_REQ_SET_PROTOCOL: u8 = 0x0b;
/// `wValue` for SET_PROTOCOL: Boot Protocol (HID 1.11 §7.2.6).
pub(crate) const HID_PROTOCOL_BOOT: u16 = 0;

pub(crate) const USB_DIR_IN: u8 = 0x80;
pub(crate) const USB_TYPE_CLASS: u8 = 0x20;
pub(crate) const USB_RECIP_DEVICE: u8 = 0x00;
pub(crate) const USB_RECIP_INTERFACE: u8 = 0x01;
pub(crate) const USB_RECIP_OTHER: u8 = 0x03;

pub(crate) const EP_ATTR_BULK: u8 = 0x02;
pub(crate) const EP_ATTR_INT: u8 = 0x03;

pub(crate) fn setup(bm: u8, req: u8, value: u16, index: u16, len: u16) -> [u8; 8] {
    [
        bm,
        req,
        value as u8,
        (value >> 8) as u8,
        index as u8,
        (index >> 8) as u8,
        len as u8,
        (len >> 8) as u8,
    ]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DeviceDesc {
    pub usb: u16,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub max_packet0: u8,
    pub vid: u16,
    pub pid: u16,
    pub num_configs: u8,
}

impl DeviceDesc {
    pub(crate) fn parse(b: &[u8]) -> Option<Self> {
        if b.len() < 18 || b[0] != 18 || b[1] != USB_DT_DEVICE {
            return None;
        }
        Some(Self {
            usb: u16::from_le_bytes([b[2], b[3]]),
            class: b[4],
            subclass: b[5],
            protocol: b[6],
            max_packet0: b[7],
            vid: u16::from_le_bytes([b[8], b[9]]),
            pid: u16::from_le_bytes([b[10], b[11]]),
            num_configs: b[17],
        })
    }

    #[cfg(any(test, ktest))]
    pub(crate) fn is_hub(self) -> bool {
        self.class == USB_CLASS_HUB
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ConfigHead {
    pub total: u16,
    pub value: u8,
    pub num_interfaces: u8,
}

impl ConfigHead {
    pub(crate) fn parse(b: &[u8]) -> Option<Self> {
        if b.len() < 9
            || b[0] != 9
            || b[1] != USB_DT_CONFIG
            || u16::from_le_bytes([b[2], b[3]]) < 9
            || b[5] == 0
        {
            return None;
        }
        Some(Self {
            total: u16::from_le_bytes([b[2], b[3]]),
            value: b[5],
            num_interfaces: b[4],
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Endpoint {
    pub addr: u8,
    pub attr: u8,
    pub max_packet: u16,
    pub interval: u8,
}

impl Endpoint {
    pub(crate) fn is_in(self) -> bool {
        self.addr & USB_DIR_IN != 0
    }
    pub(crate) fn is_bulk(self) -> bool {
        self.attr & 0x03 == EP_ATTR_BULK
    }
    pub(crate) fn is_int(self) -> bool {
        self.attr & 0x03 == EP_ATTR_INT
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Interface {
    pub number: u8,
    pub alt: u8,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IfaceKind {
    Hub,
    MscBot,
    HidBootKbd,
    HidBootMouse,
    Other,
}

impl Interface {
    pub(crate) fn kind(self) -> IfaceKind {
        if self.class == USB_CLASS_HUB {
            IfaceKind::Hub
        } else if self.class == USB_CLASS_MASS
            && self.subclass == USB_SUBCLASS_SCSI
            && self.protocol == USB_PROTO_BOT
        {
            IfaceKind::MscBot
        } else if self.class == USB_CLASS_HID && self.subclass == USB_HID_BOOT {
            if self.protocol == USB_HID_PROTO_KEYBOARD {
                IfaceKind::HidBootKbd
            } else if self.protocol == USB_HID_PROTO_MOUSE {
                IfaceKind::HidBootMouse
            } else {
                IfaceKind::Other
            }
        } else {
            IfaceKind::Other
        }
    }
}

/// Visit each complete descriptor as its type and raw bytes, including its
/// length/type header. Stop at the first truncated or zero-length descriptor.
pub(crate) fn for_each_desc(cfg: &[u8], mut f: impl FnMut(u8, &[u8])) {
    let mut i = 0usize;
    while i + 1 < cfg.len() {
        let len = cfg[i] as usize;
        if len < 2 || i + len > cfg.len() {
            break;
        }
        f(cfg[i + 1], &cfg[i..i + len]);
        i += len;
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ParsedIface {
    pub iface: Interface,
    pub eps: alloc::vec::Vec<Endpoint>,
}

/// Parse a full configuration descriptor into interfaces + endpoints.
pub(crate) fn parse_config(cfg: &[u8]) -> alloc::vec::Vec<ParsedIface> {
    use alloc::vec::Vec;
    let mut out: Vec<ParsedIface> = Vec::new();
    let mut cur: Option<ParsedIface> = None;
    for_each_desc(cfg, |ty, d| {
        if ty == USB_DT_INTERFACE && d.len() >= 9 {
            if let Some(prev) = cur.take() {
                out.push(prev);
            }
            cur = Some(ParsedIface {
                iface: Interface {
                    number: d[2],
                    alt: d[3],
                    class: d[5],
                    subclass: d[6],
                    protocol: d[7],
                },
                eps: Vec::new(),
            });
        } else if ty == USB_DT_ENDPOINT
            && d.len() >= 7
            && d[2] & 0x0f != 0
            && d[2] & 0x70 == 0
            && u16::from_le_bytes([d[4], d[5]]) & 0x7ff != 0
            && let Some(ref mut p) = cur
        {
            p.eps.push(Endpoint {
                addr: d[2],
                attr: d[3],
                max_packet: u16::from_le_bytes([d[4], d[5]]),
                interval: d[6],
            });
        }
    });
    if let Some(prev) = cur {
        out.push(prev);
    }
    out
}

pub(crate) fn find_msc_bot(cfg: &[u8]) -> Option<(Interface, Endpoint, Endpoint, u8)> {
    let head = ConfigHead::parse(cfg)?;
    for p in parse_config(cfg) {
        if p.iface.alt != 0 || p.iface.kind() != IfaceKind::MscBot {
            continue;
        }
        let Some(bulk_in) = p.eps.iter().copied().find(|e| e.is_bulk() && e.is_in()) else {
            continue;
        };
        let Some(bulk_out) = p.eps.iter().copied().find(|e| e.is_bulk() && !e.is_in()) else {
            continue;
        };
        return Some((p.iface, bulk_in, bulk_out, head.value));
    }
    None
}

pub(crate) fn find_hid_boot(cfg: &[u8], want: IfaceKind) -> Option<(Interface, Endpoint, u8)> {
    let head = ConfigHead::parse(cfg)?;
    for p in parse_config(cfg) {
        if p.iface.alt != 0 || p.iface.kind() != want {
            continue;
        }
        let Some(int_in) = p.eps.iter().copied().find(|e| e.is_int() && e.is_in()) else {
            continue;
        };
        return Some((p.iface, int_in, head.value));
    }
    None
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::ktest;

    use super::*;

    // Minimal HS mass-storage device + config (BOT, EP1 OUT / EP2 IN).
    fn device_18() -> [u8; 18] {
        let mut d = [0u8; 18];
        d[0] = 18;
        d[1] = USB_DT_DEVICE;
        d[2] = 0x00;
        d[3] = 0x02;
        d[7] = 64;
        d[8] = 0x01;
        d[9] = 0x23; // vid 0x2301
        d[10] = 0x45;
        d[11] = 0x67;
        d[17] = 1;
        d
    }

    fn config_msc() -> [u8; 32] {
        let mut c = [0u8; 32];
        // config
        c[0] = 9;
        c[1] = USB_DT_CONFIG;
        c[2] = 32;
        c[3] = 0;
        c[4] = 1;
        c[5] = 1;
        // iface
        c[9] = 9;
        c[10] = USB_DT_INTERFACE;
        c[11] = 0;
        c[13] = 2;
        c[14] = USB_CLASS_MASS;
        c[15] = USB_SUBCLASS_SCSI;
        c[16] = USB_PROTO_BOT;
        // EP1 OUT bulk
        c[18] = 7;
        c[19] = USB_DT_ENDPOINT;
        c[20] = 0x01;
        c[21] = EP_ATTR_BULK;
        c[22] = 0x00;
        c[23] = 0x02;
        // EP2 IN bulk
        c[25] = 7;
        c[26] = USB_DT_ENDPOINT;
        c[27] = 0x82;
        c[28] = EP_ATTR_BULK;
        c[29] = 0x00;
        c[30] = 0x02;
        c
    }

    #[ktest]
    fn parse_device_vid_pid() {
        let d = DeviceDesc::parse(&device_18()).unwrap();
        assert_eq!(d.vid, 0x2301);
        assert_eq!(d.pid, 0x6745);
        assert_eq!(d.max_packet0, 64);
        assert!(!d.is_hub());
    }

    #[ktest]
    fn parse_rejects_short_or_wrong_type() {
        assert!(DeviceDesc::parse(&[18, 2]).is_none());
        assert!(ConfigHead::parse(&[9, 1, 0, 0, 0, 1, 0, 0, 0]).is_none());
    }

    #[ktest]
    fn find_msc_bot_eps() {
        let (iface, inn, out, cfg) = find_msc_bot(&config_msc()).unwrap();
        assert_eq!(iface.kind(), IfaceKind::MscBot);
        assert_eq!(inn.addr, 0x82);
        assert_eq!(out.addr, 0x01);
        assert_eq!(cfg, 1);
    }

    #[ktest]
    fn setup_packet_endian() {
        let s = setup(
            0x80,
            USB_REQ_GET_DESCRIPTOR,
            (USB_DT_DEVICE as u16) << 8,
            0,
            18,
        );
        assert_eq!(s, [0x80, 6, 0, 1, 0, 0, 18, 0]);
    }

    fn config_hid_boot_kbd() -> [u8; 25] {
        let mut c = [0u8; 25];
        c[0] = 9;
        c[1] = USB_DT_CONFIG;
        c[2] = 25;
        c[4] = 1;
        c[5] = 1;
        c[9] = 9;
        c[10] = USB_DT_INTERFACE;
        c[14] = USB_CLASS_HID;
        c[15] = USB_HID_BOOT;
        c[16] = USB_HID_PROTO_KEYBOARD;
        c[18] = 7;
        c[19] = USB_DT_ENDPOINT;
        c[20] = 0x81;
        c[21] = EP_ATTR_INT;
        c[22] = 8;
        c[24] = 10; // bInterval
        c
    }

    #[ktest]
    fn find_hid_boot_kbd_ep() {
        let (iface, ep, cfg) =
            find_hid_boot(&config_hid_boot_kbd(), IfaceKind::HidBootKbd).unwrap();
        assert_eq!(iface.kind(), IfaceKind::HidBootKbd);
        assert_eq!(ep.addr, 0x81);
        assert!(ep.is_int() && ep.is_in());
        assert_eq!(ep.interval, 10);
        assert_eq!(cfg, 1);
        assert!(find_hid_boot(&config_hid_boot_kbd(), IfaceKind::HidBootMouse).is_none());
    }
}
