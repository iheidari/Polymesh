//! # Pips Module
//!
//! MESH Improvement Proposals (PIPs) are proposals (ballots) that can then be proposed and voted on
//! by all MESH token holders. If a ballot passes this community vote it is then passed to the
//! governance council to ratify (or reject).
//! - minimum of 5,000 MESH needs to be staked by the proposer of the ballot
//! in order to create a new ballot.
//! - minimum of 100,000 MESH (quorum) needs to vote in favour of the ballot in order for the
//! ballot to be considered by the governing committee.
//! - ballots run for 1 week
//! - a simple majority is needed to pass the ballot so that it heads for the
//! next stage (governing committee)
//!
//! ## Overview
//!
//! The Pips module provides functions for:
//!
//! - Creating Mesh Improvement Proposals
//! - Voting on Mesh Improvement Proposals
//! - Governance committee to ratify or reject proposals
//!
//! ## Interface
//!
//! ### Dispatchable Functions
//!
//! - `set_min_proposal_deposit` change min deposit to create a proposal
//! - `set_quorum_threshold` change stake required to make a proposal into a referendum
//! - `set_proposal_duration` change duration in blocks for which proposal stays active
//! - `propose` - Token holders can propose a new ballot.
//! - `vote` - Token holders can vote on a ballot.
//! - `kill_proposal` - close a proposal and refund all deposits
//! - `enact_referendum` committee calls to execute a referendum
//!
//! ### Public Functions
//!
//! - `end_block` - Returns details of the token
#![cfg_attr(not(feature = "std"), no_std)]

use pallet_pips_rpc_runtime_api::VoteCount;
use polymesh_primitives::{AccountKey, Beneficiary, Signatory};
use polymesh_runtime_common::{
    constants::PIP_MAX_REPORTING_SIZE,
    identity::Trait as IdentityTrait,
    protocol_fee::{ChargeProtocolFee, ProtocolOp},
    traits::{governance_group::GovernanceGroupTrait, group::GroupTrait},
    CommonTrait, Context,
};
use polymesh_runtime_identity as identity;
use polymesh_runtime_treasury::TreasuryTrait;

// use shrinkwrap::Shrinkwrap;
use codec::{Decode, Encode};
use frame_support::{
    debug, decl_error, decl_event, decl_module, decl_storage,
    dispatch::DispatchResult,
    ensure,
    traits::{Currency, LockableCurrency, ReservableCurrency},
    weights::SimpleDispatchInfo,
    Parameter,
};
use frame_system::{self as system, ensure_signed};
use sp_core::H256;
use sp_runtime::traits::{
    BlakeTwo256, CheckedAdd, CheckedSub, Dispatchable, EnsureOrigin, Hash, Saturating, Zero,
};
use sp_std::{convert::TryFrom, prelude::*};

/// Mesh Improvement Proposal id. Used offchain.
pub type PipId = u32;

/// Balance
type BalanceOf<T> =
    <<T as Trait>::Currency as Currency<<T as frame_system::Trait>::AccountId>>::Balance;

/// A wrapper for a proposal url.
#[derive(Decode, Encode, Clone, Debug, Default, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct Url(pub Vec<u8>);

impl<T: AsRef<[u8]>> From<T> for Url {
    fn from(s: T) -> Self {
        let s = s.as_ref();
        let mut v = Vec::with_capacity(s.len());
        v.extend_from_slice(s);
        Url(v)
    }
}

/// A wrapper for a proposal description.
#[derive(Decode, Encode, Clone, Debug, Default, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct PipDescription(pub Vec<u8>);

impl<T: AsRef<[u8]>> From<T> for PipDescription {
    fn from(s: T) -> Self {
        let s = s.as_ref();
        let mut v = Vec::with_capacity(s.len());
        v.extend_from_slice(s);
        PipDescription(v)
    }
}

/// Represents a proposal
#[derive(Encode, Decode, Clone, PartialEq, Eq)]
pub struct Pip<Proposal> {
    /// The proposal's unique id.
    id: PipId,
    /// The proposal being voted on.
    proposal: Proposal,
    /// The latest state
    state: ProposalState,
}

/// Either the entire proposal encoded as a byte vector or its hash. The latter represents large
/// proposals.
#[derive(Encode, Decode, Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProposalData {
    /// The hash of the proposal.
    Hash(H256),
    /// The entire proposal.
    Proposal(Vec<u8>),
}

/// Represents a proposal metadata
#[derive(Encode, Decode, Clone, PartialEq, Eq, Debug)]
pub struct PipsMetadata<T: Trait> {
    /// The creator
    pub proposer: T::AccountId,
    /// The proposal's unique id.
    pub id: PipId,
    /// When voting will end.
    pub end: T::BlockNumber,
    /// The proposal url for proposal discussion.
    pub url: Option<Url>,
    /// The proposal description.
    pub description: Option<PipDescription>,
    /// This proposal allows any changes
    /// During Cool-off period, proposal owner can amend any PIP detail or cancel the entire
    pub cool_off_until: T::BlockNumber,
    /// Beneficiaries of this Pips
    pub beneficiaries: Vec<Beneficiary<T::Balance>>,
}

/// For keeping track of proposal being voted on.
#[derive(PartialEq, Eq, Clone, Encode, Decode, Default)]
#[cfg_attr(feature = "std", derive(Debug))]
pub struct VotingResult<Balance: Parameter> {
    /// The current set of voters that approved with their stake.
    pub ayes_count: u32,
    pub ayes_stake: Balance,
    /// The current set of voters that rejected with their stake.
    pub nays_count: u32,
    pub nays_stake: Balance,
}

#[derive(PartialEq, Eq, Clone, Encode, Decode)]
#[cfg_attr(feature = "std", derive(Debug))]
pub enum Vote<Balance> {
    None,
    Yes(Balance),
    No(Balance),
}

impl<Balance> Default for Vote<Balance> {
    fn default() -> Self {
        Vote::None
    }
}

