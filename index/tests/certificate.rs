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

use crown_index::api::CertifiedReputation;

#[tokio::test]
#[ignore = "needs a running replica; run via scripts/e2e-local.sh"]
async fn certificate_verifies_against_root_key_and_recount() {
    let url = std::env::var("CROWN_REPLICA_URL").expect("CROWN_REPLICA_URL");
    let canister =
        Principal::from_text(std::env::var("CROWN_INDEX_ID").expect("CROWN_INDEX_ID")).unwrap();
    // The full public history of the pinned splitter, as
    // "chain,donor_base58,recipient_base58,gross;..." — recounted below.
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
    // Addresses come as base58 — the chain-local form, exactly like the
    // book keys.
    let address = |text: &str| Address(bs58::decode(text).into_vec().unwrap());
    for entry in history.split(';').filter(|entry| !entry.is_empty()) {
        let [chain, donor, recipient, gross] =
            entry.split(',').collect::<Vec<_>>().try_into().unwrap();
        let settled = Settled {
            chain: ChainId(chain.to_string()),
            donor: address(donor),
            recipient: address(recipient),
            gross: gross.parse().unwrap(),
        };
        reduce(&mut book, &settled).unwrap();
    }
    assert_eq!(
        crown_index::certify::book_root(&book).as_slice(),
        root.as_ref(),
        "recounted root differs from certified root"
    );

    // 4. Proving read: get_reputation_certified answers one entry together
    // with a witness that binds that exact number to the same certified root.
    // A third party needs the certificate and the witness — nothing else, no
    // chain access, no trust in the operator.
    let [chain, donor, recipient, _] = history
        .trim_end_matches(';')
        .rsplit(';')
        .next()
        .unwrap()
        .split(',')
        .collect::<Vec<_>>()
        .try_into()
        .unwrap();
    let key = (
        ChainId(chain.to_string()),
        address(donor),
        address(recipient),
    );
    let reply = agent
        .query(&canister, "get_reputation_certified")
        .with_arg(
            Encode!(
                &chain.to_string(),
                &serde_bytes::ByteBuf::from(key.1.0.clone()),
                &serde_bytes::ByteBuf::from(key.2.0.clone())
            )
            .unwrap(),
        )
        .call()
        .await
        .unwrap();
    let certified = Decode!(&reply, CertifiedReputation).unwrap();

    let certificate: Certificate =
        serde_cbor::from_slice(&certified.certificate.expect("no data certificate")).unwrap();
    agent.verify(&certificate, canister).unwrap();
    let LookupResult::Found(certified_root) = certificate.tree.lookup_path(&path) else {
        panic!("certified_data path not found in certificate");
    };

    let witness: ic_certification::HashTree = serde_cbor::from_slice(&certified.witness).unwrap();
    assert_eq!(
        crown_index::certify::seal(&witness.digest()).as_slice(),
        certified_root,
        "witness does not reconstruct to the certified root"
    );
    let key_bytes = crown_index::key_bytes(&key);
    let proven = match witness.lookup_path([key_bytes.as_slice()]) {
        LookupResult::Found(value) => {
            u128::from_le_bytes(<[u8; 16]>::try_from(value).expect("16-byte entry total"))
        }
        // A missing key is 0 by the book's own definition (core-spec §2).
        LookupResult::Absent => 0,
        other => panic!("witness does not decide the key: {other:?}"),
    };
    assert_eq!(
        candid::Nat::from(proven),
        certified.value,
        "the witness proves a different number than the answer"
    );
    assert_eq!(
        proven,
        book.get(&key),
        "the certified entry differs from the recount"
    );
}
