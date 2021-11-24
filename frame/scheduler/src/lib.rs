// This file is part of Substrate.

// Copyright (C) 2017-2021 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! # Scheduler
//! A Pallet for scheduling dispatches.
//!
//! - [`Config`]
//! - [`Call`]
//! - [`Pallet`]
//!
//! ## Overview
//!
//! This Pallet exposes capabilities for scheduling dispatches to occur at a
//! specified block number or at a specified period. These scheduled dispatches
//! may be named or anonymous and may be canceled.
//!
//! **NOTE:** The scheduled calls will be dispatched with the default filter
//! for the origin: namely `frame_system::Config::BaseCallFilter` for all origin
//! except root which will get no filter. And not the filter contained in origin
//! use to call `fn schedule`.
//!
//! If a call is scheduled using proxy or whatever mecanism which adds filter,
//! then those filter will not be used when dispatching the schedule call.
//!
//! ## Interface
//!
//! ### Dispatchable Functions
//!
//! * `schedule` - schedule a dispatch, which may be periodic, to occur at a specified block and
//!   with a specified priority.
//! * `cancel` - cancel a scheduled dispatch, specified by block number and index.
//! * `schedule_named` - augments the `schedule` interface with an additional `Vec<u8>` parameter
//!   that can be used for identification.
//! * `cancel_named` - the named complement to the cancel function.

// Ensure we're `no_std` when compiling for Wasm.
#![cfg_attr(not(feature = "std"), no_std)]

#![cfg(feature = "runtime-benchmarks")]
mod benchmarking;
pub mod weights;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod mock;

use codec::{Codec, Decode, Encode};
use frame_support::{
	dispatch::{DispatchError, DispatchResult, Dispatchable, Parameter},
	traits::{
		schedule::{self, DispatchTime, MaybeHashed},
		EnsureOrigin, Get, IsType, OriginTrait, PrivilegeCmp,
	},
	weights::{GetDispatchInfo, Weight},
};
use frame_system::{self as system, ensure_signed};
pub use pallet::*;
use scale_info::TypeInfo;
use sp_runtime::{
	traits::{BadOrigin, One, Saturating, Zero},
	RuntimeDebug,
};
use sp_std::{borrow::Borrow, cmp::Ordering, marker::PhantomData, prelude::*};
pub use weights::WeightInfo;

/// Just a simple index for naming period tasks.
pub type PeriodicIndex = u32;
/// The location of a scheduled task that can be used to remove it.
pub type TaskAddress<BlockNumber> = (BlockNumber, u32);

pub type CallOrHashOf<T> = MaybeHashed<
	<T as Config>::Call,
	<T as frame_system::Config>::Hash,
>;

#[cfg_attr(any(feature = "std", test), derive(PartialEq, Eq))]
#[derive(Clone, RuntimeDebug, Encode, Decode)]
struct ScheduledV1<Call, BlockNumber> {
	maybe_id: Option<Vec<u8>>,
	priority: schedule::Priority,
	call: Call,
	maybe_periodic: Option<schedule::Period<BlockNumber>>,
}

/// Information regarding an item to be executed in the future.
#[cfg_attr(any(feature = "std", test), derive(PartialEq, Eq))]
#[derive(Clone, RuntimeDebug, Encode, Decode, TypeInfo)]
pub struct ScheduledV3<Call, BlockNumber, PalletsOrigin, AccountId> {
	/// The unique identity for this task, if there is one.
	maybe_id: Option<Vec<u8>>,
	/// This task's priority.
	priority: schedule::Priority,
	/// The call to be dispatched.
	call: Call,
	/// If the call is periodic, then this points to the information concerning that.
	maybe_periodic: Option<schedule::Period<BlockNumber>>,
	/// The origin to dispatch the call.
	origin: PalletsOrigin,
	_phantom: PhantomData<AccountId>,
}

use crate::ScheduledV3 as ScheduledV2;

pub type ScheduledV2Of<T> = ScheduledV3<
	<T as Config>::Call,
	<T as frame_system::Config>::BlockNumber,
	<T as Config>::PalletsOrigin,
	<T as frame_system::Config>::AccountId,
