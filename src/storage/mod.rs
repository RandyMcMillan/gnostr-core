//! Save data to a desired storage backend.

mod node;
mod persist;

pub use self::node::Node;
pub use self::persist::Persist;
pub use merkle_tree_stream::Node as NodeTrait;

use anyhow::{anyhow, ensure, Result};
use ed25519_dalek::{PublicKey, SecretKey, Signature, PUBLIC_KEY_LENGTH, SECRET_KEY_LENGTH};
use flat_tree as flat;
use futures::future::FutureExt;
#[cfg(not(target_arch = "wasm32"))]
use random_access_disk::RandomAccessDisk;
use random_access_memory::RandomAccessMemory;
use random_access_storage::RandomAccess;
use sleep_parser::*;
use std::borrow::Borrow;
use std::convert::TryFrom;
use std::fmt::Debug;
use std::ops::Range;
use std::path::PathBuf;

const HEADER_OFFSET: u64 = 32;

#[derive(Debug)]
pub struct PartialKeypair {
    pub public: PublicKey,
    pub secret: Option<SecretKey>,
}

/// The types of stores that can be created.
#[derive(Debug)]
pub enum Store {
    /// Tree
    Tree,
    /// Data
    Data,
    /// Bitfield
    Bitfield,
    /// Signatures
    #[cfg(not(feature = "v10"))]
    Signatures,
    /// Keypair
    #[cfg(not(feature = "v10"))]
    Keypair,
    /// Oplog
    #[cfg(feature = "v10")]
    Oplog,
}

/// Save data to a desired storage backend.
#[derive(Debug)]
pub struct Storage<T>
where
    T: RandomAccess + Debug,
{
    tree: T,
    data: T,
    bitfield: T,
    #[cfg(not(feature = "v10"))]
    signatures: T,
    #[cfg(not(feature = "v10"))]
    keypair: T,
    #[cfg(feature = "v10")]
    oplog: T,
}

