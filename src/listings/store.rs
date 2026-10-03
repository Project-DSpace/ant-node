//! Persistent listings, backed by LMDB.
//!
//! ```text
//! {root}/listings.mdb/   -- LMDB environment directory
//! ```
//!
//! Two databases. `by_key` maps a listing's key (block number then log index,
//! both big-endian, so LMDB's byte order is the index order) to the encoded
//! listing. `by_address` maps a listed file's address to its key.

use crate::ant_protocol::XorName;
use crate::error::{Error, Result};
use ant_listings::{Listing, ListingKey};
use heed::types::Bytes;
use heed::{Database, Env, EnvOpenOptions};
use std::ops::Bound;
use std::path::Path;
use tokio::task::spawn_blocking;

/// LMDB map size for listings: 1 GiB. A listing is about 6 KB, and a node
/// holds only the listings for envelopes it stores.
const MAP_SIZE: usize = 1024 * 1024 * 1024;

/// The listings this node holds, by key and by address.
#[derive(Clone)]
pub struct ListingStore {
    env: Env,
    by_key: Database<Bytes, Bytes>,
    by_address: Database<Bytes, Bytes>,
}

impl ListingStore {
    /// Open or create the store at `{root_dir}/listings.mdb/`.
    ///
    /// # Errors
    ///
    /// Returns an error if the LMDB environment or its databases can't be
    /// opened or created.
    #[allow(unsafe_code)]
    pub async fn open(root_dir: &Path) -> Result<Self> {
        let dir = root_dir.join("listings.mdb");
        std::fs::create_dir_all(&dir)
            .map_err(|e| Error::Storage(format!("Failed to create listings directory: {e}")))?;
        spawn_blocking(move || -> Result<Self> {
            // SAFETY: `EnvOpenOptions::open()` is unsafe because LMDB relies on
            // file locking to stop two processes mapping the same environment.
            // As for the paid list, each node has its own `root_dir`, so no two
            // processes open this one.
            let env = unsafe {
                EnvOpenOptions::new()
                    .map_size(MAP_SIZE)
                    .max_dbs(2)
                    .open(&dir)
                    .map_err(|e| Error::Storage(format!("Failed to open listings LMDB env: {e}")))?
            };
            let mut wtxn = env.write_txn().map_err(storage_error)?;
            let by_key = env
                .create_database(&mut wtxn, Some("by_key"))
                .map_err(storage_error)?;
            let by_address = env
                .create_database(&mut wtxn, Some("by_address"))
                .map_err(storage_error)?;
            wtxn.commit().map_err(storage_error)?;
            Ok(Self {
                env,
                by_key,
                by_address,
            })
        })
        .await
        .map_err(|e| Error::Storage(format!("Listings init task failed: {e}")))?
    }

    /// Add a listing. Returns `false`, changing nothing, if the address is
    /// already listed: the first listing of an envelope stands.
    ///
    /// # Errors
    ///
    /// Returns an error if encoding or the LMDB write fails.
    pub async fn insert(&self, listing: Listing) -> Result<bool> {
        let store = self.clone();
        spawn_blocking(move || store.insert_blocking(&listing))
            .await
            .map_err(join_error)?
    }

    /// The listing for `address`, if held.
    ///
    /// # Errors
    ///
    /// Returns an error if the LMDB read or decoding fails.
    pub async fn get(&self, address: XorName) -> Result<Option<Listing>> {
        let store = self.clone();
        spawn_blocking(move || store.get_blocking(&address))
            .await
            .map_err(join_error)?
    }

    /// Whether `address` is listed.
    ///
    /// # Errors
    ///
    /// Returns an error if the LMDB read fails.
    pub async fn contains(&self, address: XorName) -> Result<bool> {
        let store = self.clone();
        spawn_blocking(move || -> Result<bool> {
            let rtxn = store.env.read_txn().map_err(storage_error)?;
            Ok(store
                .by_address
                .get(&rtxn, &address)
                .map_err(storage_error)?
                .is_some())
        })
        .await
        .map_err(join_error)?
    }

    /// Remove the listing for `address`. Returns whether there was one.
    ///
    /// # Errors
    ///
    /// Returns an error if the LMDB write fails.
    pub async fn remove(&self, address: XorName) -> Result<bool> {
        let store = self.clone();
        spawn_blocking(move || -> Result<bool> {
            let mut wtxn = store.env.write_txn().map_err(storage_error)?;
            let Some(key) = store
                .by_address
                .get(&wtxn, &address)
                .map_err(storage_error)?
                .map(<[u8]>::to_vec)
            else {
                return Ok(false);
            };
            store
                .by_key
                .delete(&mut wtxn, &key)
                .map_err(storage_error)?;
            store
                .by_address
                .delete(&mut wtxn, &address)
                .map_err(storage_error)?;
            wtxn.commit().map_err(storage_error)?;
            Ok(true)
        })
        .await
        .map_err(join_error)?
    }

    /// Up to `limit` listings with keys from `from` onwards, in key order, and
    /// whether later ones exist.
    ///
    /// # Errors
    ///
    /// Returns an error if the LMDB read or decoding fails.
    pub async fn range(&self, from: ListingKey, limit: usize) -> Result<(Vec<Listing>, bool)> {
        let store = self.clone();
        spawn_blocking(move || store.range_blocking(from, limit))
            .await
            .map_err(join_error)?
    }

