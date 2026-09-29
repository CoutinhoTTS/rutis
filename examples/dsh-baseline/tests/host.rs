//! A published plugin that depends on a service the rutis application
//! provides: dsh-persona registers prompt sections with `systemPrompt`.
#![cfg(all(unix, dsh_baseline))]

use std::sync::{Arc, Mutex};

use dsh_baseline::persona::{self, PromptSection, SystemPromptHostGetSectionOrderName as Order};
use rutis::{Ctx, FiberState};
use rutis_interop::rpc::Value;
use rutis_interop::Error;

#[derive(Default, Clone)]
struct Prompts {
    sections: Arc<Mutex<Vec<PromptSection>>>,
}

impl persona::SystemPromptHost for Prompts {
    fn section(&self, section: PromptSection) -> Result<Value, Error> {
        let name = section.name.clone();
        self.sections.lock().unwrap().push(section);
        // The disposer Cordis keeps as the plugin's effect.
        let sections = self.sections.clone();
        Ok(Value::callback(move |_| {
            sections
                .lock()
                .unwrap()
                .retain(|section| section.name != name);
            Ok(Value::Undefined)
        }))
    }

    fn get_section_order(&self, name: Order) -> Result<f64, Error> {
        Ok(match name {
            Order::DeploymentPersonaPrefix => 10.0,
            Order::DeploymentPersonaSuffix => 90.0,
            _ => 50.0,
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn persona_uses_the_system_prompt_the_rutis_host_provides() {
    let ctx = Ctx::root().unwrap();
    let view = ctx.plugin(persona::Plugin::new(persona::Config {
        prefix: "You are a careful reviewer.".into(),
        suffix: Some("Answer briefly.".into()),
        complete: None,
        include_runtime_context: None,
    }));
    (&view).await.unwrap();
    // The host service is not provided yet: the mount waits natively.
    assert_eq!(view.state().state, FiberState::Pending);

    let prompts = Prompts::default();
    let provider = persona::provide_system_prompt(&ctx, prompts.clone()).unwrap();
    (&view).await.unwrap();
    assert_eq!(view.state().state, FiberState::Active);
    let mut sections: Vec<_> = prompts
        .sections
        .lock()
        .unwrap()
        .iter()
        .map(|section| (section.order, section.name.clone(), section.text.clone()))
        .collect();
    sections.sort_by(|a, b| a.0.total_cmp(&b.0));
    assert_eq!(
        sections,
        vec![
            (
                10.0,
                "deployment:persona-prefix".into(),
                "You are a careful reviewer.".into()
            ),
            (
                90.0,
                "deployment:persona-suffix".into(),
                "Answer briefly.".into()
            ),
        ]
    );

    // The host emits its change event into Cordis like the native registry.
    ctx.events()
        .parallel(
            &ctx,
            &rutis::EventKey::<persona::SystemPromptChange>::of(),
            std::sync::Arc::new(persona::SystemPromptChange {}),
        )
        .await
        .unwrap();

    // Withdrawing the host service stops the mount by native dependency
    // rules; the plugin's Cordis cleanup calls the disposers it was given.
    provider.dispose().await.unwrap();
    (&view).await.unwrap();
    assert_eq!(view.state().state, FiberState::Pending);
    assert!(prompts.sections.lock().unwrap().is_empty());

    // Providing it again restarts the mount.
    let provider = persona::provide_system_prompt(&ctx, prompts.clone()).unwrap();
    (&view).await.unwrap();
    assert_eq!(view.state().state, FiberState::Active);
    assert_eq!(prompts.sections.lock().unwrap().len(), 2);

    // Unmounting runs the plugin's Cordis effects, which call the disposers
    // the host returned.
    view.dispose().await.unwrap();
    assert!(prompts.sections.lock().unwrap().is_empty());
    provider.dispose().await.unwrap();
    ctx.shutdown().await.unwrap();
}