>;

pub type ScheduledV3Of<T> = ScheduledV3<
	CallOrHashOf<T>,
	<T as frame_system::Config>::BlockNumber,
	<T as Config>::PalletsOrigin,
	<T as frame_system::Config>::AccountId,
>;

pub type ScheduledOf<T> = ScheduledV3Of<T>;

/// The current version of Scheduled struct.
pub type Scheduled<Call, BlockNumber, PalletsOrigin, AccountId> =
	ScheduledV2<Call, BlockNumber, PalletsOrigin, AccountId>;

// A value placed in storage that represents the current version of the Scheduler storage.
// This value is used by the `on_runtime_upgrade` logic to determine whether we run
// storage migration logic.
#[derive(Encode, Decode, Clone, Copy, PartialEq, Eq, RuntimeDebug, TypeInfo)]
enum Releases {
	V1,
	V2,
	V3,
}

impl Default for Releases {
	fn default() -> Self {
		Releases::V1
	}
}

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use frame_support::{pallet_prelude::*, traits::{PreimageProvider, schedule::LookupError}};
	use frame_system::pallet_prelude::*;

	#[pallet::pallet]
	#[pallet::generate_store(pub(super) trait Store)]
	pub struct Pallet<T>(_);

	/// `system::Config` should always be included in our implied traits.
	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// The overarching event type.
		type Event: From<Event<Self>> + IsType<<Self as frame_system::Config>::Event>;

		/// The aggregated origin which the dispatch will take.
		type Origin: OriginTrait<PalletsOrigin = Self::PalletsOrigin>
			+ From<Self::PalletsOrigin>
			+ IsType<<Self as system::Config>::Origin>;

		/// The caller origin, overarching type of all pallets origins.
		type PalletsOrigin: From<system::RawOrigin<Self::AccountId>> + Codec + Clone + Eq + TypeInfo;

		/// The aggregated call type.
		type Call: Parameter
			+ Dispatchable<Origin = <Self as Config>::Origin>
			+ GetDispatchInfo
			+ From<system::Call<Self>>;

		/// The maximum weight that may be scheduled per block for any dispatchables of less
		/// priority than `schedule::HARD_DEADLINE`.
		#[pallet::constant]
		type MaximumWeight: Get<Weight>;

		/// Required origin to schedule or cancel calls.
		type ScheduleOrigin: EnsureOrigin<<Self as system::Config>::Origin>;

		/// Compare the privileges of origins.
		///
		/// This will be used when canceling a task, to ensure that the origin that tries
		/// to cancel has greater or equal privileges as the origin that created the scheduled task.
		///
		/// For simplicity the [`EqualPrivilegeOnly`](frame_support::traits::EqualPrivilegeOnly) can
		/// be used. This will only check if two given origins are equal.
		type OriginPrivilegeCmp: PrivilegeCmp<Self::PalletsOrigin>;

		/// The maximum number of scheduled calls in the queue for a single block.
		/// Not strictly enforced, but used for weight estimation.
		#[pallet::constant]
		type MaxScheduledPerBlock: Get<u32>;

		/// Weight information for extrinsics in this pallet.
		type WeightInfo: WeightInfo;

		/// The preimage provider with which we look up call hashes to get the call.
		type Preimages: PreimageProvider<Self::Hash>;
	}

	/// Items to be executed, indexed by the block number that they should be executed on.
	#[pallet::storage]
	pub type Agenda<T: Config> = StorageMap<
		_,
		Twox64Concat,
		T::BlockNumber,
		Vec<Option<ScheduledV3Of<T>>>,
		ValueQuery,
	>;

	/// Lookup from identity to the block number and index of the task.
	#[pallet::storage]
	pub(crate) type Lookup<T: Config> =
		StorageMap<_, Twox64Concat, Vec<u8>, TaskAddress<T::BlockNumber>>;

	/// Storage version of the pallet.
	///
	/// New networks start with last version.
	#[pallet::storage]
	pub(crate) type StorageVersion<T> = StorageValue<_, Releases, ValueQuery>;

	/// Events type.
	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// Scheduled some task. \[when, index\]
		Scheduled(T::BlockNumber, u32),
		/// Canceled some task. \[when, index\]
		Canceled(T::BlockNumber, u32),
		/// Dispatched some task. \[task, id, result\]
		Dispatched(TaskAddress<T::BlockNumber>, Option<Vec<u8>>, DispatchResult),
		/// The call for the provided hash was not found so the task has been aborted.
		CallLookupFailed(TaskAddress<T::BlockNumber>, Option<Vec<u8>>, LookupError),
	}

	#[pallet::error]
	pub enum Error<T> {
		/// Failed to schedule a call
		FailedToSchedule,
		/// Cannot find the scheduled call.
		NotFound,
		/// Given target block number is in the past.
		TargetBlockNumberInPast,
		/// Reschedule failed because it does not change scheduled time.
		RescheduleNoChange,
	}

	#[pallet::genesis_config]
	pub struct GenesisConfig;

	#[cfg(feature = "std")]
	impl Default for GenesisConfig {
		fn default() -> Self {
			Self
		}
	}

	#[pallet::genesis_build]
	impl<T: Config> GenesisBuild<T> for GenesisConfig {
		fn build(&self) {
			StorageVersion::<T>::put(Releases::V2);
		}
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		/// Execute the scheduled calls
		///
		/// # <weight>
		/// - S = Number of already scheduled calls
		/// - N = Named scheduled calls
		/// - P = Periodic Calls
		/// - Base Weight: 9.243 + 23.45 * S µs
		/// - DB Weight:
		///     - Read: Agenda + Lookup * N + Agenda(Future) * P
		///     - Write: Agenda + Lookup * N  + Agenda(future) * P
		/// # </weight>
		fn on_initialize(now: T::BlockNumber) -> Weight {
			let mut base_weight: Weight = T::DbWeight::get().reads_writes(1, 2); // Agenda + Agenda(next)
			let limit = T::MaximumWeight::get();
			let mut queued = Agenda::<T>::take(now)
				.into_iter()
				.enumerate()
				.filter_map(|(index, s)| {
					let ScheduledV3 { maybe_id, priority, call, maybe_periodic, origin, _phantom } = s?;
					let (call, maybe_completed) = call.resolved::<T::Preimages>();
					if let Some(completed) = maybe_completed {
						// TODO Benchmark individually.
						base_weight.saturating_accrue(T::DbWeight::get().reads_writes(2, 2));
						T::Preimages::unrequest_preimage(&completed);
					}
					let s = ScheduledV3 { maybe_id, priority, call, maybe_periodic, origin, _phantom };
					Some((index as u32, s))
				})
				.collect::<Vec<_>>();
			if queued.len() as u32 > T::MaxScheduledPerBlock::get() {
				log::warn!(
					target: "runtime::scheduler",
					"Warning: This block has more items queued in Scheduler than \
					expected from the runtime configuration. An update might be needed."
				);
			}
			queued.sort_by_key(|(_, s)| s.priority);
			let next = now + One::one();
			let mut total_weight: Weight = 0;
			queued
				.into_iter()
				.enumerate()
				.scan(base_weight, |cumulative_weight, (order, (index, s))| {

					if let Some(c) = s.call.as_value() {
						*cumulative_weight =
							cumulative_weight.saturating_add(c.get_dispatch_info().weight);
						let origin =
							<<T as Config>::Origin as From<T::PalletsOrigin>>::from(s.origin.clone())
								.into();

						if ensure_signed(origin).is_ok() {
							// AccountData for inner call origin accountdata.
							*cumulative_weight =
								cumulative_weight.saturating_add(T::DbWeight::get().reads_writes(1, 1));
						}
					}

					if s.maybe_id.is_some() {
						// Remove/Modify Lookup
						*cumulative_weight =
							cumulative_weight.saturating_add(T::DbWeight::get().writes(1));
					}
					if s.maybe_periodic.is_some() {
						// Read/Write Agenda for future block
						*cumulative_weight =
							cumulative_weight.saturating_add(T::DbWeight::get().reads_writes(1, 1));
					}

					Some((order, index, *cumulative_weight, s))
				})
				.filter_map(|(order, index, cumulative_weight, mut s)| {
					let call = match s.call.as_value().cloned() {
						Some(c) => c,
						None => return Some(Some(s)),
					};
					// We allow a scheduled call if any is true:
					// - It's priority is `HARD_DEADLINE`
					// - It does not push the weight past the limit.
					// - It is the first item in the schedule
					if s.priority <= schedule::HARD_DEADLINE ||
						cumulative_weight <= limit ||
						order == 0
					{
						let r = call.dispatch(s.origin.clone().into());
						let maybe_id = s.maybe_id.clone();
						if let &Some((period, count)) = &s.maybe_periodic {
							if count > 1 {
								s.maybe_periodic = Some((period, count - 1));
							} else {
								s.maybe_periodic = None;
							}
							let next = now + period;
							// If scheduled is named, place it's information in `Lookup`
							if let Some(ref id) = s.maybe_id {
								let next_index = Agenda::<T>::decode_len(now + period).unwrap_or(0);
								Lookup::<T>::insert(id, (next, next_index as u32));
							}
							Agenda::<T>::append(next, Some(s));
						} else {
							if let Some(ref id) = s.maybe_id {
								Lookup::<T>::remove(id);
							}
						}
						Self::deposit_event(Event::Dispatched(
							(now, index),
							maybe_id,
							r.map(|_| ()).map_err(|e| e.error),
						));
						total_weight = cumulative_weight;
						None
					} else {
						Some(Some(s))
					}
				})
				.for_each(|unused| Agenda::<T>::append(next, unused));

			total_weight
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Anonymously schedule a task.
		///
		/// # <weight>
		/// - S = Number of already scheduled calls
		/// - Base Weight: 22.29 + .126 * S µs
		/// - DB Weight:
		///     - Read: Agenda
		///     - Write: Agenda
		/// - Will use base weight of 25 which should be good for up to 30 scheduled calls
		/// # </weight>
		#[pallet::weight(<T as Config>::WeightInfo::schedule(T::MaxScheduledPerBlock::get()))]
		pub fn schedule(
			origin: OriginFor<T>,
			when: T::BlockNumber,
			maybe_periodic: Option<schedule::Period<T::BlockNumber>>,
			priority: schedule::Priority,
			call: Box<CallOrHashOf<T>>,
		) -> DispatchResult {
			T::ScheduleOrigin::ensure_origin(origin.clone())?;
			let origin = <T as Config>::Origin::from(origin);
			Self::do_schedule(
				DispatchTime::At(when),
				maybe_periodic,
				priority,
				origin.caller().clone(),
				*call,
			)?;
			Ok(())
		}

		/// Cancel an anonymously scheduled task.
		///
		/// # <weight>
		/// - S = Number of already scheduled calls
		/// - Base Weight: 22.15 + 2.869 * S µs
		/// - DB Weight:
		///     - Read: Agenda
		///     - Write: Agenda, Lookup
		/// - Will use base weight of 100 which should be good for up to 30 scheduled calls
		/// # </weight>
		#[pallet::weight(<T as Config>::WeightInfo::cancel(T::MaxScheduledPerBlock::get()))]
		pub fn cancel(origin: OriginFor<T>, when: T::BlockNumber, index: u32) -> DispatchResult {
			T::ScheduleOrigin::ensure_origin(origin.clone())?;
			let origin = <T as Config>::Origin::from(origin);
			Self::do_cancel(Some(origin.caller().clone()), (when, index))?;
			Ok(())
		}

		/// Schedule a named task.
		///
		/// # <weight>
		/// - S = Number of already scheduled calls
		/// - Base Weight: 29.6 + .159 * S µs
		/// - DB Weight:
		///     - Read: Agenda, Lookup
		///     - Write: Agenda, Lookup
		/// - Will use base weight of 35 which should be good for more than 30 scheduled calls
		/// # </weight>
		#[pallet::weight(<T as Config>::WeightInfo::schedule_named(T::MaxScheduledPerBlock::get()))]
		pub fn schedule_named(
			origin: OriginFor<T>,
			id: Vec<u8>,
			when: T::BlockNumber,
			maybe_periodic: Option<schedule::Period<T::BlockNumber>>,
			priority: schedule::Priority,
			call: Box<CallOrHashOf<T>>,
		) -> DispatchResult {
			T::ScheduleOrigin::ensure_origin(origin.clone())?;
			let origin = <T as Config>::Origin::from(origin);
			Self::do_schedule_named(
				id,
				DispatchTime::At(when),
				maybe_periodic,
				priority,
				origin.caller().clone(),
				*call,
			)?;
			Ok(())
		}

		/// Cancel a named scheduled task.
		///
		/// # <weight>
		/// - S = Number of already scheduled calls
		/// - Base Weight: 24.91 + 2.907 * S µs
		/// - DB Weight:
		///     - Read: Agenda, Lookup
		///     - Write: Agenda, Lookup
		/// - Will use base weight of 100 which should be good for up to 30 scheduled calls
		/// # </weight>
		#[pallet::weight(<T as Config>::WeightInfo::cancel_named(T::MaxScheduledPerBlock::get()))]
		pub fn cancel_named(origin: OriginFor<T>, id: Vec<u8>) -> DispatchResult {
			T::ScheduleOrigin::ensure_origin(origin.clone())?;
			let origin = <T as Config>::Origin::from(origin);
			Self::do_cancel_named(Some(origin.caller().clone()), id)?;
			Ok(())
		}

		/// Anonymously schedule a task after a delay.
		///
		/// # <weight>
		/// Same as [`schedule`].
		/// # </weight>
		#[pallet::weight(<T as Config>::WeightInfo::schedule(T::MaxScheduledPerBlock::get()))]
		pub fn schedule_after(
			origin: OriginFor<T>,
			after: T::BlockNumber,
			maybe_periodic: Option<schedule::Period<T::BlockNumber>>,
			priority: schedule::Priority,
			call: Box<CallOrHashOf<T>>,
		) -> DispatchResult {
			T::ScheduleOrigin::ensure_origin(origin.clone())?;
			let origin = <T as Config>::Origin::from(origin);
			Self::do_schedule(
				DispatchTime::After(after),
				maybe_periodic,
				priority,
				origin.caller().clone(),
				*call,
			)?;
			Ok(())
		}

		/// Schedule a named task after a delay.
		///
		/// # <weight>
		/// Same as [`schedule_named`](Self::schedule_named).
		/// # </weight>
		#[pallet::weight(<T as Config>::WeightInfo::schedule_named(T::MaxScheduledPerBlock::get()))]
		pub fn schedule_named_after(
			origin: OriginFor<T>,
			id: Vec<u8>,
			after: T::BlockNumber,
			maybe_periodic: Option<schedule::Period<T::BlockNumber>>,
			priority: schedule::Priority,
			call: Box<CallOrHashOf<T>>,
		) -> DispatchResult {
			T::ScheduleOrigin::ensure_origin(origin.clone())?;
			let origin = <T as Config>::Origin::from(origin);
			Self::do_schedule_named(
				id,
				DispatchTime::After(after),
				maybe_periodic,
				priority,
				origin.caller().clone(),
				*call,
			)?;
			Ok(())
		}
	}
}

