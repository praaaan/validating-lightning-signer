use bitcoin::hashes::hex::ToHex;
use bitcoin::policy::DUST_RELAY_TX_FEE;
use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
use bitcoin::util::bip143::SigHashCache;
use bitcoin::{self, Network, OutPoint, Script, SigHash, SigHashType, Transaction};
use lightning::chain::keysinterface::{BaseSign, InMemorySigner};
use lightning::ln::chan_utils::{
    build_htlc_transaction, htlc_success_tx_weight, htlc_timeout_tx_weight,
    make_funding_redeemscript, ClosingTransaction, HTLCOutputInCommitment, TxCreationKeys,
};
use lightning::ln::PaymentHash;
use log::{debug, info};

use crate::channel::{ChannelId, ChannelSetup, ChannelSlot};
use crate::policy::validator::EnforcementState;
use crate::policy::validator::{ChainState, Validator, ValidatorFactory};
use crate::prelude::*;
use crate::sync::Arc;
use crate::tx::tx::{
    parse_offered_htlc_script, parse_received_htlc_script, parse_revokeable_redeemscript,
    CommitmentInfo, CommitmentInfo2,
};
use crate::util::crypto_utils::payload_for_p2wsh;
use crate::util::debug_utils::{
    script_debug, DebugHTLCOutputInCommitment, DebugInMemorySigner, DebugTxCreationKeys,
    DebugVecVecU8,
};
use crate::util::transaction_utils::MIN_DUST_LIMIT_SATOSHIS;
use crate::wallet::Wallet;

extern crate scopeguard;

use super::error::{policy_error, transaction_format_error, ValidationError};

/// A factory for SimpleValidator
pub struct SimpleValidatorFactory {}

/// Construct a SimpleValidator
pub fn simple_validator(
    network: Network,
    node_id: PublicKey,
    channel_id: Option<ChannelId>,
) -> SimpleValidator {
    SimpleValidator { policy: make_simple_policy(network), node_id, channel_id }
}

impl ValidatorFactory for SimpleValidatorFactory {
    fn make_validator(
        &self,
        network: Network,
        node_id: PublicKey,
        channel_id: Option<ChannelId>,
    ) -> Box<dyn Validator> {
        Box::new(simple_validator(network, node_id, channel_id))
    }
}

/// A simple policy to configure a SimpleValidator
#[derive(Clone)]
pub struct SimplePolicy {
    /// Minimum delay in blocks
    pub min_delay: u16,
    /// Maximum delay in blocks
    pub max_delay: u16,
    /// Maximum channel value in satoshi
    pub max_channel_size_sat: u64,
    /// Maximum amount allowed to be pushed (v1 channel establishment)
    pub max_push_sat: u64,
    /// amounts below this number of satoshi are not considered important
    pub epsilon_sat: u64,
    /// Maximum number of in-flight HTLCs
    pub max_htlcs: usize,
    /// Maximum value of in-flight HTLCs
    pub max_htlc_value_sat: u64,
    /// Whether to use knowledge of chain state (e.g. current_height)
    pub use_chain_state: bool,
    /// Minimum feerate
    pub min_feerate_per_kw: u32,
    /// Maximum feerate
    pub max_feerate_per_kw: u32,
    /// Minimum fee
    pub min_fee: u64,
    /// Maximum fee
    pub max_fee: u64,
}

/// A simple validator
pub struct SimpleValidator {
    policy: SimplePolicy,
    node_id: PublicKey,
    channel_id: Option<ChannelId>,
}

impl SimpleValidator {
    const ANCHOR_SEQS: [u32; 1] = [0x_0000_0001];
    const NON_ANCHOR_SEQS: [u32; 3] = [0x_0000_0000_u32, 0x_ffff_fffd_u32, 0x_ffff_ffff_u32];

    fn log_prefix(&self) -> String {
        let short_node_id = &self.node_id.to_hex()[0..4];
        let short_channel_id =
            self.channel_id.map(|c| c.0.to_hex()[0..4].to_string()).unwrap_or("".to_string());
        format!("{}/{}", short_node_id, short_channel_id)
    }

    fn validate_delay(&self, name: &str, delay: u32) -> Result<(), ValidationError> {
        let policy = &self.policy;

        if delay < policy.min_delay as u32 {
            return policy_err!("{} too small: {} < {}", name, delay, policy.min_delay);
        }
        if delay > policy.max_delay as u32 {
            return policy_err!("{} too large: {} > {}", name, delay, policy.max_delay);
        }

        Ok(())
    }

    fn validate_expiry(
        &self,
        name: &str,
        expiry: u32,
        current_height: u32,
    ) -> Result<(), ValidationError> {
        let policy = &self.policy;

        if policy.use_chain_state {
            if expiry < current_height + policy.min_delay as u32 {
                return policy_err!(
                    "{} expiry too early: {} < {}",
                    name,
                    expiry,
                    current_height + policy.min_delay as u32
                );
            }
            if expiry > current_height + policy.max_delay as u32 {
                return policy_err!(
                    "{} expiry too late: {} > {}",
                    name,
                    expiry,
                    current_height + policy.max_delay as u32
                );
            }
        }

        Ok(())
    }

    fn validate_fee(&self, sum_inputs: u64, sum_outputs: u64) -> Result<(), ValidationError> {
        let fee = sum_inputs.checked_sub(sum_outputs).ok_or_else(|| {
            policy_error(format!("fee underflow: {} - {}", sum_inputs, sum_outputs))
        })?;
        if fee < self.policy.min_fee {
            return policy_err!("fee below minimum: {} < {}", fee, self.policy.min_fee);
        }
        if fee > self.policy.max_fee {
            return policy_err!("fee above maximum: {} > {}", fee, self.policy.max_fee);
        }
        Ok(())
    }

    fn outside_epsilon_range(&self, value0: u64, value1: u64) -> (bool, String) {
        if value0 > value1 {
            (value0 - value1 > self.policy.epsilon_sat, "larger".to_string())
        } else {
            (value1 - value0 > self.policy.epsilon_sat, "smaller".to_string())
        }
    }

    // Common validation for validate_{delayed,counterparty_htlc,justice}_sweep
    fn validate_sweep(
        &self,
        wallet: &Wallet,
        tx: &Transaction,
        input: usize,
        amount_sat: u64,
        wallet_path: &Vec<u32>,
    ) -> Result<(), ValidationError> {
        // TODO - remove these checks when we handle multiple inputs/outputs
        if tx.input.len() != 1 {
            return transaction_format_err!("bad number of inputs: {} != 1", tx.input.len());
        }
        if tx.output.len() != 1 {
            return transaction_format_err!("bad number of outputs: {} != 1", tx.output.len());
        }
        if input != 0 {
            return transaction_format_err!("bad input index: {} != 0", input);
        }

        // policy-sweep-version
        if tx.version != 2 {
            return transaction_format_err!("bad version: {}", tx.version);
        }

        // policy-sweep-fee-range
        self.validate_fee(amount_sat, tx.output[0].value)
            .map_err(|ve| ve.prepend_msg(format!("{}: ", containing_function!())))?;

        // policy-sweep-destination-allowlisted
        let dest_script = &tx.output[0].script_pubkey;
        if !wallet
            .can_spend(wallet_path, dest_script)
            .map_err(|err| policy_error(format!("wallet can_spend error: {}", err)))?
            && !wallet.allowlist_contains(dest_script)
        {
            info!(
                "dest_script not matched: path={:?}, {}",
                wallet_path,
                script_debug(dest_script, wallet.network())
            );
            return policy_err!("destination is not in wallet or allowlist");
        }

        Ok(())
    }
}

// TODO - policy-onchain-change-path-predictable

// TODO - policy-commitment-spends-active-utxo
// TODO - policy-commitment-htlc-routing-balance
// TODO - policy-commitment-htlc-received-spends-active-utxo
// TODO - policy-commitment-htlc-cltv-range [NEEDS NEW HTLC DETECTION]
// TODO - policy-commitment-htlc-offered-hash-matches
// TODO - policy-commitment-previous-revoked [still need secret storage]
// TODO - policy-commitment-anchors-not-when-off
// TODO - policy-commitment-anchor-to-holder
// TODO - policy-commitment-anchor-to-counterparty
// TODO - policy-commitment-anchor-amount [NO TESTS TAGGED]
// TODO - policy-commitment-anchor-static-remotekey
// TODO - policy-commitment-anchor-match-fundingkey [NO TESTS TAGGED]

