#![cfg(unix)]
//! Objects with behaviour cross by reference: Rust reads live properties,
//! calls methods, and passing an object back hands Cordis the original.

use std::io::Write;
use std::path::Path;

use rutis_interop::rpc::Value;
use rutis_interop::{arg, decode_value, ObjectRef, Process};
use serde::{Deserialize, Serialize};
use serde_json::json;

const PLUGIN: &str = r#"
class Account {
  #balance
  constructor(owner, balance) { this.owner = owner; this.#balance = balance }
  get balance() { return this.#balance }
  deposit(amount) { this.#balance += amount; return this.#balance }
  async later(amount) { return this.deposit(amount) }
}
export function apply(ctx) {
  const accounts = [new Account('ada', 5)]
  ctx.provide('bank', {
    open(owner) { const account = new Account(owner, 0); accounts.push(account); return account },
    summary() { return { first: accounts[0], count: accounts.length } },
    same(account, index) { return account === accounts[index] },
  })
}
"#;

#[derive(Deserialize, Serialize)]
struct Summary {
    first: ObjectRef,
    count: f64,
}

#[tokio::test(flavor = "multi_thread")]
async fn live_objects_keep_identity_and_state() {
    let mut plugin = tempfile::Builder::new().suffix(".mjs").tempfile().unwrap();
    plugin.write_all(PLUGIN.as_bytes()).unwrap();
    let process = Process::launch(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interop/node"),
        plugin.path(),
        json!({}),
        json!({ "bank": ["open", "summary", "same"] }),
    )
    .await
    .unwrap();
    let bank = process.service("bank").unwrap();

    // A returned object is a reference; properties are read live.
    let account: ObjectRef = decode_value(
        process
            .invoke(&bank, "open", vec![arg("grace").unwrap()])
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        account.get("owner").unwrap().json().unwrap(),
        json!("grace")
    );
    assert_eq!(
        account
            .call("deposit", vec![arg(&3).unwrap()])
            .unwrap()
            .json()
            .unwrap(),
        json!(3)
    );
    assert_eq!(account.get("balance").unwrap().json().unwrap(), json!(3));
    assert_eq!(
        account
            .call_async("later", vec![arg(&4).unwrap()])
            .await
            .unwrap()
            .json()
            .unwrap(),
        json!(7)
    );

    // Objects nested in data decode into generated-style types.
    let summary: Summary = decode_value(process.invoke(&bank, "summary", vec![]).unwrap()).unwrap();
    assert_eq!(summary.count, 2.0);
    assert_eq!(
        summary.first.get("balance").unwrap().json().unwrap(),
        json!(5)
    );

    // Passing an object back gives Cordis the original instance.
    let same = |object: &ObjectRef, index: u32| -> bool {
        let value = process
            .invoke(
                &bank,
                "same",
                vec![arg(object).unwrap(), arg(&index).unwrap()],
            )
            .unwrap();
        value.json().unwrap() == json!(true)
    };
    assert!(same(&account, 1));
    assert!(same(&summary.first, 0));
    assert!(!same(&summary.first, 1));
    // An object inside a data argument is restored too.
    let nested = arg(&summary).unwrap();
    assert!(matches!(nested, Value::Record(_)));

    drop((account, summary));
    process.dispose().await.unwrap();
}
