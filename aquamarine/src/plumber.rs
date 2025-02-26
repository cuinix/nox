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

use eyre::eyre;
use fluence_keypair::KeyPair;
use futures::future::BoxFuture;
use futures::FutureExt;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::task::Poll::Ready;
use std::{
    collections::{HashMap, VecDeque},
    task::{Context, Poll},
};

use futures::task::Waker;
use tokio::runtime::Handle;
use tokio::task;
use tracing::instrument;

use fluence_libp2p::PeerId;
/// For tests, mocked time is used
#[cfg(test)]
use mock_time::now_ms;
use particle_execution::{ParticleFunctionStatic, ParticleParams, ServiceFunction};
use particle_protocol::ExtendedParticle;
use particle_services::PeerScope;
use peer_metrics::ParticleExecutorMetrics;
/// Get current time from OS
#[cfg(not(test))]
use real_time::now_ms;
use workers::{KeyStorage, PeerScopes, Workers};

use crate::actor::{Actor, ActorPoll};
use crate::aqua_runtime::AquaRuntime;
use crate::deadline::Deadline;
use crate::error::AquamarineApiError;
use crate::particle_effects::{LocalRoutingEffects, RemoteRoutingEffects};
use crate::particle_functions::Functions;
use crate::spawner::{RootSpawner, Spawner, WorkerSpawner};
use crate::vm_pool::VmPool;
use crate::ParticleDataStore;

#[derive(PartialEq, Hash, Eq)]
struct ActorKey {
    signature: Vec<u8>,
    peer_scope: PeerScope,
}

const MAX_CLEANUP_KEYS_SIZE: usize = 1024;

pub struct Plumber<RT: AquaRuntime, F> {
    events: VecDeque<Result<RemoteRoutingEffects, AquamarineApiError>>,
    actors: HashMap<ActorKey, Actor<RT, F>>,
    vm_pool: VmPool<RT>,
    data_store: Arc<ParticleDataStore>,
    builtins: F,
    waker: Option<Waker>,
    metrics: Option<ParticleExecutorMetrics>,
    workers: Arc<Workers>,
    key_storage: Arc<KeyStorage>,
    scopes: PeerScopes,
    cleanup_future: Option<BoxFuture<'static, ()>>,
    root_runtime_handle: Handle,
}

impl<RT: AquaRuntime, F: ParticleFunctionStatic> Plumber<RT, F> {
    pub fn new(
        vm_pool: VmPool<RT>,
        data_store: Arc<ParticleDataStore>,
        builtins: F,
        metrics: Option<ParticleExecutorMetrics>,
        workers: Arc<Workers>,
        key_storage: Arc<KeyStorage>,
        scope: PeerScopes,
    ) -> Self {
        Self {
            vm_pool,
            data_store,
            builtins,
            events: <_>::default(),
            actors: <_>::default(),
            waker: <_>::default(),
            metrics,
            workers,
            key_storage,
            scopes: scope,
            cleanup_future: None,
            root_runtime_handle: Handle::current(),
        }
    }

    /// Receives and ingests incoming particle: creates a new actor or forwards to the existing mailbox
    #[instrument(level = tracing::Level::INFO, skip_all)]
    pub fn ingest(
        &mut self,
        particle: ExtendedParticle,
        function: Option<ServiceFunction>,
        peer_scope: PeerScope,
    ) {
        self.wake();

        let deadline = Deadline::from(particle.as_ref());
        if deadline.is_expired(now_ms()) {
            tracing::info!(target: "expired", particle_id = particle.particle.id, "Particle is expired");
            self.events
                .push_back(Err(AquamarineApiError::ParticleExpired {
                    particle_id: particle.particle.id,
                }));
            return;
        }

        if let Err(err) = particle.particle.verify() {
            tracing::warn!(target: "signature", particle_id = particle.particle.id, "Particle signature verification failed: {err:?}");
            self.events
                .push_back(Err(AquamarineApiError::SignatureVerificationFailed {
                    particle_id: particle.particle.id,
                    err,
                }));
            return;
        }

        if let PeerScope::WorkerId(worker_id) = peer_scope {
            let is_active = self.workers.is_worker_active(worker_id);
            let is_manager = self.scopes.is_management(particle.particle.init_peer_id);
            let is_host = self.scopes.is_host(particle.particle.init_peer_id);

            // Only a manager or the host itself is allowed to access deactivated workers
            if !is_active && !is_manager && !is_host {
                tracing::trace!(target: "worker_inactive", particle_id = particle.particle.id, worker_id = worker_id.to_string(), "Worker is not active");
                return;
            }
        };

        let key = ActorKey {
            signature: particle.particle.signature.clone(),
            peer_scope,
        };

        let actor = self.get_or_create_actor(peer_scope, key, &particle);

        debug_assert!(actor.is_ok(), "no such worker: {:#?}", actor.err());

        match actor {
            Ok(actor) => {
                actor.ingest(particle);
                if let Some(function) = function {
                    actor.set_function(function);
                }
            }
            Err(err) => tracing::warn!(
                "No such worker {:?}, rejected particle {particle_id}: {:?}",
                peer_scope,
                err,
                particle_id = particle.particle.id,
            ),
        }
    }

