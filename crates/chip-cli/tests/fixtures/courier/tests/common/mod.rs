//! Helpers shared by the integration tests.
#![allow(dead_code)]

use courier::transport::mock::{MockTransport, Outcome};
use courier::util::ManualClock;
use courier::{Client, ClientBuilder};
use std::path::PathBuf;
use std::sync::Arc;

pub struct Rig {
    pub client: Client,
    pub transport: Arc<MockTransport>,
    pub clock: Arc<ManualClock>,
}

/// A client built from `config_text` over a scripted transport and a manual clock.
pub fn rig(config_text: &str, script: Vec<Outcome>) -> Rig {
    rig_with_env(config_text, &[], script)
}

pub fn rig_with_env(config_text: &str, env: &[(&str, &str)], script: Vec<Outcome>) -> Rig {
    let env: Vec<(String, String)> = env.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
    let transport = Arc::new(MockTransport::new(script));
    let clock = Arc::new(ManualClock::new());
    let client = ClientBuilder::from_text(config_text, &env)
        .expect("configuration loads")
        .transport(transport.clone())
        .clock(clock.clone())
        .build()
        .expect("client builds");
    Rig {
        client,
        transport,
        clock,
    }
}

/// A fresh scratch directory under the system temp dir.
pub fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("courier-it-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
