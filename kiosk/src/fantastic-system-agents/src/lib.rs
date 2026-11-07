//! THE FANTASTIC OS SYSTEM AGENTS (Phase 10, #55) as bundles. Each agent is a
//! pure ViewModel (`fantastic-system`) run by the generic [`driver`] against an
//! OS port (`sys`); only `sys/linux` talks to the machine. The six agents:
//! `power`, `network`, `audio`, `displays`, `notifications` and `lock`.

pub mod audio;
pub mod displays;
pub mod driver;
pub mod lock;
pub mod network;
pub mod notifications;
pub mod power;
pub mod sys;

/// The `handler_module` of each agent, in the order of [`IDS`].
pub const HANDLER_MODULES: [&str; 6] = [
    power::HANDLER_MODULE,
    network::HANDLER_MODULE,
    audio::HANDLER_MODULE,
    displays::HANDLER_MODULE,
    notifications::HANDLER_MODULE,
    lock::HANDLER_MODULE,
];

/// The agents' ids, each made under `user` by the `system` recipe.
pub const IDS: [&str; 6] = [
    power::ID,
    network::ID,
    audio::ID,
    displays::ID,
    notifications::ID,
    lock::ID,
];

/// What each agent's view may call, in the order of [`IDS`].
pub fn verbs(id: &str) -> &'static [&'static str] {
    match id {
        power::ID => &power::VERBS,
        network::ID => &network::VERBS,
        audio::ID => &audio::VERBS,
        displays::ID => &displays::VERBS,
        notifications::ID => &notifications::VERBS,
        lock::ID => &lock::VERBS,
        _ => &[],
    }
}

/// The agent's view (an html page for a canvas cell).
pub fn view(id: &str) -> Option<&'static str> {
    match id {
        power::ID => Some(power::VIEW),
        network::ID => Some(network::VIEW),
        audio::ID => Some(audio::VIEW),
        displays::ID => Some(displays::VIEW),
        notifications::ID => Some(notifications::VIEW),
        lock::ID => Some(lock::VIEW),
        _ => None,
    }
}

/// Register every system agent's bundle, on the machine's real OS port.
pub fn register(reg: &mut fantastic_kernel::BundleRegistry) {
    reg.register(power::HANDLER_MODULE, power::PowerBundle::new());
    reg.register(network::HANDLER_MODULE, network::NetworkBundle::new());
    reg.register(audio::HANDLER_MODULE, audio::AudioBundle::new());
    reg.register(displays::HANDLER_MODULE, displays::DisplaysBundle::new());
    reg.register(
        notifications::HANDLER_MODULE,
        notifications::NotificationsBundle::new(),
    );
    reg.register(lock::HANDLER_MODULE, lock::LockBundle::new());
}