// TODO - policy-commitment-payment-settled-preimage
// TODO - policy-commitment-payment-allowlisted
// TODO - policy-commitment-payment-velocity
// TODO - policy-commitment-payment-approved
// TODO - policy-commitment-payment-invoiced

// TODO - policy-htlc-cltv-range

// TODO - policy-forced-destination-allowlisted
// TODO - policy-forced-fee-range

// TODO - policy-velocity-funding
// TODO - policy-velocity-transferred
// TODO - policy-merchant-no-sends
// TODO - policy-routing-deltas-only-htlc

impl Validator for SimpleValidator {
    fn validate_ready_channel(
        &self,
        wallet: &Wallet,
        setup: &ChannelSetup,
        holder_shutdown_key_path: &Vec<u32>,
    ) -> Result<(), ValidationError> {
        let mut debug_on_return = scoped_debug_return!(setup, holder_shutdown_key_path);

        // NOTE - setup.channel_value_sat is not valid, set later on.

        // policy-channel-counterparty-contest-delay-range
        // policy-commitment-to-self-delay-range relies on this value
        self.validate_delay(
            "counterparty_selected_contest_delay",
            setup.counterparty_selected_contest_delay as u32,
        )?;

        // policy-channel-holder-contest-delay-range
        // policy-commitment-to-self-delay-range relies on this value
        self.validate_delay(
            "holder_selected_contest_delay",
            setup.holder_selected_contest_delay as u32,
        )?;

        // policy-mutual-destination-allowlisted
        if let Some(holder_shutdown_script) = &setup.holder_shutdown_script {
            if !wallet
                .can_spend(holder_shutdown_key_path, &holder_shutdown_script)
                .map_err(|err| policy_error(format!("wallet can_spend error: {}", err)))?
                && !wallet.allowlist_contains(&holder_shutdown_script)
            {
                info!(
                    "holder_shutdown_script not matched: path={:?}, {}",
                    holder_shutdown_key_path,
                    script_debug(holder_shutdown_script, wallet.network())
                );
                return policy_err!("holder_shutdown_script is not in wallet or allowlist");
            }
        }
        *debug_on_return = false;
        Ok(())
    }

    fn validate_channel_value(&self, setup: &ChannelSetup) -> Result<(), ValidationError> {
        if setup.channel_value_sat > self.policy.max_channel_size_sat {
            return policy_err!("channel value {} too large", setup.channel_value_sat);
        }
        Ok(())
    }

    fn validate_onchain_tx(
        &self,
        wallet: &Wallet,
        channels: Vec<Option<Arc<Mutex<ChannelSlot>>>>,
        tx: &Transaction,
        values_sat: &Vec<u64>,
        opaths: &Vec<Vec<u32>>,
    ) -> Result<(), ValidationError> {
        let mut debug_on_return = scoped_debug_return!(tx, values_sat, opaths);

        // policy-onchain-fee-range
        let mut sum_inputs: u64 = 0;
        for val in values_sat {
            sum_inputs = sum_inputs
                .checked_add(*val)
                .ok_or_else(|| policy_error(format!("funding sum inputs overflow")))?;
        }
        let mut sum_outputs: u64 = 0;
        for outp in &tx.output {
            sum_outputs = sum_outputs
                .checked_add(outp.value)
                .ok_or_else(|| policy_error(format!("funding sum outputs overflow")))?;
        }
        self.validate_fee(sum_inputs, sum_outputs)
            .map_err(|ve| ve.prepend_msg(format!("{}: ", containing_function!())))?;

        // policy-onchain-format-standard
        if tx.version != 2 {
            return policy_err!("invalid version: {}", tx.version);
        }

        for outndx in 0..tx.output.len() {
            let output = &tx.output[outndx];
            let opath = &opaths[outndx];
            let channel_slot = channels[outndx].as_ref();

            // policy-onchain-output-match-commitment
            // policy-onchain-change-to-wallet
            // All outputs must either be wallet (change) or channel funding.
            if opath.len() > 0 {
                let spendable = wallet.can_spend(opath, &output.script_pubkey).map_err(|err| {
                    policy_error(format!("output[{}]: wallet_can_spend error: {}", outndx, err))
                })?;
                if !spendable {
                    return policy_err!("wallet cannot spend output[{}]", outndx);
                }
            } else {
                let slot = channel_slot.ok_or_else(|| {
                    let outpoint = OutPoint { txid: tx.txid(), vout: outndx as u32 };
                    policy_error(format!("unknown output: {}", outpoint))
                })?;
                match &*slot.lock().unwrap() {
                    ChannelSlot::Ready(chan) => {
                        if chan.setup.push_value_msat / 1000 > self.policy.max_push_sat {
                            return policy_err!(
                                "push_value_msat {} greater than max_push_sat {}",
                                chan.setup.push_value_msat,
                                self.policy.max_push_sat
                            );
                        }

                        // policy-onchain-output-match-commitment
                        if output.value != chan.setup.channel_value_sat {
                            return policy_err!(
                                "funding output amount mismatch w/ channel: {} != {}",
                                output.value,
                                chan.setup.channel_value_sat
                            );
                        }

                        // policy-onchain-output-scriptpubkey
                        let funding_redeemscript = make_funding_redeemscript(
                            &chan.keys.pubkeys().funding_pubkey,
                            &chan.keys.counterparty_pubkeys().funding_pubkey,
                        );
                        let script_pubkey =
                            payload_for_p2wsh(&funding_redeemscript).script_pubkey();
                        if output.script_pubkey != script_pubkey {
                            return policy_err!(
                                "funding script_pubkey mismatch w/ channel: {} != {}",
                                output.script_pubkey,
                                script_pubkey
                            );
                        }

                        // policy-onchain-initial-commitment-countersigned
                        if chan.enforcement_state.next_holder_commit_num != 1 {
                            return policy_err!("initial holder commitment not validated",);
                        }
                    }
                    _ => panic!("this can't happen"),
                };
            }
        }
        *debug_on_return = false;
        Ok(())
    }

    fn decode_commitment_tx(
        &self,
        keys: &InMemorySigner,
        setup: &ChannelSetup,
        is_counterparty: bool,
        tx: &bitcoin::Transaction,
        output_witscripts: &Vec<Vec<u8>>,
    ) -> Result<CommitmentInfo, ValidationError> {
        let mut debug_on_return = scoped_debug_return!(
            DebugInMemorySigner(keys),
            setup,
            is_counterparty,
            tx,
            DebugVecVecU8(output_witscripts)
        );

        // policy-commitment-version
        if tx.version != 2 {
            return policy_err!("bad commitment version: {}", tx.version);
        }

        let mut info = CommitmentInfo::new(is_counterparty);
        for ind in 0..tx.output.len() {
            info.handle_output(keys, setup, &tx.output[ind], output_witscripts[ind].as_slice())
                .map_err(|ve| {
                    ve.prepend_msg(format!("{}: tx output[{}]: ", containing_function!(), ind))
                })?;
        }

        *debug_on_return = false;
        Ok(info)
    }

