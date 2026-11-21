//! `qloudpulse-collect`: reads cloud accounts with the customer's own credentials, records the raw API responses,
//! redacts them by a local config and packs them into a bundle. See `docs/offline-collection.md`.
//!
//! The only qloudpulse code is [`deliver::qloudpulse`], behind the `upload` feature.

pub mod analyze;
pub mod aws;
pub mod body;
pub mod bundle;
pub mod callplan;
pub mod collect;
pub mod config;
pub mod deliver;
pub mod derive;
pub mod encrypt;
pub mod identifiers;
pub mod log;
pub mod pseudonym;
pub mod record;
pub mod redact;
pub mod run;
pub mod scan;
pub mod snapshot;
pub mod window;
