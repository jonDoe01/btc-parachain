#![deny(warnings)]
#![cfg_attr(test, feature(proc_macro_hygiene))]
#![cfg_attr(not(feature = "std"), no_std)]

/// # BTC-Relay implementation
/// This is the implementation of the BTC-Relay following the spec at:
/// https://interlay.gitlab.io/polkabtc-spec/btcrelay-spec/
// Substrate
mod ext;

#[cfg(any(feature = "runtime-benchmarks", test))]
mod benchmarking;

mod default_weights;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod mock;

#[cfg(test)]
extern crate mocktopus;

#[cfg(test)]
use mocktopus::macros::mockable;

use frame_support::debug;
use frame_support::weights::Weight;
use frame_support::{
    decl_error, decl_event, decl_module, decl_storage,
    dispatch::{DispatchError, DispatchResult},
    ensure, IterableStorageMap,
};
use frame_system::ensure_signed;
use primitive_types::U256;
use sp_core::H160;
use sp_std::collections::btree_set::BTreeSet;
use sp_std::prelude::*;
use util::transactional;

// Crates
pub use bitcoin;
use bitcoin::merkle::{MerkleProof, ProofResult};
use bitcoin::parser::{parse_block_header, parse_transaction};
use bitcoin::types::{
    BlockChain, BlockHeader, H256Le, RawBlockHeader, RichBlockHeader, Transaction,
};
use bitcoin::Error as BitcoinError;
use security::types::ErrorCode;

pub use bitcoin::Address as BtcAddress;
pub use bitcoin::PublicKey as BtcPublicKey;

pub trait WeightInfo {
    fn initialize() -> Weight;
    fn store_block_header() -> Weight;
    fn store_block_headers(b: u32) -> Weight;
    fn verify_and_validate_transaction() -> Weight;
    fn verify_transaction_inclusion() -> Weight;
    fn validate_transaction() -> Weight;
}

/// ## Configuration and Constants
/// The pallet's configuration trait.
/// For further reference, see:
/// https://interlay.gitlab.io/polkabtc-spec/btcrelay-spec/spec/data-model.html
pub trait Trait: frame_system::Trait + security::Trait + sla::Trait {
    /// The overarching event type.
    type Event: From<Event> + Into<<Self as frame_system::Trait>::Event>;

    /// Weight information for the extrinsics in this module.
    type WeightInfo: WeightInfo;
}

/// Difficulty Adjustment Interval
pub const DIFFICULTY_ADJUSTMENT_INTERVAL: u32 = 2016;

/// Target Timespan
pub const TARGET_TIMESPAN: u32 = 1_209_600;

// Used in Bitcoin's retarget algorithm
pub const TARGET_TIMESPAN_DIVISOR: u32 = 4;

// Accepted minimum number of transaction outputs for okd validation
pub const ACCEPTED_MIN_TRANSACTION_OUTPUTS: u32 = 1;

// Accepted minimum number of transaction outputs for op-return validation
pub const ACCEPTED_MIN_TRANSACTION_OUTPUTS_WITH_OP_RETURN: u32 = 2;

// Accepted maximum number of transaction outputs for validation
pub const ACCEPTED_MAX_TRANSACTION_OUTPUTS: u32 = 32;

/// Unrounded Maximum Target
/// 0x00000000FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF
pub const UNROUNDED_MAX_TARGET: U256 = U256([
    <u64>::max_value(),
    <u64>::max_value(),
    <u64>::max_value(),
    0x0000_0000_ffff_ffffu64,
]);

/// Main chain id
pub const MAIN_CHAIN_ID: u32 = 0;

/// Number of outputs expected in the accepted transaction format
/// See: https://interlay.gitlab.io/polkabtc-spec/btcrelay-spec/intro/accepted-format.html
pub const ACCEPTED_NO_TRANSACTION_OUTPUTS: u32 = 2;

// This pallet's storage items.
decl_storage! {
    trait Store for Module<T: Trait> as BTCRelay {
        /// ## Storage
        /// Store Bitcoin block headers
        BlockHeaders: map hasher(blake2_128_concat) H256Le => RichBlockHeader;

        /// Priority queue of BlockChain elements, ordered by the maximum height (descending).
        /// The first index into this mapping (0) is considered to be the longest chain. The value
        /// of the entry is the index into `ChainsIndex` to retrieve the `BlockChain`.
        Chains: map hasher(blake2_128_concat) u32 => Option<u32>;

        /// Auxiliary mapping of chains ids to `BlockChain` entries. The first index into this
        /// mapping (0) is considered to be the Bitcoin main chain.
        ChainsIndex: map hasher(blake2_128_concat) u32 => Option<BlockChain>;

        /// Stores a mapping from (chain_index, block_height) to block hash
        ChainsHashes: double_map hasher(blake2_128_concat) u32, hasher(blake2_128_concat) u32 => H256Le;

        /// Store the current blockchain tip
        BestBlock: H256Le;

        /// Store the height of the best block
        BestBlockHeight: u32;

        /// Increment-only counter used to track new BlockChain entries
        ChainCounter: u32;

        /// Registers the parachain height upon storing a block
        ParachainHeight: map hasher(blake2_128_concat) H256Le => T::BlockNumber;

        /// Global security parameter k for stable Bitcoin transactions
        StableBitcoinConfirmations get(fn bitcoin_confirmations) config(): u32;

        /// Global security parameter k for stable Parachain transactions
        StableParachainConfirmations get(fn parachain_confirmations) config(): T::BlockNumber;

        /// Whether the module should perform difficulty checks.
        DisableDifficultyCheck get(fn disable_difficulty_check) config(): bool;

        /// Whether the module should perform inclusion checks.
        DisableInclusionCheck get(fn disable_inclusion_check) config(): bool;

        /// Whether the module should perform OP_RETURN checks.
        DisableOpReturnCheck get(fn disable_op_return_check) config(): bool;
    }
}

macro_rules! extract_op_return {
    ($($tx:expr),*) => {
        {
            $(
                if let Some(Ok(data)) = $tx.map(|tx| tx.script.extract_op_return_data()) {
                    data
                } else
            )*
            { return Err(Error::<T>::NotOpReturn.into()); }
        }
    };
}