    fn get_or_create_actor(
        &mut self,
        peer_scope: PeerScope,
        key: ActorKey,
        particle: &ExtendedParticle,
    ) -> eyre::Result<&mut Actor<RT, F>> {
        let entry = self.actors.entry(key);
        let builtins = &self.builtins;
        match entry {
            Entry::Occupied(actor) => Ok(actor.into_mut()),
            Entry::Vacant(entry) => {
                // TODO: move to a better place
                let particle_token = get_particle_token(
                    &self.key_storage.root_key_pair,
                    &particle.particle.signature,
                )?;
                let params = ParticleParams::clone_from(
                    particle.as_ref(),
                    peer_scope,
                    particle_token.clone(),
                );
                let functions = Functions::new(params, builtins.clone());
                let key_pair = self
                    .key_storage
                    .get_keypair(peer_scope)
                    .ok_or(eyre!("Not found key pair for {:?}", peer_scope))?;
                let (deal_id, spawner, current_peer_id) = match peer_scope {
                    PeerScope::WorkerId(worker_id) => {
                        let deal_id = self
                            .workers
                            .get_deal_id(worker_id)
                            .map_err(|err| eyre!("Not found deal for {:?} : {}", worker_id, err))?;
                        let runtime_handle = self
                            .workers
                            .get_handle(worker_id)
                            .ok_or(eyre!("Not found runtime handle for {:?}", worker_id))?;
                        let spawner =
                            Spawner::Worker(WorkerSpawner::new(runtime_handle, worker_id));
                        let current_peer_id: PeerId = worker_id.into();
                        (Some(deal_id), spawner, current_peer_id)
                    }
                    PeerScope::Host => {
                        let spawner =
                            Spawner::Root(RootSpawner::new(self.root_runtime_handle.clone()));
                        let current_peer_id = self.scopes.get_host_peer_id();
                        (None, spawner, current_peer_id)
                    }
                };

                let data_store = self.data_store.clone();
                let actor = Actor::new(
                    particle.as_ref(),
                    functions,
                    current_peer_id,
                    particle_token,
                    key_pair,
                    data_store,
                    deal_id,
                    spawner,
                );
                let res = entry.insert(actor);
                Ok(res)
            }
        }
    }

    pub fn add_service(
        &self,
        service: String,
        functions: HashMap<String, ServiceFunction>,
        fallback: Option<ServiceFunction>,
    ) {
        let builtins = self.builtins.clone();
        let task = async move {
            builtins.extend(service, functions, fallback).await;
        };
        task::Builder::new()
            .name("Add service")
            .spawn(task)
            .expect("Could not spawn add service task");
    }

    pub fn remove_service(&self, service: String) {
        let builtins = self.builtins.clone();
        let task = async move {
            builtins.remove(&service).await;
        };
        task::Builder::new()
            .name("Remove service")
            .spawn(task)
            .expect("Could not spawn remove service task");
    }