    fn insert_blocking(&self, listing: &Listing) -> Result<bool> {
        let key = key_bytes(listing.key());
        let value =
            postcard::to_stdvec(listing).map_err(|e| Error::Serialization(e.to_string()))?;
        let mut wtxn = self.env.write_txn().map_err(storage_error)?;
        if self
            .by_address
            .get(&wtxn, &listing.address)
            .map_err(storage_error)?
            .is_some()
        {
            return Ok(false);
        }
        self.by_key
            .put(&mut wtxn, &key, &value)
            .map_err(storage_error)?;
        self.by_address
            .put(&mut wtxn, &listing.address, &key)
            .map_err(storage_error)?;
        wtxn.commit().map_err(storage_error)?;
        Ok(true)
    }

    fn get_blocking(&self, address: &XorName) -> Result<Option<Listing>> {
        let rtxn = self.env.read_txn().map_err(storage_error)?;
        let Some(key) = self.by_address.get(&rtxn, address).map_err(storage_error)? else {
            return Ok(None);
        };
        self.by_key
            .get(&rtxn, key)
            .map_err(storage_error)?
            .map(decode)
            .transpose()
    }

    fn range_blocking(&self, from: ListingKey, limit: usize) -> Result<(Vec<Listing>, bool)> {
        let rtxn = self.env.read_txn().map_err(storage_error)?;
        let from = key_bytes(from);
        let bounds: (Bound<&[u8]>, Bound<&[u8]>) = (Bound::Included(&from[..]), Bound::Unbounded);
        let mut listings = Vec::new();
        let mut more = false;
        for entry in self.by_key.range(&rtxn, &bounds).map_err(storage_error)? {
            let (_, value) = entry.map_err(storage_error)?;
            if listings.len() == limit {
                more = true;
                break;
            }
            listings.push(decode(value)?);
        }
        Ok((listings, more))
    }
}

fn key_bytes(key: ListingKey) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&key.block_number.to_be_bytes());
    bytes[8..].copy_from_slice(&key.log_index.to_be_bytes());
    bytes
}

fn decode(value: &[u8]) -> Result<Listing> {
    postcard::from_bytes(value).map_err(|e| Error::Serialization(format!("bad listing: {e}")))
}

#[allow(clippy::needless_pass_by_value)]
fn storage_error(e: heed::Error) -> Error {
    Error::Storage(format!("listings: {e}"))
}

#[allow(clippy::needless_pass_by_value)]
fn join_error(e: tokio::task::JoinError) -> Error {
    Error::Storage(format!("listings task failed: {e}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use ant_listings::{ListingPayment, Metadata};
    use tempfile::TempDir;

    fn listing(address: u8, block_number: u64, log_index: u64) -> Listing {
        Listing {
            address: [address; 32],
            metadata: Metadata::default(),
            size: Some(10),
            payment: ListingPayment {
                tx_hash: [address; 32],
                block_number,
                block_timestamp: 1_790_000_000 + block_number,
                log_index,
                quote: vec![1, 2, 3],
            },
        }
    }

    #[tokio::test]
    async fn keeps_listings_in_payment_order() {
        let dir = TempDir::new().unwrap();
        let store = ListingStore::open(dir.path()).await.unwrap();
        for (address, block, log) in [(1, 20, 0), (2, 10, 5), (3, 10, 2), (4, 300, 0)] {
            assert!(store.insert(listing(address, block, log)).await.unwrap());
        }

        let (all, more) = store.range(ListingKey::default(), 10).await.unwrap();
        let order: Vec<u8> = all.iter().map(|l| l.address[0]).collect();
        assert_eq!(order, vec![3, 2, 1, 4]);
        assert!(!more);

        let (page, more) = store.range(ListingKey::default(), 2).await.unwrap();
        assert_eq!(page.len(), 2);
        assert!(more);
        let (rest, more) = store.range(page[1].key().next(), 10).await.unwrap();
        let order: Vec<u8> = rest.iter().map(|l| l.address[0]).collect();
        assert_eq!(order, vec![1, 4]);
        assert!(!more);
    }

    #[tokio::test]
    async fn the_first_listing_of_an_address_stands() {
        let dir = TempDir::new().unwrap();
        let store = ListingStore::open(dir.path()).await.unwrap();
        assert!(store.insert(listing(1, 10, 0)).await.unwrap());
        assert!(!store.insert(listing(1, 99, 0)).await.unwrap());
        assert_eq!(
            store
                .get([1; 32])
                .await
                .unwrap()
                .unwrap()
                .payment
                .block_number,
            10
        );
    }

    #[tokio::test]
    async fn removes_and_survives_reopening() {
        let dir = TempDir::new().unwrap();
        {
            let store = ListingStore::open(dir.path()).await.unwrap();
            store.insert(listing(1, 10, 0)).await.unwrap();
            store.insert(listing(2, 11, 0)).await.unwrap();
            assert!(store.remove([1; 32]).await.unwrap());
            assert!(!store.remove([1; 32]).await.unwrap());
        }
        let store = ListingStore::open(dir.path()).await.unwrap();
        assert!(!store.contains([1; 32]).await.unwrap());
        assert!(store.contains([2; 32]).await.unwrap());
        let (all, _) = store.range(ListingKey::default(), 10).await.unwrap();
        assert_eq!(all.len(), 1);
    }
}
