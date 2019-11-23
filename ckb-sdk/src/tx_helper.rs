use ckb_hash::{blake2b_256, new_blake2b};
use ckb_types::{
    bytes::Bytes,
    core::{ScriptHashType, TransactionBuilder, TransactionView},
    packed::{self, Byte32, CellInput, CellOutput, OutPoint, Script, WitnessArgs},
    prelude::*,
    H160, H256,
};
use std::collections::{HashMap, HashSet};

use crate::constants::{MULTISIG_TYPE_HASH, SECP_SIGNATURE_SIZE, SIGHASH_TYPE_HASH};
use crate::{AddressPayload, AddressType, CodeHashIndex, GenesisInfo, Since};

// TODO: Add dao support

/// A transaction helper handle input/output with secp256k1(sighash/multisg) lock
///  1. Sign transaction
///  2. Inspect transaction information
#[derive(Clone)]
pub struct TxHelper {
    transaction: TransactionView,
    multisig_configs: HashMap<H160, MultisigConfig>,
    // Only support sighash/multisig signatures
    signatures: HashMap<Bytes, HashSet<Bytes>>,
}

impl Default for TxHelper {
    fn default() -> TxHelper {
        TxHelper {
            transaction: TransactionBuilder::default().build(),
            multisig_configs: HashMap::default(),
            signatures: HashMap::default(),
        }
    }
}

impl TxHelper {
    pub fn new(transaction: TransactionView) -> TxHelper {
        TxHelper {
            transaction,
            multisig_configs: HashMap::default(),
            signatures: HashMap::default(),
        }
    }

    pub fn transaction(&self) -> &TransactionView {
        &self.transaction
    }
    pub fn multisig_configs(&self) -> &HashMap<H160, MultisigConfig> {
        &self.multisig_configs
    }
    pub fn signatures(&self) -> &HashMap<Bytes, HashSet<Bytes>> {
        &self.signatures
    }

    pub fn clear_inputs(&mut self) {
        self.transaction = self
            .transaction
            .as_advanced_builder()
            .set_inputs(Vec::new())
            .build();
    }
    pub fn clear_outputs(&mut self) {
        self.transaction = self
            .transaction
            .as_advanced_builder()
            .set_outputs(Vec::new())
            .set_outputs_data(Vec::new())
            .build();
    }
    pub fn clear_signatures(&mut self) {
        self.signatures.clear();
    }
    pub fn clear_multisig_configs(&mut self) {
        self.multisig_configs.clear()
    }

    pub fn add_input<F: FnMut(OutPoint, bool) -> Result<CellOutput, String>>(
        &mut self,
        out_point: OutPoint,
        since_absolute_epoch_opt: Option<u64>,
        mut get_live_cell: F,
        genesis_info: &GenesisInfo,
    ) -> Result<(), String> {
        let mut since_value_opt =
            since_absolute_epoch_opt.map(|number| Since::new_absolute_epoch(number).value());

        let lock = get_live_cell(out_point.clone(), false)?.lock();
        check_lock_script(&lock)?;
        if since_value_opt.is_none() {
            let lock_arg = lock.args().raw_data();
            if lock.code_hash() == MULTISIG_TYPE_HASH.pack() && lock_arg.len() == 28 {
                let mut since_bytes = [0u8; 8];
                since_bytes.copy_from_slice(&lock_arg[20..]);
                let since_value = u64::from_le_bytes(since_bytes);
                since_value_opt = Some(since_value);
            }
        }
        let since = since_value_opt.unwrap_or(0);
        let input = CellInput::new_builder()
            .previous_output(out_point)
            .since(since.pack())
            .build();

        self.transaction = self.transaction.as_advanced_builder().input(input).build();
        let mut cell_deps = Vec::new();
        for ((code_hash, _), _) in self.input_group(get_live_cell)?.into_iter() {
            let code_hash: H256 = code_hash.unpack();
            if code_hash == SIGHASH_TYPE_HASH {
                cell_deps.push(genesis_info.sighash_dep());
            } else if code_hash == MULTISIG_TYPE_HASH {
                cell_deps.push(genesis_info.multisig_dep());
            } else {
                panic!("Unexpected input code_hash: {:#x}", code_hash);
            }
        }
        self.transaction = self
            .transaction
            .as_advanced_builder()
            .set_cell_deps(cell_deps)
            .build();
        Ok(())
    }

    pub fn add_output(&mut self, output: CellOutput, data: Bytes) {
        // TODO: Check output(lock-script/type-script)
        self.transaction = self
            .transaction
            .as_advanced_builder()
            .output(output)
            .output_data(data.pack())
            .build()
    }

