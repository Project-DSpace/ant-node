//! Listings for the envelope chunks this node stores.
//!
//! A client PUT of an envelope carries its payment proof, so the node finds the
//! payment on chain and records the listing as it stores the chunk. A chunk that
//! arrives any other way (replication, repair) has no proof: the node asks the
//! chunk's close group for its listing and checks it on chain before keeping it.
//! Listings are served on [`ant_listings::TOPIC`].

use super::chain::Chain;
use super::store::ListingStore;
use crate::ant_protocol::{XorName, CLOSE_GROUP_SIZE};
use crate::error::{Error, Result};
use crate::logging::{debug, info, warn};
use crate::storage::ChunkStore;
use ::ant_protocol::evm::PaymentQuote;
use ::ant_protocol::payment::proof::deserialize_single_node_proof;
use ::ant_protocol::{detect_proof_type, ProofType};
use ant_listings::{
    find_payment, is_envelope, verify_listing, Envelope, Listing, ListingKey, ListingsBody,
    ListingsMessage, MAX_LIST_LIMIT, TOPIC,
};
use parking_lot::RwLock;
use saorsa_core::identity::PeerId;
use saorsa_core::{P2PEvent, P2PNode};
use std::path::Path;
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tokio::time::Instant;

/// How long after an envelope is stored the node checks that it has a listing.
/// A client PUT records one straight away; this leaves time for that before
/// asking other nodes.
const CHECK_DELAY: Duration = Duration::from_secs(30);

/// Waits between further attempts to fetch a missing listing.
const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(60),
    Duration::from_secs(600),
    Duration::from_secs(3600),
];

/// How long to wait for a peer to answer a listing request.
const PEER_TIMEOUT: Duration = Duration::from_secs(10);

/// Records and serves the listings for the envelopes this node stores.
pub struct ListingService {
    store: ListingStore,
    chunks: Arc<ChunkStore>,
    chain: Chain,
    vault: [u8; 20],
    p2p: RwLock<Option<Arc<P2PNode>>>,
}

impl ListingService {
    /// Open the listing store under `root_dir` and start watching `chunks` for
    /// newly stored envelopes. `rpc_url` and `vault` are the payment chain's
    /// RPC endpoint and payment vault.
    ///
    /// Must be called within a Tokio runtime.
    ///
    /// # Errors
    ///
    /// Returns an error if the store can't be opened or the RPC client built.
    pub async fn new(
        root_dir: &Path,
        chunks: Arc<ChunkStore>,
        rpc_url: String,
        vault: [u8; 20],
    ) -> Result<Arc<Self>> {
        let store = ListingStore::open(root_dir).await?;
        let service = Arc::new(Self {
            store,
            chunks: Arc::clone(&chunks),
            chain: Chain::new(rpc_url)?,
            vault,
            p2p: RwLock::new(None),
        });

        let (envelopes_tx, envelopes_rx) = mpsc::unbounded_channel();
        chunks.set_stored_observer(Box::new(move |address, content| {
            if is_envelope(content) {
                let _ = envelopes_tx.send(*address);
            }
        }));
        tokio::spawn(Self::watch_new_envelopes(
            Arc::downgrade(&service),
            envelopes_rx,
        ));
        Ok(service)
    }

    /// Attach the node's P2P handle, used to ask other nodes for listings.
    pub fn attach_p2p_node(&self, node: Arc<P2PNode>) {
        *self.p2p.write() = Some(node);
    }

    /// For a client PUT: the listing to record if `content` is an envelope,
    /// found from the payment proof that came with it. `Ok(None)` for any other
    /// chunk.
    ///
    /// # Errors
    ///
    /// Returns a message for the client if `content` is a malformed envelope or
    /// its payment can't be found. The PUT is then refused, so the client can
    /// retry with the right proof.
    pub async fn listing_for_put(
        &self,
        address: &XorName,
        content: &[u8],
        proof: Option<&[u8]>,
    ) -> std::result::Result<Option<Listing>, String> {
        if !is_envelope(content) {
            return Ok(None);
        }
        let envelope = Envelope::from_bytes(content)
            .map_err(|e| format!("invalid listed-file envelope: {e}"))?;
        let proof = proof.ok_or("a listed-file envelope needs its payment proof")?;
        if detect_proof_type(proof) != Some(ProofType::SingleNode) {
            return Err("a listed-file envelope must be paid with a single-node payment".into());
        }
        let proof = deserialize_single_node_proof(proof)?;
        let quotes: Vec<PaymentQuote> = proof
            .proof_of_payment
            .peer_quotes
            .into_iter()
            .map(|(_, quote)| quote)
            .collect();

        for tx_hash in &proof.tx_hashes {
            let payment = self
                .chain
                .payment(&tx_hash.0)
                .await
                .map_err(|e| format!("could not read the payment from the chain: {e}"))?;
            let Some((receipt, timestamp)) = payment else {
                debug!("Listing payment {tx_hash} not found on chain");
                continue;
            };
            match find_payment(address, &quotes, &receipt, timestamp, &self.vault) {
                Ok(payment) => return Ok(Some(Listing::new(&envelope, payment))),
                Err(e) => debug!("Transaction {tx_hash} does not pay for this envelope: {e}"),
            }
        }
        Err("none of the proof's transactions pays for this envelope".into())
    }

