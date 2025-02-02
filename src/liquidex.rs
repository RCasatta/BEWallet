use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use aes_gcm_siv::aead::{generic_array::GenericArray, AeadInPlace, NewAead};
use aes_gcm_siv::Aes256GcmSiv;

use rand::Rng;

use elements::bitcoin::hashes::{sha256, sha256d, Hash};
use elements::confidential::{Asset, Nonce, Value};
use elements::encode::Encodable;
use elements::secp256k1_zkp::{self, All, Secp256k1};
use elements::slip77::MasterBlindingKey;

use crate::error::Error;
use crate::transaction::{estimated_fee, DUST_VALUE};
use crate::utils::derive_blinder;

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct LiquidexMakeOpt {
    pub utxo: elements::OutPoint,
    pub asset_id: elements::issuance::AssetId,
    pub rate: f64,
}

impl LiquidexMakeOpt {
    pub fn new(txid: &str, vout: u32, asset_id: &str, rate: f64) -> Result<Self, Error> {
        let txid = elements::Txid::from_str(txid)?;
        let utxo = elements::OutPoint::new(txid, vout);
        let asset_id = elements::issuance::AssetId::from_str(asset_id)?;
        Ok(Self {
            utxo,
            asset_id,
            rate,
        })
    }
}

// Clone of TxOutSecrets, but with the name changed to match the previous struct.
// This is a temporary solution since soon we should be able to migrate to PSET.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct LiquidexTxOutSecrets {
    asset: elements::AssetId,
    asset_blinder: elements::confidential::AssetBlindingFactor,
    amount: u64,
    amount_blinder: elements::confidential::ValueBlindingFactor,
}

impl LiquidexTxOutSecrets {
    pub fn to_txoutsecrets(&self) -> elements::TxOutSecrets {
        elements::TxOutSecrets {
            asset: self.asset,
            asset_bf: self.asset_blinder,
            value: self.amount,
            value_bf: self.amount_blinder,
        }
    }
}

impl From<elements::TxOutSecrets> for LiquidexTxOutSecrets {
    fn from(txoutsecrets: elements::TxOutSecrets) -> Self {
        Self {
            asset: txoutsecrets.asset,
            asset_blinder: txoutsecrets.asset_bf,
            amount: txoutsecrets.value,
            amount_blinder: txoutsecrets.value_bf,
        }
    }
}

// TODO: use serde with to make tx a elements::Transaction
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct LiquidexProposal {
    #[serde(default)]
    version: u32,
    tx: String,
    inputs: Vec<LiquidexTxOutSecrets>,
    outputs: Vec<LiquidexTxOutSecrets>,
}

impl LiquidexProposal {
    pub fn new(
        tx: &elements::Transaction,
        input: elements::TxOutSecrets,
        output: elements::TxOutSecrets,
    ) -> Self {
        Self {
            version: 0,
            tx: hex::encode(elements::encode::serialize(tx)),
            inputs: vec![input.into()],
            outputs: vec![output.into()],
        }
    }

    pub fn transaction(&self) -> Result<elements::Transaction, Error> {
        Ok(elements::encode::deserialize(&hex::decode(
            self.tx.clone(),
        )?)?)
    }

    pub fn get_input(&self) -> Result<elements::TxOutSecrets, Error> {
        if self.inputs.len() != 1 {
            return Err(Error::Generic(
                "LiquiDEX error unexpected inputs".to_string(),
            ));
        }

        Ok(self.inputs[0].to_txoutsecrets().clone())
    }