    pub fn poll(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<RemoteRoutingEffects, AquamarineApiError>> {
        self.waker = Some(cx.waker().clone());

        self.vm_pool.poll(cx);

        if let Some(event) = self.events.pop_front() {
            return Poll::Ready(event);
        }

        // Gather effects and put VMs back
        let mut remote_effects: Vec<RemoteRoutingEffects> = vec![];
        let mut local_effects: Vec<LocalRoutingEffects> = vec![];
        let mut interpretation_stats = vec![];
        let mut mailbox_size = 0;
        for actor in self.actors.values_mut() {
            if let Poll::Ready(result) = actor.poll_completed(cx) {
                interpretation_stats.push(result.stats);

                let mut remote_peers = vec![];
                let mut local_peers = vec![];
                for next_peer in result.effects.next_peers {
                    let scope = self.scopes.scope(next_peer);
                    match scope {
                        Err(_) => {
                            remote_peers.push(next_peer);
                        }
                        Ok(scope) => {
                            local_peers.push(scope);
                        }
                    }
                }

                if !remote_peers.is_empty() {
                    remote_effects.push(RemoteRoutingEffects {
                        particle: result.effects.particle.clone(),
                        next_peers: remote_peers,
                    });
                }

                if !local_peers.is_empty() {
                    local_effects.push(LocalRoutingEffects {
                        particle: result.effects.particle.clone(),
                        next_peers: local_peers,
                    });
                }

                let (vm_id, vm) = result.runtime;
                if let Some(vm) = vm {
                    self.vm_pool.put_vm(vm_id, vm);
                } else {
                    // if `result.vm` is None, then an AVM instance was lost due to
                    // panic or cancellation, and we must ask VmPool to recreate that AVM
                    // TODO: add a Count metric to count how often we call `recreate_avm`
                    self.vm_pool.recreate_avm(vm_id, cx);
                }
            }
            mailbox_size += actor.mailbox_size();
        }

        if let Some(Ready(())) = self.cleanup_future.as_mut().map(|f| f.poll_unpin(cx)) {
            // we remove clean up future if it is ready
            self.cleanup_future.take();
        }

        // do not schedule task if another in progress
        if self.cleanup_future.is_none() {
            // Remove expired actors
            let mut cleanup_keys: Vec<(String, PeerId, Vec<u8>, String)> =
                Vec::with_capacity(MAX_CLEANUP_KEYS_SIZE);
            let now = now_ms();
            self.actors.retain(|_, actor| {
                // TODO: this code isn't optimal we continue iterate over actors if cleanup keys is full
                // should be simpler to optimize it after fixing NET-632
                // also delete fn actor.cleanup_key()
                if cleanup_keys.len() >= MAX_CLEANUP_KEYS_SIZE {
                    return true;
                }
                // if actor hasn't yet expired or is still executing, keep it
                if !actor.is_expired(now) || actor.is_executing() {
                    return true; // keep actor
                }
                cleanup_keys.push(actor.cleanup_key());
                false // remove actor
            });

            if !cleanup_keys.is_empty() {
                let data_store = self.data_store.clone();
                self.cleanup_future =
                    Some(async move { data_store.batch_cleanup_data(cleanup_keys).await }.boxed())
            }
        }

        // Execute next messages
        let mut stats = vec![];
        for actor in self.actors.values_mut() {
            if let Some((vm_id, vm)) = self.vm_pool.get_vm() {
                match actor.poll_next(vm_id, vm, cx) {
                    ActorPoll::Vm(vm_id, vm) => self.vm_pool.put_vm(vm_id, vm),
                    ActorPoll::Executing(mut s) => stats.append(&mut s),
                }
            } else {
                // TODO: calculate deviations from normal mailbox_size
                if mailbox_size > 11 {
                    log::warn!(
                        "{} particles waiting in mailboxes, but all interpreters busy",
                        mailbox_size
                    );
                }
                break;
            }
        }
        self.meter(|m| {
            for stat in &interpretation_stats {
                // count particle interpretations
                if stat.success {
                    m.interpretation_successes.inc();
                } else {
                    m.interpretation_failures.inc();
                }

                let interpretation_time = stat.interpretation_time.as_secs_f64();
                m.interpretation_time_sec.observe(interpretation_time);
            }
            m.total_actors_mailbox.set(mailbox_size as i64);
            m.alive_actors.set(self.actors.len() as i64);

            for stat in &stats {
                m.service_call(stat.success, stat.kind, stat.call_time)
            }
        });

        for effect in local_effects {
            for local_peer in effect.next_peers {
                let span = tracing::info_span!(parent: effect.particle.span.as_ref(), "Plumber: routing effect ingest");
                let _guard = span.enter();
                self.ingest(effect.particle.clone(), None, local_peer);
            }
        }

        // Turn effects into events, and buffer them
        self.events.extend(remote_effects.into_iter().map(Ok));

        // Return a new event if there is some
        if let Some(event) = self.events.pop_front() {
            return Poll::Ready(event);
        }

        Poll::Pending
    }

    fn wake(&self) {
        if let Some(waker) = &self.waker {
            waker.wake_by_ref();
        }
    }

    fn meter<U, FF: Fn(&ParticleExecutorMetrics) -> U>(&self, f: FF) {
        self.metrics.as_ref().map(f);
    }
}

fn get_particle_token(key_pair: &KeyPair, signature: &Vec<u8>) -> eyre::Result<String> {
    let particle_token = key_pair.sign(signature.as_slice()).map_err(|err| {
        eyre!(
            "Could not produce particle token by signing the particle signature: {}",
            err
        )
    })?;
    Ok(bs58::encode(particle_token.to_vec()).into_string())
}

/// Implements `now` by taking number of non-leap seconds from `Utc::now()`
mod real_time {
    #[allow(dead_code)]
    pub fn now_ms() -> u64 {
        (chrono::Utc::now().timestamp() * 1000) as u64
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::convert::Infallible;
    use std::path::PathBuf;
    use std::task::Waker;
    use std::{sync::Arc, task::Context};

    use avm_server::{AVMMemoryStats, CallResults, ParticleParameters};
    use fluence_keypair::KeyPair;
    use fluence_libp2p::RandomPeerId;
    use futures::task::noop_waker_ref;
    use workers::{DummyCoreManager, KeyStorage, PeerScopes, Workers};

    use particle_args::Args;
    use particle_execution::{FunctionOutcome, ParticleFunction, ParticleParams, ServiceFunction};
    use particle_protocol::{ExtendedParticle, Particle};

    use crate::deadline::Deadline;
    use crate::plumber::mock_time::set_mock_time;
    use crate::plumber::{now_ms, real_time};
    use crate::vm_pool::VmPool;
    use crate::AquamarineApiError::ParticleExpired;
    use crate::{AquaRuntime, ParticleDataStore, ParticleEffects, Plumber};
    use async_trait::async_trait;
    use avm_server::avm_runner::RawAVMOutcome;
    use particle_services::PeerScope;
    use tracing::Span;

    struct MockF;

    #[async_trait]
    impl ParticleFunction for MockF {
        async fn call(&self, _args: Args, _particle: ParticleParams) -> FunctionOutcome {
            panic!("no builtins in plumber tests!")
        }

        async fn extend(
            &self,
            _service: String,
            _functions: HashMap<String, ServiceFunction>,
            _fallback: Option<ServiceFunction>,
        ) {
            todo!()
        }

        async fn remove(&self, _service: &str) {
            todo!()
        }
    }

    struct VMMock;

    impl AquaRuntime for VMMock {
        type Config = ();
        type Error = Infallible;

        fn create_runtime(_config: Self::Config, _waker: Waker) -> Result<Self, Self::Error> {
            Ok(VMMock)
        }

        fn into_effects(
            _outcome: Result<RawAVMOutcome, Self::Error>,
            _particle_id: String,
        ) -> ParticleEffects {
            ParticleEffects {
                new_data: vec![],
                next_peers: vec![],
                call_requests: Default::default(),
            }
        }

        fn call(
            &mut self,
            _air: impl Into<String>,
            _prev_data: impl Into<Vec<u8>>,
            _current_data: impl Into<Vec<u8>>,
            _particle_params: ParticleParameters<'_>,
            _call_results: CallResults,
            _key_pair: &KeyPair,
        ) -> Result<RawAVMOutcome, Self::Error> {
            let soft_limits_triggering = <_>::default();
            Ok(RawAVMOutcome {
                ret_code: 0,
                error_message: "".to_string(),
                data: vec![],
                call_requests: Default::default(),
                next_peer_pks: vec![],
                soft_limits_triggering,
            })
        }

        fn memory_stats(&self) -> AVMMemoryStats {
            AVMMemoryStats {
                memory_size: 0,
                total_memory_limit: None,
                allocation_rejects: None,
            }
        }
    }

    async fn plumber() -> Plumber<VMMock, Arc<MockF>> {
        // Pool is of size 1 so it's easier to control tests
        let vm_pool = VmPool::new(1, (), None, None);
        let builtin_mock = Arc::new(MockF);

        let root_key_pair: KeyPair = KeyPair::generate_ed25519().into();
        let key_pair_path: PathBuf = "keypair".into();
        let workers_path: PathBuf = "workers".into();
        let key_storage = KeyStorage::from_path(key_pair_path.clone(), root_key_pair.clone())
            .await
            .expect("Could not load key storage");

        let key_storage = Arc::new(key_storage);

        let core_manager = Arc::new(DummyCoreManager::default().into());

        let scope = PeerScopes::new(
            root_key_pair.get_peer_id(),
            RandomPeerId::random(),
            RandomPeerId::random(),
            key_storage.clone(),
        );

        let workers = Workers::from_path(workers_path.clone(), key_storage.clone(), core_manager)
            .await
            .expect("Could not load worker registry");

        let workers = Arc::new(workers);

        let tmp_dir = tempfile::tempdir().expect("Could not create temp dir");
        let tmp_path = tmp_dir.path();
        let data_store = ParticleDataStore::new(
            tmp_path.join("particles"),
            tmp_path.join("vault"),
            tmp_path.join("anomaly"),
        );
        data_store
            .initialize()
            .await
            .expect("Could not initialize datastore");
        let data_store = Arc::new(data_store);

        Plumber::new(
            vm_pool,
            data_store,
            builtin_mock,
            None,
            workers.clone(),
            key_storage.clone(),
            scope.clone(),
        )
    }

    fn particle(ts: u64, ttl: u32) -> Particle {
        let mut particle = Particle::default();
        particle.timestamp = ts;
        particle.ttl = ttl;

        particle
    }

    fn context() -> Context<'static> {
        Context::from_waker(noop_waker_ref())
    }