impl<T: Config> Pallet<T> {
	/// Migrate storage format from V1 to V3.
	/// Return true if migration is performed.
	pub fn migrate_v1_to_v3() -> bool {
		if StorageVersion::<T>::get() == Releases::V1 {
			StorageVersion::<T>::put(Releases::V2);

			Agenda::<T>::translate::<
				Vec<Option<ScheduledV1<<T as Config>::Call, T::BlockNumber>>>,
				_,
			>(|_, agenda| {
				Some(
					agenda
						.into_iter()
						.map(|schedule| {
							schedule.map(|schedule| ScheduledV3 {
								maybe_id: schedule.maybe_id,
								priority: schedule.priority,
								call: schedule.call.into(),
								maybe_periodic: schedule.maybe_periodic,
								origin: system::RawOrigin::Root.into(),
								_phantom: Default::default(),
							})
						})
						.collect::<Vec<_>>(),
				)
			});

			true
		} else {
			false
		}
	}

	/// Migrate storage format from V2 to V3.
	/// Return true if migration is performed.
	pub fn migrate_v2_to_v3() -> bool {
		if StorageVersion::<T>::get() == Releases::V1 {
			StorageVersion::<T>::put(Releases::V2);

			Agenda::<T>::translate::<
				Vec<Option<ScheduledV2Of<T>>>,
				_,
			>(|_, agenda| {
				Some(
					agenda
						.into_iter()
						.map(|schedule| {
							schedule.map(|schedule| ScheduledV3 {
								maybe_id: schedule.maybe_id,
								priority: schedule.priority,
								call: schedule.call.into(),
								maybe_periodic: schedule.maybe_periodic,
								origin: system::RawOrigin::Root.into(),
								_phantom: Default::default(),
							})
						})
						.collect::<Vec<_>>(),
				)
			});

			true
		} else {
			false
		}
	}