    pub fn verify_output_commitment(
        &self,
        secp: &Secp256k1<All>,
    ) -> Result<elements::TxOutSecrets, Error> {
        let tx = self.transaction()?;
        if tx.input.len() != 1
            || tx.output.len() != 1
            || self.inputs.len() != 1
            || self.outputs.len() != 1
        {
            return Err(Error::Generic("LiquiDEX error".to_string()));
        }

        let output = self.outputs[0].to_txoutsecrets();

        // check output is blinded
        let (tx_asset_generator, tx_value_commitment) =
            match (tx.output[0].asset, tx.output[0].value) {
                (Asset::Confidential(generator), Value::Confidential(pedersen_commitment)) => {
                    (generator, pedersen_commitment)
                }
                _ => {
                    return Err(Error::Generic(
                        "LiquiDEX error unexpected outputs".to_string(),
                    ));
                }
            };

        let asset_tag = secp256k1_zkp::Tag::from(output.asset.into_inner().into_inner());
        let asset_generator =
            secp256k1_zkp::Generator::new_blinded(secp, asset_tag, output.asset_bf.into_inner());
        let value_commitment = secp256k1_zkp::PedersenCommitment::new(
            secp,
            output.value,
            output.value_bf.into_inner(),
            asset_generator,
        );

        if asset_generator != tx_asset_generator || value_commitment != tx_value_commitment {
            return Err(Error::Generic(
                "LiquiDEX error unexpected commitments".to_string(),
            ));
        }

        Ok(output)
    }
}

fn _liquidex_derive_blinder(
    master_blinding_key: &MasterBlindingKey,
    previous_outpoint: &elements::OutPoint,
    is_asset_blinder: bool,
) -> Result<secp256k1_zkp::Tweak, secp256k1_zkp::Error> {
    // LiquiDEX proposals do not know in advance all inputs of
    // final transaction, compute the hash only from the previous
    // outpoint we know is being spent.
    let hash_prevout = {
        let mut enc = sha256d::Hash::engine();
        previous_outpoint.consensus_encode(&mut enc).unwrap();
        sha256d::Hash::from_engine(enc)
    };

    // LiquiDEX proposals output vout is choosen by the taker,
    // for the blinder computation use a vout that may not
    // occur in a transaction.
    derive_blinder(
        master_blinding_key,
        &hash_prevout,
        u32::MAX,
        is_asset_blinder,
    )
}

fn liquidex_derive_asset_blinder(
    master_blinding_key: &MasterBlindingKey,
    previous_outpoint: &elements::OutPoint,
) -> Result<elements::confidential::AssetBlindingFactor, Error> {
    let blinder = _liquidex_derive_blinder(master_blinding_key, previous_outpoint, true)?;
    elements::confidential::AssetBlindingFactor::from_slice(&blinder[..]).map_err(Into::into)
}

fn liquidex_derive_value_blinder(
    master_blinding_key: &MasterBlindingKey,
    previous_outpoint: &elements::OutPoint,
) -> Result<elements::confidential::ValueBlindingFactor, Error> {
    let blinder = _liquidex_derive_blinder(master_blinding_key, previous_outpoint, false)?;
    elements::confidential::ValueBlindingFactor::from_slice(&blinder[..]).map_err(Into::into)
}

fn _liquidex_aes_key(
    master_blinding_key: &MasterBlindingKey,
    script: &elements::Script,
) -> Result<[u8; 32], Error> {
    // TODO: consider using tagged hashes
    const TAG: &[u8; 16] = b"liquidex_aes_key";
    let mut engine = sha256::Hash::engine();
    engine.write(TAG)?;
    engine.write(&master_blinding_key.0[..])?;
    engine.write(&script.as_bytes())?;
    Ok(sha256::Hash::from_engine(engine).into_inner())
}

fn _liquidex_aes_nonce(
    master_blinding_key: &MasterBlindingKey,
    previous_outpoint: &elements::OutPoint,
    asset: &elements::confidential::Asset,
    value: &elements::confidential::Value,
    script: &elements::Script,
) -> Result<[u8; 12], Error> {
    match (asset, value) {
        (Asset::Confidential(_), Value::Confidential(_)) => {}
        _ => {
            return Err(Error::Generic(
                "Asset and Value must be confidential".to_string(),
            ));
        }
    }
    // TODO: consider using tagged hashes
    const TAG: &[u8; 18] = b"liquidex_aes_nonce";
    let mut engine = sha256::Hash::engine();
    engine.write(TAG)?;
    engine.write(&master_blinding_key.0[..])?;
    previous_outpoint.consensus_encode(&mut engine)?;
    engine.write(&asset.commitment().unwrap().serialize())?;
    engine.write(&value.commitment().unwrap().serialize())?;
    engine.write(&script.as_bytes())?;
    let mut out = [0u8; 12];
    out.copy_from_slice(&sha256::Hash::from_engine(engine).into_inner()[..12]);
    Ok(out)
}

