// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Provides glue code over the scheduler and inclusion modules, and accepting
//! one inherent per block that can include new para candidates and bitfields.
//!
//! Unlike other modules in this crate, it does not need to be initialized by the initializer,
//! as it has no initialization logic and its finalization logic depends only on the details of
//! this module.

use crate::{
	configuration,
	disputes::DisputesHandler,
	inclusion,
	inclusion::CandidateCheckContext,
	initializer,
	metrics::METRICS,
	paras,
	scheduler::{self, FreedReason},
	shared::{self, AllowedRelayParentsTracker},
	ParaId,
};
use bitvec::prelude::BitVec;
use frame_support::{
	dispatch::{DispatchErrorWithPostInfo, PostDispatchInfo},
	inherent::{InherentData, InherentIdentifier, MakeFatalError, ProvideInherent},
	pallet_prelude::*,
	traits::Randomness,
};
use frame_system::pallet_prelude::*;
use pallet_babe::{self, ParentBlockRandomness};
use primitives::{
	effective_minimum_backing_votes, vstaging::node_features::FeatureIndex, BackedCandidate,
	CandidateHash, CandidateReceipt, CheckedDisputeStatementSet, CheckedMultiDisputeStatementSet,
	CoreIndex, DisputeStatementSet, InherentData as ParachainsInherentData,
	MultiDisputeStatementSet, ScrapedOnChainVotes, SessionIndex, SignedAvailabilityBitfields,
	SigningContext, UncheckedSignedAvailabilityBitfield, UncheckedSignedAvailabilityBitfields,
	ValidatorId, ValidatorIndex, ValidityAttestation, PARACHAINS_INHERENT_IDENTIFIER,
};
use rand::{seq::SliceRandom, SeedableRng};
use scale_info::TypeInfo;
use sp_runtime::traits::{Header as HeaderT, One};
use sp_std::{
	collections::{btree_map::BTreeMap, btree_set::BTreeSet},
	prelude::*,
	vec::Vec,
};

mod misc;
mod weights;

use self::weights::checked_multi_dispute_statement_sets_weight;
pub use self::{
	misc::{IndexedRetain, IsSortedBy},
	weights::{
		backed_candidate_weight, backed_candidates_weight, dispute_statement_set_weight,
		multi_dispute_statement_sets_weight, paras_inherent_total_weight, signed_bitfield_weight,
		signed_bitfields_weight, TestWeightInfo, WeightInfo,
	},
};

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;

#[cfg(test)]
mod tests;

const LOG_TARGET: &str = "runtime::inclusion-inherent";

/// A bitfield concerning concluded disputes for candidates
/// associated to the core index equivalent to the bit position.
#[derive(Default, PartialEq, Eq, Clone, Encode, Decode, RuntimeDebug, TypeInfo)]
pub(crate) struct DisputedBitfield(pub(crate) BitVec<u8, bitvec::order::Lsb0>);

impl From<BitVec<u8, bitvec::order::Lsb0>> for DisputedBitfield {
	fn from(inner: BitVec<u8, bitvec::order::Lsb0>) -> Self {
		Self(inner)
	}
}

#[cfg(test)]
impl DisputedBitfield {
	/// Create a new bitfield, where each bit is set to `false`.
	pub fn zeros(n: usize) -> Self {
		Self::from(BitVec::<u8, bitvec::order::Lsb0>::repeat(false, n))
	}
}

/// The context in which the inherent data is checked or processed.
#[derive(PartialEq)]
pub enum ProcessInherentDataContext {
	/// Enables filtering/limits weight of inherent up to maximum block weight.
	/// Invariant: InherentWeight <= BlockWeight.
	ProvideInherent,
	/// Checks the InherentWeight invariant.
	Enter,
}
pub use pallet::*;

#[frame_support::pallet]
pub mod pallet {
	use super::*;

	#[pallet::pallet]
	#[pallet::without_storage_info]
	pub struct Pallet<T>(_);

	#[pallet::config]
	#[pallet::disable_frame_system_supertrait_check]
	pub trait Config:
		inclusion::Config + scheduler::Config + initializer::Config + pallet_babe::Config
	{
		/// Weight information for extrinsics in this pallet.
		type WeightInfo: WeightInfo;
	}

	#[pallet::error]
	pub enum Error<T> {
		/// Inclusion inherent called more than once per block.
		TooManyInclusionInherents,
		/// The hash of the submitted parent header doesn't correspond to the saved block hash of
		/// the parent.
		InvalidParentHeader,
		/// Disputed candidate that was concluded invalid.
		CandidateConcludedInvalid,
		/// The data given to the inherent will result in an overweight block.
		InherentOverweight,
		/// The ordering of dispute statements was invalid.
		DisputeStatementsUnsortedOrDuplicates,
		/// A dispute statement was invalid.
		DisputeInvalid,
		/// A candidate was backed by a disabled validator
		BackedByDisabled,
		/// A candidate was backed even though the paraid was not scheduled.
		BackedOnUnscheduledCore,
		/// Too many candidates supplied.
		UnscheduledCandidate,
	}

	/// Whether the paras inherent was included within this block.
	///
	/// The `Option<()>` is effectively a `bool`, but it never hits storage in the `None` variant
	/// due to the guarantees of FRAME's storage APIs.
	///
	/// If this is `None` at the end of the block, we panic and render the block invalid.
	#[pallet::storage]
	pub(crate) type Included<T> = StorageValue<_, ()>;

	/// Scraped on chain data for extracting resolved disputes as well as backing votes.
	#[pallet::storage]
	#[pallet::getter(fn on_chain_votes)]
	pub(crate) type OnChainVotes<T: Config> = StorageValue<_, ScrapedOnChainVotes<T::Hash>>;

	/// Update the disputes statements set part of the on-chain votes.
	pub(crate) fn set_scrapable_on_chain_disputes<T: Config>(
		session: SessionIndex,
		checked_disputes: CheckedMultiDisputeStatementSet,
	) {
		crate::paras_inherent::OnChainVotes::<T>::mutate(move |value| {
			let disputes =
				checked_disputes.into_iter().map(DisputeStatementSet::from).collect::<Vec<_>>();
			let backing_validators_per_candidate = match value.take() {
				Some(v) => v.backing_validators_per_candidate,
				None => Vec::new(),
			};
			*value = Some(ScrapedOnChainVotes::<T::Hash> {
				backing_validators_per_candidate,
				disputes,
				session,
			});
		})
	}