#[derive(Encode, Decode, Copy, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "std", derive(Debug))]
pub enum ProposalState {
    /// Proposal is created and either in the cool-down period or open to voting
    Pending,
    /// Proposal is cancelled by its owner
    Cancelled,
    /// Proposal was killed by the GC
    Killed,
    /// Proposal failed to pass by a community vote
    Rejected,
    /// Proposal has moved to referendum stage
    Referendum,
}

impl Default for ProposalState {
    fn default() -> Self {
        ProposalState::Pending
    }
}

#[derive(Encode, Decode, Copy, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "std", derive(Debug))]
pub enum ReferendumState {
    /// Pending GC ratification
    Pending,
    /// Execution of this PIP is scheduled, i.e. it needs to wait its enactment period.
    Scheduled,
    /// Rejected by the GC
    Rejected,
    /// It has been executed, but execution failed.
    Failed,
    /// It has been successfully executed.
    Executed,
}

#[derive(Encode, Decode, Copy, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "std", derive(Debug))]
pub enum ReferendumType {
    /// Referendum pushed by GC (fast-tracked)
    FastTracked,
    /// Referendum created by GC
    Emergency,
    /// Created through a community vote
    Community,
}

/// Properties of a referendum
#[derive(Encode, Decode, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "std", derive(Debug))]
pub struct Referendum<T: Trait> {
    /// The proposal's unique id.
    pub id: PipId,
    /// Current state of this Referendum.
    pub state: ReferendumState,
    /// The type of the referendum
    pub referendum_type: ReferendumType,
    /// Enactment period.
    pub enactment_period: T::BlockNumber,
}

/// Information about deposit.
#[derive(Encode, Decode, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "std", derive(Debug))]
pub struct DepositInfo<AccountId, Balance>
where
    AccountId: Default,
    Balance: Default,
{
    /// Owner of the deposit.
    pub owner: AccountId,
    /// Amount. It can be updated during the cool off period.
    pub amount: Balance,
}

type Identity<T> = identity::Module<T>;

/// The module's configuration trait.
pub trait Trait:
    frame_system::Trait + pallet_timestamp::Trait + IdentityTrait + CommonTrait
{
    /// Currency type for this module.
    type Currency: ReservableCurrency<Self::AccountId>
        + LockableCurrency<Self::AccountId, Moment = Self::BlockNumber>;

    /// Origin for proposals.
    type CommitteeOrigin: EnsureOrigin<Self::Origin>;

    /// Origin for enacting a referundum.
    type VotingMajorityOrigin: EnsureOrigin<Self::Origin>;

    /// Committee
    type GovernanceCommittee: GovernanceGroupTrait<<Self as pallet_timestamp::Trait>::Moment>;

    type Treasury: TreasuryTrait<<Self as CommonTrait>::Balance>;

    /// The overarching event type.
    type Event: From<Event<Self>> + Into<<Self as system::Trait>::Event>;
}

// This module's storage items.
decl_storage! {
    trait Store for Module<T: Trait> as Pips {
        /// Determines whether historical PIP data is persisted or removed
        pub PruneHistoricalPips get(fn prune_historical_pips) config(): bool;

        /// The minimum amount to be used as a deposit for a public referendum proposal.
        pub MinimumProposalDeposit get(fn min_proposal_deposit) config(): BalanceOf<T>;

        /// Minimum stake a proposal must gather in order to be considered by the committee.
        pub QuorumThreshold get(fn quorum_threshold) config(): BalanceOf<T>;

        /// During Cool-off period, proposal owner can amend any PIP detail or cancel the entire
        /// proposal.
        pub ProposalCoolOffPeriod get(fn proposal_cool_off_period) config(): T::BlockNumber;

        /// How long (in blocks) a ballot runs
        pub ProposalDuration get(fn proposal_duration) config(): T::BlockNumber;

        /// Proposals so far. id can be used to keep track of PIPs off-chain.
        PipIdSequence: u32;

        /// The metadata of the active proposals.
        pub ProposalMetadata get(fn proposal_metadata): map hasher(twox_64_concat) PipId => Option<PipsMetadata<T>>;
        /// It maps the block number where a list of proposal are considered as matured.
        pub ProposalsMaturingAt get(proposals_maturing_at): map hasher(twox_64_concat) T::BlockNumber => Vec<PipId>;

        /// Those who have locked a deposit.
        /// proposal (id, proposer) -> deposit
        pub Deposits get(fn deposits): double_map hasher(twox_64_concat) PipId, hasher(twox_64_concat) T::AccountId => DepositInfo<T::AccountId, BalanceOf<T>>;

        /// Actual proposal for a given id, if it's current.
        /// proposal id -> proposal
        pub Proposals get(fn proposals): map hasher(twox_64_concat) PipId => Option<Pip<T::Proposal>>;

        /// PolymeshVotes on a given proposal, if it is ongoing.
        /// proposal id -> vote count
        pub ProposalResult get(fn proposal_result): map hasher(twox_64_concat) PipId => VotingResult<BalanceOf<T>>;

        /// Votes per Proposal and account. Used to avoid double vote issue.
        /// (proposal id, account) -> Vote
        pub ProposalVotes get(fn proposal_vote): double_map hasher(twox_64_concat) PipId, hasher(twox_64_concat) T::AccountId => Vote<BalanceOf<T>>;

        /// Proposals that have met the quorum threshold to be put forward to a governance committee
        /// proposal id -> proposal
        pub Referendums get(fn referendums): map hasher(twox_64_concat) PipId => Option<Referendum<T>>;

        /// List of id's of current scheduled referendums.
        /// block number -> Pip id
        pub ScheduledReferendumsAt get(fn scheduled_referendums_at): map hasher(twox_64_concat) T::BlockNumber => Vec<PipId>;

        /// Default enactment period that will be use after a proposal is accepted by GC.
        pub DefaultEnactmentPeriod get(fn default_enactment_period) config(): T::BlockNumber;
    }
}