/// Blind a LiquiDEX maker transaction.
/// The maker has no control on the rangeproof, thus it can't rely on it to recover the unblinding
/// data. Use deterministic blinders and use the nonce field to encrypt the output value.
pub fn liquidex_blind(
    master_blinding_key: &MasterBlindingKey,
    tx: &mut elements::Transaction,
    secp: &Secp256k1<All>,
) -> Result<elements::TxOutSecrets, Error> {
    if tx.input.len() != 1 || tx.output.len() != 1 {
        return Err(Error::Generic(
            "Unexpected LiquiDEX maker transaction num in/out".to_string(),
        ));
    }
    let (asset, value) = match (tx.output[0].asset, tx.output[0].value, tx.output[0].nonce) {
        //(Asset::Explicit(asset), Value::Explicit(value), Nonce::Null) => (asset, value),
        (Asset::Explicit(asset), Value::Explicit(value), _) => (asset, value),
        _ => {
            return Err(Error::Generic(
                "Unexpected LiquiDEX maker transaction".to_string(),
            ));
        }
    };

    let asset_blinder =
        liquidex_derive_asset_blinder(master_blinding_key, &tx.input[0].previous_output)?;
    let value_blinder =
        liquidex_derive_value_blinder(master_blinding_key, &tx.input[0].previous_output)?;

    let asset_tag = secp256k1_zkp::Tag::from(asset.into_inner().into_inner());
    let asset_generator =
        secp256k1_zkp::Generator::new_blinded(secp, asset_tag, asset_blinder.into_inner());
    let value_commitment = secp256k1_zkp::PedersenCommitment::new(
        secp,
        value,
        value_blinder.into_inner(),
        asset_generator,
    );

    tx.output[0].asset = Asset::from_commitment(&asset_generator.serialize())?;
    tx.output[0].value = Value::from_commitment(&value_commitment.serialize())?;

    let key = _liquidex_aes_key(master_blinding_key, &tx.output[0].script_pubkey)?;
    let key = GenericArray::from_slice(&key);
    let cipher = Aes256GcmSiv::new(&key);

    let aes_nonce = _liquidex_aes_nonce(
        master_blinding_key,
        &tx.input[0].previous_output,
        &tx.output[0].asset,
        &tx.output[0].value,
        &tx.output[0].script_pubkey,
    )?;
    let aes_nonce = GenericArray::from_slice(&aes_nonce);

    let mut rng = rand::thread_rng();
    let nonce_commitment = loop {
        // On average does 2 loops.
        let mut text = [0u8; 16];
        text[..8].copy_from_slice(&value.to_le_bytes());
        rng.fill(&mut text[8..]);
        let mut text = text.to_vec();
        cipher.encrypt_in_place(aes_nonce, b"", &mut text)?;
        let mut candidate = [0u8; 33];
        candidate[0] = 0x02;
        candidate[1..].copy_from_slice(&text);
        if let Ok(pk) = secp256k1_zkp::PublicKey::from_slice(&candidate) {
            break pk.serialize();
        }
    };

    tx.output[0].nonce = elements::confidential::Nonce::from_commitment(&nonce_commitment)?;

    Ok(elements::TxOutSecrets::new(
        asset,
        asset_blinder,
        value,
        value_blinder,
    ))
}

