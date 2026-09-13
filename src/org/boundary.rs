//! Open-source host versus proprietary Connect product boundary.

pub const LOCAL_OPEN_SOURCE_SURFACE: &[&str] = &[
    "src/domain",
    "src/protocol",
    "src/connect",
    "src/org",
    "src/prompts",
    "local host / GPUI / direct client",
];

pub const PROPRIETARY_CONNECT_SURFACE: &[&str] = &[
    "hosted accounts",
    "routing",
    "management/billing/retention",
    "Portal Board/BoardCard administration",
];

pub const SHARED_OPEN_WIRE_SCHEMAS: &[&str] = &[
    "Connect payload catalog v1",
    "organization generic-extension type ids 1001-1007",
    "EvidenceBundle manifest v1",
    "LocalActionRequest/Receipt v1",
];

pub const NO_PROPRIETARY_KEY_REQUIRED_FOR_LOCAL: bool = true;

pub const ANONYMOUS_STANDALONE_REMAINS_DEFAULT: bool = true;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProductBoundary {
    pub local_open_source: bool,
    pub hosted_connect_proprietary: bool,
    pub shared_wire_schemas_open: bool,
    pub local_requires_connect_key: bool,
}

impl ProductBoundary {
    pub const fn current() -> Self {
        Self {
            local_open_source: true,
            hosted_connect_proprietary: true,
            shared_wire_schemas_open: true,
            local_requires_connect_key: false,
        }
    }

    pub const fn standalone_usable(&self) -> bool {
        self.local_open_source && !self.local_requires_connect_key
    }
}
