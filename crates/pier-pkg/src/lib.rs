#![doc = include_str!("../README.md")]
mod acquire;
mod archive;
mod build;
mod config;
mod files;
mod inspect;
mod pipeline;
mod ports;
mod process;
mod proxy;
mod templates;
mod types;

pub use archive::{ElfRecord, FileRecord};
pub use config::Service;
pub use inspect::{AppMetadata, PackageManifest, SourceKind, VariableDefinition, inspect, unpack};
pub use pipeline::{pack, validate};
pub use ports::{
    Port, PortDefinition, PortDefinitions, PortProtocol, PortValue, Ports, resolve_ports,
};
pub use types::*;
