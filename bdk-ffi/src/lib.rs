mod blockchain;
mod database;
mod descriptor;
mod keys;
mod psbt;
mod wallet;

use crate::blockchain::{
    Auth, Blockchain, BlockchainConfig, ElectrumConfig, EsploraConfig, RpcConfig, RpcSyncParams,
};
use crate::database::DatabaseConfig;
use crate::descriptor::Descriptor;
use crate::keys::DerivationPath;
use crate::keys::{DescriptorPublicKey, DescriptorSecretKey, Mnemonic};
use crate::psbt::PartiallySignedTransaction;
use crate::wallet::{BumpFeeTxBuilder, TxBuilder, Wallet};
use bdk::bitcoin::blockdata::script::Script as BdkScript;
use bdk::bitcoin::consensus::Decodable;
use bdk::bitcoin::psbt::serialize::Serialize;
use bdk::bitcoin::{
    Address as BdkAddress, Network, OutPoint as BdkOutPoint, Transaction as BdkTransaction, Txid,
};
use bdk::blockchain::Progress as BdkProgress;
use bdk::database::any::{SledDbConfiguration, SqliteDbConfiguration};
use bdk::keys::bip39::WordCount;
use bdk::wallet::AddressIndex as BdkAddressIndex;
use bdk::wallet::AddressInfo as BdkAddressInfo;
use bdk::{Balance as BdkBalance, BlockTime, Error as BdkError, FeeRate, KeychainKind};
use std::convert::From;
use std::fmt;
use std::io::Cursor;
use std::str::FromStr;
use std::sync::Arc;

uniffi::include_scaffolding!("bdk");

/// A output script and an amount of satoshis.
pub struct ScriptAmount {
    pub script: Arc<Script>,
    pub amount: u64,
}

/// A derived address and the index it was found at.
pub struct AddressInfo {
    /// Child index of this address
    pub index: u32,
    /// Address
    pub address: String,
}

impl From<BdkAddressInfo> for AddressInfo {
    fn from(x: bdk::wallet::AddressInfo) -> AddressInfo {
        AddressInfo {
            index: x.index,
            address: x.address.to_string(),
        }
    }
}

/// The address index selection strategy to use to derived an address from the wallet's external
/// descriptor.
pub enum AddressIndex {
    /// Return a new address after incrementing the current descriptor index.
    New,
    /// Return the address for the current descriptor index if it has not been used in a received
    /// transaction. Otherwise return a new address as with AddressIndex::New.
    /// Use with caution, if the wallet has not yet detected an address has been used it could
    /// return an already used address. This function is primarily meant for situations where the
    /// caller is untrusted; for example when deriving donation addresses on-demand for a public
    /// web page.
    LastUnused,
    /// Return the address for a specific descriptor index. Does not change the current descriptor
    /// index used by `AddressIndex::New` and `AddressIndex::LastUsed`.
    /// Use with caution, if an index is given that is less than the current descriptor index
    /// then the returned address may have already been used.
    Peek { index: u32 },
    /// Return the address for a specific descriptor index and reset the current descriptor index
    /// used by `AddressIndex::New` and `AddressIndex::LastUsed` to this value.
    /// Use with caution, if an index is given that is less than the current descriptor index
    /// then the returned address and subsequent addresses returned by calls to `AddressIndex::New`
    /// and `AddressIndex::LastUsed` may have already been used. Also if the index is reset to a
    /// value earlier than the [`crate::blockchain::Blockchain`] stop_gap (default is 20) then a
    /// larger stop_gap should be used to monitor for all possibly used addresses.
    Reset { index: u32 },
}

impl From<AddressIndex> for BdkAddressIndex {
    fn from(x: AddressIndex) -> BdkAddressIndex {
        match x {
            AddressIndex::New => BdkAddressIndex::New,
            AddressIndex::LastUnused => BdkAddressIndex::LastUnused,
            AddressIndex::Peek { index } => BdkAddressIndex::Peek(index),
            AddressIndex::Reset { index } => BdkAddressIndex::Reset(index),
        }
    }
}

/// A wallet transaction
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TransactionDetails {
    /// Transaction id.
    pub txid: String,
    /// Received value (sats)
    /// Sum of owned outputs of this transaction.
    pub received: u64,
    /// Sent value (sats)
    /// Sum of owned inputs of this transaction.
    pub sent: u64,
    /// Fee value (sats) if confirmed.
    /// The availability of the fee depends on the backend. It's never None with an Electrum
    /// Server backend, but it could be None with a Bitcoin RPC node without txindex that receive
    /// funds while offline.
    pub fee: Option<u64>,
    /// If the transaction is confirmed, contains height and timestamp of the block containing the
    /// transaction, unconfirmed transaction contains `None`.
    pub confirmation_time: Option<BlockTime>,
}