decl_event!(
    pub enum Event<T>
    where
        Balance = BalanceOf<T>,
        <T as frame_system::Trait>::AccountId,
        <T as frame_system::Trait>::BlockNumber,
    {
        /// Pruning Historical PIPs is enabled or disabled (old value, new value)
        PruningHistoricalPips(bool, bool),
        /// A Mesh Improvement Proposal was made with a `Balance` stake
        ProposalCreated(
            AccountId,
            PipId,
            Balance,
            Option<Url>,
            Option<PipDescription>,
            BlockNumber,
            BlockNumber,
            ProposalData,
        ),
        /// A Mesh Improvement Proposal was amended with a possible change to the bond
        /// bool is +ve when bond is added, -ve when removed
        ProposalAmended(AccountId, PipId, bool, Balance),
        /// Triggered each time the state of a proposal is amended
        ProposalStateUpdated(PipId, ProposalState),
        /// `AccountId` voted `bool` on the proposal referenced by `PipId`
        Voted(AccountId, PipId, bool, Balance),
        /// Pip has been closed, bool indicates whether data is pruned
        PipClosed(PipId, bool),
        /// Referendum created for proposal.
        ReferendumCreated(PipId, ReferendumType),
        /// Referendum execution has been scheduled at specific block.
        ReferendumScheduled(PipId, BlockNumber, BlockNumber),
        /// Triggered each time the state of a referendum is amended
        ReferendumStateUpdated(PipId, ReferendumState),
        /// Default enactment period (in blocks) has been changed.
        /// (old period, new period)
        DefaultEnactmentPeriodChanged(BlockNumber, BlockNumber),
        /// Minimum deposit amount modified
        /// (old amount, new amount)
        MinimumProposalDepositChanged(Balance, Balance),
        /// Quorum threshold changed
        /// (old value, new value)
        QuorumThresholdChanged(Balance, Balance),
        /// Proposal duration changed
        /// (old value, new value)
        ProposalDurationChanged(BlockNumber, BlockNumber),
        /// Refund proposal
        /// (id, total amount)
        ProposalRefund(PipId, Balance),
        /// Proposal has beneficiaries.
        /// (id, total amount)
        ProposalPayment(PipId, Balance),
    }
);

decl_error! {
    pub enum Error for Module<T: Trait> {
        /// Incorrect origin
        BadOrigin,
        /// Proposer can't afford to lock minimum deposit
        InsufficientDeposit,
        /// when voter vote gain
        DuplicateVote,
        /// Duplicate proposal.
        DuplicateProposal,
        /// The proposal does not exist.
        NoSuchProposal,
        /// Mismatched proposal id.
        MismatchedProposalId,
        /// Not part of governance committee.
        NotACommitteeMember,
        /// After Cool-off period, proposals are not cancelable.
        ProposalOnCoolOffPeriod,
        /// Proposal is immutable after cool-off period.
        ProposalIsImmutable,
        /// Referendum is still on its enactment period.
        ReferendumOnEnactmentPeriod,
        /// Referendum is immutable.
        ReferendumIsImmutable,
        /// When a block number is less than current block number.
        InvalidFutureBlockNumber,
        /// When number of votes overflows.
        NumberOfVotesExceeded,
        /// When stake amount of a vote overflows.
        StakeAmountOfVotesExceeded,
    }
}