impl<T> Storage<T>
where
    T: RandomAccess<Error = Box<dyn std::error::Error + Send + Sync>> + Debug + Send,
{
    /// Create a new instance. Takes a keypair and a callback to create new
    /// storage instances.
    // Named `.open()` in the JS version. Replaces the `.openKey()` method too by
    // requiring a key pair to be initialized before creating a new instance.
    pub async fn new<Cb>(create: Cb, overwrite: bool) -> Result<Self>
    where
        Cb: Fn(Store) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T>> + Send>>,
    {
        let tree = create(Store::Tree).await?;
        let data = create(Store::Data).await?;
        let bitfield = create(Store::Bitfield).await?;
        #[cfg(not(feature = "v10"))]
        let signatures = create(Store::Signatures).await?;
        #[cfg(not(feature = "v10"))]
        let keypair = create(Store::Keypair).await?;

        #[cfg(feature = "v10")]
        let oplog = create(Store::Oplog).await?;

        let mut instance = Self {
            tree,
            data,
            bitfield,
            #[cfg(not(feature = "v10"))]
            signatures,
            #[cfg(not(feature = "v10"))]
            keypair,
            #[cfg(feature = "v10")]
            oplog,
        };

        #[cfg(feature = "v10")]
        if overwrite || instance.bitfield.len().await.unwrap_or(0) == 0 {
            // TODO: This has nothing in it
            instance
                .oplog
                .write(0, &[0x00])
                .await
                .map_err(|e| anyhow!(e))?;
        }

        if overwrite || instance.bitfield.len().await.unwrap_or(0) == 0 {
            let header = create_bitfield();
            instance
                .bitfield
                .write(0, &header.to_vec())
                .await
                .map_err(|e| anyhow!(e))?;
        }

        #[cfg(not(feature = "v10"))]
        if overwrite || instance.signatures.len().await.unwrap_or(0) == 0 {
            let header = create_signatures();
            instance
                .signatures
                .write(0, &header.to_vec())
                .await
                .map_err(|e| anyhow!(e))?;
        }

        if overwrite || instance.tree.len().await.unwrap_or(0) == 0 {
            let header = create_tree();
            instance
                .tree
                .write(0, &header.to_vec())
                .await
                .map_err(|e| anyhow!(e))?;
        }

        Ok(instance)
    }

    /// Write data to the feed.
    #[inline]
    pub async fn write_data(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        self.data.write(offset, &data).await.map_err(|e| anyhow!(e))
    }

    /// Write a byte vector to a data storage (random-access instance) at the
    /// position of `index`.
    ///
    /// NOTE: Meant to be called from the `.put()` feed method. Probably used to
    /// insert data as-is after receiving it from the network (need to confirm
    /// with mafintosh).
    /// TODO: Ensure the signature size is correct.
    /// NOTE: Should we create a `Data` entry type?
    pub async fn put_data(&mut self, index: u64, data: &[u8], nodes: &[Node]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }

        let range = self.data_offset(index, nodes).await?;

        ensure!(
            (range.end - range.start) as usize == data.len(),
            format!("length  `{:?} != {:?}`", range.count(), data.len())
        );

        self.data
            .write(range.start, data)
            .await
            .map_err(|e| anyhow!(e))
    }

    /// Get data from disk that the user has written to it. This is stored
    /// unencrypted, so there's no decryption needed.
    // FIXME: data_offset always reads out index 0, length 0
    #[inline]
    pub async fn get_data(&mut self, index: u64) -> Result<Vec<u8>> {
        let cached_nodes = Vec::new(); // TODO: reuse allocation.
        let range = self.data_offset(index, &cached_nodes).await?;
        self.data
            .read(range.start, range.count() as u64)
            .await
            .map_err(|e| anyhow!(e))
    }

    /// Search the signature stores for a `Signature`, starting at `index`.
    #[cfg(not(feature = "v10"))]
    pub fn next_signature(
        &mut self,
        index: u64,
    ) -> futures::future::BoxFuture<'_, Result<Signature>> {
        let bytes = async_std::task::block_on(async {
            self.signatures
                .read(HEADER_OFFSET + 64 * index, 64)
                .await
                .map_err(|e| anyhow!(e))
        });
        async move {
            let bytes = bytes?;
            if not_zeroes(&bytes) {
                Ok(Signature::try_from(&bytes[..])?)
            } else {
                Ok(self.next_signature(index + 1).await?)
            }
        }
        .boxed()
    }

    /// Get a `Signature` from the store.
    #[inline]
    #[cfg(not(feature = "v10"))]
    pub async fn get_signature(&mut self, index: u64) -> Result<Signature> {
        let bytes = self
            .signatures
            .read(HEADER_OFFSET + 64 * index, 64)
            .await
            .map_err(|e| anyhow!(e))?;
        ensure!(not_zeroes(&bytes), "No signature found");
        Ok(Signature::try_from(&bytes[..])?)
    }

    /// Write a `Signature` to `self.Signatures`.
    /// TODO: Ensure the signature size is correct.
    /// NOTE: Should we create a `Signature` entry type?
    #[inline]
    #[cfg(not(feature = "v10"))]
    pub async fn put_signature(
        &mut self,
        index: u64,
        signature: impl Borrow<Signature>,
    ) -> Result<()> {
        let signature = signature.borrow();
        self.signatures
            .write(HEADER_OFFSET + 64 * index, &signature.to_bytes())
            .await
            .map_err(|e| anyhow!(e))
    }

    /// TODO(yw) docs
    /// Get the offset for the data, return `(offset, size)`.
    ///
    /// ## Panics
    /// A panic can occur if no maximum value is found.
    pub async fn data_offset(&mut self, index: u64, cached_nodes: &[Node]) -> Result<Range<u64>> {
        let mut roots = Vec::new(); // TODO: reuse alloc
        flat::full_roots(tree_index(index), &mut roots);

        let mut offset = 0;
        let mut pending = roots.len() as u64;
        let block_index = tree_index(index);

        if pending == 0 {
            let len = match find_node(&cached_nodes, block_index) {
                Some(node) => node.len(),
                None => (self.get_node(block_index).await?).len(),
            };
            return Ok(offset..offset + len);
        }

        for root in roots {
            // FIXME: we're always having a cache miss here. Check cache first before
            // getting a node from the backend.
            //
            // ```rust
            // let node = match find_node(cached_nodes, root) {
            //   Some(node) => node,
            //   None => self.get_node(root),
            // };
            // ```
            let node = self.get_node(root).await?;

            offset += node.len();
            pending -= 1;
            if pending > 0 {
                continue;
            }

            let len = match find_node(&cached_nodes, block_index) {
                Some(node) => node.len(),
                None => (self.get_node(block_index).await?).len(),
            };

            return Ok(offset..offset + len);
        }

        unreachable!();
    }

    /// Get a `Node` from the `tree` storage.
    #[inline]
    pub async fn get_node(&mut self, index: u64) -> Result<Node> {
        let buf = self
            .tree
            .read(HEADER_OFFSET + 40 * index, 40)
            .await
            .map_err(|e| anyhow!(e))?;
        let node = Node::from_bytes(index, &buf)?;
        Ok(node)
    }

    /// Write a `Node` to the `tree` storage.
    /// TODO: prevent extra allocs here. Implement a method on node that can reuse
    /// a buffer.
    #[inline]
    pub async fn put_node(&mut self, node: &Node) -> Result<()> {
        let index = node.index();
        let buf = node.to_bytes()?;
        self.tree
            .write(HEADER_OFFSET + 40 * index, &buf)
            .await
            .map_err(|e| anyhow!(e))
    }

    /// Write data to the internal bitfield module.
    /// TODO: Ensure the chunk size is correct.
    /// NOTE: Should we create a bitfield entry type?
    #[inline]
    pub async fn put_bitfield(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        self.bitfield
            .write(HEADER_OFFSET + offset, data)
            .await
            .map_err(|e| anyhow!(e))
    }

    /// Read bitfield header.
    pub async fn read_bitfield(&mut self) -> Result<Vec<u8>> {
        let buf = self
            .bitfield
            .read(0, 32)
            .await
            .map_err(|_| anyhow::anyhow!("read bitfield header"))?;
        let header = Header::from_vec(&buf).map_err(|e| anyhow::anyhow!(e))?;

        // khodzha:
        // TODO: we should handle eof vs errors here somehow but idk how to do that
        let mut buf: Vec<u8> = Vec::new();
        let mut idx: u64 = 0;
        let ent_size: u64 = header.entry_size as u64;
        loop {
            let result = self
                .bitfield
                .read(HEADER_OFFSET + idx * ent_size, ent_size)
                .await;
            if let Ok(slice) = result {
                buf.extend_from_slice(&slice);
                idx += 1;
            } else {
                return Ok(buf);
            }
        }
    }

    /// Read a public key from storage
    #[cfg(not(feature = "v10"))]
    pub async fn read_public_key(&mut self) -> Result<PublicKey> {
        let buf = self
            .keypair
            .read(0, PUBLIC_KEY_LENGTH as u64)
            .await
            .map_err(|e| anyhow!(e))?;
        let public_key = PublicKey::from_bytes(&buf)?;
        Ok(public_key)
    }

    /// Read a secret key from storage
    #[cfg(not(feature = "v10"))]
    pub async fn read_secret_key(&mut self) -> Result<SecretKey> {
        let buf = self
            .keypair
            .read(PUBLIC_KEY_LENGTH as u64, SECRET_KEY_LENGTH as u64)
            .await
            .map_err(|e| anyhow!(e))?;
        let secret_key = SecretKey::from_bytes(&buf)?;
        Ok(secret_key)
    }

    /// Write a public key to the storage
    #[cfg(not(feature = "v10"))]
    pub async fn write_public_key(&mut self, public_key: &PublicKey) -> Result<()> {
        let buf: [u8; PUBLIC_KEY_LENGTH] = public_key.to_bytes();
        self.keypair.write(0, &buf).await.map_err(|e| anyhow!(e))
    }

    /// Write a secret key to the storage
    #[cfg(not(feature = "v10"))]
    pub async fn write_secret_key(&mut self, secret_key: &SecretKey) -> Result<()> {
        let buf: [u8; SECRET_KEY_LENGTH] = secret_key.to_bytes();
        self.keypair
            .write(PUBLIC_KEY_LENGTH as u64, &buf)
            .await
            .map_err(|e| anyhow!(e))
    }

    /// Tries to read a partial keypair (ie: with an optional secret_key) from the storage
    #[cfg(not(feature = "v10"))]
    pub async fn read_partial_keypair(&mut self) -> Option<PartialKeypair> {
        match self.read_public_key().await {
            Ok(public) => match self.read_secret_key().await {
                Ok(secret) => Some(PartialKeypair {
                    public,
                    secret: Some(secret),
                }),
                Err(_) => Some(PartialKeypair {
                    public,
                    secret: None,
                }),
            },
            Err(_) => None,
        }
    }
}