    pub fn add_signature(&mut self, lock_arg: Bytes, signature: Bytes) -> Result<bool, String> {
        if lock_arg.len() != 20 && lock_arg.len() != 28 {
            return Err(format!(
                "Invalid lock_arg(0x{}) length({}) with signature(0x{})",
                hex_string(lock_arg.as_ref()).unwrap(),
                lock_arg.len(),
                hex_string(signature.as_ref()).unwrap(),
            ));
        }
        if signature.len() != SECP_SIGNATURE_SIZE {
            return Err(format!(
                "Invalid signature length({}) for lock_arg(0x{})",
                signature.len(),
                hex_string(lock_arg.as_ref()).unwrap(),
            ));
        }

        Ok(self
            .signatures
            .entry(lock_arg)
            .or_default()
            .insert(signature))
    }
    pub fn add_multisig_config(&mut self, config: MultisigConfig) {
        self.multisig_configs.insert(config.hash160(), config);
    }

    pub fn input_group<F: FnMut(OutPoint, bool) -> Result<CellOutput, String>>(
        &self,
        mut get_live_cell: F,
    ) -> Result<HashMap<(Byte32, Bytes), Vec<usize>>, String> {
        let mut input_group: HashMap<(Byte32, Bytes), Vec<usize>> = HashMap::default();
        for (idx, input) in self.transaction.inputs().into_iter().enumerate() {
            let lock = get_live_cell(input.previous_output(), false)?.lock();
            check_lock_script(&lock).map_err(|err| format!("Input(no.{}) {}", idx + 1, err))?;

            let lock_arg = lock.args().raw_data();
            let code_hash = lock.code_hash();
            let hash160 = H160::from_slice(&lock_arg[..20]).unwrap();
            if code_hash == MULTISIG_TYPE_HASH.pack()
                && !self.multisig_configs.contains_key(&hash160)
            {
                return Err(format!(
                    "Invalid input(no.{}) lock script args prefix: 0x{}, expected: {:#x}",
                    idx + 1,
                    hex_string(&lock_arg[..20]).unwrap(),
                    hash160,
                ));
            }
            input_group
                .entry((code_hash, lock_arg))
                .or_default()
                .push(idx);
        }
        Ok(input_group)
    }

    pub fn init_witnesses(&self) -> Vec<packed::Bytes> {
        let mut witnesses: Vec<packed::Bytes> = self.transaction.witnesses().into_iter().collect();
        while witnesses.len() < self.transaction.inputs().len() {
            witnesses.push(Bytes::new().pack());
        }
        witnesses
    }

    pub fn sign_tx<S, C>(
        &self,
        signer_lock_arg: &H160,
        mut signer: S,
        get_live_cell: C,
    ) -> Result<HashMap<Bytes, HashSet<Bytes>>, String>
    where
        S: FnMut(&H256) -> Result<[u8; SECP_SIGNATURE_SIZE], String>,
        C: FnMut(OutPoint, bool) -> Result<CellOutput, String>,
    {
        let all_sighash_lock_args = self
            .multisig_configs
            .iter()
            .map(|(hash160, config)| (hash160.clone(), config.sighash_lock_args()))
            .collect::<HashMap<_, _>>();

        let witnesses = self.init_witnesses();
        let mut signatures: HashMap<Bytes, HashSet<_>> = Default::default();
        for ((code_hash, lock_arg), idxs) in self.input_group(get_live_cell)?.into_iter() {
            if code_hash == SIGHASH_TYPE_HASH.pack()
                && lock_arg.as_ref() != signer_lock_arg.as_bytes()
            {
                continue;
            }

            let hash160 = H160::from_slice(&lock_arg[..20]).unwrap();
            if code_hash == MULTISIG_TYPE_HASH.pack()
                && !all_sighash_lock_args
                    .get(&hash160)
                    .unwrap()
                    .contains(signer_lock_arg)
            {
                continue;
            }

            let multisig_config_opt = if code_hash == MULTISIG_TYPE_HASH.pack() {
                Some(self.multisig_configs.get(&hash160).unwrap())
            } else {
                None
            };
            let signature = build_signature(
                &self.transaction.hash(),
                &idxs,
                &witnesses,
                multisig_config_opt,
                &mut signer,
            )?;
            signatures.entry(lock_arg).or_default().insert(signature);
        }
        Ok(signatures)
    }