decl_module! {
    pub struct Module<T: Trait> for enum Call where origin: T::Origin {
        // Initialize errors
        type Error = Error<T>;

        // Initializing events
        fn deposit_event() = default;

        /// One time function to initialize the BTC-Relay with the first block
        /// # Arguments
        ///
        /// * `block_header_bytes` - 80 byte raw Bitcoin block header.
        /// * `block_height` - Bitcoin block height of the submitted
        /// block header.
        #[weight = <T as Trait>::WeightInfo::initialize()]
        #[transactional]
        fn initialize(
            origin,
            raw_block_header: RawBlockHeader,
            block_height: u32)
            -> DispatchResult
        {
            let _ = ensure_signed(origin)?;
            Self::_initialize(raw_block_header, block_height)
        }

        /// Stores a single new block header
        ///
        /// # Arguments
        ///
        /// * `raw_block_header` - 80 byte raw Bitcoin block header.
        #[weight = <T as Trait>::WeightInfo::store_block_header()]
        #[transactional]
        fn store_block_header(
            origin, raw_block_header: RawBlockHeader
        ) -> DispatchResult {
            let relayer = ensure_signed(origin)?;
            Self::_store_block_header(relayer, raw_block_header)
        }

        /// Stores multiple new block headers
        ///
        /// # Arguments
        ///
        /// * `raw_block_headers` - vector of Bitcoin block headers.
        #[weight = <T as Trait>::WeightInfo::store_block_headers(raw_block_headers.len() as u32)]
        #[transactional]
        fn store_block_headers(
            origin, raw_block_headers: Vec<RawBlockHeader>
        ) -> DispatchResult {
            let relayer = ensure_signed(origin)?;
            // TODO: optimize
            for raw_block_header in raw_block_headers {
                Self::_store_block_header(relayer.clone(), raw_block_header)?;
            }
            Ok(())
        }

        /// Verifies the inclusion of `tx_id` in block at height `tx_block_height` and validates the given raw Bitcoin transaction, according to the
        /// supported transaction format (see
        /// https://interlay.gitlab.io/polkabtc-spec/btcrelay-spec/intro/
        /// accepted-format.html).
        /// # Arguments
        ///
        /// * `tx_id` - The hash of the transaction to check for
        /// * `tx_block_height` - The height of the block in which the transaction should be included
        /// * `raw_merkle_proof` - The raw merkle proof as returned by bitcoin `gettxoutproof`
        /// * `confirmations` - The number of confirmations needed to accept the proof. If `none`,
        /// the value stored in the StableBitcoinConfirmations storage item is used.
        /// * `raw_tx` - raw Bitcoin transaction
        /// * `paymentValue` - value of BTC sent in the 1st / payment UTXO of the transaction
        /// * `recipientBtcAddress` - 20 byte Bitcoin address of recipient of the BTC in the 1st  / payment UTXO
        /// * `op_return_id` - 32 byte hash identifier expected in OP_RETURN (replay protection)
        #[weight = <T as Trait>::WeightInfo::verify_and_validate_transaction()]
        #[transactional]
        fn verify_and_validate_transaction(
            origin,
            tx_id: H256Le,
            raw_merkle_proof: Vec<u8>,
            confirmations: Option<u32>,
            raw_tx: Vec<u8>,
            payment_value: i64,
            recipient_btc_address: BtcAddress,
            op_return_id: Option<Vec<u8>>)
        -> DispatchResult {
            let _ = ensure_signed(origin)?;

            let transaction = Self::parse_transaction(&raw_tx)?;

            // Check that the passed raw_tx indeed matches the tx_id used for
            // transaction inclusion verification
            ensure!(tx_id == transaction.tx_id(), Error::<T>::InvalidTxid);

            // Verify that the transaction is indeed included in the main chain
            // Check for Parachain RUNNING state is performed here
            Self::_verify_transaction_inclusion(tx_id, raw_merkle_proof, confirmations)?;

            // Parse transaction and check that it matches the given parameters
            Self::_validate_transaction(raw_tx, payment_value, recipient_btc_address, op_return_id)?;

            Ok(())
        }

        /// Verifies the inclusion of `tx_id` in block at height `tx_block_height`
        ///
        /// # Arguments
        ///
        /// * `tx_id` - The hash of the transaction to check for
        /// * `raw_merkle_proof` - The raw merkle proof as returned by bitcoin `gettxoutproof`
        /// * `confirmations` - The number of confirmations needed to accept the proof. If `none`,
        /// the value stored in the StableBitcoinConfirmations storage item is used.
        #[weight = <T as Trait>::WeightInfo::verify_transaction_inclusion()]
        #[transactional]
        fn verify_transaction_inclusion(
            origin,
            tx_id: H256Le,
            raw_merkle_proof: Vec<u8>,
            confirmations: Option<u32>)
        -> DispatchResult {
            let _ = ensure_signed(origin)?;
            Self::_verify_transaction_inclusion(tx_id, raw_merkle_proof, confirmations)?;
            Ok(())
        }

        /// Validates a given raw Bitcoin transaction, according to the
        /// supported transaction format (see
        /// https://interlay.gitlab.io/polkabtc-spec/btcrelay-spec/intro/
        /// accepted-format.html)
        /// This DOES NOT check if the transaction is included in a block
        /// nor does it guarantee that the transaction is fully valid according
        /// to the consensus (needs full node).
        ///
        /// # Arguments
        /// * `raw_tx` - raw Bitcoin transaction
        /// * `paymentValue` - value of BTC sent in the 1st /
        /// payment UTXO of the transaction
        /// * `recipientBtcAddress` - 20 byte Bitcoin address of recipient
        /// of the BTC in the 1st  / payment UTXO
        /// * `op_return_id` - 32 byte hash identifier expected in
        /// OP_RETURN (replay protection)
        #[weight = <T as Trait>::WeightInfo::validate_transaction()]
        #[transactional]
        fn validate_transaction(
            origin,
            raw_tx: Vec<u8>,
            payment_value: i64,
            recipient_btc_address: BtcAddress,
            op_return_id: Option<Vec<u8>>
        ) -> DispatchResult {
            let _ = ensure_signed(origin)?;
            Self::_validate_transaction(raw_tx, payment_value, recipient_btc_address, op_return_id)?;
            Ok(())
        }
    }
}

#[cfg_attr(test, mockable)]
impl<T: Trait> Module<T> {
    pub fn _initialize(raw_block_header: RawBlockHeader, block_height: u32) -> DispatchResult {
        // Check if BTC-Relay was already initialized
        ensure!(!Self::best_block_exists(), Error::<T>::AlreadyInitialized);

        // Parse the block header bytes to extract the required info
        let basic_block_header =
            parse_block_header(&raw_block_header).map_err(|err| Error::<T>::from(err))?;
        let block_header_hash = raw_block_header.hash();

        // construct the BlockChain struct
        let blockchain = Self::initialize_blockchain(block_height, block_header_hash);
        // Create rich block header
        let block_header = RichBlockHeader {
            block_hash: block_header_hash,
            block_header: basic_block_header,
            block_height: block_height,
            chain_ref: blockchain.chain_id,
        };

        // Store a new BlockHeader struct in BlockHeaders
        Self::set_block_header_from_hash(block_header_hash, &block_header);

        // Store a pointer to BlockChain in ChainsIndex
        Self::set_block_chain_from_id(MAIN_CHAIN_ID, &blockchain);

        // Store the reference to the new BlockChain in Chains
        Self::set_chain_from_position_and_id(0, MAIN_CHAIN_ID);

        // Set BestBlock and BestBlockHeight to the submitted block
        Self::set_best_block(block_header_hash);
        Self::set_best_block_height(block_height);

        // Emit a Initialized Event
        Self::deposit_event(Event::Initialized(block_height, block_header_hash));

        Ok(())
    }

