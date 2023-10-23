use std::{
    collections::{hash_map::Entry, HashMap},
    fmt::{Debug, Display, Error as FmtError, Formatter},
};

use sp_runtime::SaturatedConversion;

use crate::{
    aleph_primitives::{AuraId, BlockNumber},
    session::{SessionBoundaryInfo, SessionId},
    session_map::AuthorityProvider,
    sync::{
        substrate::verification::{verifier::SessionVerifier, FinalizationInfo},
        Header,
    },
};

/// Ways in which a justification can fail verification.
#[derive(Debug, PartialEq, Eq)]
pub enum CacheError {
    UnknownAuthorities(SessionId),
    UnknownAuraAuthorities(SessionId),
    SessionTooOld(SessionId, SessionId),
    SessionInFuture(SessionId, SessionId),
    BadGenesisHeader,
}

impl Display for CacheError {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), FmtError> {
        use CacheError::*;
        match self {
            SessionTooOld(session, lower_bound) => write!(
                f,
                "session {session:?} is too old. Should be at least {lower_bound:?}"
            ),
            SessionInFuture(session, upper_bound) => write!(
                f,
                "session {session:?} without known authorities. Should be at most {upper_bound:?}"
            ),
            UnknownAuthorities(session) => {
                write!(
                    f,
                    "authorities for session {session:?} not known even though they should be"
                )
            }
            UnknownAuraAuthorities(session) => {
                write!(
                    f,
                    "Aura authorities for session {session:?} not known even though they should be"
                )
            }
            BadGenesisHeader => {
                write!(
                    f,
                    "the provided genesis header does not match the cached genesis header"
                )
            }
        }
    }
}

struct CachedData {
    session_verifier: SessionVerifier,
    aura_authorities: Vec<AuraId>,
}

/// Cache storing SessionVerifier structs and Aura authorities for multiple sessions.
/// Keeps up to `cache_size` verifiers of top sessions.
/// If the session is too new or ancient it will fail to return requested data.
/// Highest session verifier this cache returns is for the session after the current finalization session.
/// Lowest session verifier this cache returns is for `top_returned_session` - `cache_size`.
pub struct VerifierCache<AP, FI, H>
where
    AP: AuthorityProvider,
    FI: FinalizationInfo,
    H: Header,
{
    cached_data: HashMap<SessionId, CachedData>,
    session_info: SessionBoundaryInfo,
    finalization_info: FI,
    authority_provider: AP,
    cache_size: usize,
    /// Lowest currently available session.
    lower_bound: SessionId,
    genesis_header: H,
}

impl<AP, FI, H> VerifierCache<AP, FI, H>
where
    AP: AuthorityProvider,
    FI: FinalizationInfo,
    H: Header,
{
    pub fn new(
        session_info: SessionBoundaryInfo,
        finalization_info: FI,
        authority_provider: AP,
        cache_size: usize,
        genesis_header: H,
    ) -> Self {
        Self {
            cached_data: HashMap::new(),
            session_info,
            finalization_info,
            authority_provider,
            cache_size,
            lower_bound: SessionId(0),
            genesis_header,
        }
    }

    pub fn genesis_header(&self) -> &H {
        &self.genesis_header
    }
}

fn download_data<AP: AuthorityProvider>(
    authority_provider: &AP,
    session_id: SessionId,
    session_info: &SessionBoundaryInfo,
) -> Result<CachedData, CacheError> {
    Ok(match session_id {
        SessionId(0) => CachedData {
            session_verifier: authority_provider
                .authority_data(0)
                .ok_or(CacheError::UnknownAuthorities(session_id))?
                .into(),
            aura_authorities: authority_provider
                .aura_authorities(0)
                .ok_or(CacheError::UnknownAuraAuthorities(session_id))?,
        },
        SessionId(id) => {
            let prev_first = session_info.first_block_of_session(SessionId(id - 1));
            CachedData {
                session_verifier: authority_provider
                    .next_authority_data(prev_first)
                    .ok_or(CacheError::UnknownAuthorities(session_id))?
                    .into(),
                aura_authorities: authority_provider
                    .next_aura_authorities(prev_first)
                    .ok_or(CacheError::UnknownAuraAuthorities(session_id))?,
            }
        }
    })
}

