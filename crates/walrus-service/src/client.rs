// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::{collections::HashMap, time::Instant};

use anyhow::{anyhow, Result};
use futures::Future;
use reqwest::{Client as ReqwestClient, ClientBuilder};
use tokio::time::Duration;
use tracing::Instrument;
use walrus_core::{
    encoding::{BlobDecoder, EncodingAxis, EncodingConfig, Sliver, SliverPair},
    metadata::VerifiedBlobMetadataWithId,
    BlobId,
    SignedStorageConfirmation,
    Sliver as SliverEnum,
};
use walrus_sui::types::{Committee, StorageNode};

mod communication;
mod config;
mod error;
mod utils;

pub use self::config::Config;
use self::{
    communication::{NodeCommunication, NodeResult},
    error::SliverRetrieveError,
    utils::WeightedFutures,
};

/// A client to communicate with Walrus shards and storage nodes.
#[derive(Debug)]
pub struct Client {
    client: ReqwestClient,
    // INV: committee.total_weight > 0
    committee: Committee,
    concurrent_requests: usize,
    encoding_config: EncodingConfig,
}

impl Client {
    /// Creates a new client starting from a config file.
    ///
    /// # Panics
    ///
    /// Panics if `config.committee.total_weight == 0`.
    // TODO(giac): Remove once fetching the configuration from the chain is available.
    pub fn new(config: Config) -> Result<Self> {
        let encoding_config = config.encoding_config();
        let committee = config.committee;
        assert!(committee.total_weight != 0);
        let client = ClientBuilder::new()
            .timeout(config.connection_timeout)
            .build()?;
        Ok(Self {
            client,
            committee,
            concurrent_requests: config.concurrent_requests,
            encoding_config,
        })
    }

    /// Encodes and stores a blob into Walrus by sending sliver pairs to at least 2f+1 shards.
    #[tracing::instrument(skip_all, fields(blob_id_prefix))]
    pub async fn store_blob(
        &self,
        blob: &[u8],
    ) -> Result<(VerifiedBlobMetadataWithId, Vec<SignedStorageConfirmation>)> {
        let (pairs, metadata) = self
            .encoding_config
            .get_blob_encoder(blob)?
            .encode_with_metadata();
        tracing::Span::current().record("blob_id_prefix", truncate_blob_id(metadata.blob_id()));
        tracing::debug!(blob_id = %metadata.blob_id(), "computed blob pairs and metadata");
        let pairs_per_node = self.pairs_per_node(metadata.blob_id(), pairs);
        let comms = self.node_communications();
        let mut requests = WeightedFutures::new(
            comms
                .iter()
                .zip(pairs_per_node.into_iter())
                .map(|(n, p)| n.store_metadata_and_pairs(&metadata, p)),
        );
        let start = Instant::now();
        requests
            .execute_weight(self.committee.quorum_threshold(), self.concurrent_requests)
            .await;
        // Double the execution time, with a minimum of 100 ms. This gives the client time to
        // collect more storage confirmations.
        requests
            .execute_time(
                start.elapsed() + Duration::from_millis(100),
                self.concurrent_requests,
            )
            .await;
        let results = requests.into_results();
        let valid_confirmations = results
            .into_iter()
            .filter_map(|NodeResult(_, _, node, result)| {
                result
                    .map_err(|err| {
                        tracing::error!(?node, ?err, "storing metadata and pairs failed")
                    })
                    .ok()
            })
            .collect();
        Ok((metadata, valid_confirmations))
    }

    /// Reconstructs the blob by reading slivers from Walrus shards.
    #[tracing::instrument(skip_all, fields(blob_id_prefix = truncate_blob_id(blob_id)))]
    pub async fn read_blob<T>(&self, blob_id: &BlobId) -> Result<Vec<u8>>
    where
        T: EncodingAxis,
        Sliver<T>: TryFrom<SliverEnum>,
    {
        tracing::debug!(%blob_id, "starting to read blob");
        let metadata = self.retrieve_metadata(blob_id).await?;
        self.request_slivers_and_decode::<T>(&metadata).await
    }

