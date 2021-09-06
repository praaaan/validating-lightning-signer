use core::convert::TryFrom;
use core::convert::TryInto;
use core::fmt::{self, Debug, Formatter};
use core::iter::FromIterator;
use core::str::FromStr;
use core::time::Duration;

use bitcoin;
use bitcoin::blockdata::constants::genesis_block;
use bitcoin::hashes::hex::ToHex;
use bitcoin::hashes::sha256::Hash as Sha256Hash;
use bitcoin::hashes::sha256d::Hash as Sha256dHash;
use bitcoin::hashes::Hash;
use bitcoin::secp256k1::ecdh::SharedSecret;
use bitcoin::secp256k1::recovery::RecoverableSignature;
use bitcoin::secp256k1::{All, Message, PublicKey, Secp256k1, SecretKey};
use bitcoin::util::bip143::SigHashCache;
use bitcoin::util::bip32::{ChildNumber, ExtendedPrivKey, ExtendedPubKey};
use bitcoin::{secp256k1, Address, Transaction, TxOut};
use bitcoin::{Network, OutPoint, Script, SigHashType};
use lightning::chain;
use lightning::chain::keysinterface::{BaseSign, KeysInterface, SpendableOutputDescriptor};
use lightning::ln::chan_utils::{
    ChannelPublicKeys, ChannelTransactionParameters, CounterpartyChannelTransactionParameters,
};
use lightning::util::logger::Logger;
use log::{info, trace};

use crate::channel::{Channel, ChannelBase, ChannelId, ChannelSetup, ChannelSlot, ChannelStub};
use crate::persist::model::NodeEntry;
use crate::persist::Persist;
use crate::policy::validator::EnforcementState;
use crate::policy::validator::{SimpleValidatorFactory, ValidatorFactory, ValidatorState};
use crate::prelude::*;
use crate::signer::my_keys_manager::{KeyDerivationStyle, MyKeysManager};
use crate::sync::{Arc, Weak};
use crate::util::crypto_utils::signature_to_bitcoin_vec;
use crate::util::invoice_utils;
use crate::util::status::{internal_error, invalid_argument, Status};
use crate::wallet::Wallet;
use lightning::ln::script::ShutdownScript;

/// Node configuration parameters.

#[derive(Copy, Clone)]
pub struct NodeConfig {
    /// The derivation style to use when deriving purpose-specific keys
    pub key_derivation_style: KeyDerivationStyle,
}

/// A signer for one Lightning node.
///
/// ```rust
/// use lightning_signer::node::{Node, NodeConfig};
/// use lightning_signer::channel::{ChannelSlot, ChannelBase};
/// use lightning_signer::persist::{DummyPersister, Persist};
/// use lightning_signer::util::test_utils::TEST_NODE_CONFIG;
/// use lightning_signer::util::test_logger::TestLogger;
/// use lightning_signer::node::SyncLogger;
///
/// use bitcoin::Network;
/// use std::sync::Arc;
///
/// let persister: Arc<dyn Persist> = Arc::new(DummyPersister {});
/// let network = Network::Testnet;
/// let seed = [0; 32];
/// let config = TEST_NODE_CONFIG;
/// let node = Arc::new(Node::new(config, &seed, network, &persister, vec![]));
/// let (channel_id, opt_stub) = node.new_channel(None, None, &node).expect("new channel");
/// assert!(opt_stub.is_some());
/// let channel_slot_mutex = node.get_channel(&channel_id).expect("get channel");
/// let channel_slot = channel_slot_mutex.lock().expect("lock");
/// match &*channel_slot {
///     ChannelSlot::Stub(stub) => {
///         // Do things with the stub, such as readying it or getting the points
///         let holder_basepoints = stub.get_channel_basepoints();
///     }
///     ChannelSlot::Ready(_) => panic!("expected a stub")
/// }
/// ```
pub struct Node {
    pub(crate) node_config: NodeConfig,
    pub(crate) keys_manager: MyKeysManager,
    channels: Mutex<Map<ChannelId, Arc<Mutex<ChannelSlot>>>>,
    pub(crate) network: Network,
    pub(crate) validator_factory: Box<dyn ValidatorFactory>,
    pub(crate) persister: Arc<dyn Persist>,
    allowlist: Mutex<UnorderedSet<Script>>,
}

impl Wallet for Node {
    fn can_spend(&self, child_path: &Vec<u32>, script_pubkey: &Script) -> Result<bool, Status> {
        // If there is no path we can't spend it ...
        if child_path.len() == 0 {
            return Ok(false);
        }

        let secp_ctx = Secp256k1::signing_only();
        let pubkey = self
            .get_wallet_key(&secp_ctx, child_path)?
            .public_key(&secp_ctx);

        // Lightning layer-1 wallets can spend native segwit or wrapped segwit addresses.
        let native_addr = Address::p2wpkh(&pubkey, self.network).expect("p2wpkh failed");
        let wrapped_addr = Address::p2shwpkh(&pubkey, self.network).expect("p2shwpkh failed");

        Ok(*script_pubkey == native_addr.script_pubkey()
            || *script_pubkey == wrapped_addr.script_pubkey())
    }

    /// Returns true if script_pubkey is in the node's allowlist.
    fn allowlist_contains(&self, script_pubkey: &Script) -> bool {
        self.allowlist.lock().unwrap().contains(script_pubkey)
    }

    fn network(&self) -> Network {
        self.network
    }
}

impl Node {
    /// Create a node.
    ///
    /// NOTE: you must persist the node yourself if it is new.
    pub fn new(
        node_config: NodeConfig,
        seed: &[u8],
        network: Network,
        persister: &Arc<Persist>,
        allowlist: Vec<Script>,
    ) -> Node {
        let now = Duration::from_secs(genesis_block(network).header.time as u64);

        Node {
            keys_manager: MyKeysManager::new(
                node_config.key_derivation_style,
                seed,
                network,
                now.as_secs(),
                now.subsec_nanos(),
            ),
            node_config,
            channels: Mutex::new(Map::new()),
            network,
            validator_factory: Box::new(SimpleValidatorFactory {}),
            persister: Arc::clone(persister),
            allowlist: Mutex::new(UnorderedSet::from_iter(allowlist)),
        }
    }

    /// Get the node ID, which is the same as the node public key
    pub fn get_id(&self) -> PublicKey {
        let secp_ctx = Secp256k1::signing_only();
        PublicKey::from_secret_key(&secp_ctx, &self.keys_manager.get_node_secret())
    }

    #[allow(dead_code)]
    pub(crate) fn get_secure_random_bytes(&self) -> [u8; 32] {
        self.keys_manager.get_secure_random_bytes()
    }

    /// Get the [Mutex] protected channel slot
    pub fn get_channel(&self, channel_id: &ChannelId) -> Result<Arc<Mutex<ChannelSlot>>, Status> {
        let mut guard = self.channels();
        let elem = guard.get_mut(channel_id);
        let slot_arc = elem.ok_or_else(|| Status::invalid_argument("no such channel"))?;
        Ok(Arc::clone(slot_arc))
    }

    /// Execute a function with an existing channel.
    ///
    /// The channel may be a stub or a ready channel.
    /// An invalid_argument [Status] will be returned if the channel does not exist.
    pub fn with_channel_base<F: Sized, T>(&self, channel_id: &ChannelId, f: F) -> Result<T, Status>
    where
        F: Fn(&mut ChannelBase) -> Result<T, Status>,
    {
        let slot_arc = self.get_channel(channel_id)?;
        let mut slot = slot_arc.lock().unwrap();
        let base = match &mut *slot {
            ChannelSlot::Stub(stub) => stub as &mut ChannelBase,
            ChannelSlot::Ready(chan) => chan as &mut ChannelBase,
        };
        f(base)
    }

    /// Execute a function with an existing ready channel.
    ///
    /// An invalid_argument [Status] will be returned if the channel does not exist.
    pub fn with_ready_channel<F: Sized, T>(&self, channel_id: &ChannelId, f: F) -> Result<T, Status>
    where
        F: Fn(&mut Channel) -> Result<T, Status>,
    {
        let slot_arc = self.get_channel(channel_id)?;
        let mut slot = slot_arc.lock().unwrap();
        match &mut *slot {
            ChannelSlot::Stub(_) => Err(invalid_argument(format!(
                "channel not ready: {}",
                &channel_id
            ))),
            ChannelSlot::Ready(chan) => f(chan),
        }
    }

    /// Get a channel given its funding outpoint, or None if no such channel exists.
    pub fn find_channel_with_funding_outpoint(
        &self,
        outpoint: &OutPoint,
    ) -> Option<Arc<Mutex<ChannelSlot>>> {
        let guard = self.channels.lock().unwrap();
        for (_, slot_arc) in guard.iter() {
            let slot = slot_arc.lock().unwrap();
            match &*slot {
                ChannelSlot::Ready(chan) => {
                    if chan.setup.funding_outpoint == *outpoint {
                        return Some(Arc::clone(slot_arc));
                    }
                }
                ChannelSlot::Stub(_stub) => {
                    // ignore stubs ...
                }
            }
        }
        None
    }

    /// Create a new channel, which starts out as a stub.
    ///
    /// The initial channel ID may be specified in `opt_channel_id`.  If the channel
    /// with this ID already exists, the existing stub is returned.
    ///
    /// If unspecified, the channel nonce will default to the channel ID.
    ///
    /// This function will return an invalid_argument [Status] if there is
    /// an existing channel with this ID and it's not a compatible stub
    /// channel.
    ///
    /// Returns the channel ID and the stub.
    // TODO the relationship between nonce and ID is different from
    // the behavior used in the gRPC driver.  Here the nonce defaults to the ID
    // but in the gRPC driver, the nonce is supplied by the caller, and the ID
    // is set to the sha256 of the nonce.
    pub fn new_channel(
        &self,
        opt_channel_id: Option<ChannelId>,
        opt_channel_nonce0: Option<Vec<u8>>,
        arc_self: &Arc<Node>,
    ) -> Result<(ChannelId, Option<ChannelStub>), Status> {
        let channel_id =
            opt_channel_id.unwrap_or_else(|| ChannelId(self.keys_manager.get_channel_id()));
        let channel_nonce0 = opt_channel_nonce0.unwrap_or_else(|| channel_id.0.to_vec());
        let mut channels = self.channels.lock().unwrap();

        // Is there a preexisting channel slot?
        let maybe_slot = channels.get(&channel_id);
        if maybe_slot.is_some() {
            match &*maybe_slot.unwrap().lock().unwrap() {
                ChannelSlot::Stub(stub) => {
                    if channel_nonce0 != stub.nonce {
                        return Err(invalid_argument(format!(
                            "new_channel nonce mismatch with existing stub: \
                             channel_id={} channel_nonce0={} stub.nonce={}",
                            channel_id,
                            channel_nonce0.to_hex(),
                            stub.nonce.to_hex()
                        )));
                    }
                    // This stub is "embryonic" (hasn't signed a commitment).  This
                    // can happen if the initial channel create to this peer failed
                    // in negotiation.  It's ok to just use this stub.
                    return Ok((channel_id, Some(stub.clone())));
                }
                ChannelSlot::Ready(_) => {
                    // Calling new_channel on a channel that's already been marked
                    // ready is not allowed.
                    return Err(invalid_argument(format!(
                        "channel already ready: {}",
                        channel_id
                    )));
                }
            };
        }

        let channel_value_sat = 0; // Placeholder value, not known yet.
        let keys = self.keys_manager.get_channel_keys_with_id(
            channel_id,
            channel_nonce0.as_slice(),
            channel_value_sat,
        );

        let stub = ChannelStub {
            node: Arc::downgrade(arc_self),
            nonce: channel_nonce0,
            secp_ctx: Secp256k1::new(),
            keys,
            id0: channel_id,
        };
        // TODO this clone is expensive
        channels.insert(
            channel_id,
            Arc::new(Mutex::new(ChannelSlot::Stub(stub.clone()))),
        );
        self.persister
            .new_channel(&self.get_id(), &stub)
            // Persist.new_channel should only fail if the channel was previously persisted.
            // So if it did fail, we have an internal error.
            .expect("channel was in storage but not in memory");
        Ok((channel_id, Some(stub)))
    }

    pub(crate) fn restore_channel(
        &self,
        channel_id0: ChannelId,
        channel_id: Option<ChannelId>,
        nonce: Vec<u8>,
        channel_value_sat: u64,
        channel_setup: Option<ChannelSetup>,
        enforcement_state: EnforcementState,
        arc_self: &Arc<Node>,
    ) -> Result<Arc<Mutex<ChannelSlot>>, ()> {
        let mut channels = self.channels.lock().unwrap();
        assert!(!channels.contains_key(&channel_id0));
        let mut keys = self.keys_manager.get_channel_keys_with_id(
            channel_id0,
            nonce.as_slice(),
            channel_value_sat,
        );

        let slot = match channel_setup {
            None => {
                let stub = ChannelStub {
                    node: Arc::downgrade(arc_self),
                    nonce,
                    secp_ctx: Secp256k1::new(),
                    keys,
                    id0: channel_id0,
                };
                // TODO this clone is expensive
                let slot = Arc::new(Mutex::new(ChannelSlot::Stub(stub.clone())));
                channels.insert(channel_id0, Arc::clone(&slot));
                channel_id.map(|id| channels.insert(id, Arc::clone(&slot)));
                slot
            }
            Some(setup) => {
                let channel_transaction_parameters =
                    Node::channel_setup_to_channel_transaction_parameters(&setup, keys.pubkeys());
                keys.ready_channel(&channel_transaction_parameters);
                let channel = Channel {
                    node: Arc::downgrade(arc_self),
                    nonce,
                    secp_ctx: Secp256k1::new(),
                    keys,
                    enforcement_state,
                    setup,
                    id0: channel_id0,
                    id: channel_id,
                };
                // TODO this clone is expensive
                let slot = Arc::new(Mutex::new(ChannelSlot::Ready(channel.clone())));
                channels.insert(channel_id0, Arc::clone(&slot));
                channel_id.map(|id| channels.insert(id, Arc::clone(&slot)));
                slot
            }
        };
        self.keys_manager.increment_channel_id_child_index();
        Ok(slot)
    }

    /// Restore a node from a persisted [NodeEntry].
    ///
    /// You can get the [NodeEntry] from [Persist::get_nodes].
    ///
    /// The channels are also restored from the `persister`.
    pub fn restore_node(
        node_id: &PublicKey,
        node_entry: NodeEntry,
        persister: Arc<dyn Persist>,
    ) -> Arc<Node> {
        let config = NodeConfig {
            key_derivation_style: KeyDerivationStyle::try_from(node_entry.key_derivation_style)
                .unwrap(),
        };
        let network = Network::from_str(node_entry.network.as_str()).expect("bad network");
        let node = Arc::new(Node::new(
            config,
            node_entry
                .seed
                .as_slice()
                .try_into()
                .expect("seed wrong length"),
            network,
            &persister,
            persister.get_node_allowlist(node_id),
        ));
        assert_eq!(&node.get_id(), node_id);
        info!("Restore node {}", node_id);
        for (channel_id0, channel_entry) in persister.get_node_channels(node_id) {
            info!("  Restore channel {}", channel_id0);
            node.restore_channel(
                channel_id0,
                channel_entry.id,
                channel_entry.nonce,
                channel_entry.channel_value_satoshis,
                channel_entry.channel_setup,
                channel_entry.enforcement_state,
                &node,
            )
            .expect("restore channel");
        }
        node
    }

    /// Restore all nodes from `persister`.
    ///
    /// The channels of each node are also restored.
    pub fn restore_nodes(persister: Arc<dyn Persist>) -> Map<PublicKey, Arc<Node>> {
        let mut nodes = Map::new();
        for (node_id, node_entry) in persister.get_nodes() {
            let node = Node::restore_node(&node_id, node_entry, Arc::clone(&persister));
            nodes.insert(node_id, node);
        }
        nodes
    }

    /// Ready a new channel, making it available for use.
    ///
    /// This populates fields that are known later in the channel creation flow,
    /// such as fields that are supplied by the counterparty and funding outpoint.
    ///
    /// * `channel_id0` - the original channel ID supplied to [`Node::new_channel`]
    /// * `opt_channel_id` - the permanent channel ID
    ///
    /// The channel is promoted from a [ChannelStub] to a [Channel].
    /// After this call, the channel may be referred to by either ID.
    pub fn ready_channel(
        &self,
        channel_id0: ChannelId,
        opt_channel_id: Option<ChannelId>,
        setup: ChannelSetup,
        holder_shutdown_key_path: &Vec<u32>,
    ) -> Result<Channel, Status> {
        let chan = {
            let channels = self.channels.lock().unwrap();
            let arcobj = channels.get(&channel_id0).ok_or_else(|| {
                invalid_argument(format!("channel does not exist: {}", channel_id0))
            })?;
            let slot = arcobj.lock().unwrap();
            let stub = match &*slot {
                ChannelSlot::Stub(stub) => Ok(stub),
                ChannelSlot::Ready(_) => Err(invalid_argument(format!(
                    "channel already ready: {}",
                    channel_id0
                ))),
            }?;
            let mut keys = stub.channel_keys_with_channel_value(setup.channel_value_sat);
            let holder_pubkeys = keys.pubkeys();
            let channel_transaction_parameters =
                Node::channel_setup_to_channel_transaction_parameters(&setup, holder_pubkeys);
            keys.ready_channel(&channel_transaction_parameters);
            Channel {
                node: Weak::clone(&stub.node),
                nonce: stub.nonce.clone(),
                secp_ctx: stub.secp_ctx.clone(),
                keys,
                enforcement_state: EnforcementState::new(),
                setup: setup.clone(),
                id0: channel_id0,
                id: opt_channel_id,
            }
        };
        let validator = self.validator_factory.make_validator(chan.network());

        validator.validate_ready_channel(self, &setup, holder_shutdown_key_path)?;

        let mut channels = self.channels.lock().unwrap();

        // Wrap the ready channel with an arc so we can potentially
        // refer to it multiple times.
        // TODO this clone is expensive
        let arcobj = Arc::new(Mutex::new(ChannelSlot::Ready(chan.clone())));

        // If a permanent channel_id was provided use it, otherwise
        // continue with the initial channel_id0.
        let chan_id = opt_channel_id.unwrap_or(channel_id0);

        // Associate the new ready channel with the channel id.
        channels.insert(chan_id, arcobj.clone());

        // If we are using a new permanent channel_id additionally
        // associate the channel with the original (initial)
        // channel_id as well.
        if channel_id0 != chan_id {
            channels.insert(channel_id0, arcobj.clone());
        }

        trace_enforcement_state!(&chan.enforcement_state);
        self.persister
            .update_channel(&self.get_id(), &chan)
            .map_err(|_| Status::internal("persist failed"))?;

        Ok(chan)
    }

    /// Sign a funding transaction.
    ///
    /// The transaction may fund multiple channels at once.
    /// Returns a witness stack for each input.  Inputs that are marked
    /// as [SpendType::Invalid] are not signed and get an empty witness stack.
    /// * `ipaths` - derivation path for the wallet key per input
    /// * `values_sat` - the amount in satoshi per input
    /// * `spendtypes` - spend type per input, or `Invalid` if this input is
    ///   to be signed by someone else.
    /// * `uniclosekeys` - an optional unilateral close key to use instead of the
    ///   wallet key.  Takes precedence over the `ipaths` entry.  This is used when
    ///   we are sweeping a unilateral close and funding a channel in a single tx.
    /// * `opaths` - derivation path for change, one per output.  Empty for
    ///   non-change outputs.
    pub fn sign_funding_tx(
        &self,
        tx: &bitcoin::Transaction,
        ipaths: &Vec<Vec<u32>>,
        values_sat: &Vec<u64>,
        spendtypes: &Vec<SpendType>,
        uniclosekeys: &Vec<Option<SecretKey>>,
        opaths: &Vec<Vec<u32>>,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, Status> {
        let secp_ctx = Secp256k1::signing_only();

        // Funding transactions cannot be associated with a single channel; a single
        // transaction may fund multiple channels

        let validator = self.validator_factory.make_validator(self.network);

        let channels: Vec<Option<Arc<Mutex<ChannelSlot>>>> = tx
            .output
            .iter()
            .enumerate()
            .map(|(ndx, _)| {
                let outpoint = OutPoint {
                    txid: tx.txid(),
                    vout: ndx as u32,
                };
                self.find_channel_with_funding_outpoint(&outpoint)
            })
            .collect();
        // TODO - initialize the state
        let state = ValidatorState { current_height: 0 };
        validator.validate_funding_tx(self, channels, &state, tx, values_sat, opaths)?;

        let mut witvec: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        for idx in 0..tx.input.len() {
            if spendtypes[idx] == SpendType::Invalid {
                // If we are signing a PSBT some of the inputs may be
                // marked as SpendType::Invalid (we skip these), push
                // an empty witness element instead.
                witvec.push((vec![], vec![]));
            } else {
                let value_sat = values_sat[idx];
                let privkey = match uniclosekeys[idx] {
                    // There was a unilateral_close_key.
                    Some(sk) => Ok(bitcoin::PrivateKey {
                        compressed: true,
                        network: Network::Testnet, // FIXME
                        key: sk,
                    }),
                    // Derive the HD key.
                    None => self.get_wallet_key(&secp_ctx, &ipaths[idx]),
                }?;
                let pubkey = privkey.public_key(&secp_ctx);
                let script_code = Address::p2pkh(&pubkey, privkey.network).script_pubkey();
                let sighash = match spendtypes[idx] {
                    SpendType::P2pkh => {
                        // legacy address
                        Message::from_slice(&tx.signature_hash(0, &script_code, 0x01)[..])
                            .map_err(|err| internal_error(format!("p2pkh sighash failed: {}", err)))
                    }
                    SpendType::P2wpkh | SpendType::P2shP2wpkh => {
                        // segwit native and wrapped
                        Message::from_slice(
                            &SigHashCache::new(tx).signature_hash(
                                idx,
                                &script_code,
                                value_sat,
                                SigHashType::All,
                            )[..],
                        )
                        .map_err(|err| internal_error(format!("p2wpkh sighash failed: {}", err)))
                    }

                    _ => Err(invalid_argument(format!(
                        "unsupported spend_type: {}",
                        spendtypes[idx] as i32
                    ))),
                }?;
                let sig = secp_ctx.sign(&sighash, &privkey.key);
                let sigvec = signature_to_bitcoin_vec(sig);

                witvec.push((sigvec, pubkey.key.serialize().to_vec()));
            }
        }
        // TODO(devrandom) self.persist_channel(node_id, chan);
        Ok(witvec)
    }

    fn channel_setup_to_channel_transaction_parameters(
        setup: &ChannelSetup,
        holder_pubkeys: &ChannelPublicKeys,
    ) -> ChannelTransactionParameters {
        let funding_outpoint = Some(chain::transaction::OutPoint {
            txid: setup.funding_outpoint.txid,
            index: setup.funding_outpoint.vout as u16,
        });
        let channel_transaction_parameters = ChannelTransactionParameters {
            holder_pubkeys: holder_pubkeys.clone(),
            holder_selected_contest_delay: setup.holder_selected_contest_delay,
            is_outbound_from_holder: setup.is_outbound,
            counterparty_parameters: Some(CounterpartyChannelTransactionParameters {
                pubkeys: setup.counterparty_points.clone(),
                selected_contest_delay: setup.counterparty_selected_contest_delay,
            }),
            funding_outpoint,
        };
        channel_transaction_parameters
    }

    pub(crate) fn get_wallet_key(
        &self,
        secp_ctx: &Secp256k1<secp256k1::SignOnly>,
        child_path: &Vec<u32>,
    ) -> Result<bitcoin::PrivateKey, Status> {
        if child_path.len() != self.node_config.key_derivation_style.get_key_path_len() {
            return Err(invalid_argument(format!(
                "get_wallet_key: bad child_path len : {}",
                child_path.len()
            )));
        }
        // Start with the base xpriv for this wallet.
        let mut xkey = self.get_account_extended_key().clone();

        // Derive the rest of the child_path.
        for elem in child_path {
            xkey = xkey
                .ckd_priv(&secp_ctx, ChildNumber::from_normal_idx(*elem).unwrap())
                .map_err(|err| internal_error(format!("derive child_path failed: {}", err)))?;
        }
        Ok(xkey.private_key)
    }

    /// Get the node secret key
    /// This function will be eliminated once the node key related items
    /// are implemented.  This includes onion decoding and p2p handshake.
    // TODO leaking secret
    pub fn get_node_secret(&self) -> SecretKey {
        self.keys_manager.get_node_secret()
    }

    /// Get destination redeemScript to encumber static protocol exit points.
    pub fn get_destination_script(&self) -> Script {
        self.keys_manager.get_destination_script()
    }

    /// Get shutdown_pubkey to use as PublicKey at channel closure
    // FIXME - this method is deprecated
    pub fn get_ldk_shutdown_scriptpubkey(&self) -> ShutdownScript {
        self.keys_manager.get_shutdown_scriptpubkey()
    }

    /// Get the layer-1 xprv
    // TODO leaking private key
    pub fn get_account_extended_key(&self) -> &ExtendedPrivKey {
        self.keys_manager.get_account_extended_key()
    }

    /// Get the layer-1 xpub
    pub fn get_account_extended_pubkey(&self) -> ExtendedPubKey {
        let secp_ctx = Secp256k1::signing_only();
        ExtendedPubKey::from_private(&secp_ctx, &self.get_account_extended_key())
    }

    /// Sign a node announcement using the node key
    pub fn sign_node_announcement(&self, na: &Vec<u8>) -> Result<Vec<u8>, Status> {
        let secp_ctx = Secp256k1::signing_only();
        let na_hash = Sha256dHash::hash(na);
        let encmsg = secp256k1::Message::from_slice(&na_hash[..])
            .map_err(|err| internal_error(format!("encmsg failed: {}", err)))?;
        let sig = secp_ctx.sign(&encmsg, &self.get_node_secret());
        let res = sig.serialize_der().to_vec();
        Ok(res)
    }

    /// Sign a channel update using the node key
    pub fn sign_channel_update(&self, cu: &Vec<u8>) -> Result<Vec<u8>, Status> {
        let secp_ctx = Secp256k1::signing_only();
        let cu_hash = Sha256dHash::hash(cu);
        let encmsg = secp256k1::Message::from_slice(&cu_hash[..])
            .map_err(|err| internal_error(format!("encmsg failed: {}", err)))?;
        let sig = secp_ctx.sign(&encmsg, &self.get_node_secret());
        let res = sig.serialize_der().to_vec();
        Ok(res)
    }

    /// Sign an invoice
    pub fn sign_invoice_in_parts(
        &self,
        data_part: &Vec<u8>,
        human_readable_part: &String,
    ) -> Result<Vec<u8>, Status> {
        use bitcoin::bech32::CheckBase32;

        let hash = invoice_utils::hash_from_parts(
            human_readable_part.as_bytes(),
            &data_part.check_base32().expect("needs to be base32 data"),
        );

        let secp_ctx = Secp256k1::signing_only();
        let encmsg = secp256k1::Message::from_slice(&hash[..])
            .map_err(|err| internal_error(format!("encmsg failed: {}", err)))?;
        let node_secret = SecretKey::from_slice(self.get_node_secret().as_ref()).unwrap();
        let sig = secp_ctx.sign_recoverable(&encmsg, &node_secret);
        let (rid, sig) = sig.serialize_compact();
        let mut res = sig.to_vec();
        res.push(rid.to_i32() as u8);
        Ok(res)
    }

    /// Sign an invoice
    pub fn sign_invoice(&self, invoice_preimage: &Vec<u8>) -> RecoverableSignature {
        let secp_ctx = Secp256k1::signing_only();
        let hash = Sha256Hash::hash(invoice_preimage);
        let message = secp256k1::Message::from_slice(&hash).unwrap();
        secp_ctx.sign_recoverable(&message, &self.get_node_secret())
    }

    /// Sign a Lightning message
    pub fn sign_message(&self, message: &Vec<u8>) -> Result<Vec<u8>, Status> {
        let mut buffer = String::from("Lightning Signed Message:").into_bytes();
        buffer.extend(message);
        let secp_ctx = Secp256k1::signing_only();
        let hash = Sha256dHash::hash(&buffer);
        let encmsg = secp256k1::Message::from_slice(&hash[..])
            .map_err(|err| internal_error(format!("encmsg failed: {}", err)))?;
        let sig = secp_ctx.sign_recoverable(&encmsg, &self.get_node_secret());
        let (rid, sig) = sig.serialize_compact();
        let mut res = sig.to_vec();
        res.push(rid.to_i32() as u8);
        Ok(res)
    }

    /// Get the channels this node knows about.
    /// Currently, channels are not pruned once closed, but this will change.
    pub fn channels(&self) -> MutexGuard<Map<ChannelId, Arc<Mutex<ChannelSlot>>>> {
        self.channels.lock().unwrap()
    }

    /// Perform an ECDH operation between the node key and a public key
    /// This can be used for onion packet decoding
    pub fn ecdh(&self, other_key: &PublicKey) -> Vec<u8> {
        let our_key = self.keys_manager.get_node_secret();
        let ss = SharedSecret::new(&other_key, &our_key);
        ss[..].to_vec()
    }

    /// See [`MyKeysManager::spend_spendable_outputs`].
    ///
    /// For LDK compatibility.
    pub fn spend_spendable_outputs(
        &self,
        descriptors: &[&SpendableOutputDescriptor],
        outputs: Vec<TxOut>,
        change_destination_script: Script,
        feerate_sat_per_1000_weight: u32,
        secp_ctx: &Secp256k1<All>,
    ) -> Result<Transaction, ()> {
        self.keys_manager.spend_spendable_outputs(
            descriptors,
            outputs,
            change_destination_script,
            feerate_sat_per_1000_weight,
            secp_ctx,
        )
    }

    /// Returns the node's current allowlist.
    pub fn allowlist(&self) -> Result<Vec<String>, Status> {
        let alset = self.allowlist.lock().unwrap();
        (*alset)
            .iter()
            .map(|script_pubkey| {
                let addr = Address::from_script(&script_pubkey, self.network);
                if addr.is_none() {
                    return Err(invalid_argument(format!(
                        "address from script faied on {}",
                        &script_pubkey
                    )));
                }
                Ok(addr.unwrap().to_string())
            })
            .collect::<Result<Vec<String>, Status>>()
    }

    /// Adds addresses to the node's current allowlist.
    pub fn add_allowlist(&self, addlist: &Vec<String>) -> Result<(), Status> {
        let addresses = addlist
            .iter()
            .map(|addrstr| {
                let addr = addrstr.parse::<Address>().map_err(|err| {
                    invalid_argument(format!("parse address {} failed: {}", addrstr, err))
                })?;
                if addr.network != self.network {
                    return Err(invalid_argument(format!(
                        "network mismatch for addr {}: addr={}, node={}",
                        addr, addr.network, self.network
                    )));
                }
                Ok(addr)
            })
            .collect::<Result<Vec<Address>, Status>>()?;
        let mut alset = self.allowlist.lock().unwrap();
        for addr in addresses {
            alset.insert(addr.script_pubkey());
        }
        let wlvec = (*alset).iter().cloned().collect();
        self.persister
            .update_node_allowlist(&self.get_id(), wlvec)
            .map_err(|_| Status::internal("persist failed"))?;
        Ok(())
    }

    /// Removes addresses from the node's current allowlist.
    pub fn remove_allowlist(&self, rmlist: &Vec<String>) -> Result<(), Status> {
        let addresses = rmlist
            .iter()
            .map(|addrstr| {
                let addr = addrstr.parse::<Address>().map_err(|err| {
                    invalid_argument(format!("parse address {} failed: {}", addrstr, err))
                })?;
                if addr.network != self.network {
                    return Err(invalid_argument(format!(
                        "network mismatch for addr {}: addr={}, node={}",
                        addr, addr.network, self.network
                    )));
                }
                Ok(addr)
            })
            .collect::<Result<Vec<Address>, Status>>()?;
        let mut alset = self.allowlist.lock().unwrap();
        for addr in addresses {
            alset.remove(&addr.script_pubkey());
        }
        let wlvec = (*alset).iter().cloned().collect();
        self.persister
            .update_node_allowlist(&self.get_id(), wlvec)
            .map_err(|_| Status::internal("persist failed"))?;
        Ok(())
    }
}

impl Debug for Node {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("node")
    }
}

/// The type of address, for layer-1 input signing
#[derive(PartialEq, Clone, Copy)]
#[repr(i32)]
pub enum SpendType {
    /// To be signed by someone else
    Invalid = 0,
    /// Pay to public key hash
    P2pkh = 1,
    /// Pay to witness public key hash
    P2wpkh = 3,
    /// Pay to p2sh wrapped p2wpkh
    P2shP2wpkh = 4,
}

impl TryFrom<i32> for SpendType {
    type Error = ();