    pub fn _store_block_header(
        relayer: T::AccountId,
        raw_block_header: RawBlockHeader,
    ) -> DispatchResult {
        // Make sure Parachain is not shutdown
        ext::security::ensure_parachain_status_not_shutdown::<T>()?;

        // Parse the block header bytes to extract the required info
        let basic_block_header = Self::verify_block_header(&raw_block_header)?;
        let block_header_hash = raw_block_header.hash();

        let prev_header = Self::get_block_header_from_hash(basic_block_header.hash_prev_block)?;

        // get the block chain of the previous header
        let prev_blockchain = Self::get_block_chain_from_id(prev_header.chain_ref)?;

        // Update the current block header
        // check if the prev block is the highest block in the chain
        // load the previous block header block height
        let prev_block_height = prev_header.block_height;

        // update the current block header with height and chain ref
        // Set the height of the block header
        let current_block_height = prev_block_height + 1;

        // Update the blockchain
        // check if we create a new blockchain or extend the existing one
        debug::print!("Prev max height: {:?}\n", prev_blockchain.max_height);
        debug::print!("Prev block height: {:?}\n", prev_block_height);
        let is_fork = prev_blockchain.max_height != prev_block_height;
        debug::print!("Fork detected: {:?}\n", is_fork);

        let blockchain = if is_fork {
            // create new blockchain element
            Self::create_blockchain(current_block_height, block_header_hash)
        } else {
            // extend the current chain
            Self::extend_blockchain(current_block_height, &block_header_hash, prev_blockchain)?
        };

        // Create rich block header
        let block_header = RichBlockHeader {
            block_hash: block_header_hash,
            block_header: basic_block_header,
            block_height: current_block_height,
            chain_ref: blockchain.chain_id,
        };

        // Store a new BlockHeader struct in BlockHeaders
        Self::set_block_header_from_hash(block_header_hash, &block_header);

        // Storing the blockchain depends if we extend or create a new chain
        if is_fork {
            // create a new chain
            // Store a pointer to BlockChain in ChainsIndex
            Self::set_block_chain_from_id(blockchain.chain_id, &blockchain);
            // Store the reference to the blockchain in Chains
            Self::insert_sorted(&blockchain)?;
        } else {
            // extended the chain
            // Update the pointer to BlockChain in ChainsIndex
            <ChainsIndex>::mutate(blockchain.chain_id, |_b| &blockchain);

            // check if ordering of Chains needs updating
            Self::check_and_do_reorg(&blockchain)?;

            if blockchain.chain_id == MAIN_CHAIN_ID {
                Self::set_best_block(block_header_hash);
                Self::set_best_block_height(current_block_height)
            }
        };

        ext::sla::event_update_relayer_sla::<T>(relayer, ext::sla::RelayerEvent::BlockSubmission)?;

        // Determine if this block extends the main chain or a fork
        let current_best_block = Self::get_best_block();

        if current_best_block == block_header_hash {
            // extends the main chain
            Self::deposit_event(Event::StoreMainChainHeader(
                current_block_height,
                block_header_hash,
            ));
        } else {
            // created a new fork or updated an existing one
            Self::deposit_event(Event::StoreForkHeader(
                blockchain.chain_id,
                current_block_height,
                block_header_hash,
            ));
        };

        Ok(())
    }

    pub fn _verify_transaction_inclusion(
        tx_id: H256Le,
        raw_merkle_proof: Vec<u8>,
        confirmations: Option<u32>,
    ) -> Result<(), DispatchError> {
        if Self::disable_inclusion_check() {
            return Ok(());
        }

        let best_block_height = Self::get_best_block_height();
        Self::ensure_no_ongoing_fork(best_block_height)?;

        let merkle_proof = Self::parse_merkle_proof(&raw_merkle_proof)?;

        let rich_header = Self::get_block_header_from_hash(merkle_proof.block_header.hash())?;

        ensure!(
            rich_header.chain_ref == MAIN_CHAIN_ID,
            Error::<T>::InvalidChainID
        );

        let block_height = rich_header.block_height;

        Self::transaction_verification_allowed(block_height)?;

        // This call fails if not enough confirmations
        Self::check_bitcoin_confirmations(best_block_height, confirmations, block_height)?;

        // This call fails if the block was stored too recently
        Self::check_parachain_confirmations(rich_header.block_hash)?;

        let proof_result = Self::verify_merkle_proof(&merkle_proof)?;

        // fail if the transaction hash is invalid
        ensure!(
            proof_result.transaction_hash == tx_id,
            Error::<T>::InvalidTxid
        );

        // fail if the merkle root is invalid
        ensure!(
            proof_result.extracted_root == rich_header.block_header.merkle_root,
            Error::<T>::InvalidMerkleProof
        );
        Ok(())
    }

    /// Extract all payments and op_return outputs from a transaction.
    /// Rejects transactions with too many outputs.
    ///
    /// # Arguments
    ///
    /// * `transaction` - Bitcoin transaction
    pub fn extract_outputs(
        transaction: Transaction,
    ) -> Result<(Vec<(i64, BtcAddress)>, Vec<(i64, Vec<u8>)>), Error<T>> {
        ensure!(
            transaction.outputs.len() <= ACCEPTED_MAX_TRANSACTION_OUTPUTS as usize,
            Error::<T>::MalformedTransaction
        );

        let mut payments = Vec::new();
        let mut op_returns = Vec::new();
        for tx in transaction.outputs {
            if let Ok(address) = tx.extract_address() {
                payments.push((tx.value, address));
            } else if let Ok(data) = tx.script.extract_op_return_data() {
                op_returns.push((tx.value, data));
            }
        }

        Ok((payments, op_returns))
    }

    /// Extract the payment value from the first output with an address
    /// that matches the `recipient_btc_address`.
    ///
    /// # Arguments
    ///
    /// * `transaction` - Bitcoin transaction
    /// * `recipient_btc_address` - expected payment recipient
    fn extract_payment_value(
        transaction: Transaction,
        recipient_btc_address: BtcAddress,
    ) -> Result<i64, DispatchError> {
        ensure!(
            // We would typically expect two outputs here (payment, refund) but
            // the input amount may be exact so we would only require one
            transaction.outputs.len() >= ACCEPTED_MIN_TRANSACTION_OUTPUTS as usize,
            Error::<T>::MalformedTransaction
        );

        // Check if payment is first output
        match transaction
            .outputs
            .get(0)
            .map(|output| output.extract_address())
        {
            Some(Ok(extr_recipient_btc_address)) => {
                if recipient_btc_address == extr_recipient_btc_address {
                    return Ok(transaction.outputs[0].value);
                }
            }
            _ => (),
        };

        // Check if payment is second output
        match transaction
            .outputs
            .get(1)
            .map(|output| output.extract_address())
        {
            Some(Ok(extr_recipient_btc_address)) => {
                if recipient_btc_address == extr_recipient_btc_address {
                    return Ok(transaction.outputs[1].value);
                }
            }
            _ => (),
        };

        // Check if payment is third output
        match transaction
            .outputs
            .get(1)
            .map(|output| output.extract_address())
        {
            Some(Ok(extr_recipient_btc_address)) => {
                if recipient_btc_address == extr_recipient_btc_address {
                    return Ok(transaction.outputs[2].value);
                }
            }
            _ => (),
        };

        // Payment UTXO sends to incorrect address
        Err(Error::<T>::WrongRecipient.into())
    }

