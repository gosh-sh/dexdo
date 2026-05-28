//! Random keypair generation via tvm_client mnemonic.

use std::sync::Arc;

use ackinacki_kit::tvm_client::crypto;
use ackinacki_kit::tvm_client::crypto::KeyPair;
use ackinacki_kit::tvm_client::crypto::ParamsOfMnemonicDeriveSignKeys;
use ackinacki_kit::tvm_client::crypto::ParamsOfMnemonicFromRandom;
use ackinacki_kit::tvm_client::ClientContext;

pub fn gen_keys(context: Arc<ClientContext>) -> KeyPair {
    let phrase = crypto::mnemonic_from_random(
        context.clone(),
        ParamsOfMnemonicFromRandom { dictionary: None, word_count: Some(24) },
    )
    .expect("mnemonic")
    .phrase;
    crypto::mnemonic_derive_sign_keys(
        context,
        ParamsOfMnemonicDeriveSignKeys {
            phrase,
            path: None,
            dictionary: None,
            word_count: Some(24),
        },
    )
    .expect("derive keys")
}