    async fn request_slivers_and_decode<T>(
        &self,
        metadata: &VerifiedBlobMetadataWithId,
    ) -> Result<Vec<u8>>
    where
        T: EncodingAxis,
        Sliver<T>: TryFrom<SliverEnum>,
    {
        // TODO(giac): optimize by reading first from the shards that have the systematic part of
        // the encoding.
        let comms = self.node_communications();
        // Create requests to get all slivers from all nodes.
        let futures = comms.iter().flat_map(|n| {
            n.node.shard_ids.iter().map(|s| {
                n.retrieve_verified_sliver::<T>(metadata, *s)
                    .instrument(n.span.clone())
            })
        });
        let mut decoder = self
            .encoding_config
            .get_blob_decoder::<T>(metadata.metadata().unencoded_length.try_into()?)?;
        // Get the first ~1/3 or ~2/3 of slivers directly, and decode with these.
        let mut requests = WeightedFutures::new(futures);
        requests
            .execute_weight(
                self.encoding_config.n_source_symbols::<T>().get().into(),
                self.concurrent_requests,
            )
            .await;

        let slivers = requests
            .take_results()
            .into_iter()
            .filter_map(|NodeResult(_, _, node, result)| {
                result
                    .map_err(|err| {
                        tracing::error!(?node, ?err, "retrieving sliver failed");
                    })
                    .ok()
            })
            .collect::<Vec<_>>();

        if let Some((blob, _meta)) = decoder.decode_and_verify(metadata.blob_id(), slivers)? {
            // We have enough to decode the blob.
            Ok(blob)
        } else {
            // We were not able to decode. Keep requesting slivers and try decoding as soon as every
            // new sliver is received.
            self.decode_sliver_by_sliver(&mut requests, &mut decoder, metadata.blob_id())
                .await
        }
    }

    /// Decodes the blob of given blob ID by requesting slivers and trying to decode at each new
    /// sliver it receives.
    #[tracing::instrument(skip_all)]
    async fn decode_sliver_by_sliver<'a, I, Fut, T>(
        &self,
        requests: &mut WeightedFutures<I, Fut, NodeResult<Sliver<T>, SliverRetrieveError>>,
        decoder: &mut BlobDecoder<'a, T>,
        blob_id: &BlobId,
    ) -> Result<Vec<u8>>
    where
        T: EncodingAxis,
        I: Iterator<Item = Fut>,
        Fut: Future<Output = NodeResult<Sliver<T>, SliverRetrieveError>>,
    {
        while let Some(NodeResult(_, _, node, result)) =
            requests.next(self.concurrent_requests).await
        {
            match result {
                Ok(sliver) => {
                    let result = decoder.decode_and_verify(blob_id, [sliver])?;
                    if let Some((blob, _meta)) = result {
                        return Ok(blob);
                    }
                }
                Err(err) => {
                    tracing::error!(?node, ?err, "retrieving sliver failed");
                }
            }
        }
        // We have exhausted all the slivers but were not able to reconstruct the blob.
        Err(anyhow!(
            "not enough slivers were received to reconstruct the blob"
        ))
    }

    /// Requests the metadata from all storage nodes, and keeps the first that is correctly verified
    /// against the blob ID.
    pub async fn retrieve_metadata(&self, blob_id: &BlobId) -> Result<VerifiedBlobMetadataWithId> {
        let comms = self.node_communications();
        let futures = comms.iter().map(|n| {
            n.retrieve_verified_metadata(blob_id)
                .instrument(n.span.clone())
        });
        // Wait until the first request succeeds
        let mut requests = WeightedFutures::new(futures);
        requests.execute_weight(1, self.concurrent_requests).await;
        let metadata = requests.take_inner_ok().pop().ok_or(anyhow!(
            "could not retrieve the metadata from the storage nodes"
        ))?;
        Ok(metadata)
    }

    /// Builds a [`NodeCommunication`] object for the given storage node.
    fn new_node_communication<'a>(&'a self, node: &'a StorageNode) -> NodeCommunication {
        NodeCommunication::new(
            self.committee.epoch,
            &self.client,
            node,
            &self.encoding_config,
        )
    }

    fn node_communications(&self) -> Vec<NodeCommunication> {
        self.committee
            .members
            .iter()
            .map(|n| self.new_node_communication(n))
            .collect()
    }

    /// Maps the sliver pairs to the node that holds their shard.
    fn pairs_per_node(&self, blob_id: &BlobId, pairs: Vec<SliverPair>) -> Vec<Vec<SliverPair>> {
        let mut pairs_per_node = Vec::with_capacity(self.committee.members.len());
        pairs_per_node.extend(
            self.committee
                .members
                .iter()
                .map(|n| Vec::with_capacity(n.shard_ids.len())),
        );
        let shard_to_node = self
            .committee
            .members
            .iter()
            .enumerate()
            .flat_map(|(idx, m)| m.shard_ids.iter().map(move |s| (*s, idx)))
            .collect::<HashMap<_, _>>();
        pairs.into_iter().for_each(|p| {
            pairs_per_node[shard_to_node[&p.index().to_shard_index(
                self.committee
                    .total_weight
                    .try_into()
                    .expect("checked in constructor"),
                blob_id,
            )]]
                .push(p)
        });
        pairs_per_node
    }
}

/// Returns the 8 characters of the blob ID.
fn truncate_blob_id(blob_id: &BlobId) -> String {
    let mut blob_id_string = blob_id.to_string();
    blob_id_string.truncate(8);
    blob_id_string
}