// The module's dispatchable functions.
decl_module! {
    /// The module declaration.
    pub struct Module<T: Trait> for enum Call where origin: T::Origin {
        type Error = Error<T>;

        fn deposit_event() = default;

        /// Change whether completed PIPs are pruned. Can only be called by governance council
        ///
        /// # Arguments
        /// * `deposit` the new min deposit required to start a proposal
        #[weight = SimpleDispatchInfo::FixedOperational(100_000)]
        pub fn set_prune_historical_pips(origin, new_value: bool) {
            T::CommitteeOrigin::try_origin(origin).map_err(|_| Error::<T>::BadOrigin)?;
            Self::deposit_event(RawEvent::PruningHistoricalPips(Self::prune_historical_pips(), new_value));
            <PruneHistoricalPips>::put(new_value);
        }

        /// Change the minimum proposal deposit amount required to start a proposal. Only Governance
        /// committee is allowed to change this value.
        ///
        /// # Arguments
        /// * `deposit` the new min deposit required to start a proposal
        #[weight = SimpleDispatchInfo::FixedOperational(100_000)]
        pub fn set_min_proposal_deposit(origin, deposit: BalanceOf<T>) {
            T::CommitteeOrigin::try_origin(origin).map_err(|_| Error::<T>::BadOrigin)?;
            Self::deposit_event(RawEvent::MinimumProposalDepositChanged(Self::min_proposal_deposit(), deposit));
            <MinimumProposalDeposit<T>>::put(deposit);
        }

        /// Change the quorum threshold amount. This is the amount which a proposal must gather so
        /// as to be considered by a committee. Only Governance committee is allowed to change
        /// this value.
        ///
        /// # Arguments
        /// * `threshold` the new quorum threshold amount value
        #[weight = SimpleDispatchInfo::FixedOperational(100_000)]
        pub fn set_quorum_threshold(origin, threshold: BalanceOf<T>) {
            T::CommitteeOrigin::try_origin(origin).map_err(|_| Error::<T>::BadOrigin)?;
            Self::deposit_event(RawEvent::MinimumProposalDepositChanged(Self::quorum_threshold(), threshold));
            <QuorumThreshold<T>>::put(threshold);
        }

        /// Change the proposal duration value. This is the number of blocks for which votes are
        /// accepted on a proposal. Only Governance committee is allowed to change this value.
        ///
        /// # Arguments
        /// * `duration` proposal duration in blocks
        #[weight = SimpleDispatchInfo::FixedOperational(100_000)]
        pub fn set_proposal_duration(origin, duration: T::BlockNumber) {
            T::CommitteeOrigin::try_origin(origin).map_err(|_| Error::<T>::BadOrigin)?;
            Self::deposit_event(RawEvent::ProposalDurationChanged(Self::proposal_duration(), duration));
            <ProposalDuration<T>>::put(duration);
        }

        /// Change the default enact period.
        #[weight = SimpleDispatchInfo::FixedOperational(100_000)]
        pub fn set_default_enact_period(origin, duration: T::BlockNumber) {
            T::CommitteeOrigin::try_origin(origin).map_err(|_| Error::<T>::BadOrigin)?;
            let previous_duration = <DefaultEnactmentPeriod<T>>::get();
            <DefaultEnactmentPeriod<T>>::put(duration);
            Self::deposit_event(RawEvent::DefaultEnactmentPeriodChanged(duration, previous_duration));
        }

        /// A network member creates a Mesh Improvement Proposal by submitting a dispatchable which
        /// changes the network in someway. A minimum deposit is required to open a new proposal.
        ///
        /// # Arguments
        /// * `proposal` a dispatchable call
        /// * `deposit` minimum deposit value
        /// * `url` a link to a website for proposal discussion
        #[weight = SimpleDispatchInfo::FixedNormal(5_000_000)]
        pub fn propose(
            origin,
            proposal: Box<T::Proposal>,
            deposit: BalanceOf<T>,
            url: Option<Url>,
            description: Option<PipDescription>,
            beneficiaries: Vec<Beneficiary<T::Balance>>
        ) -> DispatchResult {
            let proposer = ensure_signed(origin)?;
            let proposer_key = AccountKey::try_from(proposer.encode())?;
            let signer = Signatory::from(proposer_key);

            // Pre conditions: caller must have min balance
            ensure!(
                deposit >= Self::min_proposal_deposit(),
                Error::<T>::InsufficientDeposit
            );

            // Reserve the minimum deposit
            <T as Trait>::Currency::reserve(&proposer, deposit).map_err(|_| Error::<T>::InsufficientDeposit)?;
            <T as IdentityTrait>::ProtocolFee::charge_fee(
                &signer,
                ProtocolOp::PipsPropose
            )?;

            let id = Self::next_pip_id();
            let curr_block_number = <system::Module<T>>::block_number();
            let cool_off_until = curr_block_number + Self::proposal_cool_off_period();
            let end = cool_off_until + Self::proposal_duration();
            let proposal_metadata = PipsMetadata {
                proposer: proposer.clone(),
                id,
                end: end.clone(),
                url: url.clone(),
                description: description.clone(),
                cool_off_until: cool_off_until.clone(),
                beneficiaries,
            };
            let _ = <ProposalsMaturingAt<T>>::append(end, [id].iter())?;
            <ProposalMetadata<T>>::insert(id, proposal_metadata);

            let deposit_info = DepositInfo {
                owner: proposer.clone(),
                amount: deposit
            };
            <Deposits<T>>::insert(id, &proposer, deposit_info);

            let pip = Pip {
                id,
                proposal: *proposal.clone(),
                state: ProposalState::Pending,
            };
            <Proposals<T>>::insert(id, pip);

            // Add vote and update voting counter.
            // INTERNAL: It is impossible to overflow counters in the first vote.
            Self::unsafe_vote( id, proposer.clone(), Vote::Yes(deposit))
                .map_err(|vote_error| {
                    debug::error!("The counters of voting (id={}) have an overflow during the 1st vote", id);
                    vote_error
                })?;
            Self::deposit_event(RawEvent::ProposalCreated(
                proposer,
                id,
                deposit,
                url,
                description,
                cool_off_until,
                end,
                Self::reportable_proposal_data(*proposal),
            ));
            Ok(())
        }

        /// It amends the `url` and the `description` of the proposal with id `id`.
        ///
        /// # Errors
        /// * `BadOrigin`: Only the owner of the proposal can amend it.
        /// * `ProposalIsImmutable`: A proposals is mutable only during its cool off period.
        ///
        #[weight = SimpleDispatchInfo::FixedNormal(1_000_000)]
        pub fn amend_proposal(
                origin,
                id: PipId,
                url: Option<Url>,
                description: Option<PipDescription>
                ) -> DispatchResult {
            // 0. Initial info.
            let proposer = ensure_signed(origin)?;
            let meta = Self::proposal_metadata(id)
                .ok_or_else(|| Error::<T>::MismatchedProposalId)?;

            // 1. Only owner can cancel it.
            ensure!( meta.proposer == proposer, Error::<T>::BadOrigin);
            // Check that the proposal is pending
            Self::is_proposal_pending(id)?;

            // 2. Proposal can be cancelled *ONLY* during its cool-off period.
            let curr_block_number = <system::Module<T>>::block_number();
            ensure!( meta.cool_off_until > curr_block_number, Error::<T>::ProposalIsImmutable);

            // 3. Update proposal metadata.
            <ProposalMetadata<T>>::mutate( id, |meta| {
                if let Some(meta) = meta {
                    meta.url = url;
                    meta.description = description;
                }
            });
            Self::deposit_event(RawEvent::ProposalAmended(proposer, id, true, Zero::zero()));

            Ok(())
        }

        /// It cancels the proposal of the id `id`.
        ///
        /// Proposals can be cancelled only during its _cool-off period.
        ///
        /// # Errors
        /// * `BadOrigin`: Only the owner of the proposal can amend it.
        /// * `ProposalIsImmutable`: A Proposal is mutable only during its cool off period.
        #[weight = SimpleDispatchInfo::FixedNormal(1_000_000)]
        pub fn cancel_proposal(origin, id: PipId) -> DispatchResult {
            // 0. Initial info.
            let proposer = ensure_signed(origin)?;
            // 1. Only owner can cancel it.
            let meta = Self::proposal_metadata(id)
                .ok_or_else(|| Error::<T>::MismatchedProposalId)?;
            ensure!( meta.proposer == proposer, Error::<T>::BadOrigin);
            // Check that the proposal is pending
            Self::is_proposal_pending(id)?;
            // 2. Proposal can be cancelled *ONLY* during its cool-off period.
            let curr_block_number = <system::Module<T>>::block_number();
            ensure!( meta.cool_off_until > curr_block_number, Error::<T>::ProposalIsImmutable);

            // 3. Refund the bond for the proposal
            Self::refund_proposal(id);

            // 4. Close that proposal.
            Self::update_proposal_state(id, ProposalState::Cancelled);
            Self::prune_data(id, Self::prune_historical_pips());

            Ok(())
        }

        /// Id bonds an additional deposit to proposal with id `id`.
        /// That amount is added to the current deposit.
        ///
        /// # Errors
        /// * `BadOrigin`: Only the owner of the proposal can bond an additional deposit.
        /// * `ProposalIsImmutable`: A Proposal is mutable only during its cool off period.
        #[weight = SimpleDispatchInfo::FixedNormal(200_000)]
        pub fn bond_additional_deposit(origin,
            id: PipId,
            additional_deposit: BalanceOf<T>
        ) -> DispatchResult {
            let proposer = ensure_signed(origin)?;
            let meta = Self::proposal_metadata(id)
                .ok_or_else(|| Error::<T>::MismatchedProposalId)?;

            // 1. Only owner can add additional deposit.
            ensure!( meta.proposer == proposer, Error::<T>::BadOrigin);
            // Check that the proposal is pending
            Self::is_proposal_pending(id)?;

            // 2. Proposal can be amended *ONLY* during its cool-off period.
            let curr_block_number = <system::Module<T>>::block_number();
            ensure!( meta.cool_off_until > curr_block_number, Error::<T>::ProposalIsImmutable);

            // 3. Reserve extra deposit & update deposit info for this proposal
            let curr_deposit = Self::deposits(id, &proposer).amount;
            let max_additional_deposit = curr_deposit.saturating_add( additional_deposit) - curr_deposit;
            <T as Trait>::Currency::reserve(&proposer, max_additional_deposit)
                .map_err(|_| Error::<T>::InsufficientDeposit)?;

            <Deposits<T>>::mutate(
                id,
                &proposer,
                |depo_info| depo_info.amount += max_additional_deposit);

            // 4. Update vote details to record additional vote
            <ProposalResult<T>>::mutate(
                id,
                |stats| stats.ayes_stake += max_additional_deposit
            );
            <ProposalVotes<T>>::insert(id, &proposer, Vote::Yes(curr_deposit + max_additional_deposit));

            Self::deposit_event(RawEvent::ProposalAmended(proposer, id, true, max_additional_deposit));

            Ok(())
        }

        /// It unbonds any amount from the deposit of the proposal with id `id`.
        ///
        /// # Errors
        /// * `BadOrigin`: Only the owner of the proposal can release part of the deposit.
        /// * `ProposalIsImmutable`: A Proposal is mutable only during its cool off period.
        /// * `InsufficientDeposit`: If the final deposit will be less that the minimum deposit for
        /// a proposal.
        #[weight = SimpleDispatchInfo::FixedNormal(200_000)]
        pub fn unbond_deposit(origin,
            id: PipId,
            amount: BalanceOf<T>
        ) -> DispatchResult {
            let proposer = ensure_signed(origin)?;
            let meta = Self::proposal_metadata(id)
                .ok_or_else(|| Error::<T>::MismatchedProposalId)?;

            // 1. Only owner can cancel it.
            ensure!( meta.proposer == proposer, Error::<T>::BadOrigin);
            // Check that the proposal is pending
            Self::is_proposal_pending(id)?;

            // 2. Proposal can be cancelled *ONLY* during its cool-off period.
            let curr_block_number = <system::Module<T>>::block_number();
            ensure!( meta.cool_off_until > curr_block_number, Error::<T>::ProposalIsImmutable);

            // 3. Double-check that `amount` is valid.
            let mut depo_info = Self::deposits(id, &proposer);
            let new_deposit = depo_info.amount.checked_sub(&amount)
                    .ok_or_else(|| Error::<T>::InsufficientDeposit)?;
            ensure!(
                new_deposit >= Self::min_proposal_deposit(),
                Error::<T>::InsufficientDeposit);
            let diff_amount = depo_info.amount - new_deposit;
            depo_info.amount = new_deposit;

            // 3.1. Unreserve and update deposit info.
            <T as Trait>::Currency::unreserve(&depo_info.owner, diff_amount);
            <Deposits<T>>::insert(id, &proposer, depo_info);

            // 4. Update vote details to record reduced vote
            <ProposalResult<T>>::mutate(
                id,
                |stats| stats.ayes_stake = new_deposit
            );
            <ProposalVotes<T>>::insert(id, &proposer, Vote::Yes(new_deposit));


            Self::deposit_event(RawEvent::ProposalAmended(proposer, id, false, amount));
            Ok(())
        }

        /// A network member can vote on any Mesh Improvement Proposal by selecting the id that
        /// corresponds ot the dispatchable action and vote with some balance.
        ///
        /// # Arguments
        /// * `proposal` a dispatchable call
        /// * `id` proposal id
        /// * `aye_or_nay` a bool representing for or against vote
        /// * `deposit` minimum deposit value
        #[weight = SimpleDispatchInfo::FixedNormal(200_000)]
        pub fn vote(origin, id: PipId, aye_or_nay: bool, deposit: BalanceOf<T>) {
            let proposer = ensure_signed(origin)?;
            let meta = Self::proposal_metadata(id)
                .ok_or_else(|| Error::<T>::MismatchedProposalId)?;

            // No one should be able to vote during the proposal cool-off period.
            let curr_block_number = <system::Module<T>>::block_number();
            ensure!( meta.cool_off_until <= curr_block_number, Error::<T>::ProposalOnCoolOffPeriod);

            // Check that the proposal is pending
            Self::is_proposal_pending(id)?;

            // Valid PipId
            ensure!(<ProposalResult<T>>::contains_key(id), Error::<T>::NoSuchProposal);

            // Double-check vote duplication.
            ensure!( Self::proposal_vote(id, &proposer) == Vote::None, Error::<T>::DuplicateVote);

            // Reserve the deposit
            <T as Trait>::Currency::reserve(&proposer, deposit).map_err(|_| Error::<T>::InsufficientDeposit)?;

            // Save your vote.
            let vote = if aye_or_nay {
                Vote::Yes(deposit)
            } else {
                Vote::No(deposit)
            };
            Self::unsafe_vote( id, proposer.clone(), vote)
                .map_err( |vote_error| {
                    debug::warn!("The counters of voting (id={}) have an overflow, transaction is roll-back", id);
                    let _ = <T as Trait>::Currency::unreserve(&proposer, deposit);
                    vote_error
                })?;

            let depo_info = DepositInfo {
                owner: proposer.clone(),
                amount: deposit,
            };
            <Deposits<T>>::insert(id, &proposer, depo_info);

            Self::deposit_event(RawEvent::Voted(proposer, id, aye_or_nay, deposit));
        }

        /// An emergency stop measure to kill a proposal. Governance committee can kill
        /// a proposal at any time.
        #[weight = SimpleDispatchInfo::FixedOperational(100_000)]
        pub fn kill_proposal(origin, id: PipId) {
            T::CommitteeOrigin::try_origin(origin).map_err(|_| Error::<T>::BadOrigin)?;
            ensure!(<Proposals<T>>::contains_key(id), Error::<T>::NoSuchProposal);
            // Check that the proposal is pending
            Self::is_proposal_pending(id)?;
            Self::refund_proposal(id);
            Self::update_proposal_state(id, ProposalState::Killed);
            Self::prune_data(id, Self::prune_historical_pips());
        }

        /// Any governance committee member can fast track a proposal and turn it into a referendum
        /// that will be voted on by the committee.
        #[weight = SimpleDispatchInfo::FixedOperational(200_000)]
        pub fn fast_track_proposal(origin, id: PipId) -> DispatchResult {
            let sender_key = AccountKey::try_from(ensure_signed(origin)?.encode())?;
            let did = Context::current_identity_or::<Identity<T>>(&sender_key)?;

            ensure!(
                T::GovernanceCommittee::is_member(&did),
                Error::<T>::NotACommitteeMember
            );

            ensure!(<Proposals<T>>::contains_key(id), Error::<T>::MismatchedProposalId);
            // Check that the proposal is pending
            Self::is_proposal_pending(id)?;
            Self::create_referendum(
                id,
                ReferendumState::Pending,
                ReferendumType::FastTracked,
            );
            Self::refund_proposal(id);

            Ok(())
        }

        /// Governance committee can make a proposal that automatically becomes a referendum on
        /// which the committee can vote on.
        #[weight = SimpleDispatchInfo::FixedOperational(200_000)]
        pub fn emergency_referendum(
            origin,
            proposal: Box<T::Proposal>,
            url: Option<Url>,
            description: Option<PipDescription>,
            beneficiaries: Vec<Beneficiary<T::Balance>>
        ) -> DispatchResult {
            let proposer = ensure_signed(origin)?;
            let proposer_key = AccountKey::try_from(proposer.encode())?;
            let did = Context::current_identity_or::<Identity<T>>(&proposer_key)?;

            ensure!(
                T::GovernanceCommittee::is_member(&did),
                Error::<T>::NotACommitteeMember
            );

            let id = Self::next_pip_id();
            let pip = Pip {
                id,
                proposal: *proposal.clone(),
                state: ProposalState::Pending,
            };
            <Proposals<T>>::insert(id, pip);

            let proposal_metadata = PipsMetadata {
                proposer: proposer.clone(),
                id,
                end: Zero::zero(),
                url: url.clone(),
                description: description.clone(),
                cool_off_until: Zero::zero(),
                beneficiaries
            };
            <ProposalMetadata<T>>::insert(id, proposal_metadata);
            Self::deposit_event(RawEvent::ProposalCreated(
                proposer,
                id,
                Zero::zero(),
                url,
                description,
                Zero::zero(),
                Zero::zero(),
                Self::reportable_proposal_data(*proposal),
            ));
            Self::create_referendum(
                id,
                ReferendumState::Pending,
                ReferendumType::Emergency,
            );
            Ok(())
        }

        /// Moves a referendum instance into dispatch queue.
        #[weight = SimpleDispatchInfo::FixedOperational(100_000)]
        pub fn enact_referendum(origin, id: PipId) -> DispatchResult {
            T::VotingMajorityOrigin::try_origin(origin).map_err(|_| Error::<T>::BadOrigin)?;
            // Check that referendum is Pending
            Self::is_referendum_pending(id)?;
            Self::prepare_to_dispatch(id)
        }

        /// Moves a referendum instance into rejected state.
        #[weight = SimpleDispatchInfo::FixedOperational(100_000)]
        pub fn reject_referendum(origin, id: PipId) -> DispatchResult {
            T::VotingMajorityOrigin::try_origin(origin).map_err(|_| Error::<T>::BadOrigin)?;
            // Check that referendum is Pending
            Self::is_referendum_pending(id)?;

            // Close proposal
            Self::update_referendum_state(id, ReferendumState::Rejected);
            Self::prune_data(id, Self::prune_historical_pips());
            Ok(())
        }

        /// It updates the enactment period of a specific referendum.
        ///
        /// # Arguments
        /// * `until`, It defines the future block where the enactment period will finished.  A
        /// `None` value means that enactment period is going to finish in the next block.
        ///
        /// # Errors
        /// * `BadOrigin`, Only the release coordinator can update the enactment period.
        /// * ``,
        #[weight = SimpleDispatchInfo::FixedOperational(100_000)]
        pub fn set_referendum_enactment_period(origin, mid: PipId, until: Option<T::BlockNumber>) -> DispatchResult {
            let sender_key = AccountKey::try_from(ensure_signed(origin)?.encode())?;
            let id = Context::current_identity_or::<Identity<T>>(&sender_key)?;

            // 1. Only release coordinator
            ensure!(
                Some(id) == T::GovernanceCommittee::release_coordinator(),
                Error::<T>::BadOrigin);

            // 2. New value should be valid block number.
            let next_block = <system::Module<T>>::block_number() + 1.into();
            let new_until = until.unwrap_or(next_block);
            ensure!( new_until >= next_block, Error::<T>::InvalidFutureBlockNumber);

            // 2. Valid referendum: check mid & state == Scheduled
            let referendum = Self::referendums(mid)
                .ok_or_else(|| Error::<T>::MismatchedProposalId)?;
            ensure!( referendum.state == ReferendumState::Scheduled, Error::<T>::ReferendumIsImmutable);

            // 3. Update enactment period.
            // 3.1 Update referendum.
            let old_until = referendum.enactment_period;

            <Referendums<T>>::mutate( mid, |referendum| {
                if let Some(ref mut referendum) = referendum {
                    referendum.enactment_period = new_until;
                }
            });

            // 3.1. Re-schedule it
            <ScheduledReferendumsAt<T>>::mutate( old_until, |ids| ids.retain( |i| *i != mid));
            <ScheduledReferendumsAt<T>>::mutate( new_until, |ids| ids.push(mid));

            Self::deposit_event(RawEvent::ReferendumScheduled(mid, old_until, new_until));
            Ok(())
        }

        /// When constructing a block check if it's time for a ballot to end. If ballot ends,
        /// proceed to ratification process.
        fn on_initialize(n: T::BlockNumber) {
            if let Err(e) = Self::end_block(n) {
                sp_runtime::print(e);
            }
        }

    }
}