pub fn liquidex_unblind(
    master_blinding_key: &MasterBlindingKey,
    tx: &elements::Transaction,
    vout: u32,
    secp: &Secp256k1<All>,
    assets: &HashSet<elements::issuance::AssetId>,
) -> Result<elements::TxOutSecrets, Error> {
    // check vout is reasonable
    let vout = vout as usize;
    if vout + 1 > tx.output.len() || vout + 1 > tx.input.len() {
        return Err(Error::Generic("LiquiDEX error 1".to_string()));
    }
    // check output is blinded
    match (
        tx.output[vout].asset,
        tx.output[vout].value,
        tx.output[vout].nonce,
    ) {
        (Asset::Confidential(_), Value::Confidential(_), Nonce::Confidential(_)) => {}
        _ => {
            return Err(Error::Generic("LiquiDEX error 2".to_string()));
        }
    }
    // FIXME: check input has sighash single | anyonecanpay
    // FIXME: check input has a script belonging to the wallet
    // compute blinders
    let asset_blinder =
        liquidex_derive_asset_blinder(master_blinding_key, &tx.input[vout].previous_output)?;
    let value_blinder =
        liquidex_derive_value_blinder(master_blinding_key, &tx.input[vout].previous_output)?;

    // compute key
    let key = _liquidex_aes_key(master_blinding_key, &tx.output[vout].script_pubkey)?;
    let key = GenericArray::from_slice(&key);
    let cipher = Aes256GcmSiv::new(&key);

    // compute aes nonce
    let aes_nonce = _liquidex_aes_nonce(
        master_blinding_key,
        &tx.input[vout].previous_output,
        &tx.output[vout].asset,
        &tx.output[vout].value,
        &tx.output[vout].script_pubkey,
    )?;
    let aes_nonce = GenericArray::from_slice(&aes_nonce);

    // parse nonce_commitment
    let nonce_commitment = tx.output[vout].nonce.commitment().unwrap().serialize();
    let mut text = vec![];
    text.extend(&nonce_commitment[1..]);

    // decrypt value
    cipher.decrypt_in_place(aes_nonce, b"", &mut text)?;
    let mut value_bytes = [0u8; 8];
    value_bytes.copy_from_slice(&text[..8]);
    let value = u64::from_le_bytes(value_bytes);

    // check value matches value commitment
    let tx_asset_generator = tx.output[vout].asset.commitment().unwrap();
    let tx_value_commitment = tx.output[vout].value.commitment().unwrap();
    let value_commitment = secp256k1_zkp::PedersenCommitment::new(
        secp,
        value,
        value_blinder.into_inner(),
        tx_asset_generator,
    );
    if value_commitment != tx_value_commitment {
        return Err(Error::Generic(
            "LiquiDEX error value commitment".to_string(),
        ));
    }

    let mut asset: Option<elements::issuance::AssetId> = None;
    // loop on assets
    for candidate in assets {
        // check asset matches asset commitment
        let asset_tag = secp256k1_zkp::Tag::from(candidate.into_inner().into_inner());
        let asset_generator =
            secp256k1_zkp::Generator::new_blinded(secp, asset_tag, asset_blinder.into_inner());
        if asset_generator == tx_asset_generator {
            asset = Some(candidate.clone());
            break;
        }
    }

    // check a match happened
    if asset.is_none() {
        return Err(Error::Generic("LiquiDEX error asset not found".to_string()));
    }
    let asset = asset.unwrap();

    // return unblinded
    Ok(elements::TxOutSecrets::new(
        asset,
        asset_blinder,
        value,
        value_blinder,
    ))
}

fn outputs(
    maker_output: &elements::TxOutSecrets,
    tx: &elements::Transaction,
) -> HashMap<elements::issuance::AssetId, u64> {
    let mut outputs: HashMap<elements::issuance::AssetId, u64> = HashMap::new();
    for (idx, output) in tx.output.iter().enumerate() {
        if idx == 0 {
            *outputs.entry(maker_output.asset).or_insert(0) += maker_output.value;
        } else {
            match (output.asset, output.value) {
                (Asset::Explicit(asset), Value::Explicit(value)) => {
                    *outputs.entry(asset).or_insert(0) += value;
                }
                _ => panic!("asset and value should be explicit here"),
            }
        }
    }
    outputs
}