    fn validate_counterparty_commitment_tx(
        &self,
        estate: &EnforcementState,
        commit_num: u64,
        commitment_point: &PublicKey,
        setup: &ChannelSetup,
        cstate: &ChainState,
        info: &CommitmentInfo2,
    ) -> Result<(), ValidationError> {
        if let Some(current) = &estate.current_counterparty_commit_info {
            let (added, removed) = current.delta_offered_htlcs(info);
            debug!(
                "{} counterparty offered delta outbound={} +{:?} -{:?}",
                self.log_prefix(),
                setup.is_outbound,
                added.collect::<Vec<_>>(),
                removed.collect::<Vec<_>>()
            );
            let (added, removed) = current.delta_received_htlcs(info);
            debug!(
                "{} counterparty received delta outbound={} +{:?} -{:?}",
                self.log_prefix(),
                setup.is_outbound,
                added.collect::<Vec<_>>(),
                removed.collect::<Vec<_>>()
            );
        }
        // Validate common commitment constraints
        self.validate_commitment_tx(estate, commit_num, commitment_point, setup, cstate, info)
            .map_err(|ve| ve.prepend_msg(format!("{}: ", containing_function!())))?;

        let mut debug_on_return =
            scoped_debug_return!(estate, commit_num, commitment_point, setup, cstate, info);

        // policy-commitment-to-self-delay-range
        if info.to_self_delay != setup.holder_selected_contest_delay {
            return Err(policy_error("holder_selected_contest_delay mismatch".to_string()));
        }

        // policy-commitment-previous-revoked
        // if next_counterparty_revoke_num is 20:
        // - commit_num 19 has been revoked
        // - commit_num 20 is current, previously signed, ok to resign
        // - commit_num 21 is ok to sign, advances the state
        // - commit_num 22 is not ok to sign
        // This check overlaps the check in set_next_counterparty_commit_num
        // but gives better diagnostic.
        if commit_num > estate.next_counterparty_revoke_num + 1 {
            return policy_err!(
                "invalid attempt to sign counterparty commit_num {} \
                         with next_counterparty_revoke_num {}",
                commit_num,
                estate.next_counterparty_revoke_num
            );
        }

        // policy-commitment-retry-same
        // Is this a retry?
        if commit_num + 1 == estate.next_counterparty_commit_num {
            // The commit_point must be the same as previous
            let prev_commit_point = estate.get_previous_counterparty_point(commit_num)?;
            if *commitment_point != prev_commit_point {
                return policy_err!(
                    "retry of sign_counterparty_commitment {} with changed point: \
                             prev {} != new {}",
                    commit_num,
                    &prev_commit_point,
                    &commitment_point
                );
            }

            // The CommitmentInfo2 must be the same as previously
            let prev_commit_info = estate.get_previous_counterparty_commit_info(commit_num)?;
            if *info != prev_commit_info {
                debug_vals!(*info, prev_commit_info);
                return policy_err!(
                    "retry of sign_counterparty_commitment {} with changed info",
                    commit_num,
                );
            }
        }

        *debug_on_return = false;
        Ok(())
    }

    fn validate_holder_commitment_tx(
        &self,
        estate: &EnforcementState,
        commit_num: u64,
        commitment_point: &PublicKey,
        setup: &ChannelSetup,
        cstate: &ChainState,
        info: &CommitmentInfo2,
    ) -> Result<(), ValidationError> {
        if let Some(current) = &estate.current_holder_commit_info {
            let (added, removed) = current.delta_offered_htlcs(info);
            debug!(
                "{} holder offered delta outbound={} +{:?} -{:?}",
                self.log_prefix(),
                setup.is_outbound,
                added.collect::<Vec<_>>(),
                removed.collect::<Vec<_>>()
            );
            let (added, removed) = current.delta_received_htlcs(info);
            debug!(
                "{} holder received delta outbound={} +{:?} -{:?}",
                self.log_prefix(),
                setup.is_outbound,
                added.collect::<Vec<_>>(),
                removed.collect::<Vec<_>>()
            );
        }

        // Validate common commitment constraints
        self.validate_commitment_tx(estate, commit_num, commitment_point, setup, cstate, info)
            .map_err(|ve| ve.prepend_msg(format!("{}: ", containing_function!())))?;

        let mut debug_on_return =
            scoped_debug_return!(estate, commit_num, commitment_point, setup, cstate, info);

        // policy-commitment-to-self-delay-range
        if info.to_self_delay != setup.counterparty_selected_contest_delay {
            return Err(policy_error("counterparty_selected_contest_delay mismatch".to_string()));
        }

        // policy-commitment-retry-same
        // Is this a retry?
        if commit_num + 1 == estate.next_holder_commit_num {
            // The CommitmentInfo2 must be the same as previously
            let holder_commit_info =
                &estate.current_holder_commit_info.as_ref().expect("current_holder_commit_info");
            if info != *holder_commit_info {
                debug_vals!(*info, holder_commit_info);
                return policy_err!("retry holder commitment {} with changed info", commit_num);
            }
        }

        // policy-commitment-holder-not-revoked
        // This test overlaps the check in set_next_holder_commit_num but gives
        // better diagnostic.
        if commit_num + 2 <= estate.next_holder_commit_num {
            debug_failed_vals!(estate, commit_num);
            return policy_err!(
                "can't sign revoked commitment_number {}, \
                 next_holder_commit_num is {}",
                commit_num,
                estate.next_holder_commit_num
            );
        };

        // policy-revoke-not-closed
        // It's ok to validate the current state when closed, but not ok to validate
        // a new state.
        if commit_num == estate.next_holder_commit_num && estate.mutual_close_signed {
            debug_failed_vals!(estate);
            return policy_err!("mutual close already signed");
        }

        *debug_on_return = false;
        Ok(())
    }

    fn validate_counterparty_revocation(
        &self,
        state: &EnforcementState,
        revoke_num: u64,
        commitment_secret: &SecretKey,
    ) -> Result<(), ValidationError> {
        let secp_ctx = Secp256k1::signing_only();

        // Only allowed to revoke expected next or retry.
        if revoke_num != state.next_counterparty_revoke_num
            && revoke_num + 1 != state.next_counterparty_revoke_num
        {
            debug_failed_vals!(state, revoke_num, commitment_secret);
            return policy_err!(
                "invalid counterparty revoke_num {} with next_counterparty_revoke_num {}",
                revoke_num,
                state.next_counterparty_revoke_num
            );
        }

        // policy-commitment-previous-revoked (partial: secret validated, but not stored here)
        let supplied_commit_point = PublicKey::from_secret_key(&secp_ctx, &commitment_secret);
        let prev_commit_point = state.get_previous_counterparty_point(revoke_num)?;
        if supplied_commit_point != prev_commit_point {
            debug_failed_vals!(state, revoke_num, commitment_secret);
            return policy_err!(
                "revocation commit point mismatch for commit_num {}: supplied {}, previous {}",
                revoke_num,
                supplied_commit_point,
                prev_commit_point
            );
        }

        Ok(())
    }

    // Phase 1
    // setup and txkeys must come from a trusted source
    fn decode_and_validate_htlc_tx(
        &self,
        is_counterparty: bool,
        setup: &ChannelSetup,
        txkeys: &TxCreationKeys,
        tx: &Transaction,
        redeemscript: &Script,
        htlc_amount_sat: u64,
        output_witscript: &Script,
    ) -> Result<(u32, HTLCOutputInCommitment, SigHash, SigHashType), ValidationError> {
        let to_self_delay = if is_counterparty {
            setup.holder_selected_contest_delay // the local side imposes this value
        } else {
            setup.counterparty_selected_contest_delay // the remote side imposes this value
        };
        let sighash_type = if is_counterparty && setup.option_anchor_outputs() {
            SigHashType::SinglePlusAnyoneCanPay
        } else {
            SigHashType::All
        };
        let original_tx_sighash =
            SigHashCache::new(tx).signature_hash(0, &redeemscript, htlc_amount_sat, sighash_type);

        let offered = if parse_offered_htlc_script(redeemscript, setup.option_anchor_outputs())
            .is_ok()
        {
            true
        } else if parse_received_htlc_script(redeemscript, setup.option_anchor_outputs()).is_ok() {
            false
        } else {
            debug_failed_vals!(
                is_counterparty,
                setup,
                DebugTxCreationKeys(txkeys),
                tx,
                redeemscript,
                htlc_amount_sat,
                output_witscript
            );
            return Err(policy_error("invalid redeemscript".to_string()));
        };

        // Extract some parameters from the submitted transaction.
        let cltv_expiry = if offered { tx.lock_time } else { 0 };
        let transaction_output_index = tx.input[0].previous_output.vout;
        let commitment_txid = tx.input[0].previous_output.txid;
        let total_fee = htlc_amount_sat - tx.output[0].value;

        // Derive the feerate_per_kw used to generate this
        // transaction.  Compensate for the total_fee being rounded
        // down when computed.
        let weight = if offered {
            htlc_timeout_tx_weight(setup.option_anchor_outputs())
        } else {
            htlc_success_tx_weight(setup.option_anchor_outputs())
        };
        let feerate_per_kw = (((total_fee * 1000) + weight - 1) / weight) as u32;

        let htlc = HTLCOutputInCommitment {
            offered,
            amount_msat: htlc_amount_sat * 1000,
            cltv_expiry,
            payment_hash: PaymentHash([0; 32]), // isn't used
            transaction_output_index: Some(transaction_output_index),
        };

        // Recompose the transaction.
        let recomposed_tx = build_htlc_transaction(
            &commitment_txid,
            feerate_per_kw,
            to_self_delay,
            &htlc,
            setup.option_anchor_outputs(),
            &txkeys.broadcaster_delayed_payment_key,
            &txkeys.revocation_key,
        );

        let recomposed_tx_sighash = SigHashCache::new(&recomposed_tx).signature_hash(
            0,
            &redeemscript,
            htlc_amount_sat,
            sighash_type,
        );

        if recomposed_tx_sighash != original_tx_sighash {
            debug_failed_vals!(
                is_counterparty,
                setup,
                DebugTxCreationKeys(txkeys),
                tx,
                redeemscript,
                htlc_amount_sat,
                output_witscript
            );
            let (revocation_key, contest_delay, delayed_pubkey) =
                parse_revokeable_redeemscript(output_witscript, setup.option_anchor_outputs())
                    .unwrap_or_else(|_| (vec![], 0, vec![]));
            debug!(
                "ORIGINAL_TX={:#?}\n\
                     output witscript params: [\n\
                     \x20  revocation_pubkey: {},\n\
                     \x20  to_self_delay: {},\n\
                     \x20  delayed_pubkey: {},\n\
                     ]",
                &tx,
                revocation_key.to_hex(),
                contest_delay,
                delayed_pubkey.to_hex()
            );
            debug!(
                "RECOMPOSED_TX={:#?}\n\
                     output witscript params: [\n\
                     \x20  revocation_pubkey: {},\n\
                     \x20  to_self_delay: {},\n\
                     \x20  delayed_pubkey: {},\n\
                     ]",
                &recomposed_tx,
                &txkeys.revocation_key,
                to_self_delay,
                &txkeys.broadcaster_delayed_payment_key
            );
            return Err(policy_error("sighash mismatch".to_string()));
        }

        // The sighash comparison in the previous block will fail if any of the
        // following policies are violated:
        // - policy-htlc-version
        // - policy-htlc-locktime
        // - policy-htlc-sequence
        // - policy-htlc-to-self-delay
        // - policy-htlc-revocation-pubkey
        // - policy-htlc-delayed-pubkey

        Ok((feerate_per_kw, htlc, recomposed_tx_sighash, sighash_type))
    }

