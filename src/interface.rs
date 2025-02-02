use crate::model::{GetTransactionsOpt, SPVVerifyResult};
use elements;
use elements::bitcoin::hashes::hex::ToHex;
use elements::bitcoin::hashes::{sha256, Hash};
use elements::bitcoin::secp256k1::{self, All, Secp256k1};
use elements::bitcoin::util::bip32::{
    ChildNumber, DerivationPath, ExtendedPrivKey, ExtendedPubKey,
};
use elements::bitcoin::PublicKey;
use elements::secp256k1_zkp;
use elements::{BlockHash, Script, Txid};
use hex;
use log::{info, trace};

use crate::model::{CreateTransactionOpt, TransactionDetails, UnblindedTXO, TXO};
use crate::network::{Config, ElementsNetwork};
use crate::scripts::{p2pkh_script, p2shwpkh_script, p2shwpkh_script_sig};
use bip39;

use crate::error::{fn_err, Error};
use crate::store::{Store, StoreMeta};
use crate::utils::derive_blinder;

use crate::transaction::*;
use elements::confidential::{Asset, Nonce, Value};
use elements::slip77::MasterBlindingKey;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, RwLock};

use crate::liquidex::{
    liquidex_blind, liquidex_changes, liquidex_estimated_changes, liquidex_fee, liquidex_needs,
    LiquidexMakeOpt, LiquidexProposal,
};

pub struct WalletCtx {
    pub secp: Secp256k1<All>,
    pub config: Config,
    pub store: Store,
    pub xpub: ExtendedPubKey,
    pub master_blinding: MasterBlindingKey,
    pub change_max_deriv: u32,
}

fn mnemonic2seed(mnemonic: &str) -> Result<Vec<u8>, Error> {
    let mnemonic = bip39::Mnemonic::parse_in(bip39::Language::English, mnemonic)?;
    // TODO: passphrase?
    let passphrase: &str = "";
    let seed = mnemonic.to_seed(passphrase);
    Ok(seed.to_vec())
}

fn mnemonic2xprv(mnemonic: &str, config: Config) -> Result<ExtendedPrivKey, Error> {
    let seed = mnemonic2seed(mnemonic)?;
    let xprv = ExtendedPrivKey::new_master(
        elements::bitcoin::network::constants::Network::Testnet,
        &seed,
    )?;

    // BIP44: m / purpose' / coin_type' / account' / change / address_index
    // coin_type = 1776 liquid bitcoin as defined in https://github.com/satoshilabs/slips/blob/master/slip-0044.md
    // slip44 suggest 1 for every testnet, so we are using it also for regtest
    let coin_type: u32 = match config.network() {
        ElementsNetwork::Liquid => 1776,
        ElementsNetwork::ElementsRegtest => 1,
    };
    // since we use P2WPKH-nested-in-P2SH it is 49 https://github.com/bitcoin/bips/blob/master/bip-0049.mediawiki
    let path_string = format!("m/49'/{}'/0'", coin_type);
    info!("Using derivation path {}/0|1/*", path_string);
    let path = DerivationPath::from_str(&path_string)?;
    let secp = Secp256k1::new();
    Ok(xprv.derive_priv(&secp, &path)?)
}

// Copied from current elements master
// TODO: remove when updating elements
/// Create the shared secret.
pub fn make_shared_secret(
    pk: &secp256k1::PublicKey,
    sk: &secp256k1::SecretKey,
) -> secp256k1::SecretKey {
    let shared_secret = secp256k1_zkp::ecdh::SharedSecret::new_with_hash(pk, sk, |x, y| {
        // Yes, what follows is the compressed representation of a Bitcoin public key.
        // However, this is more by accident then by design, see here: https://github.com/rust-bitcoin/rust-secp256k1/pull/255#issuecomment-744146282

        let mut dh_secret = [0u8; 33];
        dh_secret[0] = if y.last().unwrap() % 2 == 0 {
            0x02
        } else {
            0x03
        };
        dh_secret[1..].copy_from_slice(&x);

        elements::bitcoin::hashes::sha256d::Hash::hash(&dh_secret)
            .into_inner()
            .into()
    });

    secp256k1::SecretKey::from_slice(&shared_secret.as_ref()[..32])
        .expect("always has exactly 32 bytes")
}

pub fn make_rangeproof_message(
    asset: elements::issuance::AssetId,
    bf: secp256k1_zkp::Tweak,
) -> [u8; 64] {
    let mut message = [0u8; 64];

    message[..32].copy_from_slice(&asset.into_inner());
    message[32..].copy_from_slice(bf.as_ref());

    message
}