impl<T: Trait> Module<T> {
    /// Runs the following procedure:
    /// 1. Find all proposals that need to end as of this block and close voting
    /// 2. Tally votes
    /// 3. Submit any proposals that meet the quorum threshold, to the governance committee
    /// 4. Automatically execute any referendum
    pub fn end_block(block_number: T::BlockNumber) -> DispatchResult {
        // Find all matured proposals...
        <ProposalsMaturingAt<T>>::take(block_number)
            .into_iter()
            .for_each(|id| {
                // It is possible the proposal has been killed, cancelled or fast tracked
                if let Some(proposal) = Self::proposals(id) {
                    if proposal.state == ProposalState::Pending {
                        // Tally votes and create referendums
                        let voting = Self::proposal_result(id);

                        // 1. Ayes staked must be more than nays staked (simple majority)
                        // 2. Ayes staked are more than the minimum quorum threshold
                        if voting.ayes_stake > voting.nays_stake
                            && voting.ayes_stake >= Self::quorum_threshold()
                        {
                            Self::refund_proposal(id);
                            Self::create_referendum(
                                id,
                                ReferendumState::Pending,
                                ReferendumType::Community,
                            );
                        } else {
                            Self::update_proposal_state(id, ProposalState::Rejected);
                            Self::refund_proposal(id);
                            Self::prune_data(id, Self::prune_historical_pips());
                        }
                    }
                }
            });
        <ProposalsMaturingAt<T>>::remove(block_number);
        // Execute automatically referendums after its enactment period.
        let referendum_ids = <ScheduledReferendumsAt<T>>::take(block_number);
        referendum_ids
            .into_iter()
            .for_each(|id| Self::execute_referendum(id));
        <ScheduledReferendumsAt<T>>::remove(block_number);
        Ok(())
    }

