/*
 * Copyright 2020 Fluence Labs Limited
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use humantime::FormattedDuration;
use thiserror::Error;

use particle_protocol::ParticleError;

#[derive(Debug, Error)]
pub enum AquamarineApiError {
    #[error("AquamarineApiError::ParticleExpired: particle_id = {particle_id}")]
    ParticleExpired { particle_id: String },
    #[error(
        r#"AquamarineApiError::OneshotCancelled: particle_id = {particle_id}.
        Aquamarine dropped particle processing before sending effects back.
        This is unexpected and shouldn't happen"#
    )]
    OneshotCancelled { particle_id: String },
    #[error(
        r#"AquamarineApiError::AquamarineDied: particle_id = {particle_id:?}.
        Aquamarine couldn't be reached from the NetworkApi.
        This is unexpected and shouldn't happen."#
    )]
    AquamarineDied { particle_id: Option<String> },
    #[error(
        "AquamarineApiError::ExecutionTimedOut: particle_id = {particle_id}, timeout = {timeout}"
    )]
    ExecutionTimedOut {
        particle_id: String,
        timeout: FormattedDuration,
    },
    #[error(
        "AquamarineApiError::AquamarineQueueFull: can't send particle {particle_id:?} to Aquamarine"
    )]
    AquamarineQueueFull { particle_id: Option<String> },
    #[error("AquamarineApiError::SignatureVerificationFailed: particle_id = {particle_id}, error = {err}")]
    SignatureVerificationFailed {
        particle_id: String,
        err: ParticleError,
    },
    #[error("AquamarineApiError::WorkerIsNotActive: worker_id = {worker_id}, particle_id = {particle_id}")]
    WorkerIsNotActive {
        worker_id: String,
        particle_id: String,
    },
}

impl AquamarineApiError {
    pub fn into_particle_id(self) -> Option<String> {
        match self {
            AquamarineApiError::ParticleExpired { particle_id } => Some(particle_id),
            AquamarineApiError::OneshotCancelled { particle_id } => Some(particle_id),
            AquamarineApiError::ExecutionTimedOut { particle_id, .. } => Some(particle_id),
            AquamarineApiError::WorkerIsNotActive { particle_id, .. } => Some(particle_id),
            // Should it be `None`  considering usage of signature as particle id?
            // It can compromise valid particles into thinking they are invalid.
            // But still there can be a case when signature was generated wrong
            // and client will never know about it.
            AquamarineApiError::SignatureVerificationFailed { .. } => None,
            AquamarineApiError::AquamarineDied { particle_id } => particle_id,
            AquamarineApiError::AquamarineQueueFull { particle_id, .. } => particle_id,
        }
    }
}
