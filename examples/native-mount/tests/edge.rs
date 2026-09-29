#![cfg(unix)]
//! Edge cases of the generated bindings (PR #73 review of 2fe5e3e): null
//! versus undefined in parameters, fields and configuration; parameter
//! names that match generated locals; unions of live objects.

rutis_interop::include_mounts!();

use edge::{Edge, Input, JoinerHost, Left, Right};

struct Host;
impl JoinerHost for Host {
    // Named like the dispatcher's own locals once were.
    fn call(&self, args: Vec<String>, suffix: String) -> Result<String, rutis_interop::Error> {
        Ok(format!("{}{suffix}", args.join("+")))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn generated_bindings_keep_null_names_and_live_unions() {
    let ctx = rutis::Ctx::root().unwrap();
    let _host = edge::provide_edge_host(&ctx, Host).unwrap();
    let view = ctx.plugin(edge::Plugin::new(edge::Config { key: None }));
    (&view).await.unwrap();
    let service = ctx.get::<Edge>().unwrap();

    // A required nullable value sends null; an omissible one undefined.
    assert_eq!(service.config_key().unwrap(), "null");
    assert_eq!(service.nullable(None).unwrap(), "null");
    assert_eq!(service.nullable(Some("x")).unwrap(), "x");
    assert_eq!(service.either(None).unwrap(), "undefined");
    assert_eq!(service.either(Some(None)).unwrap(), "null");
    assert_eq!(service.either(Some(Some("x"))).unwrap(), "x");
    let input = Input {
        key: None,
        note: None,
        label: Some(None),
    };
    assert_eq!(service.input(&input).unwrap(), "null,missing,null");
    let input = Input {
        key: Some("k".into()),
        note: Some("n".into()),
        label: None,
    };
    assert_eq!(service.input(&input).unwrap(), "k,n,missing");

    // Parameters named like generated locals.
    assert_eq!(service.call(|value| Ok(value * 2.0)).unwrap(), 41.0);
    assert_eq!(service.host().unwrap(), "a+b!");

    // A union of live objects stays a reference.
    let left = Left(service.object(true).unwrap());
    assert_eq!(left.kind().unwrap(), "left");
    let right = Right(service.object(false).unwrap());
    assert_eq!(right.kind().unwrap(), "right");
    // A union of a live object and data keeps the reference in its variant,
    // in both directions.
    let edge::EdgePickResult::Left(picked) = service.pick(true).unwrap() else {
        panic!("expected the live object variant")
    };
    assert_eq!(picked.kind().unwrap(), "left");
    assert!(matches!(
        service.pick(false).unwrap(),
        edge::EdgePickResult::Label(label) if label.text == "label"
    ));
    assert_eq!(
        service
            .describe_item(&edge::EdgeDescribeItemItem::Left(picked.clone()))
            .unwrap(),
        "object left"
    );
    assert_eq!(
        service
            .describe_item(&edge::EdgeDescribeItemItem::Text("x".into()))
            .unwrap(),
        "text x"
    );
    // Dynamic JSON cannot hold a live object: the decode fails instead of
    // returning the internal marker as data.
    let error = service.dynamic().unwrap_err();
    assert!(
        error.to_string().contains("live Cordis object or function"),
        "{error}"
    );

    drop((service, left, right, picked));
    view.dispose().await.unwrap();
    ctx.shutdown().await.unwrap();
}
