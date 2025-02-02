/// Definition of State structure containing the data defining the current state of the
/// blockchain. The struct wraps an interface to the persistence layer as well as a cache.
///
pub mod cache;
pub mod chain_state;

use crate::db::{IterOrder, KVBatch, KValue, MerkleDB};
pub use cache::{KVMap, KVecMap, SessionedCache};
pub use chain_state::ChainState;
use parking_lot::RwLock;
use ruc::*;
use std::sync::Arc;

/// State Definition used by all stores
///
/// Contains a Reference to the ChainState and a Session Cache used for collecting batch data
/// and transaction simulation.
pub struct State<D: MerkleDB> {
    chain_state: Arc<RwLock<ChainState<D>>>,
    cache: SessionedCache,
}

impl<D: MerkleDB> State<D> {
    pub fn substate(&self) -> Self {
        Self {
            chain_state: self.chain_state.clone(),
            cache: self.cache.clone(),
        }
    }
}

/// Implementation of concrete State struct
impl<D> State<D>
where
    D: MerkleDB,
{
    /// Creates a State with a new cache and shared ChainState
    pub fn new(cs: Arc<RwLock<ChainState<D>>>, is_merkle: bool) -> Self {
        State {
            // lock whole State object for now
            chain_state: cs,
            cache: SessionedCache::new(is_merkle),
        }
    }

    /// Creates a State with a same cache and shared ChainState
    pub fn copy(&self) -> Self {
        State {
            chain_state: self.chain_state.clone(),
            cache: self.cache.clone(),
        }
    }

    /// Returns the chain state of the store.
    pub fn chain_state(&self) -> Arc<RwLock<ChainState<D>>> {
        self.chain_state.clone()
    }

    /// Gets a value for the given key.
    ///
    /// First checks the cache for the latest value for that key.
    /// Returns the value if found, otherwise queries the chainState.
    ///
    /// Can either return None or a Vec<u8> as the value.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        //Check if value was deleted
        if self.cache.deleted(key) {
            return Ok(None);
        }
        //Check if key has a value
        if self.cache.hasv(key) {
            return Ok(self.cache.getv(key));
        }

        //If the key isn't found in the cache then query the chain state directly
        let cs = self.chain_state.read();
        cs.get(key)
    }

    pub fn get_ver(&self, key: &[u8], height: u64) -> Result<Option<Vec<u8>>> {
        self.chain_state.read().get_ver(key, height)
    }

    /// Queries whether a key exists in the current state.
    ///
    /// First Checks the cache, returns true if found otherwise queries the chainState.
    pub fn exists(&self, key: &[u8]) -> Result<bool> {
        //Check if the key exists in the cache otherwise check the chain state
        let val = self.cache.getv(key);
        if val.is_some() {
            return Ok(true);
        }
        let cs = self.chain_state.read();
        cs.exists(key)
    }

    /// Sets a key value pair in the cache
    pub fn set(&mut self, key: &[u8], value: Vec<u8>) -> Result<()> {
        if self.cache.put(key, value) {
            Ok(())
        } else {
            Err(eg!("Invalid key-value pair detected."))
        }
    }

    /// Deletes a key from the State.
    ///
    /// The deletion of a key is represented by setting the value to None for a given key.
    ///
    /// When attempting to delete a key that doesn't exist in the ChainState, the MerkleDB
    /// will panic.
    ///
    /// To avoid this case, the ChainState is first queried for the key. If the key is found,
    /// the deletion proceeds as usual. If it isn't found in the ChainState but exists in the
    /// cache, then the key value record will be removed from the cache.
    pub fn delete(&mut self, key: &[u8]) -> Result<()> {
        let cs = self.chain_state.read();
        match cs.get(key).c(d!())? {
            //Mark key as deleted
            Some(_) => self.cache.delete(key),
            //Remove key from cache
            None => self.cache.remove(key),
        }
        Ok(())
    }

    /// Iterates the ChainState for the given range of keys
    pub fn iterate(
        &self,
        lower: &[u8],
        upper: &[u8],
        order: IterOrder,
        func: &mut dyn FnMut(KValue) -> bool,
    ) -> bool {
        let cs = self.chain_state.read();
        cs.iterate(lower, upper, order, func)
    }

    /// Iterates the cache for a given prefix
    pub fn iterate_cache(&self, prefix: &[u8], map: &mut KVecMap) {
        self.cache.iter_prefix(prefix, map);
    }

    /// Commits the current state to the DB with the given height
    ///
    /// The cache gets persisted to the MerkleDB and then cleared
    pub fn commit(&mut self, height: u64) -> Result<(Vec<u8>, u64)> {
        let mut cs = self.chain_state.write();
        //Get batch for current block
        let kv_batch = self.cache.commit();
        //Clear the cache from the current state
        self.cache = SessionedCache::new(self.cache.is_merkle());

        //Commit batch to db
        cs.commit(kv_batch, height, true)
    }

    /// Commits the cache of the current session.
    ///
    /// The Base cache gets updated with the current cache.
    pub fn commit_session(&mut self) -> KVBatch {
        self.cache.commit()
    }

    /// Discards the current session cache.
    ///
    /// The current cache is rebased.
    pub fn discard_session(&mut self) {
        self.cache.discard()
    }

    /// Export a copy of chain state on a specific height.
    ///
    /// * `cs` - The target chain state that holds the copy.
    /// * `height` - On which height the copy will be taken.
    ///
    pub fn export(&self, cs: &mut ChainState<D>, height: u64) -> Result<()> {
        self.chain_state.read().export(cs, height)
    }

    /// Returns whether or not a key has been modified in the current block
    pub fn touched(&self, key: &[u8]) -> bool {
        self.cache.touched(key)
    }

    /// Return the current height of the Merkle tree
    pub fn height(&self) -> Result<u64> {
        let cs = self.chain_state.read();
        cs.height()
    }

    /// Returns the root hash of the last commit
    pub fn root_hash(&self) -> Vec<u8> {
        let cs = self.chain_state.read();
        cs.root_hash()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{KValue, TempFinDB, TempRocksDB};
    use std::thread;
    const VER_WINDOW: u64 = 100;

    /// create chain state of `FinDB`
    fn gen_cs(path: String) -> Arc<RwLock<ChainState<TempFinDB>>> {
        let fdb = TempFinDB::open(path).expect("failed to open findb");
        let cs = chain_state::ChainState::new(fdb, "test_db".to_string(), VER_WINDOW);
        Arc::new(RwLock::new(cs))
    }

    /// create chain state of `RocksDB`
    fn gen_cs_rocks(path: String) -> Arc<RwLock<ChainState<TempRocksDB>>> {
        let fdb = TempRocksDB::open(path).expect("failed to open rocksdb");
        let cs = chain_state::ChainState::new(fdb, "test_db".to_string(), 0);
        Arc::new(RwLock::new(cs))
    }

    fn test_get_impl<D: MerkleDB>(cs: Arc<RwLock<ChainState<D>>>) {
        //Setup
        let mut state = State::new(cs.clone(), true);

        //Set some kv pairs
        state.set(b"prefix_validator_1", b"v10".to_vec()).unwrap();
        state.set(b"prefix_delegator_1", b"v20".to_vec()).unwrap();

        //Get the values
        assert_eq!(
            state.get(b"prefix_validator_1").unwrap(),
            Some(b"v10".to_vec())
        );
        assert_eq!(
            state.get(b"prefix_delegator_1").unwrap(),
            Some(b"v20".to_vec())
        );
        assert_eq!(state.get(b"prefix_validator_2").unwrap(), None);

        //Commit and create new state - Simulate new block
        let _res = state.commit(89);
        state = State::new(cs, true);

        //Should get this value from the chain state as the state cache is empty
        assert_eq!(
            state.get(b"prefix_validator_1").unwrap(),
            Some(b"v10".to_vec())
        );
    }

    #[test]
    fn test_get() {
        let path = thread::current().name().unwrap().to_owned();
        let cs = gen_cs(path);
        test_get_impl(cs);
    }

    #[test]
    fn test_get_rocks() {
        let path = thread::current().name().unwrap().to_owned();
        let cs = gen_cs_rocks(path);
        test_get_impl(cs);
    }

    fn test_exists_impl<D: MerkleDB>(cs: Arc<RwLock<ChainState<D>>>) {
        //Setup
        let mut state = State::new(cs, true);

        //Set some kv pairs
        state.set(b"prefix_validator_1", b"v10".to_vec()).unwrap();
        state.set(b"prefix_delegator_1", b"v20".to_vec()).unwrap();

        //Get the values
        assert!(state.exists(b"prefix_validator_1").unwrap());
        assert!(state.exists(b"prefix_delegator_1").unwrap());
        assert!(!state.exists(b"prefix_validator_2").unwrap());

        //Commit and create new state - Simulate new block
        let _res = state.commit(89);

        //Should get this value from the chain state as the state cache is empty
        assert!(state.exists(b"prefix_validator_1").unwrap());
    }

    #[test]
    fn test_exists() {
        let path = thread::current().name().unwrap().to_owned();
        let cs = gen_cs(path);
        test_exists_impl(cs);
    }

    #[test]
    fn test_exists_rocks() {
        let path = thread::current().name().unwrap().to_owned();
        let cs = gen_cs_rocks(path);
        test_exists_impl(cs);
    }

    fn test_set_impl<D: MerkleDB>(cs: Arc<RwLock<ChainState<D>>>) {
        //Setup
        let mut state = State::new(cs, true);

        //Set some kv pairs
        state.set(b"prefix_validator_1", b"v10".to_vec()).unwrap();
        state.set(b"prefix_delegator_1", b"v20".to_vec()).unwrap();

        //Get the values
        assert_eq!(
            state.get(b"prefix_validator_1").unwrap(),
            Some(b"v10".to_vec())
        );
        assert_eq!(
            state.get(b"prefix_delegator_1").unwrap(),
            Some(b"v20".to_vec())
        );
        assert_eq!(state.get(b"prefix_validator_2").unwrap(), None);
    }

    #[test]
    fn test_set() {
        let path = thread::current().name().unwrap().to_owned();
        let cs = gen_cs(path);
        test_set_impl(cs);
    }

    #[test]
    fn test_set_rocks() {
        let path = thread::current().name().unwrap().to_owned();
        let cs = gen_cs_rocks(path);
        test_set_impl(cs);
    }

    #[test]
    fn test_set_big_kv_checked() {
        // Setup
        let path = thread::current().name().unwrap().to_owned();
        let fdb = TempFinDB::open(path).expect("failed to open db");
        let cs = Arc::new(RwLock::new(ChainState::new(
            fdb,
            "test_db".to_string(),
            VER_WINDOW,
        )));
        let mut state = State::new(cs, true);

        // Set maximum valid key and value
        let max_key = "k".repeat(u8::MAX as usize).as_bytes().to_vec();
        let max_val = "v".repeat(u16::MAX as usize).as_bytes().to_vec();
        state.set(&max_key, max_val.clone()).unwrap();
        assert_eq!(state.get(&max_key).unwrap(), Some(max_val));

        // Set invalid key and value
        let big_key = "k".repeat(u8::MAX as usize + 1).as_bytes().to_vec();
        let big_val = "v".repeat(u16::MAX as usize + 1).as_bytes().to_vec();
        assert!(state.set(&big_key, b"v10".to_vec()).is_err());
        assert!(state.set(b"key10", big_val).is_err());
        assert_eq!(state.get(&big_key).unwrap(), None);
        assert_eq!(state.get(b"key10").unwrap(), None);
    }

    #[test]
    #[should_panic]
    fn test_set_big_key_unchecked_panic() {
        // Setup
        let path = thread::current().name().unwrap().to_owned();
        let fdb = TempFinDB::open(path).expect("failed to open db");
        let cs = Arc::new(RwLock::new(ChainState::new(
            fdb,
            "test_db".to_string(),
            VER_WINDOW,
        )));
        let mut state = State::new(cs, true);

        // Set maximum valid key and value
        let max_key = "k".repeat(u8::MAX as usize).as_bytes().to_vec();
        let max_val = "v".repeat(u16::MAX as usize).as_bytes().to_vec();
        state.set(&max_key, max_val.clone()).unwrap();
        assert_eq!(state.get(&max_key).unwrap(), Some(max_val));

        // Set a big key
        let big_key = "k".repeat(u8::MAX as usize + 1).as_bytes().to_vec();
        assert!(state.set(&big_key, b"v10".to_vec()).is_ok());

        // Panic on commit
        state.commit(1).unwrap();
    }

    #[test]
    #[should_panic]
    fn test_set_big_value_unchecked_panic() {
        // Setup
        let path = thread::current().name().unwrap().to_owned();
        let fdb = TempFinDB::open(path).expect("failed to open db");
        let cs = Arc::new(RwLock::new(ChainState::new(
            fdb,
            "test_db".to_string(),
            VER_WINDOW,
        )));
        let mut state = State::new(cs, true);

        // Set a big value
        let big_val = "v".repeat(u16::MAX as usize + 1).as_bytes().to_vec();
        assert!(state.set(b"key10", big_val).is_ok());

        // Panic on commit
        state.commit(1).unwrap();
    }

    #[test]
    fn test_rocksdb_set_big_value_unchecked() {
        // Setup
        let path = thread::current().name().unwrap().to_owned();
        let cs = gen_cs_rocks(path);

        // Make sure is_merkle flag is false
        let mut state = State::new(cs, false);

        // Set a big value
        let big_val = "v".repeat(u16::MAX as usize + 1).as_bytes().to_vec();
        assert!(state.set(b"key10", big_val).is_ok());

        // commit
        state.commit(1).unwrap();
    }

    #[test]
    fn test_rocksdb_set_big_key_unchecked() {
        // Setup
        let path = thread::current().name().unwrap().to_owned();
        let cs = gen_cs_rocks(path);

        // Make sure is_merkle flag is false
        let mut state = State::new(cs, false);

        // Set maximum valid key and value
        let max_key = "k".repeat(u8::MAX as usize).as_bytes().to_vec();
        let max_val = "v".repeat(u16::MAX as usize).as_bytes().to_vec();
        state.set(&max_key, max_val.clone()).unwrap();
        assert_eq!(state.get(&max_key).unwrap(), Some(max_val));

        // Set a big key
        let big_key = "k".repeat(u8::MAX as usize + 1).as_bytes().to_vec();
        assert!(state.set(&big_key, b"v10".to_vec()).is_ok());

        // Panic on commit
        state.commit(1).unwrap();
    }

    fn test_delete_impl<D: MerkleDB>(cs: Arc<RwLock<ChainState<D>>>) {
        //Setup
        let mut state = State::new(cs, true);

        //Set some kv pairs
        state.set(b"prefix_validator_1", b"v10".to_vec()).unwrap();
        state.set(b"prefix_validator_2", b"v20".to_vec()).unwrap();
        state.set(b"prefix_validator_3", b"v30".to_vec()).unwrap();

        //Get the values
        assert_eq!(
            state.get(b"prefix_validator_1").unwrap(),
            Some(b"v10".to_vec())
        );
        assert_eq!(
            state.get(b"prefix_validator_2").unwrap(),
            Some(b"v20".to_vec())
        );
        assert_eq!(
            state.get(b"prefix_validator_3").unwrap(),
            Some(b"v30".to_vec())
        );

        // ----------- Commit and clear cache - Simulate new block -----------
        let _res = state.commit(89);

        //Should get this value from the chain state as the state cache is empty
        assert_eq!(
            state.get(b"prefix_validator_1").unwrap(),
            Some(b"v10".to_vec())
        );

        state.set(b"prefix_validator_4", b"v40".to_vec()).unwrap();
        let _res = state.delete(b"prefix_validator_4");

        println!(
            "test_delete Batch after delete: {:?}",
            state.commit_session()
        );

        //Delete key from chain state
        let _res2 = state.delete(b"prefix_validator_3");

        // ----------- Commit and clear cache - Simulate new block -----------
        let _res1 = state.commit(90);

        //Should get this value from the chain state as the state cache is empty
        assert_eq!(
            state.get(b"prefix_validator_1").unwrap(),
            Some(b"v10".to_vec())
        );
        assert_eq!(
            state.get(b"prefix_validator_2").unwrap(),
            Some(b"v20".to_vec())
        );
        assert_eq!(state.get(b"prefix_validator_3").unwrap(), None);
    }

    #[test]
    fn test_delete() {
        let path = thread::current().name().unwrap().to_owned();
        let cs = gen_cs(path);
        test_delete_impl(cs);
    }

    #[test]
    fn test_delete_rocks() {
        let path = thread::current().name().unwrap().to_owned();
        let cs = gen_cs_rocks(path);
        test_delete_impl(cs);
    }

    fn test_get_deleted_impl<D: MerkleDB>(cs: Arc<RwLock<ChainState<D>>>) {
        let mut state = State::new(cs, true);

        //Set some kv pairs
        state.set(b"prefix_validator_1", b"v10".to_vec()).unwrap();
        state.set(b"prefix_validator_2", b"v20".to_vec()).unwrap();
        state.set(b"prefix_validator_3", b"v30".to_vec()).unwrap();

        let _res = state.commit(89);

        //Should detect the key as deleted from the cache and return None without querying db
        let _res2 = state.delete(b"prefix_validator_3");
        assert_eq!(state.get(b"prefix_validator_3").unwrap(), None);
    }

    #[test]
    fn test_get_deleted() {
        let path = thread::current().name().unwrap().to_owned();
        let cs = gen_cs(path);
        test_get_deleted_impl(cs);
    }

    #[test]
    fn test_get_deleted_rocks() {
        let path = thread::current().name().unwrap().to_owned();
        let cs = gen_cs_rocks(path);
        test_get_deleted_impl(cs);
    }

    #[test]
    fn test_commit() {
        //Setup
        let path = thread::current().name().unwrap().to_owned();
        let cs = gen_cs(path);
        let mut state = State::new(cs, true);

        //Set some kv pairs
        state.set(b"prefix_validator_1", b"v10".to_vec()).unwrap();
        state.set(b"prefix_validator_2", b"v10".to_vec()).unwrap();

        //Commit state to db
        let (app_hash1, height1) = state.commit(90).unwrap();

        //Modify a value in the db
        state.set(b"prefix_validator_2", b"v20".to_vec()).unwrap();
        assert_eq!(height1, 90);

        //Commit state to db
        let (app_hash2, height2) = state.commit(91).unwrap();

        //Root hashes must be different
        assert_ne!(app_hash1, app_hash2);
        assert_eq!(height2, 91);

        //Commit state to db
        let (app_hash3, height3) = state.commit(92).unwrap();

        // Root hashes must be equal
        assert_eq!(app_hash2, app_hash3);
        assert_eq!(height3, 92)
    }

    #[test]
    fn test_commit_rocks() {
        //Setup
        let path = thread::current().name().unwrap().to_owned();
        let cs = gen_cs_rocks(path);
        let mut state = State::new(cs, true);

        //Set some kv pairs
        state.set(b"prefix_validator_1", b"v10".to_vec()).unwrap();
        state.set(b"prefix_validator_2", b"v10".to_vec()).unwrap();

        //Commit state to db
        let (app_hash1, height1) = state.commit(90).unwrap();

        //Modify a value in the db
        state.set(b"prefix_validator_2", b"v20".to_vec()).unwrap();
        assert_eq!(height1, 90);

        //Commit state to db
        let (app_hash2, height2) = state.commit(91).unwrap();

        //Root hashes must be different
        assert_eq!(app_hash1, app_hash2);
        assert_eq!(height2, 91);

        //Commit state to db
        let (app_hash3, height3) = state.commit(92).unwrap();

        // Root hashes must be equal
        assert_eq!(app_hash2, app_hash3);
        assert_eq!(height3, 92)
    }

    #[test]
    fn test_root_hash() {
        //Setup
        let path = thread::current().name().unwrap().to_owned();
        let fdb = TempFinDB::open(path).expect("failed to open db");
        let cs = Arc::new(RwLock::new(ChainState::new(
            fdb,
            "test_db".to_string(),
            VER_WINDOW,
        )));
        let mut state = State::new(cs, true);

        //Set some kv pairs
        state.set(b"prefix_validator_1", b"v10".to_vec()).unwrap();
        state.set(b"prefix_validator_2", b"v10".to_vec()).unwrap();

        //Commit state to db
        let (app_hash1, _height) = state.commit(90).unwrap();

        //Modify a value in the db
        state.set(b"prefix_validator_2", b"v20".to_vec()).unwrap();

        //Commit state to db
        let (app_hash2, _height) = state.commit(91).unwrap();

        //Root hashes must be different
        assert_ne!(app_hash1, app_hash2);

        //Commit state to db
        let (app_hash3, _height) = state.commit(92).unwrap();

        // Root hashes must be equal
        assert_eq!(app_hash2, app_hash3);
    }

    fn test_iterate_impl<D: MerkleDB>(cs: Arc<RwLock<ChainState<D>>>) {
        let mut state = State::new(cs, true);
        let mut count = 0;

        state.set(b"prefix_validator_1", b"v10".to_vec()).unwrap();
        state.set(b"prefix_validator_2", b"v10".to_vec()).unwrap();
        state.set(b"prefix_3", b"v10".to_vec()).unwrap();
        state.set(b"prefix_4", b"v10".to_vec()).unwrap();
        state.set(b"prefix_validator_5", b"v10".to_vec()).unwrap();
        state.set(b"prefix_validator_6", b"v10".to_vec()).unwrap();
        state.set(b"prefix_7", b"v10".to_vec()).unwrap();
        state.set(b"prefix_validator_8", b"v10".to_vec()).unwrap();

        // ----------- Commit state to db and clear cache -----------
        let res1 = state.commit(55).unwrap();
        assert_eq!(res1.1, 55);

        let mut func_iter = |entry: KValue| {
            println!("Key: {:?}, Value: {:?}", entry.0, entry.1);
            count += 1;
            false
        };
        state.iterate(
            &b"prefix_validator_".to_vec(),
            &b"prefix_validator~".to_vec(),
            IterOrder::Asc,
            &mut func_iter,
        );
        assert_eq!(count, 5);
    }

    #[test]
    fn test_iterate() {
        let path = thread::current().name().unwrap().to_owned();
        let cs = gen_cs(path);
        test_iterate_impl(cs);
    }

    #[test]
    fn test_iterate_rocks() {
        let path = thread::current().name().unwrap().to_owned();
        let cs = gen_cs_rocks(path);
        test_iterate_impl(cs);
    }
}