    /// Extract the payment value and `OP_RETURN` payload from the first
    /// output with an address that matches the `recipient_btc_address`.
    ///
    /// # Arguments
    ///
    /// * `transaction` - Bitcoin transaction
    /// * `recipient_btc_address` - expected payment recipient
    fn extract_payment_value_and_op_return(
        transaction: Transaction,
        recipient_btc_address: BtcAddress,
    ) -> Result<(i64, Vec<u8>), DispatchError> {
        ensure!(
            // We would typically expect three outputs (payment, op_return, refund) but
            // exceptionally the input amount may be exact so we would only require two
            transaction.outputs.len() >= ACCEPTED_MIN_TRANSACTION_OUTPUTS_WITH_OP_RETURN as usize,
            Error::<T>::MalformedTransaction
        );

        // Check if payment is first output
        match transaction.outputs[0].extract_address() {
            Ok(extr_recipient_btc_address) => {
                if recipient_btc_address == extr_recipient_btc_address {
                    return Ok((
                        transaction.outputs[0].value,
                        extract_op_return!(transaction.outputs.get(1), transaction.outputs.get(2)),
                    ));
                }
            }
            Err(_) => (),
        };

        // Check if payment is second output
        match transaction.outputs[1].extract_address() {
            Ok(extr_recipient_btc_address) => {
                if recipient_btc_address == extr_recipient_btc_address {
                    return Ok((
                        transaction.outputs[1].value,
                        extract_op_return!(transaction.outputs.get(0), transaction.outputs.get(2)),
                    ));
                }
            }
            Err(_) => (),
        };

        // Check if payment is third output
        match transaction
            .outputs
            .get(2)
            .map(|output| output.extract_address())
        {
            Some(Ok(extr_recipient_btc_address)) => {
                if recipient_btc_address == extr_recipient_btc_address {
                    return Ok((
                        transaction.outputs[2].value,
                        extract_op_return!(transaction.outputs.get(0), transaction.outputs.get(1)),
                    ));
                }
            }
            _ => (),
        };

        // Payment UTXO sends to incorrect address
        Err(Error::<T>::WrongRecipient.into())
    }

    pub fn is_op_return_disabled() -> bool {
        Self::disable_op_return_check()
    }

    /// Checks if transaction is valid. If so, it returns the first origin address, which can be
    /// use as the destination address for a potential refund, and the payment value
    pub fn _validate_transaction(
        raw_tx: Vec<u8>,
        payment_value: i64,
        recipient_btc_address: BtcAddress,
        op_return_id: Option<Vec<u8>>,
    ) -> Result<(BtcAddress, i64), DispatchError> {
        let transaction = Self::parse_transaction(&raw_tx)?;

        let input_address = transaction
            .clone()
            .inputs
            .get(0)
            .ok_or(Error::<T>::MalformedTransaction)?
            .extract_address()
            .map_err(|_| Error::<T>::MalformedTransaction)?;

        let extr_payment_value = if Self::is_op_return_disabled() {
            Self::extract_payment_value(transaction, recipient_btc_address)?
        } else {
            if let Some(op_return_id) = op_return_id {
                // NOTE: op_return UTXO should not contain any value
                let (extr_payment_value, extr_op_return) =
                    Self::extract_payment_value_and_op_return(transaction, recipient_btc_address)?;

                // Check if data UTXO has correct OP_RETURN value
                ensure!(extr_op_return == op_return_id, Error::<T>::InvalidOpReturn);

                extr_payment_value
            } else {
                // using the on-chain key derivation scheme we only expect a simple
                // payment to the vault's new deposit address
                Self::extract_payment_value(transaction, recipient_btc_address)?
            }
        };

        // Check if payment UTXO transfers sufficient value
        ensure!(
            extr_payment_value >= payment_value,
            Error::<T>::InsufficientValue
        );

        Ok((input_address, extr_payment_value))
    }

    // ********************************
    // START: Storage getter functions
    // ********************************

    /// Get chain id from position (sorted by max block height)
    fn get_chain_id_from_position(position: u32) -> Result<u32, DispatchError> {
        <Chains>::get(position).ok_or(Error::<T>::InvalidChainID.into())
    }

    /// Get the position of the fork in Chains
    fn get_chain_position_from_chain_id(chain_id: u32) -> Result<u32, DispatchError> {
        for (k, v) in <Chains>::iter() {
            if v == chain_id {
                return Ok(k);
            }
        }
        Err(Error::<T>::ForkIdNotFound.into())
    }

    /// Get a blockchain from the id
    fn get_block_chain_from_id(chain_id: u32) -> Result<BlockChain, DispatchError> {
        <ChainsIndex>::get(chain_id).ok_or(Error::<T>::InvalidChainID.into())
    }

    /// Get the current best block hash
    pub fn get_best_block() -> H256Le {
        <BestBlock>::get()
    }

    /// Check if a best block hash is set
    fn best_block_exists() -> bool {
        <BestBlock>::exists()
    }

    /// get the best block height
    pub fn get_best_block_height() -> u32 {
        <BestBlockHeight>::get()
    }

    /// Get the current chain counter
    fn get_chain_counter() -> u32 {
        <ChainCounter>::get()
    }

    /// Get a block hash from a blockchain
    /// # Arguments
    ///
    /// * `chain_id`: the id of the blockchain to search in
    /// * `block_height`: the height if the block header
    fn get_block_hash(chain_id: u32, block_height: u32) -> Result<H256Le, DispatchError> {
        if !Self::block_exists(chain_id, block_height) {
            return Err(Error::<T>::MissingBlockHeight.into());
        }
        Ok(<ChainsHashes>::get(chain_id, block_height))
    }

    /// Get a block header from its hash
    fn get_block_header_from_hash(block_hash: H256Le) -> Result<RichBlockHeader, DispatchError> {
        if <BlockHeaders>::contains_key(block_hash) {
            return Ok(<BlockHeaders>::get(block_hash));
        }
        Err(Error::<T>::BlockNotFound.into())
    }

    /// Check if a block header exists
    pub fn block_header_exists(block_hash: H256Le) -> bool {
        <BlockHeaders>::contains_key(block_hash)
    }

    /// Get a block header from
    fn get_block_header_from_height(
        blockchain: &BlockChain,
        block_height: u32,
    ) -> Result<RichBlockHeader, DispatchError> {
        let block_hash = Self::get_block_hash(blockchain.chain_id, block_height)?;
        Self::get_block_header_from_hash(block_hash)
    }

    /// Storage setter functions
    /// Set a new chain with position and id
    fn set_chain_from_position_and_id(position: u32, id: u32) {
        <Chains>::insert(position, id);
    }

    /// Swap chain elements
    fn swap_chain(pos_1: u32, pos_2: u32) {
        // swaps the values of two keys
        <Chains>::swap(pos_1, pos_2)
    }

    /// Remove a chain id from chains
    fn remove_blockchain_from_chain(position: u32) -> Result<(), DispatchError> {
        // swap the element with the last element in the mapping
        // collect the unsorted chains iterable as a vector and sort it by index
        let mut chains = <Chains>::iter().collect::<Vec<(u32, u32)>>();
        chains.sort_by_key(|k| k.0);

        // get the last position as stored in the list
        let last_pos = match chains.len() {
            0 => return Err(Error::<T>::ForkIdNotFound.into()),
            // chains stores (position, index)
            n => chains[n - 1].0,
        };
        Self::swap_chain(position, last_pos);
        // don't remove main chain id
        if last_pos > 0 {
            // remove the old head (now the value at the initial position)
            <Chains>::remove(last_pos);
        }
        Ok(())
    }