	/// Update the backing votes including part of the on-chain votes.
	pub(crate) fn set_scrapable_on_chain_backings<T: Config>(
		session: SessionIndex,
		backing_validators_per_candidate: Vec<(
			CandidateReceipt<T::Hash>,
			Vec<(ValidatorIndex, ValidityAttestation)>,
		)>,
	) {
		crate::paras_inherent::OnChainVotes::<T>::mutate(move |value| {
			let disputes = match value.take() {
				Some(v) => v.disputes,
				None => MultiDisputeStatementSet::default(),
			};
			*value = Some(ScrapedOnChainVotes::<T::Hash> {
				backing_validators_per_candidate,
				disputes,
				session,
			});
		})
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn on_initialize(_: BlockNumberFor<T>) -> Weight {
			T::DbWeight::get().reads_writes(1, 1) // in `on_finalize`.
		}

		fn on_finalize(_: BlockNumberFor<T>) {
			if Included::<T>::take().is_none() {
				panic!("Bitfields and heads must be included every block");
			}
		}
	}

	#[pallet::inherent]
	impl<T: Config> ProvideInherent for Pallet<T> {
		type Call = Call<T>;
		type Error = MakeFatalError<()>;
		const INHERENT_IDENTIFIER: InherentIdentifier = PARACHAINS_INHERENT_IDENTIFIER;

		fn create_inherent(data: &InherentData) -> Option<Self::Call> {
			let inherent_data = Self::create_inherent_inner(data)?;

			Some(Call::enter { data: inherent_data })
		}

		fn is_inherent(call: &Self::Call) -> bool {
			matches!(call, Call::enter { .. })
		}
	}

	/// Collect all freed cores based on storage data. (i.e. append cores freed from timeouts to
	/// the given `freed_concluded`).
	///
	/// The parameter `freed_concluded` contains all core indicies that became
	/// free due to candidate that became available.
	pub(crate) fn collect_all_freed_cores<T, I>(
		freed_concluded: I,
	) -> BTreeMap<CoreIndex, FreedReason>
	where
		I: core::iter::IntoIterator<Item = (CoreIndex, CandidateHash)>,
		T: Config,
	{
		// Handle timeouts for any availability core work.
		let freed_timeout = if <scheduler::Pallet<T>>::availability_timeout_check_required() {
			let pred = <scheduler::Pallet<T>>::availability_timeout_predicate();
			<inclusion::Pallet<T>>::collect_pending(pred)
		} else {
			Vec::new()
		};

		// Schedule paras again, given freed cores, and reasons for freeing.
		let freed = freed_concluded
			.into_iter()
			.map(|(c, _hash)| (c, FreedReason::Concluded))
			.chain(freed_timeout.into_iter().map(|c| (c, FreedReason::TimedOut)))
			.collect::<BTreeMap<CoreIndex, FreedReason>>();
		freed
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Enter the paras inherent. This will process bitfields and backed candidates.
		#[pallet::call_index(0)]
		#[pallet::weight((
			paras_inherent_total_weight::<T>(
				data.backed_candidates.as_slice(),
				&data.bitfields,
				&data.disputes,
			),
			DispatchClass::Mandatory,
		))]
		pub fn enter(
			origin: OriginFor<T>,
			data: ParachainsInherentData<HeaderFor<T>>,
		) -> DispatchResultWithPostInfo {
			ensure_none(origin)?;

			ensure!(!Included::<T>::exists(), Error::<T>::TooManyInclusionInherents);
			Included::<T>::set(Some(()));

			Self::process_inherent_data(data, ProcessInherentDataContext::Enter)
				.map(|(_processed, post_info)| post_info)
		}
	}
}

impl<T: Config> Pallet<T> {
	/// Create the `ParachainsInherentData` that gets passed to [`Self::enter`] in
	/// [`Self::create_inherent`]. This code is pulled out of [`Self::create_inherent`] so it can be
	/// unit tested.
	fn create_inherent_inner(data: &InherentData) -> Option<ParachainsInherentData<HeaderFor<T>>> {
		let parachains_inherent_data = match data.get_data(&Self::INHERENT_IDENTIFIER) {
			Ok(Some(d)) => d,
			Ok(None) => return None,
			Err(_) => {
				log::warn!(target: LOG_TARGET, "ParachainsInherentData failed to decode");
				return None
			},
		};
		match Self::process_inherent_data(
			parachains_inherent_data,
			ProcessInherentDataContext::ProvideInherent,
		) {
			Ok((processed, _)) => Some(processed),
			Err(err) => {
				log::warn!(target: LOG_TARGET, "Processing inherent data failed: {:?}", err);
				None
			},
		}
	}