    fn try_from(i: i32) -> Result<Self, Self::Error> {
        let res = match i {
            x if x == SpendType::Invalid as i32 => SpendType::Invalid,
            x if x == SpendType::P2pkh as i32 => SpendType::P2pkh,
            x if x == SpendType::P2wpkh as i32 => SpendType::P2wpkh,
            x if x == SpendType::P2shP2wpkh as i32 => SpendType::P2shP2wpkh,
            _ => return Err(()),
        };
        Ok(res)
    }
}

/// Marker trait for LDK compatible logger
pub trait SyncLogger: Logger + SendSync {}

#[cfg(test)]
mod tests {
    use std::mem;

    use bitcoin;
    use bitcoin::blockdata::opcodes;
    use bitcoin::blockdata::script::Builder;
    use bitcoin::consensus::deserialize;
    use bitcoin::hash_types::Txid;
    use bitcoin::hashes::hash160::Hash as Hash160;
    use bitcoin::hashes::hex::{FromHex, ToHex};
    use bitcoin::hashes::sha256d::Hash as Sha256dHash;
    use bitcoin::hashes::Hash;
    use bitcoin::secp256k1;
    use bitcoin::secp256k1::recovery::{RecoverableSignature, RecoveryId};
    use bitcoin::secp256k1::{Message, SecretKey, Signature};
    use bitcoin::util::bip143::SigHashCache;
    use bitcoin::util::psbt::serialize::Serialize;
    use bitcoin::{Address, OutPoint, Script, SigHashType, Transaction, TxIn, TxOut};
    use lightning::chain::keysinterface::BaseSign;
    use lightning::ln::chan_utils::{
        build_htlc_transaction, get_htlc_redeemscript, get_revokeable_redeemscript,
        make_funding_redeemscript, BuiltCommitmentTransaction, ChannelPublicKeys,
        ChannelTransactionParameters, HTLCOutputInCommitment, TxCreationKeys,
    };
    use lightning::ln::PaymentHash;
    use test_env_log::test;

    use crate::channel::channel_nonce_to_id;
    use crate::channel::{ChannelBase, ChannelSetup, CommitmentType};
    use crate::policy::error::policy_error;
    use crate::policy::validator::EnforcementState;
    use crate::tx::tx::{build_close_tx, CommitmentInfo2, HTLCInfo2, ANCHOR_SAT};
    use crate::util::crypto_utils::{
        derive_private_revocation_key, derive_public_key, derive_revocation_pubkey,
        signature_to_bitcoin_vec,
    };
    use crate::util::status::{internal_error, invalid_argument, Code, Status};
    use crate::util::test_utils::*;
    use crate::util::test_utils::{hex_decode, hex_encode};

    use super::*;

    macro_rules! hex (($hex:expr) => (Vec::from_hex($hex).unwrap()));
    macro_rules! hex_script (($hex:expr) => (Script::from(hex!($hex))));

    #[test]
    fn channel_debug_test() {
        let (node, channel_id) =
            init_node_and_channel(TEST_NODE_CONFIG, TEST_SEED[1], make_test_channel_setup());
        let _status: Result<(), Status> = node.with_ready_channel(&channel_id, |chan| {
            assert_eq!(format!("{:?}", chan), "channel");
            Ok(())
        });
    }