    fn validate_htlc_tx(
        &self,
        _setup: &ChannelSetup,
        _cstate: &ChainState,
        _is_counterparty: bool,
        htlc: &HTLCOutputInCommitment,
        feerate_per_kw: u32,
    ) -> Result<(), ValidationError> {
        let mut debug_on_return =
            scoped_debug_return!(DebugHTLCOutputInCommitment(htlc), feerate_per_kw);

        // This must be further checked with policy-htlc-cltv-range.
        // Note that we can't check cltv_expiry for non-offered 2nd level
        // HTLC txs in phase 1, because they don't mention the cltv_expiry
        // there, only in the commitment tx output.
        // policy-htlc-locktime
        if htlc.offered && htlc.cltv_expiry == 0 {
            return policy_err!("offered lock_time must be non-zero");
        }

        // policy-htlc-fee-range
        if feerate_per_kw < self.policy.min_feerate_per_kw {
            return policy_err!(
                "feerate_per_kw of {} is smaller than the minimum of {}",
                feerate_per_kw,
                self.policy.min_feerate_per_kw
            );
        }
        if feerate_per_kw > self.policy.max_feerate_per_kw {
            return policy_err!(
                "feerate_per_kw of {} is larger than the maximum of {}",
                feerate_per_kw,
                self.policy.max_feerate_per_kw
            );
        }

        *debug_on_return = false;
        Ok(())
    }

    fn decode_and_validate_mutual_close_tx(
        &self,
        wallet: &Wallet,
        setup: &ChannelSetup,
        estate: &EnforcementState,
        tx: &Transaction,
        wallet_paths: &Vec<Vec<u32>>,
    ) -> Result<ClosingTransaction, ValidationError> {
        // Log state and inputs if we don't succeed.
        let should_debug = true;
        let mut debug_on_return = scopeguard::guard(should_debug, |should_debug| {
            if should_debug {
                if log::log_enabled!(log::Level::Debug) {
                    debug!(
                        "{} failed: {}",
                        containing_function!(),
                        vals_str!(setup, estate, tx, wallet_paths)
                    );

                    // Log the addresses associated with the outputs
                    let mut addrstrs = String::new();
                    for ndx in 0..tx.output.len() {
                        let script = &tx.output[ndx].script_pubkey;
                        addrstrs.push_str(
                            &format!(
                                "\ntxout[{}]: {}",
                                ndx,
                                &script_debug(script, wallet.network())
                            )[..],
                        );
                    }
                    debug!("output addresses: {}", &addrstrs);
                }
            }
        });

        if tx.output.len() > 2 {
            return transaction_format_err!("invalid number of outputs: {}", tx.output.len(),);
        }

        // The caller checked, this shouldn't happen
        assert_eq!(wallet_paths.len(), tx.output.len());

        if estate.current_holder_commit_info.is_none() {
            return policy_err!("current_holder_commit_info missing");
        }
        if estate.current_counterparty_commit_info.is_none() {
            return policy_err!("current_counterparty_commit_info missing");
        }

        // Establish which output belongs to the holder by trying all possibilities

        // Guess which ordering is most likely based on commitment values.
        // - Makes it unlikely we'll have to call validate a second time.
        // - Allows us to return the "better" validation error.

        #[derive(Debug)]
        struct ValidateArgs {
            to_holder_value_sat: u64,
            to_counterparty_value_sat: u64,
            holder_script: Option<Script>,
            counterparty_script: Option<Script>,
            wallet_path: Vec<u32>,
        }

        // If the commitments are not in the expected state, or the values
        // are outside epsilon from each other the comparison won't be
        // meaningful and an arbitrary order will have to do ...
        //
        let holder_value = estate.minimum_to_holder_value(self.policy.epsilon_sat);
        let cparty_value = estate.minimum_to_counterparty_value(self.policy.epsilon_sat);
        debug!("holder_value={:#?}, cparty_value={:#?}", holder_value, cparty_value);
        let holder_value_is_larger = holder_value > cparty_value;
        debug!("holder_value_is_larger={}", holder_value_is_larger);

        let (likely_args, unlikely_args) = if tx.output.len() == 1 {
            let holders_output = ValidateArgs {
                to_holder_value_sat: tx.output[0].value,
                to_counterparty_value_sat: 0,
                holder_script: Some(tx.output[0].script_pubkey.clone()),
                counterparty_script: None,
                wallet_path: wallet_paths[0].clone(),
            };
            let cpartys_output = ValidateArgs {
                to_holder_value_sat: 0,
                to_counterparty_value_sat: tx.output[0].value,
                holder_script: None,
                counterparty_script: Some(tx.output[0].script_pubkey.clone()),
                wallet_path: vec![],
            };
            if holder_value_is_larger {
                debug!("{}: likely the holder's output", short_function!());
                (holders_output, cpartys_output)
            } else {
                debug!("{}: likely the counterparty's output", short_function!());
                (cpartys_output, holders_output)
            }
        } else {
            let holder_first = ValidateArgs {
                to_holder_value_sat: tx.output[0].value,
                to_counterparty_value_sat: tx.output[1].value,
                holder_script: Some(tx.output[0].script_pubkey.clone()),
                counterparty_script: Some(tx.output[1].script_pubkey.clone()),
                wallet_path: wallet_paths[0].clone(),
            };
            let cparty_first = ValidateArgs {
                to_holder_value_sat: tx.output[1].value,
                to_counterparty_value_sat: tx.output[0].value,
                holder_script: Some(tx.output[1].script_pubkey.clone()),
                counterparty_script: Some(tx.output[0].script_pubkey.clone()),
                wallet_path: wallet_paths[1].clone(),
            };
            if holder_value_is_larger {
                debug!(
                    "{}: likely output[0] is counterparty, output[1] is holder",
                    short_function!()
                );
                (cparty_first, holder_first)
            } else {
                debug!(
                    "{}: likely output[0] is holder, output[1] is counterparty",
                    short_function!()
                );
                (holder_first, cparty_first)
            }
        };

        debug!("{}: trying likely args: {:#?}", short_function!(), &likely_args);
        let likely_rv = self.validate_mutual_close_tx(
            wallet,
            setup,
            estate,
            likely_args.to_holder_value_sat,
            likely_args.to_counterparty_value_sat,
            &likely_args.holder_script,
            &likely_args.counterparty_script,
            &likely_args.wallet_path,
        );

        let good_args = if likely_rv.is_ok() {
            likely_args
        } else {
            // Try the other case
            debug!("{}: trying unlikely args: {:#?}", short_function!(), &unlikely_args);
            let unlikely_rv = self.validate_mutual_close_tx(
                wallet,
                setup,
                estate,
                unlikely_args.to_holder_value_sat,
                unlikely_args.to_counterparty_value_sat,
                &unlikely_args.holder_script,
                &unlikely_args.counterparty_script,
                &unlikely_args.wallet_path,
            );
            if unlikely_rv.is_ok() {
                unlikely_args
            } else {
                // Return the error from the likely attempt, it's probably "better"
                return Err(likely_rv.unwrap_err());
            }
        };

        let closing_tx = ClosingTransaction::new(
            good_args.to_holder_value_sat,
            good_args.to_counterparty_value_sat,
            good_args.holder_script.unwrap_or_else(|| Script::new()),
            good_args.counterparty_script.unwrap_or_else(|| Script::new()),
            setup.funding_outpoint,
        );
        let trusted = closing_tx.trust();
        let recomposed_tx = trusted.built_transaction();

        if *recomposed_tx != *tx {
            debug!("ORIGINAL_TX={:#?}", &tx);
            debug!("RECOMPOSED_TX={:#?}", &recomposed_tx);
            return policy_err!("recomposed tx mismatch");
        }

        *debug_on_return = false; // don't debug when we succeed
        Ok(closing_tx)
    }