    /// Keep a listing found for a client PUT, once its chunk is stored.
    pub async fn record(&self, listing: Listing) {
        let address = hex::encode(listing.address);
        match self.store.insert(listing).await {
            Ok(true) => info!("Listed {address}"),
            Ok(false) => debug!("{address} was already listed"),
            Err(e) => warn!("Could not record the listing for {address}: {e}"),
        }
    }

    /// Answer a message received on [`TOPIC`]. Returns the encoded response,
    /// or `None` for messages that need none, such as responses to this node's
    /// own requests.
    pub async fn handle_message(&self, data: &[u8]) -> Option<Vec<u8>> {
        let message = match ListingsMessage::decode(data) {
            Ok(message) => message,
            Err(e) => {
                debug!("Ignoring an undecodable listings message: {e}");
                return None;
            }
        };
        let body = match message.body {
            ListingsBody::ListRequest { from, limit } => self.list(from, limit).await,
            ListingsBody::GetRequest { address } => ListingsBody::GetResponse {
                listing: self.held_listing(address).await,
            },
            _ => return None,
        };
        ListingsMessage {
            request_id: message.request_id,
            body,
        }
        .encode()
        .map_err(|e| warn!("Could not encode a listings response: {e}"))
        .ok()
    }

    async fn list(&self, from: ListingKey, limit: u32) -> ListingsBody {
        let limit = usize::try_from(limit.min(MAX_LIST_LIMIT)).unwrap_or(0);
        let (listings, more) = match self.store.range(from, limit).await {
            Ok(page) => page,
            Err(e) => {
                warn!("Could not read listings: {e}");
                return ListingsBody::Error {
                    message: "listings are unavailable".into(),
                };
            }
        };
        let mut held = Vec::with_capacity(listings.len());
        for listing in listings {
            if self.still_held(&listing.address).await {
                held.push(listing);
            }
        }
        ListingsBody::ListResponse {
            listings: held,
            more,
        }
    }

    /// The listing for `address`, if this node holds both it and the chunk.
    async fn held_listing(&self, address: XorName) -> Option<Listing> {
        let listing = match self.store.get(address).await {
            Ok(listing) => listing?,
            Err(e) => {
                warn!(
                    "Could not read the listing for {}: {e}",
                    hex::encode(address)
                );
                return None;
            }
        };
        self.still_held(&address).await.then_some(listing)
    }

    /// Whether this node still holds the chunk at `address`. A listing whose
    /// chunk has gone (pruned when it moved to other nodes) is dropped.
    async fn still_held(&self, address: &XorName) -> bool {
        match self.chunks.exists(address) {
            // When in doubt, keep the listing: the next request asks again.
            Ok(true) | Err(_) => true,
            Ok(false) => {
                if let Err(e) = self.store.remove(*address).await {
                    warn!(
                        "Could not drop the listing for {}: {e}",
                        hex::encode(address)
                    );
                }
                false
            }
        }
    }

    /// Check every newly stored envelope for a listing, a little after it lands.
    async fn watch_new_envelopes(
        service: Weak<Self>,
        mut envelopes: mpsc::UnboundedReceiver<XorName>,
    ) {
        while let Some(address) = envelopes.recv().await {
            tokio::spawn(Self::ensure_listing_with_retries(service.clone(), address));
        }
    }

    async fn ensure_listing_with_retries(service: Weak<Self>, address: XorName) {
        tokio::time::sleep(CHECK_DELAY).await;
        let mut retries = RETRY_DELAYS.iter();
        loop {
            let Some(listings) = service.upgrade() else {
                return;
            };
            match listings.ensure_listing(&address).await {
                Ok(true) => return,
                Ok(false) => {}
                Err(e) => debug!("Listing check for {} failed: {e}", hex::encode(address)),
            }
            drop(listings);
            let Some(delay) = retries.next() else {
                warn!(
                    "No close-group peer had a valid listing for envelope {}",
                    hex::encode(address)
                );
                return;
            };
            tokio::time::sleep(*delay).await;
        }
    }