    pub fn build_tx<F: FnMut(OutPoint, bool) -> Result<CellOutput, String>>(
        &self,
        get_live_cell: F,
    ) -> Result<TransactionView, String> {
        let mut witnesses = self.init_witnesses();
        for ((code_hash, lock_arg), idxs) in self.input_group(get_live_cell)?.into_iter() {
            let signatures = self.signatures.get(&lock_arg).ok_or_else(|| {
                format!(
                    "Missing signatures for lock_arg: 0x{}",
                    hex_string(&lock_arg).unwrap()
                )
            })?;
            let lock_field = if code_hash == MULTISIG_TYPE_HASH.pack() {
                let hash160 = H160::from_slice(&lock_arg[..20]).unwrap();
                let multisig_config = self.multisig_configs.get(&hash160).unwrap();
                let threshold = multisig_config.threshold() as usize;
                let mut data = multisig_config.to_witness_data();
                if signatures.len() != threshold {
                    return Err(format!(
                        "Invalid multisig signature length for lock_arg: 0x{}, got: {}, expected: {}",
                        hex_string(&lock_arg).unwrap(),
                        signatures.len(),
                        threshold,
                    ));
                }
                for signature in signatures {
                    data.extend_from_slice(signature.as_ref());
                }
                data
            } else {
                if signatures.len() != 1 {
                    return Err(format!(
                        "Invalid secp signature length for lock_arg: 0x{}, got: {}, expected: 1",
                        hex_string(&lock_arg).unwrap(),
                        signatures.len(),
                    ));
                }
                signatures.iter().last().unwrap().clone()
            };

            let init_witness = if witnesses[idxs[0]].raw_data().is_empty() {
                WitnessArgs::default()
            } else {
                WitnessArgs::from_slice(witnesses[idxs[0]].raw_data().as_ref())
                    .map_err(|err| err.to_string())?
            };
            witnesses[idxs[0]] = init_witness
                .as_builder()
                .lock(Some(lock_field).pack())
                .build()
                .as_bytes()
                .pack();
        }
        Ok(self
            .transaction
            .as_advanced_builder()
            .set_witnesses(witnesses)
            .build())
    }

    pub fn check_tx<F: FnMut(OutPoint, bool) -> Result<CellOutput, String>>(
        &self,
        mut get_live_cell: F,
    ) -> Result<(u64, u64), String> {
        // Check inputs
        let mut previous_outputs: HashSet<OutPoint> = HashSet::default();
        let mut input_total: u64 = 0;
        for (i, input) in self.transaction.inputs().into_iter().enumerate() {
            let out_point = input.previous_output();
            if previous_outputs.contains(&out_point) {
                return Err(format!("Already have input: {}", out_point));
            } else {
                previous_outputs.insert(out_point.clone());
            }
            let output = get_live_cell(out_point, false)?;
            let capacity: u64 = output.capacity().unpack();
            input_total += capacity;

            check_lock_script(&output.lock())
                .map_err(|err| format!("Input(no.{}) {}", i + 1, err))?;
        }

        // Check output
        let mut output_total: u64 = 0;
        for (i, output) in self.transaction.outputs().into_iter().enumerate() {
            let capacity: u64 = output.capacity().unpack();
            output_total += capacity;

            check_lock_script(&output.lock())
                .map_err(|err| format!("Output(no.{}) {}", i + 1, err))?;
        }

        Ok((input_total, output_total))
    }
}

#[derive(Eq, PartialEq, Clone)]
pub struct MultisigConfig {
    sighash_addresses: HashSet<AddressPayload>,
    require_first_n: u8,
    threshold: u8,
}

impl MultisigConfig {
    pub fn new_with(
        sighash_addresses: HashSet<AddressPayload>,
        require_first_n: u8,
        threshold: u8,
    ) -> Result<MultisigConfig, String> {
        if threshold as usize > sighash_addresses.len() {
            return Err(format!(
                "Invalid threshold {} > {}",
                threshold,
                sighash_addresses.len()
            ));
        }
        if require_first_n > threshold {
            return Err(format!(
                "Invalid require-first-n {} > {}",
                require_first_n, threshold
            ));
        }
        for address_payload in &sighash_addresses {
            if address_payload.ty() != AddressType::Short {
                return Err(format!("Expected a short payload format address, got a full payload format address: {:?}", address_payload));
            }
            if address_payload.code_hash() != SIGHASH_TYPE_HASH.pack() {
                return Err("Invalid code hash expected sighash, got multisig".to_string());
            }
        }
        Ok(MultisigConfig {
            sighash_addresses,
            require_first_n,
            threshold,
        })
    }

