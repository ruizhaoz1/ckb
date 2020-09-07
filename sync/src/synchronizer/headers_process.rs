use crate::block_status::BlockStatus;
use crate::synchronizer::Synchronizer;
use crate::types::{ActiveChain, SyncShared};
use crate::{Status, StatusCode, MAX_HEADERS_LEN};
use ckb_error::Error;
use ckb_logger::{debug, log_enabled, warn, Level};
use ckb_network::{CKBProtocolContext, PeerIndex};
use ckb_traits::{BlockMedianTimeContext, HeaderProvider};
use ckb_types::{core, packed, prelude::*};
use ckb_verification::{HeaderError, HeaderResolver, HeaderVerifier, Verifier};

pub struct HeadersProcess<'a> {
    message: packed::SendHeadersReader<'a>,
    synchronizer: &'a Synchronizer,
    peer: PeerIndex,
    nc: &'a dyn CKBProtocolContext,
    active_chain: ActiveChain,
}

pub struct VerifierResolver<'a> {
    shared: &'a SyncShared,
    header: &'a core::HeaderView,
    parent: Option<&'a core::HeaderView>,
}

impl<'a> VerifierResolver<'a> {
    pub fn new(
        parent: Option<&'a core::HeaderView>,
        header: &'a core::HeaderView,
        shared: &'a SyncShared,
    ) -> Self {
        VerifierResolver {
            parent,
            header,
            shared,
        }
    }
}

impl<'a> ::std::clone::Clone for VerifierResolver<'a> {
    fn clone(&self) -> Self {
        VerifierResolver {
            parent: self.parent,
            header: self.header,
            shared: self.shared,
        }
    }
}

impl<'a> BlockMedianTimeContext for VerifierResolver<'a> {
    fn median_block_count(&self) -> u64 {
        self.shared.consensus().median_time_block_count() as u64
    }
}

impl<'a> HeaderProvider for VerifierResolver<'a> {
    fn get_header(&self, hash: &packed::Byte32) -> Option<core::HeaderView> {
        self.shared.get_header(hash)
    }
}

impl<'a> HeaderResolver for VerifierResolver<'a> {
    fn header(&self) -> &core::HeaderView {
        self.header
    }

    fn parent(&self) -> Option<&core::HeaderView> {
        self.parent
    }
}

impl<'a> HeadersProcess<'a> {
    pub fn new(
        message: packed::SendHeadersReader<'a>,
        synchronizer: &'a Synchronizer,
        peer: PeerIndex,
        nc: &'a dyn CKBProtocolContext,
    ) -> Self {
        let active_chain = synchronizer.shared.active_chain();
        HeadersProcess {
            message,
            nc,
            synchronizer,
            peer,
            active_chain,
        }
    }

    fn is_continuous(&self, headers: &[core::HeaderView]) -> bool {
        for window in headers.windows(2) {
            if let [parent, header] = &window {
                if header.data().raw().parent_hash() != parent.hash() {
                    debug!(
                        "header.parent_hash {} parent.hash {}",
                        header.parent_hash(),
                        parent.hash()
                    );
                    return false;
                }
            }
        }
        true
    }

    pub fn accept_first(&self, first: &core::HeaderView) -> ValidationResult {
        let shared = self.synchronizer.shared();
        let parent = shared.get_header(&first.data().raw().parent_hash());
        let resolver = VerifierResolver::new(parent.as_ref(), &first, &shared);
        let verifier = HeaderVerifier::new(&resolver, &shared.consensus());
        let acceptor = HeaderAcceptor::new(
            first,
            self.peer,
            resolver.clone(),
            verifier,
            self.active_chain.clone(),
        );
        acceptor.accept()
    }

    fn debug(&self) {
        if log_enabled!(Level::Debug) {
            // Regain the updated best known
            let shared_best_known = self.synchronizer.shared.state().shared_best_header();
            let peer_best_known = self.synchronizer.peers().get_best_known_header(self.peer);
            debug!(
                "chain: num={}, diff={:#x};",
                self.active_chain.tip_number(),
                self.active_chain.total_difficulty()
            );
            debug!(
                "shared best_known_header: num={}, diff={:#x}, hash={};",
                shared_best_known.number(),
                shared_best_known.total_difficulty(),
                shared_best_known.hash(),
            );
            if let Some(header) = peer_best_known {
                debug!(
                    "peer's best_known_header: peer: {}, num={}; diff={:#x}, hash={};",
                    self.peer,
                    header.number(),
                    header.total_difficulty(),
                    header.hash()
                );
            } else {
                debug!("state: null;");
            }
            debug!("peer: {}", self.peer);
        }
    }

