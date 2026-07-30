//! OWNER: unit S-04 (server/src/api/{rig,fit,catalog,usage,requests,jobs}.rs,
//! server/src/jobs.rs). Do not edit outside that unit.
//!
//! The `?no_wait=true` job runner.
//!
//! The one rule: a job is flipped to `Failed` on **every** error path, including a
//! `JoinError` from a panicking task. **Nothing sits `Pending` forever.**

use apexrouter_protocol::{JobId, JobRecord};

/// Every background operation, live and recently finished.
#[derive(Debug, Default)]
pub struct JobRegistry {/* S-04 */}

impl JobRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        JobRegistry::default()
    }

    /// One job's record.
    pub fn get(&self, id: JobId) -> Option<JobRecord> {
        todo!("S-04: JobRegistry::get")
    }

    /// Every job's record, newest first.
    pub fn all(&self) -> Vec<JobRecord> {
        todo!("S-04: JobRegistry::all")
    }
}
