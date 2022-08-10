// This file is part of Substrate.

// Copyright (C) 2021-2022 Parity Technologies (UK) Ltd.
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

use crate::{build_executor, ensure_matching_spec, extract_code, full_extensions, local_spec, parse, state_machine_call_with_proof, SharedParams, LOG_TARGET, twox_128};
use jsonrpsee::{
	core::client::{Subscription, SubscriptionClientT},
	ws_client::WsClientBuilder,
};
use parity_scale_codec::Decode;
use remote_externalities::{rpc_api, Builder, Mode, OnlineConfig};
use sc_executor::NativeExecutionDispatch;
use sc_service::Configuration;
use sp_core::H256;
use sp_runtime::traits::{Block as BlockT, Header, NumberFor};
use std::{fmt::Debug, str::FromStr};

const SUB: &str = "chain_subscribeFinalizedHeads";
const UN_SUB: &str = "chain_unsubscribeFinalizedHeads";

/// Configurations of the [`Command::FollowChain`].
#[derive(Debug, Clone, clap::Parser)]
pub struct FollowChainCmd {
	/// The url to connect to.
	#[clap(short, long, parse(try_from_str = parse::url))]
	uri: String,

	#[clap(long)]
	checking: bool,
}

pub(crate) async fn follow_chain<Block, ExecDispatch>(
	shared: SharedParams,
	command: FollowChainCmd,
	config: Configuration,
) -> sc_cli::Result<()>
where
	Block: BlockT<Hash = H256> + serde::de::DeserializeOwned,
	Block::Hash: FromStr,
	Block::Header: serde::de::DeserializeOwned,
	<Block::Hash as FromStr>::Err: Debug,
	NumberFor<Block>: FromStr,
	<NumberFor<Block> as FromStr>::Err: Debug,
	ExecDispatch: NativeExecutionDispatch + 'static,
{
	let mut maybe_state_ext = None;

	let client = WsClientBuilder::default()
		.connection_timeout(std::time::Duration::new(20, 0))
		.max_notifs_per_subscription(1024)
		.max_request_body_size(u32::MAX)
		.build(&command.uri)
		.await
		.unwrap();

	log::info!(target: LOG_TARGET, "subscribing to {:?} / {:?}", SUB, UN_SUB);
	let mut subscription: Subscription<Block::Header> =
		client.subscribe(SUB, None, UN_SUB).await.unwrap();

	let (code_key, code) = extract_code(&config.chain_spec)?;
	let executor = build_executor::<ExecDispatch>(&shared, &config);
	let execution = shared.execution;

	loop {
		let header = match subscription.next().await {
			Some(Ok(header)) => header,
			None => {
				log::warn!("subscription closed");
				break
			},
			Some(Err(why)) => {
				log::warn!("subscription returned error: {:?}. Probably decoding has failed.", why);
				continue
			},
		};

		let hash = header.hash();
		let number = header.number();

		let block = rpc_api::get_block::<Block, _>(&command.uri, hash).await.unwrap();

		log::error!("number: {:?}, hash: {:?}", block.header().number(), block.header().hash());
		log::error!("state root: {:?}", block.header().state_root());
		log::error!("ext root: {:?}", block.header().extrinsics_root());
		log::error!("#digests: {:?}", block.header().digest().logs.len());

		log::debug!(
			target: LOG_TARGET,
			"new block event: {:?} => {:?}, extrinsics: {}, storage root: {:?}",
			hash,
			number,
			block.extrinsics().len(),
			header.state_root()
		);

		// create an ext at the state of this block, whatever is the first subscription event.
		if maybe_state_ext.is_none() {
			let mut builder = Builder::<Block>::new().mode(Mode::Online(OnlineConfig {
				transport: command.uri.clone().into(),
				at: Some(*header.parent_hash()),
				scrape_children: true,
				..Default::default()
			})).inject_hashed_key(
					&[twox_128(b"System"), twox_128(b"LastRuntimeUpgrade")].concat(),
				).inject_default_child_tree_prefix();

			let new_ext = builder
				// .inject_hashed_key_value(&[(code_key.clone(), code.clone())])
				.build()
				.await?;
			log::info!(
				target: LOG_TARGET,
				"initialized state externalities at {:?}, storage root {:?}",
				number,
				new_ext.as_backend().root()
			);

			let (expected_spec_name, expected_spec_version, spec_state_version) =
				local_spec::<Block, ExecDispatch>(&new_ext, &executor);
			ensure_matching_spec::<Block>(
				command.uri.clone(),
				expected_spec_name,
				expected_spec_version,
				shared.no_spec_name_check,
			)
			.await;

			maybe_state_ext = Some((new_ext, spec_state_version));
		}

		let (state_ext, spec_state_version) =
			maybe_state_ext.as_mut().expect("state_ext either existed or was just created");

		let method = if command.checking {
			"Core_execute_block"
		} else {
			"TryRuntime_execute_block_no_check"
		};

		let (mut header, extrinsics) = block.deconstruct();
		header.digest_mut().pop();
		let block = Block::new(header, extrinsics);

		log::error!("number: {:?}, hash: {:?}", block.header().number(), block.header().hash());
		log::error!("state root: {:?}", block.header().state_root());
		log::error!("ext root: {:?}", block.header().extrinsics_root());
		log::error!("#digests: {:?}", block.header().digest().logs.len());

		println!("Using method {}", method);

		let (mut changes, encoded_result) = state_machine_call_with_proof::<Block, ExecDispatch>(
			state_ext,
			&executor,
			execution,
			method,
			block.encode().as_ref(),
			full_extensions(),
		)?;

		let consumed_weight = <u128 as Decode>::decode(&mut &*encoded_result)
			.map_err(|e| format!("failed to decode weight: {:?}", e))?;

		let storage_changes = changes
			.drain_storage_changes(
				&state_ext.backend,
				&mut Default::default(),
				// Note that in case a block contains a runtime upgrade,
				// state version could potentially be incorrect here,
				// this is very niche and would only result in unaligned
				// roots, so this use case is ignored for now.
				*spec_state_version,
			)
			.unwrap();
		state_ext.backend.apply_transaction(
			storage_changes.transaction_storage_root,
			storage_changes.transaction,
		);

		log::info!(
			target: LOG_TARGET,
			"executed block {}, consumed weight {}, new storage root {:?}",
			number,
			consumed_weight,
			state_ext.as_backend().root(),
		);
	}

	log::error!(target: LOG_TARGET, "ws subscription must have terminated.");
	Ok(())
}