#[allow(dead_code)]
pub fn parse_rangeproof_message(
    message: &[u8],
) -> Result<(elements::issuance::AssetId, secp256k1_zkp::Tweak), Error> {
    if message.len() < 64 {
        return Err(Error::Generic("Unexpected rangeproof message".to_string()));
    }
    let asset = elements::issuance::AssetId::from_slice(&message[..32])?;
    let asset_blinder = secp256k1_zkp::Tweak::from_slice(&message[32..64])?;

    Ok((asset, asset_blinder))
}

impl WalletCtx {
    pub fn from_mnemonic(mnemonic: &str, data_root: &str, config: Config) -> Result<Self, Error> {
        let xprv = mnemonic2xprv(mnemonic, config.clone())?;
        let secp = Secp256k1::new();
        let xpub = ExtendedPubKey::from_private(&secp, &xprv);

        let wallet_desc = format!("{}{:?}", xpub, config);
        let wallet_id = hex::encode(sha256::Hash::hash(wallet_desc.as_bytes()));

        let seed = mnemonic2seed(mnemonic)?;
        let master_blinding = MasterBlindingKey::new(&seed);

        let mut path: PathBuf = data_root.into();
        if !path.exists() {
            std::fs::create_dir_all(&path)?;
        }
        path.push(wallet_id);
        info!("Store root path: {:?}", path);
        let store = Arc::new(RwLock::new(StoreMeta::new(&path, xpub)?));

        Ok(WalletCtx {
            store,
            config, // TODO: from db
            secp,
            xpub,
            master_blinding,
            change_max_deriv: 0,
        })
    }

    fn derive_address(
        &self,
        xpub: &ExtendedPubKey,
        path: [u32; 2],
    ) -> Result<elements::Address, Error> {
        let path: Vec<ChildNumber> = path
            .iter()
            .map(|x| ChildNumber::Normal { index: *x })
            .collect();
        let derived = xpub.derive_pub(&self.secp, &path)?;
        let script = p2shwpkh_script(&derived.public_key);
        let blinding_key = self.master_blinding.derive_blinding_key(&script);
        let public_key = secp256k1::PublicKey::from_secret_key(&self.secp, &blinding_key);
        let blinder = Some(public_key);
        let addr = elements::Address::p2shwpkh(
            &derived.public_key,
            blinder,
            address_params(self.config.network()),
        );

        Ok(addr)
    }

    pub fn get_tip(&self) -> Result<(u32, BlockHash), Error> {
        Ok(self.store.read()?.cache.tip)
    }

    pub fn list_tx(&self, opt: &GetTransactionsOpt) -> Result<Vec<TransactionDetails>, Error> {
        let store_read = self.store.read()?;

        let mut txs = vec![];
        let mut my_txids: Vec<(&Txid, &Option<u32>)> = store_read.cache.heights.iter().collect();
        my_txids.sort_by(|a, b| {
            let height_cmp =
                b.1.unwrap_or(std::u32::MAX)
                    .cmp(&a.1.unwrap_or(std::u32::MAX));
            match height_cmp {
                Ordering::Equal => b.0.cmp(a.0),
                h @ _ => h,
            }
        });

        let policy_asset = Some(elements::confidential::Asset::Explicit(
            self.config.policy_asset(),
        ));
        for (tx_id, height) in my_txids.iter().skip(opt.first).take(opt.count) {
            trace!("tx_id {}", tx_id);

            let tx = store_read
                .cache
                .all_txs
                .get(*tx_id)
                .ok_or_else(fn_err(&format!("list_tx no tx {}", tx_id)))?;

            let fee = fee(
                &tx,
                &store_read.cache.all_txs,
                &store_read.cache.unblinded,
                &policy_asset,
            )?;
            trace!("tx_id {} fee {}", tx_id, fee);

            let balances = my_balance_changes(&tx, &store_read.cache.unblinded);
            trace!("tx_id {} balances {:?}", tx_id, balances);

            let spv_verified = if self.config.spv_enabled {
                store_read
                    .cache
                    .txs_verif
                    .get(*tx_id)
                    .unwrap_or(&SPVVerifyResult::InProgress)
                    .clone()
            } else {
                SPVVerifyResult::Disabled
            };

            trace!("tx_id {} spv_verified {:?}", tx_id, spv_verified);

            let tx_details =
                TransactionDetails::new(tx.clone(), balances, fee, **height, spv_verified);

            txs.push(tx_details);
        }
        info!(
            "list_tx {:?}",
            txs.iter().map(|e| &e.txid).collect::<Vec<&String>>()
        );

        Ok(txs)
    }