impl From<&bdk::TransactionDetails> for TransactionDetails {
    fn from(x: &bdk::TransactionDetails) -> TransactionDetails {
        TransactionDetails {
            fee: x.fee,
            txid: x.txid.to_string(),
            received: x.received,
            sent: x.sent,
            confirmation_time: x.confirmation_time.clone(),
        }
    }
}

/// A reference to a transaction output.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct OutPoint {
    /// The referenced transaction's txid.
    txid: String,
    /// The index of the referenced output in its transaction's vout.
    vout: u32,
}

impl From<&OutPoint> for BdkOutPoint {
    fn from(x: &OutPoint) -> BdkOutPoint {
        BdkOutPoint {
            txid: Txid::from_str(&x.txid).unwrap(),
            vout: x.vout,
        }
    }
}

pub struct Balance {
    // All coinbase outputs not yet matured
    pub immature: u64,
    /// Unconfirmed UTXOs generated by a wallet tx
    pub trusted_pending: u64,
    /// Unconfirmed UTXOs received from an external wallet
    pub untrusted_pending: u64,
    /// Confirmed and immediately spendable balance
    pub confirmed: u64,
    /// Get sum of trusted_pending and confirmed coins
    pub spendable: u64,
    /// Get the whole balance visible to the wallet
    pub total: u64,
}

impl From<BdkBalance> for Balance {
    fn from(bdk_balance: BdkBalance) -> Self {
        Balance {
            immature: bdk_balance.immature,
            trusted_pending: bdk_balance.trusted_pending,
            untrusted_pending: bdk_balance.untrusted_pending,
            confirmed: bdk_balance.confirmed,
            spendable: bdk_balance.get_spendable(),
            total: bdk_balance.get_total(),
        }
    }
}

/// A transaction output, which defines new coins to be created from old ones.
pub struct TxOut {
    /// The value of the output, in satoshis.
    value: u64,
    /// The address of the output.
    address: String,
}

pub struct LocalUtxo {
    outpoint: OutPoint,
    txout: TxOut,
    keychain: KeychainKind,
    is_spent: bool,
}

// This trait is used to convert the bdk TxOut type with field `script_pubkey: Script`
// into the bdk-ffi TxOut type which has a field `address: String` instead
trait NetworkLocalUtxo {
    fn from_utxo(x: &bdk::LocalUtxo, network: Network) -> LocalUtxo;
}

impl NetworkLocalUtxo for LocalUtxo {
    fn from_utxo(x: &bdk::LocalUtxo, network: Network) -> LocalUtxo {
        LocalUtxo {
            outpoint: OutPoint {
                txid: x.outpoint.txid.to_string(),
                vout: x.outpoint.vout,
            },
            txout: TxOut {
                value: x.txout.value,
                address: BdkAddress::from_script(&x.txout.script_pubkey, network)
                    .unwrap()
                    .to_string(),
            },
            keychain: x.keychain,
            is_spent: x.is_spent,
        }
    }
}

/// Trait that logs at level INFO every update received (if any).
pub trait Progress: Send + Sync + 'static {
    /// Send a new progress update. The progress value should be in the range 0.0 - 100.0, and the message value is an
    /// optional text message that can be displayed to the user.
    fn update(&self, progress: f32, message: Option<String>);
}

struct ProgressHolder {
    progress: Box<dyn Progress>,
}

impl BdkProgress for ProgressHolder {
    fn update(&self, progress: f32, message: Option<String>) -> Result<(), BdkError> {
        self.progress.update(progress, message);
        Ok(())
    }
}

impl fmt::Debug for ProgressHolder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProgressHolder").finish_non_exhaustive()
    }
}

/// A Bitcoin transaction.
#[derive(Debug)]
pub struct Transaction {
    internal: BdkTransaction,
}

impl Transaction {
    fn new(transaction_bytes: Vec<u8>) -> Result<Self, BdkError> {
        let mut decoder = Cursor::new(transaction_bytes);
        let tx: BdkTransaction = BdkTransaction::consensus_decode(&mut decoder)?;
        Ok(Transaction { internal: tx })
    }