    /// Set a new blockchain in ChainsIndex
    fn set_block_chain_from_id(id: u32, chain: &BlockChain) {
        <ChainsIndex>::insert(id, &chain);
    }

    /// Update a blockchain in ChainsIndex
    fn mutate_block_chain_from_id(id: u32, chain: BlockChain) {
        <ChainsIndex>::mutate(id, |b| *b = Some(chain));
    }

    /// Remove a blockchain element from ChainsIndex
    fn remove_blockchain_from_chain_index(id: u32) {
        <ChainsIndex>::remove(id);
    }

    /// Set a new block header
    fn set_block_header_from_hash(hash: H256Le, header: &RichBlockHeader) {
        <BlockHeaders>::insert(hash, header);
        // register the current height to track stable parachain confirmations
        Self::set_parachain_height_from_hash(hash);
    }

    /// Store the height of the parachain when storing a Bitcoin header
    fn set_parachain_height_from_hash(hash: H256Le) {
        let height = <frame_system::Module<T>>::block_number();
        <ParachainHeight<T>>::insert(hash, height);
    }

    /// update the chain_ref of a block header
    fn mutate_block_header_from_chain_id(hash: &H256Le, chain_ref: u32) {
        <BlockHeaders>::mutate(&hash, |header| header.chain_ref = chain_ref);
    }

    /// Set a new best block
    fn set_best_block(hash: H256Le) {
        <BestBlock>::put(hash);
    }

    /// Set a new best block height
    fn set_best_block_height(height: u32) {
        <BestBlockHeight>::put(height);
    }

    /// Set a new chain counter
    fn increment_chain_counter() -> u32 {
        let new_counter = Self::get_chain_counter() + 1;
        <ChainCounter>::put(new_counter);

        new_counter
    }

    /// Initialize the new main blockchain with a single block
    fn initialize_blockchain(block_height: u32, block_hash: H256Le) -> BlockChain {
        let chain_id = MAIN_CHAIN_ID;

        // generate an empty blockchain
        Self::generate_blockchain(chain_id, block_height, block_hash)
    }

    /// Create a new blockchain element with a new chain id
    fn create_blockchain(block_height: u32, block_hash: H256Le) -> BlockChain {
        // get a new chain id
        let chain_id: u32 = Self::increment_chain_counter();

        // generate an empty blockchain
        Self::generate_blockchain(chain_id, block_height, block_hash)
    }

    /// Generate the raw blockchain from a chain Id and with a single block
    fn generate_blockchain(chain_id: u32, block_height: u32, block_hash: H256Le) -> BlockChain {
        // initialize an empty chain

        Self::insert_block_hash(chain_id, block_height, block_hash);

        BlockChain {
            chain_id,
            start_height: block_height,
            max_height: block_height,
            no_data: BTreeSet::new(),
            invalid: BTreeSet::new(),
        }
    }

    fn insert_block_hash(chain_id: u32, block_height: u32, block_hash: H256Le) {
        <ChainsHashes>::insert(chain_id, block_height, block_hash);
    }

    fn remove_block_hash(chain_id: u32, block_height: u32) {
        <ChainsHashes>::remove(chain_id, block_height);
    }

    fn block_exists(chain_id: u32, block_height: u32) -> bool {
        <ChainsHashes>::contains_key(chain_id, block_height)
    }

    fn _blocks_count(chain_id: u32) -> usize {
        <ChainsHashes>::iter_prefix_values(chain_id).count()
    }

    /// Add a new block header to an existing blockchain
    fn extend_blockchain(
        block_height: u32,
        block_hash: &H256Le,
        prev_blockchain: BlockChain,
    ) -> Result<BlockChain, DispatchError> {
        let mut blockchain = prev_blockchain;

        if Self::block_exists(blockchain.chain_id, block_height) {
            return Err(Error::<T>::DuplicateBlock.into());
        }
        Self::insert_block_hash(blockchain.chain_id, block_height, *block_hash);

        blockchain.max_height = block_height;
        Self::set_block_chain_from_id(blockchain.chain_id, &blockchain);

        Ok(blockchain)
    }

    // Get require conformations for stable transactions
    fn get_stable_transaction_confirmations() -> u32 {
        Self::bitcoin_confirmations()
    }

    // *********************************
    // END: Storage getter functions
    // *********************************

    // Wrapper functions around bitcoin lib for testing purposes
    fn parse_transaction(raw_tx: &[u8]) -> Result<Transaction, DispatchError> {
        Ok(parse_transaction(&raw_tx).map_err(|err| Error::<T>::from(err))?)
    }

    fn parse_merkle_proof(raw_merkle_proof: &[u8]) -> Result<MerkleProof, DispatchError> {
        MerkleProof::parse(&raw_merkle_proof).map_err(|err| Error::<T>::from(err).into())
    }

    fn verify_merkle_proof(merkle_proof: &MerkleProof) -> Result<ProofResult, DispatchError> {
        merkle_proof
            .verify_proof()
            .map_err(|err| Error::<T>::from(err).into())
    }

    /// Parses and verifies a raw Bitcoin block header.
    /// # Arguments
    /// * block_header` - 80-byte block header
    ///
    /// # Returns
    /// * `pure_block_header` - PureBlockHeader representation of the 80-byte block header
    ///
    /// # Panics
    /// If ParachainStatus in Security module is not set to RUNNING
    fn verify_block_header(
        raw_block_header: &RawBlockHeader,
    ) -> Result<BlockHeader, DispatchError> {
        let basic_block_header =
            parse_block_header(&raw_block_header).map_err(|err| Error::<T>::from(err))?;

        let block_header_hash = raw_block_header.hash();

        // Check that the block header is not yet stored in BTC-Relay
        ensure!(
            !Self::block_header_exists(block_header_hash),
            Error::<T>::DuplicateBlock
        );

        // Check that the referenced previous block header exists in BTC-Relay
        let prev_block_header =
            Self::get_block_header_from_hash(basic_block_header.hash_prev_block)?;
        // Check that the PoW hash satisfies the target set in the block header
        ensure!(
            block_header_hash.as_u256() < basic_block_header.target,
            Error::<T>::LowDiff
        );

        // Check that the diff. target is indeed correctly set in the block header, i.e., check for re-target.
        let block_height = prev_block_header.block_height + 1;

        if Self::disable_difficulty_check() {
            return Ok(basic_block_header);
        }

        let expected_target =
            if block_height >= 2016 && block_height % DIFFICULTY_ADJUSTMENT_INTERVAL == 0 {
                Self::compute_new_target(&prev_block_header, block_height)?
            } else {
                prev_block_header.block_header.target
            };

        ensure!(
            basic_block_header.target == expected_target,
            Error::<T>::DiffTargetHeader
        );

        Ok(basic_block_header)
    }