    pub fn execute(self) -> Status {
        debug!("HeadersProcess begin");
        let shared = self.synchronizer.shared();
        let headers = self
            .message
            .headers()
            .to_entity()
            .into_iter()
            .map(packed::Header::into_view)
            .collect::<Vec<_>>();

        if headers.len() > MAX_HEADERS_LEN {
            shared.state().misbehavior(self.peer, 20);
            warn!("HeadersProcess is_oversize");
            return Status::ok();
        }

        if headers.is_empty() {
            debug!("HeadersProcess is_empty (synchronized)");
            let ibd = self.active_chain.is_initial_block_download();
            if !ibd {
                if let Some(ref mut peer_state) =
                    self.synchronizer.peers().state.write().get_mut(&self.peer)
                {
                    peer_state.stop_headers_sync();
                }
            }
            return Status::ok();
        }

        if !self.is_continuous(&headers) {
            shared.state().misbehavior(self.peer, 20);
            debug!("HeadersProcess is not continuous");
            return Status::ok();
        }

        let result = self.accept_first(&headers[0]);
        if !result.is_valid() {
            if result.misbehavior > 0 {
                shared.state().misbehavior(self.peer, result.misbehavior);
            }
            debug!(
                "HeadersProcess accept_first is_valid {:?} headers = {:?}",
                result, headers[0]
            );
            return Status::ok();
        }

        for window in headers.windows(2) {
            if let [parent, header] = &window {
                let resolver = VerifierResolver::new(Some(&parent), &header, &shared);
                let verifier = HeaderVerifier::new(&resolver, &shared.consensus());
                let acceptor = HeaderAcceptor::new(
                    &header,
                    self.peer,
                    resolver.clone(),
                    verifier,
                    self.active_chain.clone(),
                );
                let result = acceptor.accept();

                if !result.is_valid() {
                    if result.misbehavior > 0 {
                        shared.state().misbehavior(self.peer, result.misbehavior);
                    }
                    debug!("HeadersProcess accept is invalid {:?}", result);
                    return Status::ok();
                }
            }
        }

        self.debug();

        if headers.len() == MAX_HEADERS_LEN {
            let start = headers.last().expect("empty checked");
            self.active_chain
                .send_getheaders_to_peer(self.nc, self.peer, start);
        }

        // If we're in IBD, we want outbound peers that will serve us a useful
        // chain. Disconnect peers that are on chains with insufficient work.
        let peer_flags = self
            .synchronizer
            .peers()
            .state
            .read()
            .get(&self.peer)
            .map(|state| state.peer_flags)
            .unwrap_or_default();
        if self.active_chain.is_initial_block_download()
            && headers.len() != MAX_HEADERS_LEN
            && (!peer_flags.is_protect && !peer_flags.is_whitelist && peer_flags.is_outbound)
        {
            debug!("Disconnect peer({}) is unprotected outbound", self.peer);
            if let Err(err) = self
                .nc
                .disconnect(self.peer, "useless outbound peer in IBD")
            {
                return StatusCode::Network.with_context(format!("Disconnect error: {:?}", err));
            }
        }

        Status::ok()
    }
}

#[derive(Clone)]
pub struct HeaderAcceptor<'a, V: Verifier> {
    header: &'a core::HeaderView,
    active_chain: ActiveChain,
    peer: PeerIndex,
    resolver: V::Target,
    verifier: V,
}

