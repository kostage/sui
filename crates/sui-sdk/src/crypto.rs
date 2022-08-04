// Copyright (c) 2022, Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::fmt::Write;
use std::fmt::{Display, Formatter};
use std::fs;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use bip39::Mnemonic;
use rand::rngs::adapter::ReadRng;
use serde::{Deserialize, Serialize};
use signature::Signer;

use sui_types::base_types::SuiAddress;
use sui_types::crypto::{
    get_key_pair_from_rng, AccountKeyPair, AccountPublicKey, EncodeDecodeBase64, KeypairTraits,
    Signature, ToFromBytes,
};

#[derive(Serialize, Deserialize)]
#[non_exhaustive]
// This will work on user signatures, but not suitable for authority signatures.
pub enum KeystoreType {
    File(PathBuf),
}

pub trait AccountKeystore: Send + Sync {
    fn sign(&self, address: &SuiAddress, msg: &[u8]) -> Result<Signature, signature::Error>;
    fn add_key(&mut self, keypair: AccountKeyPair) -> Result<(), anyhow::Error>;
    fn keys(&self) -> Vec<AccountPublicKey>;
}

impl KeystoreType {
    pub fn init(&self) -> Result<SuiKeystore, anyhow::Error> {
        Ok(match self {
            KeystoreType::File(path) => SuiKeystore::from(FileBasedKeystore::load_or_create(path)?),
        })
    }
}

impl Display for KeystoreType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut writer = String::new();
        match self {
            KeystoreType::File(path) => {
                writeln!(writer, "Keystore Type : File")?;
                write!(writer, "Keystore Path : {:?}", path)?;
                write!(f, "{}", writer)
            }
        }
    }
}

#[derive(Serialize, Deserialize, Default)]
pub struct FileBasedKeystore {
    keys: BTreeMap<SuiAddress, AccountKeyPair>,
    path: Option<PathBuf>,
}

impl AccountKeystore for FileBasedKeystore {
    fn sign(&self, address: &SuiAddress, msg: &[u8]) -> Result<Signature, signature::Error> {
        self.keys
            .get(address)
            .ok_or_else(|| {
                signature::Error::from_source(format!("Cannot find key for address: [{address}]"))
            })?
            .try_sign(msg)
    }

    fn add_key(&mut self, keypair: AccountKeyPair) -> Result<(), anyhow::Error> {
        let address: SuiAddress = keypair.public().into();
        self.keys.insert(address, keypair);
        self.save()?;
        Ok(())
    }

    fn keys(&self) -> Vec<AccountPublicKey> {
        self.keys.values().map(|key| key.public().clone()).collect()
    }
}

impl FileBasedKeystore {
    pub fn load_or_create(path: &Path) -> Result<Self, anyhow::Error> {
        let keys = if path.exists() {
            let reader = BufReader::new(File::open(path)?);
            let kp_strings: Vec<String> = serde_json::from_reader(reader)?;
            kp_strings
                .iter()
                .map(|kpstr| {
                    let key = AccountKeyPair::decode_base64(kpstr);
                    key.map(|k| (k.public().into(), k))
                })
                .collect::<Result<BTreeMap<_, _>, _>>()
                .map_err(|_| anyhow::anyhow!("Invalid Keypair file"))?
        } else {
            BTreeMap::new()
        };

        Ok(Self {
            keys,
            path: Some(path.to_path_buf()),
        })
    }

    pub fn set_path(&mut self, path: &Path) {
        self.path = Some(path.to_path_buf());
    }

    pub fn save(&self) -> Result<(), anyhow::Error> {
        if let Some(path) = &self.path {
            let store = serde_json::to_string_pretty(
                &self
                    .keys
                    .values()
                    .map(|k| k.encode_base64())
                    .collect::<Vec<_>>(),
            )
            .unwrap();
            fs::write(path, store)?
        }
        Ok(())
    }

    pub fn key_pairs(&self) -> Vec<&AccountKeyPair> {
        self.keys.values().collect()
    }
}

pub struct SuiKeystore(Box<dyn AccountKeystore>);

impl SuiKeystore {
    fn from<S: AccountKeystore + 'static>(keystore: S) -> Self {
        Self(Box::new(keystore))
    }

    pub fn add_key(&mut self, keypair: AccountKeyPair) -> Result<String, anyhow::Error> {
        let pk = keypair.private();
        let phrase = Mnemonic::from_entropy(pk.as_bytes())?.to_string();
        let keypair = AccountKeyPair::from(pk);
        self.0.add_key(keypair)?;
        Ok(phrase)
    }

    pub fn keys(&self) -> Vec<AccountPublicKey> {
        self.0.keys()
    }

    pub fn addresses(&self) -> Vec<SuiAddress> {
        self.keys().iter().map(|k| k.into()).collect()
    }

    pub fn signer(&self, signer: SuiAddress) -> impl Signer<Signature> + '_ {
        KeystoreSigner::new(&*self.0, signer)
    }

    pub fn import_from_mnemonic(&mut self, phrase: &str) -> Result<SuiAddress, anyhow::Error> {
        let seed = &Mnemonic::from_str(phrase).unwrap().to_seed("");
        let mut rng = RngWrapper(ReadRng::new(seed));
        let (address, kp) = get_key_pair_from_rng(&mut rng);
        self.0.add_key(kp)?;
        Ok(address)
    }

    pub fn sign(&self, address: &SuiAddress, msg: &[u8]) -> Result<Signature, signature::Error> {
        self.0.sign(address, msg)
    }
}

/// wrapper for adding CryptoRng and RngCore impl to ReadRng.
struct RngWrapper<'a>(ReadRng<&'a [u8]>);

impl rand::CryptoRng for RngWrapper<'_> {}
impl rand::RngCore for RngWrapper<'_> {
    fn next_u32(&mut self) -> u32 {
        self.0.next_u32()
    }

    fn next_u64(&mut self) -> u64 {
        self.0.next_u64()
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        self.0.fill_bytes(dest)
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand::Error> {
        self.0.try_fill_bytes(dest)
    }
}

struct KeystoreSigner<'a> {
    keystore: &'a dyn AccountKeystore,
    address: SuiAddress,
}

impl<'a> KeystoreSigner<'a> {
    pub fn new(keystore: &'a dyn AccountKeystore, account: SuiAddress) -> Self {
        Self {
            keystore,
            address: account,
        }
    }
}

impl Signer<Signature> for KeystoreSigner<'_> {
    fn try_sign(&self, msg: &[u8]) -> Result<Signature, signature::Error> {
        self.keystore.sign(&self.address, msg)
    }
}