    pub fn utxos(&self) -> Result<Vec<UnblindedTXO>, Error> {
        info!("start utxos");

        let store_read = self.store.read()?;
        let mut txos = vec![];
        let spent = store_read.spent()?;
        for (tx_id, height) in store_read.cache.heights.iter() {
            let tx = store_read
                .cache
                .all_txs
                .get(tx_id)
                .ok_or_else(fn_err(&format!("txos no tx {}", tx_id)))?;
            let tx_txos: Vec<UnblindedTXO> = {
                let policy_asset = self.config.policy_asset();
                tx.output
                    .clone()
                    .into_iter()
                    .enumerate()
                    .map(|(vout, output)| {
                        (
                            elements::OutPoint {
                                txid: tx.txid(),
                                vout: vout as u32,
                            },
                            output,
                        )
                    })
                    .filter(|(outpoint, _)| !spent.contains(&outpoint))
                    .filter_map(|(outpoint, output)| {
                        if let Some(unblinded) = store_read.cache.unblinded.get(&outpoint) {
                            if unblinded.value < DUST_VALUE && unblinded.asset == policy_asset {
                                return None;
                            }
                            let txo = TXO::new(outpoint, output.script_pubkey, height.clone());
                            return Some(UnblindedTXO {
                                txo: txo,
                                unblinded: unblinded.clone(),
                            });
                        }
                        None
                    })
                    .collect()
            };
            txos.extend(tx_txos);
        }
        txos.sort_by(|a, b| b.unblinded.value.cmp(&a.unblinded.value));

        Ok(txos)
    }

    pub fn balance(&self) -> Result<HashMap<elements::issuance::AssetId, u64>, Error> {
        info!("start balance");
        let mut result = HashMap::new();
        result.entry(self.config.policy_asset()).or_insert(0);
        for u in self.utxos()?.iter() {
            *result.entry(u.unblinded.asset).or_default() += u.unblinded.value;
        }
        Ok(result)
    }