    /// Computes Bitcoin's PoW retarget algorithm for a given block height
    /// # Arguments
    ///  * `prev_block_header`: previous block header
    ///  * `block_height` : block height of new target
    fn compute_new_target(
        prev_block_header: &RichBlockHeader,
        block_height: u32,
    ) -> Result<U256, DispatchError> {
        // get time of last retarget
        let last_retarget_time =
            Self::get_last_retarget_time(prev_block_header.chain_ref, block_height)?;
        // Compute new target
        let actual_timespan = if ((prev_block_header.block_header.timestamp as u64
            - last_retarget_time) as u32)
            < (TARGET_TIMESPAN / TARGET_TIMESPAN_DIVISOR)
        {
            TARGET_TIMESPAN / TARGET_TIMESPAN_DIVISOR
        } else {
            TARGET_TIMESPAN * TARGET_TIMESPAN_DIVISOR
        };

        let new_target = U256::from(actual_timespan) * prev_block_header.block_header.target
            / U256::from(TARGET_TIMESPAN);

        // ensure target does not exceed max. target
        Ok(if new_target > UNROUNDED_MAX_TARGET {
            UNROUNDED_MAX_TARGET
        } else {
            new_target
        })
    }

    /// Returns the timestamp of the last difficulty retarget on the specified BlockChain, given the current block height
    ///
    /// # Arguments
    /// * `chain_ref` - BlockChain identifier
    /// * `block_height` - current block height
    fn get_last_retarget_time(chain_ref: u32, block_height: u32) -> Result<u64, DispatchError> {
        let block_chain = Self::get_block_chain_from_id(chain_ref)?;
        let last_retarget_header = Self::get_block_header_from_height(
            &block_chain,
            block_height - DIFFICULTY_ADJUSTMENT_INTERVAL,
        )?;
        Ok(last_retarget_header.block_header.timestamp as u64)
    }

    /// Swap the main chain with a fork. This method takes the starting height
    /// of the fork and replaces each block in the main chain with the blocks
    /// in the fork. It moves the replaced blocks in the main chain to a new
    /// fork.
    /// Last, it replaces the chain_ref of each block header in the new main
    /// chain to the MAIN_CHAIN_ID and each block header in the new fork to the
    /// new chain id.
    ///
    /// # Arguments
    ///
    /// * `fork` - the fork that is going to become the main chain
    fn swap_main_blockchain(fork: &BlockChain) -> Result<(), DispatchError> {
        // load the main chain
        let mut main_chain = Self::get_block_chain_from_id(MAIN_CHAIN_ID)?;

        // the start height of the fork
        let start_height = fork.start_height;

        // create a new blockchain element to store the part of the main chain
        // that is being forked
        // generate a chain id
        let chain_id = Self::increment_chain_counter();

        // maybe split off the no data elements
        // check if there is a no_data block element
        // that is greater than start_height
        let index_no_data = main_chain
            .no_data
            .iter()
            .position(|&h| h >= start_height)
            .map(|v| v as u32);
        let no_data = match index_no_data {
            Some(index) => main_chain.no_data.split_off(&index),
            None => BTreeSet::new(),
        };

        // maybe split off the invalid elements
        let index_invalid = main_chain
            .invalid
            .iter()
            .position(|&h| h >= start_height)
            .map(|v| v as u32);
        let invalid = match index_invalid {
            Some(index) => main_chain.invalid.split_off(&index),
            None => BTreeSet::new(),
        };

        // store the main chain part that is going to be replaced by the new fork
        // into the forked_main_chain element
        let forked_main_chain: BlockChain = BlockChain {
            chain_id,
            start_height,
            max_height: main_chain.max_height,
            no_data,
            invalid,
        };

        main_chain.max_height = fork.max_height;
        main_chain.no_data.append(&mut fork.no_data.clone());
        main_chain.invalid.append(&mut fork.invalid.clone());

        // get the best block hash
        let best_block = Self::get_block_hash(fork.chain_id, fork.max_height)?;

        // get the position of the fork in Chains
        let position: u32 = Self::get_chain_position_from_chain_id(fork.chain_id)?;

        // Update the stored main chain
        Self::set_block_chain_from_id(MAIN_CHAIN_ID, &main_chain);

        // Set BestBlock and BestBlockHeight to the submitted block
        Self::set_best_block(best_block);
        Self::set_best_block_height(main_chain.max_height);

        // remove the fork from storage
        Self::remove_blockchain_from_chain_index(fork.chain_id);
        Self::remove_blockchain_from_chain(position)?;

        // store the forked main chain
        Self::set_block_chain_from_id(forked_main_chain.chain_id, &forked_main_chain);

        // insert the reference to the forked main chain in Chains
        Self::insert_sorted(&forked_main_chain)?;

        // update all the forked block headers
        for height in fork.start_height..=forked_main_chain.max_height {
            let block_hash = Self::get_block_hash(main_chain.chain_id, height)?;
            Self::insert_block_hash(forked_main_chain.chain_id, height, block_hash);
            Self::mutate_block_header_from_chain_id(&block_hash, forked_main_chain.chain_id);
            Self::remove_block_hash(MAIN_CHAIN_ID, height);
        }

        // update all new main chain block headers
        for height in fork.start_height..=fork.max_height {
            let block = Self::get_block_hash(fork.chain_id, height)?;
            Self::mutate_block_header_from_chain_id(&block, MAIN_CHAIN_ID);
            Self::insert_block_hash(MAIN_CHAIN_ID, height, block);
        }
        <ChainsHashes>::remove_prefix(fork.chain_id);
        if !fork.is_invalid() && !fork.is_no_data() {
            Self::recover_if_needed()?
        }

        Ok(())
    }

    /// Checks if a newly inserted fork results in an update to the sorted
    /// Chains mapping. This happens when the max height of the fork is greater
    /// than the max height of the previous element in the Chains mapping.
    ///
    /// # Arguments
    ///
    /// * `fork` - the blockchain element that may cause a reorg
    fn check_and_do_reorg(fork: &BlockChain) -> Result<(), DispatchError> {
        // Check if the ordering needs updating
        // if the fork is the main chain, we don't need to update the ordering
        if fork.chain_id == MAIN_CHAIN_ID {
            return Ok(());
        }

        // TODO: remove, fix for rm head_index
        if let None = <Chains>::get(0) {
            <Chains>::insert(0, 0);
        }

        // get the position of the fork in Chains
        let fork_position: u32 = Self::get_chain_position_from_chain_id(fork.chain_id)?;
        // check if the previous element in Chains has a lower block_height
        let mut current_position = fork_position;
        let mut current_height = fork.max_height;

        // swap elements as long as previous block height is smaller
        while current_position > 0 {
            // get the previous position
            let prev_position = current_position - 1;
            // get the blockchain id
            let prev_blockchain_id =
                if let Ok(chain_id) = Self::get_chain_id_from_position(prev_position) {
                    chain_id
                } else {
                    // swap chain positions if previous doesn't exist and retry
                    Self::swap_chain(prev_position, current_position);
                    continue;
                };

            // get the previous blockchain height
            let prev_height = Self::get_block_chain_from_id(prev_blockchain_id)?.max_height;
            // swap elements if block height is greater
            if prev_height < current_height {
                // Check if swap occurs on the main chain element
                if prev_blockchain_id == MAIN_CHAIN_ID {
                    // if the previous position is the top element
                    // and the current height is more than the
                    // STABLE_TRANSACTION_CONFIRMATIONS ahead
                    // we are swapping the main chain
                    if prev_height + Self::get_stable_transaction_confirmations() < current_height {
                        Self::swap_main_blockchain(&fork)?;

                        // announce the new main chain
                        let new_chain_tip = <BestBlock>::get();
                        let block_height = <BestBlockHeight>::get();
                        let fork_depth = fork.max_height - fork.start_height;
                        Self::deposit_event(Event::ChainReorg(
                            new_chain_tip,
                            block_height,
                            fork_depth,
                        ));
                    } else {
                        Self::deposit_event(Event::ForkAheadOfMainChain(
                            prev_height,     // main chain height
                            fork.max_height, // fork height
                            fork.chain_id,   // fork id
                        ));
                    }
                    // successful reorg
                    break;
                } else {
                    // else, simply swap the chain_id ordering in Chains
                    Self::swap_chain(prev_position, current_position);
                }

                // update the current chain to the previous one
                current_position = prev_position;
                current_height = prev_height;
            } else {
                break;
            }
        }

        Ok(())
    }