    fn validate_mutual_close_tx(
        &self,
        wallet: &Wallet,
        setup: &ChannelSetup,
        estate: &EnforcementState,
        to_holder_value_sat: u64,
        to_counterparty_value_sat: u64,
        holder_script: &Option<Script>,
        counterparty_script: &Option<Script>,
        holder_wallet_path_hint: &Vec<u32>,
    ) -> Result<(), ValidationError> {
        let mut debug_on_return = scoped_debug_return!(
            setup,
            estate,
            to_holder_value_sat,
            to_counterparty_value_sat,
            holder_script,
            counterparty_script
        );

        let holder_info = estate
            .current_holder_commit_info
            .as_ref()
            .ok_or_else(|| policy_error("current_holder_commit_info missing"))?;

        let counterparty_info = estate
            .current_counterparty_commit_info
            .as_ref()
            .ok_or_else(|| policy_error("current_counterparty_commit_info missing"))?;

        if to_holder_value_sat > 0 && holder_script.is_none() {
            return policy_err!(
                "missing holder_script with {} to_holder_value_sat",
                to_holder_value_sat
            );
        }

        if to_counterparty_value_sat > 0 && counterparty_script.is_none() {
            return policy_err!(
                "missing counterparty_script with {} to_counterparty_value_sat",
                to_counterparty_value_sat
            );
        }

        // If the upfront holder_shutdown_script was in effect, make sure the
        // holder script matches.
        if setup.holder_shutdown_script.is_some() && to_holder_value_sat > 0 {
            if *holder_script != setup.holder_shutdown_script {
                return policy_err!("holder_script doesn't match upfront holder_shutdown_script");
            }
        }

        // policy-mutual-no-pending-htlcs
        if !holder_info.htlcs_is_empty() || !counterparty_info.htlcs_is_empty() {
            return policy_err!("cannot close with pending htlcs");
        }

        // policy-mutual-fee-range
        let sum_outputs = to_holder_value_sat
            .checked_add(to_counterparty_value_sat)
            .ok_or_else(|| policy_error("consumed overflow".to_string()))?;
        self.validate_fee(setup.channel_value_sat, sum_outputs)
            .map_err(|ve| ve.prepend_msg(format!("{}: ", containing_function!())))?;

        // policy-mutual-value-matches-commitment
        // To make this test independent of variable fees we compare the side that
        // isn't paying the fees.
        if setup.is_outbound {
            // We are the funder and paying the fees, make sure the counterparty's output matches
            // the latest commitments.  Our value will then be enforced by the max-fee policy.
            if let (true, descr) = self.outside_epsilon_range(
                to_counterparty_value_sat,
                counterparty_info.to_broadcaster_value_sat,
            ) {
                return policy_err!(
                    "to_counterparty_value {} \
                     is {} than counterparty_info.broadcaster_value_sat {}",
                    to_counterparty_value_sat,
                    descr,
                    counterparty_info.to_broadcaster_value_sat
                );
            }
            if let (true, descr) = self.outside_epsilon_range(
                to_counterparty_value_sat,
                holder_info.to_countersigner_value_sat,
            ) {
                return policy_err!(
                    "to_counterparty_value {} \
                     is {} than holder_info.countersigner_value_sat {}",
                    to_counterparty_value_sat,
                    descr,
                    holder_info.to_countersigner_value_sat
                );
            }
        } else {
            // The counterparty is the funder, make sure the holder's
            // output matches the latest commitments.
            if let (true, descr) = self
                .outside_epsilon_range(to_holder_value_sat, holder_info.to_broadcaster_value_sat)
            {
                return policy_err!(
                    "to_holder_value {} is {} than holder_info.broadcaster_value_sat {}",
                    to_holder_value_sat,
                    descr,
                    holder_info.to_broadcaster_value_sat
                );
            }
            if let (true, descr) = self.outside_epsilon_range(
                to_holder_value_sat,
                counterparty_info.to_countersigner_value_sat,
            ) {
                return policy_err!(
                    "to_holder_value {} is {} than counterparty_info.countersigner_value_sat {}",
                    to_holder_value_sat,
                    descr,
                    counterparty_info.to_countersigner_value_sat
                );
            }
        }

        // policy-mutual-destination-allowlisted
        if let Some(script) = &holder_script {
            if !wallet
                .can_spend(holder_wallet_path_hint, script)
                .map_err(|err| policy_error(format!("wallet can_spend error: {}", err)))?
                && !wallet.allowlist_contains(script)
            {
                return policy_err!("holder output not to wallet or in allowlist");
            }
        }

        *debug_on_return = false; // don't debug when we succeed
        Ok(())
    }

    fn validate_delayed_sweep(
        &self,
        wallet: &Wallet,
        setup: &ChannelSetup,
        cstate: &ChainState,
        tx: &Transaction,
        input: usize,
        amount_sat: u64,
        wallet_path: &Vec<u32>,
    ) -> Result<(), ValidationError> {
        let mut debug_on_return =
            scoped_debug_return!(setup, cstate, tx, input, amount_sat, wallet_path);

        // Common sweep validation
        self.validate_sweep(wallet, tx, input, amount_sat, wallet_path)
            .map_err(|ve| ve.prepend_msg(format!("{}: ", containing_function!())))?;

        // policy-sweep-locktime
        if tx.lock_time > cstate.current_height {
            return transaction_format_err!(
                "bad locktime: {} > {}",
                tx.lock_time,
                cstate.current_height
            );
        }

        // policy-sweep-sequence
        let seq = tx.input[0].sequence;
        if seq != setup.counterparty_selected_contest_delay as u32 {
            return transaction_format_err!(
                "bad sequence: {} != {}",
                seq,
                setup.counterparty_selected_contest_delay
            );
        }

        *debug_on_return = false;
        Ok(())
    }