	/// Helper to migrate scheduler when the pallet origin type has changed.
	pub fn migrate_origin<OldOrigin: Into<T::PalletsOrigin> + codec::Decode>() {
		Agenda::<T>::translate::<
			Vec<Option<Scheduled<CallOrHashOf<T>, T::BlockNumber, OldOrigin, T::AccountId>>>,
			_
		>(|_, agenda| {
			Some(
				agenda
					.into_iter()
					.map(|schedule| {
						schedule.map(|schedule| Scheduled {
							maybe_id: schedule.maybe_id,
							priority: schedule.priority,
							call: schedule.call,
							maybe_periodic: schedule.maybe_periodic,
							origin: schedule.origin.into(),
							_phantom: Default::default(),
						})
					})
					.collect::<Vec<_>>(),
			)
		});
	}

	fn resolve_time(when: DispatchTime<T::BlockNumber>) -> Result<T::BlockNumber, DispatchError> {
		let now = frame_system::Pallet::<T>::block_number();

		let when = match when {
			DispatchTime::At(x) => x,
			// The current block has already completed it's scheduled tasks, so
			// Schedule the task at lest one block after this current block.
			DispatchTime::After(x) => now.saturating_add(x).saturating_add(One::one()),
		};

		if when <= now {
			return Err(Error::<T>::TargetBlockNumberInPast.into())
		}

		Ok(when)
	}

