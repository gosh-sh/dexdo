//! On-chain contract wrappers hosted in this repo — message encoders/decoders,
//! getters, and typed event decoders. The wrapper style and trait surface
//! (`ContractBase`, `HasContractBase`, `AutoContract`, the encode/decode/send/
//! getter traits, error/account/event/deserialize infra) are imported from
//! `ackinacki_kit`; only the contract-specific code lives here. ABIs are read
//! from the repo's `contracts/<group>/` sources, the single source of truth.
//!
//! `dex` holds the DEX contracts; `airegistry` wrappers land here too as they
//! are authored.

pub mod airegistry;
pub mod dex;

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ackinacki_kit::tvm_client::ClientConfig;
    use ackinacki_kit::tvm_client::ClientContext;

    const NETWORK_ENDPOINT: &str = "shellnet.ackinacki.org";

    pub fn create_context() -> Arc<ClientContext> {
        let mut config = ClientConfig::default();
        config.network.endpoints = Some(vec![NETWORK_ENDPOINT.to_string()]);
        let context = ClientContext::new(config).expect("Create context");
        Arc::new(context)
    }
}