    fn serialize(&self) -> Vec<u8> {
        self.internal.serialize()
    }

    fn weight(&self) -> u64 {
        self.internal.weight() as u64
    }

    fn size(&self) -> u64 {
        self.internal.size() as u64
    }

    fn vsize(&self) -> u64 {
        self.internal.vsize() as u64
    }
}

/// A Bitcoin address.
struct Address {
    address: BdkAddress,
}

impl Address {
    fn new(address: String) -> Result<Self, BdkError> {
        BdkAddress::from_str(address.as_str())
            .map(|a| Address { address: a })
            .map_err(|e| BdkError::Generic(e.to_string()))
    }

    fn script_pubkey(&self) -> Arc<Script> {
        Arc::new(Script {
            script: self.address.script_pubkey(),
        })
    }
}

/// A Bitcoin script.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Script {
    script: BdkScript,
}

impl Script {
    fn new(raw_output_script: Vec<u8>) -> Self {
        let script: BdkScript = BdkScript::from(raw_output_script);
        Script { script }
    }
}

#[derive(Clone, Debug)]
enum RbfValue {
    Default,
    Value(u32),
}

/// The result after calling the TxBuilder finish() function. Contains unsigned PSBT and
/// transaction details.
pub struct TxBuilderResult {
    pub(crate) psbt: Arc<PartiallySignedTransaction>,
    pub transaction_details: TransactionDetails,
}

uniffi::deps::static_assertions::assert_impl_all!(Wallet: Sync, Send);

// The goal of these tests to to ensure `bdk-ffi` intermediate code correctly calls `bdk` APIs.
// These tests should not be used to verify `bdk` behavior that is already tested in the `bdk`
// crate.
#[cfg(test)]
mod test {
    use super::Transaction;
    use bdk::bitcoin::hashes::hex::FromHex;

    // Verify that bdk-ffi Transaction can be created from valid bytes and serialized back into the same bytes.
    #[test]
    fn test_transaction_serde() {
        let test_tx_bytes = Vec::from_hex("020000000001031cfbc8f54fbfa4a33a30068841371f80dbfe166211242213188428f437445c91000000006a47304402206fbcec8d2d2e740d824d3d36cc345b37d9f65d665a99f5bd5c9e8d42270a03a8022013959632492332200c2908459547bf8dbf97c65ab1a28dec377d6f1d41d3d63e012103d7279dfb90ce17fe139ba60a7c41ddf605b25e1c07a4ddcb9dfef4e7d6710f48feffffff476222484f5e35b3f0e43f65fc76e21d8be7818dd6a989c160b1e5039b7835fc00000000171600140914414d3c94af70ac7e25407b0689e0baa10c77feffffffa83d954a62568bbc99cc644c62eb7383d7c2a2563041a0aeb891a6a4055895570000000017160014795d04cc2d4f31480d9a3710993fbd80d04301dffeffffff06fef72f000000000017a91476fd7035cd26f1a32a5ab979e056713aac25796887a5000f00000000001976a914b8332d502a529571c6af4be66399cd33379071c588ac3fda0500000000001976a914fc1d692f8de10ae33295f090bea5fe49527d975c88ac522e1b00000000001976a914808406b54d1044c429ac54c0e189b0d8061667e088ac6eb68501000000001976a914dfab6085f3a8fb3e6710206a5a959313c5618f4d88acbba20000000000001976a914eb3026552d7e3f3073457d0bee5d4757de48160d88ac0002483045022100bee24b63212939d33d513e767bc79300051f7a0d433c3fcf1e0e3bf03b9eb1d70220588dc45a9ce3a939103b4459ce47500b64e23ab118dfc03c9caa7d6bfc32b9c601210354fd80328da0f9ae6eef2b3a81f74f9a6f66761fadf96f1d1d22b1fd6845876402483045022100e29c7e3a5efc10da6269e5fc20b6a1cb8beb92130cc52c67e46ef40aaa5cac5f0220644dd1b049727d991aece98a105563416e10a5ac4221abac7d16931842d5c322012103960b87412d6e169f30e12106bdf70122aabb9eb61f455518322a18b920a4dfa887d30700").unwrap();
        let new_tx_from_bytes = Transaction::new(test_tx_bytes.clone()).unwrap();
        let serialized_tx_to_bytes = new_tx_from_bytes.serialize();
        assert_eq!(test_tx_bytes, serialized_tx_to_bytes);
    }
}