	/// Process inherent data.
	///
	/// The given inherent data is processed and state is altered accordingly. If any data could
	/// not be applied (inconsitencies, weight limit, ...) it is removed.
	///
	/// When called from `create_inherent` the `context` must be set to
	/// `ProcessInherentDataContext::ProvideInherent` so it guarantees the invariant that inherent
	/// is not overweight.
	/// It is **mandatory** that calls from `enter` set `context` to
	/// `ProcessInherentDataContext::Enter` to ensure the weight invariant is checked.
	///
	/// Returns: Result containing processed inherent data and weight, the processed inherent would
	/// consume.
	fn process_inherent_data(
		data: ParachainsInherentData<HeaderFor<T>>,
		context: ProcessInherentDataContext,
	) -> sp_std::result::Result<
		(ParachainsInherentData<HeaderFor<T>>, PostDispatchInfo),
		DispatchErrorWithPostInfo,
	> {
		#[cfg(feature = "runtime-metrics")]
		sp_io::init_tracing();

		let ParachainsInherentData {
			mut bitfields,
			mut backed_candidates,
			parent_header,
			mut disputes,
		} = data;

		log::debug!(
			target: LOG_TARGET,
			"[process_inherent_data] bitfields.len(): {}, backed_candidates.len(): {}, disputes.len() {}",
			bitfields.len(),
			backed_candidates.len(),
			disputes.len()
		);

		let parent_hash = <frame_system::Pallet<T>>::parent_hash();

		ensure!(
			parent_header.hash().as_ref() == parent_hash.as_ref(),
			Error::<T>::InvalidParentHeader,
		);

		let now = <frame_system::Pallet<T>>::block_number();
		let config = <configuration::Pallet<T>>::config();

		// Before anything else, update the allowed relay-parents.
		{
			let parent_number = now - One::one();
			let parent_storage_root = *parent_header.state_root();

			shared::AllowedRelayParents::<T>::mutate(|tracker| {
				tracker.update(
					parent_hash,
					parent_storage_root,
					parent_number,
					config.async_backing_params.allowed_ancestry_len,
				);
			});
		}
		let allowed_relay_parents = <shared::Pallet<T>>::allowed_relay_parents();

		let candidates_weight = backed_candidates_weight::<T>(&backed_candidates);
		let bitfields_weight = signed_bitfields_weight::<T>(&bitfields);
		let disputes_weight = multi_dispute_statement_sets_weight::<T>(&disputes);

		// Weight before filtering/sanitization
		let all_weight_before = candidates_weight + bitfields_weight + disputes_weight;

		METRICS.on_before_filter(all_weight_before.ref_time());
		log::debug!(target: LOG_TARGET, "Size before filter: {}, candidates + bitfields: {}, disputes: {}", all_weight_before.proof_size(), candidates_weight.proof_size() + bitfields_weight.proof_size(), disputes_weight.proof_size());
		log::debug!(target: LOG_TARGET, "Time weight before filter: {}, candidates + bitfields: {}, disputes: {}", all_weight_before.ref_time(), candidates_weight.ref_time() + bitfields_weight.ref_time(), disputes_weight.ref_time());

		let current_session = <shared::Pallet<T>>::session_index();
		let expected_bits = <scheduler::Pallet<T>>::availability_cores().len();
		let validator_public = shared::Pallet::<T>::active_validator_keys();

		// We are assuming (incorrectly) to have all the weight (for the mandatory class or even
		// full block) available to us. This can lead to slightly overweight blocks, which still
		// works as the dispatch class for `enter` is `Mandatory`. By using the `Mandatory`
		// dispatch class, the upper layers impose no limit on the weight of this inherent, instead
		// we limit ourselves and make sure to stay within reasonable bounds. It might make sense
		// to subtract BlockWeights::base_block to reduce chances of becoming overweight.
		let max_block_weight = {
			let dispatch_class = DispatchClass::Mandatory;
			let max_block_weight_full = <T as frame_system::Config>::BlockWeights::get();
			log::debug!(target: LOG_TARGET, "Max block weight: {}", max_block_weight_full.max_block);
			// Get max block weight for the mandatory class if defined, otherwise total max weight
			// of the block.
			let max_weight = max_block_weight_full
				.per_class
				.get(dispatch_class)
				.max_total
				.unwrap_or(max_block_weight_full.max_block);
			log::debug!(target: LOG_TARGET, "Used max block time weight: {}", max_weight);

			let max_block_size_full = <T as frame_system::Config>::BlockLength::get();
			let max_block_size = max_block_size_full.max.get(dispatch_class);
			log::debug!(target: LOG_TARGET, "Used max block size: {}", max_block_size);

			// Adjust proof size to max block size as we are tracking tx size.
			max_weight.set_proof_size(*max_block_size as u64)
		};
		log::debug!(target: LOG_TARGET, "Used max block weight: {}", max_block_weight);

		let entropy = compute_entropy::<T>(parent_hash);
		let mut rng = rand_chacha::ChaChaRng::from_seed(entropy.into());

		// Filter out duplicates and continue.
		if let Err(()) = T::DisputesHandler::deduplicate_and_sort_dispute_data(&mut disputes) {
			log::debug!(target: LOG_TARGET, "Found duplicate statement sets, retaining the first");
		}

		let post_conclusion_acceptance_period = config.dispute_post_conclusion_acceptance_period;

		let dispute_statement_set_valid = move |set: DisputeStatementSet| {
			T::DisputesHandler::filter_dispute_data(set, post_conclusion_acceptance_period)
		};

		// Limit the disputes first, since the following statements depend on the votes include
		// here.
		let (checked_disputes_sets, checked_disputes_sets_consumed_weight) =
			limit_and_sanitize_disputes::<T, _>(
				disputes,
				dispute_statement_set_valid,
				max_block_weight,
			);

		let all_weight_after = if context == ProcessInherentDataContext::ProvideInherent {
			// Assure the maximum block weight is adhered, by limiting bitfields and backed
			// candidates. Dispute statement sets were already limited before.
			let non_disputes_weight = apply_weight_limit::<T>(
				&mut backed_candidates,
				&mut bitfields,
				max_block_weight.saturating_sub(checked_disputes_sets_consumed_weight),
				&mut rng,
			);

			let all_weight_after =
				non_disputes_weight.saturating_add(checked_disputes_sets_consumed_weight);

			METRICS.on_after_filter(all_weight_after.ref_time());
			log::debug!(
			target: LOG_TARGET,
			"[process_inherent_data] after filter: bitfields.len(): {}, backed_candidates.len(): {}, checked_disputes_sets.len() {}",
			bitfields.len(),
			backed_candidates.len(),
			checked_disputes_sets.len()
			);
			log::debug!(target: LOG_TARGET, "Size after filter: {}, candidates + bitfields: {}, disputes: {}", all_weight_after.proof_size(), non_disputes_weight.proof_size(), checked_disputes_sets_consumed_weight.proof_size());
			log::debug!(target: LOG_TARGET, "Time weight after filter: {}, candidates + bitfields: {}, disputes: {}", all_weight_after.ref_time(), non_disputes_weight.ref_time(), checked_disputes_sets_consumed_weight.ref_time());

			if all_weight_after.any_gt(max_block_weight) {
				log::warn!(target: LOG_TARGET, "Post weight limiting weight is still too large, time: {}, size: {}", all_weight_after.ref_time(), all_weight_after.proof_size());
			}
			all_weight_after
		} else {
			// This check is performed in the context of block execution. Ensures inherent weight
			// invariants guaranteed by `create_inherent_data` for block authorship.
			if all_weight_before.any_gt(max_block_weight) {
				log::error!(
					"Overweight para inherent data reached the runtime {:?}: {} > {}",
					parent_hash,
					all_weight_before,
					max_block_weight
				);
			}

			ensure!(all_weight_before.all_lte(max_block_weight), Error::<T>::InherentOverweight);
			all_weight_before
		};

		// Note that `process_checked_multi_dispute_data` will iterate and import each
		// dispute; so the input here must be reasonably bounded,
		// which is guaranteed by the checks and weight limitation above.
		// We don't care about fresh or not disputes
		// this writes them to storage, so let's query it via those means
		// if this fails for whatever reason, that's ok.
		if let Err(e) =
			T::DisputesHandler::process_checked_multi_dispute_data(&checked_disputes_sets)
		{
			log::warn!(target: LOG_TARGET, "MultiDisputesData failed to update: {:?}", e);
		};
		METRICS.on_disputes_imported(checked_disputes_sets.len() as u64);

		set_scrapable_on_chain_disputes::<T>(current_session, checked_disputes_sets.clone());

		if T::DisputesHandler::is_frozen() {
			// Relay chain freeze, at this point we will not include any parachain blocks.
			METRICS.on_relay_chain_freeze();

			let disputes = checked_disputes_sets
				.into_iter()
				.map(|checked| checked.into())
				.collect::<Vec<_>>();
			let processed = ParachainsInherentData {
				bitfields: Vec::new(),
				backed_candidates: Vec::new(),
				disputes,
				parent_header,
			};

			// The relay chain we are currently on is invalid. Proceed no further on parachains.
			return Ok((processed, Some(checked_disputes_sets_consumed_weight).into()))
		}

		// Contains the disputes that are concluded in the current session only,
		// since these are the only ones that are relevant for the occupied cores
		// and lightens the load on `collect_disputed` significantly.
		// Cores can't be occupied with candidates of the previous sessions, and only
		// things with new votes can have just concluded. We only need to collect
		// cores with disputes that conclude just now, because disputes that
		// concluded longer ago have already had any corresponding cores cleaned up.
		let current_concluded_invalid_disputes = checked_disputes_sets
			.iter()
			.map(AsRef::as_ref)
			.filter(|dss| dss.session == current_session)
			.map(|dss| (dss.session, dss.candidate_hash))
			.filter(|(session, candidate)| {
				<T>::DisputesHandler::concluded_invalid(*session, *candidate)
			})
			.map(|(_session, candidate)| candidate)
			.collect::<BTreeSet<CandidateHash>>();

		let freed_disputed: BTreeMap<CoreIndex, FreedReason> =
			<inclusion::Pallet<T>>::collect_disputed(&current_concluded_invalid_disputes)
				.into_iter()
				.map(|core| (core, FreedReason::Concluded))
				.collect();

		// Create a bit index from the set of core indices where each index corresponds to
		// a core index that was freed due to a dispute.
		//
		// I.e. 010100 would indicate, the candidates on Core 1 and 3 would be disputed.
		let disputed_bitfield = create_disputed_bitfield(expected_bits, freed_disputed.keys());

		if !freed_disputed.is_empty() {
			<scheduler::Pallet<T>>::free_cores_and_fill_claimqueue(freed_disputed.clone(), now);
		}

		let bitfields = sanitize_bitfields::<T>(
			bitfields,
			disputed_bitfield,
			expected_bits,
			parent_hash,
			current_session,
			&validator_public[..],
		);
		METRICS.on_bitfields_processed(bitfields.len() as u64);

		// Process new availability bitfields, yielding any availability cores whose
		// work has now concluded.
		let freed_concluded =
			<inclusion::Pallet<T>>::update_pending_availability_and_get_freed_cores::<_>(
				expected_bits,
				&validator_public[..],
				bitfields.clone(),
				<scheduler::Pallet<T>>::core_para,
			);

		// Inform the disputes module of all included candidates.
		for (_, candidate_hash) in &freed_concluded {
			T::DisputesHandler::note_included(current_session, *candidate_hash, now);
		}

		METRICS.on_candidates_included(freed_concluded.len() as u64);

		let freed = collect_all_freed_cores::<T, _>(freed_concluded.iter().cloned());

		<scheduler::Pallet<T>>::free_cores_and_fill_claimqueue(freed, now);

		METRICS.on_candidates_processed_total(backed_candidates.len() as u64);

		let core_index_enabled = configuration::Pallet::<T>::config()
			.node_features
			.get(FeatureIndex::ElasticScalingMVP as usize)
			.map(|b| *b)
			.unwrap_or(false);

		let mut scheduled: BTreeMap<ParaId, BTreeSet<CoreIndex>> = BTreeMap::new();
		let mut total_scheduled_cores = 0;

		for (core_idx, para_id) in <scheduler::Pallet<T>>::scheduled_paras() {
			total_scheduled_cores += 1;
			scheduled.entry(para_id).or_default().insert(core_idx);
		}

		let SanitizedBackedCandidates {
			backed_candidates_with_core,
			votes_from_disabled_were_dropped,
			dropped_unscheduled_candidates,
		} = sanitize_backed_candidates::<T, _>(
			backed_candidates,
			&allowed_relay_parents,
			|candidate_idx: usize,
			 backed_candidate: &BackedCandidate<<T as frame_system::Config>::Hash>|
			 -> bool {
				let para_id = backed_candidate.descriptor().para_id;
				let prev_context = <paras::Pallet<T>>::para_most_recent_context(para_id);
				let check_ctx = CandidateCheckContext::<T>::new(prev_context);

				// never include a concluded-invalid candidate
				current_concluded_invalid_disputes.contains(&backed_candidate.hash()) ||
					// Instead of checking the candidates with code upgrades twice
					// move the checking up here and skip it in the training wheels fallback.
					// That way we avoid possible duplicate checks while assuring all
					// backed candidates fine to pass on.
					//
					// NOTE: this is the only place where we check the relay-parent.
					check_ctx
						.verify_backed_candidate(&allowed_relay_parents, candidate_idx, backed_candidate.candidate())
						.is_err()
			},
			scheduled,
			core_index_enabled,
		);

		ensure!(
			backed_candidates_with_core.len() <= total_scheduled_cores,
			Error::<T>::UnscheduledCandidate
		);

		METRICS.on_candidates_sanitized(backed_candidates_with_core.len() as u64);

		// In `Enter` context (invoked during execution) there should be no backing votes from
		// disabled validators because they should have been filtered out during inherent data
		// preparation (`ProvideInherent` context). Abort in such cases.
		if context == ProcessInherentDataContext::Enter {
			ensure!(!votes_from_disabled_were_dropped, Error::<T>::BackedByDisabled);
		}

		// In `Enter` context (invoked during execution) we shouldn't have filtered any candidates
		// due to a para not being scheduled. They have been filtered during inherent data
		// preparation (`ProvideInherent` context). Abort in such cases.
		if context == ProcessInherentDataContext::Enter {
			ensure!(!dropped_unscheduled_candidates, Error::<T>::BackedOnUnscheduledCore);
		}

		// Process backed candidates according to scheduled cores.
		let inclusion::ProcessedCandidates::<<HeaderFor<T> as HeaderT>::Hash> {
			core_indices: occupied,
			candidate_receipt_with_backing_validator_indices,
		} = <inclusion::Pallet<T>>::process_candidates(
			&allowed_relay_parents,
			backed_candidates_with_core.clone(),
			<scheduler::Pallet<T>>::group_validators,
			core_index_enabled,
		)?;
		// Note which of the scheduled cores were actually occupied by a backed candidate.
		<scheduler::Pallet<T>>::occupied(occupied.into_iter().map(|e| (e.0, e.1)).collect());

		set_scrapable_on_chain_backings::<T>(
			current_session,
			candidate_receipt_with_backing_validator_indices,
		);

		let disputes = checked_disputes_sets
			.into_iter()
			.map(|checked| checked.into())
			.collect::<Vec<_>>();

		let bitfields = bitfields.into_iter().map(|v| v.into_unchecked()).collect();

		let processed = ParachainsInherentData {
			bitfields,
			backed_candidates: backed_candidates_with_core
				.into_iter()
				.map(|(candidate, _)| candidate)
				.collect(),
			disputes,
			parent_header,
		};
		Ok((processed, Some(all_weight_after).into()))
	}
}