    fn validate_counterparty_htlc_sweep(
        &self,
        wallet: &Wallet,
        setup: &ChannelSetup,
        cstate: &ChainState,
        tx: &Transaction,
        redeemscript: &Script,
        input: usize,
        amount_sat: u64,
        wallet_path: &Vec<u32>,
    ) -> Result<(), ValidationError> {
        let mut debug_on_return =
            scoped_debug_return!(setup, cstate, tx, input, amount_sat, wallet_path);

        // Common sweep validation
        self.validate_sweep(wallet, tx, input, amount_sat, wallet_path)
            .map_err(|ve| ve.prepend_msg(format!("{}: ", containing_function!())))?;

        // Parse the redeemscript to determine the cltv_expiry
        if let Ok((
            _revocation_hash,
            _remote_htlc_pubkey,
            _payment_hash_vec,
            _local_htlc_pubkey,
            cltv_expiry,
        )) = parse_received_htlc_script(redeemscript, setup.option_anchor_outputs())
        {
            // It's a received htlc (counterparty perspective)
            if cltv_expiry < 0 || cltv_expiry > u32::MAX as i64 {
                return transaction_format_err!("bad cltv_expiry: {}", cltv_expiry);
            }

            // policy-sweep-locktime
            if tx.lock_time > cltv_expiry as u32 {
                return transaction_format_err!(
                    "bad locktime: {} > {}",
                    tx.lock_time,
                    cltv_expiry as u32
                );
            }
        } else if let Ok((
            _revocation_hash,
            _remote_htlc_pubkey,
            _local_htlc_pubkey,
            _payment_hash_vec,
        )) = parse_offered_htlc_script(redeemscript, setup.option_anchor_outputs())
        {
            // It's an offered htlc (counterparty perspective)

            // policy-sweep-locktime
            if tx.lock_time > cstate.current_height {
                return transaction_format_err!(
                    "bad locktime: {} > {}",
                    tx.lock_time,
                    cstate.current_height
                );
            }
        } else {
            // The redeemscript didn't parse as received or offered ...
            return transaction_format_err!("bad redeemscript: {}", &redeemscript);
        };

        // policy-sweep-sequence
        let seq = tx.input[0].sequence;
        let valid_seqs = if setup.option_anchor_outputs() {
            SimpleValidator::ANCHOR_SEQS.to_vec()
        } else {
            SimpleValidator::NON_ANCHOR_SEQS.to_vec()
        };
        if !valid_seqs.contains(&seq) {
            return transaction_format_err!("bad sequence: {} not in {:?}", seq, valid_seqs,);
        }

        *debug_on_return = false;
        Ok(())
    }

    fn validate_justice_sweep(
        &self,
        wallet: &Wallet,
        _setup: &ChannelSetup,
        cstate: &ChainState,
        tx: &Transaction,
        input: usize,
        amount_sat: u64,
        wallet_path: &Vec<u32>,
    ) -> Result<(), ValidationError> {
        let mut debug_on_return =
            scoped_debug_return!(_setup, cstate, tx, input, amount_sat, wallet_path);

        // Common sweep validation
        self.validate_sweep(wallet, tx, input, amount_sat, wallet_path)
            .map_err(|ve| ve.prepend_msg(format!("{}: ", containing_function!())))?;

        // policy-sweep-locktime
        if tx.lock_time > cstate.current_height {
            return transaction_format_err!(
                "bad locktime: {} > {}",
                tx.lock_time,
                cstate.current_height
            );
        }

        // policy-sweep-sequence
        let seq = tx.input[0].sequence;
        let valid_seqs = SimpleValidator::NON_ANCHOR_SEQS.to_vec();
        if !valid_seqs.contains(&seq) {
            return transaction_format_err!("bad sequence: {} not in {:?}", seq, valid_seqs);
        }

        *debug_on_return = false;
        Ok(())
    }
}

impl SimpleValidator {
    // Common commitment validation applicable to both holder and counterparty txs
    fn validate_commitment_tx(
        &self,
        estate: &EnforcementState,
        commit_num: u64,
        commitment_point: &PublicKey,
        setup: &ChannelSetup,
        cstate: &ChainState,
        info: &CommitmentInfo2,
    ) -> Result<(), ValidationError> {
        let mut debug_on_return =
            scoped_debug_return!(estate, commit_num, commitment_point, setup, cstate, info);

        let policy = &self.policy;

        // policy-commitment-outputs-trimmed
        if info.to_broadcaster_value_sat > 0
            && info.to_broadcaster_value_sat < MIN_DUST_LIMIT_SATOSHIS
        {
            return policy_err!(
                "to_broadcaster_value_sat {} less than dust limit {}",
                info.to_broadcaster_value_sat,
                MIN_DUST_LIMIT_SATOSHIS
            );
        }
        if info.to_countersigner_value_sat > 0
            && info.to_countersigner_value_sat < MIN_DUST_LIMIT_SATOSHIS
        {
            return policy_err!(
                "to_countersigner_value_sat {} less than dust limit {}",
                info.to_countersigner_value_sat,
                MIN_DUST_LIMIT_SATOSHIS
            );
        }

        // policy-commitment-htlc-count-limit
        if info.offered_htlcs.len() + info.received_htlcs.len() > policy.max_htlcs {
            return Err(policy_error("too many HTLCs".to_string()));
        }

        let mut htlc_value_sat: u64 = 0;

        let offered_htlc_dust_limit = MIN_DUST_LIMIT_SATOSHIS
            + (DUST_RELAY_TX_FEE as u64 * htlc_timeout_tx_weight(setup.option_anchor_outputs())
                / 1000);
        for htlc in &info.offered_htlcs {
            // TODO - this check should be converted into two checks, one the first time
            // the HTLC is introduced and the other every time it is encountered.
            //
            // policy-commitment-htlc-cltv-range
            self.validate_expiry("offered HTLC", htlc.cltv_expiry, cstate.current_height)?;

            htlc_value_sat = htlc_value_sat
                .checked_add(htlc.value_sat)
                .ok_or_else(|| policy_error("offered HTLC value overflow".to_string()))?;

            // policy-commitment-outputs-trimmed
            if htlc.value_sat < offered_htlc_dust_limit {
                return policy_err!(
                    "offered htlc.value_sat {} less than dust limit {}",
                    htlc.value_sat,
                    offered_htlc_dust_limit
                );
            }
        }

        let received_htlc_dust_limit = MIN_DUST_LIMIT_SATOSHIS
            + (DUST_RELAY_TX_FEE as u64 * htlc_success_tx_weight(setup.option_anchor_outputs())
                / 1000);
        for htlc in &info.received_htlcs {
            // TODO - this check should be converted into two checks, one the first time
            // the HTLC is introduced and the other every time it is encountered.
            //
            // policy-commitment-htlc-cltv-range
            self.validate_expiry("received HTLC", htlc.cltv_expiry, cstate.current_height)?;

            htlc_value_sat = htlc_value_sat
                .checked_add(htlc.value_sat)
                .ok_or_else(|| policy_error("received HTLC value overflow".to_string()))?;

            // policy-commitment-outputs-trimmed
            if htlc.value_sat < received_htlc_dust_limit {
                return policy_err!(
                    "received htlc.value_sat {} less than dust limit {}",
                    htlc.value_sat,
                    received_htlc_dust_limit
                );
            }
        }

        // policy-commitment-htlc-inflight-limit
        if htlc_value_sat > policy.max_htlc_value_sat {
            return policy_err!("sum of HTLC values {} too large", htlc_value_sat);
        }

        // policy-commitment-fee-range
        let sum_outputs = info
            .to_broadcaster_value_sat
            .checked_add(info.to_countersigner_value_sat)
            .ok_or_else(|| policy_error("channel value overflow".to_string()))?
            .checked_add(htlc_value_sat)
            .ok_or_else(|| policy_error("channel value overflow on HTLC".to_string()))?;
        self.validate_fee(setup.channel_value_sat, sum_outputs)
            .map_err(|ve| ve.prepend_msg(format!("{}: ", containing_function!())))?;

        // Enforce additional requirements on initial commitments.
        if commit_num == 0 {
            if info.offered_htlcs.len() + info.received_htlcs.len() > 0 {
                return policy_err!("initial commitment may not have HTLCS");
            }

            // policy-commitment-initial-funding-value
            // If we are the funder, the value to us of the initial
            // commitment transaction should be equal to our funding
            // value.
            if setup.is_outbound {
                // Ensure that no extra value is sent to fundee, the
                // no-initial-htlcs and fee checks above will ensure
                // that our share is valid.

                let fundee_value_sat = if info.is_counterparty_broadcaster {
                    info.to_broadcaster_value_sat
                } else {
                    info.to_countersigner_value_sat
                };

                // The fundee is only entitled to push_value
                if fundee_value_sat > setup.push_value_msat / 1000 {
                    return policy_err!(
                        "initial commitment may only send push_value_msat ({}) to fundee",
                        setup.push_value_msat
                    );
                }
            }
        }

        *debug_on_return = false;
        Ok(())
    }
}