fn inputs(
    maker_input: &elements::TxOutSecrets,
    tx: &elements::Transaction,
    unblinded: &HashMap<elements::OutPoint, elements::TxOutSecrets>,
) -> HashMap<elements::issuance::AssetId, u64> {
    let mut inputs: HashMap<elements::issuance::AssetId, u64> = HashMap::new();
    for (idx, input) in tx.input.iter().enumerate() {
        if idx == 0 {
            *inputs.entry(maker_input.asset).or_insert(0) += maker_input.value;
        } else {
            let unblinded = unblinded.get(&input.previous_output).unwrap();
            *inputs.entry(unblinded.asset).or_insert(0) += unblinded.value;
        }
    }
    inputs
}

pub fn liquidex_needs(
    maker_input: &elements::TxOutSecrets,
    maker_output: &elements::TxOutSecrets,
    tx: &elements::Transaction,
    fee_rate: f64,
    policy_asset: &elements::issuance::AssetId,
    unblinded: &HashMap<elements::OutPoint, elements::TxOutSecrets>,
) -> Vec<(elements::issuance::AssetId, u64)> {
    let mut outputs = outputs(maker_output, tx);
    let mut inputs = inputs(maker_input, tx, unblinded);
    let estimated_fee = estimated_fee(
        &tx,
        fee_rate,
        liquidex_estimated_changes(maker_input, &tx, unblinded),
    );
    *outputs.entry(policy_asset.clone()).or_insert(0) += estimated_fee;

    let mut result = vec![];
    for (asset, value) in outputs.iter() {
        if let Some(sum) = value.checked_sub(inputs.remove(asset).unwrap_or(0)) {
            if sum > 0 {
                result.push((*asset, sum));
            }
        }
    }

    result
}

pub fn liquidex_estimated_changes(
    maker_input: &elements::TxOutSecrets,
    tx: &elements::Transaction,
    unblinded: &HashMap<elements::OutPoint, elements::TxOutSecrets>,
) -> u8 {
    inputs(maker_input, tx, unblinded).len() as u8
}

pub fn liquidex_changes(
    maker_input: &elements::TxOutSecrets,
    maker_output: &elements::TxOutSecrets,
    tx: &elements::Transaction,
    estimated_fee: u64,
    policy_asset: &elements::issuance::AssetId,
    unblinded: &HashMap<elements::OutPoint, elements::TxOutSecrets>,
) -> HashMap<elements::issuance::AssetId, u64> {
    let mut outputs_asset_amounts = outputs(maker_output, tx);
    let inputs_asset_amounts = inputs(maker_input, tx, unblinded);
    let mut result: HashMap<elements::issuance::AssetId, u64> = HashMap::new();
    for (asset, value) in inputs_asset_amounts.iter() {
        let mut sum: u64 = value - outputs_asset_amounts.remove(asset).unwrap_or(0);
        if asset == policy_asset {
            // from a purely privacy perspective could make sense to always create the change output in liquid, so min change = 0
            // however elements core use the dust anyway for 2 reasons: rebasing from core and economical considerations
            sum -= estimated_fee;
            if sum > DUST_VALUE {
                // we apply dust rules for liquid bitcoin as elements do
                result.insert(*asset, sum);
            }
        } else if sum > 0 {
            result.insert(*asset, sum);
        }
    }
    assert!(outputs_asset_amounts.is_empty());
    result
}

pub fn liquidex_fee(
    maker_input: &elements::TxOutSecrets,
    maker_output: &elements::TxOutSecrets,
    tx: &elements::Transaction,
    policy_asset: &elements::issuance::AssetId,
    unblinded: &HashMap<elements::OutPoint, elements::TxOutSecrets>,
) -> u64 {
    assert!(!tx.output.iter().any(|o| o.is_fee()));
    let outputs = outputs(maker_output, tx);
    let inputs = inputs(maker_input, tx, unblinded);
    inputs.get(policy_asset).unwrap() - outputs.get(policy_asset).unwrap()
}

#[cfg(test)]
mod tests {
    use crate::liquidex::{liquidex_blind, liquidex_unblind, LiquidexProposal};
    use crate::transaction::add_input;