    pub fn sighash_addresses(&self) -> &HashSet<AddressPayload> {
        &self.sighash_addresses
    }
    pub fn require_first_n(&self) -> u8 {
        self.require_first_n
    }
    pub fn threshold(&self) -> u8 {
        self.threshold
    }
    pub fn sighash_lock_args(&self) -> HashSet<H160> {
        self.sighash_addresses
            .iter()
            .map(|address| match address {
                AddressPayload::Short { hash, .. } => hash.clone(),
                _ => panic!(
                    "MultisigConfig sighash_addresses can not have full payload format address"
                ),
            })
            .collect()
    }

    pub fn hash160(&self) -> H160 {
        let witness_data = self.to_witness_data();
        let params_hash = blake2b_256(&witness_data);
        H160::from_slice(&params_hash[0..20]).unwrap()
    }

    pub fn to_address_payload(&self, since_absolute_epoch: Option<u64>) -> AddressPayload {
        let hash160 = self.hash160();
        if let Some(absolute_epoch_number) = since_absolute_epoch {
            let since_value = Since::new_absolute_epoch(absolute_epoch_number).value();
            let mut args = Bytes::from(hash160.as_bytes());
            args.extend_from_slice(&since_value.to_le_bytes()[..]);
            AddressPayload::new_full_type(MULTISIG_TYPE_HASH.pack(), args)
        } else {
            AddressPayload::new_short(CodeHashIndex::Multisig, hash160)
        }
    }

    pub fn to_witness_data(&self) -> Bytes {
        let reserved_byte = 0u8;
        let mut witness_data = vec![
            reserved_byte,
            self.require_first_n,
            self.threshold,
            self.sighash_addresses.len() as u8,
        ];
        for sighash_address in &self.sighash_addresses {
            witness_data.extend_from_slice(sighash_address.args().as_ref());
        }
        Bytes::from(witness_data)
    }
}

pub fn check_lock_script(lock: &Script) -> Result<(), String> {
    let lock_arg = lock.args().raw_data();
    if lock.hash_type() != ScriptHashType::Type.into() {
        return Err("invalid lock script hash type, expected `type`".to_string());
    }
    let code_hash: H256 = lock.code_hash().unpack();
    if (code_hash == SIGHASH_TYPE_HASH && lock_arg.len() == 20)
        | (code_hash == MULTISIG_TYPE_HASH && lock_arg.len() == 20)
        | (code_hash == MULTISIG_TYPE_HASH && lock_arg.len() == 28)
    {
        Ok(())
    } else {
        Err(format!(
            "invalid lock script code_hash: {:#x}, args.length: {}",
            code_hash,
            lock_arg.len(),
        ))
    }
}

pub fn build_signature<S: FnMut(&H256) -> Result<[u8; SECP_SIGNATURE_SIZE], String>>(
    tx_hash: &Byte32,
    input_group_idxs: &[usize],
    witnesses: &[packed::Bytes],
    multisig_config_opt: Option<&MultisigConfig>,
    mut signer: S,
) -> Result<Bytes, String> {
    let init_witness_idx = input_group_idxs[0];
    let init_witness = if witnesses[init_witness_idx].raw_data().is_empty() {
        WitnessArgs::default()
    } else {
        WitnessArgs::from_slice(witnesses[init_witness_idx].raw_data().as_ref())
            .map_err(|err| err.to_string())?
    };

    let init_witness = if let Some(multisig_config) = multisig_config_opt {
        let lock_without_sig = {
            let sig_len = (multisig_config.threshold() as usize) * SECP_SIGNATURE_SIZE;
            let mut data = multisig_config.to_witness_data();
            data.extend_from_slice(vec![0u8; sig_len].as_slice());
            data
        };
        init_witness
            .as_builder()
            .lock(Some(lock_without_sig).pack())
            .build()
    } else {
        init_witness
            .as_builder()
            .lock(Some(Bytes::from(vec![0u8; SECP_SIGNATURE_SIZE])).pack())
            .build()
    };

    let mut blake2b = new_blake2b();
    blake2b.update(tx_hash.as_slice());
    blake2b.update(&(init_witness.as_bytes().len() as u64).to_le_bytes());
    blake2b.update(&init_witness.as_bytes());
    for idx in input_group_idxs.iter().skip(1).cloned() {
        let other_witness: &packed::Bytes = &witnesses[idx];
        blake2b.update(&(other_witness.len() as u64).to_le_bytes());
        blake2b.update(&other_witness.raw_data());
    }
    let mut message = [0u8; 32];
    blake2b.finalize(&mut message);
    let message = H256::from(message);
    signer(&message).map(|data| Bytes::from(&data[..]))
}