    #[allow(clippy::cognitive_complexity)]
    pub fn create_tx(&self, opt: &mut CreateTransactionOpt) -> Result<TransactionDetails, Error> {
        info!("create_tx {:?}", opt);

        // TODO put checks into CreateTransaction::validate, add check asset are valid asset hex
        // eagerly check for address validity
        let address_params = address_params(self.config.network());
        for address in opt.addressees.iter().map(|a| a.address()) {
            if address.params != address_params {
                return Err(Error::InvalidAddress);
            }
        }

        if opt.addressees.is_empty() {
            return Err(Error::EmptyAddressees);
        }

        if opt.addressees.iter().any(|a| a.satoshi() == 0) {
            return Err(Error::InvalidAmount);
        }

        for address_amount in opt.addressees.iter() {
            if address_amount.satoshi() <= DUST_VALUE {
                if address_amount.asset() == self.config.policy_asset() {
                    // we apply dust rules for liquid bitcoin as elements do
                    return Err(Error::InvalidAmount);
                }
            }
        }

        // convert from satoshi/kbyte to satoshi/byte
        let default_value = 100;
        let fee_rate = (opt.fee_rate.unwrap_or(default_value) as f64) / 1000.0;
        info!("target fee_rate {:?} satoshi/byte", fee_rate);

        let utxos = match &opt.utxos {
            None => self.utxos()?,
            Some(utxos) => utxos.clone(),
        };
        info!("utxos len:{}", utxos.len());

        let mut tx = elements::Transaction {
            version: 2,
            lock_time: 0,
            input: vec![],
            output: vec![],
        };
        // transaction is created in 3 steps:
        // 1) adding requested outputs to tx outputs
        // 2) adding enough utxso to inputs such that tx outputs and estimated fees are covered
        // 3) adding change(s)

        // STEP 1) add the outputs requested for this transactions
        for out in opt.addressees.iter() {
            add_output(&mut tx, &out.address(), out.satoshi(), out.asset().to_hex())
                .map_err(|_| Error::InvalidAddress)?;
        }

        // STEP 2) add utxos until tx outputs are covered (including fees) or fail
        let store_read = self.store.read()?;
        let mut used_utxo: HashSet<elements::OutPoint> = HashSet::new();
        loop {
            let mut needs = needs(
                &tx,
                fee_rate,
                self.config.policy_asset(),
                &store_read.cache.all_txs,
                &store_read.cache.unblinded,
            );
            info!("needs: {:?}", needs);
            if needs.is_empty() {
                // SUCCESS tx doesn't need other inputs
                break;
            }

            let (asset, _) = needs.pop().unwrap(); // safe to unwrap just checked it's not empty

            // taking only utxos of current asset considered, filters also utxos used in this loop
            let mut asset_utxos: Vec<&UnblindedTXO> = utxos
                .iter()
                .filter(|u| u.unblinded.asset == asset && !used_utxo.contains(&u.txo.outpoint))
                .collect();

            // sort by biggest utxo, random maybe another option, but it should be deterministically random (purely random breaks send_all algorithm)
            asset_utxos.sort_by(|a, b| a.unblinded.value.cmp(&b.unblinded.value));
            let utxo = asset_utxos.pop().ok_or(Error::InsufficientFunds)?;

            // Don't spend same script together in liquid. This would allow an attacker
            // to cheaply send assets without value to the target, which will have to
            // waste fees for the extra tx inputs and (eventually) outputs.
            // While blinded address are required and not public knowledge,
            // they are still available to whom transacted with us in the past
            used_utxo.insert(utxo.txo.outpoint.clone());
            add_input(&mut tx, utxo.txo.outpoint.clone());
        }

        // STEP 3) adding change(s)
        let estimated_fee = estimated_fee(
            &tx,
            fee_rate,
            estimated_changes(&tx, &store_read.cache.all_txs, &store_read.cache.unblinded),
        );
        let changes = changes(
            &tx,
            estimated_fee,
            self.config.policy_asset(),
            &store_read.cache.all_txs,
            &store_read.cache.unblinded,
        );
        for (i, (asset, satoshi)) in changes.iter().enumerate() {
            let change_index = store_read.cache.indexes.internal + i as u32 + 1;
            let change_address = self.derive_address(&self.xpub, [1, change_index])?;
            info!(
                "adding change to {} of {} asset {:?}",
                &change_address, satoshi, asset
            );
            add_output(&mut tx, &change_address, *satoshi, asset.to_hex())?;
        }

        // randomize inputs and outputs, BIP69 has been rejected because lacks wallets adoption
        scramble(&mut tx);

        let policy_asset = Some(elements::confidential::Asset::Explicit(
            self.config.policy_asset(),
        ));
        let fee_val = fee(
            &tx,
            &store_read.cache.all_txs,
            &store_read.cache.unblinded,
            &policy_asset,
        )?; // recompute exact fee_val from built tx
        add_fee_output(&mut tx, fee_val, &policy_asset)?;

        info!("created tx fee {:?}", fee_val);

        let mut satoshi = my_balance_changes(&tx, &store_read.cache.unblinded);

        for (_, v) in satoshi.iter_mut() {
            *v = v.abs();
        }

        // Also return changes used?
        Ok(TransactionDetails::new(
            tx,
            satoshi,
            fee_val,
            None,
            SPVVerifyResult::NotVerified,
        ))
    }
    // TODO when we can serialize psbt
    //pub fn sign(&self, psbt: PartiallySignedTransaction) -> Result<PartiallySignedTransaction, Error> { Err(Error::Generic("NotImplemented".to_string())) }

    pub fn internal_sign_elements(
        &self,
        tx: &elements::Transaction,
        input_index: usize,
        derivation_path: &DerivationPath,
        value: Value,
        xprv: ExtendedPrivKey,
        sighash_type: Option<elements::SigHashType>,
    ) -> (Script, Vec<Vec<u8>>) {
        let xprv = xprv.derive_priv(&self.secp, &derivation_path).unwrap();
        let private_key = &xprv.private_key;
        let public_key = &PublicKey::from_private_key(&self.secp, private_key);

        let script_code = p2pkh_script(public_key);
        let sighash_type = sighash_type.unwrap_or(elements::SigHashType::All);
        let sighash = elements::sighash::SigHashCache::new(tx).segwitv0_sighash(
            input_index,
            &script_code,
            value,
            sighash_type,
        );
        let message = secp256k1::Message::from_slice(&sighash[..]).unwrap();
        let signature = self.secp.sign(&message, &private_key.key);
        let mut signature = signature.serialize_der().to_vec();
        signature.push(sighash_type as u8);

        let script_sig = p2shwpkh_script_sig(public_key);
        let witness = vec![signature, public_key.to_bytes()];
        info!(
            "added size len: script_sig:{} witness:{}",
            script_sig.len(),
            witness.iter().map(|v| v.len()).sum::<usize>()
        );
        (script_sig, witness)
    }

    pub fn sign_with_mnemonic(
        &self,
        tx: &mut elements::Transaction,
        mnemonic: &str,
    ) -> Result<(), Error> {
        let xprv = mnemonic2xprv(mnemonic, self.config.clone())?;
        self.sign_with_xprv(tx, xprv)
    }