/// Derive a bitfield from dispute
pub(super) fn create_disputed_bitfield<'a, I>(
	expected_bits: usize,
	freed_cores: I,
) -> DisputedBitfield
where
	I: 'a + IntoIterator<Item = &'a CoreIndex>,
{
	let mut bitvec = BitVec::repeat(false, expected_bits);
	for core_idx in freed_cores {
		let core_idx = core_idx.0 as usize;
		if core_idx < expected_bits {
			bitvec.set(core_idx, true);
		}
	}
	DisputedBitfield::from(bitvec)
}

/// Select a random subset, with preference for certain indices.
///
/// Adds random items to the set until all candidates
/// are tried or the remaining weight is depleted.
///
/// Returns the weight of all selected items from `selectables`
/// as well as their indices in ascending order.
fn random_sel<X, F: Fn(&X) -> Weight>(
	rng: &mut rand_chacha::ChaChaRng,
	selectables: &[X],
	mut preferred_indices: Vec<usize>,
	weight_fn: F,
	weight_limit: Weight,
) -> (Weight, Vec<usize>) {
	if selectables.is_empty() {
		return (Weight::zero(), Vec::new())
	}
	// all indices that are not part of the preferred set
	let mut indices = (0..selectables.len())
		.into_iter()
		.filter(|idx| !preferred_indices.contains(idx))
		.collect::<Vec<_>>();
	let mut picked_indices = Vec::with_capacity(selectables.len().saturating_sub(1));

	let mut weight_acc = Weight::zero();

	preferred_indices.shuffle(rng);
	for preferred_idx in preferred_indices {
		// preferred indices originate from outside
		if let Some(item) = selectables.get(preferred_idx) {
			let updated = weight_acc.saturating_add(weight_fn(item));
			if updated.any_gt(weight_limit) {
				continue
			}
			weight_acc = updated;
			picked_indices.push(preferred_idx);
		}
	}

	indices.shuffle(rng);
	for idx in indices {
		let item = &selectables[idx];
		let updated = weight_acc.saturating_add(weight_fn(item));

		if updated.any_gt(weight_limit) {
			continue
		}
		weight_acc = updated;

		picked_indices.push(idx);
	}

	// sorting indices, so the ordering is retained
	// unstable sorting is fine, since there are no duplicates in indices
	// and even if there were, they don't have an identity
	picked_indices.sort_unstable();
	(weight_acc, picked_indices)
}