    /// Create a referendum object from a proposal. If governance committee is composed of less
    /// than 2 members, enact it immediately. Otherwise, committee votes on this referendum and
    /// decides whether it should be enacted.
    fn create_referendum(id: PipId, state: ReferendumState, referendum_type: ReferendumType) {
        let enactment_period: T::BlockNumber = 0.into();
        let referendum = Referendum {
            id,
            state,
            referendum_type,
            enactment_period,
        };
        <Referendums<T>>::insert(id, referendum);
        Self::update_proposal_state(id, ProposalState::Referendum);
        Self::deposit_event(RawEvent::ReferendumCreated(id, referendum_type));
    }

    /// Refunds any tokens used to vote or bond a proposal
    fn refund_proposal(id: PipId) {
        let total_refund = <Deposits<T>>::iter_prefix(id).fold(0.into(), |acc, depo_info| {
            let amount = <T as Trait>::Currency::unreserve(&depo_info.owner, depo_info.amount);
            amount.saturating_add(acc)
        });
        <Deposits<T>>::remove_prefix(id);

        Self::deposit_event(RawEvent::ProposalRefund(id, total_refund));
    }

    /// Close a proposal.
    ///
    /// Voting ceases and proposal is removed from storage.
    /// It also refunds all deposits.
    ///
    /// # Internal
    /// * `ProposalsMaturingat` does not need to be deleted here.
    ///
    /// # TODO
    /// * Should we remove the proposal when it is Cancelled?, killed?, rejected?
    fn prune_data(id: PipId, prune: bool) {
        if prune {
            <ProposalResult<T>>::remove(id);
            <ProposalVotes<T>>::remove_prefix(id);
            <ProposalMetadata<T>>::remove(id);
            <Proposals<T>>::remove(id);
            <Referendums<T>>::remove(id);
            Self::deposit_event(RawEvent::PipClosed(id, true));
        } else {
            Self::deposit_event(RawEvent::PipClosed(id, false));
        }
    }