impl<'a, V> HeaderAcceptor<'a, V>
where
    V: Verifier<Target = VerifierResolver<'a>>,
{
    pub fn new(
        header: &'a core::HeaderView,
        peer: PeerIndex,
        resolver: VerifierResolver<'a>,
        verifier: V,
        active_chain: ActiveChain,
    ) -> Self {
        HeaderAcceptor {
            header,
            peer,
            resolver,
            verifier,
            active_chain,
        }
    }

    pub fn prev_block_check(&self, state: &mut ValidationResult) -> Result<(), ()> {
        if self.active_chain.contains_block_status(
            &self.header.data().raw().parent_hash(),
            BlockStatus::BLOCK_INVALID,
        ) {
            state.dos(Some(ValidationError::InvalidParent), 100);
            return Err(());
        }
        Ok(())
    }

    pub fn non_contextual_check(&self, state: &mut ValidationResult) -> Result<(), bool> {
        self.verifier.verify(&self.resolver).map_err(|error| {
            debug!(
                "HeadersProcess accept {:?} error {:?}",
                self.header.number(),
                error
            );
            // HeaderVerifier return HeaderError or UnknownParentError
            if let Some(header_error) = error.downcast_ref::<HeaderError>() {
                if header_error.is_too_new() {
                    state.temporary_invalid(Some(ValidationError::Verify(error)));
                    false
                } else {
                    state.dos(Some(ValidationError::Verify(error)), 100);
                    true
                }
            } else {
                state.dos(Some(ValidationError::Verify(error)), 100);
                true
            }
        })
    }

    pub fn version_check(&self, state: &mut ValidationResult) -> Result<(), ()> {
        if self.header.version() != 0 {
            state.invalid(Some(ValidationError::Version));
            Err(())
        } else {
            Ok(())
        }
    }

    pub fn accept(&self) -> ValidationResult {
        let mut result = ValidationResult::default();
        let shared = self.active_chain.shared();
        let state = shared.state();

        // FIXME If status == BLOCK_INVALID then return early. But which error
        // type should we return?
        let status = self.active_chain.get_block_status(&self.header.hash());
        if status.contains(BlockStatus::HEADER_VALID) {
            let header_view = shared
                .get_header_view(
                    &self.header.hash(),
                    Some(status.contains(BlockStatus::BLOCK_STORED)),
                )
                .expect("header with HEADER_VALID should exist");
            state
                .peers()
                .may_set_best_known_header(self.peer, header_view);
            return result;
        }

        if self.prev_block_check(&mut result).is_err() {
            debug!(
                "HeadersProcess reject invalid-parent header: {} {}",
                self.header.number(),
                self.header.hash(),
            );
            state.insert_block_status(self.header.hash(), BlockStatus::BLOCK_INVALID);
            return result;
        }

        if let Some(is_invalid) = self.non_contextual_check(&mut result).err() {
            debug!(
                "HeadersProcess reject non-contextual header: {} {}",
                self.header.number(),
                self.header.hash(),
            );
            if is_invalid {
                state.insert_block_status(self.header.hash(), BlockStatus::BLOCK_INVALID);
            }
            return result;
        }

        if self.version_check(&mut result).is_err() {
            debug!(
                "HeadersProcess reject invalid-version header {} {}",
                self.header.number(),
                self.header.hash(),
            );
            state.insert_block_status(self.header.hash(), BlockStatus::BLOCK_INVALID);
            return result;
        }

        shared.insert_valid_header(self.peer, &self.header);
        result
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ValidationState {
    Valid,
    TemporaryInvalid,
    Invalid,
}

impl Default for ValidationState {
    fn default() -> Self {
        ValidationState::Valid
    }
}

#[derive(Debug)]
pub enum ValidationError {
    Verify(Error),
    Version,
    InvalidParent,
}

#[derive(Debug, Default)]
pub struct ValidationResult {
    pub error: Option<ValidationError>,
    pub misbehavior: u32,
    pub state: ValidationState,
}

impl ValidationResult {
    pub fn invalid(&mut self, error: Option<ValidationError>) {
        self.dos(error, 0);
    }

    pub fn dos(&mut self, error: Option<ValidationError>, misbehavior: u32) {
        self.error = error;
        self.misbehavior += misbehavior;
        self.state = ValidationState::Invalid;
    }

    pub fn temporary_invalid(&mut self, error: Option<ValidationError>) {
        self.error = error;
        self.state = ValidationState::TemporaryInvalid;
    }

    pub fn is_valid(&self) -> bool {
        self.state == ValidationState::Valid
    }
}