/// Considers an upper threshold that the inherent data must not exceed.
///
/// If there is sufficient space, all bitfields and all candidates
/// will be included.
///
/// Otherwise tries to include all disputes, and then tries to fill the remaining space with
/// bitfields and then candidates.
///
/// The selection process is random. For candidates, there is an exception for code upgrades as they
/// are preferred. And for disputes, local and older disputes are preferred (see
/// `limit_and_sanitize_disputes`). for backed candidates, since with a increasing number of
/// parachains their chances of inclusion become slim. All backed candidates  are checked
/// beforehands in `fn create_inherent_inner` which guarantees sanity.
///
/// Assumes disputes are already filtered by the time this is called.
///
/// Returns the total weight consumed by `bitfields` and `candidates`.
pub(crate) fn apply_weight_limit<T: Config + inclusion::Config>(
	candidates: &mut Vec<BackedCandidate<<T>::Hash>>,
	bitfields: &mut UncheckedSignedAvailabilityBitfields,
	max_consumable_weight: Weight,
	rng: &mut rand_chacha::ChaChaRng,
) -> Weight {
	let total_candidates_weight = backed_candidates_weight::<T>(candidates.as_slice());

	let total_bitfields_weight = signed_bitfields_weight::<T>(&bitfields);

	let total = total_bitfields_weight.saturating_add(total_candidates_weight);

	// candidates + bitfields fit into the block
	if max_consumable_weight.all_gte(total) {
		return total
	}

	// Invariant: block author provides candidate in the order in which they form a chain
	// wrt elastic scaling. If the invariant is broken, we'd fail later when filtering candidates
	// which are unchained.

	let mut chained_candidates: Vec<Vec<_>> = Vec::new();
	let mut current_para_id = None;

	for candidate in sp_std::mem::take(candidates).into_iter() {
		let candidate_para_id = candidate.descriptor().para_id;
		if Some(candidate_para_id) == current_para_id {
			let chain = chained_candidates
				.last_mut()
				.expect("if the current_para_id is Some, then vec is not empty; qed");
			chain.push(candidate);
		} else {
			current_para_id = Some(candidate_para_id);
			chained_candidates.push(vec![candidate]);
		}
	}

	// Elastic scaling: we prefer chains that have a code upgrade among the candidates,
	// as the candidates containing the upgrade tend to be large and hence stand no chance to
	// be picked late while maintaining the weight bounds.
	//
	// Limitations: For simplicity if total weight of a chain of candidates is larger than
	// the remaining weight, the chain will still not be included while it could still be possible
	// to include part of that chain.
	let preferred_chain_indices = chained_candidates
		.iter()
		.enumerate()
		.filter_map(|(idx, candidates)| {
			// Check if any of the candidate in chain contains a code upgrade.
			if candidates
				.iter()
				.any(|candidate| candidate.candidate().commitments.new_validation_code.is_some())
			{
				Some(idx)
			} else {
				None
			}
		})
		.collect::<Vec<usize>>();

	// There is weight remaining to be consumed by a subset of chained candidates
	// which are going to be picked now.
	if let Some(max_consumable_by_candidates) =
		max_consumable_weight.checked_sub(&total_bitfields_weight)
	{
		let (acc_candidate_weight, chained_indices) =
			random_sel::<Vec<BackedCandidate<<T as frame_system::Config>::Hash>>, _>(
				rng,
				&chained_candidates,
				preferred_chain_indices,
				|candidates| backed_candidates_weight::<T>(&candidates),
				max_consumable_by_candidates,
			);
		log::debug!(target: LOG_TARGET, "Indices Candidates: {:?}, size: {}", chained_indices, candidates.len());
		chained_candidates
			.indexed_retain(|idx, _backed_candidates| chained_indices.binary_search(&idx).is_ok());
		// pick all bitfields, and
		// fill the remaining space with candidates
		let total_consumed = acc_candidate_weight.saturating_add(total_bitfields_weight);

		*candidates = chained_candidates.into_iter().flatten().collect::<Vec<_>>();

		return total_consumed
	}

	candidates.clear();

	// insufficient space for even the bitfields alone, so only try to fit as many of those
	// into the block and skip the candidates entirely
	let (total_consumed, indices) = random_sel::<UncheckedSignedAvailabilityBitfield, _>(
		rng,
		&bitfields,
		vec![],
		|bitfield| signed_bitfield_weight::<T>(&bitfield),
		max_consumable_weight,
	);
	log::debug!(target: LOG_TARGET, "Indices Bitfields: {:?}, size: {}", indices, bitfields.len());

	bitfields.indexed_retain(|idx, _bitfield| indices.binary_search(&idx).is_ok());

	total_consumed
}