/// Construct a default simple policy
pub fn make_simple_policy(network: Network) -> SimplePolicy {
    if network == Network::Bitcoin {
        SimplePolicy {
            min_delay: 60,
            max_delay: 2016, // Match LDK maximum and default
            max_channel_size_sat: 1_000_000_001,
            max_push_sat: 0,
            epsilon_sat: 1_600_000,
            max_htlcs: 1000,
            max_htlc_value_sat: 16_777_216,
            use_chain_state: false,
            min_feerate_per_kw: 1000,
            max_feerate_per_kw: 1000 * 1000,
            min_fee: 100,
            max_fee: 1000,
        }
    } else {
        SimplePolicy {
            min_delay: 4,
            max_delay: 2016,                     // Match LDK maximum and default
            max_channel_size_sat: 1_000_000_001, // lnd itest: wumbu default + 1
            max_push_sat: 20_000,
            // lnd itest: async_bidirectional_payments (large amount of dust HTLCs) 1_600_000
            epsilon_sat: 10_000, // c-lightning
            max_htlcs: 1000,
            max_htlc_value_sat: 16_777_216, // lnd itest: multi-hop_htlc_error_propagation
            use_chain_state: false,
            min_feerate_per_kw: 500,    // c-lightning integration
            max_feerate_per_kw: 16_000, // c-lightning integration
            min_fee: 100,
            max_fee: 80_000, // c-lightning integration 79641
        }
    }
}

#[cfg(test)]
mod tests {
    use lightning::ln::PaymentHash;
    use test_env_log::test;

    use crate::tx::tx::HTLCInfo2;
    use crate::util::key_utils::*;
    use crate::util::test_utils::*;

    use super::*;

    fn make_test_validator() -> SimpleValidator {
        let policy = SimplePolicy {
            min_delay: 5,
            max_delay: 1440,
            max_channel_size_sat: 100_000_000,
            max_push_sat: 0,
            epsilon_sat: 100_000,
            max_htlcs: 1000,
            max_htlc_value_sat: 10_000_000,
            use_chain_state: true,
            min_feerate_per_kw: 1000,
            max_feerate_per_kw: 1000 * 1000,
            min_fee: 100,
            max_fee: 10_000,
        };

        SimpleValidator {
            policy,
            node_id: PublicKey::from_slice(&[2u8; 33]).unwrap(),
            channel_id: None,
        }
    }

    #[test]
    fn decode_commitment_test() {
        let validator = make_test_validator();
        let info = validator
            .decode_commitment_tx(
                &make_test_channel_keys(),
                &make_test_channel_setup(),
                true,
                &make_test_commitment_tx(),
                &vec![vec![]],
            )
            .unwrap();
        assert_eq!(info.is_counterparty_broadcaster, true);
    }

    #[test]
    fn validate_policy_commitment_version() {
        let validator = make_test_validator();
        let mut tx = make_test_commitment_tx();
        tx.version = 1;
        let res = validator.decode_commitment_tx(
            &make_test_channel_keys(),
            &make_test_channel_setup(),
            true,
            &tx,
            &vec![vec![]],
        );
        assert_policy_err!(res, "decode_commitment_tx: bad commitment version: 1");
    }

    #[test]
    fn validate_channel_value_test() {
        let mut setup = make_test_channel_setup();
        let validator = make_test_validator();
        setup.channel_value_sat = 100_000_000;
        assert!(validator.validate_channel_value(&setup).is_ok());
        setup.channel_value_sat = 100_000_001;
        assert!(validator.validate_channel_value(&setup).is_err());
    }

    fn make_counterparty_info(
        to_holder_value_sat: u64,
        to_counterparty_value_sat: u64,
        to_self_delay: u16,
        offered_htlcs: Vec<HTLCInfo2>,
        received_htlcs: Vec<HTLCInfo2>,
    ) -> CommitmentInfo2 {
        make_counterparty_info_with_feerate(
            to_holder_value_sat,
            to_counterparty_value_sat,
            to_self_delay,
            offered_htlcs,
            received_htlcs,
            7500,
        )
    }

    fn make_counterparty_info_with_feerate(
        to_holder_value_sat: u64,
        to_counterparty_value_sat: u64,
        to_self_delay: u16,
        offered_htlcs: Vec<HTLCInfo2>,
        received_htlcs: Vec<HTLCInfo2>,
        feerate_per_kw: u32,
    ) -> CommitmentInfo2 {
        let to_holder_pubkey = make_test_pubkey(1);
        let revocation_pubkey = make_test_pubkey(2);
        let to_broadcaster_delayed_pubkey = make_test_pubkey(3);
        CommitmentInfo2::new(
            true,
            to_holder_pubkey,
            to_holder_value_sat,
            revocation_pubkey,
            to_broadcaster_delayed_pubkey,
            to_counterparty_value_sat,
            to_self_delay,
            offered_htlcs,
            received_htlcs,
            feerate_per_kw,
        )
    }

    fn make_htlc_info2(expiry: u32) -> HTLCInfo2 {
        HTLCInfo2 { value_sat: 5010, payment_hash: PaymentHash([0; 32]), cltv_expiry: expiry }
    }

    #[test]
    fn validate_commitment_tx_test() {
        let validator = make_test_validator();
        let mut enforcement_state = EnforcementState::new();
        let commit_num = 23;
        enforcement_state
            .set_next_counterparty_commit_num_for_testing(commit_num, make_test_pubkey(0x10));
        enforcement_state.set_next_counterparty_revoke_num_for_testing(commit_num - 1);
        let commit_point = make_test_pubkey(0x12);
        let cstate = make_test_chain_state();
        let setup = make_test_channel_setup();
        let delay = setup.holder_selected_contest_delay;
        let info = make_counterparty_info(2_000_000, 999_000, delay, vec![], vec![]);
        assert_status_ok!(validator.validate_commitment_tx(
            &enforcement_state,
            commit_num,
            &commit_point,
            &setup,
            &cstate,
            &info,
        ));
    }

    // policy-channel-holder-contest-delay-range
    // policy-commitment-to-self-delay-range
    #[test]
    fn validate_to_holder_min_delay_test() {
        let node = init_node(TEST_NODE_CONFIG, TEST_SEED[1]);
        let mut setup = make_test_channel_setup();
        let validator = make_test_validator();
        setup.holder_selected_contest_delay = 5;
        assert!(validator.validate_ready_channel(&*node, &setup, &vec![]).is_ok());
        setup.holder_selected_contest_delay = 4;
        assert_policy_err!(
            validator.validate_ready_channel(&*node, &setup, &vec![]),
            "validate_delay: holder_selected_contest_delay too small: 4 < 5"
        );
    }

    // policy-channel-holder-contest-delay-range
    // policy-commitment-to-self-delay-range
    #[test]
    fn validate_to_holder_max_delay_test() {
        let node = init_node(TEST_NODE_CONFIG, TEST_SEED[1]);
        let mut setup = make_test_channel_setup();
        let validator = make_test_validator();
        setup.holder_selected_contest_delay = 1440;
        assert!(validator.validate_ready_channel(&*node, &setup, &vec![]).is_ok());
        setup.holder_selected_contest_delay = 1441;
        assert_policy_err!(
            validator.validate_ready_channel(&*node, &setup, &vec![]),
            "validate_delay: holder_selected_contest_delay too large: 1441 > 1440"
        );
    }

    // policy-channel-counterparty-contest-delay-range
    // policy-commitment-to-self-delay-range
    #[test]
    fn validate_to_counterparty_min_delay_test() {
        let node = init_node(TEST_NODE_CONFIG, TEST_SEED[1]);
        let mut setup = make_test_channel_setup();
        let validator = make_test_validator();
        setup.counterparty_selected_contest_delay = 5;
        assert!(validator.validate_ready_channel(&*node, &setup, &vec![]).is_ok());
        setup.counterparty_selected_contest_delay = 4;
        assert_policy_err!(
            validator.validate_ready_channel(&*node, &setup, &vec![]),
            "validate_delay: counterparty_selected_contest_delay too small: 4 < 5"
        );
    }

