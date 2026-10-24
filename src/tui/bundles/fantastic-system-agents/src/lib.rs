//! THE FANTASTIC OS SYSTEM AGENTS (Phase 10, #55) as bundles. Each agent is a
//! pure ViewModel (`fantastic-system`) run by the generic [`driver`] against an
//! OS port (`sys`); only `sys/linux` talks to the machine. This slice: `power`.

pub mod driver;
pub mod power;
pub mod sys;

/// The `handler_module` of each agent, in the order of [`IDS`].
pub const HANDLER_MODULES: [&str; 1] = [power::HANDLER_MODULE];

/// The agents' ids, each made under `user` by the `system` recipe.
pub const IDS: [&str; 1] = [power::ID];

/// What each agent's view may call, in the order of [`IDS`].
pub fn verbs(id: &str) -> &'static [&'static str] {
    match id {
        power::ID => &power::VERBS,
        _ => &[],
    }
}

/// The agent's view (an html page for a canvas cell).
pub fn view(id: &str) -> Option<&'static str> {
    match id {
        power::ID => Some(power::VIEW),
        _ => None,
    }
}

/// Register every system agent's bundle, on the machine's real OS port.
pub fn register(reg: &mut fantastic_kernel::BundleRegistry) {
    reg.register(power::HANDLER_MODULE, power::PowerBundle::new());
}