    pub fn sign_with_xprv(
        &self,
        tx: &mut elements::Transaction,
        xprv: ExtendedPrivKey,
    ) -> Result<(), Error> {
        info!("sign");
        let store_read = self.store.read()?;
        // FIXME: is blinding here the right thing to do?
        self.blind_tx(tx)?;

        for i in 0..tx.input.len() {
            let prev_output = tx.input[i].previous_output;
            info!("input#{} prev_output:{:?}", i, prev_output);
            let prev_tx = store_read
                .cache
                .all_txs
                .get(&prev_output.txid)
                .ok_or_else(|| Error::Generic("expected tx".into()))?;
            let out = prev_tx.output[prev_output.vout as usize].clone();
            let derivation_path: DerivationPath = store_read
                .cache
                .paths
                .get(&out.script_pubkey)
                .ok_or_else(|| Error::Generic("can't find derivation path".into()))?
                .clone();

            let (script_sig, witness) =
                self.internal_sign_elements(&tx, i, &derivation_path, out.value, xprv, None);

            tx.input[i].script_sig = script_sig;
            tx.input[i].witness.script_witness = witness;
        }

        let fee: u64 = tx
            .output
            .iter()
            .filter(|o| o.is_fee())
            .map(|o| o.minimum_value())
            .sum();
        info!(
            "transaction final size is {} bytes and {} vbytes and fee is {}",
            tx.get_size(),
            tx.get_weight() / 4,
            fee
        );
        info!(
            "FINALTX inputs:{} outputs:{}",
            tx.input.len(),
            tx.output.len()
        );
        /*
        drop(store_read);
        let mut store_write = self.store.write()?;

        let changes_used = request.changes_used.unwrap_or(0);
        if changes_used > 0 {
            info!("tx used {} changes", changes_used);
            // The next sync would update the internal index but we increment the internal index also
            // here after sign so that if we immediately create another tx we are not reusing addresses
            // This implies signing multiple times without broadcasting leads to gaps in the internal chain
            store_write.cache.indexes.internal += changes_used;
        }
        */

        Ok(())
    }

    fn blind_tx(&self, tx: &mut elements::Transaction) -> Result<(), Error> {
        // TODO: take a PSET
        let mut pset = elements::pset::PartiallySignedTransaction::from_tx(tx.clone());
        let mut inp_txout_sec: Vec<Option<elements::TxOutSecrets>> = vec![];

        let store_read = self.store.read()?;
        for input in pset.inputs.iter_mut() {
            let previous_output =
                elements::OutPoint::new(input.previous_txid, input.previous_output_index);
            let unblinded = store_read
                .cache
                .unblinded
                .get(&previous_output)
                .ok_or_else(|| Error::Generic("cannot find unblinded values".into()))?;
            inp_txout_sec.push(Some(unblinded.clone()));

            let prev_tx = store_read
                .cache
                .all_txs
                .get(&input.previous_txid)
                .ok_or_else(|| Error::Generic("expected tx".into()))?;
            let txout = prev_tx.output[input.previous_output_index as usize].clone();
            input.witness_utxo = Some(txout);
        }

        for output in pset.outputs.iter_mut() {
            // Elements Core when adding a new confidential output puts the receiver blinding key
            // in the nonce field, then when blinding this is replaced by the sender ephemeral
            // public key (ecdh_pubkey). We do the same in transaction creation. However when
            // creating the PSET from the transaction, the value stored in the nonce field is the
            // receiver blinding key not the ecdh_pubkey, so we swap them.
            std::mem::swap(&mut output.blinding_key, &mut output.ecdh_pubkey);
            // We are the owner of all inputs and outputs
            output.blinder_index = Some(0);
        }

        let inp_txout_sec: Vec<_> = inp_txout_sec.iter().map(|e| e.as_ref()).collect();
        pset.blind_last(&mut rand::thread_rng(), &self.secp, &inp_txout_sec[..])?;
        *tx = pset.extract_tx()?;
        Ok(())
    }

    pub fn get_address(&self) -> Result<elements::Address, Error> {
        let pointer = {
            let store = &mut self.store.write()?.cache;
            store.indexes.external += 1;
            store.indexes.external
        };
        self.derive_address(&self.xpub, [0, pointer])
    }

    pub fn liquidex_assets(&self) -> Result<HashSet<elements::issuance::AssetId>, Error> {
        Ok(self.store.read()?.liquidex_assets())
    }

    pub fn liquidex_assets_insert(
        &self,
        asset: elements::issuance::AssetId,
    ) -> Result<bool, Error> {
        self.store.write()?.liquidex_assets_insert(asset)
    }