/// Filter bitfields based on freed core indices, validity, and other sanity checks.
///
/// Do sanity checks on the bitfields:
///
///  1. no more than one bitfield per validator
///  2. bitfields are ascending by validator index.
///  3. each bitfield has exactly `expected_bits`
///  4. signature is valid
///  5. remove any disputed core indices
///
/// If any of those is not passed, the bitfield is dropped.
pub(crate) fn sanitize_bitfields<T: crate::inclusion::Config>(
	unchecked_bitfields: UncheckedSignedAvailabilityBitfields,
	disputed_bitfield: DisputedBitfield,
	expected_bits: usize,
	parent_hash: T::Hash,
	session_index: SessionIndex,
	validators: &[ValidatorId],
) -> SignedAvailabilityBitfields {
	let mut bitfields = Vec::with_capacity(unchecked_bitfields.len());

	let mut last_index: Option<ValidatorIndex> = None;

	if disputed_bitfield.0.len() != expected_bits {
		// This is a system logic error that should never occur, but we want to handle it gracefully
		// so we just drop all bitfields
		log::error!(target: LOG_TARGET, "BUG: disputed_bitfield != expected_bits");
		return vec![]
	}

	let all_zeros = BitVec::<u8, bitvec::order::Lsb0>::repeat(false, expected_bits);
	let signing_context = SigningContext { parent_hash, session_index };
	for unchecked_bitfield in unchecked_bitfields {
		// Find and skip invalid bitfields.
		if unchecked_bitfield.unchecked_payload().0.len() != expected_bits {
			log::trace!(
				target: LOG_TARGET,
				"bad bitfield length: {} != {:?}",
				unchecked_bitfield.unchecked_payload().0.len(),
				expected_bits,
			);
			continue
		}

		if unchecked_bitfield.unchecked_payload().0.clone() & disputed_bitfield.0.clone() !=
			all_zeros
		{
			log::trace!(
				target: LOG_TARGET,
				"bitfield contains disputed cores: {:?}",
				unchecked_bitfield.unchecked_payload().0.clone() & disputed_bitfield.0.clone()
			);
			continue
		}

		let validator_index = unchecked_bitfield.unchecked_validator_index();

		if !last_index.map_or(true, |last_index: ValidatorIndex| last_index < validator_index) {
			log::trace!(
				target: LOG_TARGET,
				"bitfield validator index is not greater than last: !({:?} < {})",
				last_index.as_ref().map(|x| x.0),
				validator_index.0
			);
			continue
		}

		if unchecked_bitfield.unchecked_validator_index().0 as usize >= validators.len() {
			log::trace!(
				target: LOG_TARGET,
				"bitfield validator index is out of bounds: {} >= {}",
				validator_index.0,
				validators.len(),
			);
			continue
		}

		let validator_public = &validators[validator_index.0 as usize];

		// Validate bitfield signature.
		if let Ok(signed_bitfield) =
			unchecked_bitfield.try_into_checked(&signing_context, validator_public)
		{
			bitfields.push(signed_bitfield);
			METRICS.on_valid_bitfield_signature();
		} else {
			log::warn!(target: LOG_TARGET, "Invalid bitfield signature");
			METRICS.on_invalid_bitfield_signature();
		};

		last_index = Some(validator_index);
	}
	bitfields
}

// Result from `sanitize_backed_candidates`
#[derive(Debug, PartialEq)]
struct SanitizedBackedCandidates<Hash> {
	// Sanitized backed candidates along with the assigned core. The `Vec` is sorted according to
	// the occupied core index.
	backed_candidates_with_core: Vec<(BackedCandidate<Hash>, CoreIndex)>,
	// Set to true if any votes from disabled validators were dropped from the input.
	votes_from_disabled_were_dropped: bool,
	// Set to true if any candidates were dropped due to filtering done in
	// `map_candidates_to_cores`
	dropped_unscheduled_candidates: bool,
}

/// Filter out:
/// 1. any candidates that have a concluded invalid dispute
/// 2. any unscheduled candidates, as well as candidates whose paraid has multiple cores assigned
///    but have no injected core index.
/// 3. all backing votes from disabled validators
/// 4. any candidates that end up with less than `effective_minimum_backing_votes` backing votes
///
/// `scheduled` follows the same naming scheme as provided in the
/// guide: Currently `free` but might become `occupied`.
/// For the filtering here the relevant part is only the current `free`
/// state.
///
/// `candidate_has_concluded_invalid_dispute` must return `true` if the candidate
/// is disputed, false otherwise. The passed `usize` is the candidate index.
///
/// Returns struct `SanitizedBackedCandidates` where `backed_candidates` are sorted according to the
/// occupied core index.
fn sanitize_backed_candidates<
	T: crate::inclusion::Config,
	F: FnMut(usize, &BackedCandidate<T::Hash>) -> bool,