    #[test]
    fn ready_channel_test() {
        let (node, channel_id) =
            init_node_and_channel(TEST_NODE_CONFIG, TEST_SEED[1], make_test_channel_setup());
        node.with_ready_channel(&channel_id, |c| {
            let params = c.keys.get_channel_parameters();
            assert!(params.is_outbound_from_holder);
            assert_eq!(params.holder_selected_contest_delay, 6);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn ready_channel_not_exist_test() {
        let node = init_node(TEST_NODE_CONFIG, TEST_SEED[1]);
        let channel_nonce_x = "nonceX".as_bytes().to_vec();
        let channel_id_x = channel_nonce_to_id(&channel_nonce_x);
        let status: Result<_, Status> =
            node.ready_channel(channel_id_x, None, make_test_channel_setup(), &vec![]);
        assert!(status.is_err());
        let err = status.unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
        assert_eq!(
            err.message(),
            format!("channel does not exist: {}", &channel_id_x)
        );
    }

    #[test]
    fn ready_channel_dual_channelid_test() {
        let node = init_node(TEST_NODE_CONFIG, TEST_SEED[1]);
        let channel_nonce = "nonce1".as_bytes().to_vec();
        let channel_id = channel_nonce_to_id(&channel_nonce);
        node.new_channel(Some(channel_id), Some(channel_nonce), &node)
            .expect("new_channel");

        // Issue ready_channel w/ an alternate id.
        let channel_nonce_x = "nonceX".as_bytes().to_vec();
        let channel_id_x = channel_nonce_to_id(&channel_nonce_x);
        node.ready_channel(
            channel_id,
            Some(channel_id_x),
            make_test_channel_setup(),
            &vec![],
        )
        .expect("ready_channel");

        // Original channel_id should work with_ready_channel.
        let val = node
            .with_ready_channel(&channel_id, |_chan| Ok(42))
            .expect("u32");
        assert_eq!(val, 42);

        // Alternate channel_id should work with_ready_channel.
        let val_x = node
            .with_ready_channel(&channel_id_x, |_chan| Ok(43))
            .expect("u32");
        assert_eq!(val_x, 43);
    }

    #[test]
    fn with_ready_channel_not_exist_test() {
        let (node, _channel_id) =
            init_node_and_channel(TEST_NODE_CONFIG, TEST_SEED[1], make_test_channel_setup());
        let channel_nonce_x = "nonceX".as_bytes().to_vec();
        let channel_id_x = channel_nonce_to_id(&channel_nonce_x);

        let status: Result<(), Status> = node.with_ready_channel(&channel_id_x, |_chan| Ok(()));
        assert!(status.is_err());
        let err = status.unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
        assert_eq!(err.message(), "no such channel");
    }

    #[test]
    fn node_debug_test() {
        let (node, _channel_id) =
            init_node_and_channel(TEST_NODE_CONFIG, TEST_SEED[1], make_test_channel_setup());
        assert_eq!(format!("{:?}", node), "node");
    }

    #[test]
    fn node_invalid_argument_test() {
        let err = invalid_argument("testing invalid_argument");
        assert_eq!(err.code(), Code::InvalidArgument);
        assert_eq!(err.message(), "testing invalid_argument");
    }

    #[test]
    fn node_internal_error_test() {
        let err = internal_error("testing internal_error");
        assert_eq!(err.code(), Code::Internal);
        assert_eq!(err.message(), "testing internal_error");
    }

    #[test]
    fn channel_stub_test() {
        let node = init_node(TEST_NODE_CONFIG, TEST_SEED[1]);
        let channel_nonce = "nonce1".as_bytes().to_vec();
        let channel_id = channel_nonce_to_id(&channel_nonce);
        node.new_channel(Some(channel_id), Some(channel_nonce), &node)
            .expect("new_channel");

        // with_ready_channel should return not ready.
        let result: Result<(), Status> = node.with_ready_channel(&channel_id, |_chan| {
            assert!(false); // shouldn't get here
            Ok(())
        });
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
        assert_eq!(
            err.message(),
            format!("channel not ready: {}", TEST_CHANNEL_ID[0]),
        );

        let _: Result<(), Status> = node.with_channel_base(&channel_id, |base| {
            // get_per_commitment_point for the first commitment should work.
            let result = base.get_per_commitment_point(0);
            assert!(result.is_ok());

            // get_per_commitment_point for future commit_num should policy-fail.
            assert_failed_precondition_err!(
                base.get_per_commitment_point(1),
                "policy failure: channel stub can only return point for commitment number zero"
            );

            // get_per_commitment_secret never works for a stub.
            assert_failed_precondition_err!(
                base.get_per_commitment_secret(0),
                "policy failure: channel stub cannot release commitment secret"
            );

            Ok(())
        });

        let basepoints = node
            .with_channel_base(&channel_id, |base| Ok(base.get_channel_basepoints()))
            .unwrap();
        // get_channel_basepoints should work.
        check_basepoints(&basepoints);

        // check_future_secret should work.
        let n: u64 = 10;
        let suggested = SecretKey::from_slice(
            hex_decode("4220531d6c8b15d66953c46b5c4d67c921943431452d5543d8805b9903c6b858")
                .unwrap()
                .as_slice(),
        )
        .unwrap();
        let correct = node
            .with_channel_base(&channel_id, |base| base.check_future_secret(n, &suggested))
            .unwrap();
        assert_eq!(correct, true);

        let notcorrect = node
            .with_channel_base(&channel_id, |base| {
                base.check_future_secret(n + 1, &suggested)
            })
            .unwrap();
        assert_eq!(notcorrect, false);
    }

    #[ignore] // Ignore this test while we allow extra NewChannel calls.
    #[test]
    fn node_new_channel_already_exists_test() {
        let (node, _channel_id) =
            init_node_and_channel(TEST_NODE_CONFIG, TEST_SEED[1], make_test_channel_setup());

        // Try and create the channel again.
        let channel_nonce = "nonce1".as_bytes().to_vec();
        let channel_id = channel_nonce_to_id(&channel_nonce);
        let result = node.new_channel(Some(channel_id), Some(channel_nonce), &node);
        let err = result.err().unwrap();
        assert_eq!(err.code(), Code::InvalidArgument);
        assert_eq!(
            err.message(),
            format!("channel already exists: {}", TEST_CHANNEL_ID[0])
        );
    }

    #[test]
    fn ready_channel_already_ready_test() {
        let (node, channel_id) =
            init_node_and_channel(TEST_NODE_CONFIG, TEST_SEED[1], make_test_channel_setup());

        // Trying to ready it again should fail.
        let result = node.ready_channel(channel_id, None, make_test_channel_setup(), &vec![]);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument);
        assert_eq!(
            err.message(),
            format!("channel already ready: {}", TEST_CHANNEL_ID[0])
        );
    }

    #[test]
    fn ready_channel_unknown_holder_shutdown_script() {
        let node = init_node(TEST_NODE_CONFIG, TEST_SEED[1]);
        let channel_nonce = "nonce1".as_bytes().to_vec();
        let channel_id = channel_nonce_to_id(&channel_nonce);
        node.new_channel(Some(channel_id), Some(channel_nonce), &node)
            .expect("new_channel");
        let mut setup = make_test_channel_setup();
        setup.holder_shutdown_script =
            Some(hex_script!("0014be56df7de366ad8ee9ccdad54e9a9993e99ef565"));
        let holder_shutdown_key_path = vec![];
        assert_failed_precondition_err!(
            node.ready_channel(channel_id, None, setup.clone(), &holder_shutdown_key_path),
            "policy failure: validate_ready_channel: \
             holder_shutdown_script is not in wallet or allowlist"
        );
    }

    #[test]
    fn ready_channel_holder_shutdown_script_in_allowlist() {
        let node = init_node(TEST_NODE_CONFIG, TEST_SEED[1]);
        let channel_nonce = "nonce1".as_bytes().to_vec();
        let channel_id = channel_nonce_to_id(&channel_nonce);
        node.new_channel(Some(channel_id), Some(channel_nonce), &node)
            .expect("new_channel");
        let mut setup = make_test_channel_setup();
        setup.holder_shutdown_script =
            Some(hex_script!("0014be56df7de366ad8ee9ccdad54e9a9993e99ef565"));
        node.add_allowlist(&vec![
            "tb1qhetd7l0rv6kca6wvmt25ax5ej05eaat9q29z7z".to_string()
        ])
        .expect("added allowlist");
        let holder_shutdown_key_path = vec![];
        assert_status_ok!(node.ready_channel(
            channel_id,
            None,
            setup.clone(),
            &holder_shutdown_key_path
        ));
    }

    #[test]
    fn ready_channel_holder_shutdown_script_in_wallet() {
        let node = init_node(TEST_NODE_CONFIG, TEST_SEED[1]);
        let channel_nonce = "nonce1".as_bytes().to_vec();
        let channel_id = channel_nonce_to_id(&channel_nonce);
        node.new_channel(Some(channel_id), Some(channel_nonce), &node)
            .expect("new_channel");
        let mut setup = make_test_channel_setup();
        setup.holder_shutdown_script =
            Some(hex_script!("0014b76dd61e41b5ef052af21cda3260888c070bb9af"));
        let holder_shutdown_key_path = vec![7];
        assert_status_ok!(node.ready_channel(
            channel_id,
            None,
            setup.clone(),
            &holder_shutdown_key_path
        ));
    }

    #[test]
    fn sign_counterparty_commitment_tx_static_test() {
        let setup = make_test_channel_setup();
        sign_counterparty_commitment_tx_test(&setup);
    }

    #[test]
    fn sign_counterparty_commitment_tx_legacy_test() {
        let mut setup = make_test_channel_setup();
        setup.commitment_type = CommitmentType::Legacy;
        sign_counterparty_commitment_tx_test(&setup);
    }

    fn sign_counterparty_commitment_tx_test(setup: &ChannelSetup) {
        let (node, channel_id) =
            init_node_and_channel(TEST_NODE_CONFIG, TEST_SEED[1], setup.clone());
        let remote_percommitment_point = make_test_pubkey(10);
        let counterparty_points = make_test_counterparty_points();
        let (sig, tx) = node
            .with_ready_channel(&channel_id, |chan| {
                let channel_parameters = chan.make_channel_parameters();
                let parameters = channel_parameters.as_counterparty_broadcastable();
                let keys = chan
                    .make_counterparty_tx_keys(&remote_percommitment_point)
                    .unwrap();
                let commit_num = 23;
                let feerate_per_kw = 0;
                let to_broadcaster = 2_000_000;
                let to_countersignatory = 1_000_000;
                let mut htlcs = vec![];

                // Set the commit_num and revoke_num.
                chan.enforcement_state
                    .set_next_counterparty_commit_num_for_testing(
                        commit_num,
                        make_test_pubkey(0x10),
                    );
                chan.enforcement_state
                    .set_next_counterparty_revoke_num_for_testing(commit_num - 1);

                let commitment_tx = chan.make_counterparty_commitment_tx(
                    &remote_percommitment_point,
                    commit_num,
                    feerate_per_kw,
                    to_broadcaster,
                    to_countersignatory,
                    htlcs.clone(),
                );

                let redeem_scripts = build_tx_scripts(
                    &keys,
                    to_countersignatory,
                    to_broadcaster,
                    &mut htlcs,
                    &parameters,
                )
                .expect("scripts");
                let output_witscripts = redeem_scripts.iter().map(|s| s.serialize()).collect();

                // rebuild to get the scripts
                let trusted_tx = commitment_tx.trust();
                let tx = trusted_tx.built_transaction();

                let sig = chan
                    .sign_counterparty_commitment_tx(
                        &tx.transaction,
                        &output_witscripts,
                        &remote_percommitment_point,
                        commit_num,
                        feerate_per_kw,
                        vec![],
                        vec![],
                    )
                    .expect("sign");
                Ok((sig, tx.transaction.clone()))
            })
            .expect("build_commitment_tx");

        assert_eq!(
            tx.txid().to_hex(),
            "770f45e5093d10ed3c7dc05f152bcf954200015cca98e701811714b6a4132b38"
        );

        let funding_pubkey = get_channel_funding_pubkey(&node, &channel_id);
        let channel_funding_redeemscript =
            make_funding_redeemscript(&funding_pubkey, &counterparty_points.funding_pubkey);

        check_signature(
            &tx,
            0,
            signature_to_bitcoin_vec(sig),
            &funding_pubkey,
            setup.channel_value_sat,
            &channel_funding_redeemscript,
        );
    }

    #[test]
    #[ignore] // we don't support anchors yet
    fn sign_counterparty_commitment_tx_with_anchors_test() {
        let mut setup = make_test_channel_setup();
        setup.commitment_type = CommitmentType::Anchors;
        let (node, channel_id) =
            init_node_and_channel(TEST_NODE_CONFIG, TEST_SEED[1], setup.clone());
        let remote_percommitment_point = make_test_pubkey(10);
        let counterparty_points = make_test_counterparty_points();
        let to_counterparty_value_sat = 2_000_000;
        let to_holder_value_sat =
            setup.channel_value_sat - to_counterparty_value_sat - (2 * ANCHOR_SAT);
        let feerate_per_kw = 0;
        let (sig, tx) = node
            .with_ready_channel(&channel_id, |chan| {
                let info = chan.build_counterparty_commitment_info(
                    &remote_percommitment_point,
                    to_holder_value_sat,
                    to_counterparty_value_sat,
                    vec![],
                    vec![],
                )?;
                let commit_num = 23;
                let (tx, output_scripts, _) =
                    chan.build_commitment_tx(&remote_percommitment_point, commit_num, &info)?;
                let output_witscripts = output_scripts.iter().map(|s| s.serialize()).collect();
                let sig = chan
                    .sign_counterparty_commitment_tx(
                        &tx,
                        &output_witscripts,
                        &remote_percommitment_point,
                        commit_num,
                        feerate_per_kw,
                        vec![],
                        vec![],
                    )
                    .expect("sign");
                Ok((sig, tx))
            })
            .expect("build_commitment_tx");

        assert_eq!(
            tx.txid().to_hex(),
            "68a0916cea22e66438f0cd2c50f667866ebd16f59ba395352602bd817d6c0fd9"
        );

        let funding_pubkey = get_channel_funding_pubkey(&node, &channel_id);
        let channel_funding_redeemscript =
            make_funding_redeemscript(&funding_pubkey, &counterparty_points.funding_pubkey);

        check_signature(
            &tx,
            0,
            signature_to_bitcoin_vec(sig),
            &funding_pubkey,
            setup.channel_value_sat,
            &channel_funding_redeemscript,
        );
    }

    #[test]
    fn sign_counterparty_commitment_tx_with_htlc_static_test() {
        let setup = make_test_channel_setup();
        sign_counterparty_commitment_tx_with_htlc_test(&setup);
    }

    #[test]
    fn sign_counterparty_commitment_tx_with_htlc_legacy_test() {
        let mut setup = make_test_channel_setup();
        setup.commitment_type = CommitmentType::Legacy;
        sign_counterparty_commitment_tx_with_htlc_test(&setup);
    }

    fn sign_counterparty_commitment_tx_with_htlc_test(setup: &ChannelSetup) {
        let (node, channel_id) =
            init_node_and_channel(TEST_NODE_CONFIG, TEST_SEED[1], setup.clone());

        let remote_percommitment_point = make_test_pubkey(10);
        let counterparty_points = make_test_counterparty_points();

        let htlc1 = HTLCInfo2 {
            value_sat: 1,
            payment_hash: PaymentHash([1; 32]),
            cltv_expiry: 2 << 16,
        };

        let htlc2 = HTLCInfo2 {
            value_sat: 1,
            payment_hash: PaymentHash([3; 32]),
            cltv_expiry: 3 << 16,
        };

        let htlc3 = HTLCInfo2 {
            value_sat: 1,
            payment_hash: PaymentHash([5; 32]),
            cltv_expiry: 4 << 16,
        };

        let offered_htlcs = vec![htlc1];
        let received_htlcs = vec![htlc2, htlc3];

        let (sig, tx) = node
            .with_ready_channel(&channel_id, |chan| {
                let channel_parameters = chan.make_channel_parameters();
                let parameters = channel_parameters.as_counterparty_broadcastable();
                let mut htlcs =
                    Channel::htlcs_info2_to_oic(offered_htlcs.clone(), received_htlcs.clone());
                let keys = chan
                    .make_counterparty_tx_keys(&remote_percommitment_point)
                    .unwrap();
                let to_broadcaster_value_sat = 1_000_000;
                let to_countersignatory_value_sat = 1_999_997;
                let redeem_scripts = build_tx_scripts(
                    &keys,
                    to_broadcaster_value_sat,
                    to_countersignatory_value_sat,
                    &mut htlcs,
                    &parameters,
                )
                .expect("scripts");

                let commit_num = 23;
                let feerate_per_kw = 0;

                // Set the commit_num and revoke_num.
                chan.enforcement_state
                    .set_next_counterparty_commit_num_for_testing(
                        commit_num,
                        make_test_pubkey(0x10),
                    );
                chan.enforcement_state
                    .set_next_counterparty_revoke_num_for_testing(commit_num - 1);

                let commitment_tx = chan.make_counterparty_commitment_tx(
                    &remote_percommitment_point,
                    commit_num,
                    feerate_per_kw,
                    to_countersignatory_value_sat,
                    to_broadcaster_value_sat,
                    htlcs,
                );
                // rebuild to get the scripts
                let trusted_tx = commitment_tx.trust();
                let tx = trusted_tx.built_transaction();
                let output_witscripts = redeem_scripts.iter().map(|s| s.serialize()).collect();
                let sig = chan
                    .sign_counterparty_commitment_tx(
                        &tx.transaction,
                        &output_witscripts,
                        &remote_percommitment_point,
                        commit_num,
                        feerate_per_kw,
                        offered_htlcs.clone(),
                        received_htlcs.clone(),
                    )
                    .expect("sign");
                Ok((sig, tx.transaction.clone()))
            })
            .expect("build_commitment_tx");

        assert_eq!(
            tx.txid().to_hex(),
            "3f3238ed033a13ab1cf43d8eb6e81e5beca2080f9530a13931c10f40e04697fb"
        );

        let funding_pubkey = get_channel_funding_pubkey(&node, &channel_id);
        let channel_funding_redeemscript =
            make_funding_redeemscript(&funding_pubkey, &counterparty_points.funding_pubkey);

        check_signature(
            &tx,
            0,
            signature_to_bitcoin_vec(sig),
            &funding_pubkey,
            setup.channel_value_sat,
            &channel_funding_redeemscript,
        );
    }

    #[test]
    #[ignore] // we don't support anchors yet
    fn sign_counterparty_commitment_tx_with_htlc_and_anchors_test() {
        let mut setup = make_test_channel_setup();
        setup.commitment_type = CommitmentType::Anchors;
        let (node, channel_id) =
            init_node_and_channel(TEST_NODE_CONFIG, TEST_SEED[1], setup.clone());

        let remote_percommitment_point = make_test_pubkey(10);
        let counterparty_points = make_test_counterparty_points();

        let htlc1 = HTLCInfo2 {
            value_sat: 1,
            payment_hash: PaymentHash([1; 32]),
            cltv_expiry: 2 << 16,
        };

        let htlc2 = HTLCInfo2 {
            value_sat: 1,
            payment_hash: PaymentHash([3; 32]),
            cltv_expiry: 3 << 16,
        };

        let htlc3 = HTLCInfo2 {
            value_sat: 1,
            payment_hash: PaymentHash([5; 32]),
            cltv_expiry: 4 << 16,
        };

        let offered_htlcs = vec![htlc1.clone()];
        let received_htlcs = vec![htlc2.clone(), htlc3.clone()];
        let feerate_per_kw = 0;

        let to_counterparty_value_sat = 2_000_000;
        let to_holder_value_sat =
            setup.channel_value_sat - to_counterparty_value_sat - 3 - (2 * ANCHOR_SAT);

        let (sig, tx) = node
            .with_ready_channel(&channel_id, |chan| {
                let info = chan.build_counterparty_commitment_info(
                    &remote_percommitment_point,
                    to_holder_value_sat,
                    to_counterparty_value_sat,
                    offered_htlcs.clone(),
                    received_htlcs.clone(),
                )?;
                let commit_num = 23;
                let (tx, output_scripts, _) =
                    chan.build_commitment_tx(&remote_percommitment_point, commit_num, &info)?;
                let output_witscripts = output_scripts.iter().map(|s| s.serialize()).collect();
                let sig = chan
                    .sign_counterparty_commitment_tx(
                        &tx,
                        &output_witscripts,
                        &remote_percommitment_point,
                        commit_num,
                        feerate_per_kw,
                        offered_htlcs.clone(),
                        received_htlcs.clone(),
                    )
                    .expect("sign");
                Ok((sig, tx))
            })
            .expect("build_commitment_tx");

        assert_eq!(
            tx.txid().to_hex(),
            "52aa09518edbdbd77ca56790efbb9392710c3bed10d7d27b04d98f6f6d8a207d"
        );

        let funding_pubkey = get_channel_funding_pubkey(&node, &channel_id);
        let channel_funding_redeemscript =
            make_funding_redeemscript(&funding_pubkey, &counterparty_points.funding_pubkey);

        check_signature(
            &tx,
            0,
            signature_to_bitcoin_vec(sig),
            &funding_pubkey,
            setup.channel_value_sat,
            &channel_funding_redeemscript,
        );
    }

    #[test]
    fn sign_counterparty_commitment_tx_phase2_static_test() {
        let setup = make_test_channel_setup();
        sign_counterparty_commitment_tx_phase2_test(&setup);
    }

    #[test]
    fn sign_counterparty_commitment_tx_phase2_legacy_test() {
        let mut setup = make_test_channel_setup();
        setup.commitment_type = CommitmentType::Legacy;
        sign_counterparty_commitment_tx_phase2_test(&setup);
    }

    fn sign_counterparty_commitment_tx_phase2_test(setup: &ChannelSetup) {
        let (node, channel_id) =
            init_node_and_channel(TEST_NODE_CONFIG, TEST_SEED[1], setup.clone());

        let remote_percommitment_point = make_test_pubkey(10);
        let funding_pubkey = get_channel_funding_pubkey(&node, &channel_id);

        let commit_num = 23;
        let to_holder_value_sat = 1_000_000;
        let to_counterparty_value_sat = 2_000_000;

        let tx = node
            .with_ready_channel(&channel_id, |chan| {
                // Set the commit_num and revoke_num.
                chan.enforcement_state
                    .set_next_counterparty_commit_num_for_testing(
                        commit_num,
                        make_test_pubkey(0x10),
                    );
                chan.enforcement_state
                    .set_next_counterparty_revoke_num_for_testing(commit_num - 1);

                let commitment_tx = chan.make_counterparty_commitment_tx(
                    &remote_percommitment_point,
                    commit_num,
                    0,
                    to_holder_value_sat,
                    to_counterparty_value_sat,
                    vec![],
                );
                let trusted_tx = commitment_tx.trust();
                let tx = trusted_tx.built_transaction();
                assert_eq!(
                    tx.txid.to_hex(),
                    "75a87d13138017f2c62c86be375e526821a40805e5f31808bf782ce7e13fe951"
                );
                Ok(tx.transaction.clone())
            })
            .expect("build");
        let (ser_signature, _) = node
            .with_ready_channel(&channel_id, |chan| {
                chan.sign_counterparty_commitment_tx_phase2(
                    &remote_percommitment_point,
                    commit_num,
                    0, // we are not looking at HTLCs yet
                    to_holder_value_sat,
                    to_counterparty_value_sat,
                    vec![],
                    vec![],
                )
            })
            .expect("sign");
        let channel_funding_redeemscript =
            make_funding_redeemscript(&funding_pubkey, &setup.counterparty_points.funding_pubkey);

        check_signature(
            &tx,
            0,
            ser_signature,
            &funding_pubkey,
            setup.channel_value_sat,
            &channel_funding_redeemscript,
        );
    }

    #[test]
    fn sign_holder_commitment_tx_phase2_static_test() {
        let setup = make_test_channel_setup();
        sign_holder_commitment_tx_phase2_test(&setup);
    }

    #[test]
    fn sign_holder_commitment_tx_phase2_legacy_test() {
        let mut setup = make_test_channel_setup();
        setup.commitment_type = CommitmentType::Legacy;
        sign_holder_commitment_tx_phase2_test(&setup);
    }

    fn sign_holder_commitment_tx_phase2_test(setup: &ChannelSetup) {
        let (node, channel_id) =
            init_node_and_channel(TEST_NODE_CONFIG, TEST_SEED[1], setup.clone());

        let commit_num = 23;
        let to_holder_value_sat = 1_000_000;
        let to_counterparty_value_sat = 2_000_000;
        let tx = node
            .with_ready_channel(&channel_id, |chan| {
                chan.enforcement_state
                    .set_next_holder_commit_num_for_testing(commit_num);

                let commitment_tx = chan
                    .make_holder_commitment_tx(
                        commit_num,
                        0,
                        to_holder_value_sat,
                        to_counterparty_value_sat,
                        vec![],
                    )
                    .expect("holder_commitment_tx");
                Ok(commitment_tx
                    .trust()
                    .built_transaction()
                    .transaction
                    .clone())
            })
            .expect("build");
        let (ser_signature, _) = node
            .with_ready_channel(&channel_id, |chan| {
                chan.sign_holder_commitment_tx_phase2(
                    commit_num,
                    0, // feerate not used
                    to_holder_value_sat,
                    to_counterparty_value_sat,
                    vec![],
                    vec![],
                )
            })
            .expect("sign");
        assert_eq!(
            tx.txid().to_hex(),
            "deb063aa75d0a43fecd8330a150dce8fd794d835c0b6db97b755cb8cfa3803fc"
        );

        let funding_pubkey = get_channel_funding_pubkey(&node, &channel_id);
        let channel_funding_redeemscript =
            make_funding_redeemscript(&funding_pubkey, &setup.counterparty_points.funding_pubkey);

        check_signature(
            &tx,
            0,
            ser_signature,
            &funding_pubkey,
            setup.channel_value_sat,
            &channel_funding_redeemscript,
        );
    }

    fn get_channel_funding_pubkey(node: &Node, channel_id: &ChannelId) -> PublicKey {
        let res: Result<PublicKey, Status> =
            node.with_ready_channel(&channel_id, |chan| Ok(chan.keys.pubkeys().funding_pubkey));
        res.unwrap()
    }

    fn get_channel_htlc_pubkey(
        node: &Node,
        channel_id: &ChannelId,
        remote_per_commitment_point: &PublicKey,
    ) -> PublicKey {
        let res: Result<PublicKey, Status> = node.with_ready_channel(&channel_id, |chan| {
            let secp_ctx = &chan.secp_ctx;
            let pubkey = derive_public_key(
                &secp_ctx,
                &remote_per_commitment_point,
                &chan.keys.pubkeys().htlc_basepoint,
            )
            .unwrap();
            Ok(pubkey)
        });
        res.unwrap()
    }

    fn get_channel_delayed_payment_pubkey(
        node: &Node,
        channel_id: &ChannelId,
        remote_per_commitment_point: &PublicKey,
    ) -> PublicKey {
        let res: Result<PublicKey, Status> = node.with_ready_channel(&channel_id, |chan| {
            let secp_ctx = &chan.secp_ctx;
            let pubkey = derive_public_key(
                &secp_ctx,
                &remote_per_commitment_point,
                &chan.keys.pubkeys().delayed_payment_basepoint,
            )
            .unwrap();
            Ok(pubkey)
        });
        res.unwrap()
    }

    fn get_channel_revocation_pubkey(
        node: &Node,
        channel_id: &ChannelId,
        revocation_point: &PublicKey,
    ) -> PublicKey {
        let res: Result<PublicKey, Status> = node.with_ready_channel(&channel_id, |chan| {
            let secp_ctx = &chan.secp_ctx;
            let pubkey = derive_revocation_pubkey(
                secp_ctx,
                revocation_point, // matches revocation_secret
                &chan.keys.pubkeys().revocation_basepoint,
            )
            .unwrap();
            Ok(pubkey)
        });
        res.unwrap()
    }

    fn check_signature(
        tx: &bitcoin::Transaction,
        input: usize,
        ser_signature: Vec<u8>,
        pubkey: &PublicKey,
        input_value_sat: u64,
        redeemscript: &Script,
    ) {
        check_signature_with_setup(
            tx,
            input,
            ser_signature,
            pubkey,
            input_value_sat,
            redeemscript,
            &make_test_channel_setup(),
        )
    }

    fn check_signature_with_setup(
        tx: &bitcoin::Transaction,
        input: usize,
        ser_signature: Vec<u8>,
        pubkey: &PublicKey,
        input_value_sat: u64,
        redeemscript: &Script,
        setup: &ChannelSetup,
    ) {
        let sig_hash_type = if setup.option_anchor_outputs() {
            SigHashType::SinglePlusAnyoneCanPay
        } else {
            SigHashType::All
        };

        let sighash = Message::from_slice(
            &SigHashCache::new(tx).signature_hash(
                input,
                &redeemscript,
                input_value_sat,
                sig_hash_type,
            )[..],
        )
        .expect("sighash");
        let mut der_signature = ser_signature.clone();
        der_signature.pop(); // Pop the sighash type byte
        let signature = Signature::from_der(&der_signature).expect("from_der");
        let secp_ctx = Secp256k1::new();
        secp_ctx
            .verify(&sighash, &signature, &pubkey)
            .expect("verify");
    }

    #[test]
    fn new_channel_test() {
        let node = init_node(TEST_NODE_CONFIG, TEST_SEED[0]);

        let (channel_id, _) = node.new_channel(None, None, &node).unwrap();
        assert!(node.get_channel(&channel_id).is_ok());
    }

    #[test]
    fn bad_channel_lookup_test() -> Result<(), ()> {
        let node = init_node(TEST_NODE_CONFIG, TEST_SEED[0]);
        let channel_id = ChannelId([1; 32]);
        assert!(node.get_channel(&channel_id).is_err());
        Ok(())
    }

    fn check_basepoints(basepoints: &ChannelPublicKeys) {
        assert_eq!(
            basepoints.funding_pubkey.serialize().to_vec().to_hex(),
            "02868b7bc9b6d307509ed97758636d2d3628970bbd3bd36d279f8d3cde8ccd45ae"
        );
        assert_eq!(
            basepoints
                .revocation_basepoint
                .serialize()
                .to_vec()
                .to_hex(),
            "02982b69bb2d70b083921cbc862c0bcf7761b55d7485769ddf81c2947155b1afe4"
        );
        assert_eq!(
            basepoints.payment_point.serialize().to_vec().to_hex(),
            "026bb6655b5e0b5ff80d078d548819f57796013b09de8085ddc04b49854ae1e483"
        );
        assert_eq!(
            basepoints
                .delayed_payment_basepoint
                .serialize()
                .to_vec()
                .to_hex(),
            "0291dfb201bc87a2da8c7ffe0a7cf9691962170896535a7fd00d8ee4406a405e98"
        );
        assert_eq!(
            basepoints.htlc_basepoint.serialize().to_vec().to_hex(),
            "02c0c8ff7278e50bd07d7b80c109621d44f895e216400a7e95b09f544eb3fafee2"
        );
    }

    #[test]
    fn get_channel_basepoints_test() {
        let (node, channel_id) =
            init_node_and_channel(TEST_NODE_CONFIG, TEST_SEED[1], make_test_channel_setup());

        let basepoints = node
            .with_channel_base(&channel_id, |base| Ok(base.get_channel_basepoints()))
            .unwrap();

        check_basepoints(&basepoints);
    }

    #[test]
    fn get_per_commitment_point_and_secret_test() {
        let (node, channel_id) =
            init_node_and_channel(TEST_NODE_CONFIG, TEST_SEED[1], make_test_channel_setup());

        let commit_num = 23;

        let (point, secret) = node
            .with_ready_channel(&channel_id, |chan| {
                // The channel next_holder_commit_num must be 2 past the
                // requested commit_num for get_per_commitment_secret.
                chan.enforcement_state
                    .set_next_holder_commit_num_for_testing(commit_num + 2);
                let point = chan.get_per_commitment_point(commit_num)?;
                let secret = chan.get_per_commitment_secret(commit_num)?;
                Ok((point, secret))
            })
            .expect("point");

        let derived_point = PublicKey::from_secret_key(&Secp256k1::new(), &secret);

        assert_eq!(point, derived_point);
    }

    #[test]
    fn get_check_future_secret_test() {
        let (node, channel_id) =
            init_node_and_channel(TEST_NODE_CONFIG, TEST_SEED[1], make_test_channel_setup());

        let n: u64 = 10;

        let suggested = SecretKey::from_slice(
            hex_decode("4220531d6c8b15d66953c46b5c4d67c921943431452d5543d8805b9903c6b858")
                .unwrap()
                .as_slice(),
        )
        .unwrap();

        let correct = node
            .with_channel_base(&channel_id, |base| base.check_future_secret(n, &suggested))
            .unwrap();
        assert_eq!(correct, true);

        let notcorrect = node
            .with_channel_base(&channel_id, |base| {
                base.check_future_secret(n + 1, &suggested)
            })
            .unwrap();
        assert_eq!(notcorrect, false);
    }

    #[test]
    fn sign_funding_tx_p2wpkh_test() -> Result<(), ()> {
        let secp_ctx = Secp256k1::signing_only();
        let node = init_node(TEST_NODE_CONFIG, TEST_SEED[0]);
        let ipaths = vec![vec![0u32], vec![1u32]];
        let ival0 = 100u64;
        let ival1 = 300u64;
        let chanamt = 300u64;
        let values_sat = vec![ival0, ival1];

        let input1 = TxIn {
            previous_output: OutPoint {
                txid: Default::default(),
                vout: 0,
            },
            script_sig: Script::new(),
            sequence: 0,
            witness: vec![],
        };

        let input2 = TxIn {
            previous_output: OutPoint {
                txid: Default::default(),
                vout: 1,
            },
            script_sig: Script::new(),
            sequence: 0,
            witness: vec![],
        };
        let (opath, mut tx) = make_test_funding_tx(&secp_ctx, &node, vec![input1, input2], chanamt);
        let spendtypes = vec![SpendType::P2wpkh, SpendType::P2wpkh];
        let uniclosekeys = vec![None, None];

        let witvec = node
            .sign_funding_tx(
                &tx,
                &ipaths,
                &values_sat,
                &spendtypes,
                &uniclosekeys,
                &vec![opath],
            )
            .expect("good sigs");
        assert_eq!(witvec.len(), 2);

        let address = |n: u32| {
            Address::p2wpkh(
                &node
                    .get_wallet_key(&secp_ctx, &vec![n])
                    .unwrap()
                    .public_key(&secp_ctx),
                Network::Testnet,
            )
            .unwrap()
        };

        tx.input[0].witness = vec![witvec[0].0.clone(), witvec[0].1.clone()];
        tx.input[1].witness = vec![witvec[1].0.clone(), witvec[1].1.clone()];

        let outs = vec![
            TxOut {
                value: ival0,
                script_pubkey: address(0).script_pubkey(),
            },
            TxOut {
                value: ival1,
                script_pubkey: address(1).script_pubkey(),
            },
        ];
        let verify_result = tx.verify(|p| Some(outs[p.vout as usize].clone()));

        assert!(verify_result.is_ok());

        Ok(())
    }

    #[test]
    fn sign_funding_tx_p2wpkh_test1() -> Result<(), ()> {
        let secp_ctx = Secp256k1::signing_only();
        let node = init_node(TEST_NODE_CONFIG, TEST_SEED[0]);
        let txid = bitcoin::Txid::from_slice(&[2u8; 32]).unwrap();
        let ipaths = vec![vec![0u32]];
        let ival0 = 200u64;
        let chanamt = 100u64;
        let values_sat = vec![ival0];

        let input1 = TxIn {
            previous_output: OutPoint { txid, vout: 0 },
            script_sig: Script::new(),
            sequence: 0,
            witness: vec![],
        };

        let (opath, mut tx) = make_test_funding_tx(&secp_ctx, &node, vec![input1], chanamt);
        let spendtypes = vec![SpendType::P2wpkh];
        let uniclosekeys = vec![None];

        let witvec = node
            .sign_funding_tx(
                &tx,
                &ipaths,
                &values_sat,
                &spendtypes,
                &uniclosekeys,
                &vec![opath],
            )
            .expect("good sigs");
        assert_eq!(witvec.len(), 1);

        let address = |n: u32| {
            Address::p2wpkh(
                &node
                    .get_wallet_key(&secp_ctx, &vec![n])
                    .unwrap()
                    .public_key(&secp_ctx),
                Network::Testnet,
            )
            .unwrap()
        };

        tx.input[0].witness = vec![witvec[0].0.clone(), witvec[0].1.clone()];

        println!("{:?}", tx.input[0].script_sig);
        let outs = vec![TxOut {
            value: ival0,
            script_pubkey: address(0).script_pubkey(),
        }];
        println!("{:?}", &outs[0].script_pubkey);
        let verify_result = tx.verify(|p| Some(outs[p.vout as usize].clone()));

        assert!(verify_result.is_ok());

        Ok(())
    }

    // policy-v1-funding-fee-range
    #[test]
    fn sign_funding_tx_fee_too_low() {
        let secp_ctx = Secp256k1::signing_only();
        let node = init_node(TEST_NODE_CONFIG, TEST_SEED[0]);
        let txid = bitcoin::Txid::from_slice(&[2u8; 32]).unwrap();
        let ipaths = vec![vec![0u32]];
        let ival0 = 199u64;
        let chanamt = 100u64;
        let values_sat = vec![ival0];

        let input1 = TxIn {
            previous_output: OutPoint { txid, vout: 0 },
            script_sig: Script::new(),
            sequence: 0,
            witness: vec![],
        };

        let (opath, tx) = make_test_funding_tx(&secp_ctx, &node, vec![input1], chanamt);
        let spendtypes = vec![SpendType::P2wpkh];
        let uniclosekeys = vec![None];

        assert_failed_precondition_err!(
            node.sign_funding_tx(
                &tx,
                &ipaths,
                &values_sat,
                &spendtypes,
                &uniclosekeys,
                &vec![opath.clone()],
            ),
            "policy failure: validate_fee: validate_funding_tx: fee below minimum: 99 < 100"
        );
    }

    // policy-v1-funding-fee-range
    #[test]
    fn sign_funding_tx_fee_too_high() {
        let secp_ctx = Secp256k1::signing_only();
        let node = init_node(TEST_NODE_CONFIG, TEST_SEED[0]);
        let txid = bitcoin::Txid::from_slice(&[2u8; 32]).unwrap();
        let ipaths = vec![vec![0u32]];
        let fee = 22_000u64;
        let ival0 = 100u64 + fee;
        let chanamt = 100u64;
        let values_sat = vec![ival0];

        let input1 = TxIn {
            previous_output: OutPoint { txid, vout: 0 },
            script_sig: Script::new(),
            sequence: 0,
            witness: vec![],
        };

        let (opath, tx) = make_test_funding_tx(&secp_ctx, &node, vec![input1], chanamt);
        let spendtypes = vec![SpendType::P2wpkh];
        let uniclosekeys = vec![None];

        assert_failed_precondition_err!(
            node.sign_funding_tx(
                &tx,
                &ipaths,
                &values_sat,
                &spendtypes,
                &uniclosekeys,
                &vec![opath.clone()],
            ),
            "policy failure: validate_fee: validate_funding_tx: above maximum: 22000 > 21000"
        );
    }

    #[test]
    fn sign_funding_tx_unilateral_close_info_test() -> Result<(), ()> {
        let secp_ctx = Secp256k1::signing_only();
        let node = init_node(TEST_NODE_CONFIG, TEST_SEED[0]);
        let txid = bitcoin::Txid::from_slice(&[2u8; 32]).unwrap();
        let ival0 = 300u64;
        let chanamt = 200u64;
        let ipaths = vec![vec![0u32]];
        let values_sat = vec![ival0];

        let input1 = TxIn {
            previous_output: OutPoint { txid, vout: 0 },
            script_sig: Script::new(),
            sequence: 0,
            witness: vec![],
        };

        let (opath, mut tx) = make_test_funding_tx(&secp_ctx, &node, vec![input1], chanamt);
        let spendtypes = vec![SpendType::P2wpkh];

        let uniclosekey = SecretKey::from_slice(
            hex_decode("4220531d6c8b15d66953c46b5c4d67c921943431452d5543d8805b9903c6b858")
                .unwrap()
                .as_slice(),
        )
        .unwrap();
        let uniclosepubkey = bitcoin::PublicKey::from_slice(
            &PublicKey::from_secret_key(&secp_ctx, &uniclosekey).serialize()[..],
        )
        .unwrap();
        let uniclosekeys = vec![Some(uniclosekey)];

        let witvec = node
            .sign_funding_tx(
                &tx,
                &ipaths,
                &values_sat,
                &spendtypes,
                &uniclosekeys,
                &vec![opath],
            )
            .expect("good sigs");
        assert_eq!(witvec.len(), 1);

        assert_eq!(witvec[0].1, uniclosepubkey.serialize());

        let address = Address::p2wpkh(&uniclosepubkey, Network::Testnet).unwrap();

        tx.input[0].witness = vec![witvec[0].0.clone(), witvec[0].1.clone()];
        println!("{:?}", tx.input[0].script_sig);
        let outs = vec![TxOut {
            value: ival0,
            script_pubkey: address.script_pubkey(),
        }];
        println!("{:?}", &outs[0].script_pubkey);
        let verify_result = tx.verify(|p| Some(outs[p.vout as usize].clone()));

        assert!(verify_result.is_ok());

        Ok(())
    }

    #[test]
    fn sign_funding_tx_p2pkh_test() -> Result<(), ()> {
        let secp_ctx = Secp256k1::signing_only();
        let node = init_node(TEST_NODE_CONFIG, TEST_SEED[0]);
        let txid = bitcoin::Txid::from_slice(&[2u8; 32]).unwrap();
        let ipaths = vec![vec![0u32]];
        let values_sat = vec![200u64];

        let input1 = TxIn {
            previous_output: OutPoint { txid, vout: 0 },
            script_sig: Script::new(),
            sequence: 0,
            witness: vec![],
        };

        let (opath, mut tx) = make_test_funding_tx(&secp_ctx, &node, vec![input1], 100);
        let spendtypes = vec![SpendType::P2pkh];
        let uniclosekeys = vec![None];

        let witvec = node
            .sign_funding_tx(
                &tx,
                &ipaths,
                &values_sat,
                &spendtypes,
                &uniclosekeys,
                &vec![opath],
            )
            .expect("good sigs");
        assert_eq!(witvec.len(), 1);

        let address = |n: u32| {
            Address::p2pkh(
                &node
                    .get_wallet_key(&secp_ctx, &vec![n])
                    .unwrap()
                    .public_key(&secp_ctx),
                Network::Testnet,
            )
        };

        tx.input[0].script_sig = Builder::new()
            .push_slice(witvec[0].0.as_slice())
            .push_slice(witvec[0].1.as_slice())
            .into_script();
        println!("{:?}", tx.input[0].script_sig);
        let outs = vec![TxOut {
            value: 100,
            script_pubkey: address(0).script_pubkey(),
        }];
        println!("{:?}", &outs[0].script_pubkey);
        let verify_result = tx.verify(|p| Some(outs[p.vout as usize].clone()));
        assert!(verify_result.is_ok());

        Ok(())
    }

    #[test]
    fn sign_funding_tx_p2sh_p2wpkh_test() -> Result<(), ()> {
        let secp_ctx = Secp256k1::signing_only();
        let node = init_node(TEST_NODE_CONFIG, TEST_SEED[0]);
        let txid = bitcoin::Txid::from_slice(&[2u8; 32]).unwrap();
        let ipaths = vec![vec![0u32]];
        let ival0 = 200u64;
        let chanamt = 100u64;
        let values_sat = vec![ival0];

        let input1 = TxIn {
            previous_output: OutPoint { txid, vout: 0 },
            script_sig: Script::new(),
            sequence: 0,
            witness: vec![],
        };

        let (opath, mut tx) =
            make_test_funding_tx_with_p2shwpkh_change(&secp_ctx, &node, vec![input1], chanamt);
        let spendtypes = vec![SpendType::P2shP2wpkh];
        let uniclosekeys = vec![None];

        let witvec = node
            .sign_funding_tx(
                &tx,
                &ipaths,
                &values_sat,
                &spendtypes,
                &uniclosekeys,
                &vec![opath],
            )
            .expect("good sigs");
        assert_eq!(witvec.len(), 1);

        let address = |n: u32| {
            Address::p2shwpkh(
                &node
                    .get_wallet_key(&secp_ctx, &vec![n])
                    .unwrap()
                    .public_key(&secp_ctx),
                Network::Testnet,
            )
            .unwrap()
        };

        let pubkey = &node
            .get_wallet_key(&secp_ctx, &ipaths[0])
            .unwrap()
            .public_key(&secp_ctx);

        let keyhash = Hash160::hash(&pubkey.serialize()[..]);

        tx.input[0].script_sig = Builder::new()
            .push_slice(
                Builder::new()
                    .push_opcode(opcodes::all::OP_PUSHBYTES_0)
                    .push_slice(&keyhash.into_inner())
                    .into_script()
                    .as_bytes(),
            )
            .into_script();

        tx.input[0].witness = vec![witvec[0].0.clone(), witvec[0].1.clone()];

        println!("{:?}", tx.input[0].script_sig);
        let outs = vec![TxOut {
            value: ival0,
            script_pubkey: address(0).script_pubkey(),
        }];
        println!("{:?}", &outs[0].script_pubkey);
        let verify_result = tx.verify(|p| Some(outs[p.vout as usize].clone()));

        assert!(verify_result.is_ok());

        Ok(())
    }

    #[test]
    fn sign_funding_tx_psbt_test() -> Result<(), ()> {
        let secp_ctx = Secp256k1::signing_only();
        let node = init_node(TEST_NODE_CONFIG, TEST_SEED[0]);
        let txids = vec![
            bitcoin::Txid::from_slice(&[2u8; 32]).unwrap(),
            bitcoin::Txid::from_slice(&[4u8; 32]).unwrap(),
            bitcoin::Txid::from_slice(&[6u8; 32]).unwrap(),
        ];

        let inputs = vec![
            TxIn {
                previous_output: OutPoint {
                    txid: txids[0],
                    vout: 0,
                },
                script_sig: Script::new(),
                sequence: 0,
                witness: vec![],
            },
            TxIn {
                previous_output: OutPoint {
                    txid: txids[1],
                    vout: 0,
                },
                script_sig: Script::new(),
                sequence: 0,
                witness: vec![],
            },
            TxIn {
                previous_output: OutPoint {
                    txid: txids[2],
                    vout: 0,
                },
                script_sig: Script::new(),
                sequence: 0,
                witness: vec![],
            },
        ];

        let (opath, tx) = make_test_funding_tx(&secp_ctx, &node, inputs, 100);
        let ipaths = vec![vec![0u32], vec![1u32], vec![2u32]];
        let values_sat = vec![100u64, 101u64, 102u64];
        let spendtypes = vec![
            SpendType::Invalid,
            SpendType::P2shP2wpkh,
            SpendType::Invalid,
        ];
        let uniclosekeys = vec![None, None, None];

        let witvec = node
            .sign_funding_tx(
                &tx,
                &ipaths,
                &values_sat,
                &spendtypes,
                &uniclosekeys,
                &vec![opath],
            )
            .expect("good sigs");
        // Should have three witness stack items.
        assert_eq!(witvec.len(), 3);

        // First item should be empty sig/pubkey.
        assert_eq!(witvec[0].0.len(), 0);
        assert_eq!(witvec[0].1.len(), 0);

        // Second should have values.
        assert!(witvec[1].0.len() > 0);
        assert!(witvec[1].1.len() > 0);

        // Third should be empty.
        assert_eq!(witvec[2].0.len(), 0);
        assert_eq!(witvec[2].1.len(), 0);

        // Doesn't verify, not fully signed.
        Ok(())
    }

    fn sign_funding_tx_with_mutator<TxMutator>(txmut: TxMutator) -> Result<(), Status>
    where
        TxMutator: Fn(&mut Transaction),
    {
        let is_p2sh = false;
        let node_ctx = test_node_ctx(1);

        let incoming = 5_000_000;
        let channel_amount = 3_000_000;
        let fee = 1000;
        let change = incoming - channel_amount - fee;

        let mut chan_ctx = test_chan_ctx(&node_ctx, 1, channel_amount);
        let mut tx_ctx = test_funding_tx_ctx();

        funding_tx_add_wallet_input(&mut tx_ctx, is_p2sh, 1, incoming);
        funding_tx_add_wallet_output(&node_ctx, &mut tx_ctx, is_p2sh, 1, change);
        let outpoint_ndx =
            funding_tx_add_channel_outpoint(&node_ctx, &chan_ctx, &mut tx_ctx, channel_amount);

        let mut tx = funding_tx_from_ctx(&tx_ctx);

        // mutate the tx before calling funding_tx_ready_channel so txid will be valid
        txmut(&mut tx);

        funding_tx_ready_channel(&node_ctx, &mut chan_ctx, &tx, outpoint_ndx);

        let mut commit_tx_ctx = channel_initial_holder_commitment(&node_ctx, &chan_ctx);
        let (csig, hsigs) =
            counterparty_sign_holder_commitment(&node_ctx, &chan_ctx, &mut commit_tx_ctx);
        validate_holder_commitment(&node_ctx, &chan_ctx, &commit_tx_ctx, &csig, &hsigs)?;

        let witvec = funding_tx_sign(&node_ctx, &tx_ctx, &tx)?;
        funding_tx_validate_sig(&node_ctx, &tx_ctx, &mut tx, &witvec);
        Ok(())
    }

    #[test]
    fn sign_funding_tx_with_no_mut_test() {
        let status = sign_funding_tx_with_mutator(|_tx| {
            // don't mutate the tx, should pass
        });
        assert!(status.is_ok());
    }

    // policy-v1-funding-format-standard
    #[test]
    fn sign_funding_tx_with_version_1() {
        assert_failed_precondition_err!(
            sign_funding_tx_with_mutator(|tx| {
                tx.version = 1;
            }),
            "policy failure: validate_funding_tx: invalid version: 1"
        );
    }

    // policy-v1-funding-format-standard
    #[test]
    fn sign_funding_tx_with_version_3() {
        assert_failed_precondition_err!(
            sign_funding_tx_with_mutator(|tx| {
                tx.version = 3;
            }),
            "policy failure: validate_funding_tx: invalid version: 3"
        );
    }

    fn sign_funding_tx_with_output_and_change(is_p2sh: bool) {
        let node_ctx = test_node_ctx(1);

        let incoming = 5_000_000;
        let channel_amount = 3_000_000;
        let fee = 1000;
        let change = incoming - channel_amount - fee;

        let mut chan_ctx = test_chan_ctx(&node_ctx, 1, channel_amount);
        let mut tx_ctx = test_funding_tx_ctx();

        funding_tx_add_wallet_input(&mut tx_ctx, is_p2sh, 1, incoming);
        funding_tx_add_wallet_output(&node_ctx, &mut tx_ctx, is_p2sh, 1, change);
        let outpoint_ndx =
            funding_tx_add_channel_outpoint(&node_ctx, &chan_ctx, &mut tx_ctx, channel_amount);

        let mut tx = funding_tx_from_ctx(&tx_ctx);

        funding_tx_ready_channel(&node_ctx, &mut chan_ctx, &tx, outpoint_ndx);

        let mut commit_tx_ctx = channel_initial_holder_commitment(&node_ctx, &chan_ctx);
        let (csig, hsigs) =
            counterparty_sign_holder_commitment(&node_ctx, &chan_ctx, &mut commit_tx_ctx);
        validate_holder_commitment(&node_ctx, &chan_ctx, &commit_tx_ctx, &csig, &hsigs)
            .expect("valid holder commitment");

        let witvec = funding_tx_sign(&node_ctx, &tx_ctx, &tx).expect("witvec");
        funding_tx_validate_sig(&node_ctx, &tx_ctx, &mut tx, &witvec);
    }

    #[test]
    fn sign_funding_tx_with_p2wpkh_wallet() {
        sign_funding_tx_with_output_and_change(false);
    }

    #[test]
    fn sign_funding_tx_with_p2sh_wallet() {
        sign_funding_tx_with_output_and_change(true);
    }

    #[test]
    fn sign_funding_tx_with_multiple_wallet_inputs() {
        let is_p2sh = false;
        let node_ctx = test_node_ctx(1);

        let incoming0 = 2_000_000;
        let incoming1 = 3_000_000;
        let channel_amount = 3_000_000;
        let fee = 1000;
        let change = incoming0 + incoming1 - channel_amount - fee;

        let mut chan_ctx = test_chan_ctx(&node_ctx, 1, channel_amount);
        let mut tx_ctx = test_funding_tx_ctx();

        funding_tx_add_wallet_input(&mut tx_ctx, is_p2sh, 1, incoming0);
        funding_tx_add_wallet_input(&mut tx_ctx, is_p2sh, 2, incoming1);

        funding_tx_add_wallet_output(&node_ctx, &mut tx_ctx, is_p2sh, 1, change);
        let outpoint_ndx =
            funding_tx_add_channel_outpoint(&node_ctx, &chan_ctx, &mut tx_ctx, channel_amount);

        let mut tx = funding_tx_from_ctx(&tx_ctx);

        funding_tx_ready_channel(&node_ctx, &mut chan_ctx, &tx, outpoint_ndx);

        let mut commit_tx_ctx = channel_initial_holder_commitment(&node_ctx, &chan_ctx);
        let (csig, hsigs) =
            counterparty_sign_holder_commitment(&node_ctx, &chan_ctx, &mut commit_tx_ctx);
        validate_holder_commitment(&node_ctx, &chan_ctx, &commit_tx_ctx, &csig, &hsigs)
            .expect("valid holder commitment");

        let witvec = funding_tx_sign(&node_ctx, &tx_ctx, &tx).expect("witvec");
        funding_tx_validate_sig(&node_ctx, &tx_ctx, &mut tx, &witvec);
    }

    #[test]
    fn sign_funding_tx_with_output_and_multiple_change() {
        let is_p2sh = false;
        let node_ctx = test_node_ctx(1);

        let incoming = 5_000_000;
        let channel_amount = 3_000_000;
        let fee = 1000;
        let change0 = 1_000_000;
        let change1 = incoming - channel_amount - fee - change0;

        let mut chan_ctx = test_chan_ctx(&node_ctx, 1, channel_amount);
        let mut tx_ctx = test_funding_tx_ctx();

        funding_tx_add_wallet_input(&mut tx_ctx, is_p2sh, 1, incoming);
        funding_tx_add_wallet_output(&node_ctx, &mut tx_ctx, is_p2sh, 1, change0);
        funding_tx_add_wallet_output(&node_ctx, &mut tx_ctx, is_p2sh, 1, change1);
        let outpoint_ndx =
            funding_tx_add_channel_outpoint(&node_ctx, &chan_ctx, &mut tx_ctx, channel_amount);

        let mut tx = funding_tx_from_ctx(&tx_ctx);

        funding_tx_ready_channel(&node_ctx, &mut chan_ctx, &tx, outpoint_ndx);

        let mut commit_tx_ctx = channel_initial_holder_commitment(&node_ctx, &chan_ctx);
        let (csig, hsigs) =
            counterparty_sign_holder_commitment(&node_ctx, &chan_ctx, &mut commit_tx_ctx);
        validate_holder_commitment(&node_ctx, &chan_ctx, &commit_tx_ctx, &csig, &hsigs)
            .expect("valid holder commitment");

        let witvec = funding_tx_sign(&node_ctx, &tx_ctx, &tx).expect("witvec");
        funding_tx_validate_sig(&node_ctx, &tx_ctx, &mut tx, &witvec);
    }

    #[test]
    fn sign_funding_tx_with_multiple_outputs_and_change() {
        let is_p2sh = false;
        let node_ctx = test_node_ctx(1);

        let incoming = 10_000_000;
        let channel_amount0 = 3_000_000;
        let channel_amount1 = 4_000_000;
        let fee = 1000;
        let change = incoming - channel_amount0 - channel_amount1 - fee;

        let mut chan_ctx0 = test_chan_ctx(&node_ctx, 1, channel_amount0);
        let mut chan_ctx1 = test_chan_ctx(&node_ctx, 2, channel_amount1);
        let mut tx_ctx = test_funding_tx_ctx();

        funding_tx_add_wallet_input(&mut tx_ctx, is_p2sh, 1, incoming);
        funding_tx_add_wallet_output(&node_ctx, &mut tx_ctx, is_p2sh, 1, change);

        let outpoint_ndx0 =
            funding_tx_add_channel_outpoint(&node_ctx, &chan_ctx0, &mut tx_ctx, channel_amount0);

        let outpoint_ndx1 =
            funding_tx_add_channel_outpoint(&node_ctx, &chan_ctx1, &mut tx_ctx, channel_amount1);

        let mut tx = funding_tx_from_ctx(&tx_ctx);

        funding_tx_ready_channel(&node_ctx, &mut chan_ctx0, &tx, outpoint_ndx0);
        funding_tx_ready_channel(&node_ctx, &mut chan_ctx1, &tx, outpoint_ndx1);

        let mut commit_tx_ctx0 = channel_initial_holder_commitment(&node_ctx, &chan_ctx0);
        let (csig0, hsigs0) =
            counterparty_sign_holder_commitment(&node_ctx, &chan_ctx0, &mut commit_tx_ctx0);
        validate_holder_commitment(&node_ctx, &chan_ctx0, &commit_tx_ctx0, &csig0, &hsigs0)
            .expect("valid holder commitment");

        let mut commit_tx_ctx1 = channel_initial_holder_commitment(&node_ctx, &chan_ctx1);
        let (csig1, hsigs1) =
            counterparty_sign_holder_commitment(&node_ctx, &chan_ctx1, &mut commit_tx_ctx1);
        validate_holder_commitment(&node_ctx, &chan_ctx1, &commit_tx_ctx1, &csig1, &hsigs1)
            .expect("valid holder commitment");

        let witvec = funding_tx_sign(&node_ctx, &tx_ctx, &tx).expect("witvec");
        funding_tx_validate_sig(&node_ctx, &tx_ctx, &mut tx, &witvec);
    }

    // policy-v1-funding-initial-commitment-countersigned
    #[test]
    fn sign_funding_tx_with_missing_initial_commitment_validation() {
        let is_p2sh = false;
        let node_ctx = test_node_ctx(1);

        let incoming = 10_000_000;
        let channel_amount0 = 3_000_000;
        let channel_amount1 = 4_000_000;
        let fee = 1000;
        let change = incoming - channel_amount0 - channel_amount1 - fee;

        let mut chan_ctx0 = test_chan_ctx(&node_ctx, 1, channel_amount0);
        let mut chan_ctx1 = test_chan_ctx(&node_ctx, 2, channel_amount1);
        let mut tx_ctx = test_funding_tx_ctx();

        funding_tx_add_wallet_input(&mut tx_ctx, is_p2sh, 1, incoming);
        funding_tx_add_wallet_output(&node_ctx, &mut tx_ctx, is_p2sh, 1, change);

        let outpoint_ndx0 =
            funding_tx_add_channel_outpoint(&node_ctx, &chan_ctx0, &mut tx_ctx, channel_amount0);

        let outpoint_ndx1 =
            funding_tx_add_channel_outpoint(&node_ctx, &chan_ctx1, &mut tx_ctx, channel_amount1);

        let tx = funding_tx_from_ctx(&tx_ctx);

        funding_tx_ready_channel(&node_ctx, &mut chan_ctx0, &tx, outpoint_ndx0);
        funding_tx_ready_channel(&node_ctx, &mut chan_ctx1, &tx, outpoint_ndx1);

        let mut commit_tx_ctx0 = channel_initial_holder_commitment(&node_ctx, &chan_ctx0);
        let (csig0, hsigs0) =
            counterparty_sign_holder_commitment(&node_ctx, &chan_ctx0, &mut commit_tx_ctx0);
        validate_holder_commitment(&node_ctx, &chan_ctx0, &commit_tx_ctx0, &csig0, &hsigs0)
            .expect("valid holder commitment");

        // Don't validate the second channel's holder commitment.

        assert_failed_precondition_err!(
            funding_tx_sign(&node_ctx, &tx_ctx, &tx),
            "policy failure: validate_funding_tx: initial holder commitment not validated"
        );
    }

    // policy-v1-funding-output-match-commitment
    // policy-v2-funding-change-to-wallet
    #[test]
    fn sign_funding_tx_with_unknown_output() {
        let is_p2sh = false;
        let node_ctx = test_node_ctx(1);

        let incoming = 5_000_000;
        let channel_amount = 3_000_000;
        let unknown = 500_000;
        let fee = 1000;
        let change = incoming - channel_amount - unknown - fee;

        let mut chan_ctx = test_chan_ctx(&node_ctx, 1, channel_amount);
        let mut tx_ctx = test_funding_tx_ctx();

        funding_tx_add_wallet_input(&mut tx_ctx, is_p2sh, 1, incoming);
        funding_tx_add_wallet_output(&node_ctx, &mut tx_ctx, is_p2sh, 1, change);
        funding_tx_add_unknown_output(&node_ctx, &mut tx_ctx, is_p2sh, 42, unknown);
        let outpoint_ndx =
            funding_tx_add_channel_outpoint(&node_ctx, &chan_ctx, &mut tx_ctx, channel_amount);

        let tx = funding_tx_from_ctx(&tx_ctx);

        funding_tx_ready_channel(&node_ctx, &mut chan_ctx, &tx, outpoint_ndx);

        assert_failed_precondition_err!(
            funding_tx_sign(&node_ctx, &tx_ctx, &tx),
            "policy failure: unknown output: a5b4d12cf257a92e0536ddfce77635f92283f1e81e4d4f5ce7239bd36cfe925c:1"
        );
    }

    #[test]
    fn sign_funding_tx_with_bad_input_path() {
        let is_p2sh = false;
        let node_ctx = test_node_ctx(1);

        let incoming = 5_000_000;
        let channel_amount = 3_000_000;
        let fee = 1000;
        let change = incoming - channel_amount - fee;

        let mut chan_ctx = test_chan_ctx(&node_ctx, 1, channel_amount);
        let mut tx_ctx = test_funding_tx_ctx();

        funding_tx_add_wallet_input(&mut tx_ctx, is_p2sh, 1, incoming);
        funding_tx_add_wallet_output(&node_ctx, &mut tx_ctx, is_p2sh, 1, change);
        let outpoint_ndx =
            funding_tx_add_channel_outpoint(&node_ctx, &chan_ctx, &mut tx_ctx, channel_amount);

        let tx = funding_tx_from_ctx(&tx_ctx);

        funding_tx_ready_channel(&node_ctx, &mut chan_ctx, &tx, outpoint_ndx);

        let mut commit_tx_ctx = channel_initial_holder_commitment(&node_ctx, &chan_ctx);
        let (csig, hsigs) =
            counterparty_sign_holder_commitment(&node_ctx, &chan_ctx, &mut commit_tx_ctx);
        validate_holder_commitment(&node_ctx, &chan_ctx, &commit_tx_ctx, &csig, &hsigs)
            .expect("valid holder commitment");

        tx_ctx.ipaths[0] = vec![42, 42]; // bad input path

        assert_invalid_argument_err!(
            funding_tx_sign(&node_ctx, &tx_ctx, &tx),
            "get_wallet_key: bad child_path len : 2"
        );
    }

    #[test]
    fn sign_funding_tx_with_bad_output_path() {
        let is_p2sh = false;
        let node_ctx = test_node_ctx(1);

        let incoming = 5_000_000;
        let channel_amount = 3_000_000;
        let fee = 1000;
        let change = incoming - channel_amount - fee;

        let mut chan_ctx = test_chan_ctx(&node_ctx, 1, channel_amount);
        let mut tx_ctx = test_funding_tx_ctx();

        funding_tx_add_wallet_input(&mut tx_ctx, is_p2sh, 1, incoming);
        funding_tx_add_wallet_output(&node_ctx, &mut tx_ctx, is_p2sh, 1, change);
        let outpoint_ndx =
            funding_tx_add_channel_outpoint(&node_ctx, &chan_ctx, &mut tx_ctx, channel_amount);

        let tx = funding_tx_from_ctx(&tx_ctx);

        funding_tx_ready_channel(&node_ctx, &mut chan_ctx, &tx, outpoint_ndx);

        tx_ctx.opaths[0] = vec![42, 42]; // bad output path

        assert_failed_precondition_err!(
            funding_tx_sign(&node_ctx, &tx_ctx, &tx),
            "policy failure: output[0]: wallet_can_spend error: \
             status: InvalidArgument, message: \"get_wallet_key: bad child_path len : 2\""
        );
    }

    #[test]
    fn sign_funding_tx_with_bad_output_value() {
        let is_p2sh = false;
        let node_ctx = test_node_ctx(1);

        let incoming = 5_000_000;
        let channel_amount = 3_000_000;
        let fee = 1000;
        let change = incoming - channel_amount - fee;

        let mut chan_ctx = test_chan_ctx(&node_ctx, 1, channel_amount);
        let mut tx_ctx = test_funding_tx_ctx();

        funding_tx_add_wallet_input(&mut tx_ctx, is_p2sh, 1, incoming);
        funding_tx_add_wallet_output(&node_ctx, &mut tx_ctx, is_p2sh, 1, change);
        let outpoint_ndx =
            funding_tx_add_channel_outpoint(&node_ctx, &chan_ctx, &mut tx_ctx, channel_amount);

        let mut tx = funding_tx_from_ctx(&tx_ctx);

        funding_tx_ready_channel(&node_ctx, &mut chan_ctx, &tx, outpoint_ndx);

        // Modify the output value after funding_tx_ready_channel
        tx.output[1].value = channel_amount + 42; // bad output value

        assert_failed_precondition_err!(
            funding_tx_sign(&node_ctx, &tx_ctx, &tx),
            "policy failure: unknown output: 445f380db31cb6647304fefe17d69df19d0a7e8840394a295cb99a98dfce2b73:1"
        );
    }

    #[test]
    fn sign_funding_tx_with_bad_output_value2() {
        let is_p2sh = false;
        let node_ctx = test_node_ctx(1);

        let incoming = 5_000_000;
        let channel_amount = 3_000_000;
        let fee = 1000;
        let change = incoming - channel_amount - fee;

        let mut chan_ctx = test_chan_ctx(&node_ctx, 1, channel_amount);
        let mut tx_ctx = test_funding_tx_ctx();

        funding_tx_add_wallet_input(&mut tx_ctx, is_p2sh, 1, incoming);
        funding_tx_add_wallet_output(&node_ctx, &mut tx_ctx, is_p2sh, 1, change);
        let outpoint_ndx =
            funding_tx_add_channel_outpoint(&node_ctx, &chan_ctx, &mut tx_ctx, channel_amount);

        let mut tx = funding_tx_from_ctx(&tx_ctx);

        // Modify the output value before funding_tx_ready_channel
        tx.output[1].value = channel_amount + 42; // bad output value

        funding_tx_ready_channel(&node_ctx, &mut chan_ctx, &tx, outpoint_ndx);

        assert_failed_precondition_err!(
            funding_tx_sign(&node_ctx, &tx_ctx, &tx),
            "policy failure: validate_funding_tx: \
             funding output amount mismatch w/ channel: 3000042 != 3000000"
        );
    }

    #[test]
    fn sign_funding_tx_with_bad_output_script_pubkey() {
        let is_p2sh = false;
        let node_ctx = test_node_ctx(1);

        let incoming = 5_000_000;
        let channel_amount = 3_000_000;
        let fee = 1000;
        let change = incoming - channel_amount - fee;

        let mut tx_ctx = test_funding_tx_ctx();
        let mut chan_ctx = test_chan_ctx(&node_ctx, 1, channel_amount);

        funding_tx_add_wallet_input(&mut tx_ctx, is_p2sh, 1, incoming);
        funding_tx_add_wallet_output(&node_ctx, &mut tx_ctx, is_p2sh, 1, change);
        let outpoint_ndx =
            funding_tx_add_channel_outpoint(&node_ctx, &chan_ctx, &mut tx_ctx, channel_amount);

        let mut tx = funding_tx_from_ctx(&tx_ctx);

        funding_tx_ready_channel(&node_ctx, &mut chan_ctx, &tx, outpoint_ndx);

        // very bogus script
        tx.output[1].script_pubkey = Builder::new()
            .push_opcode(opcodes::all::OP_PUSHBYTES_0)
            .push_slice(&[27; 32])
            .into_script();

        assert_failed_precondition_err!(
            funding_tx_sign(&node_ctx, &tx_ctx, &tx),
            "policy failure: unknown output: 81fe91f5705b1a893494726cc9019614aa108fd02809e9f23673c83ea6404bce:1"
        );
    }

    // policy-v1-funding-output-scriptpubkey
    #[test]
    fn sign_funding_tx_with_bad_output_script_pubkey2() {
        let is_p2sh = false;
        let node_ctx = test_node_ctx(1);

        let incoming = 5_000_000;
        let channel_amount = 3_000_000;
        let fee = 1000;
        let change = incoming - channel_amount - fee;

        let mut chan_ctx = test_chan_ctx(&node_ctx, 1, channel_amount);
        let mut tx_ctx = test_funding_tx_ctx();

        funding_tx_add_wallet_input(&mut tx_ctx, is_p2sh, 1, incoming);
        funding_tx_add_wallet_output(&node_ctx, &mut tx_ctx, is_p2sh, 1, change);
        let outpoint_ndx =
            funding_tx_add_channel_outpoint(&node_ctx, &chan_ctx, &mut tx_ctx, channel_amount);

        let mut tx = funding_tx_from_ctx(&tx_ctx);

        // very bogus script
        tx.output[1].script_pubkey = Builder::new()
            .push_opcode(opcodes::all::OP_PUSHBYTES_0)
            .push_slice(&[27; 32])
            .into_script();

        funding_tx_ready_channel(&node_ctx, &mut chan_ctx, &tx, outpoint_ndx);

        assert_failed_precondition_err!(
            funding_tx_sign(&node_ctx, &tx_ctx, &tx),
            "policy failure: validate_funding_tx: funding script_pubkey mismatch w/ channel: Script(OP_0 OP_PUSHBYTES_32 1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b) != Script(OP_0 OP_PUSHBYTES_32 7ac8486233edd675a9745d9eefd4386880312b3930a2195567b4b89220b5c833)"
        );
    }

    #[test]
    fn validate_holder_commitment_with_htlcs() {
        let node_ctx = test_node_ctx(1);

        let channel_amount = 3_000_000;
        let chan_ctx = fund_test_channel(&node_ctx, channel_amount);

        let offered_htlcs = vec![
            HTLCInfo2 {
                value_sat: 1000,
                payment_hash: PaymentHash([1; 32]),
                cltv_expiry: 1 << 16,
            },
            HTLCInfo2 {
                value_sat: 1000,
                payment_hash: PaymentHash([2; 32]),
                cltv_expiry: 2 << 16,
            },
        ];
        let received_htlcs = vec![
            HTLCInfo2 {
                value_sat: 1000,
                payment_hash: PaymentHash([3; 32]),
                cltv_expiry: 3 << 16,
            },
            HTLCInfo2 {
                value_sat: 1000,
                payment_hash: PaymentHash([4; 32]),
                cltv_expiry: 4 << 16,
            },
            HTLCInfo2 {
                value_sat: 1000,
                payment_hash: PaymentHash([5; 32]),
                cltv_expiry: 5 << 16,
            },
        ];
        let sum_htlc = 5000;

        let commit_num = 1;
        let feerate_per_kw = 1100;
        let fees = 20_000;
        let to_broadcaster = 1_000_000;
        let to_countersignatory = channel_amount - to_broadcaster - sum_htlc - fees;

        let mut commit_tx_ctx = channel_commitment(
            &node_ctx,
            &chan_ctx,
            commit_num,
            feerate_per_kw,
            to_broadcaster,
            to_countersignatory,
            offered_htlcs,
            received_htlcs,
        );
        let (csig, hsigs) =
            counterparty_sign_holder_commitment(&node_ctx, &chan_ctx, &mut commit_tx_ctx);
        validate_holder_commitment(&node_ctx, &chan_ctx, &commit_tx_ctx, &csig, &hsigs)
            .expect("valid holder commitment");
    }

    // policy-v2-revoke-new-commitment-signed
    #[test]
    fn validate_holder_commitment_with_bad_commit_num() {
        let node_ctx = test_node_ctx(1);

        let channel_amount = 3_000_000;
        let chan_ctx = fund_test_channel(&node_ctx, channel_amount);
        let offered_htlcs = vec![];
        let received_htlcs = vec![];

        let commit_num = 2;
        let feerate_per_kw = 1100;
        let fees = 20_000;
        let to_broadcaster = 1_000_000;
        let to_countersignatory = channel_amount - to_broadcaster - fees;

        // Force the channel to commit_num 2 to build the bogus commitment ...
        set_next_holder_commit_num_for_testing(&node_ctx, &chan_ctx, commit_num);

        let mut commit_tx_ctx = channel_commitment(
            &node_ctx,
            &chan_ctx,
            commit_num,
            feerate_per_kw,
            to_broadcaster,
            to_countersignatory,
            offered_htlcs,
            received_htlcs,
        );
        let (csig, hsigs) =
            counterparty_sign_holder_commitment(&node_ctx, &chan_ctx, &mut commit_tx_ctx);

        set_next_holder_commit_num_for_testing(&node_ctx, &chan_ctx, 1);

        assert_failed_precondition_err!(
            validate_holder_commitment(&node_ctx, &chan_ctx, &commit_tx_ctx, &csig, &hsigs,),
            "policy failure: get_per_commitment_point: \
                commitment_number 2 invalid when next_holder_commit_num is 1"
        );
    }

    // policy-v2-commitment-local-not-revoked
    #[test]
    fn validate_holder_commitment_with_revoked_commit_num() {
        let node_ctx = test_node_ctx(1);

        let channel_amount = 3_000_000;
        let chan_ctx = fund_test_channel(&node_ctx, channel_amount);
        let offered_htlcs = vec![];
        let received_htlcs = vec![];

        let feerate_per_kw = 1100;
        let fees = 20_000;
        let to_broadcaster = 1_000_000;
        let to_countersignatory = channel_amount - to_broadcaster - fees;

        // Start by validating holder commitment #10 (which revokes #9)
        let commit_num = 10;
        set_next_holder_commit_num_for_testing(&node_ctx, &chan_ctx, commit_num);

        let mut commit_tx_ctx = channel_commitment(
            &node_ctx,
            &chan_ctx,
            commit_num,
            feerate_per_kw,
            to_broadcaster,
            to_countersignatory,
            offered_htlcs.clone(),
            received_htlcs.clone(),
        );
        let (csig, hsigs) =
            counterparty_sign_holder_commitment(&node_ctx, &chan_ctx, &mut commit_tx_ctx);

        validate_holder_commitment(&node_ctx, &chan_ctx, &commit_tx_ctx, &csig, &hsigs)
            .expect("valid holder commitment");

        let revoked_commit_num = commit_num - 1;

        // Now attempt to holder sign holder commitment #9
        let commit_tx_ctx = channel_commitment(
            &node_ctx,
            &chan_ctx,
            revoked_commit_num,
            feerate_per_kw,
            to_broadcaster,
            to_countersignatory,
            offered_htlcs,
            received_htlcs,
        );

        assert_failed_precondition_err!(
            sign_holder_commitment(&node_ctx, &chan_ctx, &commit_tx_ctx),
            "policy failure: validate_sign_holder_commitment_tx: \
             can't sign revoked commitment_number 9, next_holder_commit_num is 11"
        );
    }

    #[test]
    fn validate_holder_commitment_with_same_commit_num() {
        let node_ctx = test_node_ctx(1);

        let channel_amount = 3_000_000;
        let chan_ctx = fund_test_channel(&node_ctx, channel_amount);
        let offered_htlcs = vec![];
        let received_htlcs = vec![];

        let commit_num = 1;
        let feerate_per_kw = 1100;
        let fees = 20_000;
        let to_broadcaster = 1_000_000;
        let to_countersignatory = channel_amount - to_broadcaster - fees;

        let mut commit_tx_ctx = channel_commitment(
            &node_ctx,
            &chan_ctx,
            commit_num,
            feerate_per_kw,
            to_broadcaster,
            to_countersignatory,
            offered_htlcs,
            received_htlcs,
        );
        let (csig, hsigs) =
            counterparty_sign_holder_commitment(&node_ctx, &chan_ctx, &mut commit_tx_ctx);
        validate_holder_commitment(&node_ctx, &chan_ctx, &commit_tx_ctx, &csig, &hsigs)
            .expect("valid holder commitment");

        // You can do it again w/ same commit num.
        validate_holder_commitment(&node_ctx, &chan_ctx, &commit_tx_ctx, &csig, &hsigs)
            .expect("valid holder commitment");
    }

    #[test]
    fn sign_local_htlc_tx_static_test() {
        let setup = make_test_channel_setup();
        sign_local_htlc_tx_test(&setup);
    }

    #[test]
    fn sign_local_htlc_tx_legacy_test() {
        let mut setup = make_test_channel_setup();
        setup.commitment_type = CommitmentType::Legacy;
        sign_local_htlc_tx_test(&setup);
    }

    fn sign_local_htlc_tx_test(setup: &ChannelSetup) {
        let (node, channel_id) =
            init_node_and_channel(TEST_NODE_CONFIG, TEST_SEED[1], setup.clone());

        let htlc_amount_sat = 10 * 1000;

        let commitment_txid = bitcoin::Txid::from_slice(&[2u8; 32]).unwrap();
        let feerate_per_kw = 1000;
        let htlc = HTLCOutputInCommitment {
            offered: true,
            amount_msat: htlc_amount_sat * 1000,
            cltv_expiry: 2 << 16,
            payment_hash: PaymentHash([1; 32]),
            transaction_output_index: Some(0),
        };

        let n: u64 = 1;

        let (per_commitment_point, txkeys, to_self_delay) = node
            .with_ready_channel(&channel_id, |chan| {
                chan.enforcement_state
                    .set_next_holder_commit_num_for_testing(n);
                let per_commitment_point = chan.get_per_commitment_point(n).expect("point");
                let txkeys = chan
                    .make_holder_tx_keys(&per_commitment_point)
                    .expect("failed to make txkeys");
                let to_self_delay = chan
                    .make_channel_parameters()
                    .as_holder_broadcastable()
                    .contest_delay();
                Ok((per_commitment_point, txkeys, to_self_delay))
            })
            .expect("point");

        let htlc_tx = build_htlc_transaction(
            &commitment_txid,
            feerate_per_kw,
            to_self_delay,
            &htlc,
            &txkeys.broadcaster_delayed_payment_key,
            &txkeys.revocation_key,
        );

        let htlc_redeemscript = get_htlc_redeemscript(&htlc, &txkeys);

        let output_witscript = get_revokeable_redeemscript(
            &txkeys.revocation_key,
            to_self_delay,
            &txkeys.broadcaster_delayed_payment_key,
        );

        let htlc_pubkey = get_channel_htlc_pubkey(&node, &channel_id, &per_commitment_point);

        let sigvec = node
            .with_ready_channel(&channel_id, |chan| {
                let sig = chan
                    .sign_holder_htlc_tx(
                        &htlc_tx,
                        n,
                        None,
                        &htlc_redeemscript,
                        htlc_amount_sat,
                        &output_witscript,
                    )
                    .unwrap();
                Ok(signature_to_bitcoin_vec(sig))
            })
            .unwrap();

        check_signature(
            &htlc_tx,
            0,
            sigvec,
            &htlc_pubkey,
            htlc_amount_sat,
            &htlc_redeemscript,
        );

        let sigvec1 = node
            .with_ready_channel(&channel_id, |chan| {
                let sig = chan
                    .sign_holder_htlc_tx(
                        &htlc_tx,
                        999,
                        Some(per_commitment_point),
                        &htlc_redeemscript,
                        htlc_amount_sat,
                        &output_witscript,
                    )
                    .unwrap();
                Ok(signature_to_bitcoin_vec(sig))
            })
            .unwrap();

        check_signature(
            &htlc_tx,
            0,
            sigvec1,
            &htlc_pubkey,
            htlc_amount_sat,
            &htlc_redeemscript,
        );
    }

    #[test]
    fn sign_delayed_sweep_static_test() {
        let setup = make_test_channel_setup();
        sign_delayed_sweep_test(&setup);
    }

    #[test]
    fn sign_delayed_sweep_legacy_test() {
        let mut setup = make_test_channel_setup();
        setup.commitment_type = CommitmentType::Legacy;
        sign_delayed_sweep_test(&setup);
    }

    fn sign_delayed_sweep_test(setup: &ChannelSetup) {
        let (node, channel_id) =
            init_node_and_channel(TEST_NODE_CONFIG, TEST_SEED[1], setup.clone());

        let commitment_txid = bitcoin::Txid::from_slice(&[2u8; 32]).unwrap();
        let feerate_per_kw = 1000;
        let to_self_delay = 32;
        let htlc = HTLCOutputInCommitment {
            offered: true,
            amount_msat: 1 * 1000 * 1000,
            cltv_expiry: 2 << 16,
            payment_hash: PaymentHash([1; 32]),
            transaction_output_index: Some(0),
        };

        let secp_ctx_all = Secp256k1::new();

        let n: u64 = 1;

        let per_commitment_point = node
            .with_ready_channel(&channel_id, |chan| {
                chan.enforcement_state
                    .set_next_holder_commit_num_for_testing(n);
                chan.get_per_commitment_point(n)
            })
            .expect("point");

        let a_delayed_payment_base = make_test_pubkey(2);
        let b_revocation_base = make_test_pubkey(3);

        let a_delayed_payment_key = derive_public_key(
            &secp_ctx_all,
            &per_commitment_point,
            &a_delayed_payment_base,
        )
        .expect("a_delayed_payment_key");

        let revocation_pubkey =
            derive_revocation_pubkey(&secp_ctx_all, &per_commitment_point, &b_revocation_base)
                .expect("revocation_pubkey");

        let htlc_tx = build_htlc_transaction(
            &commitment_txid,
            feerate_per_kw,
            to_self_delay,
            &htlc,
            &a_delayed_payment_key,
            &revocation_pubkey,
        );

        let redeemscript =
            get_revokeable_redeemscript(&revocation_pubkey, to_self_delay, &a_delayed_payment_key);

        let htlc_amount_sat = 10 * 1000;

        let sigvec = node
            .with_ready_channel(&channel_id, |chan| {
                let sig = chan
                    .sign_delayed_sweep(&htlc_tx, 0, n, &redeemscript, htlc_amount_sat)
                    .unwrap();
                Ok(signature_to_bitcoin_vec(sig))
            })
            .unwrap();

        let htlc_pubkey =
            get_channel_delayed_payment_pubkey(&node, &channel_id, &per_commitment_point);

        check_signature(
            &htlc_tx,
            0,
            sigvec,
            &htlc_pubkey,
            htlc_amount_sat,
            &redeemscript,
        );
    }

    fn sign_counterparty_htlc_tx_with_mutators<ChanParamMutator, KeysMutator, TxMutator>(
        is_offered: bool,
        chanparammut: ChanParamMutator,
        keysmut: KeysMutator,
        txmut: TxMutator,
    ) -> Result<(), Status>
    where
        ChanParamMutator: Fn(&mut ChannelTransactionParameters),
        KeysMutator: Fn(&mut TxCreationKeys),
        TxMutator: Fn(&mut Transaction),
    {
        let (node, channel_id) =
            init_node_and_channel(TEST_NODE_CONFIG, TEST_SEED[1], make_test_channel_setup());

        let remote_per_commitment_point = make_test_pubkey(10);
        let htlc_amount_sat = 1_000_000;

        let (sig, htlc_tx, htlc_redeemscript) = node.with_ready_channel(&channel_id, |chan| {
            let mut channel_parameters = chan.make_channel_parameters();

            // Mutate the channel parameters
            chanparammut(&mut channel_parameters);

            let mut keys = chan.make_counterparty_tx_keys(&remote_per_commitment_point)?;

            // Mutate the tx creation keys.
            keysmut(&mut keys);

            let commitment_txid = bitcoin::Txid::from_slice(&[2u8; 32]).unwrap();
            let feerate_per_kw = 1000;
            let to_self_delay = channel_parameters
                .as_counterparty_broadcastable()
                .contest_delay();

            let htlc = HTLCOutputInCommitment {
                offered: is_offered,
                amount_msat: htlc_amount_sat * 1000,
                cltv_expiry: if is_offered { 2 << 16 } else { 0 },
                payment_hash: PaymentHash([1; 32]),
                transaction_output_index: Some(0),
            };

            let mut htlc_tx = build_htlc_transaction(
                &commitment_txid,
                feerate_per_kw,
                to_self_delay,
                &htlc,
                &keys.broadcaster_delayed_payment_key,
                &keys.revocation_key,
            );

            // Mutate the transaction.
            txmut(&mut htlc_tx);

            let htlc_redeemscript = get_htlc_redeemscript(&htlc, &keys);

            let output_witscript = get_revokeable_redeemscript(
                &keys.revocation_key,
                to_self_delay,
                &keys.broadcaster_delayed_payment_key,
            );

            let sig = chan.sign_counterparty_htlc_tx(
                &htlc_tx,
                &remote_per_commitment_point,
                &htlc_redeemscript,
                htlc_amount_sat,
                &output_witscript,
            )?;
            Ok((sig, htlc_tx, htlc_redeemscript))
        })?;

        if is_offered {
            assert_eq!(
                htlc_tx.txid().to_hex(),
                "66a108d7722fdb160206ba075a49c03c9e0174421c0c845cddd4a5b931fa5ab5"
            );
        } else {
            assert_eq!(
                htlc_tx.txid().to_hex(),
                "a052c48d7cba8eb1107d72b15741292267d4f4af754a7136168de50d4359b714"
            );
        }

        let htlc_pubkey = get_channel_htlc_pubkey(&node, &channel_id, &remote_per_commitment_point);

        check_signature(
            &htlc_tx,
            0,
            signature_to_bitcoin_vec(sig),
            &htlc_pubkey,
            htlc_amount_sat,
            &htlc_redeemscript,
        );

        Ok(())
    }

    fn sign_holder_htlc_tx_with_mutators<ChanParamMutator, KeysMutator, TxMutator>(
        is_offered: bool,
        chanparammut: ChanParamMutator,
        keysmut: KeysMutator,
        txmut: TxMutator,
    ) -> Result<(), Status>
    where
        ChanParamMutator: Fn(&mut ChannelTransactionParameters),
        KeysMutator: Fn(&mut TxCreationKeys),
        TxMutator: Fn(&mut Transaction),
    {
        let (node, channel_id) =
            init_node_and_channel(TEST_NODE_CONFIG, TEST_SEED[1], make_test_channel_setup());

        let commit_num = 23;
        let htlc_amount_sat = 1_000_000;

        let (sig, per_commitment_point, htlc_tx, htlc_redeemscript) =
            node.with_ready_channel(&channel_id, |chan| {
                chan.enforcement_state
                    .set_next_holder_commit_num_for_testing(commit_num);
                let mut channel_parameters = chan.make_channel_parameters();

                // Mutate the channel parameters
                chanparammut(&mut channel_parameters);

                let per_commitment_point =
                    chan.get_per_commitment_point(commit_num).expect("point");
                let mut keys = chan.make_holder_tx_keys(&per_commitment_point)?;

                // Mutate the tx creation keys.
                keysmut(&mut keys);

                let commitment_txid = bitcoin::Txid::from_slice(&[2u8; 32]).unwrap();
                let feerate_per_kw = 1000;
                let to_self_delay = channel_parameters.as_holder_broadcastable().contest_delay();

                let htlc = HTLCOutputInCommitment {
                    offered: is_offered,
                    amount_msat: htlc_amount_sat * 1000,
                    cltv_expiry: if is_offered { 2 << 16 } else { 0 },
                    payment_hash: PaymentHash([1; 32]),
                    transaction_output_index: Some(0),
                };

                let mut htlc_tx = build_htlc_transaction(
                    &commitment_txid,
                    feerate_per_kw,
                    to_self_delay,
                    &htlc,
                    &keys.broadcaster_delayed_payment_key,
                    &keys.revocation_key,
                );

                // Mutate the transaction.
                txmut(&mut htlc_tx);

                let htlc_redeemscript = get_htlc_redeemscript(&htlc, &keys);

                let output_witscript = get_revokeable_redeemscript(
                    &keys.revocation_key,
                    to_self_delay,
                    &keys.broadcaster_delayed_payment_key,
                );

                let sig = chan.sign_holder_htlc_tx(
                    &htlc_tx,
                    commit_num,
                    Some(per_commitment_point),
                    &htlc_redeemscript,
                    htlc_amount_sat,
                    &output_witscript,
                )?;
                Ok((sig, per_commitment_point, htlc_tx, htlc_redeemscript))
            })?;

        if is_offered {
            assert_eq!(
                htlc_tx.txid().to_hex(),
                "783ca2bb360dc712301d43daef0dbae2e15a8f06dcc73062b24e1d86cb918e5c"
            );
        } else {
            assert_eq!(
                htlc_tx.txid().to_hex(),
                "89cf05ddaef231827291e32cc67d17810b867614bbb8e1a39c001f62f57421ab"
            );
        }

        let htlc_pubkey = get_channel_htlc_pubkey(&node, &channel_id, &per_commitment_point);

        check_signature(
            &htlc_tx,
            0,
            signature_to_bitcoin_vec(sig),
            &htlc_pubkey,
            htlc_amount_sat,
            &htlc_redeemscript,
        );

        Ok(())
    }

    macro_rules! sign_counterparty_offered_htlc_tx_with_mutators {
        ($pm: expr, $km: expr, $tm: expr) => {
            sign_counterparty_htlc_tx_with_mutators(true, $pm, $km, $tm)
        };
    }

    macro_rules! sign_counterparty_received_htlc_tx_with_mutators {
        ($pm: expr, $km: expr, $tm: expr) => {
            sign_counterparty_htlc_tx_with_mutators(false, $pm, $km, $tm)
        };
    }

    macro_rules! sign_holder_offered_htlc_tx_with_mutators {
        ($pm: expr, $km: expr, $tm: expr) => {
            sign_holder_htlc_tx_with_mutators(true, $pm, $km, $tm)
        };
    }

    macro_rules! sign_holder_received_htlc_tx_with_mutators {
        ($pm: expr, $km: expr, $tm: expr) => {
            sign_holder_htlc_tx_with_mutators(false, $pm, $km, $tm)
        };
    }

    #[test]
    fn sign_counterparty_offered_htlc_tx_with_no_mut_test() {
        let status = sign_counterparty_offered_htlc_tx_with_mutators!(
            |_param| {
                // don't mutate the channel parameters, should pass
            },
            |_keys| {
                // don't mutate the keys, should pass
            },
            |_tx| {
                // don't mutate the tx, should pass
            }
        );
        assert!(status.is_ok());
    }

    #[test]
    fn sign_counterparty_received_htlc_tx_with_no_mut_test() {
        let status = sign_counterparty_received_htlc_tx_with_mutators!(
            |_param| {
                // don't mutate the channel parameters, should pass
            },
            |_keys| {
                // don't mutate the keys, should pass
            },
            |_tx| {
                // don't mutate the tx, should pass
            }
        );
        assert!(status.is_ok());
    }

    #[test]
    fn sign_holder_offered_htlc_tx_with_no_mut_test() {
        let status = sign_holder_offered_htlc_tx_with_mutators!(
            |_param| {
                // don't mutate the channel parameters, should pass
            },
            |_keys| {
                // don't mutate the keys, should pass
            },
            |_tx| {
                // don't mutate the tx, should pass
            }
        );
        assert!(status.is_ok());
    }

    #[test]
    fn sign_holder_received_htlc_tx_with_no_mut_test() {
        let status = sign_holder_received_htlc_tx_with_mutators!(
            |_param| {
                // don't mutate the channel parameters, should pass
            },
            |_keys| {
                // don't mutate the keys, should pass
            },
            |_tx| {
                // don't mutate the tx, should pass
            }
        );
        assert!(status.is_ok());
    }

    // policy-v1-htlc-version
    #[test]
    fn sign_counterparty_offered_htlc_tx_with_bad_version_test() {
        assert_failed_precondition_err!(
            sign_counterparty_offered_htlc_tx_with_mutators!(
                |_param| {},
                |_keys| {},
                |tx| tx.version = 3 // only version 2 allowed
            ),
            "policy failure: sighash mismatch"
        );
    }

    // policy-v1-htlc-version
    #[test]
    fn sign_counterparty_received_htlc_tx_with_bad_version_test() {
        assert_failed_precondition_err!(
            sign_counterparty_received_htlc_tx_with_mutators!(
                |_param| {},
                |_keys| {},
                |tx| tx.version = 3 // only version 2 allowed
            ),
            "policy failure: sighash mismatch"
        );
    }

    // policy-v1-htlc-version
    #[test]
    fn sign_holder_offered_htlc_tx_with_bad_version_test() {
        assert_failed_precondition_err!(
            sign_holder_offered_htlc_tx_with_mutators!(
                |_param| {},
                |_keys| {},
                |tx| tx.version = 3 // only version 2 allowed
            ),
            "policy failure: sighash mismatch"
        );
    }

    // policy-v1-htlc-version
    #[test]
    fn sign_holder_received_htlc_tx_with_bad_version_test() {
        assert_failed_precondition_err!(
            sign_holder_received_htlc_tx_with_mutators!(
                |_param| {},
                |_keys| {},
                |tx| tx.version = 3 // only version 2 allowed
            ),
            "policy failure: sighash mismatch"
        );
    }

    // policy-v1-htlc-locktime
    #[test]
    fn sign_counterparty_offered_htlc_tx_with_bad_locktime_test() {
        assert_failed_precondition_err!(
            sign_counterparty_offered_htlc_tx_with_mutators!(
                |_param| {},
                |_keys| {},
                |tx| tx.lock_time = 0 // offered must have non-zero locktime
            ),
            "policy failure: validate_htlc_tx: offered lock_time must be non-zero"
        );
    }

    // policy-v1-htlc-locktime
    #[test]
    fn sign_counterparty_received_htlc_tx_with_bad_locktime_test() {
        assert_failed_precondition_err!(
            sign_counterparty_received_htlc_tx_with_mutators!(
                |_param| {},
                |_keys| {},
                |tx| tx.lock_time = 42 // received must have zero locktime
            ),
            "policy failure: sighash mismatch"
        );
    }

    // policy-v1-htlc-locktime
    #[test]
    fn sign_holder_offered_htlc_tx_with_bad_locktime_test() {
        assert_failed_precondition_err!(
            sign_holder_offered_htlc_tx_with_mutators!(
                |_param| {},
                |_keys| {},
                |tx| tx.lock_time = 0 // offered must have non-zero locktime
            ),
            "policy failure: validate_htlc_tx: offered lock_time must be non-zero"
        );
    }

    // policy-v1-htlc-locktime
    #[test]
    fn sign_holder_received_htlc_tx_with_bad_locktime_test() {
        assert_failed_precondition_err!(
            sign_holder_received_htlc_tx_with_mutators!(
                |_param| {},
                |_keys| {},
                |tx| tx.lock_time = 42 // received must have zero locktime
            ),
            "policy failure: sighash mismatch"
        );
    }

    // policy-v1-htlc-sequence
    #[test]
    fn sign_counterparty_offered_htlc_tx_with_bad_sequence_test() {
        assert_failed_precondition_err!(
            sign_counterparty_offered_htlc_tx_with_mutators!(
                |_param| {},
                |_keys| {},
                |tx| tx.input[0].sequence = 42 // sequence must be per BOLT#3
            ),
            "policy failure: sighash mismatch"
        );
    }

    // policy-v1-htlc-sequence
    #[test]
    fn sign_counterparty_received_htlc_tx_with_bad_sequence_test() {
        assert_failed_precondition_err!(
            sign_counterparty_received_htlc_tx_with_mutators!(
                |_param| {},
                |_keys| {},
                |tx| tx.input[0].sequence = 42 // sequence must be per BOLT#3
            ),
            "policy failure: sighash mismatch"
        );
    }

    // policy-v1-htlc-sequence
    #[test]
    fn sign_holder_offered_htlc_tx_with_bad_sequence_test() {
        assert_failed_precondition_err!(
            sign_holder_offered_htlc_tx_with_mutators!(
                |_param| {},
                |_keys| {},
                |tx| tx.input[0].sequence = 42 // sequence must be per BOLT#3
            ),
            "policy failure: sighash mismatch"
        );
    }

    // policy-v1-htlc-sequence
    #[test]
    fn sign_holder_received_htlc_tx_with_bad_sequence_test() {
        assert_failed_precondition_err!(
            sign_holder_received_htlc_tx_with_mutators!(
                |_param| {},
                |_keys| {},
                |tx| tx.input[0].sequence = 42 // sequence must be per BOLT#3
            ),
            "policy failure: sighash mismatch"
        );
    }

    // policy-v1-htlc-to-self-delay
    #[test]
    fn sign_counterparty_offered_htlc_tx_with_bad_to_self_delay_test() {
        assert_failed_precondition_err!(
            sign_counterparty_offered_htlc_tx_with_mutators!(
                |param| param.holder_selected_contest_delay = 42,
                |_keys| {},
                |_tx| {}
            ),
            "policy failure: sighash mismatch"
        );
    }

    // policy-v1-htlc-to-self-delay
    #[test]
    fn sign_counterparty_received_htlc_tx_with_bad_to_self_delay_test() {
        assert_failed_precondition_err!(
            sign_counterparty_received_htlc_tx_with_mutators!(
                |param| param.holder_selected_contest_delay = 42,
                |_keys| {},
                |_tx| {}
            ),
            "policy failure: sighash mismatch"
        );
    }

    // policy-v1-htlc-to-self-delay
    #[test]
    fn sign_holder_offered_htlc_tx_with_bad_to_self_delay_test() {
        assert_failed_precondition_err!(
            sign_holder_offered_htlc_tx_with_mutators!(
                |param| {
                    let mut cptp = param.counterparty_parameters.as_ref().unwrap().clone();
                    cptp.selected_contest_delay = 42;
                    param.counterparty_parameters = Some(cptp);
                },
                |_keys| {},
                |_tx| {}
            ),
            "policy failure: sighash mismatch"
        );
    }

    // policy-v1-htlc-to-self-delay
    #[test]
    fn sign_holder_received_htlc_tx_with_bad_to_self_delay_test() {
        assert_failed_precondition_err!(
            sign_holder_received_htlc_tx_with_mutators!(
                |param| {
                    let mut cptp = param.counterparty_parameters.as_ref().unwrap().clone();
                    cptp.selected_contest_delay = 42;
                    param.counterparty_parameters = Some(cptp);
                },
                |_keys| {},
                |_tx| {}
            ),
            "policy failure: sighash mismatch"
        );
    }

    // policy-v1-htlc-revocation-pubkey
    #[test]
    fn sign_counterparty_offered_htlc_tx_with_bad_revpubkey_test() {
        assert_failed_precondition_err!(
            sign_counterparty_offered_htlc_tx_with_mutators!(
                |_param| {},
                |keys| keys.revocation_key = make_test_pubkey(42),
                |_tx| {}
            ),
            "policy failure: sighash mismatch"
        );
    }

    // policy-v1-htlc-revocation-pubkey
    #[test]
    fn sign_counterparty_received_htlc_tx_with_bad_revpubkey_test() {
        assert_failed_precondition_err!(
            sign_counterparty_received_htlc_tx_with_mutators!(
                |_param| {},
                |keys| keys.revocation_key = make_test_pubkey(42),
                |_tx| {}
            ),
            "policy failure: sighash mismatch"
        );
    }

    // policy-v1-htlc-revocation-pubkey
    #[test]
    fn sign_holder_offered_htlc_tx_with_bad_revpubkey_test() {
        assert_failed_precondition_err!(
            sign_holder_offered_htlc_tx_with_mutators!(
                |_param| {},
                |keys| keys.revocation_key = make_test_pubkey(42),
                |_tx| {}
            ),
            "policy failure: sighash mismatch"
        );
    }

    // policy-v1-htlc-revocation-pubkey
    #[test]
    fn sign_holder_received_htlc_tx_with_bad_revpubkey_test() {
        assert_failed_precondition_err!(
            sign_holder_received_htlc_tx_with_mutators!(
                |_param| {},
                |keys| keys.revocation_key = make_test_pubkey(42),
                |_tx| {}
            ),
            "policy failure: sighash mismatch"
        );
    }

    // policy-v1-htlc-delayed-pubkey
    #[test]
    fn sign_counterparty_offered_htlc_tx_with_bad_delayedpubkey_test() {
        assert_failed_precondition_err!(
            sign_counterparty_offered_htlc_tx_with_mutators!(
                |_param| {},
                |keys| keys.broadcaster_delayed_payment_key = make_test_pubkey(42),
                |_tx| {}
            ),
            "policy failure: sighash mismatch"
        );
    }

    // policy-v1-htlc-delayed-pubkey
    #[test]
    fn sign_counterparty_received_htlc_tx_with_bad_delayedpubkey_test() {
        assert_failed_precondition_err!(
            sign_counterparty_received_htlc_tx_with_mutators!(
                |_param| {},
                |keys| keys.broadcaster_delayed_payment_key = make_test_pubkey(42),
                |_tx| {}
            ),
            "policy failure: sighash mismatch"
        );
    }

    // policy-v1-htlc-delayed-pubkey
    #[test]
    fn sign_holder_offered_htlc_tx_with_bad_delayedpubkey_test() {
        assert_failed_precondition_err!(
            sign_holder_offered_htlc_tx_with_mutators!(
                |_param| {},
                |keys| keys.broadcaster_delayed_payment_key = make_test_pubkey(42),
                |_tx| {}
            ),
            "policy failure: sighash mismatch"
        );
    }

    // policy-v1-htlc-delayed-pubkey
    #[test]
    fn sign_holder_received_htlc_tx_with_bad_delayedpubkey_test() {
        assert_failed_precondition_err!(
            sign_holder_received_htlc_tx_with_mutators!(
                |_param| {},
                |keys| keys.broadcaster_delayed_payment_key = make_test_pubkey(42),
                |_tx| {}
            ),
            "policy failure: sighash mismatch"
        );
    }

    // policy-v1-htlc-fee-range
    #[test]
    fn sign_counterparty_offered_htlc_tx_with_low_feerate_test() {
        assert_failed_precondition_err!(
            sign_counterparty_offered_htlc_tx_with_mutators!(
                |_param| {},
                |_keys| {},
                |tx| tx.output[0].value = 999_900 // htlc_amount_sat is 1_000_000
            ),
            "policy failure: validate_htlc_tx: \
             feerate_per_kw of 151 is smaller than the minimum of 500"
        );
    }

    // policy-v1-htlc-fee-range
    #[test]
    fn sign_counterparty_offered_htlc_tx_with_high_feerate_test() {
        assert_failed_precondition_err!(
            sign_counterparty_offered_htlc_tx_with_mutators!(
                |_param| {},
                |_keys| {},
                |tx| tx.output[0].value = 980_000 // htlc_amount_sat is 1_000_000
            ),
            "policy failure: validate_htlc_tx: \
             feerate_per_kw of 30166 is larger than the maximum of 16000"
        );
    }

    // policy-v1-htlc-fee-range
    #[test]
    fn sign_holder_received_htlc_tx_with_low_feerate_test() {
        assert_failed_precondition_err!(
            sign_holder_received_htlc_tx_with_mutators!(
                |_param| {},
                |_keys| {},
                |tx| tx.output[0].value = 999_900 // htlc_amount_sat is 1_000_000
            ),
            "policy failure: validate_htlc_tx: \
             feerate_per_kw of 143 is smaller than the minimum of 500"
        );
    }

    // policy-v1-htlc-fee-range
    #[test]
    fn sign_holder_received_htlc_tx_with_high_feerate_test() {
        assert_failed_precondition_err!(
            sign_holder_received_htlc_tx_with_mutators!(
                |_param| {},
                |_keys| {},
                |tx| tx.output[0].value = 980_000 // htlc_amount_sat is 1_000_000
            ),
            "policy failure: validate_htlc_tx: \
             feerate_per_kw of 28450 is larger than the maximum of 16000"
        );
    }

    #[test]
    #[ignore] // we don't support anchors yet
    fn sign_remote_htlc_tx_with_anchors_test() {
        let mut setup = make_test_channel_setup();
        setup.commitment_type = CommitmentType::Anchors;
        let (node, channel_id) =
            init_node_and_channel(TEST_NODE_CONFIG, TEST_SEED[1], setup.clone());

        let htlc_amount_sat = 10 * 1000;

        let commitment_txid = bitcoin::Txid::from_slice(&[2u8; 32]).unwrap();
        let feerate_per_kw = 1000;
        let to_self_delay = 32;
        let htlc = HTLCOutputInCommitment {
            offered: true,
            amount_msat: htlc_amount_sat * 1000,
            cltv_expiry: 2 << 16,
            payment_hash: PaymentHash([1; 32]),
            transaction_output_index: Some(0),
        };

        let remote_per_commitment_point = make_test_pubkey(10);

        let per_commitment_point = make_test_pubkey(1);
        let a_delayed_payment_base = make_test_pubkey(2);
        let b_revocation_base = make_test_pubkey(3);

        let secp_ctx = Secp256k1::new();

        let keys = TxCreationKeys::derive_new(
            &secp_ctx,
            &per_commitment_point,
            &a_delayed_payment_base,
            &make_test_pubkey(4), // a_htlc_base
            &b_revocation_base,
            &make_test_pubkey(6),
        ) // b_htlc_base
        .expect("new TxCreationKeys");

        let a_delayed_payment_key =
            derive_public_key(&secp_ctx, &per_commitment_point, &a_delayed_payment_base)
                .expect("a_delayed_payment_key");

        let revocation_key =
            derive_revocation_pubkey(&secp_ctx, &per_commitment_point, &b_revocation_base)
                .expect("revocation_key");

        let htlc_tx = build_htlc_transaction(
            &commitment_txid,
            feerate_per_kw,
            to_self_delay,
            &htlc,
            &a_delayed_payment_key,
            &revocation_key,
        );

        let htlc_redeemscript = get_htlc_redeemscript(&htlc, &keys);

        let output_witscript =
            get_revokeable_redeemscript(&revocation_key, to_self_delay, &a_delayed_payment_key);

        let ser_signature = node
            .with_ready_channel(&channel_id, |chan| {
                let sig = chan
                    .sign_counterparty_htlc_tx(
                        &htlc_tx,
                        &remote_per_commitment_point,
                        &htlc_redeemscript,
                        htlc_amount_sat,
                        &output_witscript,
                    )
                    .unwrap();
                Ok(signature_to_bitcoin_vec(sig))
            })
            .unwrap();

        let htlc_pubkey = get_channel_htlc_pubkey(&node, &channel_id, &remote_per_commitment_point);

        check_signature_with_setup(
            &htlc_tx,
            0,
            ser_signature,
            &htlc_pubkey,
            htlc_amount_sat,
            &htlc_redeemscript,
            &setup,
        );
    }

    #[test]
    fn sign_counterparty_htlc_sweep_test() {
        let (node, channel_id) =
            init_node_and_channel(TEST_NODE_CONFIG, TEST_SEED[1], make_test_channel_setup());

        let commitment_txid = bitcoin::Txid::from_slice(&[2u8; 32]).unwrap();
        let feerate_per_kw = 1000;
        let to_self_delay = 32;
        let htlc = HTLCOutputInCommitment {
            offered: true,
            amount_msat: 1 * 1000 * 1000,
            cltv_expiry: 2 << 16,
            payment_hash: PaymentHash([1; 32]),
            transaction_output_index: Some(0),
        };

        let remote_per_commitment_point = make_test_pubkey(10);

        let per_commitment_point = make_test_pubkey(1);
        let a_delayed_payment_base = make_test_pubkey(2);
        let b_revocation_base = make_test_pubkey(3);

        let secp_ctx = Secp256k1::new();

        let keys = TxCreationKeys::derive_new(
            &secp_ctx,
            &per_commitment_point,
            &a_delayed_payment_base,
            &make_test_pubkey(4), // a_htlc_base
            &b_revocation_base,
            &make_test_pubkey(6),
        ) // b_htlc_base
        .expect("new TxCreationKeys");

        let a_delayed_payment_key =
            derive_public_key(&secp_ctx, &per_commitment_point, &a_delayed_payment_base)
                .expect("a_delayed_payment_key");

        let revocation_key =
            derive_revocation_pubkey(&secp_ctx, &per_commitment_point, &b_revocation_base)
                .expect("revocation_key");

        let htlc_tx = build_htlc_transaction(
            &commitment_txid,
            feerate_per_kw,
            to_self_delay,
            &htlc,
            &a_delayed_payment_key,
            &revocation_key,
        );

        let htlc_redeemscript = get_htlc_redeemscript(&htlc, &keys);

        let htlc_amount_sat = 10 * 1000;

        let ser_signature = node
            .with_ready_channel(&channel_id, |chan| {
                let sig = chan
                    .sign_counterparty_htlc_sweep(
                        &htlc_tx,
                        0,
                        &remote_per_commitment_point,
                        &htlc_redeemscript,
                        htlc_amount_sat,
                    )
                    .unwrap();
                Ok(signature_to_bitcoin_vec(sig))
            })
            .unwrap();

        let htlc_pubkey = get_channel_htlc_pubkey(&node, &channel_id, &remote_per_commitment_point);

        check_signature(
            &htlc_tx,
            0,
            ser_signature,
            &htlc_pubkey,
            htlc_amount_sat,
            &htlc_redeemscript,
        );
    }

    #[test]
    fn sign_holder_commitment_tx_test() {
        let setup = make_test_channel_setup();
        let (node, channel_id) =
            init_node_and_channel(TEST_NODE_CONFIG, TEST_SEED[1], setup.clone());

        let (sig, tx) = node
            .with_ready_channel(&channel_id, |chan| {
                let channel_parameters = chan.make_channel_parameters();
                let commit_num = 23;
                let feerate_per_kw = 0;
                let to_broadcaster = 2_000_000;
                let to_countersignatory = 1_000_000;
                let offered_htlcs = vec![];
                let received_htlcs = vec![];
                let mut htlcs =
                    Channel::htlcs_info2_to_oic(offered_htlcs.clone(), received_htlcs.clone());

                chan.enforcement_state
                    .set_next_holder_commit_num_for_testing(commit_num);

                let parameters = channel_parameters.as_holder_broadcastable();

                let per_commitment_point =
                    chan.get_per_commitment_point(commit_num).expect("point");
                let keys = chan.make_holder_tx_keys(&per_commitment_point).unwrap();

                let redeem_scripts = build_tx_scripts(
                    &keys,
                    to_broadcaster,
                    to_countersignatory,
                    &mut htlcs,
                    &parameters,
                )
                .expect("scripts");
                let output_witscripts = redeem_scripts.iter().map(|s| s.serialize()).collect();

                let commitment_tx = chan
                    .make_holder_commitment_tx(
                        commit_num,
                        feerate_per_kw,
                        to_broadcaster,
                        to_countersignatory,
                        htlcs.clone(),
                    )
                    .expect("holder_commitment_tx");

                // rebuild to get the scripts
                let trusted_tx = commitment_tx.trust();
                let tx = trusted_tx.built_transaction();

                let sig = chan
                    .sign_holder_commitment_tx(
                        &tx.transaction,
                        &output_witscripts,
                        commit_num,
                        feerate_per_kw,
                        offered_htlcs,
                        received_htlcs,
                    )
                    .expect("sign");
                Ok((sig, tx.transaction.clone()))
            })
            .expect("build_commitment_tx");

        assert_eq!(
            tx.txid().to_hex(),
            "566333b63b2696cd51516dee93baa01243a0c0f17d646da1d1450a4f98de6a5e"
        );

        let funding_pubkey = get_channel_funding_pubkey(&node, &channel_id);
        let channel_funding_redeemscript =
            make_funding_redeemscript(&funding_pubkey, &setup.counterparty_points.funding_pubkey);

        check_signature(
            &tx,
            0,
            signature_to_bitcoin_vec(sig),
            &funding_pubkey,
            setup.channel_value_sat,
            &channel_funding_redeemscript,
        );
    }

    fn setup_mutual_close_tx() -> Result<
        (
            Secp256k1<secp256k1::SignOnly>,
            ChannelSetup,
            Arc<Node>,
            ChannelId,
            u64,
            u64,
            u64,
            Vec<u32>,
            ChannelPublicKeys,
        ),
        Status,
    > {
        let secp_ctx = Secp256k1::signing_only();
        let setup = make_test_channel_setup();
        let (node, channel_id) =
            init_node_and_channel(TEST_NODE_CONFIG, TEST_SEED[1], setup.clone());

        let counterparty_points = make_test_counterparty_points();
        let holder_commit_num = 22;
        let counterparty_commit_num = 43;
        let holder_wallet_path_hint = vec![7];

        let fee = 2000;
        let to_counterparty_value_sat = 1_000_000;
        let to_holder_value_sat = setup.channel_value_sat - to_counterparty_value_sat - fee;

        node.with_ready_channel(&channel_id, |chan| {
            // Construct the EnforcementState prior to the mutual_close.
            let mut estate = &mut chan.enforcement_state;
            estate.next_holder_commit_num = holder_commit_num + 1;
            estate.next_counterparty_commit_num = counterparty_commit_num + 1;
            estate.next_counterparty_revoke_num = counterparty_commit_num;
            estate.current_counterparty_point =
                Some(make_test_pubkey(counterparty_commit_num as u8));
            estate.previous_counterparty_point = None;
            estate.current_holder_commit_info = Some(CommitmentInfo2 {
                is_counterparty_broadcaster: false,
                to_countersigner_pubkey: make_test_pubkey((holder_commit_num + 100) as u8),
                to_countersigner_value_sat: to_counterparty_value_sat,
                revocation_pubkey: make_test_pubkey((holder_commit_num + 101) as u8),
                to_broadcaster_delayed_pubkey: make_test_pubkey((holder_commit_num + 102) as u8),
                to_broadcaster_value_sat: to_holder_value_sat,
                to_self_delay: setup.counterparty_selected_contest_delay,
                offered_htlcs: vec![],
                received_htlcs: vec![],
            });
            estate.current_counterparty_commit_info = Some(CommitmentInfo2 {
                is_counterparty_broadcaster: true,
                to_countersigner_pubkey: make_test_pubkey((counterparty_commit_num + 100) as u8),
                to_countersigner_value_sat: to_holder_value_sat,
                revocation_pubkey: make_test_pubkey((counterparty_commit_num + 101) as u8),
                to_broadcaster_delayed_pubkey: make_test_pubkey(
                    (counterparty_commit_num + 102) as u8,
                ),
                to_broadcaster_value_sat: to_counterparty_value_sat,
                to_self_delay: setup.holder_selected_contest_delay,
                offered_htlcs: vec![],
                received_htlcs: vec![],
            });
            estate.previous_counterparty_commit_info = None;
            estate.mutual_close_signed = false;
            Ok(())
        })
        .expect("state setup");

        Ok((
            secp_ctx,
            setup,
            node,
            channel_id,
            holder_commit_num,
            to_holder_value_sat,
            to_counterparty_value_sat,
            holder_wallet_path_hint,
            counterparty_points,
        ))
    }

    fn sign_mutual_close_tx_with_mutators<
        MutualCloseInputMutator,
        MutualCloseTxMutator,
        ChannelStateValidator,
    >(
        mutate_close_input: MutualCloseInputMutator,
        mutate_close_tx: MutualCloseTxMutator,
        validate_channel_state: ChannelStateValidator,
    ) -> Result<(), Status>
    where
        MutualCloseInputMutator: Fn(
            &mut Channel,
            &mut u64,
            &mut u64,
            &mut Option<Script>,
            &mut Option<Script>,
            &mut OutPoint,
        ),
        MutualCloseTxMutator: Fn(&mut Transaction, &mut Vec<Vec<u32>>, &mut Vec<String>),
        ChannelStateValidator: Fn(&Channel),
    {
        let (
            secp_ctx,
            setup,
            node,
            channel_id,
            holder_commit_num,
            to_holder_value_sat,
            to_counterparty_value_sat,
            holder_wallet_path_hint,
            counterparty_points,
        ) = setup_mutual_close_tx()?;

        let (tx, sigvec) = node.with_ready_channel(&channel_id, |chan| {
            let mut holder_value_sat = to_holder_value_sat;
            let mut counterparty_value_sat = to_counterparty_value_sat;
            let mut holder_shutdown_script = Some(
                Address::p2wpkh(
                    &node
                        .get_wallet_key(&secp_ctx, &holder_wallet_path_hint)
                        .unwrap()
                        .public_key(&secp_ctx),
                    Network::Testnet,
                )
                .expect("Address")
                .script_pubkey(),
            );
            let mut counterparty_shutdown_script = Some(
                Script::from_hex("0014be56df7de366ad8ee9ccdad54e9a9993e99ef565")
                    .expect("script_pubkey"),
            );
            let mut funding_outpoint = setup.funding_outpoint;

            mutate_close_input(
                chan,
                &mut holder_value_sat,
                &mut counterparty_value_sat,
                &mut holder_shutdown_script,
                &mut counterparty_shutdown_script,
                &mut funding_outpoint,
            );

            let tx = build_close_tx(
                holder_value_sat,
                counterparty_value_sat,
                &holder_shutdown_script,
                &counterparty_shutdown_script,
                funding_outpoint,
            )?;

            // Secrets can be released before the mutual close.
            assert!(chan
                .get_per_commitment_secret(holder_commit_num - 1)
                .is_ok());

            let mut mtx = tx.clone();
            let mut wallet_paths = vec![vec![], holder_wallet_path_hint.clone()];
            let mut allowlist = vec![];

            mutate_close_tx(&mut mtx, &mut wallet_paths, &mut allowlist);

            node.add_allowlist(&allowlist)?;

            // Sign the mutual close, but defer error returns till after
            // we check the state of the channel for side-effects.
            let deferred_rv = chan.sign_mutual_close_tx(&mtx, &wallet_paths);

            // This will panic if the state is not good.
            validate_channel_state(chan);

            let sig = deferred_rv?;
            Ok((tx, signature_to_bitcoin_vec(sig)))
        })?;

        let funding_pubkey = get_channel_funding_pubkey(&node, &channel_id);

        let channel_funding_redeemscript =
            make_funding_redeemscript(&funding_pubkey, &counterparty_points.funding_pubkey);

        check_signature(
            &tx,
            0,
            sigvec,
            &funding_pubkey,
            setup.channel_value_sat,
            &channel_funding_redeemscript,
        );

        // Secrets can still be released if they are old enough.
        assert_status_ok!(node.with_ready_channel(&channel_id, |chan| {
            chan.get_per_commitment_secret(holder_commit_num - 1)
        }));

        // policy-v2-revoke-not-closed
        // Channel is marked closed.
        assert_status_ok!(node.with_ready_channel(&channel_id, |chan| {
            assert_eq!(chan.enforcement_state.mutual_close_signed, true);
            Ok(())
        }));

        Ok(())
    }

    fn sign_mutual_close_tx_phase2_with_mutators<MutualCloseInput2Mutator, ChannelStateValidator>(
        mutate_close_input: MutualCloseInput2Mutator,
        validate_channel_state: ChannelStateValidator,
    ) -> Result<(), Status>
    where
        MutualCloseInput2Mutator: Fn(
            &mut Channel,
            &mut u64,
            &mut u64,
            &mut Option<Script>,
            &mut Option<Script>,
            &mut OutPoint,
            &mut Vec<u32>,
            &mut Vec<String>,
        ),
        ChannelStateValidator: Fn(&Channel),
    {
        let (
            secp_ctx,
            setup,
            node,
            channel_id,
            holder_commit_num,
            to_holder_value_sat,
            to_counterparty_value_sat,
            init_holder_wallet_path_hint,
            counterparty_points,
        ) = setup_mutual_close_tx()?;

        let (
            holder_value_sat,
            counterparty_value_sat,
            holder_shutdown_script,
            counterparty_shutdown_script,
            funding_outpoint,
            sigvec,
        ) = node.with_ready_channel(&channel_id, |chan| {
            let mut wallet_path = init_holder_wallet_path_hint.clone();
            let mut holder_value_sat = to_holder_value_sat;
            let mut counterparty_value_sat = to_counterparty_value_sat;
            let mut holder_shutdown_script = Some(
                Address::p2wpkh(
                    &node
                        .get_wallet_key(&secp_ctx, &wallet_path)
                        .unwrap()
                        .public_key(&secp_ctx),
                    Network::Testnet,
                )
                .expect("Address")
                .script_pubkey(),
            );
            let mut counterparty_shutdown_script = Some(
                Script::from_hex("0014be56df7de366ad8ee9ccdad54e9a9993e99ef565")
                    .expect("script_pubkey"),
            );
            let mut funding_outpoint = setup.funding_outpoint;

            // Secrets can be released before the mutual close.
            assert!(chan
                .get_per_commitment_secret(holder_commit_num - 1)
                .is_ok());

            let mut allowlist = vec![];

            mutate_close_input(
                chan,
                &mut holder_value_sat,
                &mut counterparty_value_sat,
                &mut holder_shutdown_script,
                &mut counterparty_shutdown_script,
                &mut funding_outpoint,
                &mut wallet_path,
                &mut allowlist,
            );

            node.add_allowlist(&allowlist)?;

            // Sign the mutual close, but defer error returns till after
            // we check the state of the channel for side-effects.
            let deferred_rv = chan.sign_mutual_close_tx_phase2(
                holder_value_sat,
                counterparty_value_sat,
                &holder_shutdown_script,
                &counterparty_shutdown_script,
                &wallet_path,
            );
            // This will panic if the state is not good.
            validate_channel_state(chan);

            let sig = deferred_rv?;
            Ok((
                holder_value_sat,
                counterparty_value_sat,
                holder_shutdown_script,
                counterparty_shutdown_script,
                funding_outpoint,
                signature_to_bitcoin_vec(sig),
            ))
        })?;

        let tx = {
            build_close_tx(
                holder_value_sat,
                counterparty_value_sat,
                &holder_shutdown_script,
                &counterparty_shutdown_script,
                funding_outpoint,
            )
        }?;

        let funding_pubkey = get_channel_funding_pubkey(&node, &channel_id);

        let channel_funding_redeemscript =
            make_funding_redeemscript(&funding_pubkey, &counterparty_points.funding_pubkey);

        check_signature(
            &tx,
            0,
            sigvec,
            &funding_pubkey,
            setup.channel_value_sat,
            &channel_funding_redeemscript,
        );

        // Secrets can still be released if they are old enough.
        assert_status_ok!(node.with_ready_channel(&channel_id, |chan| {
            chan.get_per_commitment_secret(holder_commit_num - 1)
        }));

        // policy-v2-revoke-not-closed
        // Channel is marked closed.
        assert_status_ok!(node.with_ready_channel(&channel_id, |chan| {
            assert_eq!(chan.enforcement_state.mutual_close_signed, true);
            Ok(())
        }));

        Ok(())
    }

    #[test]
    fn sign_mutual_close_tx_phase2_success() {
        assert_status_ok!(sign_mutual_close_tx_phase2_with_mutators(
            |_chan,
             _to_holder,
             _to_counterparty,
             _holder_script,
             _counter_script,
             _outpoint,
             _wallet_path,
             _allowlist| {
                // If we don't mutate anything it should succeed.
            },
            |chan| {
                // Channel should be marked closed
                assert_eq!(chan.enforcement_state.mutual_close_signed, true);
            }
        ));
    }

    #[test]
    fn sign_mutual_close_tx_success() {
        assert_status_ok!(sign_mutual_close_tx_with_mutators(
            |_chan, _to_holder, _to_counterparty, _holder_script, _counter_script, _outpoint| {
                // If we don't mutate anything it should succeed.
            },
            |_tx, _wallet_paths, _allowlist| {
                // If we don't mutate anything it should succeed.
            },
            |chan| {
                // Channel should be marked closed
                assert_eq!(chan.enforcement_state.mutual_close_signed, true);
            }
        ));
    }

    #[test]
    fn sign_mutual_close_tx_only_holder_success() {
        assert_status_ok!(sign_mutual_close_tx_with_mutators(
            |chan, to_holder, to_counterparty, _holder_script, counter_script, _outpoint| {
                // remove the counterparty from current_holder_commit_info
                let mut holder = chan
                    .enforcement_state
                    .current_holder_commit_info
                    .as_ref()
                    .unwrap()
                    .clone();
                holder.to_broadcaster_value_sat += holder.to_countersigner_value_sat;
                holder.to_countersigner_value_sat = 0;
                chan.enforcement_state.current_holder_commit_info = Some(holder);

                // remove the counterparty from current_counterparty_commit_info
                let mut cparty = chan
                    .enforcement_state
                    .current_counterparty_commit_info
                    .as_ref()
                    .unwrap()
                    .clone();
                cparty.to_countersigner_value_sat += cparty.to_broadcaster_value_sat;
                cparty.to_broadcaster_value_sat = 0;
                chan.enforcement_state.current_counterparty_commit_info = Some(cparty);

                // from the constructed tx
                *to_holder += *to_counterparty;
                *to_counterparty = 0;
                *counter_script = None;
            },
            |_tx, wallet_paths, _allowlist| {
                // remove the counterparties wallet_path
                wallet_paths[0] = wallet_paths[1].clone();
                wallet_paths.pop();
            },
            |chan| {
                // Channel should be marked closed
                assert_eq!(chan.enforcement_state.mutual_close_signed, true);
            }
        ));
    }

    #[test]
    fn sign_mutual_close_tx_only_counterparty_success() {
        assert_status_ok!(sign_mutual_close_tx_with_mutators(
            |chan, to_holder, to_counterparty, holder_script, _counter_script, _outpoint| {
                let fee = 2000;

                // remove the holder from current_holder_commit_info
                let mut holder = chan
                    .enforcement_state
                    .current_holder_commit_info
                    .as_ref()
                    .unwrap()
                    .clone();
                holder.to_countersigner_value_sat += holder.to_broadcaster_value_sat - fee;
                holder.to_broadcaster_value_sat = 0;
                chan.enforcement_state.current_holder_commit_info = Some(holder);

                // remove the holder from current_counterparty_commit_info
                let mut cparty = chan
                    .enforcement_state
                    .current_counterparty_commit_info
                    .as_ref()
                    .unwrap()
                    .clone();
                cparty.to_broadcaster_value_sat += cparty.to_countersigner_value_sat - fee;
                cparty.to_countersigner_value_sat = 0;
                chan.enforcement_state.current_counterparty_commit_info = Some(cparty);

                // from the constructed tx
                *to_counterparty += *to_holder - fee;
                *to_holder = 0;
                *holder_script = None;
            },
            |_tx, wallet_paths, _allowlist| {
                // remove the holders wallet_path
                wallet_paths.pop();
            },
            |chan| {
                // Channel should be marked closed
                assert_eq!(chan.enforcement_state.mutual_close_signed, true);
            }
        ));
    }

    #[test]
    fn sign_mutual_close_tx_catch_allowlist_bad_assign_success() {
        // This could happen if:
        // 1. A company used a common allowlist for all of it's nodes.
        // 2. NodeA opens a channel to NodeB (both company nodes).
        // 3. Channel is mutually closed immediately (only one output, to NodeA).
        // 4. NodeB incorrectly assigns the output because it's in the allowlist.
        assert_status_ok!(sign_mutual_close_tx_with_mutators(
            |chan, to_holder, to_counterparty, holder_script, counter_script, _outpoint| {
                let fee = 2000;

                // remove the holder from current_holder_commit_info
                let mut holder = chan
                    .enforcement_state
                    .current_holder_commit_info
                    .as_ref()
                    .unwrap()
                    .clone();
                holder.to_countersigner_value_sat += holder.to_broadcaster_value_sat - fee;
                holder.to_broadcaster_value_sat = 0;
                chan.enforcement_state.current_holder_commit_info = Some(holder);

                // remove the holder from current_counterparty_commit_info
                let mut cparty = chan
                    .enforcement_state
                    .current_counterparty_commit_info
                    .as_ref()
                    .unwrap()
                    .clone();
                cparty.to_broadcaster_value_sat += cparty.to_countersigner_value_sat - fee;
                cparty.to_countersigner_value_sat = 0;
                chan.enforcement_state.current_counterparty_commit_info = Some(cparty);

                // from the constructed tx
                *to_counterparty += *to_holder - fee;
                *to_holder = 0;
                *holder_script = None;

                // counterparty is using the allowlist
                *counter_script = Some(hex_script!("0014be56df7de366ad8ee9ccdad54e9a9993e99ef565"));
            },
            |_tx, wallet_paths, allowlist| {
                // remove all the walletpaths
                wallet_paths.pop();
                wallet_paths.pop();
                wallet_paths.push(vec![]); // only push one back, one output

                // add allowlist entry
                allowlist.push("tb1qhetd7l0rv6kca6wvmt25ax5ej05eaat9q29z7z".to_string());
            },
            |chan| {
                // Channel should be marked closed
                assert_eq!(chan.enforcement_state.mutual_close_signed, true);
            }
        ));
    }

    // policy-v2-mutual-destination-allowlisted
    #[test]
    fn sign_mutual_close_tx_with_allowlist_success() {
        assert_status_ok!(sign_mutual_close_tx_with_mutators(
            |_chan, _to_holder, _to_counterparty, _holder_script, _counter_script, _outpoint| {
                // If we don't mutate anything it should succeed.
            },
            |_tx, wallet_paths, allowlist| {
                // empty the wallet paths
                wallet_paths.pop();
                wallet_paths.pop();
                wallet_paths.push(vec![]);
                wallet_paths.push(vec![]);
                // use the allowlist
                allowlist.push("tb1qkakav8jpkhhs22hjrndrycyg3srshwd09gax07".to_string());
            },
            |chan| {
                // Channel should be marked closed
                assert_eq!(chan.enforcement_state.mutual_close_signed, true);
            }
        ));
    }

    // policy-v2-mutual-destination-allowlisted
    #[test]
    fn sign_mutual_close_tx_phase2_with_allowlist_success() {
        assert_status_ok!(sign_mutual_close_tx_phase2_with_mutators(
            |_chan,
             _to_holder,
             _to_counterparty,
             _holder_script,
             _counter_script,
             _outpoint,
             wallet_path,
             allowlist| {
                // Remove the wallet_path and use allowlist instead.
                *wallet_path = vec![];
                allowlist.push("tb1qkakav8jpkhhs22hjrndrycyg3srshwd09gax07".to_string());
            },
            |chan| {
                // Channel should be marked closed
                assert_eq!(chan.enforcement_state.mutual_close_signed, true);
            }
        ));
    }

    // policy-v2-mutual-destination-allowlisted
    #[test]
    fn sign_mutual_close_tx_phase2_no_wallet_path_or_allowlist() {
        assert_failed_precondition_err!(
            sign_mutual_close_tx_phase2_with_mutators(
                |_chan,
                 _to_holder,
                 _to_counterparty,
                 _holder_script,
                 _counter_script,
                 _outpoint,
                 wallet_path,
                 _allowlist| {
                    // Remove the wallet_path
                    *wallet_path = vec![];
                },
                |chan| {
                    // Channel should not be marked closed
                    assert_eq!(chan.enforcement_state.mutual_close_signed, false);
                }
            ),
            "policy failure: validate_mutual_close_tx: holder output not to wallet or in allowlist"
        );
    }

    #[test]
    fn sign_mutual_close_tx_phase2_holder_upfront_script_mismatch() {
        assert_failed_precondition_err!(
            sign_mutual_close_tx_phase2_with_mutators(
                |chan,
                 _to_holder,
                 _to_counterparty,
                 holder_script,
                 _counter_script,
                 _outpoint,
                 _wallet_path,
                 _allowlist| {
                    chan.setup.holder_shutdown_script =
                        Some(hex_script!("0014b76dd61e41b5ef052af21cda3260888c070bb9af"));
                    *holder_script = Some(hex_script!(
                        "76a9149f9a7abd600c0caa03983a77c8c3df8e062cb2fa88ac"
                    ));
                },
                |chan| {
                    // Channel should not be marked closed
                    assert_eq!(chan.enforcement_state.mutual_close_signed, false);
                }
            ),
            "policy failure: validate_mutual_close_tx: \
             holder_script doesn't match upfront holder_shutdown_script"
        );
    }

    // policy-v2-mutual-fee-range
    #[test]
    fn sign_mutual_close_tx_phase2_with_fee_too_large() {
        assert_failed_precondition_err!(
            sign_mutual_close_tx_phase2_with_mutators(
                |_chan,
                 to_holder,
                 to_counterparty,
                 _holder_script,
                 _counter_script,
                 _outpoint,
                 _wallet_path,
                 _allowlist| {
                    *to_holder -= 10_000;
                    *to_counterparty -= 10_000;
                },
                |chan| {
                    // Channel should not be marked closed
                    assert_eq!(chan.enforcement_state.mutual_close_signed, false);
                }
            ),
            "policy failure: validate_mutual_close_tx: fee too large 22000 > 21000"
        );
    }

    // policy-v2-mutual-fee-range
    #[test]
    fn sign_mutual_close_tx_phase2_with_fee_too_small() {
        assert_failed_precondition_err!(
            sign_mutual_close_tx_phase2_with_mutators(
                |_chan,
                 to_holder,
                 to_counterparty,
                 _holder_script,
                 _counter_script,
                 _outpoint,
                 _wallet_path,
                 _allowlist| {
                    *to_holder += 1_000;
                    *to_counterparty += 950;
                },
                |chan| {
                    // Channel should not be marked closed
                    assert_eq!(chan.enforcement_state.mutual_close_signed, false);
                }
            ),
            "policy failure: validate_mutual_close_tx: fee too small 50 < 100"
        );
    }

    #[test]
    fn sign_mutual_close_tx_with_bad_num_txout() {
        assert_failed_precondition_err!(
            sign_mutual_close_tx_with_mutators(
                |_chan,
                 _to_holder,
                 _to_counterparty,
                 _holder_script,
                 _counter_script,
                 _outpoint| {
                    // don't need to mutate these
                },
                |tx, wallet_paths, _allowlist| {
                    // Steal some of the first output and make a new output.
                    let steal_amt = 1_000;
                    tx.output[0].value -= steal_amt;
                    tx.output.push(TxOut {
                        value: steal_amt,
                        script_pubkey: hex_script!(
                            "76a9149f9a7abd600c0caa03983a77c8c3df8e062cb2fa88ac"
                        ),
                    });
                    wallet_paths.push(vec![]); // needs to match
                },
                |chan| {
                    // Channel should not be marked closed
                    assert_eq!(chan.enforcement_state.mutual_close_signed, false);
                }
            ),
            "transaction format: decode_and_validate_mutual_close_tx: invalid number of outputs: 3"
        );
    }

    #[test]
    fn sign_mutual_close_tx_with_opath_len_mismatch() {
        assert_invalid_argument_err!(
            sign_mutual_close_tx_with_mutators(
                |_chan,
                 _to_holder,
                 _to_counterparty,
                 _holder_script,
                 _counter_script,
                 _outpoint| {
                    // don't need to mutate these
                },
                |_tx, wallet_paths, _allowlist| {
                    wallet_paths.push(vec![]); // an extra opath element
                },
                |chan| {
                    // Channel should be not marked closed
                    assert_eq!(chan.enforcement_state.mutual_close_signed, false);
                }
            ),
            "sign_mutual_close_tx: bad opath len 3 with tx.output len 2"
        );
    }

    // policy-v2-mutual-destination-allowlisted
    #[test]
    fn sign_mutual_close_tx_with_unestablished_holder() {
        assert_failed_precondition_err!(
            sign_mutual_close_tx_with_mutators(
                |_chan,
                 _to_holder,
                 _to_counterparty,
                 _holder_script,
                 _counter_script,
                 _outpoint| {
                    // don't need to mutate these
                },
                |_tx, wallet_paths, _allowlist| {
                    wallet_paths.pop();
                    wallet_paths.pop();
                    wallet_paths.push(vec![]);
                    wallet_paths.push(vec![]);
                },
                |chan| {
                    // Channel should not be marked closed
                    assert_eq!(chan.enforcement_state.mutual_close_signed, false);
                }
            ),
            "policy failure: validate_mutual_close_tx: holder output not to wallet or in allowlist"
        );
    }

    #[test]
    fn sign_mutual_close_tx_with_ambiguous_holder_output() {
        // Both outputs are allowlisted (common company allowlist,
        // channel w/ two company nodes).  Need to use value to pick the output ...
        assert_status_ok!(sign_mutual_close_tx_with_mutators(
            |chan, to_holder, to_counterparty, _holder_script, _counter_script, _outpoint| {
                // The hard case is when the holder's input is the first, so we need
                // to swap the outputs and values here.

                // Swap the setup values
                mem::swap(to_holder, to_counterparty);

                // Swap the holder commitment's values
                let mut hinfo = chan
                    .enforcement_state
                    .current_holder_commit_info
                    .as_ref()
                    .unwrap()
                    .clone();
                mem::swap(
                    &mut hinfo.to_broadcaster_value_sat,
                    &mut hinfo.to_countersigner_value_sat,
                );
                chan.enforcement_state.current_holder_commit_info = Some(hinfo);

                // Swap the counterparty commitment values
                let mut cinfo = chan
                    .enforcement_state
                    .current_counterparty_commit_info
                    .as_ref()
                    .unwrap()
                    .clone();
                mem::swap(
                    &mut cinfo.to_broadcaster_value_sat,
                    &mut cinfo.to_countersigner_value_sat,
                );
                chan.enforcement_state.current_counterparty_commit_info = Some(cinfo);
            },
            |_tx, wallet_paths, allowlist| {
                // remove the wallet paths
                wallet_paths.pop();
                wallet_paths.pop();
                wallet_paths.push(vec![]);
                wallet_paths.push(vec![]);

                // add both outputs to the allowlist
                allowlist.push("tb1qhetd7l0rv6kca6wvmt25ax5ej05eaat9q29z7z".to_string());
                allowlist.push("tb1qkakav8jpkhhs22hjrndrycyg3srshwd09gax07".to_string());
            },
            |chan| {
                // Channel should be marked closed
                assert_eq!(chan.enforcement_state.mutual_close_signed, true);
            }
        ));
    }

    #[test]
    fn sign_mutual_close_tx_without_holder_commitment() {
        assert_failed_precondition_err!(
            sign_mutual_close_tx_with_mutators(
                |chan, _to_holder, _to_counterparty, _holder_script, _counter_script, _outpoint| {
                    chan.enforcement_state.current_holder_commit_info = None;
                },
                |_tx, _wallet_paths, _allowlist| {
                    // don't need to mutate these
                },
                |chan| {
                    // Channel should not be marked closed
                    assert_eq!(chan.enforcement_state.mutual_close_signed, false);
                }
            ),
            "policy failure: decode_and_validate_mutual_close_tx: \
             current_holder_commit_info missing"
        );
    }

    #[test]
    fn sign_mutual_close_tx_without_counterparty_commitment() {
        assert_failed_precondition_err!(
            sign_mutual_close_tx_with_mutators(
                |chan, _to_holder, _to_counterparty, _holder_script, _counter_script, _outpoint| {
                    chan.enforcement_state.current_counterparty_commit_info = None;
                },
                |_tx, _wallet_paths, _allowlist| {
                    // don't need to mutate these
                },
                |chan| {
                    // Channel should not be marked closed
                    assert_eq!(chan.enforcement_state.mutual_close_signed, false);
                }
            ),
            "policy failure: decode_and_validate_mutual_close_tx: \
             current_counterparty_commit_info missing"
        );
    }

    // policy-v2-mutual-no-pending-htlcs
    #[test]
    fn sign_mutual_close_tx_with_holder_offered_htlcs() {
        assert_failed_precondition_err!(
            sign_mutual_close_tx_with_mutators(
                |chan, _to_holder, _to_counterparty, _holder_script, _counter_script, _outpoint| {
                    let mut holder = chan
                        .enforcement_state
                        .current_holder_commit_info
                        .as_ref()
                        .unwrap()
                        .clone();
                    holder.offered_htlcs.push(HTLCInfo2 {
                        value_sat: 1,
                        payment_hash: PaymentHash([1; 32]),
                        cltv_expiry: 2 << 16,
                    });
                    chan.enforcement_state.current_holder_commit_info = Some(holder);
                },
                |_tx, _wallet_paths, _allowlist| {
                    // don't need to mutate these
                },
                |chan| {
                    // Channel should not be marked closed
                    assert_eq!(chan.enforcement_state.mutual_close_signed, false);
                }
            ),
            "policy failure: validate_mutual_close_tx: cannot close with pending htlcs"
        );
    }

    // policy-v2-mutual-no-pending-htlcs
    #[test]
    fn sign_mutual_close_tx_with_holder_received_htlcs() {
        assert_failed_precondition_err!(
            sign_mutual_close_tx_with_mutators(
                |chan, _to_holder, _to_counterparty, _holder_script, _counter_script, _outpoint| {
                    let mut holder = chan
                        .enforcement_state
                        .current_holder_commit_info
                        .as_ref()
                        .unwrap()
                        .clone();
                    holder.received_htlcs.push(HTLCInfo2 {
                        value_sat: 1,
                        payment_hash: PaymentHash([1; 32]),
                        cltv_expiry: 2 << 16,
                    });
                    chan.enforcement_state.current_holder_commit_info = Some(holder);
                },
                |_tx, _wallet_paths, _allowlist| {
                    // don't need to mutate these
                },
                |chan| {
                    // Channel should not be marked closed
                    assert_eq!(chan.enforcement_state.mutual_close_signed, false);
                }
            ),
            "policy failure: validate_mutual_close_tx: cannot close with pending htlcs"
        );
    }

    // policy-v2-mutual-no-pending-htlcs
    #[test]
    fn sign_mutual_close_tx_with_counterparty_offered_htlcs() {
        assert_failed_precondition_err!(
            sign_mutual_close_tx_with_mutators(
                |chan, _to_holder, _to_counterparty, _holder_script, _counter_script, _outpoint| {
                    let mut cparty = chan
                        .enforcement_state
                        .current_counterparty_commit_info
                        .as_ref()
                        .unwrap()
                        .clone();
                    cparty.offered_htlcs.push(HTLCInfo2 {
                        value_sat: 1,
                        payment_hash: PaymentHash([1; 32]),
                        cltv_expiry: 2 << 16,
                    });
                    chan.enforcement_state.current_counterparty_commit_info = Some(cparty);
                },
                |_tx, _wallet_paths, _allowlist| {
                    // don't need to mutate these
                },
                |chan| {
                    // Channel should not be marked closed
                    assert_eq!(chan.enforcement_state.mutual_close_signed, false);
                }
            ),
            "policy failure: validate_mutual_close_tx: cannot close with pending htlcs"
        );
    }

    // policy-v2-mutual-no-pending-htlcs
    #[test]
    fn sign_mutual_close_tx_with_counterparty_received_htlcs() {
        assert_failed_precondition_err!(
            sign_mutual_close_tx_with_mutators(
                |chan, _to_holder, _to_counterparty, _holder_script, _counter_script, _outpoint| {
                    let mut cparty = chan
                        .enforcement_state
                        .current_counterparty_commit_info
                        .as_ref()
                        .unwrap()
                        .clone();
                    cparty.received_htlcs.push(HTLCInfo2 {
                        value_sat: 1,
                        payment_hash: PaymentHash([1; 32]),
                        cltv_expiry: 2 << 16,
                    });
                    chan.enforcement_state.current_counterparty_commit_info = Some(cparty);
                },
                |_tx, _wallet_paths, _allowlist| {
                    // don't need to mutate these
                },
                |chan| {
                    // Channel should not be marked closed
                    assert_eq!(chan.enforcement_state.mutual_close_signed, false);
                }
            ),
            "policy failure: validate_mutual_close_tx: cannot close with pending htlcs"
        );
    }

    // policy-v2-mutual-value-matches-commitment
    #[test]
    fn sign_mutual_close_tx_with_holder_commitment_too_large() {
        assert_failed_precondition_err!(
            sign_mutual_close_tx_with_mutators(
                |chan, _to_holder, _to_counterparty, _holder_script, _counter_script, _outpoint| {
                    let mut holder = chan
                        .enforcement_state
                        .current_holder_commit_info
                        .as_ref()
                        .unwrap()
                        .clone();
                    holder.to_broadcaster_value_sat += 80_000;
                    holder.to_countersigner_value_sat -= 80_000;
                    chan.enforcement_state.current_holder_commit_info = Some(holder);
                },
                |_tx, _wallet_paths, _allowlist| {
                    // don't need to mutate these
                },
                |chan| {
                    // Channel should not be marked closed
                    assert_eq!(chan.enforcement_state.mutual_close_signed, false);
                }
            ),
            "policy failure: validate_mutual_close_tx: \
             to_holder_value 1000000 is smaller than holder_info.broadcaster_value_sat 2078000"
        );
    }

    // policy-v2-mutual-value-matches-commitment
    #[test]
    fn sign_mutual_close_tx_with_holder_commitment_too_small() {
        assert_failed_precondition_err!(
            sign_mutual_close_tx_with_mutators(
                |chan, _to_holder, _to_counterparty, _holder_script, _counter_script, _outpoint| {
                    let mut holder = chan
                        .enforcement_state
                        .current_holder_commit_info
                        .as_ref()
                        .unwrap()
                        .clone();
                    holder.to_broadcaster_value_sat -= 80_000;
                    holder.to_countersigner_value_sat += 80_000;
                    chan.enforcement_state.current_holder_commit_info = Some(holder);
                },
                |_tx, _wallet_paths, _allowlist| {
                    // don't need to mutate these
                },
                |chan| {
                    // Channel should not be marked closed
                    assert_eq!(chan.enforcement_state.mutual_close_signed, false);
                }
            ),
            "policy failure: validate_mutual_close_tx: \
             to_holder_value 1000000 is smaller than holder_info.broadcaster_value_sat 1918000"
        );
    }

    // policy-v2-mutual-value-matches-commitment
    #[test]
    fn sign_mutual_close_tx_with_counterparty_commitment_too_small() {
        assert_failed_precondition_err!(
            sign_mutual_close_tx_with_mutators(
                |chan, _to_holder, _to_counterparty, _holder_script, _counter_script, _outpoint| {
                    let mut counterparty = chan
                        .enforcement_state
                        .current_counterparty_commit_info
                        .as_ref()
                        .unwrap()
                        .clone();
                    counterparty.to_broadcaster_value_sat += 80_000;
                    counterparty.to_countersigner_value_sat -= 80_000;
                    chan.enforcement_state.current_counterparty_commit_info = Some(counterparty);
                },
                |_tx, _wallet_paths, _allowlist| {
                    // don't need to mutate these
                },
                |chan| {
                    // Channel should not be marked closed
                    assert_eq!(chan.enforcement_state.mutual_close_signed, false);
                }
            ),
            "policy failure: validate_mutual_close_tx: \
             to_holder_value 1000000 is smaller than holder_info.broadcaster_value_sat 1998000"
        );
    }

    // policy-v2-mutual-value-matches-commitment
    #[test]
    fn sign_mutual_close_tx_with_counterparty_commitment_too_large() {
        assert_failed_precondition_err!(
            sign_mutual_close_tx_with_mutators(
                |chan, _to_holder, _to_counterparty, _holder_script, _counter_script, _outpoint| {
                    let mut counterparty = chan
                        .enforcement_state
                        .current_counterparty_commit_info
                        .as_ref()
                        .unwrap()
                        .clone();
                    counterparty.to_broadcaster_value_sat -= 80_000;
                    counterparty.to_countersigner_value_sat += 80_000;
                    chan.enforcement_state.current_counterparty_commit_info = Some(counterparty);
                },
                |_tx, _wallet_paths, _allowlist| {
                    // don't need to mutate these
                },
                |chan| {
                    // Channel should not be marked closed
                    assert_eq!(chan.enforcement_state.mutual_close_signed, false);
                }
            ),
            "policy failure: validate_mutual_close_tx: \
             to_holder_value 1000000 is smaller than holder_info.broadcaster_value_sat 1998000"
        );
    }

    #[test]
    fn sign_justice_sweep_test() {
        let (node, channel_id) =
            init_node_and_channel(TEST_NODE_CONFIG, TEST_SEED[1], make_test_channel_setup());

        let commitment_txid = bitcoin::Txid::from_slice(&[2u8; 32]).unwrap();
        let feerate_per_kw = 1000;
        let to_self_delay = 32;
        let htlc = HTLCOutputInCommitment {
            offered: true,
            amount_msat: 1 * 1000 * 1000,
            cltv_expiry: 2 << 16,
            payment_hash: PaymentHash([1; 32]),
            transaction_output_index: Some(0),
        };

        let secp_ctx = Secp256k1::new();

        let n: u64 = 1;

        let (per_commitment_point, per_commitment_secret) = node
            .with_ready_channel(&channel_id, |chan| {
                chan.enforcement_state
                    .set_next_holder_commit_num_for_testing(n);
                let point = chan.get_per_commitment_point(n)?;
                chan.enforcement_state
                    .set_next_holder_commit_num_for_testing(n + 2);
                let secret = chan.get_per_commitment_secret(n)?;
                Ok((point, secret))
            })
            .expect("point");

        let a_delayed_payment_base = make_test_pubkey(2);

        let a_delayed_payment_pubkey =
            derive_public_key(&secp_ctx, &per_commitment_point, &a_delayed_payment_base)
                .expect("a_delayed_payment_pubkey");

        let (b_revocation_base_point, b_revocation_base_secret) = make_test_key(42);

        let revocation_pubkey =
            derive_revocation_pubkey(&secp_ctx, &per_commitment_point, &b_revocation_base_point)
                .expect("revocation_pubkey");

        let htlc_tx = build_htlc_transaction(
            &commitment_txid,
            feerate_per_kw,
            to_self_delay,
            &htlc,
            &a_delayed_payment_pubkey,
            &revocation_pubkey,
        );

        let redeemscript = get_revokeable_redeemscript(
            &revocation_pubkey,
            to_self_delay,
            &a_delayed_payment_pubkey,
        );

        let htlc_amount_sat = 10 * 1000;

        let revocation_secret = derive_private_revocation_key(
            &secp_ctx,
            &per_commitment_secret,
            &b_revocation_base_secret,
        )
        .expect("revocation_secret");

        let revocation_point = PublicKey::from_secret_key(&secp_ctx, &revocation_secret);

        let sigvec = node
            .with_ready_channel(&channel_id, |chan| {
                let sig = chan
                    .sign_justice_sweep(
                        &htlc_tx,
                        0,
                        &revocation_secret,
                        &redeemscript,
                        htlc_amount_sat,
                    )
                    .unwrap();
                Ok(signature_to_bitcoin_vec(sig))
            })
            .unwrap();

        let pubkey = get_channel_revocation_pubkey(&node, &channel_id, &revocation_point);

        check_signature(&htlc_tx, 0, sigvec, &pubkey, htlc_amount_sat, &redeemscript);
    }

    #[test]
    fn sign_channel_announcement_test() {
        let (node, channel_id) =
            init_node_and_channel(TEST_NODE_CONFIG, TEST_SEED[1], make_test_channel_setup());

        let ann = hex_decode("0123456789abcdef").unwrap();
        let (nsig, bsig) = node
            .with_ready_channel(&channel_id, |chan| Ok(chan.sign_channel_announcement(&ann)))
            .unwrap();

        let ca_hash = Sha256dHash::hash(&ann);
        let encmsg = secp256k1::Message::from_slice(&ca_hash[..]).expect("encmsg");
        let secp_ctx = Secp256k1::new();
        secp_ctx
            .verify(&encmsg, &nsig, &node.get_id())
            .expect("verify nsig");
        let _res: Result<(), Status> = node.with_ready_channel(&channel_id, |chan| {
            let funding_pubkey = PublicKey::from_secret_key(&secp_ctx, &chan.keys.funding_key);
            Ok(secp_ctx
                .verify(&encmsg, &bsig, &funding_pubkey)
                .expect("verify bsig"))
        });
    }

    #[test]
    fn sign_node_announcement_test() -> Result<(), ()> {
        let node = init_node(TEST_NODE_CONFIG, TEST_SEED[1]);
        let ann = hex_decode("000302aaa25e445fef0265b6ab5ec860cd257865d61ef0bbf5b3339c36cbda8b26b74e7f1dca490b65180265b64c4f554450484f544f2d2e302d3139392d67613237336639642d6d6f646465640000").unwrap();
        let sigvec = node.sign_node_announcement(&ann).unwrap();
        assert_eq!(sigvec, hex_decode("30450221008ef1109b95f127a7deec63b190b72180f0c2692984eaf501c44b6bfc5c4e915502207a6fa2f250c5327694967be95ff42a94a9c3d00b7fa0fbf7daa854ceb872e439").unwrap());
        Ok(())
    }

    #[test]
    fn sign_channel_update_test() -> Result<(), ()> {
        let node = init_node(TEST_NODE_CONFIG, TEST_SEED[1]);
        let cu = hex_decode("06226e46111a0b59caaf126043eb5bbf28c34f3a5e332a1fc7b2b73cf188910f00006700000100015e42ddc6010000060000000000000000000000010000000a000000003b023380").unwrap();
        let sigvec = node.sign_channel_update(&cu).unwrap();
        assert_eq!(sigvec, hex_decode("3045022100be9840696c868b161aaa997f9fa91a899e921ea06c8083b2e1ea32b8b511948d0220352eec7a74554f97c2aed26950b8538ca7d7d7568b42fd8c6f195bd749763fa5").unwrap());
        Ok(())
    }

    #[test]
    fn sign_invoice_test() -> Result<(), ()> {
        let node = init_node(TEST_NODE_CONFIG, TEST_SEED[1]);
        let human_readable_part = String::from("lnbcrt1230n");
        let data_part = hex_decode("010f0418090a010101141917110f01040e050f06100003021e1b0e13161c150301011415060204130c0018190d07070a18070a1c1101111e111f130306000d00120c11121706181b120d051807081a0b0f0d18060004120e140018000105100114000b130b01110c001a05041a181716020007130c091d11170d10100d0b1a1b00030e05190208171e16080d00121a00110719021005000405001000").unwrap();
        let rsig = node
            .sign_invoice_in_parts(&data_part, &human_readable_part)
            .unwrap();
        assert_eq!(rsig, hex_decode("739ffb91aa7c0b3d3c92de1600f7a9afccedc5597977095228232ee4458685531516451b84deb35efad27a311ea99175d10c6cdb458cd27ce2ed104eb6cf806400").unwrap());
        Ok(())
    }

    #[test]
    fn sign_invoice_with_overhang_test() -> Result<(), ()> {
        let node = init_node(TEST_NODE_CONFIG, TEST_SEED[1]);
        let human_readable_part = String::from("lnbcrt2m");
        let data_part = hex_decode("010f0a001d051e0101140c0c000006140009160c09051a0d1a190708020d17141106171f0f07131616111f1910070b0d0e150c0c0c0d010d1a01181c15100d010009181a06101a0a0309181b040a111a0a06111705100c0b18091909030e151b14060004120e14001800010510011419080f1307000a0a0517021c171410101a1e101605050a08180d0d110e13150409051d02091d181502020f050e1a1f161a09130005000405001000").unwrap();
        // The data_part is 170 bytes.
        // overhang = (data_part.len() * 5) % 8 = 2
        // looking for a verified invoice where overhang is in 1..3
        let rsig = node
            .sign_invoice_in_parts(&data_part, &human_readable_part)
            .unwrap();
        assert_eq!(rsig, hex_decode("f278cdba3fd4a37abf982cee5a66f52e142090631ef57763226f1232eead78b43da7962fcfe29ffae9bd918c588df71d6d7b92a4787de72801594b22f0e7e62a00").unwrap());
        Ok(())
    }

    #[test]
    fn ecdh_test() {
        let node = init_node(TEST_NODE_CONFIG, TEST_SEED[1]);
        let pointvec =
            hex_decode("0330febba06ba074378dec994669cf5ebf6b15e24a04ec190fb93a9482e841a0ca")
                .unwrap();
        let other_key = PublicKey::from_slice(pointvec.as_slice()).unwrap();

        let ssvec = node.ecdh(&other_key);
        assert_eq!(
            ssvec,
            hex_decode("48db1582f4b42a0068b5727fd37090a65fbf1f9bd842f4393afc2e794719ae47").unwrap()
        );
    }

    #[test]
    fn get_unilateral_close_key_test() {
        let node = init_node(TEST_NODE_CONFIG, TEST_SEED[0]);
        let channel_nonce = hex_decode(
            "022d223620a359a47ff7f7ac447c85c46c923da53389221a0054c11c1e3ca31d590100000000000000",
        )
        .unwrap();
        let (channel_id, _) = node.new_channel(None, Some(channel_nonce), &node).unwrap();

        node.ready_channel(channel_id, None, make_test_channel_setup(), &vec![])
            .expect("ready channel");

        let uck = node
            .with_ready_channel(&channel_id, |chan| chan.get_unilateral_close_key(&None))
            .unwrap();

        assert_eq!(
            uck,
            SecretKey::from_slice(
                &hex_decode("d5f8a9fdd0e4be18c33656944b91dc1f6f2c38ce2a4bbd0ef330ffe4e106127c")
                    .unwrap()[..]
            )
            .unwrap()
        );
    }

    #[test]
    fn get_account_ext_pub_key_test() {
        let node = init_node(TEST_NODE_CONFIG, TEST_SEED[1]);
        let xpub = node.get_account_extended_pubkey();
        assert_eq!(format!("{}", xpub), "tpubDAu312RD7nE6R9qyB4xJk9QAMyi3ppq3UJ4MMUGpB9frr6eNDd8FJVPw27zTVvWAfYFVUtJamgfh5ZLwT23EcymYgLx7MHsU8zZxc9L3GKk");
    }

    #[test]
    fn sign_message_test() {
        let node = init_node(TEST_NODE_CONFIG, TEST_SEED[1]);
        let message = String::from("Testing 1 2 3").into_bytes();
        let mut rsigvec = node.sign_message(&message).unwrap();
        let rid = rsigvec.pop().unwrap() as i32;
        let rsig =
            RecoverableSignature::from_compact(&rsigvec[..], RecoveryId::from_i32(rid).unwrap())
                .unwrap();
        let secp_ctx = secp256k1::Secp256k1::new();
        let mut buffer = String::from("Lightning Signed Message:").into_bytes();
        buffer.extend(message);
        let hash = Sha256dHash::hash(&buffer);
        let encmsg = secp256k1::Message::from_slice(&hash[..]).unwrap();
        let sig =
            secp256k1::Signature::from_compact(&rsig.to_standard().serialize_compact()).unwrap();
        let pubkey = secp_ctx.recover(&encmsg, &rsig).unwrap();
        assert!(secp_ctx.verify(&encmsg, &sig, &pubkey).is_ok());
        assert_eq!(
            pubkey.serialize().to_vec(),
            node.get_id().serialize().to_vec()
        );
    }

    // TODO move this elsewhere
    #[test]
    fn transaction_verify_test() {
        // a random recent segwit transaction from blockchain using both old and segwit inputs
        let spending: bitcoin::Transaction = deserialize(hex_decode("020000000001031cfbc8f54fbfa4a33a30068841371f80dbfe166211242213188428f437445c91000000006a47304402206fbcec8d2d2e740d824d3d36cc345b37d9f65d665a99f5bd5c9e8d42270a03a8022013959632492332200c2908459547bf8dbf97c65ab1a28dec377d6f1d41d3d63e012103d7279dfb90ce17fe139ba60a7c41ddf605b25e1c07a4ddcb9dfef4e7d6710f48feffffff476222484f5e35b3f0e43f65fc76e21d8be7818dd6a989c160b1e5039b7835fc00000000171600140914414d3c94af70ac7e25407b0689e0baa10c77feffffffa83d954a62568bbc99cc644c62eb7383d7c2a2563041a0aeb891a6a4055895570000000017160014795d04cc2d4f31480d9a3710993fbd80d04301dffeffffff06fef72f000000000017a91476fd7035cd26f1a32a5ab979e056713aac25796887a5000f00000000001976a914b8332d502a529571c6af4be66399cd33379071c588ac3fda0500000000001976a914fc1d692f8de10ae33295f090bea5fe49527d975c88ac522e1b00000000001976a914808406b54d1044c429ac54c0e189b0d8061667e088ac6eb68501000000001976a914dfab6085f3a8fb3e6710206a5a959313c5618f4d88acbba20000000000001976a914eb3026552d7e3f3073457d0bee5d4757de48160d88ac0002483045022100bee24b63212939d33d513e767bc79300051f7a0d433c3fcf1e0e3bf03b9eb1d70220588dc45a9ce3a939103b4459ce47500b64e23ab118dfc03c9caa7d6bfc32b9c601210354fd80328da0f9ae6eef2b3a81f74f9a6f66761fadf96f1d1d22b1fd6845876402483045022100e29c7e3a5efc10da6269e5fc20b6a1cb8beb92130cc52c67e46ef40aaa5cac5f0220644dd1b049727d991aece98a105563416e10a5ac4221abac7d16931842d5c322012103960b87412d6e169f30e12106bdf70122aabb9eb61f455518322a18b920a4dfa887d30700")
            .unwrap().as_slice()).unwrap();
        let spent1: bitcoin::Transaction = deserialize(hex_decode("020000000001040aacd2c49f5f3c0968cfa8caf9d5761436d95385252e3abb4de8f5dcf8a582f20000000017160014bcadb2baea98af0d9a902e53a7e9adff43b191e9feffffff96cd3c93cac3db114aafe753122bd7d1afa5aa4155ae04b3256344ecca69d72001000000171600141d9984579ceb5c67ebfbfb47124f056662fe7adbfeffffffc878dd74d3a44072eae6178bb94b9253177db1a5aaa6d068eb0e4db7631762e20000000017160014df2a48cdc53dae1aba7aa71cb1f9de089d75aac3feffffffe49f99275bc8363f5f593f4eec371c51f62c34ff11cc6d8d778787d340d6896c0100000017160014229b3b297a0587e03375ab4174ef56eeb0968735feffffff03360d0f00000000001976a9149f44b06f6ee92ddbc4686f71afe528c09727a5c788ac24281b00000000001976a9140277b4f68ff20307a2a9f9b4487a38b501eb955888ac227c0000000000001976a9148020cd422f55eef8747a9d418f5441030f7c9c7788ac0247304402204aa3bd9682f9a8e101505f6358aacd1749ecf53a62b8370b97d59243b3d6984f02200384ad449870b0e6e89c92505880411285ecd41cf11e7439b973f13bad97e53901210205b392ffcb83124b1c7ce6dd594688198ef600d34500a7f3552d67947bbe392802473044022033dfd8d190a4ae36b9f60999b217c775b96eb10dee3a1ff50fb6a75325719106022005872e4e36d194e49ced2ebcf8bb9d843d842e7b7e0eb042f4028396088d292f012103c9d7cbf369410b090480de2aa15c6c73d91b9ffa7d88b90724614b70be41e98e0247304402207d952de9e59e4684efed069797e3e2d993e9f98ec8a9ccd599de43005fe3f713022076d190cc93d9513fc061b1ba565afac574e02027c9efbfa1d7b71ab8dbb21e0501210313ad44bc030cc6cb111798c2bf3d2139418d751c1e79ec4e837ce360cc03b97a024730440220029e75edb5e9413eb98d684d62a077b17fa5b7cc19349c1e8cc6c4733b7b7452022048d4b9cae594f03741029ff841e35996ef233701c1ea9aa55c301362ea2e2f68012103590657108a72feb8dc1dec022cf6a230bb23dc7aaa52f4032384853b9f8388baf9d20700")
            .unwrap().as_slice()).unwrap();
        let spent2: bitcoin::Transaction = deserialize(hex_decode("0200000000010166c3d39490dc827a2594c7b17b7d37445e1f4b372179649cd2ce4475e3641bbb0100000017160014e69aa750e9bff1aca1e32e57328b641b611fc817fdffffff01e87c5d010000000017a914f3890da1b99e44cd3d52f7bcea6a1351658ea7be87024830450221009eb97597953dc288de30060ba02d4e91b2bde1af2ecf679c7f5ab5989549aa8002202a98f8c3bd1a5a31c0d72950dd6e2e3870c6c5819a6c3db740e91ebbbc5ef4800121023f3d3b8e74b807e32217dea2c75c8d0bd46b8665b3a2d9b3cb310959de52a09bc9d20700")
            .unwrap().as_slice()).unwrap();
        let spent3: bitcoin::Transaction = deserialize(hex_decode("01000000027a1120a30cef95422638e8dab9dedf720ec614b1b21e451a4957a5969afb869d000000006a47304402200ecc318a829a6cad4aa9db152adbf09b0cd2de36f47b53f5dade3bc7ef086ca702205722cda7404edd6012eedd79b2d6f24c0a0c657df1a442d0a2166614fb164a4701210372f4b97b34e9c408741cd1fc97bcc7ffdda6941213ccfde1cb4075c0f17aab06ffffffffc23b43e5a18e5a66087c0d5e64d58e8e21fcf83ce3f5e4f7ecb902b0e80a7fb6010000006b483045022100f10076a0ea4b4cf8816ed27a1065883efca230933bf2ff81d5db6258691ff75202206b001ef87624e76244377f57f0c84bc5127d0dd3f6e0ef28b276f176badb223a01210309a3a61776afd39de4ed29b622cd399d99ecd942909c36a8696cfd22fc5b5a1affffffff0200127a000000000017a914f895e1dd9b29cb228e9b06a15204e3b57feaf7cc8769311d09000000001976a9144d00da12aaa51849d2583ae64525d4a06cd70fde88ac00000000")
            .unwrap().as_slice()).unwrap();

        println!("{:?}", &spending.txid());
        println!("{:?}", &spent1.txid());
        println!("{:?}", &spent2.txid());
        println!("{:?}", &spent3.txid());
        println!("{:?}", &spent1.output[0].script_pubkey);
        println!("{:?}", &spent2.output[0].script_pubkey);
        println!("{:?}", &spent3.output[0].script_pubkey);

        let mut spent = Map::new();
        spent.insert(spent1.txid(), spent1);
        spent.insert(spent2.txid(), spent2);
        spent.insert(spent3.txid(), spent3);
        spending
            .verify(|point: &OutPoint| {
                if let Some(tx) = spent.remove(&point.txid) {
                    return tx.output.get(point.vout as usize).cloned();
                }
                None
            })
            .unwrap();
    }

    // TODO move this elsewhere
    #[test]
    fn bip143_p2wpkh_test() {
        let tx: bitcoin::Transaction = deserialize(hex_decode("0100000002fff7f7881a8099afa6940d42d1e7f6362bec38171ea3edf433541db4e4ad969f0000000000eeffffffef51e1b804cc89d182d279655c3aa89e815b1b309fe287d9b2b55d57b90ec68a0100000000ffffffff02202cb206000000001976a9148280b37df378db99f66f85c95a783a76ac7a6d5988ac9093510d000000001976a9143bde42dbee7e4dbe6a21b2d50ce2f0167faa815988ac11000000")
            .unwrap().as_slice()).unwrap();
        let secp_ctx = Secp256k1::signing_only();
        let priv2 = SecretKey::from_slice(
            hex_decode("619c335025c7f4012e556c2a58b2506e30b8511b53ade95ea316fd8c3286feb9")
                .unwrap()
                .as_slice(),
        )
        .unwrap();
        let pub2 = bitcoin::PublicKey::from_slice(
            &PublicKey::from_secret_key(&secp_ctx, &priv2).serialize(),
        )
        .unwrap();

        let script_code = Address::p2pkh(&pub2, Network::Testnet).script_pubkey();
        assert_eq!(
            hex_encode(script_code.as_bytes()),
            "76a9141d0f172a0ecb48aee1be1f2687d2963ae33f71a188ac"
        );
        let value = 600_000_000;

        let sighash =
            &SigHashCache::new(&tx).signature_hash(1, &script_code, value, SigHashType::All)[..];
        assert_eq!(
            hex_encode(sighash),
            "c37af31116d1b27caf68aae9e3ac82f1477929014d5b917657d0eb49478cb670"
        );
    }

    fn sign_commitment_tx_with_mutators_setup() -> (
        Arc<Node>,
        ChannelSetup,
        ChannelId,
        Vec<HTLCInfo2>,
        Vec<HTLCInfo2>,
    ) {
        let setup = make_test_channel_setup();
        let (node, channel_id) =
            init_node_and_channel(TEST_NODE_CONFIG, TEST_SEED[1], setup.clone());

        let htlc1 = HTLCInfo2 {
            value_sat: 1,
            payment_hash: PaymentHash([1; 32]),
            cltv_expiry: 2 << 16,
        };

        let htlc2 = HTLCInfo2 {
            value_sat: 1,
            payment_hash: PaymentHash([3; 32]),
            cltv_expiry: 3 << 16,
        };

        let htlc3 = HTLCInfo2 {
            value_sat: 1,
            payment_hash: PaymentHash([5; 32]),
            cltv_expiry: 4 << 16,
        };
        let offered_htlcs = vec![htlc1];
        let received_htlcs = vec![htlc2, htlc3];
        (node, setup, channel_id, offered_htlcs, received_htlcs)
    }

    fn sign_counterparty_commitment_tx_with_mutators<StateMutator, KeysMutator, TxMutator>(
        statemut: StateMutator,
        keysmut: KeysMutator,
        txmut: TxMutator,
    ) -> Result<(), Status>
    where
        StateMutator: Fn(&mut EnforcementState),
        KeysMutator: Fn(&mut TxCreationKeys),
        TxMutator: Fn(&mut BuiltCommitmentTransaction),
    {
        let (node, setup, channel_id, offered_htlcs, received_htlcs) =
            sign_commitment_tx_with_mutators_setup();

        let remote_percommitment_point = make_test_pubkey(10);

        let (sig, tx) = node.with_ready_channel(&channel_id, |chan| {
            let channel_parameters = chan.make_channel_parameters();

            let commit_num = 23;
            let feerate_per_kw = 0;
            let to_broadcaster = 1_999_997;
            let to_countersignatory = 1_000_000;

            chan.enforcement_state
                .set_next_counterparty_commit_num_for_testing(commit_num, make_test_pubkey(0x10));
            chan.enforcement_state
                .set_next_counterparty_revoke_num_for_testing(commit_num - 1);

            // Mutate the signer state.
            statemut(&mut chan.enforcement_state);

            let parameters = channel_parameters.as_counterparty_broadcastable();
            let mut keys = chan.make_counterparty_tx_keys(&remote_percommitment_point)?;

            // Mutate the tx creation keys.
            keysmut(&mut keys);

            let htlcs = Channel::htlcs_info2_to_oic(offered_htlcs.clone(), received_htlcs.clone());

            let redeem_scripts = build_tx_scripts(
                &keys,
                to_countersignatory,
                to_broadcaster,
                &htlcs,
                &parameters,
            )
            .expect("scripts");
            let output_witscripts = redeem_scripts.iter().map(|s| s.serialize()).collect();

            let commitment_tx = chan.make_counterparty_commitment_tx_with_keys(
                keys,
                commit_num,
                feerate_per_kw,
                to_broadcaster,
                to_countersignatory,
                htlcs.clone(),
            );

            // rebuild to get the scripts
            let trusted_tx = commitment_tx.trust();
            let mut tx = trusted_tx.built_transaction().clone();

            // Mutate the transaction and recalculate the txid.
            txmut(&mut tx);
            tx.txid = tx.transaction.txid();

            let sig = chan.sign_counterparty_commitment_tx(
                &tx.transaction,
                &output_witscripts,
                &remote_percommitment_point,
                commit_num,
                feerate_per_kw,
                offered_htlcs.clone(),
                received_htlcs.clone(),
            )?;
            Ok((sig, tx.transaction.clone()))
        })?;

        assert_eq!(
            tx.txid().to_hex(),
            "3f3238ed033a13ab1cf43d8eb6e81e5beca2080f9530a13931c10f40e04697fb"
        );

        let funding_pubkey = get_channel_funding_pubkey(&node, &channel_id);
        let channel_funding_redeemscript =
            make_funding_redeemscript(&funding_pubkey, &setup.counterparty_points.funding_pubkey);

        check_signature(
            &tx,
            0,
            signature_to_bitcoin_vec(sig),
            &funding_pubkey,
            setup.channel_value_sat,
            &channel_funding_redeemscript,
        );

        Ok(())
    }

    fn sign_holder_commitment_tx_with_mutators<StateMutator, KeysMutator, TxMutator>(
        statemut: StateMutator,
        keysmut: KeysMutator,
        txmut: TxMutator,
    ) -> Result<(), Status>
    where
        StateMutator: Fn(&mut EnforcementState),
        KeysMutator: Fn(&mut TxCreationKeys),
        TxMutator: Fn(&mut BuiltCommitmentTransaction),
    {
        let (node, setup, channel_id, offered_htlcs, received_htlcs) =
            sign_commitment_tx_with_mutators_setup();

        let (sig, tx) = node.with_ready_channel(&channel_id, |chan| {
            let channel_parameters = chan.make_channel_parameters();

            let commit_num = 23;
            let feerate_per_kw = 0;
            let to_broadcaster = 1_999_997;
            let to_countersignatory = 1_000_000;

            chan.enforcement_state
                .set_next_holder_commit_num_for_testing(commit_num);

            // Mutate the signer state.
            statemut(&mut chan.enforcement_state);

            let parameters = channel_parameters.as_holder_broadcastable();

            let per_commitment_point = chan.get_per_commitment_point(commit_num).expect("point");
            let mut keys = chan.make_holder_tx_keys(&per_commitment_point)?;

            // Mutate the tx creation keys.
            keysmut(&mut keys);

            let htlcs = Channel::htlcs_info2_to_oic(offered_htlcs.clone(), received_htlcs.clone());

            let redeem_scripts = build_tx_scripts(
                &keys,
                to_broadcaster,
                to_countersignatory,
                &htlcs,
                &parameters,
            )
            .expect("scripts");
            let output_witscripts = redeem_scripts.iter().map(|s| s.serialize()).collect();

            let commitment_tx = chan.make_holder_commitment_tx_with_keys(
                keys,
                commit_num,
                feerate_per_kw,
                to_broadcaster,
                to_countersignatory,
                htlcs.clone(),
            );
            // rebuild to get the scripts
            let trusted_tx = commitment_tx.trust();
            let mut tx = trusted_tx.built_transaction().clone();

            // Mutate the transaction and recalculate the txid.
            txmut(&mut tx);
            tx.txid = tx.transaction.txid();

            let sig = chan.sign_holder_commitment_tx(
                &tx.transaction,
                &output_witscripts,
                commit_num,
                feerate_per_kw,
                offered_htlcs.clone(),
                received_htlcs.clone(),
            )?;
            Ok((sig, tx.transaction.clone()))
        })?;

        assert_eq!(
            tx.txid().to_hex(),
            "f438eac18af86e17f7dd74a8630e7427fefb2d81becb0ae563914a4e3e9aef9f"
        );

        let funding_pubkey = get_channel_funding_pubkey(&node, &channel_id);
        let channel_funding_redeemscript =
            make_funding_redeemscript(&funding_pubkey, &setup.counterparty_points.funding_pubkey);

        check_signature(
            &tx,
            0,
            signature_to_bitcoin_vec(sig),
            &funding_pubkey,
            setup.channel_value_sat,
            &channel_funding_redeemscript,
        );

        Ok(())
    }

    #[test]
    fn sign_counterparty_commitment_tx_with_no_mut_test() {
        let status = sign_counterparty_commitment_tx_with_mutators(
            |_state| {
                // don't mutate the signer, should pass
            },
            |_keys| {
                // don't mutate the keys, should pass
            },
            |_tx| {
                // don't mutate the tx, should pass
            },
        );
        assert!(status.is_ok());
    }

    #[test]
    fn sign_holder_commitment_tx_with_no_mut_test() {
        let status = sign_holder_commitment_tx_with_mutators(
            |_state| {
                // don't mutate the signer, should pass
            },
            |_keys| {
                // don't mutate the keys, should pass
            },
            |_tx| {
                // don't mutate the tx, should pass
            },
        );
        assert!(status.is_ok());
    }

    // policy-v1-commitment-version
    #[test]
    fn sign_counterparty_commitment_tx_with_bad_version_test() {
        let status = sign_counterparty_commitment_tx_with_mutators(
            |_state| {},
            |_keys| {},
            |tx| {
                tx.transaction.version = 3;
            },
        );
        assert_failed_precondition_err!(
            status,
            "policy failure: make_info: bad commitment version: 3"
        );
    }

    // policy-v1-commitment-version
    #[test]
    fn sign_holder_commitment_tx_with_bad_version_test() {
        let status = sign_holder_commitment_tx_with_mutators(
            |_state| {},
            |_keys| {},
            |tx| {
                tx.transaction.version = 3;
            },
        );
        assert_failed_precondition_err!(
            status,
            "policy failure: make_info: bad commitment version: 3"
        );
    }

    // policy-v1-commitment-locktime
    #[test]
    fn sign_counterparty_commitment_tx_with_bad_locktime_test() {
        let status = sign_counterparty_commitment_tx_with_mutators(
            |_state| {},
            |_keys| {
                // don't mutate the keys
            },
            |tx| {
                tx.transaction.lock_time = 42;
            },
        );
        assert_failed_precondition_err!(status, "policy failure: recomposed tx mismatch");
    }

    // policy-v1-commitment-locktime
    #[test]
    fn sign_holder_commitment_tx_with_bad_locktime_test() {
        let status = sign_holder_commitment_tx_with_mutators(
            |_state| {},
            |_keys| {
                // don't mutate the keys
            },
            |tx| {
                tx.transaction.lock_time = 42;
            },
        );
        assert_failed_precondition_err!(status, "policy failure: recomposed tx mismatch");
    }

    // policy-v1-commitment-sequence
    #[test]
    fn sign_counterparty_commitment_tx_with_bad_sequence_test() {
        let status = sign_counterparty_commitment_tx_with_mutators(
            |_state| {},
            |_keys| {},
            |tx| {
                tx.transaction.input[0].sequence = 42;
            },
        );
        assert_failed_precondition_err!(status, "policy failure: recomposed tx mismatch");
    }

    // policy-v1-commitment-sequence
    #[test]
    fn sign_holder_commitment_tx_with_bad_sequence_test() {
        let status = sign_holder_commitment_tx_with_mutators(
            |_state| {},
            |_keys| {},
            |tx| {
                tx.transaction.input[0].sequence = 42;
            },
        );
        assert_failed_precondition_err!(status, "policy failure: recomposed tx mismatch");
    }

    // policy-v1-commitment-input-single
    #[test]
    fn sign_counterparty_commitment_tx_with_bad_numinputs_test() {
        let status = sign_counterparty_commitment_tx_with_mutators(
            |_state| {},
            |_keys| {},
            |tx| {
                let mut inp2 = tx.transaction.input[0].clone();
                inp2.previous_output.txid = bitcoin::Txid::from_slice(&[3u8; 32]).unwrap();
                tx.transaction.input.push(inp2);
            },
        );
        assert_failed_precondition_err!(status, "policy failure: recomposed tx mismatch");
    }

    // policy-v1-commitment-input-single
    #[test]
    fn sign_holder_commitment_tx_with_bad_numinputs_test() {
        let status = sign_holder_commitment_tx_with_mutators(
            |_state| {},
            |_keys| {},
            |tx| {
                let mut inp2 = tx.transaction.input[0].clone();
                inp2.previous_output.txid = bitcoin::Txid::from_slice(&[3u8; 32]).unwrap();
                tx.transaction.input.push(inp2);
            },
        );
        assert_failed_precondition_err!(status, "policy failure: recomposed tx mismatch");
    }

    // policy-v1-commitment-input-match-funding
    #[test]
    fn sign_counterparty_commitment_tx_with_input_mismatch_test() {
        let status = sign_counterparty_commitment_tx_with_mutators(
            |_state| {},
            |_keys| {},
            |tx| {
                tx.transaction.input[0].previous_output.txid =
                    bitcoin::Txid::from_slice(&[3u8; 32]).unwrap();
            },
        );
        assert_failed_precondition_err!(status, "policy failure: recomposed tx mismatch");
    }

    // policy-v1-commitment-input-match-funding
    #[test]
    fn sign_holder_commitment_tx_with_input_mismatch_test() {
        let status = sign_holder_commitment_tx_with_mutators(
            |_state| {},
            |_keys| {},
            |tx| {
                tx.transaction.input[0].previous_output.txid =
                    bitcoin::Txid::from_slice(&[3u8; 32]).unwrap();
            },
        );
        assert_failed_precondition_err!(status, "policy failure: recomposed tx mismatch");
    }

    // policy-v1-commitment-revocation-pubkey
    #[test]
    fn sign_counterparty_commitment_tx_with_bad_revpubkey_test() {
        let status = sign_counterparty_commitment_tx_with_mutators(
            |_state| {},
            |keys| {
                keys.revocation_key = make_test_pubkey(42);
            },
            |_tx| {},
        );
        assert_failed_precondition_err!(status, "policy failure: recomposed tx mismatch");
    }

    // policy-v1-commitment-revocation-pubkey
    #[test]
    fn sign_holder_commitment_tx_with_bad_revpubkey_test() {
        let status = sign_holder_commitment_tx_with_mutators(
            |_state| {},
            |keys| {
                keys.revocation_key = make_test_pubkey(42);
            },
            |_tx| {},
        );
        assert_failed_precondition_err!(status, "policy failure: recomposed tx mismatch");
    }

    // policy-v1-commitment-htlc-pubkey
    #[test]
    fn sign_counterparty_commitment_tx_with_bad_htlcpubkey_test() {
        let status = sign_counterparty_commitment_tx_with_mutators(
            |_state| {},
            |keys| {
                keys.countersignatory_htlc_key = make_test_pubkey(42);
            },
            |_tx| {},
        );
        assert_failed_precondition_err!(status, "policy failure: recomposed tx mismatch");
    }

    // policy-v1-commitment-htlc-pubkey
    #[test]
    fn sign_holder_commitment_tx_with_bad_htlcpubkey_test() {
        let status = sign_holder_commitment_tx_with_mutators(
            |_state| {},
            |keys| {
                keys.countersignatory_htlc_key = make_test_pubkey(42);
            },
            |_tx| {},
        );
        assert_failed_precondition_err!(status, "policy failure: recomposed tx mismatch");
    }

    // policy-v1-commitment-delayed-pubkey
    #[test]
    fn sign_counterparty_commitment_tx_with_bad_delayed_pubkey_test() {
        let status = sign_counterparty_commitment_tx_with_mutators(
            |_state| {},
            |keys| {
                keys.broadcaster_delayed_payment_key = make_test_pubkey(42);
            },
            |_tx| {},
        );
        assert_failed_precondition_err!(status, "policy failure: recomposed tx mismatch");
    }

    // policy-v1-commitment-delayed-pubkey
    #[test]
    fn sign_holder_commitment_tx_with_bad_delayed_pubkey_test() {
        let status = sign_holder_commitment_tx_with_mutators(
            |_state| {},
            |keys| {
                keys.broadcaster_delayed_payment_key = make_test_pubkey(42);
            },
            |_tx| {},
        );
        assert_failed_precondition_err!(status, "policy failure: recomposed tx mismatch");
    }

    // policy-v2-commitment-previous-revoked
    #[test]
    fn sign_counterparty_commitment_tx_with_unrevoked_prior() {
        let status = sign_counterparty_commitment_tx_with_mutators(
            |state| {
                state.set_next_counterparty_revoke_num_for_testing(21);
            },
            |_keys| {},
            |_tx| {},
        );
        assert_failed_precondition_err!(
            status,
            "policy failure: validate_commitment_tx: invalid attempt \
             to sign counterparty commit_num 23 \
             with next_counterparty_revoke_num 21"
        );
    }

    #[test]
    fn sign_counterparty_commitment_tx_with_old_commit_num() {
        let status = sign_counterparty_commitment_tx_with_mutators(
            |state| {
                // Advance both commit_num and revoke_num:
                state.set_next_counterparty_commit_num_for_testing(25, make_test_pubkey(0x10));
                state.set_next_counterparty_revoke_num_for_testing(24);
            },
            |_keys| {},
            |_tx| {},
        );
        assert_failed_precondition_err!(
            status,
            "policy failure: set_next_counterparty_commit_num: \
             24 too small relative to next_counterparty_revoke_num 24"
        );
    }

    #[test]
    fn sign_holder_commitment_tx_after_mutual_close() {
        let status = sign_holder_commitment_tx_with_mutators(
            |state| state.mutual_close_signed = true,
            |_keys| {},
            |_tx| {},
        );
        assert!(status.is_ok());
    }

    fn sign_counterparty_commitment_tx_retry_with_mutator<SignCommitmentMutator>(
        sign_comm_mut: SignCommitmentMutator,
    ) -> Result<(), Status>
    where
        SignCommitmentMutator: Fn(
            &mut bitcoin::Transaction,
            &mut Vec<Vec<u8>>,
            &mut PublicKey,
            &mut u32,
            &mut Vec<HTLCInfo2>,
            &mut Vec<HTLCInfo2>,
        ),
    {
        let (node, _setup, channel_id, offered_htlcs0, received_htlcs0) =
            sign_commitment_tx_with_mutators_setup();

        node.with_ready_channel(&channel_id, |chan| {
            let mut offered_htlcs = offered_htlcs0.clone();
            let mut received_htlcs = received_htlcs0.clone();
            let channel_parameters = chan.make_channel_parameters();

            let mut remote_percommitment_point = make_test_pubkey(10);

            let commit_num = 23;
            let mut feerate_per_kw = 0;
            let to_broadcaster = 1_999_997;
            let to_countersignatory = 1_000_000;
            let htlcs = Channel::htlcs_info2_to_oic(offered_htlcs.clone(), received_htlcs.clone());

            chan.enforcement_state
                .set_next_counterparty_commit_num_for_testing(commit_num, make_test_pubkey(0x10));
            chan.enforcement_state
                .set_next_counterparty_revoke_num_for_testing(commit_num - 1);

            let parameters = channel_parameters.as_counterparty_broadcastable();
            let keys = chan.make_counterparty_tx_keys(&remote_percommitment_point)?;

            let redeem_scripts = build_tx_scripts(
                &keys,
                to_countersignatory,
                to_broadcaster,
                &htlcs,
                &parameters,
            )
            .expect("scripts");
            let mut output_witscripts = redeem_scripts.iter().map(|s| s.serialize()).collect();

            let commitment_tx = chan.make_counterparty_commitment_tx_with_keys(
                keys,
                commit_num,
                feerate_per_kw,
                to_broadcaster,
                to_countersignatory,
                htlcs.clone(),
            );

            // rebuild to get the scripts
            let trusted_tx = commitment_tx.trust();
            let mut tx = trusted_tx.built_transaction().clone();

            // Sign the commitment the first time.
            let _sig = chan.sign_counterparty_commitment_tx(
                &tx.transaction,
                &output_witscripts,
                &remote_percommitment_point,
                commit_num,
                feerate_per_kw,
                offered_htlcs.clone(),
                received_htlcs.clone(),
            )?;

            // Mutate the arguments to the commitment.
            sign_comm_mut(
                &mut tx.transaction,
                &mut output_witscripts,
                &mut remote_percommitment_point,
                &mut feerate_per_kw,
                &mut offered_htlcs,
                &mut received_htlcs,
            );

            // Sign it again (retry).
            let _sig = chan.sign_counterparty_commitment_tx(
                &tx.transaction,
                &output_witscripts,
                &remote_percommitment_point,
                commit_num,
                feerate_per_kw,
                offered_htlcs,
                received_htlcs,
            )?;

            Ok(())
        })
    }

    #[test]
    fn sign_counterparty_commitment_tx_retry_same() {
        assert!(sign_counterparty_commitment_tx_retry_with_mutator(
            |_tx,
             _output_witscripts,
             _remote_percommitment_point,
             _feerate_per_kw,
             _offered_htlcs,
             _received_htlcs| {
                // If we don't mutate anything it should succeed.
            }
        )
        .is_ok());
    }

    // policy-v2-commitment-retry-same (remote_percommitment_point)
    #[test]
    fn sign_counterparty_commitment_tx_retry_with_bad_point() {
        assert_failed_precondition_err!(
            sign_counterparty_commitment_tx_retry_with_mutator(
                |_tx,
                 _output_witscripts,
                 remote_percommitment_point,
                 _feerate_per_kw,
                 _offered_htlcs,
                 _received_htlcs| {
                    *remote_percommitment_point = make_test_pubkey(42);
                }
            ),
            "policy failure: validate_commitment_tx: \
             retry of sign_counterparty_commitment 23 with changed point: \
             prev 03f76a39d05686e34a4420897e359371836145dd3973e3982568b60f8433adde6e != \
             new 035be5e9478209674a96e60f1f037f6176540fd001fa1d64694770c56a7709c42c"
        );
    }

    // TODO - policy-v2-commitment-retry-same (tx)
    // TODO - policy-v2-commitment-retry-same (output_witscripts)
    // TODO - policy-v2-commitment-retry-same (payment_hashmap)

    const REV_COMMIT_NUM: u64 = 23;

    fn validate_counterparty_revocation_with_mutator<RevocationMutator, ChannelStateValidator>(
        mutate_revocation_input: RevocationMutator,
        validate_channel_state: ChannelStateValidator,
    ) -> Result<(), Status>
    where
        RevocationMutator: Fn(&mut Channel, &mut SecretKey),
        ChannelStateValidator: Fn(&Channel),
    {
        let (node, _setup, channel_id, offered_htlcs, received_htlcs) =
            sign_commitment_tx_with_mutators_setup();

        node.with_ready_channel(&channel_id, |chan| {
            let channel_parameters = chan.make_channel_parameters();

            let remote_percommit_point = make_test_pubkey(10);
            let mut remote_percommit_secret = make_test_privkey(10);

            let feerate_per_kw = 0;
            let to_broadcaster = 1_999_997;
            let to_countersignatory = 1_000_000;

            chan.enforcement_state
                .set_next_counterparty_revoke_num_for_testing(REV_COMMIT_NUM - 1);
            chan.enforcement_state
                .set_next_counterparty_commit_num_for_testing(
                    REV_COMMIT_NUM,
                    make_test_pubkey(0x10),
                );

            // commit 21: revoked
            // commit 22: current  <- next revoke
            // commit 23: next     <- next commit

            let parameters = channel_parameters.as_counterparty_broadcastable();
            let keys = chan.make_counterparty_tx_keys(&remote_percommit_point)?;
            let htlcs = Channel::htlcs_info2_to_oic(offered_htlcs.clone(), received_htlcs.clone());

            let redeem_scripts = build_tx_scripts(
                &keys,
                to_countersignatory,
                to_broadcaster,
                &htlcs,
                &parameters,
            )
            .expect("scripts");
            let output_witscripts = redeem_scripts.iter().map(|s| s.serialize()).collect();

            let commitment_tx = chan.make_counterparty_commitment_tx_with_keys(
                keys,
                REV_COMMIT_NUM,
                feerate_per_kw,
                to_broadcaster,
                to_countersignatory,
                htlcs.clone(),
            );

            let trusted_tx = commitment_tx.trust();
            let tx = trusted_tx.built_transaction().clone();

            let _sig = chan.sign_counterparty_commitment_tx(
                &tx.transaction,
                &output_witscripts,
                &remote_percommit_point,
                REV_COMMIT_NUM,
                feerate_per_kw,
                offered_htlcs.clone(),
                received_htlcs.clone(),
            )?;

            // commit 21: revoked
            // commit 22: unrevoked <- next revoke
            // commit 23: current
            // commit 24: next      <- next commit

            // Advance the state one full cycle:
            // - validate_counterparty_revocation(22, ..)
            // - sign_counterparty_commitment_tx(.., 24)
            chan.set_next_counterparty_revoke_num_for_testing(REV_COMMIT_NUM);
            chan.set_next_counterparty_commit_num_for_testing(
                REV_COMMIT_NUM + 2,
                make_test_pubkey(0x10),
            );

            // commit 23: unrevoked <- next revoke
            // commit 24: current
            // commit 25: next      <- next commit

            // Let unit tests mess with stuff.
            mutate_revocation_input(chan, &mut remote_percommit_secret);

            // Validate the revocation, but defer error returns till after we've had
            // a chance to validate the channel state for side-effects
            let deferred_rv =
                chan.validate_counterparty_revocation(REV_COMMIT_NUM, &remote_percommit_secret);

            // commit 23: revoked
            // commit 24: current   <- next revoke
            // commit 25: next      <- next commit

            // Make sure the revocation state is as expected for each test.
            validate_channel_state(chan);
            deferred_rv?;

            assert_eq!(
                tx.txid.to_hex(),
                "3f3238ed033a13ab1cf43d8eb6e81e5beca2080f9530a13931c10f40e04697fb"
            );

            Ok(())
        })
    }

    #[test]
    fn validate_counterparty_revocation_success() {
        assert!(validate_counterparty_revocation_with_mutator(
            |_chan, _old_secret| {
                // If we don't mutate anything it should succeed.
            },
            |chan| {
                // Channel state should advance.
                assert_eq!(
                    chan.enforcement_state.next_counterparty_revoke_num,
                    REV_COMMIT_NUM + 1
                );
            }
        )
        .is_ok());
    }

    #[test]
    fn validate_counterparty_revocation_can_retry() {
        assert!(validate_counterparty_revocation_with_mutator(
            |chan, _old_secret| {
                // Set the channel's next_revoke_num ahead one;
                // pretend we already revoked it.
                chan.enforcement_state
                    .set_next_counterparty_revoke_num_for_testing(REV_COMMIT_NUM + 1);
            },
            |chan| {
                // Channel state should stay where we advanced it..
                assert_eq!(
                    chan.enforcement_state.next_counterparty_revoke_num,
                    REV_COMMIT_NUM + 1
                );
            }
        )
        .is_ok());
    }

    #[test]
    fn validate_counterparty_revocation_not_ahead() {
        assert_failed_precondition_err!(
            validate_counterparty_revocation_with_mutator(
                |chan, _old_secret| {
                    // Set the channel's next_revoke_num ahead two, past the retry ...
                    chan.enforcement_state
                        .set_next_counterparty_revoke_num_for_testing(REV_COMMIT_NUM + 2);
                },
                |chan| {
                    // Channel state should stay where we advanced it..
                    assert_eq!(
                        chan.enforcement_state.next_counterparty_revoke_num,
                        REV_COMMIT_NUM + 2
                    );
                }
            ),
            "policy failure: validate_counterparty_revocation: \
             invalid counterparty revoke_num 23 with next_counterparty_revoke_num 25"
        );
    }

    #[test]
    fn validate_counterparty_revocation_not_behind() {
        assert_failed_precondition_err!(
            validate_counterparty_revocation_with_mutator(
                |chan, _old_secret| {
                    // Set the channel's next_revoke_num behind 1, in the past ...
                    chan.enforcement_state
                        .set_next_counterparty_revoke_num_for_testing(REV_COMMIT_NUM - 1);
                },
                |chan| {
                    // Channel state should stay where we set it..
                    assert_eq!(
                        chan.enforcement_state.next_counterparty_revoke_num,
                        REV_COMMIT_NUM - 1
                    );
                }
            ),
            "policy failure: validate_counterparty_revocation: \
             invalid counterparty revoke_num 23 with next_counterparty_revoke_num 22"
        );
    }

    // policy-v2-commitment-previous-revoked (invalid secret on revoke)
    #[test]
    fn validate_counterparty_revocation_with_bad_secret() {
        assert_failed_precondition_err!(
            validate_counterparty_revocation_with_mutator(
                |_chan, old_secret| {
                    *old_secret = make_test_privkey(42);
                },
                |chan| {
                    // Channel state should NOT advance.
                    assert_eq!(
                        chan.enforcement_state.next_counterparty_revoke_num,
                        REV_COMMIT_NUM
                    );
                }
            ),
            "policy failure: validate_counterparty_revocation: \
             revocation commit point mismatch for commit_num 23: \
             supplied 035be5e9478209674a96e60f1f037f6176540fd001fa1d64694770c56a7709c42c, \
             previous 03f76a39d05686e34a4420897e359371836145dd3973e3982568b60f8433adde6e"
        );
    }

    #[test]
    fn validate_counterparty_revocation_with_retry() {
        let (node, _setup, channel_id, offered_htlcs, received_htlcs) =
            sign_commitment_tx_with_mutators_setup();

        // Setup enforcement state
        assert_status_ok!(node.with_ready_channel(&channel_id, |chan| {
            chan.enforcement_state
                .set_next_counterparty_revoke_num_for_testing(REV_COMMIT_NUM - 1);
            chan.enforcement_state
                .set_next_counterparty_commit_num_for_testing(
                    REV_COMMIT_NUM,
                    make_test_pubkey((REV_COMMIT_NUM - 1) as u8),
                );
            // commit 21: revoked
            // commit 22: current  <- next revoke
            // commit 23: next     <- next commit
            Ok(())
        }));

        // Sign counterparty REV_COMMIT_NUM
        assert_status_ok!(node.with_ready_channel(&channel_id, |chan| {
            let channel_parameters = chan.make_channel_parameters();

            let remote_percommit_point = make_test_pubkey(REV_COMMIT_NUM as u8);

            let feerate_per_kw = 0;
            let to_broadcaster = 1_999_997;
            let to_countersignatory = 1_000_000;

            let parameters = channel_parameters.as_counterparty_broadcastable();
            let keys = chan.make_counterparty_tx_keys(&remote_percommit_point)?;
            let htlcs = Channel::htlcs_info2_to_oic(offered_htlcs.clone(), received_htlcs.clone());

            let redeem_scripts = build_tx_scripts(
                &keys,
                to_countersignatory,
                to_broadcaster,
                &htlcs,
                &parameters,
            )
            .expect("scripts");
            let output_witscripts = redeem_scripts.iter().map(|s| s.serialize()).collect();

            let commitment_tx = chan.make_counterparty_commitment_tx_with_keys(
                keys,
                REV_COMMIT_NUM,
                feerate_per_kw,
                to_broadcaster,
                to_countersignatory,
                htlcs.clone(),
            );

            let trusted_tx = commitment_tx.trust();
            let tx = trusted_tx.built_transaction().clone();

            let _sig = chan.sign_counterparty_commitment_tx(
                &tx.transaction,
                &output_witscripts,
                &remote_percommit_point,
                REV_COMMIT_NUM,
                feerate_per_kw,
                offered_htlcs.clone(),
                received_htlcs.clone(),
            )?;

            // commit 21: revoked
            // commit 22: unrevoked <- next revoke
            // commit 23: current
            // commit 24: next      <- next commit
            Ok(())
        }));

        // Revoke REV_COMMIT_NUM - 1
        assert_status_ok!(node.with_ready_channel(&channel_id, |chan| {
            assert_status_ok!(chan.validate_counterparty_revocation(
                REV_COMMIT_NUM - 1,
                &make_test_privkey((REV_COMMIT_NUM - 1) as u8)
            ));

            // commit 22: revoked
            // commit 23: current   <- next revoke
            // commit 24: next      <- next commit
            Ok(())
        }));

        // Sign counterparty REV_COMMIT_NUM + 1
        assert_status_ok!(node.with_ready_channel(&channel_id, |chan| {
            let channel_parameters = chan.make_channel_parameters();

            let remote_percommit_point = make_test_pubkey((REV_COMMIT_NUM + 1) as u8);

            let feerate_per_kw = 0;
            let to_broadcaster = 1_999_097; // -900
            let to_countersignatory = 1_000_900; // +900

            let parameters = channel_parameters.as_counterparty_broadcastable();
            let keys = chan.make_counterparty_tx_keys(&remote_percommit_point)?;
            let htlcs = Channel::htlcs_info2_to_oic(offered_htlcs.clone(), received_htlcs.clone());

            let redeem_scripts = build_tx_scripts(
                &keys,
                to_countersignatory,
                to_broadcaster,
                &htlcs,
                &parameters,
            )
            .expect("scripts");
            let output_witscripts = redeem_scripts.iter().map(|s| s.serialize()).collect();

            let commitment_tx = chan.make_counterparty_commitment_tx_with_keys(
                keys,
                REV_COMMIT_NUM + 1,
                feerate_per_kw,
                to_broadcaster,
                to_countersignatory,
                htlcs.clone(),
            );

            let trusted_tx = commitment_tx.trust();
            let tx = trusted_tx.built_transaction().clone();

            let _sig = chan.sign_counterparty_commitment_tx(
                &tx.transaction,
                &output_witscripts,
                &remote_percommit_point,
                REV_COMMIT_NUM + 1,
                feerate_per_kw,
                offered_htlcs.clone(),
                received_htlcs.clone(),
            )?;

            // commit 22: revoked
            // commit 23: unrevoked <- next revoke
            // commit 24: current
            // commit 25: next      <- next commit
            Ok(())
        }));

        // Revoke REV_COMMIT_NUM with lots of checking
        assert_status_ok!(node.with_ready_channel(&channel_id, |chan| {
            // state is what we think it is
            assert_eq!(
                chan.enforcement_state.next_counterparty_revoke_num,
                REV_COMMIT_NUM
            );
            assert!(chan
                .enforcement_state
                .previous_counterparty_commit_info
                .is_some());

            // Can't assert older
            assert_failed_precondition_err!(
                chan.validate_counterparty_revocation(
                    REV_COMMIT_NUM - 2,
                    &make_test_privkey((REV_COMMIT_NUM - 2) as u8)
                ),
                "policy failure: validate_counterparty_revocation: \
                 invalid counterparty revoke_num 21 with next_counterparty_revoke_num 23"
            );

            // state is unchanged
            assert_eq!(
                chan.enforcement_state.next_counterparty_revoke_num,
                REV_COMMIT_NUM
            );
            assert!(chan
                .enforcement_state
                .previous_counterparty_commit_info
                .is_some());

            // Can't skip
            assert_failed_precondition_err!(
                chan.validate_counterparty_revocation(
                    REV_COMMIT_NUM + 1,
                    &make_test_privkey((REV_COMMIT_NUM + 1) as u8)
                ),
                "policy failure: validate_counterparty_revocation: \
                 invalid counterparty revoke_num 24 with next_counterparty_revoke_num 23"
            );

            // state is unchanged
            assert_eq!(
                chan.enforcement_state.next_counterparty_revoke_num,
                REV_COMMIT_NUM
            );
            assert!(chan
                .enforcement_state
                .previous_counterparty_commit_info
                .is_some());

            // can revoke correctly
            assert_status_ok!(chan.validate_counterparty_revocation(
                REV_COMMIT_NUM,
                &make_test_privkey(REV_COMMIT_NUM as u8)
            ));

            // state is modified
            assert_eq!(
                chan.enforcement_state.next_counterparty_revoke_num,
                REV_COMMIT_NUM + 1
            );
            assert!(chan
                .enforcement_state
                .previous_counterparty_commit_info
                .is_none());

            // Retry is ok
            assert_status_ok!(chan.validate_counterparty_revocation(
                REV_COMMIT_NUM,
                &make_test_privkey(REV_COMMIT_NUM as u8)
            ));

            // state is unchanged
            assert_eq!(
                chan.enforcement_state.next_counterparty_revoke_num,
                REV_COMMIT_NUM + 1
            );
            assert!(chan
                .enforcement_state
                .previous_counterparty_commit_info
                .is_none());

            // Can't assert older
            assert_failed_precondition_err!(
                chan.validate_counterparty_revocation(
                    REV_COMMIT_NUM - 1,
                    &make_test_privkey((REV_COMMIT_NUM - 1) as u8)
                ),
                "policy failure: validate_counterparty_revocation: \
                 invalid counterparty revoke_num 22 with next_counterparty_revoke_num 24"
            );

            // state is unchanged
            assert_eq!(
                chan.enforcement_state.next_counterparty_revoke_num,
                REV_COMMIT_NUM + 1
            );
            assert!(chan
                .enforcement_state
                .previous_counterparty_commit_info
                .is_none());

            // Can't skip
            assert_failed_precondition_err!(
                chan.validate_counterparty_revocation(
                    REV_COMMIT_NUM + 2,
                    &make_test_privkey((REV_COMMIT_NUM + 2) as u8)
                ),
                "policy failure: validate_counterparty_revocation: \
                 invalid counterparty revoke_num 25 with next_counterparty_revoke_num 24"
            );

            // state is unchanged
            assert_eq!(
                chan.enforcement_state.next_counterparty_revoke_num,
                REV_COMMIT_NUM + 1
            );
            assert!(chan
                .enforcement_state
                .previous_counterparty_commit_info
                .is_none());

            Ok(())
        }))
    }

    const HOLD_COMMIT_NUM: u64 = 43;

    fn validate_holder_commitment_with_mutator<ValidationMutator, ChannelStateValidator>(
        mutate_validation_input: ValidationMutator,
        validate_channel_state: ChannelStateValidator,
    ) -> Result<(), Status>
    where
        ValidationMutator:
            Fn(&mut Channel, &mut TestCommitmentTxContext, &mut Signature, &mut Vec<Signature>),
        ChannelStateValidator: Fn(&Channel),
    {
        let node_ctx = test_node_ctx(1);

        let channel_amount = 3_000_000;

        let mut chan_ctx = test_chan_ctx(&node_ctx, 1, channel_amount);

        // Pretend we funded the channel and ran for a while ...
        synthesize_ready_channel(
            &node_ctx,
            &mut chan_ctx,
            bitcoin::OutPoint {
                txid: Txid::from_slice(&[2u8; 32]).unwrap(),
                vout: 0,
            },
            HOLD_COMMIT_NUM,
        );

        let fee = 1000;
        let to_broadcaster = 1_000_000;
        let to_countersignatory = channel_amount - to_broadcaster - fee;
        let offered_htlcs = vec![];
        let received_htlcs = vec![];

        let feerate_per_kw = 1200;

        let mut commit_tx_ctx0 = channel_commitment(
            &node_ctx,
            &chan_ctx,
            HOLD_COMMIT_NUM,
            feerate_per_kw,
            to_broadcaster,
            to_countersignatory,
            offered_htlcs.clone(),
            received_htlcs.clone(),
        );

        let (commit_sig0, htlc_sigs0) =
            counterparty_sign_holder_commitment(&node_ctx, &chan_ctx, &mut commit_tx_ctx0);

        node_ctx
            .node
            .with_ready_channel(&chan_ctx.channel_id, |chan| {
                let mut commit_tx_ctx = commit_tx_ctx0.clone();
                let mut commit_sig = commit_sig0.clone();
                let mut htlc_sigs = htlc_sigs0.clone();

                let htlcs = Channel::htlcs_info2_to_oic(
                    commit_tx_ctx.offered_htlcs.clone(),
                    commit_tx_ctx.received_htlcs.clone(),
                );
                let channel_parameters = chan.make_channel_parameters();
                let parameters = channel_parameters.as_holder_broadcastable();
                let per_commitment_point =
                    chan.get_per_commitment_point(commit_tx_ctx.commit_num)?;
                let keys = chan.make_holder_tx_keys(&per_commitment_point).unwrap();
                let redeem_scripts = build_tx_scripts(
                    &keys,
                    commit_tx_ctx.to_broadcaster,
                    commit_tx_ctx.to_countersignatory,
                    &htlcs,
                    &parameters,
                )
                .expect("scripts");
                let output_witscripts = redeem_scripts.iter().map(|s| s.serialize()).collect();

                mutate_validation_input(chan, &mut commit_tx_ctx, &mut commit_sig, &mut htlc_sigs);

                // Validate the holder_commitment, but defer error returns till after we've had
                // a chance to validate the channel state for side-effects
                let deferred_rv = chan.validate_holder_commitment_tx(
                    &commit_tx_ctx
                        .tx
                        .as_ref()
                        .unwrap()
                        .trust()
                        .built_transaction()
                        .transaction,
                    &output_witscripts,
                    commit_tx_ctx.commit_num,
                    commit_tx_ctx.feerate_per_kw,
                    commit_tx_ctx.offered_htlcs.clone(),
                    commit_tx_ctx.received_htlcs.clone(),
                    &commit_sig,
                    &htlc_sigs,
                );
                validate_channel_state(chan);
                deferred_rv?;
                Ok(())
            })
    }

    #[test]
    fn validate_holder_commitment_success() {
        assert!(validate_holder_commitment_with_mutator(
            |_chan, _commit_tx_ctx, _commit_sig, _htlc_sigs| {
                // If we don't mutate anything it should succeed.
            },
            |chan| {
                // Channel state should advance.
                assert_eq!(
                    chan.enforcement_state.next_holder_commit_num,
                    HOLD_COMMIT_NUM + 1
                );
            }
        )
        .is_ok());
    }

    #[test]
    fn validate_holder_commitment_can_retry() {
        assert!(validate_holder_commitment_with_mutator(
            |chan, _commit_tx_ctx, _commit_sig, _htlc_sigs| {
                // Set the channel's next_holder_commit_num ahead one;
                // pretend we've already seen it ...
                chan.enforcement_state
                    .set_next_holder_commit_num_for_testing(HOLD_COMMIT_NUM + 1);
            },
            |chan| {
                // Channel state should stay where we advanced it.
                assert_eq!(
                    chan.enforcement_state.next_holder_commit_num,
                    HOLD_COMMIT_NUM + 1
                );
            }
        )
        .is_ok());
    }

    #[test]
    fn validate_holder_commitment_not_ahead() {
        assert_failed_precondition_err!(
            validate_holder_commitment_with_mutator(
                |chan, _commit_tx_ctx, _commit_sig, _htlc_sigs| {
                    // Set the channel's next_holder_commit_num ahead two, past the retry ...
                    chan.enforcement_state
                        .set_next_holder_commit_num_for_testing(HOLD_COMMIT_NUM + 2);
                },
                |chan| {
                    // Channel state should stay where we advanced it.
                    assert_eq!(
                        chan.enforcement_state.next_holder_commit_num,
                        HOLD_COMMIT_NUM + 2
                    );
                }
            ),
            "policy failure: set_next_holder_commit_num: invalid progression: 45 to 44"
        );
    }

    #[test]
    fn validate_holder_commitment_not_behind() {
        assert_failed_precondition_err!(
            validate_holder_commitment_with_mutator(
                |chan, _commit_tx_ctx, _commit_sig, _htlc_sigs| {
                    // Set the channel's next_holder_commit_num ahead two behind 1, in the past ...
                    chan.enforcement_state
                        .set_next_holder_commit_num_for_testing(HOLD_COMMIT_NUM - 1);
                },
                |chan| {
                    // Channel state should stay where we set it.
                    assert_eq!(
                        chan.enforcement_state.next_holder_commit_num,
                        HOLD_COMMIT_NUM - 1
                    );
                }
            ),
            "policy failure: get_per_commitment_point: \
             commitment_number 43 invalid when next_holder_commit_num is 42"
        );
    }

    // policy-v2-revoke-not-closed
    #[test]
    fn validate_holder_commitment_not_closed() {
        assert_failed_precondition_err!(
            validate_holder_commitment_with_mutator(
                |chan, _commit_tx_ctx, _commit_sig, _htlc_sigs| {
                    chan.enforcement_state.mutual_close_signed = true;
                },
                |chan| {
                    // Channel state should not advance.
                    assert_eq!(
                        chan.enforcement_state.next_holder_commit_num,
                        HOLD_COMMIT_NUM
                    );
                }
            ),
            "policy failure: validate_holder_commitment_state: mutual close already signed"
        );
    }

    #[test]
    fn channel_state_counterparty_commit_and_revoke_test() {
        let node_ctx = test_node_ctx(1);
        let mut chan_ctx = test_chan_ctx(&node_ctx, 1, 3_000_000);
        synthesize_ready_channel(
            &node_ctx,
            &mut chan_ctx,
            bitcoin::OutPoint {
                txid: Txid::from_slice(&[2u8; 32]).unwrap(),
                vout: 0,
            },
            HOLD_COMMIT_NUM,
        );
        node_ctx
            .node
            .with_ready_channel(&chan_ctx.channel_id, |chan| {
                let state = &mut chan.enforcement_state;

                // We'll need a placeholder; actual values not checked here ...
                let commit_info = make_test_commitment_info();

                // confirm initial state
                assert_eq!(state.next_counterparty_revoke_num, 0);
                assert_eq!(state.next_counterparty_commit_num, 0);
                // commit 0: unitialized <- next_revoke, <- next_commit

                // can't set next_commit to 0 (what would current point be?)
                assert_policy_err!(
                    state.set_next_counterparty_commit_num(
                        0,
                        make_test_pubkey(0x08),
                        commit_info.clone()
                    ),
                    "set_next_counterparty_commit_num: can\'t set next to 0"
                );
                assert_eq!(state.next_counterparty_commit_num, 0);

                // can't set next_revoke to 0 either
                assert_policy_err!(
                    state.set_next_counterparty_revoke_num(0),
                    "set_next_counterparty_revoke_num: can\'t set next to 0"
                );
                assert_eq!(state.next_counterparty_revoke_num, 0);

                // ADVANCE next_commit to 1
                assert!(state
                    .set_next_counterparty_commit_num(
                        1,
                        make_test_pubkey(0x10),
                        commit_info.clone()
                    )
                    .is_ok());
                assert_eq!(state.next_counterparty_revoke_num, 0);
                assert_eq!(state.next_counterparty_commit_num, 1);
                // commit 0: current <- next_revoke
                // commit 1: next    <- next_commit

                // retries are ok
                assert!(state
                    .set_next_counterparty_commit_num(
                        1,
                        make_test_pubkey(0x10),
                        commit_info.clone()
                    )
                    .is_ok());
                assert_eq!(state.next_counterparty_revoke_num, 0);
                assert_eq!(state.next_counterparty_commit_num, 1);

                // can't skip next_commit forward
                assert_policy_err!(
                    state.set_next_counterparty_commit_num(
                        3,
                        make_test_pubkey(0x14),
                        commit_info.clone()
                    ),
                    "set_next_counterparty_commit_num: 3 too large \
                     relative to next_counterparty_revoke_num 0"
                );
                assert_eq!(state.next_counterparty_commit_num, 1);

                // can't skip next_revoke forward
                assert_policy_err!(
                    state.set_next_counterparty_revoke_num(1),
                    "set_next_counterparty_revoke_num: \
                     1 too large relative to next_counterparty_commit_num 1"
                );
                assert_eq!(state.next_counterparty_revoke_num, 0);

                // ADVANCE next_commit to 2
                assert!(state
                    .set_next_counterparty_commit_num(
                        2,
                        make_test_pubkey(0x12),
                        commit_info.clone()
                    )
                    .is_ok());
                assert_eq!(state.next_counterparty_revoke_num, 0);
                assert_eq!(state.next_counterparty_commit_num, 2);
                // commit 0: unrevoked <- next_revoke
                // commit 1: current
                // commit 2: next    <- next_commit

                // retries are ok
                assert!(state
                    .set_next_counterparty_commit_num(
                        2,
                        make_test_pubkey(0x12),
                        commit_info.clone()
                    )
                    .is_ok());
                assert_eq!(state.next_counterparty_revoke_num, 0);
                assert_eq!(state.next_counterparty_commit_num, 2);

                // can't commit old thing
                assert_policy_err!(
                    state.set_next_counterparty_commit_num(
                        1,
                        make_test_pubkey(0x10),
                        commit_info.clone()
                    ),
                    "set_next_counterparty_commit_num: invalid progression: 2 to 1"
                );
                assert_eq!(state.next_counterparty_commit_num, 2);

                // can't advance commit again
                assert_policy_err!(
                    state.set_next_counterparty_commit_num(
                        3,
                        make_test_pubkey(0x14),
                        commit_info.clone()
                    ),
                    "set_next_counterparty_commit_num: 3 too large \
                     relative to next_counterparty_revoke_num 0"
                );
                assert_eq!(state.next_counterparty_commit_num, 2);

                // can't (ever) set next_revoke to 0
                assert_policy_err!(
                    state.set_next_counterparty_revoke_num(0),
                    "set_next_counterparty_revoke_num: can\'t set next to 0"
                );
                assert_eq!(state.next_counterparty_revoke_num, 0);

                // can't skip revoke ahead
                assert_policy_err!(
                    state.set_next_counterparty_revoke_num(2),
                    "set_next_counterparty_revoke_num: 2 too large relative to \
                     next_counterparty_commit_num 2"
                );
                assert_eq!(state.next_counterparty_revoke_num, 0);

                // REVOKE commit 0
                assert!(state.set_next_counterparty_revoke_num(1).is_ok());
                assert_eq!(state.next_counterparty_revoke_num, 1);
                assert_eq!(state.next_counterparty_commit_num, 2);
                // commit 0: revoked
                // commit 1: current   <- next_revoke
                // commit 2: next      <- next_commit

                // retries are ok
                assert!(state.set_next_counterparty_revoke_num(1).is_ok());
                assert_eq!(state.next_counterparty_revoke_num, 1);
                assert_eq!(state.next_counterparty_commit_num, 2);

                // can't retry the previous commit anymore
                assert_policy_err!(
                    state.set_next_counterparty_commit_num(
                        2,
                        make_test_pubkey(0x12),
                        commit_info.clone()
                    ),
                    "set_next_counterparty_commit_num: 2 too small relative to \
                     next_counterparty_revoke_num 1"
                );
                assert_eq!(state.next_counterparty_commit_num, 2);

                // can't skip commit ahead
                assert_policy_err!(
                    state.set_next_counterparty_commit_num(
                        4,
                        make_test_pubkey(0x16),
                        commit_info.clone()
                    ),
                    "set_next_counterparty_commit_num: 4 too large relative to \
                     next_counterparty_revoke_num 1"
                );
                assert_eq!(state.next_counterparty_commit_num, 2);

                // can't revoke backwards
                assert_policy_err!(
                    state.set_next_counterparty_revoke_num(0),
                    "set_next_counterparty_revoke_num: can\'t set next to 0"
                );
                assert_eq!(state.next_counterparty_revoke_num, 1);

                // can't skip revoke ahead
                assert_policy_err!(
                    state.set_next_counterparty_revoke_num(2),
                    "set_next_counterparty_revoke_num: 2 too large \
                     relative to next_counterparty_commit_num 2"
                );
                assert_eq!(state.next_counterparty_revoke_num, 1);

                // ADVANCE next_commit to 3
                assert!(state
                    .set_next_counterparty_commit_num(
                        3,
                        make_test_pubkey(0x14),
                        commit_info.clone()
                    )
                    .is_ok());
                // commit 0: revoked
                // commit 1: unrevoked <- next_revoke
                // commit 2: current
                // commit 3: next      <- next_commit
                assert_eq!(state.next_counterparty_revoke_num, 1);
                assert_eq!(state.next_counterparty_commit_num, 3);

                // retries ok
                assert!(state
                    .set_next_counterparty_commit_num(
                        3,
                        make_test_pubkey(0x14),
                        commit_info.clone()
                    )
                    .is_ok());
                assert_eq!(state.next_counterparty_commit_num, 3);

                // Can still retry the old revoke (they may not have seen our commit).
                assert!(state.set_next_counterparty_revoke_num(1).is_ok());
                assert_eq!(state.next_counterparty_revoke_num, 1);
                assert_eq!(state.next_counterparty_commit_num, 3);

                // Can't skip revoke ahead
                assert_policy_err!(
                    state.set_next_counterparty_revoke_num(3),
                    "set_next_counterparty_revoke_num: 3 too large relative to \
                     next_counterparty_commit_num 3"
                );
                assert_eq!(state.next_counterparty_revoke_num, 1);

                // can't commit ahead until revoke catches up
                assert_policy_err!(
                    state.set_next_counterparty_commit_num(
                        4,
                        make_test_pubkey(0x16),
                        commit_info.clone()
                    ),
                    "set_next_counterparty_commit_num: 4 too large relative to \
                     next_counterparty_revoke_num 1"
                );
                assert_eq!(state.next_counterparty_commit_num, 3);

                // can't commit behind
                assert_policy_err!(
                    state.set_next_counterparty_commit_num(
                        2,
                        make_test_pubkey(0x12),
                        commit_info.clone()
                    ),
                    "set_next_counterparty_commit_num: 2 too small relative to \
                     next_counterparty_revoke_num 1"
                );
                assert_eq!(state.next_counterparty_commit_num, 3);

                // REVOKE commit 1
                assert!(state.set_next_counterparty_revoke_num(2).is_ok());
                // commit 1: revoked
                // commit 2: current   <- next_revoke
                // commit 3: next      <- next_commit
                assert_eq!(state.next_counterparty_revoke_num, 2);
                assert_eq!(state.next_counterparty_commit_num, 3);

                // revoke retries ok
                assert!(state.set_next_counterparty_revoke_num(2).is_ok());
                assert_eq!(state.next_counterparty_revoke_num, 2);
                assert_eq!(state.next_counterparty_commit_num, 3);

                // can't revoke backwards
                assert_policy_err!(
                    state.set_next_counterparty_revoke_num(1),
                    "set_next_counterparty_revoke_num: invalid progression: 2 to 1"
                );
                assert_eq!(state.next_counterparty_revoke_num, 2);

                // can't revoke ahead until next commit
                assert_policy_err!(
                    state.set_next_counterparty_revoke_num(3),
                    "set_next_counterparty_revoke_num: 3 too large relative to \
                     next_counterparty_commit_num 3"
                );
                assert_eq!(state.next_counterparty_revoke_num, 2);

                // commit retry not ok anymore
                assert_policy_err!(
                    state.set_next_counterparty_commit_num(
                        3,
                        make_test_pubkey(0x14),
                        commit_info.clone()
                    ),
                    "set_next_counterparty_commit_num: 3 too small relative to \
                     next_counterparty_revoke_num 2"
                );
                assert_eq!(state.next_counterparty_commit_num, 3);

                // can't skip commit ahead
                assert_policy_err!(
                    state.set_next_counterparty_commit_num(
                        5,
                        make_test_pubkey(0x18),
                        commit_info.clone()
                    ),
                    "set_next_counterparty_commit_num: 5 too large relative to \
                     next_counterparty_revoke_num 2"
                );
                assert_eq!(state.next_counterparty_commit_num, 3);

                // ADVANCE next_commit to 4
                assert!(state
                    .set_next_counterparty_commit_num(
                        4,
                        make_test_pubkey(0x16),
                        commit_info.clone()
                    )
                    .is_ok());
                // commit 2: unrevoked <- next_revoke
                // commit 3: current
                // commit 4: next      <- next_commit
                assert_eq!(state.next_counterparty_revoke_num, 2);
                assert_eq!(state.next_counterparty_commit_num, 4);

                Ok(())
            })
            .expect("success");
    }

    fn vecs_match<T: PartialEq + std::cmp::Ord>(mut a: Vec<T>, mut b: Vec<T>) -> bool {
        a.sort();
        b.sort();
        let matching = a.iter().zip(b.iter()).filter(|&(a, b)| a == b).count();
        matching == a.len() && matching == b.len()
    }

    #[test]
    fn node_allowlist_test() {
        let node = init_node(TEST_NODE_CONFIG, TEST_SEED[1]);

        // initial allowlist should be empty
        assert!(node.allowlist().expect("allowlist").len() == 0);

        // can insert some entries
        let adds0 = vec![
            "mv4rnyY3Su5gjcDNzbMLKBQkBicCtHUtFB",
            "2N6i2gfgTonx88yvYm32PRhnHxqxtEfocbt",
            "tb1qhetd7l0rv6kca6wvmt25ax5ej05eaat9q29z7z",
            "tb1qycu764qwuvhn7u0enpg0x8gwumyuw565f3mspnn58rsgar5hkjmqtjegrh",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_status_ok!(node.add_allowlist(&adds0));

        // now allowlist should have the added entries
        assert!(vecs_match(
            node.allowlist().expect("allowlist").clone(),
            adds0.clone()
        ));

        // adding duplicates shouldn't change the node allowlist
        assert_status_ok!(node.add_allowlist(&adds0));
        assert!(vecs_match(
            node.allowlist().expect("allowlist").clone(),
            adds0.clone()
        ));

        // can remove some elements from the allowlist
        let removes0 = vec![adds0[0].clone(), adds0[3].clone()];
        assert_status_ok!(node.remove_allowlist(&removes0));
        assert!(vecs_match(
            node.allowlist().expect("allowlist").clone(),
            vec![adds0[1].clone(), adds0[2].clone()]
        ));

        // can't add bogus addresses
        assert_invalid_argument_err!(
            node.add_allowlist(&vec!["1234567890".to_string()]),
            "parse address 1234567890 failed: base58: invalid base58 character 0x30"
        );

        // can't add w/ wrong network
        assert_invalid_argument_err!(
            node.add_allowlist(&vec!["1287uUybCYgf7Tb76qnfPf8E1ohCgSZATp".to_string()]),
            "network mismatch for addr 1287uUybCYgf7Tb76qnfPf8E1ohCgSZATp: addr=bitcoin, node=testnet"
        );

        // can't remove w/ wrong network
        assert_invalid_argument_err!(
            node.remove_allowlist(&vec!["1287uUybCYgf7Tb76qnfPf8E1ohCgSZATp".to_string()]),
            "network mismatch for addr 1287uUybCYgf7Tb76qnfPf8E1ohCgSZATp: addr=bitcoin, node=testnet"
        );
    }
}