    // policy-channel-counterparty-contest-delay-range
    // policy-commitment-to-self-delay-range
    #[test]
    fn validate_to_counterparty_max_delay_test() {
        let node = init_node(TEST_NODE_CONFIG, TEST_SEED[1]);
        let mut setup = make_test_channel_setup();
        let validator = make_test_validator();
        setup.counterparty_selected_contest_delay = 1440;
        assert!(validator.validate_ready_channel(&*node, &setup, &vec![]).is_ok());
        setup.counterparty_selected_contest_delay = 1441;
        assert_policy_err!(
            validator.validate_ready_channel(&*node, &setup, &vec![]),
            "validate_delay: counterparty_selected_contest_delay too large: 1441 > 1440"
        );
    }

    // policy-commitment-fee-range
    #[test]
    fn validate_commitment_tx_shortage_test() {
        let validator = make_test_validator();
        let enforcement_state = EnforcementState::new();
        let commit_num = 0;
        let commit_point = make_test_pubkey(0x12);
        let cstate = make_test_chain_state();
        let setup = make_test_channel_setup();
        let delay = setup.holder_selected_contest_delay;
        let info_bad = make_counterparty_info(2_000_000, 1_000_001, delay, vec![], vec![]);
        assert_policy_err!(
            validator.validate_commitment_tx(
                &enforcement_state,
                commit_num,
                &commit_point,
                &setup,
                &cstate,
                &info_bad,
            ),
            "validate_commitment_tx: fee underflow: 3000000 - 3000001"
        );
    }

    // policy-commitment-fee-range
    #[test]
    fn validate_commitment_tx_htlc_shortage_test() {
        let validator = make_test_validator();
        let htlc =
            HTLCInfo2 { value_sat: 100_000, payment_hash: PaymentHash([0; 32]), cltv_expiry: 1005 };
        let mut enforcement_state = EnforcementState::new();
        let commit_num = 23;
        enforcement_state
            .set_next_counterparty_commit_num_for_testing(commit_num, make_test_pubkey(0x10));
        enforcement_state.set_next_counterparty_revoke_num_for_testing(commit_num - 1);
        let commit_point = make_test_pubkey(0x12);
        let cstate = make_test_chain_state();
        let setup = make_test_channel_setup();
        let delay = setup.holder_selected_contest_delay;
        let info = make_counterparty_info(2_000_000, 899_000, delay, vec![htlc.clone()], vec![]);

        assert_status_ok!(validator.validate_commitment_tx(
            &enforcement_state,
            commit_num,
            &commit_point,
            &setup,
            &cstate,
            &info,
        ));

        let info_bad =
            make_counterparty_info(2_000_000, 1_000_000, delay, vec![htlc.clone()], vec![]);
        assert_policy_err!(
            validator.validate_commitment_tx(
                &enforcement_state,
                commit_num,
                &commit_point,
                &setup,
                &cstate,
                &info_bad,
            ),
            "validate_commitment_tx: fee underflow: 3000000 - 3100000"
        );
    }

    #[test]
    fn validate_commitment_tx_initial_with_htlcs() {
        let validator = make_test_validator();
        let htlc =
            HTLCInfo2 { value_sat: 199_000, payment_hash: PaymentHash([0; 32]), cltv_expiry: 1005 };
        let enforcement_state = EnforcementState::new();
        let commit_num = 0;
        let commit_point = make_test_pubkey(0x12);
        let cstate = make_test_chain_state();
        let setup = make_test_channel_setup();
        let delay = setup.holder_selected_contest_delay;
        let info = make_counterparty_info(2_000_000, 800_000, delay, vec![htlc.clone()], vec![]);

        let status = validator.validate_commitment_tx(
            &enforcement_state,
            commit_num,
            &commit_point,
            &setup,
            &cstate,
            &info,
        );
        assert_policy_err!(status, "validate_commitment_tx: initial commitment may not have HTLCS");
    }

    // policy-commitment-initial-funding-value
    #[test]
    fn validate_commitment_tx_initial_with_bad_fundee_output() {
        let validator = make_test_validator();
        let enforcement_state = EnforcementState::new();
        let commit_num = 0;
        let commit_point = make_test_pubkey(0x12);
        let cstate = make_test_chain_state();
        let setup = make_test_channel_setup();
        let delay = setup.holder_selected_contest_delay;
        let info = make_counterparty_info(2_000_000, 999_000, delay, vec![], vec![]);

        let status = validator.validate_commitment_tx(
            &enforcement_state,
            commit_num,
            &commit_point,
            &setup,
            &cstate,
            &info,
        );
        assert_policy_err!(
            status,
            "validate_commitment_tx: initial commitment may only send push_value_msat (0) to fundee"
        );
    }

    // policy-commitment-htlc-count-limit
    #[test]
    fn validate_commitment_tx_htlc_count_test() {
        let validator = make_test_validator();
        let enforcement_state = EnforcementState::new();
        let commit_num = 0;
        let commit_point = make_test_pubkey(0x12);
        let cstate = make_test_chain_state();
        let setup = make_test_channel_setup();
        let htlcs = (0..1001).map(|_| make_htlc_info2(1100)).collect();
        let delay = setup.holder_selected_contest_delay;
        let info_bad = make_counterparty_info(99_000_000, 900_000, delay, vec![], htlcs);
        assert_policy_err!(
            validator.validate_commitment_tx(
                &enforcement_state,
                commit_num,
                &commit_point,
                &setup,
                &cstate,
                &info_bad,
            ),
            "too many HTLCs"
        );
    }

    // policy-commitment-htlc-inflight-limit
    #[test]
    fn validate_commitment_tx_htlc_value_test() {
        let validator = make_test_validator();
        let enforcement_state = EnforcementState::new();
        let commit_num = 0;
        let commit_point = make_test_pubkey(0x12);
        let cstate = make_test_chain_state();
        let setup = make_test_channel_setup();
        let delay = setup.holder_selected_contest_delay;
        let htlcs = (0..1000)
            .map(|_| HTLCInfo2 {
                value_sat: 10001,
                payment_hash: PaymentHash([0; 32]),
                cltv_expiry: 1100,
            })
            .collect();
        let info_bad = make_counterparty_info(99_000_000, 900_000, delay, vec![], htlcs);
        assert_policy_err!(
            validator.validate_commitment_tx(
                &enforcement_state,
                commit_num,
                &commit_point,
                &setup,
                &cstate,
                &info_bad,
            ),
            "validate_commitment_tx: sum of HTLC values 10001000 too large"
        );
    }

    #[test]
    fn validate_commitment_tx_htlc_delay_test() {
        let validator = make_test_validator();
        let mut enforcement_state = EnforcementState::new();
        let commit_num = 23;
        enforcement_state
            .set_next_counterparty_commit_num_for_testing(commit_num, make_test_pubkey(0x10));
        enforcement_state.set_next_counterparty_revoke_num_for_testing(commit_num - 1);
        let commit_point = make_test_pubkey(0x12);
        let cstate = make_test_chain_state();
        let setup = make_test_channel_setup();
        let delay = setup.holder_selected_contest_delay;
        let info_good =
            make_counterparty_info(2_000_000, 990_000, delay, vec![], vec![make_htlc_info2(1005)]);
        assert_validation_ok!(validator.validate_commitment_tx(
            &enforcement_state,
            commit_num,
            &commit_point,
            &setup,
            &cstate,
            &info_good,
        ));
        let info_good =
            make_counterparty_info(2_000_000, 990_000, delay, vec![], vec![make_htlc_info2(2440)]);
        assert_validation_ok!(validator.validate_commitment_tx(
            &enforcement_state,
            commit_num,
            &commit_point,
            &setup,
            &cstate,
            &info_good,
        ));
        let info_bad =
            make_counterparty_info(2_000_000, 990_000, delay, vec![], vec![make_htlc_info2(1004)]);
        assert_policy_err!(
            validator.validate_commitment_tx(
                &enforcement_state,
                commit_num,
                &commit_point,
                &setup,
                &cstate,
                &info_bad,
            ),
            "validate_expiry: received HTLC expiry too early: 1004 < 1005"
        );
        let info_bad =
            make_counterparty_info(2_000_000, 990_000, delay, vec![], vec![make_htlc_info2(2441)]);
        assert_policy_err!(
            validator.validate_commitment_tx(
                &enforcement_state,
                commit_num,
                &commit_point,
                &setup,
                &cstate,
                &info_bad,
            ),
            "validate_expiry: received HTLC expiry too late: 2441 > 2440"
        );
    }
}