	fn do_schedule(
		when: DispatchTime<T::BlockNumber>,
		maybe_periodic: Option<schedule::Period<T::BlockNumber>>,
		priority: schedule::Priority,
		origin: T::PalletsOrigin,
		call: CallOrHashOf<T>,
	) -> Result<TaskAddress<T::BlockNumber>, DispatchError> {
		let when = Self::resolve_time(when)?;
		call.ensure_requested::<T::Preimages>();

		// sanitize maybe_periodic
		let maybe_periodic = maybe_periodic
			.filter(|p| p.1 > 1 && !p.0.is_zero())
			// Remove one from the number of repetitions since we will schedule one now.
			.map(|(p, c)| (p, c - 1));
		let s = Some(Scheduled {
			maybe_id: None,
			priority,
			call,
			maybe_periodic,
			origin,
			_phantom: PhantomData::<T::AccountId>::default(),
		});
		Agenda::<T>::append(when, s);
		let index = Agenda::<T>::decode_len(when).unwrap_or(1) as u32 - 1;
		if index > T::MaxScheduledPerBlock::get() {
			log::warn!(
				target: "runtime::scheduler",
				"Warning: There are more items queued in the Scheduler than \
				expected from the runtime configuration. An update might be needed.",
			);
		}
		Self::deposit_event(Event::Scheduled(when, index));

		Ok((when, index))
	}