>(
	mut backed_candidates: Vec<BackedCandidate<T::Hash>>,
	allowed_relay_parents: &AllowedRelayParentsTracker<T::Hash, BlockNumberFor<T>>,
	mut candidate_has_concluded_invalid_dispute_or_is_invalid: F,
	scheduled: BTreeMap<ParaId, BTreeSet<CoreIndex>>,
	core_index_enabled: bool,
) -> SanitizedBackedCandidates<T::Hash> {
	// Remove any candidates that were concluded invalid.
	// This does not assume sorting.
	backed_candidates.indexed_retain(move |candidate_idx, backed_candidate| {
		!candidate_has_concluded_invalid_dispute_or_is_invalid(candidate_idx, backed_candidate)
	});

	let initial_candidate_count = backed_candidates.len();
	// Map candidates to scheduled cores. Filter out any unscheduled candidates.
	let mut backed_candidates_with_core = map_candidates_to_cores::<T>(
		&allowed_relay_parents,
		scheduled,
		core_index_enabled,
		backed_candidates,
	);

	let dropped_unscheduled_candidates =
		initial_candidate_count != backed_candidates_with_core.len();

	// Filter out backing statements from disabled validators
	let votes_from_disabled_were_dropped = filter_backed_statements_from_disabled_validators::<T>(
		&mut backed_candidates_with_core,
		&allowed_relay_parents,
		core_index_enabled,
	);

	// Sort the `Vec` last, once there is a guarantee that these
	// `BackedCandidates` references the expected relay chain parent,
	// but more importantly are scheduled for a free core.
	// This both avoids extra work for obviously invalid candidates,
	// but also allows this to be done in place.
	backed_candidates_with_core.sort_by(|(_x, core_x), (_y, core_y)| core_x.cmp(&core_y));

	SanitizedBackedCandidates {
		dropped_unscheduled_candidates,
		votes_from_disabled_were_dropped,
		backed_candidates_with_core,
	}
}

/// Derive entropy from babe provided per block randomness.
///
/// In the odd case none is available, uses the `parent_hash` and
/// a const value, while emitting a warning.
fn compute_entropy<T: Config>(parent_hash: T::Hash) -> [u8; 32] {
	const CANDIDATE_SEED_SUBJECT: [u8; 32] = *b"candidate-seed-selection-subject";
	// NOTE: this is slightly gameable since this randomness was already public
	// by the previous block, while for the block author this randomness was
	// known 2 epochs ago. it is marginally better than using the parent block
	// hash since it's harder to influence the VRF output than the block hash.
	let vrf_random = ParentBlockRandomness::<T>::random(&CANDIDATE_SEED_SUBJECT[..]).0;
	let mut entropy: [u8; 32] = CANDIDATE_SEED_SUBJECT;
	if let Some(vrf_random) = vrf_random {
		entropy.as_mut().copy_from_slice(vrf_random.as_ref());
	} else {
		// in case there is no VRF randomness present, we utilize the relay parent
		// as seed, it's better than a static value.
		log::warn!(target: LOG_TARGET, "ParentBlockRandomness did not provide entropy");
		entropy.as_mut().copy_from_slice(parent_hash.as_ref());
	}
	entropy
}

/// Limit disputes in place.
///
/// Assumes ordering of disputes, retains sorting of the statement.
///
/// Prime source of overload safety for dispute votes:
/// 1. Check accumulated weight does not exceed the maximum block weight.
/// 2. If exceeded:
///   1. Check validity of all dispute statements sequentially
/// 2. If not exceeded:
///   1. If weight is exceeded by locals, pick the older ones (lower indices) until the weight limit
///      is reached.
///
/// Returns the consumed weight amount, that is guaranteed to be less than the provided
/// `max_consumable_weight`.
fn limit_and_sanitize_disputes<
	T: Config,
	CheckValidityFn: FnMut(DisputeStatementSet) -> Option<CheckedDisputeStatementSet>,
>(
	disputes: MultiDisputeStatementSet,
	mut dispute_statement_set_valid: CheckValidityFn,
	max_consumable_weight: Weight,
) -> (Vec<CheckedDisputeStatementSet>, Weight) {
	// The total weight if all disputes would be included
	let disputes_weight = multi_dispute_statement_sets_weight::<T>(&disputes);

	if disputes_weight.any_gt(max_consumable_weight) {
		log::debug!(target: LOG_TARGET, "Above max consumable weight: {}/{}", disputes_weight, max_consumable_weight);
		let mut checked_acc = Vec::<CheckedDisputeStatementSet>::with_capacity(disputes.len());

		// Accumualated weight of all disputes picked, that passed the checks.
		let mut weight_acc = Weight::zero();

		// Select disputes in-order until the remaining weight is attained
		disputes.into_iter().for_each(|dss| {
			let dispute_weight = dispute_statement_set_weight::<T, &DisputeStatementSet>(&dss);
			let updated = weight_acc.saturating_add(dispute_weight);
			if max_consumable_weight.all_gte(updated) {
				// Always apply the weight. Invalid data cost processing time too:
				weight_acc = updated;
				if let Some(checked) = dispute_statement_set_valid(dss) {
					checked_acc.push(checked);
				}
			}
		});

		(checked_acc, weight_acc)
	} else {
		// Go through all of them, and just apply the filter, they would all fit
		let checked = disputes
			.into_iter()
			.filter_map(|dss| dispute_statement_set_valid(dss))
			.collect::<Vec<CheckedDisputeStatementSet>>();
		// some might have been filtered out, so re-calc the weight
		let checked_disputes_weight = checked_multi_dispute_statement_sets_weight::<T>(&checked);
		(checked, checked_disputes_weight)
	}
}