    #[test]
    fn test_liquidex_roundtrip() {
        assert_eq!(2, 2);
        let seed = [0u8; 32];
        let master_blinding_key = elements::slip77::MasterBlindingKey::new(&seed);
        let mut tx = elements::Transaction {
            version: 2,
            lock_time: 0,
            input: vec![],
            output: vec![],
        };
        // add input
        let outpoint = elements::OutPoint::new(tx.txid(), 0);
        add_input(&mut tx, outpoint);
        // add output
        let asset = [1u8; 32];
        let asset = elements::issuance::AssetId::from_slice(&asset).unwrap();
        let value = 10;
        let script = elements::Script::from(vec![0x51]);
        let new_out = elements::TxOut {
            asset: elements::confidential::Asset::Explicit(asset),
            value: elements::confidential::Value::Explicit(value),
            nonce: elements::confidential::Nonce::Null,
            script_pubkey: script,
            witness: elements::TxOutWitness::default(),
        };
        tx.output.push(new_out);

        let secp = elements::secp256k1_zkp::Secp256k1::new();
        liquidex_blind(&master_blinding_key, &mut tx, &secp).unwrap();
        let mut assets = std::collections::HashSet::<elements::issuance::AssetId>::new();
        assets.insert(asset.clone());
        let unblinded = liquidex_unblind(&master_blinding_key, &tx, 0, &secp, &assets).unwrap();
        assert_eq!(unblinded.asset, asset);
        assert_eq!(unblinded.value, value);
    }

    #[test]
    fn test_liquidex_proposal() {
        // Taken proposal:
        // https://blockstream.info/liquid/tx/a43dafc00a6c488085bdf849ca954e4a82f80d56a1c8931873df83d5d22981a4
        let proposal_str = r#"
        {
            "tx": "020000000101071c86c2e1eff6245e3589dce4f98df081256f7143b20a71d1a11081f234808f01000000171600140b22d358af49422e133684f57d0eb49a9fca84e0ffffffff010a39e73aac4854ce1a1d0ec397db58ec6ce018413f6886abdcaaea3244cc2f803c099380bc1c9039e82a27df4217d54d8f107b8868ad5a947b802a4bfe48134fc6d2028e9004696ef308f97994ebe47294e5fa4273479f7e1a779f581a70f17f7b35be17a914f69b2673d97b6bdf04bbfee2afdf26056de39450870000000000000247304402201a3a6b57b7c70e8efbffd59c4b1e2402448436d97beb37fedc81897eade4f3f702202cce73b837719ac7d332aef7f9b2d7412ffbeffb677635458dc745b3190822bc83210249c7906961ac155d2a7f60429a4c8e90cc7b1857be5c7cb5c2f5fb736e3df8a4000000",
            "inputs": [{
                "asset": "8026fa969633b7b6f504f99dde71335d633b43d18314c501055fcd88b9fcb8de",
                "amount": 175000000,
                "asset_blinder": "e9fe8ff23076c01fe0e5b545807c01157c99501288d9479bfb7e7d24feba694d",
                "amount_blinder": "6a80b9e7b887bdde8f23ebe48b307d9516259591681d71d376fb290b13df1674"
            }],
            "outputs": [{
                "asset": "f638b720fe531bbba23a71495aebf55592f45adc6c89f00de38303f60c7b51d7",
                "amount": 175,
                "asset_blinder": "07b4a065649a9f57e07dba6d87672f5e9d617bca0b8593da593ec77eec746b9c",
                "amount_blinder": "216f304aaadd2b62b81ac4d6ebc219b4d6b9b61611cf2103ab377944c9b69ae8"
            }]
        }"#;

        let proposal: LiquidexProposal = serde_json::from_str(proposal_str).unwrap();
        println!("{:#?}", proposal);
        assert_eq!(proposal.outputs[0].amount, 175);

        // verify commitments matches the tx output and that the blinder are deserialized correctly
        let secp = elements::secp256k1_zkp::Secp256k1::new();
        proposal.verify_output_commitment(&secp).unwrap();

        // verify that the serialized proposal matches the deserialized one
        let proposal_str2 = serde_json::to_string(&proposal).unwrap();
        let proposal2: LiquidexProposal = serde_json::from_str(&proposal_str2).unwrap();
        assert_eq!(proposal, proposal2);
    }
}