impl Storage<RandomAccessMemory> {
    /// Create a new instance backed by a `RandomAccessMemory` instance.
    pub async fn new_memory() -> Result<Self> {
        let create = |_| async { Ok(RandomAccessMemory::default()) }.boxed();
        Ok(Self::new(create, true).await?)
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Storage<RandomAccessDisk> {
    /// Create a new instance backed by a `RandomAccessDisk` instance.
    pub async fn new_disk(dir: &PathBuf, overwrite: bool) -> Result<Self> {
        let storage = |storage: Store| {
            let name = match storage {
                Store::Tree => "tree",
                Store::Data => "data",
                Store::Bitfield => "bitfield",
                #[cfg(not(feature = "v10"))]
                Store::Signatures => "signatures",
                #[cfg(not(feature = "v10"))]
                Store::Keypair => "key",
                #[cfg(feature = "v10")]
                Store::Oplog => "oplog",
            };
            RandomAccessDisk::open(dir.as_path().join(name)).boxed()
        };
        Ok(Self::new(storage, overwrite).await?)
    }
}

/// Get a node from a vector of nodes.
#[inline]
fn find_node(nodes: &[Node], index: u64) -> Option<&Node> {
    for node in nodes {
        if node.index() == index {
            return Some(node);
        }
    }
    None
}

/// Check if a byte slice is not completely zero-filled.
#[inline]
fn not_zeroes(bytes: &[u8]) -> bool {
    for byte in bytes {
        if *byte != 0 {
            return true;
        }
    }
    false
}

/// Convert the index to the index in the tree.
#[inline]
fn tree_index(index: u64) -> u64 {
    2 * index
}

#[test]
fn should_detect_zeroes() {
    let nums = vec![0; 10];
    assert!(!not_zeroes(&nums));

    let nums = vec![1; 10];
    assert!(not_zeroes(&nums));
}