    /// Checks that expired actor will be removed
    #[ignore]
    #[tokio::test]
    async fn remove_expired() {
        set_mock_time(real_time::now_ms());

        let mut plumber = plumber().await;

        let particle = particle(now_ms(), 1);
        let deadline = Deadline::from(&particle);
        assert!(!deadline.is_expired(now_ms()));

        plumber.ingest(
            ExtendedParticle::new(particle, Span::none()),
            None,
            PeerScope::Host,
        );

        assert_eq!(plumber.actors.len(), 1);
        let mut cx = context();
        assert!(plumber.poll(&mut cx).is_pending());
        assert_eq!(plumber.actors.len(), 1);

        assert_eq!(plumber.vm_pool.free_vms(), 0);
        // pool is single VM, wait until VM is free
        loop {
            if plumber.vm_pool.free_vms() == 1 {
                break;
            };
            // 'is_pending' is used to suppress "must use" warning
            plumber.poll(&mut cx).is_pending();
        }

        set_mock_time(now_ms() + 2);
        assert!(plumber.poll(&mut cx).is_pending());
        assert_eq!(plumber.actors.len(), 0);
    }

    /// Checks that expired particle won't create an actor
    #[tokio::test]
    async fn ignore_expired() {
        set_mock_time(real_time::now_ms());
        // set_mock_time(1000);

        let mut plumber = plumber().await;
        let particle = particle(now_ms() - 100, 99);
        let deadline = Deadline::from(&particle);
        assert!(deadline.is_expired(now_ms()));

        plumber.ingest(
            ExtendedParticle::new(particle.clone(), Span::none()),
            None,
            PeerScope::Host,
        );

        assert_eq!(plumber.actors.len(), 0);

        // Check actor doesn't appear after poll somehow
        set_mock_time(now_ms() + 1000);
        let poll = plumber.poll(&mut context());
        assert!(poll.is_ready());
        match poll {
            std::task::Poll::Ready(Err(ParticleExpired { particle_id })) => {
                assert_eq!(particle_id, particle.id)
            }
            unexpected => panic!(
                "Expected Poll::Ready(Err(AquamarineApiError::ParticleExpired)), got {:?}",
                unexpected
            ),
        }
        assert_eq!(plumber.actors.len(), 0);
    }
}

/// Code taken from https://blog.iany.me/2019/03/how-to-mock-time-in-rust-tests-and-cargo-gotchas-we-met/
/// And then modified to use u64 instead of `SystemTime`
#[cfg(test)]
pub mod mock_time {
    #![allow(dead_code)]

    use std::cell::RefCell;

    thread_local! {
        static MOCK_TIME: RefCell<u64> = RefCell::new(0);
    }

    pub fn now_ms() -> u64 {
        MOCK_TIME.with(|cell| *cell.borrow())
    }

    pub fn set_mock_time(time: u64) {
        MOCK_TIME.with(|cell| *cell.borrow_mut() = time);
    }
}
