// Suppress removed lints from ttrpc-codegen generated code
#![allow(unknown_lints)]
#![allow(renamed_and_removed_lints)]

// Generated code is placed in src/generated/ by build.rs.
#[allow(clippy::all)]
#[allow(non_snake_case)]
#[allow(non_camel_case_types)]
#[path = "generated"]
pub mod generated {
    pub mod agent;
    pub mod agent_ttrpc;
}

pub use generated::agent::*;
pub use generated::agent_ttrpc::*;