	fn do_cancel(
		origin: Option<T::PalletsOrigin>,
		(when, index): TaskAddress<T::BlockNumber>,
	) -> Result<(), DispatchError> {
		let scheduled = Agenda::<T>::try_mutate(when, |agenda| {
			agenda.get_mut(index as usize).map_or(
				Ok(None),
				|s| -> Result<Option<Scheduled<_, _, _, _>>, DispatchError> {
					if let (Some(ref o), Some(ref s)) = (origin, s.borrow()) {
						if matches!(
							T::OriginPrivilegeCmp::cmp_privilege(o, &s.origin),
							Some(Ordering::Less) | None
						) {
							return Err(BadOrigin.into())
						}
					};
					Ok(s.take())
				},
			)
		})?;
		if let Some(s) = scheduled {
			s.call.ensure_unrequested::<T::Preimages>();
			if let Some(id) = s.maybe_id {
				Lookup::<T>::remove(id);
			}
			Self::deposit_event(Event::Canceled(when, index));
			Ok(())
		} else {
			Err(Error::<T>::NotFound)?
		}
	}

	fn do_reschedule(
		(when, index): TaskAddress<T::BlockNumber>,
		new_time: DispatchTime<T::BlockNumber>,
	) -> Result<TaskAddress<T::BlockNumber>, DispatchError> {
		let new_time = Self::resolve_time(new_time)?;

		if new_time == when {
			return Err(Error::<T>::RescheduleNoChange.into())
		}

		Agenda::<T>::try_mutate(when, |agenda| -> DispatchResult {
			let task = agenda.get_mut(index as usize).ok_or(Error::<T>::NotFound)?;
			let task = task.take().ok_or(Error::<T>::NotFound)?;
			Agenda::<T>::append(new_time, Some(task));
			Ok(())
		})?;

		let new_index = Agenda::<T>::decode_len(new_time).unwrap_or(1) as u32 - 1;
		Self::deposit_event(Event::Canceled(when, index));
		Self::deposit_event(Event::Scheduled(new_time, new_index));

		Ok((new_time, new_index))
	}

