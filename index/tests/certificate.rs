//! Offchain verification of `get_certificate` (docs/build-plan.md S3 DoD):
//! the certificate must verify against the network root key and certify
//! exactly the root recomputed independently from the public settlements via
//! crown-reduce. "Recount it yourself" is this test.
//!
//! Needs a running replica and env; driven by scripts/e2e-local.sh.

use candid::{Decode, Encode, Principal};
use crown_reduce::{Address, ChainId, Settled, reduce};
use ic_agent::Agent;
use ic_certification::{Certificate, LookupResult};

#[tokio::test]
#[ignore = "needs a running replica; run via scripts/e2e-local.sh"]
async fn certificate_verifies_against_root_key_and_recount() {
    let url = std::env::var("CROWN_REPLICA_URL").expect("CROWN_REPLICA_URL");
    let canister =
        Principal::from_text(std::env::var("CROWN_INDEX_ID").expect("CROWN_INDEX_ID")).unwrap();
    // The full public history of the pinned splitter, as
    // "chain,payer_base58,streamer_base58,gross;..." — recounted below.
    let history = std::env::var("CROWN_E2E_SETTLEMENTS").expect("CROWN_E2E_SETTLEMENTS");

    let agent = Agent::builder().with_url(&url).build().unwrap();
    // Local replica key here; against production this is the NNS root key.
    agent.fetch_root_key().await.unwrap();

    let reply = agent
        .query(&canister, "get_certificate")
        .with_arg(Encode!().unwrap())
        .call()
        .await
        .unwrap();
    let (certificate, root) =
        Decode!(&reply, Option<serde_bytes::ByteBuf>, serde_bytes::ByteBuf).unwrap();
    let certificate = certificate.expect("query carried no data certificate");

    // 1. Genuine: the certificate is signed by the network root key.
    let certificate: Certificate = serde_cbor::from_slice(&certificate).unwrap();
    agent.verify(&certificate, canister).unwrap();

    // 2. Bound: it certifies exactly the root the canister answered with.
    let path = [
        "canister".as_bytes(),
        canister.as_slice(),
        "certified_data".as_bytes(),
    ];
    let LookupResult::Found(certified) = certificate.tree.lookup_path(&path) else {
        panic!("certified_data path not found in certificate");
    };
    assert_eq!(certified, root.as_ref(), "certificate rebinds another root");

    // 3. Honest: independent recount through the public reduce law lands on
    // the same root.
    let mut book = crown_reduce::Book::new();
    for entry in history.split(';').filter(|entry| !entry.is_empty()) {
        let [chain, payer, streamer, gross] =
            entry.split(',').collect::<Vec<_>>().try_into().unwrap();
        let settled = Settled {
            chain: ChainId(chain.to_string()),
            payer: Address(bs58::decode(payer).into_vec().unwrap()),
            streamer: Address(bs58::decode(streamer).into_vec().unwrap()),
            gross: gross.parse().unwrap(),
        };
        reduce(&mut book, &settled).unwrap();
    }
    assert_eq!(
        crown_index::certify::book_root(&book).as_slice(),
        root.as_ref(),
        "recounted root differs from certified root"
    );
}