    fn prepare_to_dispatch(id: PipId) -> DispatchResult {
        ensure!(
            <Referendums<T>>::contains_key(id),
            Error::<T>::MismatchedProposalId
        );

        // Set the default enactment period and move it to `Scheduled`
        let curr_block_number = <system::Module<T>>::block_number();
        let enactment_period = curr_block_number + Self::default_enactment_period();

        <Referendums<T>>::mutate(id, |referendum| {
            if let Some(ref mut referendum) = referendum {
                referendum.enactment_period = enactment_period;
                referendum.state = ReferendumState::Scheduled;
            }
        });
        <ScheduledReferendumsAt<T>>::mutate(enactment_period, |ids| ids.push(id));

        Self::deposit_event(RawEvent::ReferendumScheduled(
            id,
            Zero::zero(),
            enactment_period,
        ));
        Ok(())
    }

    fn execute_referendum(id: PipId) {
        if let Some(proposal) = Self::proposals(id) {
            if proposal.state == ProposalState::Referendum {
                match proposal.proposal.dispatch(system::RawOrigin::Root.into()) {
                    Ok(_) => {
                        Self::update_referendum_state(id, ReferendumState::Executed);
                        Self::pay_to_beneficiaries(id);
                    }
                    Err(e) => {
                        Self::update_referendum_state(id, ReferendumState::Failed);
                        debug::error!("Referendum {}, its execution fails: {:?}", id, e);
                    }
                };
                Self::prune_data(id, Self::prune_historical_pips());
            }
        }
    }