impl<AP, FI, H> VerifierCache<AP, FI, H>
where
    AP: AuthorityProvider,
    FI: FinalizationInfo,
    H: Header,
{
    // Prune old session data if necessary
    fn try_prune(&mut self, session_id: SessionId) {
        if session_id.0
            >= self
                .lower_bound
                .0
                .saturating_add(self.cache_size.saturated_into())
        {
            let new_lower_bound = SessionId(
                session_id
                    .0
                    .saturating_sub(self.cache_size.saturated_into())
                    + 1,
            );
            self.cached_data.retain(|&id, _| id >= new_lower_bound);
            self.lower_bound = new_lower_bound;
        }
    }

    fn get_data(&mut self, number: BlockNumber) -> Result<&CachedData, CacheError> {
        let session_id = self.session_info.session_id_from_block_num(number);

        if session_id < self.lower_bound {
            return Err(CacheError::SessionTooOld(session_id, self.lower_bound));
        }

        // We are sure about authorities in all session that have first block
        // from previous session finalized.
        let upper_bound = SessionId(
            self.session_info
                .session_id_from_block_num(self.finalization_info.finalized_number())
                .0
                + 1,
        );
        if session_id > upper_bound {
            return Err(CacheError::SessionInFuture(session_id, upper_bound));
        }

        self.try_prune(session_id);

        Ok(match self.cached_data.entry(session_id) {
            Entry::Occupied(occupied) => occupied.into_mut(),
            Entry::Vacant(vacant) => vacant.insert(download_data(
                &self.authority_provider,
                session_id,
                &self.session_info,
            )?),
        })
    }

    /// Returns the list of Aura authorities for a given block number. Updates cache if necessary.
    /// Must be called using the number of the PARENT of the verified block.
    /// This method assumes that the queued Aura authorities will indeed become Aura authorities
    /// in the next session.
    pub fn get_aura_authorities(
        &mut self,
        number: BlockNumber,
    ) -> Result<&Vec<AuraId>, CacheError> {
        Ok(&self.get_data(number)?.aura_authorities)
    }

    /// Returns session verifier for block number if available. Updates cache if necessary.
    /// Must be called using the number of the verified block.
    pub fn get(&mut self, number: BlockNumber) -> Result<&SessionVerifier, CacheError> {
        Ok(&self.get_data(number)?.session_verifier)
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, collections::HashMap};

    use sp_consensus_aura::sr25519::AuthorityId as AuraId;
    use sp_runtime::testing::UintAuthorityId;

    use super::{
        AuthorityProvider, BlockNumber, CacheError, FinalizationInfo, SessionVerifier,
        VerifierCache,
    };
    use crate::{
        aleph_primitives::SessionAuthorityData,
        session::{testing::authority_data, SessionBoundaryInfo, SessionId},
        sync::mock::MockHeader,
        SessionPeriod,
    };

    const SESSION_PERIOD: u32 = 30;
    const CACHE_SIZE: usize = 3;

    type TestVerifierCache<'a> =
        VerifierCache<MockAuthorityProvider, MockFinalizationInfo<'a>, MockHeader>;

    struct MockFinalizationInfo<'a> {
        finalized_number: &'a Cell<BlockNumber>,
    }

    impl<'a> FinalizationInfo for MockFinalizationInfo<'a> {
        fn finalized_number(&self) -> BlockNumber {
            self.finalized_number.get()
        }
    }

    struct MockAuthorityProvider {
        session_map: HashMap<SessionId, SessionAuthorityData>,
        aura_authority_map: HashMap<SessionId, Vec<AuraId>>,
        session_info: SessionBoundaryInfo,
    }

    fn authority_data_for_session(session_id: u32) -> SessionAuthorityData {
        authority_data(session_id * 4, (session_id + 1) * 4)
    }

    fn aura_authority_data_for_session(session_id: u32) -> Vec<AuraId> {
        (session_id * 4..(session_id + 1) * 4)
            .map(|id| UintAuthorityId(id.into()).to_public_key())
            .collect()
    }

    impl MockAuthorityProvider {
        fn new(session_n: u32) -> Self {
            let session_map = (0..session_n + 1)
                .map(|s| (SessionId(s), authority_data_for_session(s)))
                .collect();
            let aura_authority_map = (0..session_n + 1)
                .map(|s| (SessionId(s), aura_authority_data_for_session(s)))
                .collect();
            Self {
                session_map,
                aura_authority_map,
                session_info: SessionBoundaryInfo::new(SessionPeriod(SESSION_PERIOD)),
            }
        }
    }

    impl AuthorityProvider for MockAuthorityProvider {
        fn authority_data(&self, block_number: BlockNumber) -> Option<SessionAuthorityData> {
            self.session_map
                .get(&self.session_info.session_id_from_block_num(block_number))
                .cloned()
        }

        fn next_authority_data(&self, block_number: BlockNumber) -> Option<SessionAuthorityData> {
            self.session_map
                .get(&SessionId(
                    self.session_info.session_id_from_block_num(block_number).0 + 1,
                ))
                .cloned()
        }

        fn aura_authorities(&self, block_number: BlockNumber) -> Option<Vec<AuraId>> {
            self.aura_authority_map
                .get(&self.session_info.session_id_from_block_num(block_number))
                .cloned()
        }

        fn next_aura_authorities(&self, block_number: BlockNumber) -> Option<Vec<AuraId>> {
            self.aura_authority_map
                .get(&SessionId(
                    self.session_info.session_id_from_block_num(block_number).0 + 1,
                ))
                .cloned()
        }
    }

    fn setup_test(max_session_n: u32, finalized_number: &'_ Cell<u32>) -> TestVerifierCache<'_> {
        let finalization_info = MockFinalizationInfo { finalized_number };
        let authority_provider = MockAuthorityProvider::new(max_session_n);
        let genesis_header = MockHeader::random_parentless(0);

        VerifierCache::new(
            SessionBoundaryInfo::new(SessionPeriod(SESSION_PERIOD)),
            finalization_info,
            authority_provider,
            CACHE_SIZE,
            genesis_header,
        )
    }

    fn finalize_first_in_session(finalized_number: &Cell<u32>, session_id: u32) {
        finalized_number.set(session_id * SESSION_PERIOD);
    }

    fn session_verifier(
        verifier: &mut TestVerifierCache,
        session_id: u32,
    ) -> Result<SessionVerifier, CacheError> {
        verifier.get((session_id + 1) * SESSION_PERIOD - 1).cloned()
    }

    fn check_session_verifier(verifier: &mut TestVerifierCache, session_id: u32) {
        let session_verifier =
            session_verifier(verifier, session_id).expect("Should return verifier. Got error");
        let expected_verifier: SessionVerifier = authority_data_for_session(session_id).into();
        assert_eq!(session_verifier, expected_verifier);
    }

    #[test]
    fn genesis_session() {
        let finalized_number = Cell::new(0);

        let mut verifier = setup_test(0, &finalized_number);

        check_session_verifier(&mut verifier, 0);
    }

    #[test]
    fn normal_session() {
        let finalized_number = Cell::new(0);

        let mut verifier = setup_test(3, &finalized_number);

        check_session_verifier(&mut verifier, 0);
        check_session_verifier(&mut verifier, 1);

        finalize_first_in_session(&finalized_number, 1);
        check_session_verifier(&mut verifier, 0);
        check_session_verifier(&mut verifier, 1);
        check_session_verifier(&mut verifier, 2);

        finalize_first_in_session(&finalized_number, 2);
        check_session_verifier(&mut verifier, 1);
        check_session_verifier(&mut verifier, 2);
        check_session_verifier(&mut verifier, 3);
    }

    #[test]
    fn prunes_old_sessions() {
        assert_eq!(CACHE_SIZE, 3);

        let finalized_number = Cell::new(0);

        let mut verifier = setup_test(4, &finalized_number);

        check_session_verifier(&mut verifier, 0);
        check_session_verifier(&mut verifier, 1);

        finalize_first_in_session(&finalized_number, 1);
        check_session_verifier(&mut verifier, 2);

        finalize_first_in_session(&finalized_number, 2);
        check_session_verifier(&mut verifier, 3);

        // Should no longer have verifier for session 0
        assert_eq!(
            session_verifier(&mut verifier, 0),
            Err(CacheError::SessionTooOld(SessionId(0), SessionId(1)))
        );

        finalize_first_in_session(&finalized_number, 3);
        check_session_verifier(&mut verifier, 4);

        // Should no longer have verifier for session 1
        assert_eq!(
            session_verifier(&mut verifier, 1),
            Err(CacheError::SessionTooOld(SessionId(1), SessionId(2)))
        );
    }

    #[test]
    fn session_from_future() {
        let finalized_number = Cell::new(0);

        let mut verifier = setup_test(3, &finalized_number);

        finalize_first_in_session(&finalized_number, 1);

        // Did not finalize first block in session 2 yet
        assert_eq!(
            session_verifier(&mut verifier, 3),
            Err(CacheError::SessionInFuture(SessionId(3), SessionId(2)))
        );
    }

    #[test]
    fn authority_provider_error() {
        let finalized_number = Cell::new(0);
        let mut verifier = setup_test(0, &finalized_number);

        assert_eq!(
            session_verifier(&mut verifier, 1),
            Err(CacheError::UnknownAuthorities(SessionId(1)))
        );

        finalize_first_in_session(&finalized_number, 1);

        assert_eq!(
            session_verifier(&mut verifier, 2),
            Err(CacheError::UnknownAuthorities(SessionId(2)))
        );
    }
}