    pub fn liquidex_assets_remove(
        &self,
        asset: &elements::issuance::AssetId,
    ) -> Result<bool, Error> {
        self.store.write()?.liquidex_assets_remove(asset)
    }

    pub fn liquidex_make(
        &self,
        opt: &LiquidexMakeOpt,
        mnemonic: &str,
    ) -> Result<LiquidexProposal, Error> {
        let address = self.get_address()?;
        let store_read = self.store.read()?;
        let unblinded_input = store_read
            .cache
            .unblinded
            .get(&opt.utxo)
            .ok_or_else(|| Error::Generic("cannot find unblinded values".into()))?;

        let receive_value = (opt.rate * unblinded_input.value as f64) as u64;
        let mut tx = elements::Transaction {
            version: 2,
            lock_time: 0,
            input: vec![],
            output: vec![],
        };
        add_input(&mut tx, opt.utxo.clone());
        add_output(&mut tx, &address, receive_value, opt.asset_id.to_hex())?;

        let unblinded_output = liquidex_blind(&self.master_blinding, &mut tx, &self.secp)?;

        // FIXME: sign with sighash single || anyonecanpay !!
        let prev_tx = store_read
            .cache
            .all_txs
            .get(&opt.utxo.txid)
            .ok_or_else(|| Error::Generic("expected tx".into()))?;
        let out = prev_tx.output[opt.utxo.vout as usize].clone();
        let derivation_path: DerivationPath = store_read
            .cache
            .paths
            .get(&out.script_pubkey)
            .ok_or_else(|| Error::Generic("can't find derivation path".into()))?
            .clone();

        let xprv = mnemonic2xprv(mnemonic, self.config.clone())?;
        let sighash_type = Some(elements::SigHashType::SinglePlusAnyoneCanPay);
        let (script_sig, witness) =
            self.internal_sign_elements(&tx, 0, &derivation_path, out.value, xprv, sighash_type);

        tx.input[0].script_sig = script_sig;
        tx.input[0].witness.script_witness = witness;

        let proposal = LiquidexProposal::new(&tx, unblinded_input.clone(), unblinded_output);
        Ok(proposal)
    }

    pub fn liquidex_take(
        &self,
        proposal: &LiquidexProposal,
        mnemonic: &str,
    ) -> Result<elements::Transaction, Error> {
        let mut tx = proposal.transaction()?;
        // verify output commitment
        let maker_output = proposal.verify_output_commitment(&self.secp)?;

        // TODO: verify previous output commitment
        let maker_input = proposal.get_input()?;

        let address = self.get_address()?;
        add_output(
            &mut tx,
            &address,
            maker_input.value,
            maker_input.asset.to_hex(),
        )?;

        // satoshi/byte
        let fee_rate = 0.1;

        let utxos = self.utxos()?;

        let store_read = self.store.read()?;
        let mut used_utxo: HashSet<elements::OutPoint> = HashSet::new();
        // If the wallet is taking a proposal made by the wallet itself,
        // do not add the "maker" input again.
        let input_outpoint = tx.input[0].previous_output.clone();
        if utxos.iter().any(|u| u.txo.outpoint == input_outpoint) {
            used_utxo.insert(input_outpoint);
        }
        loop {
            let mut needs = liquidex_needs(
                &maker_input,
                &maker_output,
                &tx,
                fee_rate,
                &self.config.policy_asset(),
                &store_read.cache.unblinded,
            );
            info!("needs: {:?}", needs);
            if needs.is_empty() {
                break;
            }

            let (asset, _) = needs.pop().unwrap(); // safe to unwrap just checked it's not empty

            let mut asset_utxos: Vec<&UnblindedTXO> = utxos
                .iter()
                .filter(|u| u.unblinded.asset == asset && !used_utxo.contains(&u.txo.outpoint))
                .collect();

            info!("asset utxos: {:?}", asset_utxos);
            asset_utxos.sort_by(|a, b| a.unblinded.value.cmp(&b.unblinded.value));
            let utxo = asset_utxos.pop().ok_or(Error::InsufficientFunds)?;

            used_utxo.insert(utxo.txo.outpoint.clone());
            add_input(&mut tx, utxo.txo.outpoint.clone());
        }

        let estimated_fee = estimated_fee(
            &tx,
            fee_rate,
            liquidex_estimated_changes(&maker_input, &tx, &store_read.cache.unblinded),
        );
        let changes = liquidex_changes(
            &maker_input,
            &maker_output,
            &tx,
            estimated_fee,
            &self.config.policy_asset(),
            &store_read.cache.unblinded,
        );
        for (i, (asset, satoshi)) in changes.iter().enumerate() {
            let change_index = store_read.cache.indexes.internal + i as u32 + 1;
            let change_address = self.derive_address(&self.xpub, [1, change_index])?;
            add_output(&mut tx, &change_address, *satoshi, asset.to_hex())?;
        }

        let fee_value = liquidex_fee(
            &maker_input,
            &maker_output,
            &tx,
            &self.config.policy_asset(),
            &store_read.cache.unblinded,
        );

        let fee_output = elements::TxOut {
            asset: Asset::Explicit(self.config.policy_asset()),
            value: Value::Explicit(fee_value),
            ..Default::default()
        };
        tx.output.push(fee_output);

        // Blind tx
        self.liquidex_take_blind(&maker_input, &maker_output, &mut tx)?;
        // Sign inputs
        self.liquidex_take_sign(&mut tx, mnemonic)?;
        Ok(tx)
    }