// Filters statements from disabled validators in `BackedCandidate`, non-scheduled candidates and
// few more sanity checks. Returns `true` if at least one statement is removed and `false`
// otherwise.
fn filter_backed_statements_from_disabled_validators<T: shared::Config + scheduler::Config>(
	backed_candidates_with_core: &mut Vec<(
		BackedCandidate<<T as frame_system::Config>::Hash>,
		CoreIndex,
	)>,
	allowed_relay_parents: &AllowedRelayParentsTracker<T::Hash, BlockNumberFor<T>>,
	core_index_enabled: bool,
) -> bool {
	let disabled_validators =
		BTreeSet::<_>::from_iter(shared::Pallet::<T>::disabled_validators().into_iter());

	if disabled_validators.is_empty() {
		// No disabled validators - nothing to do
		return false
	}

	let backed_len_before = backed_candidates_with_core.len();

	// Flag which will be returned. Set to `true` if at least one vote is filtered.
	let mut filtered = false;

	let minimum_backing_votes = configuration::Pallet::<T>::config().minimum_backing_votes;

	// Process all backed candidates. `validator_indices` in `BackedCandidates` are indices within
	// the validator group assigned to the parachain. To obtain this group we need:
	// 1. Core index assigned to the parachain which has produced the candidate
	// 2. The relay chain block number of the candidate
	backed_candidates_with_core.retain_mut(|(bc, core_idx)| {
		let (validator_indices, maybe_core_index) = bc.validator_indices_and_core_index(core_index_enabled);
		let mut validator_indices = BitVec::<_>::from(validator_indices);

		// Get relay parent block number of the candidate. We need this to get the group index assigned to this core at this block number
		let relay_parent_block_number = match allowed_relay_parents
			.acquire_info(bc.descriptor().relay_parent, None) {
				Some((_, block_num)) => block_num,
				None => {
					log::debug!(target: LOG_TARGET, "Relay parent {:?} for candidate is not in the allowed relay parents. Dropping the candidate.", bc.descriptor().relay_parent);
					return false
				}
			};

		// Get the group index for the core
		let group_idx = match <scheduler::Pallet<T>>::group_assigned_to_core(
			*core_idx,
			relay_parent_block_number + One::one(),
		) {
			Some(group_idx) => group_idx,
			None => {
				log::debug!(target: LOG_TARGET, "Can't get the group index for core idx {:?}. Dropping the candidate.", core_idx);
				return false
			},
		};

		// And finally get the validator group for this group index
		let validator_group = match <scheduler::Pallet<T>>::group_validators(group_idx) {
			Some(validator_group) => validator_group,
			None => {
				log::debug!(target: LOG_TARGET, "Can't get the validators from group {:?}. Dropping the candidate.", group_idx);
				return false
			}
		};

		// Bitmask with the disabled indices within the validator group
		let disabled_indices = BitVec::<u8, bitvec::order::Lsb0>::from_iter(validator_group.iter().map(|idx| disabled_validators.contains(idx)));
		// The indices of statements from disabled validators in `BackedCandidate`. We have to drop these.
		let indices_to_drop = disabled_indices.clone() & &validator_indices;
		// Apply the bitmask to drop the disabled validator from `validator_indices`
		validator_indices &= !disabled_indices;
		// Update the backed candidate
		bc.set_validator_indices_and_core_index(validator_indices, maybe_core_index);

		// Remove the corresponding votes from `validity_votes`
		for idx in indices_to_drop.iter_ones().rev() {
			bc.validity_votes_mut().remove(idx);
		}

		// If at least one statement was dropped we need to return `true`
		if indices_to_drop.count_ones() > 0 {
			filtered = true;
		}

		// By filtering votes we might render the candidate invalid and cause a failure in
		// [`process_candidates`]. To avoid this we have to perform a sanity check here. If there
		// are not enough backing votes after filtering we will remove the whole candidate.
		if bc.validity_votes().len() < effective_minimum_backing_votes(
			validator_group.len(),
			minimum_backing_votes
		) {
			return false
		}

		true
	});

	// Also return `true` if a whole candidate was dropped from the set
	filtered || backed_len_before != backed_candidates_with_core.len()
}

/// Map candidates to scheduled cores.
/// If the para only has one scheduled core and no `CoreIndex` is injected, map the candidate to the
/// single core. If the para has multiple cores scheduled, only map the candidates which have a
/// proper core injected. Filter out the rest.
/// Also returns whether or not we dropped any candidates.
fn map_candidates_to_cores<T: configuration::Config + scheduler::Config + inclusion::Config>(
	allowed_relay_parents: &AllowedRelayParentsTracker<T::Hash, BlockNumberFor<T>>,
	mut scheduled: BTreeMap<ParaId, BTreeSet<CoreIndex>>,
	core_index_enabled: bool,
	candidates: Vec<BackedCandidate<T::Hash>>,
) -> Vec<(BackedCandidate<T::Hash>, CoreIndex)> {
	let mut backed_candidates_with_core = Vec::with_capacity(candidates.len());

	// We keep a candidate if the parachain has only one core assigned or if
	// a core index is provided by block author and it's indeed scheduled.
	for backed_candidate in candidates {
		let maybe_injected_core_index = get_injected_core_index::<T>(
			allowed_relay_parents,
			&backed_candidate,
			core_index_enabled,
		);

		let scheduled_cores = scheduled.get_mut(&backed_candidate.descriptor().para_id);
		// Candidates without scheduled cores are silently filtered out.
		if let Some(scheduled_cores) = scheduled_cores {
			if let Some(core_idx) = maybe_injected_core_index {
				if scheduled_cores.contains(&core_idx) {
					scheduled_cores.remove(&core_idx);
					backed_candidates_with_core.push((backed_candidate, core_idx));
				}
			} else if scheduled_cores.len() == 1 {
				backed_candidates_with_core
					.push((backed_candidate, scheduled_cores.pop_first().expect("Length is 1")));
			}
		}
	}

	backed_candidates_with_core
}

fn get_injected_core_index<T: configuration::Config + scheduler::Config + inclusion::Config>(
	allowed_relay_parents: &AllowedRelayParentsTracker<T::Hash, BlockNumberFor<T>>,
	candidate: &BackedCandidate<T::Hash>,
	core_index_enabled: bool,
) -> Option<CoreIndex> {
	// After stripping the 8 bit extensions, the `validator_indices` field length is expected
	// to be equal to backing group size. If these don't match, the `CoreIndex` is badly encoded,
	// or not supported.
	let (validator_indices, maybe_core_idx) =
		candidate.validator_indices_and_core_index(core_index_enabled);

	let Some(core_idx) = maybe_core_idx else { return None };

	let relay_parent_block_number =
		match allowed_relay_parents.acquire_info(candidate.descriptor().relay_parent, None) {
			Some((_, block_num)) => block_num,
			None => {
				log::debug!(
					target: LOG_TARGET,
					"Relay parent {:?} for candidate {:?} is not in the allowed relay parents. Dropping the candidate.",
					candidate.descriptor().relay_parent,
					candidate.candidate().hash(),
				);
				return None
			},
		};

	// Get the backing group of the candidate backed at `core_idx`.
	let group_idx = match <scheduler::Pallet<T>>::group_assigned_to_core(
		core_idx,
		relay_parent_block_number + One::one(),
	) {
		Some(group_idx) => group_idx,
		None => {
			log::debug!(
				target: LOG_TARGET,
				"Can't get the group index for core idx {:?}. Dropping the candidate {:?}.",
				core_idx,
				candidate.candidate().hash(),
			);
			return None
		},
	};

	let group_validators = match <scheduler::Pallet<T>>::group_validators(group_idx) {
		Some(validators) => validators,
		None => return None,
	};

	if group_validators.len() == validator_indices.len() {
		Some(core_idx)
	} else {
		None
	}
}
