//! qloudpulse from the command line. Its first command, `estimate`, prices an infrastructure change before it is
//! applied; `pulumi preview` and `pulumi up` run Pulumi and price the change on the way.

pub mod catalogue;
pub mod credentials;
pub mod link;
pub mod login;
pub mod pulumi;
pub mod render;
pub mod request;
pub mod send;
pub mod token;
pub mod update;
pub mod version;
pub mod wrap;