    /// Insert a new fork into the Chains mapping sorted by its max height
    ///
    /// # Arguments
    ///
    /// * `blockchain` - new blockchain element
    fn insert_sorted(blockchain: &BlockChain) -> Result<(), DispatchError> {
        // get a sorted vector over the Chains elements
        // NOTE: LinkedStorageMap iterators are not sorted over the keys
        let mut chains = <Chains>::iter().collect::<Vec<(u32, u32)>>();
        chains.sort_by_key(|k| k.0);

        let max_chain_element = chains.len() as u32;
        // define the position of the new blockchain
        // by default, we insert it as the last element
        let mut position_blockchain = max_chain_element;

        // Starting from the second highest element, find where to insert the new fork
        // the previous element's block height should be higher or equal
        // the next element's block height should be lower or equal
        // NOTE: we never want to insert a new main chain through this function
        for (curr_position, curr_chain_id) in chains.iter().skip(1) {
            // get the height of the current chain_id
            let curr_height = Self::get_block_chain_from_id(*curr_chain_id)?.max_height;

            // if the height of the current blockchain is lower than
            // the new blockchain, it should be inserted at that position
            if curr_height <= blockchain.max_height {
                position_blockchain = *curr_position;
                break;
            };
        }

        // insert the new fork into the chains element
        Self::set_chain_from_position_and_id(max_chain_element, blockchain.chain_id);
        // starting from the last element swap the positions until
        // the new blockchain is at the position_blockchain
        for curr_position in (position_blockchain + 1..max_chain_element + 1).rev() {
            // stop when the blockchain element is at it's
            // designated position
            if curr_position < position_blockchain {
                break;
            };
            let prev_position = curr_position - 1;
            // swap the current element with the previous one
            Self::swap_chain(curr_position, prev_position);
        }
        Ok(())
    }

    /// Flag an error in a block header. This function is called by the
    /// security pallet.
    ///
    /// # Arguments
    ///
    /// * `block_hash` - the hash of the block header with the error
    /// * `error` - the error code for the block header
    pub fn flag_block_error(block_hash: H256Le, error: ErrorCode) -> Result<(), DispatchError> {
        // Get the chain id of the block header
        let block_header = Self::get_block_header_from_hash(block_hash)?;
        let chain_id = block_header.chain_ref;

        // Get the blockchain element for the chain id
        let mut blockchain = Self::get_block_chain_from_id(chain_id)?;

        // Flag errors in the blockchain entry
        // Check which error we are dealing with
        let newly_flagged = match error {
            ErrorCode::NoDataBTCRelay => blockchain.no_data.insert(block_header.block_height),
            ErrorCode::InvalidBTCRelay => blockchain.invalid.insert(block_header.block_height),
            _ => return Err(Error::<T>::UnknownErrorcode.into()),
        };

        // If the block was not already flagged, store the updated blockchain entry
        if newly_flagged {
            Self::mutate_block_chain_from_id(chain_id, blockchain);
            Self::deposit_event(Event::FlagBlockError(block_hash, chain_id, error));
        }

        Ok(())
    }

    /// Clear an error from a block header. This function is called by the
    /// security pallet.
    ///
    /// # Arguments
    ///
    /// * `block_hash` - the hash of the block header being cleared
    /// * `error` - the error code for the block header
    pub fn clear_block_error(block_hash: H256Le, error: ErrorCode) -> Result<(), DispatchError> {
        // Get the chain id of the block header
        let block_header = Self::get_block_header_from_hash(block_hash)?;
        let chain_id = block_header.chain_ref;

        // Get the blockchain element for the chain id
        let mut blockchain = Self::get_block_chain_from_id(chain_id)?;

        // Clear errors in the blockchain entry
        // Check which error we are dealing with
        let block_exists = match error {
            ErrorCode::NoDataBTCRelay => blockchain.no_data.remove(&block_header.block_height),
            ErrorCode::InvalidBTCRelay => blockchain.invalid.remove(&block_header.block_height),
            _ => return Err(Error::<T>::UnknownErrorcode.into()),
        };

        if block_exists {
            if !blockchain.is_invalid() && !blockchain.is_no_data() {
                Self::recover_if_needed()?
            }

            // Store the updated blockchain entry
            Self::mutate_block_chain_from_id(chain_id, blockchain);

            Self::deposit_event(Event::ClearBlockError(block_hash, chain_id, error));
        }

        Ok(())
    }

    /// Checks if the given transaction confirmations are greater/equal to the
    /// requested confirmations (and/or the global k security parameter)
    ///
    /// # Arguments
    ///
    /// * `block_height` - current main chain block height
    /// * `confirmations` - The number of confirmations requested. If `none`,
    /// the value stored in the StableBitcoinConfirmations storage item is used.
    /// * `tx_block_height` - block height of checked transaction
    pub fn check_bitcoin_confirmations(
        main_chain_height: u32,
        req_confs: Option<u32>,
        tx_block_height: u32,
    ) -> Result<(), DispatchError> {
        let required_confirmations =
            req_confs.unwrap_or_else(|| Self::get_stable_transaction_confirmations());

        if main_chain_height >= tx_block_height + required_confirmations {
            Ok(())
        } else {
            Err(Error::<T>::BitcoinConfirmations.into())
        }
    }

    /// Checks if the given bitcoin block has been stored for a sufficient
    /// amount of blocks. This should give sufficient time for staked relayers
    /// to flag potentially invalid blocks.
    ///
    /// # Arguments
    /// * `block_hash` - hash of the block to check
    pub fn check_parachain_confirmations(block_hash: H256Le) -> Result<(), DispatchError> {
        let current_height = <frame_system::Module<T>>::block_number();
        let submitted_height = <ParachainHeight<T>>::get(block_hash);

        ensure!(
            submitted_height + Self::parachain_confirmations() <= current_height,
            Error::<T>::ParachainConfirmations
        );

        Ok(())
    }

