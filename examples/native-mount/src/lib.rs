//! An ordinary rutis plugin. It has no protocol types or adapter annotations.

use std::sync::Mutex;

use rutis::{BoxFuture, CordisError, Ctx, Effect, Plugin};

pub struct Counter {
    value: Mutex<f64>,
}

impl Counter {
    fn new(initial: f64) -> Self {
        Self {
            value: Mutex::new(initial),
        }
    }

    pub fn add(&self, amount: f64) -> f64 {
        let mut value = self.value.lock().unwrap();
        *value += amount;
        *value
    }

    pub fn current(&self) -> f64 {
        *self.value.lock().unwrap()
    }

    pub fn reset(&self) {
        *self.value.lock().unwrap() = 0.0;
    }

    pub async fn delayed_add(&self, amount: f64) -> f64 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        self.add(amount)
    }

    pub fn fail(&self) -> Result<f64, std::io::Error> {
        Err(std::io::Error::other("counter refused operation"))
    }
}

pub struct CounterPlugin {
    pub initial: f64,
}

impl Plugin for CounterPlugin {
    fn name(&self) -> &str {
        "ordinary-rust-counter"
    }

    fn validate(&self) -> Result<(), CordisError> {
        if self.initial < 0.0 {
            return Err(CordisError::PluginFailed(Box::new(std::io::Error::other(
                "initial value must be non-negative",
            ))));
        }
        Ok(())
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            ctx.provide(Counter::new(self.initial))?;
            Ok(Effect::Done)
        })
    }
}