    fn liquidex_take_blind(
        &self,
        maker_input: &elements::TxOutSecrets,
        maker_output: &elements::TxOutSecrets,
        tx: &mut elements::Transaction,
    ) -> Result<(), Error> {
        let mut input_domain = vec![];
        let mut input_commitment_secrets = vec![];
        let mut output_commitment_secrets = vec![];
        let store_read = self.store.read()?;
        for (idx, input) in tx.input.iter().enumerate() {
            let unblinded = if idx == 0 {
                maker_input
            } else {
                store_read
                    .cache
                    .unblinded
                    .get(&input.previous_output)
                    .ok_or_else(|| Error::Generic("cannot find unblinded values".into()))?
            };

            let asset_tag = secp256k1_zkp::Tag::from(unblinded.asset.into_inner().into_inner());
            let asset_generator = secp256k1_zkp::Generator::new_blinded(
                &self.secp,
                asset_tag,
                unblinded.asset_bf.into_inner(),
            );
            let commitment_secrets = secp256k1_zkp::CommitmentSecrets::new(
                unblinded.value,
                unblinded.value_bf.into_inner(),
                unblinded.asset_bf.into_inner(),
            );
            input_commitment_secrets.push(commitment_secrets);
            input_domain.push((asset_generator, asset_tag, unblinded.asset_bf.into_inner()));
        }

        let ct_exp = 0;
        let ct_bits = 52;

        let out_num = tx.output.len();
        let hash_prevouts = get_hash_prevout(&tx);
        let mut rng = rand::thread_rng();
        for (i, mut output) in tx.output.iter_mut().enumerate() {
            if !output.is_fee() {
                match (i, output.value, output.asset, output.nonce) {
                    (
                        0,
                        Value::Confidential(_),
                        Asset::Confidential(_),
                        Nonce::Confidential(receiver_blinding_pk),
                    ) => {
                        let sender_sk = secp256k1::SecretKey::new(&mut rng);
                        let shared_secret = make_shared_secret(&receiver_blinding_pk, &sender_sk);

                        let asset = maker_output.asset;
                        let asset_blinder = maker_output.asset_bf.into_inner();
                        let value_blinder = maker_output.value_bf.into_inner();
                        let value = maker_output.value;

                        output_commitment_secrets.push(secp256k1_zkp::CommitmentSecrets::new(
                            value,
                            value_blinder,
                            asset_blinder,
                        ));

                        let asset_tag = secp256k1_zkp::Tag::from(asset.into_inner().into_inner());
                        let asset_generator = secp256k1_zkp::Generator::new_blinded(
                            &self.secp,
                            asset_tag,
                            asset_blinder,
                        );
                        let value_commitment = secp256k1_zkp::PedersenCommitment::new(
                            &self.secp,
                            value,
                            value_blinder,
                            asset_generator,
                        );
                        let min_value = if output.script_pubkey.is_provably_unspendable() {
                            0
                        } else {
                            1
                        };

                        let message = make_rangeproof_message(asset, asset_blinder);

                        let rangeproof = secp256k1_zkp::RangeProof::new(
                            &self.secp,
                            min_value,
                            value_commitment,
                            value,
                            value_blinder,
                            &message,
                            &output.script_pubkey.as_bytes(),
                            shared_secret,
                            ct_exp,
                            ct_bits,
                            asset_generator,
                        )?;

                        let surjectionproof = secp256k1_zkp::SurjectionProof::new(
                            &self.secp,
                            &mut rng,
                            asset_tag,
                            asset_blinder,
                            &input_domain,
                        )?;

                        output.witness.surjection_proof = Some(surjectionproof);
                        output.witness.rangeproof = Some(rangeproof);
                    }
                    (
                        _,
                        Value::Explicit(value),
                        Asset::Explicit(asset),
                        Nonce::Confidential(receiver_blinding_pk),
                    ) => {
                        let sender_sk = secp256k1::SecretKey::new(&mut rng);
                        let sender_pk =
                            secp256k1::PublicKey::from_secret_key(&self.secp, &sender_sk);
                        let shared_secret = make_shared_secret(&receiver_blinding_pk, &sender_sk);

                        let asset_blinder =
                            derive_blinder(&self.master_blinding, &hash_prevouts, i as u32, true)?;

                        let value_blinder = if i < out_num - 2 {
                            let value_blinder = derive_blinder(
                                &self.master_blinding,
                                &hash_prevouts,
                                i as u32,
                                false,
                            )?;

                            output_commitment_secrets.push(secp256k1_zkp::CommitmentSecrets::new(
                                value,
                                value_blinder,
                                asset_blinder,
                            ));

                            value_blinder
                        } else {
                            // last value blinder is special and must be set to balance the transaction
                            secp256k1_zkp::compute_adaptive_blinding_factor(
                                &self.secp,
                                value,
                                asset_blinder,
                                &input_commitment_secrets[..],
                                &output_commitment_secrets[..],
                            )
                        };

                        let asset_tag = secp256k1_zkp::Tag::from(asset.into_inner().into_inner());
                        let asset_generator = secp256k1_zkp::Generator::new_blinded(
                            &self.secp,
                            asset_tag,
                            asset_blinder,
                        );
                        let value_commitment = secp256k1_zkp::PedersenCommitment::new(
                            &self.secp,
                            value,
                            value_blinder,
                            asset_generator,
                        );
                        let min_value = if output.script_pubkey.is_provably_unspendable() {
                            0
                        } else {
                            1
                        };

                        let message = make_rangeproof_message(asset, asset_blinder);

                        let rangeproof = secp256k1_zkp::RangeProof::new(
                            &self.secp,
                            min_value,
                            value_commitment,
                            value,
                            value_blinder,
                            &message,
                            &output.script_pubkey.as_bytes(),
                            shared_secret,
                            ct_exp,
                            ct_bits,
                            asset_generator,
                        )?;

                        let surjectionproof = secp256k1_zkp::SurjectionProof::new(
                            &self.secp,
                            &mut rng,
                            asset_tag,
                            asset_blinder,
                            &input_domain,
                        )?;

                        output.nonce =
                            elements::confidential::Nonce::from_commitment(&sender_pk.serialize())?;
                        output.asset = elements::confidential::Asset::from_commitment(
                            &asset_generator.serialize(),
                        )?;
                        output.value = elements::confidential::Value::from_commitment(
                            &value_commitment.serialize(),
                        )?;
                        output.witness.surjection_proof = Some(surjectionproof);
                        output.witness.rangeproof = Some(rangeproof);
                    }
                    _ => panic!("create_tx created things not right"),
                }
            }
        }
        Ok(())
    }