    fn pay_to_beneficiaries(id: PipId) {
        if let Some(meta) = Self::proposal_metadata(id) {
            let _total_amount = meta.beneficiaries.into_iter().fold(0.into(), |acc, b| {
                T::Treasury::disbursement(b.id, b.amount);
                b.amount.saturating_add(acc)
            });
            // Self::deposit_event(RawEvent::ProposalPayment(id, total_amount));
        }
    }

    fn update_proposal_state(id: PipId, new_state: ProposalState) {
        <Proposals<T>>::mutate(id, |proposal| {
            if let Some(ref mut proposal) = proposal {
                proposal.state = new_state;
            }
        });
        Self::deposit_event(RawEvent::ProposalStateUpdated(id, new_state));
    }

    fn update_referendum_state(id: PipId, new_state: ReferendumState) {
        <Referendums<T>>::mutate(id, |referendum| {
            if let Some(ref mut referendum) = referendum {
                referendum.state = new_state;
            }
        });
        Self::deposit_event(RawEvent::ReferendumStateUpdated(id, new_state));
    }

    fn is_proposal_pending(id: PipId) -> DispatchResult {
        let proposal = Self::proposals(id).ok_or_else(|| Error::<T>::MismatchedProposalId)?;
        ensure!(
            proposal.state == ProposalState::Pending,
            Error::<T>::ProposalIsImmutable
        );
        Ok(())
    }

    fn is_referendum_pending(id: PipId) -> DispatchResult {
        let referundum = Self::referendums(id).ok_or_else(|| Error::<T>::MismatchedProposalId)?;
        ensure!(
            referundum.state == ReferendumState::Pending,
            Error::<T>::ReferendumIsImmutable
        );
        Ok(())
    }
}

impl<T: Trait> Module<T> {
    /// Retrieve votes for a proposal represented by PipId `id`.
    pub fn get_votes(id: PipId) -> VoteCount<BalanceOf<T>>
    where
        T: Send + Sync,
        BalanceOf<T>: Send + Sync,
    {
        if !<ProposalResult<T>>::contains_key(id) {
            return VoteCount::ProposalNotFound;
        }

        let voting = Self::proposal_result(id);
        VoteCount::Success {
            ayes: voting.ayes_stake,
            nays: voting.nays_stake,
        }
    }

    /// Retrieve proposals made by `address`.
    pub fn proposed_by(address: T::AccountId) -> Vec<PipId> {
        <ProposalMetadata<T>>::iter()
            .filter(|meta| meta.proposer == address)
            .map(|meta| meta.id)
            .collect()
    }

    /// Retrieve proposals `address` voted on
    pub fn voted_on(address: T::AccountId) -> Vec<PipId> {
        <ProposalMetadata<T>>::iter()
            .filter_map(|meta| match Self::proposal_vote(meta.id, &address) {
                Vote::None => None,
                _ => Some(meta.id),
            })
            .collect::<Vec<_>>()
    }

    /// It generates the next id for proposals and referendums.
    fn next_pip_id() -> u32 {
        let id = <PipIdSequence>::get();
        <PipIdSequence>::put(id + 1);

        id
    }

    /// It inserts the vote and updates the accountability of target proposal.
    fn unsafe_vote(id: PipId, proposer: T::AccountId, vote: Vote<BalanceOf<T>>) -> DispatchResult {
        let mut stats = Self::proposal_result(id);
        match vote {
            Vote::Yes(deposit) => {
                stats.ayes_count = stats
                    .ayes_count
                    .checked_add(1)
                    .ok_or_else(|| Error::<T>::NumberOfVotesExceeded)?;
                stats.ayes_stake = stats
                    .ayes_stake
                    .checked_add(&deposit)
                    .ok_or_else(|| Error::<T>::StakeAmountOfVotesExceeded)?;
            }
            Vote::No(deposit) => {
                stats.nays_count += stats
                    .nays_count
                    .checked_add(1)
                    .ok_or_else(|| Error::<T>::NumberOfVotesExceeded)?;
                stats.nays_stake += stats
                    .nays_stake
                    .checked_add(&deposit)
                    .ok_or_else(|| Error::<T>::StakeAmountOfVotesExceeded)?;
            }
            Vote::None => {
                // It should be unreachable because public API only allows binary options.
                debug::warn!("Unexpected none vote");
            }
        };

        <ProposalResult<T>>::insert(id, stats);
        <ProposalVotes<T>>::insert(id, proposer, vote);
        Ok(())
    }

    /// Returns a reportable representation of a proposal taking care that the reported data are not
    /// too large.
    fn reportable_proposal_data(proposal: T::Proposal) -> ProposalData {
        let encoded_proposal = proposal.encode();
        let proposal_data = if encoded_proposal.len() > PIP_MAX_REPORTING_SIZE {
            ProposalData::Hash(BlakeTwo256::hash(encoded_proposal.as_slice()))
        } else {
            ProposalData::Proposal(encoded_proposal)
        };
        proposal_data
    }
}