	fn do_schedule_named(
		id: Vec<u8>,
		when: DispatchTime<T::BlockNumber>,
		maybe_periodic: Option<schedule::Period<T::BlockNumber>>,
		priority: schedule::Priority,
		origin: T::PalletsOrigin,
		call: CallOrHashOf<T>,
	) -> Result<TaskAddress<T::BlockNumber>, DispatchError> {
		// ensure id it is unique
		if Lookup::<T>::contains_key(&id) {
			return Err(Error::<T>::FailedToSchedule)?
		}

		let when = Self::resolve_time(when)?;

		call.ensure_requested::<T::Preimages>();

		// sanitize maybe_periodic
		let maybe_periodic = maybe_periodic
			.filter(|p| p.1 > 1 && !p.0.is_zero())
			// Remove one from the number of repetitions since we will schedule one now.
			.map(|(p, c)| (p, c - 1));

		let s = Scheduled {
			maybe_id: Some(id.clone()),
			priority,
			call,
			maybe_periodic,
			origin,
			_phantom: Default::default(),
		};
		Agenda::<T>::append(when, Some(s));
		let index = Agenda::<T>::decode_len(when).unwrap_or(1) as u32 - 1;
		if index > T::MaxScheduledPerBlock::get() {
			log::warn!(
				target: "runtime::scheduler",
				"Warning: There are more items queued in the Scheduler than \
				expected from the runtime configuration. An update might be needed.",
			);
		}
		let address = (when, index);
		Lookup::<T>::insert(&id, &address);
		Self::deposit_event(Event::Scheduled(when, index));

		Ok(address)
	}

	fn do_cancel_named(origin: Option<T::PalletsOrigin>, id: Vec<u8>) -> DispatchResult {
		Lookup::<T>::try_mutate_exists(id, |lookup| -> DispatchResult {
			if let Some((when, index)) = lookup.take() {
				let i = index as usize;
				Agenda::<T>::try_mutate(when, |agenda| -> DispatchResult {
					if let Some(s) = agenda.get_mut(i) {
						if let (Some(ref o), Some(ref s)) = (origin, s.borrow()) {
							if matches!(
								T::OriginPrivilegeCmp::cmp_privilege(o, &s.origin),
								Some(Ordering::Less) | None
							) {
								return Err(BadOrigin.into())
							}
							s.call.ensure_unrequested::<T::Preimages>();
						}
						*s = None;
					}
					Ok(())
				})?;
				Self::deposit_event(Event::Canceled(when, index));
				Ok(())
			} else {
				Err(Error::<T>::NotFound)?
			}
		})
	}