    fn liquidex_take_sign(
        &self,
        tx: &mut elements::Transaction,
        mnemonic: &str,
    ) -> Result<(), Error> {
        let xprv = mnemonic2xprv(mnemonic, self.config.clone())?;
        let store_read = self.store.read()?;

        for i in 1..tx.input.len() {
            let prev_output = tx.input[i].previous_output;
            let prev_tx = store_read
                .cache
                .all_txs
                .get(&prev_output.txid)
                .ok_or_else(|| Error::Generic("expected tx".into()))?;
            let out = prev_tx.output[prev_output.vout as usize].clone();
            let derivation_path: DerivationPath = store_read
                .cache
                .paths
                .get(&out.script_pubkey)
                .ok_or_else(|| Error::Generic("can't find derivation path".into()))?
                .clone();

            let (script_sig, witness) =
                self.internal_sign_elements(&tx, i, &derivation_path, out.value, xprv, None);

            tx.input[i].script_sig = script_sig;
            tx.input[i].witness.script_witness = witness;
        }

        Ok(())
    }
}

fn address_params(net: ElementsNetwork) -> &'static elements::AddressParams {
    match net {
        ElementsNetwork::Liquid => &elements::AddressParams::LIQUID,
        ElementsNetwork::ElementsRegtest => &elements::AddressParams::ELEMENTS,
    }
}

fn get_hash_prevout(tx: &elements::Transaction) -> elements::bitcoin::hashes::sha256d::Hash {
    elements::sighash::SigHashCache::new(tx).hash_prevouts()
}