    /// Make sure an envelope this node stores has a listing, fetching it from
    /// the close group if needed. Returns `false` if none could be found yet.
    async fn ensure_listing(&self, address: &XorName) -> Result<bool> {
        if self.store.contains(*address).await? {
            return Ok(true);
        }
        let Some(content) = self.chunks.get(address).await? else {
            return Ok(true);
        };
        let envelope = match Envelope::from_bytes(&content) {
            Ok(envelope) => envelope,
            Err(e) => {
                debug!(
                    "Stored chunk {} is not a valid envelope: {e}",
                    hex::encode(address)
                );
                return Ok(true);
            }
        };
        let p2p = self
            .p2p
            .read()
            .clone()
            .ok_or_else(|| Error::Network("no P2P node attached".into()))?;
        let self_id = *p2p.peer_id();
        let peers: Vec<PeerId> = p2p
            .dht_manager()
            .find_closest_nodes_local_with_self(address, CLOSE_GROUP_SIZE)
            .await
            .into_iter()
            .map(|node| node.peer_id)
            .filter(|peer| *peer != self_id)
            .collect();

        for peer in peers {
            let Some(listing) = request_listing(&p2p, &peer, address).await else {
                continue;
            };
            if listing.address != *address || !listing.matches(&envelope) {
                debug!(
                    "Peer {peer} sent a listing that doesn't match {}",
                    hex::encode(address)
                );
                continue;
            }
            if let Err(e) = self.check_payment(&listing).await {
                debug!("Peer {peer} sent a listing that doesn't check out: {e}");
                continue;
            }
            self.record(listing).await;
            return Ok(true);
        }
        Ok(false)
    }

    async fn check_payment(&self, listing: &Listing) -> Result<()> {
        let (receipt, timestamp) = self
            .chain
            .payment(&listing.payment.tx_hash)
            .await?
            .ok_or_else(|| Error::Payment("the payment transaction is not on chain".into()))?;
        verify_listing(listing, &receipt, timestamp, &self.vault)
            .map_err(|e| Error::Payment(e.to_string()))
    }
}