	fn do_reschedule_named(
		id: Vec<u8>,
		new_time: DispatchTime<T::BlockNumber>,
	) -> Result<TaskAddress<T::BlockNumber>, DispatchError> {
		let new_time = Self::resolve_time(new_time)?;

		Lookup::<T>::try_mutate_exists(
			id,
			|lookup| -> Result<TaskAddress<T::BlockNumber>, DispatchError> {
				let (when, index) = lookup.ok_or(Error::<T>::NotFound)?;

				if new_time == when {
					return Err(Error::<T>::RescheduleNoChange.into())
				}

				Agenda::<T>::try_mutate(when, |agenda| -> DispatchResult {
					let task = agenda.get_mut(index as usize).ok_or(Error::<T>::NotFound)?;
					let task = task.take().ok_or(Error::<T>::NotFound)?;
					Agenda::<T>::append(new_time, Some(task));

					Ok(())
				})?;

				let new_index = Agenda::<T>::decode_len(new_time).unwrap_or(1) as u32 - 1;
				Self::deposit_event(Event::Canceled(when, index));
				Self::deposit_event(Event::Scheduled(new_time, new_index));

				*lookup = Some((new_time, new_index));

				Ok((new_time, new_index))
			},
		)
	}
}

impl<T: Config> schedule::v2::Anon<T::BlockNumber, <T as Config>::Call, T::PalletsOrigin>
	for Pallet<T>
{
	type Address = TaskAddress<T::BlockNumber>;
	type Hash = T::Hash;

	fn schedule(
		when: DispatchTime<T::BlockNumber>,
		maybe_periodic: Option<schedule::Period<T::BlockNumber>>,
		priority: schedule::Priority,
		origin: T::PalletsOrigin,
		call: CallOrHashOf<T>,
	) -> Result<Self::Address, DispatchError> {
		Self::do_schedule(when, maybe_periodic, priority, origin, call)
	}

	fn cancel((when, index): Self::Address) -> Result<(), ()> {
		Self::do_cancel(None, (when, index)).map_err(|_| ())
	}

	fn reschedule(
		address: Self::Address,
		when: DispatchTime<T::BlockNumber>,
	) -> Result<Self::Address, DispatchError> {
		Self::do_reschedule(address, when)
	}

	fn next_dispatch_time((when, index): Self::Address) -> Result<T::BlockNumber, ()> {
		Agenda::<T>::get(when).get(index as usize).ok_or(()).map(|_| when)
	}
}

impl<T: Config> schedule::v2::Named<T::BlockNumber, <T as Config>::Call, T::PalletsOrigin>
	for Pallet<T>
{
	type Address = TaskAddress<T::BlockNumber>;
	type Hash = T::Hash;

	fn schedule_named(
		id: Vec<u8>,
		when: DispatchTime<T::BlockNumber>,
		maybe_periodic: Option<schedule::Period<T::BlockNumber>>,
		priority: schedule::Priority,
		origin: T::PalletsOrigin,
		call: CallOrHashOf<T>,
	) -> Result<Self::Address, ()> {
		Self::do_schedule_named(id, when, maybe_periodic, priority, origin, call).map_err(|_| ())
	}

	fn cancel_named(id: Vec<u8>) -> Result<(), ()> {
		Self::do_cancel_named(None, id).map_err(|_| ())
	}

	fn reschedule_named(
		id: Vec<u8>,
		when: DispatchTime<T::BlockNumber>,
	) -> Result<Self::Address, DispatchError> {
		Self::do_reschedule_named(id, when)
	}

	fn next_dispatch_time(id: Vec<u8>) -> Result<T::BlockNumber, ()> {
		Lookup::<T>::get(id)
			.and_then(|(when, index)| Agenda::<T>::get(when).get(index as usize).map(|_| when))
			.ok_or(())
	}
}
