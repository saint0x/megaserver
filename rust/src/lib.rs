pub mod app;
pub mod cli;
pub mod controlplane;
pub mod daemon;
pub mod dns;
pub mod ffi;
pub mod host_abi;
pub mod ingress;
pub mod manifest;
pub mod model;
pub mod network;
pub mod planner;
pub mod proxy;
pub mod runtime;
pub mod sandbox;
pub mod state;
pub mod tls;

pub use app::run;

#[cfg(test)]
pub mod test_support {
    use std::sync::Mutex;

    pub static TEST_LOCK: Mutex<()> = Mutex::new(());
}