/// Ask `peer` for its listing of `address`.
async fn request_listing(p2p: &P2PNode, peer: &PeerId, address: &XorName) -> Option<Listing> {
    let request_id = rand::random::<u64>();
    let request = ListingsMessage {
        request_id,
        body: ListingsBody::GetRequest { address: *address },
    }
    .encode()
    .ok()?;

    // Subscribe before sending, so the response can't be missed.
    let mut events = p2p.subscribe_events();
    p2p.send_message(peer, TOPIC, request, &[]).await.ok()?;
    let deadline = Instant::now() + PEER_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Ok(P2PEvent::Message {
                topic,
                source: Some(source),
                data,
                ..
            })) if topic == TOPIC && source == *peer => {
                let Ok(response) = ListingsMessage::decode(&data) else {
                    continue;
                };
                if response.request_id != request_id {
                    continue;
                }
                return match response.body {
                    ListingsBody::GetResponse { listing } => listing,
                    _ => None,
                };
            }
            Ok(Ok(_) | Err(RecvError::Lagged(_))) => {}
            Ok(Err(RecvError::Closed)) | Err(_) => return None,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::storage::ChunkStoreConfig;
    use ant_listings::self_encryption::{ChunkInfo, DataMap, XorName as SeXorName};
    use ant_listings::{ListingPayment, Metadata};
    use tempfile::TempDir;

    async fn service(dir: &TempDir) -> (Arc<ListingService>, Arc<ChunkStore>) {
        let chunks = Arc::new(
            ChunkStore::new(ChunkStoreConfig {
                root_dir: dir.path().to_path_buf(),
                ..ChunkStoreConfig::test_default()
            })
            .await
            .unwrap(),
        );
        // No test reaches the chain.
        let listings = ListingService::new(
            dir.path(),
            Arc::clone(&chunks),
            "http://127.0.0.1:9".into(),
            [0x69; 20],
        )
        .await
        .unwrap();
        (listings, chunks)
    }

    fn envelope(seed: u8) -> Envelope {
        let infos = (0..3u8)
            .map(|index| ChunkInfo {
                index: usize::from(index),
                dst_hash: SeXorName([seed.wrapping_add(index); 32]),
                src_hash: SeXorName([seed.wrapping_add(index).wrapping_add(50); 32]),
                src_size: 10,
            })
            .collect();
        Envelope::new(
            &DataMap::new(infos),
            Metadata {
                content_type: Some("text/plain".into()),
                name: Some("a.txt".into()),
            },
        )
        .unwrap()
    }

    fn listing_for(envelope: &Envelope, block_number: u64) -> Listing {
        Listing::new(
            envelope,
            ListingPayment {
                tx_hash: [7; 32],
                block_number,
                block_timestamp: 1_790_000_000,
                log_index: 0,
                quote: vec![1],
            },
        )
    }

    async fn ask(listings: &ListingService, body: ListingsBody) -> ListingsBody {
        let request = ListingsMessage {
            request_id: 42,
            body,
        }
        .encode()
        .unwrap();
        let response =
            ListingsMessage::decode(&listings.handle_message(&request).await.unwrap()).unwrap();
        assert_eq!(response.request_id, 42);
        response.body
    }

    #[tokio::test]
    async fn serves_listings_for_held_chunks_in_order() {
        let dir = TempDir::new().unwrap();
        let (listings, chunks) = service(&dir).await;
        let (a, b) = (envelope(1), envelope(100));
        chunks.put(&a.address(), &a.to_bytes()).await.unwrap();
        chunks.put(&b.address(), &b.to_bytes()).await.unwrap();
        listings.record(listing_for(&b, 20)).await;
        listings.record(listing_for(&a, 10)).await;

        let ListingsBody::ListResponse {
            listings: page,
            more,
        } = ask(
            &listings,
            ListingsBody::ListRequest {
                from: ListingKey::default(),
                limit: 10,
            },
        )
        .await
        else {
            panic!("expected a list response");
        };
        assert!(!more);
        assert_eq!(
            page.iter().map(|l| l.address).collect::<Vec<_>>(),
            vec![a.address(), b.address()]
        );

        let ListingsBody::GetResponse { listing } = ask(
            &listings,
            ListingsBody::GetRequest {
                address: b.address(),
            },
        )
        .await
        else {
            panic!("expected a get response");
        };
        assert_eq!(listing.unwrap().payment.block_number, 20);
    }

    #[tokio::test]
    async fn drops_listings_whose_chunk_is_gone() {
        let dir = TempDir::new().unwrap();
        let (listings, chunks) = service(&dir).await;
        let a = envelope(1);
        chunks.put(&a.address(), &a.to_bytes()).await.unwrap();
        listings.record(listing_for(&a, 10)).await;
        chunks.delete(&a.address()).await.unwrap();

        let ListingsBody::GetResponse { listing } = ask(
            &listings,
            ListingsBody::GetRequest {
                address: a.address(),
            },
        )
        .await
        else {
            panic!("expected a get response");
        };
        assert!(listing.is_none());
        assert!(!listings.store.contains(a.address()).await.unwrap());
    }

    #[tokio::test]
    async fn ignores_responses_and_garbage() {
        let dir = TempDir::new().unwrap();
        let (listings, _) = service(&dir).await;
        let response = ListingsMessage {
            request_id: 1,
            body: ListingsBody::GetResponse { listing: None },
        }
        .encode()
        .unwrap();
        assert!(listings.handle_message(&response).await.is_none());
        assert!(listings.handle_message(&[0xff; 4]).await.is_none());
    }

    #[tokio::test]
    async fn puts_of_other_chunks_need_no_listing() {
        let dir = TempDir::new().unwrap();
        let (listings, _) = service(&dir).await;
        assert_eq!(
            listings
                .listing_for_put(&[1; 32], b"ordinary chunk bytes", None)
                .await,
            Ok(None)
        );
    }

    #[tokio::test]
    async fn refuses_envelopes_without_a_usable_proof() {
        let dir = TempDir::new().unwrap();
        let (listings, _) = service(&dir).await;
        let a = envelope(1);
        assert!(listings
            .listing_for_put(&a.address(), &a.to_bytes(), None)
            .await
            .is_err());
        assert!(listings
            .listing_for_put(&a.address(), &a.to_bytes(), Some(&[0xff, 1, 2]))
            .await
            .is_err());
        let mut broken = a.to_bytes();
        broken.truncate(12);
        assert!(listings
            .listing_for_put(&a.address(), &broken, None)
            .await
            .is_err());
    }
}