    /// Checks if transaction verification is enabled for this block height
    /// Returs an error if:
    ///   * Parachain is shutdown
    ///   * the main chain contains an invalid block
    ///   * the main chain contains a "NO_DATA" block at a lower height than `block_height`
    /// # Arguments
    ///   * `block_height` - block height of the to-be-verified transaction
    fn transaction_verification_allowed(block_height: u32) -> Result<(), DispatchError> {
        // Make sure Parachain is not shutdown
        ext::security::ensure_parachain_status_not_shutdown::<T>()?;

        // Ensure main chain has no invalid block
        let main_chain = Self::get_block_chain_from_id(MAIN_CHAIN_ID)?;
        ensure!(!main_chain.is_invalid(), Error::<T>::Invalid);

        // Check if a NO_DATA block exists at a lower height than block_height
        if main_chain.is_no_data() {
            match main_chain.no_data.iter().next_back() {
                Some(no_data_height) => ensure!(block_height < *no_data_height, Error::<T>::NoData),
                None => (),
            }
        }
        Ok(())
    }

    fn ensure_no_ongoing_fork(best_block_height: u32) -> Result<(), DispatchError> {
        // check if there is a next best fork
        match Self::get_chain_id_from_position(1) {
            // if yes, check that the main chain is at least Self::confirmations() ahead
            Ok(id) => {
                let next_best_fork_height = Self::get_block_chain_from_id(id)?.max_height;

                debug::print!("Best block height: {}", best_block_height);
                debug::print!("Next best fork height: {}", next_best_fork_height);
                // fail if there is an ongoing fork
                ensure!(
                    best_block_height
                        >= next_best_fork_height + Self::get_stable_transaction_confirmations(),
                    Error::<T>::OngoingFork
                );
            }
            // else, do nothing if there is no fork
            Err(_) => {}
        }
        Ok(())
    }

    fn recover_if_needed() -> Result<(), DispatchError> {
        if ext::security::is_parachain_error_invalid_btcrelay::<T>()
            || ext::security::is_parachain_error_no_data_btcrelay::<T>()
        {
            Ok(ext::security::recover_from_btc_relay_failure::<T>()?)
        } else {
            Ok(())
        }
    }
}

decl_event! {
    pub enum Event {
        Initialized(u32, H256Le),
        StoreMainChainHeader(u32, H256Le),
        StoreForkHeader(u32, u32, H256Le),
        ChainReorg(H256Le, u32, u32),
        ForkAheadOfMainChain(u32, u32, u32),
        VerifyTransaction(H256Le, u32, u32),
        ValidateTransaction(H256Le, u32, H160, H256Le),
        FlagBlockError(H256Le, u32, ErrorCode),
        ClearBlockError(H256Le, u32, ErrorCode),
    }
}

decl_error! {
    pub enum Error for Module<T: Trait> {
        /// Already initialized
        AlreadyInitialized,
        /// Missing the block at this height
        MissingBlockHeight,
        /// Invalid block header size
        InvalidHeaderSize,
        /// Block already stored
        DuplicateBlock,
        /// Previous block hash not found
        PrevBlock,
        /// Invalid chain ID
        InvalidChainID,
        /// PoW hash does not meet difficulty target of header
        LowDiff,
        /// Incorrect difficulty target specified in block header
        DiffTargetHeader,
        /// Malformed transaction identifier
        MalformedTxid,
        /// Transaction has less confirmations of Bitcoin blocks than required
        BitcoinConfirmations,
        /// Transaction has less confirmations of Parachain blocks than required
        ParachainConfirmations,
        /// Current fork ongoing
        OngoingFork,
        /// Merkle proof is malformed
        MalformedMerkleProof,
        /// Invalid merkle proof
        InvalidMerkleProof,
        /// Feature disabled. Reason: a main chain block with a lower height is flagged with NO_DATA.
        NoData,
        /// Feature disabled. Reason: a main chain block is flagged as INVALID.
        Invalid,
        /// BTC Parachain has shut down
        Shutdown,
        /// Transaction hash does not match given txid
        InvalidTxid,
        /// Value of payment below requested amount
        InsufficientValue,
        /// Transaction has incorrect format
        MalformedTransaction,
        /// Incorrect recipient Bitcoin address
        WrongRecipient,
        /// Incorrect transaction output format
        InvalidOutputFormat,
        /// Incorrect identifier in OP_RETURN field
        InvalidOpReturn,
        /// Invalid transaction version
        InvalidTxVersion,
        /// Expecting OP_RETURN output, but got another type
        NotOpReturn,
        /// Error code not applicable to blocks
        UnknownErrorcode,
        /// Blockchain with requested ID not found
        ForkIdNotFound,
        /// Block header not found for given hash
        BlockNotFound,
        /// Error code already reported
        AlreadyReported,
        /// Unauthorized staked relayer
        UnauthorizedRelayer,
        /// Overflow of chain counter
        ChainCounterOverflow,
        /// Overflow of block height
        BlockHeightOverflow,
        /// Underflow of stored blockchains counter
        ChainsUnderflow,
        /// EOS reached while parsing
        EOS,
        /// Format of the header is invalid
        MalformedHeader,
        /// Format of the BIP141 witness transaction output is invalid
        MalformedWitnessOutput,
        // Format of the P2PKH transaction output is invalid
        MalformedP2PKHOutput,
        // Format of the P2SH transaction output is invalid
        MalformedP2SHOutput,
        /// Format of the OP_RETURN transaction output is invalid
        MalformedOpReturnOutput,
        // Output does not match format of supported output types (Witness, P2PKH, P2SH)
        UnsupportedOutputFormat,
        // Input does not match format of supported input types (Witness, P2PKH, P2SH)
        UnsupportedInputFormat,
        /// There are no NO_DATA blocks in this BlockChain
        NoDataEmpty,
        /// User supplied an invalid address
        InvalidBtcHash,
        /// User supplied an invalid script
        InvalidScript,
        /// Specified invalid Bitcoin address
        InvalidBtcAddress,
        /// Arithmetic overflow
        ArithmeticOverflow
    }
}

impl<T: Trait> From<BitcoinError> for Error<T> {
    fn from(err: BitcoinError) -> Self {
        match err {
            BitcoinError::MalformedMerkleProof => Self::MalformedMerkleProof,
            BitcoinError::InvalidMerkleProof => Self::InvalidMerkleProof,
            BitcoinError::EOS => Self::EOS,
            BitcoinError::MalformedHeader => Self::MalformedHeader,
            BitcoinError::MalformedTransaction => Self::MalformedTransaction,
            BitcoinError::UnsupportedInputFormat => Self::UnsupportedInputFormat,
            BitcoinError::MalformedWitnessOutput => Self::MalformedWitnessOutput,
            BitcoinError::MalformedP2PKHOutput => Self::MalformedP2PKHOutput,
            BitcoinError::MalformedP2SHOutput => Self::MalformedP2SHOutput,
            BitcoinError::UnsupportedOutputFormat => Self::UnsupportedOutputFormat,
            BitcoinError::MalformedOpReturnOutput => Self::MalformedOpReturnOutput,
            BitcoinError::InvalidHeaderSize => Self::InvalidHeaderSize,
            BitcoinError::InvalidBtcHash => Self::InvalidBtcHash,
            BitcoinError::InvalidScript => Self::InvalidScript,
            BitcoinError::InvalidBtcAddress => Self::InvalidBtcAddress,
            BitcoinError::ArithmeticOverflow => Self::ArithmeticOverflow,
        }
    }
}
